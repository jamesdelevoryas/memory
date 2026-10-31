//! Client commands that talk to a running server over HTTP: uploading Claude
//! Code sessions from a hook, importing a claude.ai export, and the dreamer's
//! pending check and trigger. They read `MEMORY_URL` (the server's base URL)
//! and `MEMORY_TOKEN` (this client's bearer token).

use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, de::DeserializeOwned};

use crate::{
    episodes::{MAX_UPLOAD_EPISODES, Upload, Uploaded},
    transcripts,
};

/// `--recent` uploads sessions written to within this long.
const RECENT: Duration = Duration::from_secs(24 * 60 * 60);

pub struct Remote {
    agent: ureq::Agent,
    url: String,
    auth: String,
}

impl Remote {
    pub fn from_env() -> Result<Self> {
        let url = std::env::var("MEMORY_URL")
            .context("set MEMORY_URL to the server's base URL, e.g. https://memory.example.com")?;
        let token =
            std::env::var("MEMORY_TOKEN").context("set MEMORY_TOKEN to this client's token")?;
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .build();
        Ok(Self {
            agent: ureq::Agent::new_with_config(config),
            url: url.trim_end_matches('/').to_string(),
            auth: format!("Bearer {}", token.trim()),
        })
    }

    pub fn upload(&self, upload: &Upload) -> Result<Uploaded> {
        let resp = self
            .agent
            .post(format!("{}/episodes", self.url))
            .header("Authorization", &self.auth)
            .send_json(upload)?;
        parse(resp)
    }

    /// How many sessions are waiting for the dreamer.
    pub fn pending(&self) -> Result<u64> {
        #[derive(Deserialize)]
        struct Pending {
            sessions: u64,
        }
        let resp = self
            .agent
            .get(format!("{}/episodes/pending", self.url))
            .header("Authorization", &self.auth)
            .call()?;
        Ok(parse::<Pending>(resp)?.sessions)
    }

    pub fn dream(&self) -> Result<()> {
        let resp = self
            .agent
            .post(format!("{}/dream", self.url))
            .header("Authorization", &self.auth)
            .send_empty()?;
        parse::<serde_json::Value>(resp)?;
        Ok(())
    }
}

fn parse<T: DeserializeOwned>(mut resp: ureq::http::Response<ureq::Body>) -> Result<T> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        bail!("server answered {status}: {}", body.trim());
    }
    Ok(resp.body_mut().read_json()?)
}

/// What a Claude Code hook sends on stdin.
#[derive(Deserialize)]
pub struct HookInput {
    pub session_id: String,
    pub transcript_path: PathBuf,
}

/// Uploads the lines added to a Claude Code session file since the last
/// upload, remembering how far it got in a state file. Returns the number of
/// episodes the server added.
pub fn upload_claude_code(remote: &Remote, path: &Path, session: &str) -> Result<u64> {
    let state = state_dir()?
        .join("claude-code")
        .join(format!("{session}.offset"));
    let mut offset: u64 = std::fs::read_to_string(&state)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    if offset > file.metadata()?.len() {
        // Rewritten from the start; the server skips what it already has.
        offset = 0;
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    // Only whole lines: the last one may still be being written.
    let Some(end) = bytes.iter().rposition(|&b| b == b'\n').map(|i| i + 1) else {
        return Ok(0);
    };
    let parsed = transcripts::claude_code(&String::from_utf8_lossy(&bytes[..end]));
    let title = parsed.cwd.map(|cwd| home_relative(&cwd));
    let mut added = 0;
    for batch in parsed.episodes.chunks(MAX_UPLOAD_EPISODES) {
        let up = remote.upload(&Upload {
            session: session.to_string(),
            title: title.clone(),
            episodes: batch.to_vec(),
        })?;
        added += up.added;
    }
    std::fs::create_dir_all(state.parent().unwrap())?;
    std::fs::write(&state, (offset + end as u64).to_string())?;
    Ok(added)
}

/// Every Claude Code session file written to recently, with its session id.
pub fn recent_claude_code_sessions() -> Result<Vec<(PathBuf, String)>> {
    let projects = home()?.join(".claude").join("projects");
    let since = SystemTime::now() - RECENT;
    let mut out = Vec::new();
    for project in
        std::fs::read_dir(&projects).with_context(|| format!("reading {}", projects.display()))?
    {
        let project = project?.path();
        if !project.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&project)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            if path.metadata()?.modified()? < since {
                continue;
            }
            let Some(session) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            out.push((path.clone(), session.to_string()));
        }
    }
    Ok(out)
}

/// Uploads every conversation in a claude.ai export's `conversations.json`.
/// Returns (conversations, episodes added, episodes skipped).
pub fn import_claude_ai(remote: &Remote, path: &Path) -> Result<(usize, u64, u64)> {
    let json =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let uploads = transcripts::claude_ai_export(&json)?;
    let (mut added, mut skipped) = (0, 0);
    for upload in &uploads {
        for batch in upload.episodes.chunks(MAX_UPLOAD_EPISODES) {
            let up = remote
                .upload(&Upload {
                    session: upload.session.clone(),
                    title: upload.title.clone(),
                    episodes: batch.to_vec(),
                })
                .with_context(|| format!("uploading conversation {}", upload.session))?;
            added += up.added;
            skipped += up.skipped;
        }
    }
    Ok((uploads.len(), added, skipped))
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn state_dir() -> Result<PathBuf> {
    match std::env::var_os("XDG_STATE_HOME") {
        Some(dir) => Ok(PathBuf::from(dir).join("memory")),
        None => Ok(home()?.join(".local").join("state").join("memory")),
    }
}

fn home_relative(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}
