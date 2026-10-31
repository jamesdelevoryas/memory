//! Turning clients' own transcripts into episodes: Claude Code's session
//! files and claude.ai's data export. Only what a person or model said is
//! kept; harness bookkeeping, injected context, and thinking are dropped, and
//! tool traffic is kept short.

use anyhow::{Context, Result};
use serde_json::Value;

use crate::episodes::{NewEpisode, Role, Upload, truncate};

/// Tool calls and results are context, not the conversation; keep them short.
const MAX_TOOL_CHARS: usize = 2_000;

/// User-role text the harness writes, not the user.
const HARNESS_PREFIXES: &[&str] = &[
    "<local-command-caveat>",
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<command-name>",
    "<command-message>",
    "<task-notification>",
    "<system-reminder>",
    "[Request interrupted by user",
];

pub struct ClaudeCode {
    pub episodes: Vec<NewEpisode>,
    /// The session's working directory, from the first line that has one.
    pub cwd: Option<String>,
}

/// Parses complete lines of a Claude Code session file (`~/.claude/projects/*/<session>.jsonl`).
pub fn claude_code(lines: &str) -> ClaudeCode {
    let mut out = ClaudeCode {
        episodes: Vec::new(),
        cwd: None,
    };
    for line in lines.lines() {
        // Unknown or malformed lines are skipped: the format isn't ours.
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if out.cwd.is_none() {
            out.cwd = v["cwd"].as_str().map(str::to_string);
        }
        let kind = v["type"].as_str().unwrap_or_default();
        if !matches!(kind, "user" | "assistant")
            || v["isMeta"] == true
            || v["isSidechain"] == true
            || v["isCompactSummary"] == true
            || v["isApiErrorMessage"] == true
        {
            continue;
        }
        let (Some(uuid), Some(at)) = (v["uuid"].as_str(), v["timestamp"].as_str()) else {
            continue;
        };
        let speaker = if kind == "user" {
            Role::User
        } else {
            Role::Assistant
        };
        let content = &v["message"]["content"];
        if let Some(s) = content.as_str() {
            if let Some((role, text)) = spoken(speaker, s) {
                out.episodes.push(episode(uuid.to_string(), role, text, at));
            }
            continue;
        }
        let Some(blocks) = content.as_array() else {
            continue;
        };
        for (i, block) in blocks.iter().enumerate() {
            let key = format!("{uuid}:{i}");
            let found = match block["type"].as_str() {
                Some("text") => block["text"].as_str().and_then(|s| spoken(speaker, s)),
                Some("tool_use") => Some((Role::Tool, tool_use(block))),
                Some("tool_result") => Some((Role::Tool, flatten(&block["content"]))),
                _ => None,
            };
            if let Some((role, text)) = found
                && !text.trim().is_empty()
            {
                out.episodes.push(episode(key, role, text, at));
            }
        }
    }
    out
}

/// The text a speaker said, and in what role; None for harness noise.
fn spoken(speaker: Role, text: &str) -> Option<(Role, String)> {
    let text = text.trim();
    if text.is_empty() || HARNESS_PREFIXES.iter().any(|p| text.starts_with(p)) {
        return None;
    }
    // `!` commands: the input is the user's, the output is a tool's.
    if text.starts_with("<bash-stdout>") || text.starts_with("<bash-stderr>") {
        return Some((Role::Tool, text.to_string()));
    }
    Some((speaker, text.to_string()))
}

fn episode(key: String, role: Role, text: String, at: &str) -> NewEpisode {
    let text = match role {
        Role::Tool => truncate(&text, MAX_TOOL_CHARS),
        _ => text,
    };
    NewEpisode {
        key,
        role,
        text,
        at: at.to_string(),
    }
}

fn tool_use(block: &Value) -> String {
    let name = block["name"].as_str().unwrap_or("tool");
    format!("[{name}] {}", block["input"])
}

/// A tool result's text: a string, or the text parts of an array.
fn flatten(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            let mut out = Vec::new();
            for p in parts {
                if let Some(t) = p["text"].as_str() {
                    out.push(t);
                }
            }
            out.join("\n")
        }
        _ => String::new(),
    }
}

/// Parses `conversations.json` from a claude.ai data export (Settings →
/// Privacy → Export data) into one upload per conversation, in order.
pub fn claude_ai_export(json: &str) -> Result<Vec<Upload>> {
    let v: Value = serde_json::from_str(json).context("conversations.json is not JSON")?;
    let conversations = v
        .as_array()
        .context("expected conversations.json to hold an array of conversations")?;
    let mut out = Vec::new();
    for c in conversations {
        let Some(id) = c["uuid"].as_str() else {
            continue;
        };
        let mut episodes = Vec::new();
        for m in c["chat_messages"].as_array().into_iter().flatten() {
            let (Some(uuid), Some(at)) = (m["uuid"].as_str(), m["created_at"].as_str()) else {
                continue;
            };
            let speaker = match m["sender"].as_str() {
                Some("human") => Role::User,
                Some("assistant") => Role::Assistant,
                _ => continue,
            };
            let mut parts = Vec::new();
            match m["content"].as_array() {
                Some(blocks) if !blocks.is_empty() => {
                    for (i, block) in blocks.iter().enumerate() {
                        let found = match block["type"].as_str() {
                            Some("text") => {
                                block["text"].as_str().map(|s| (speaker, s.to_string()))
                            }
                            Some("tool_use") => Some((Role::Tool, tool_use(block))),
                            Some("tool_result") => Some((Role::Tool, flatten(&block["content"]))),
                            _ => None,
                        };
                        if let Some(found) = found {
                            parts.push((format!("{uuid}:{i}"), found));
                        }
                    }
                }
                _ => {
                    if let Some(t) = m["text"].as_str() {
                        parts.push((uuid.to_string(), (speaker, t.to_string())));
                    }
                }
            }
            // Pasted documents and files the user attached: theirs, but not their words.
            for (i, a) in m["attachments"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                if let Some(t) = a["extracted_content"].as_str() {
                    let name = a["file_name"].as_str().unwrap_or("attachment");
                    parts.push((
                        format!("{uuid}:a{i}"),
                        (Role::Tool, format!("[{name}] {t}")),
                    ));
                }
            }
            for (key, (role, text)) in parts {
                if !text.trim().is_empty() {
                    episodes.push(episode(key, role, text.trim().to_string(), at));
                }
            }
        }
        if episodes.is_empty() {
            continue;
        }
        out.push(Upload {
            session: id.to_string(),
            title: c["name"]
                .as_str()
                .map(str::to_string)
                .filter(|n| !n.is_empty()),
            episodes,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_keeps_the_conversation() {
        let lines = [
            r#"{"type":"permission-mode","sessionId":"s"}"#,
            r#"{"type":"user","uuid":"u1","timestamp":"2026-10-26T10:00:00Z","cwd":"/Users/p/code/memory","isSidechain":false,"message":{"role":"user","content":"start the server branch"}}"#,
            r#"{"type":"user","uuid":"u2","timestamp":"2026-10-26T10:00:00Z","isMeta":true,"message":{"role":"user","content":"<system-reminder>injected</system-reminder>"}}"#,
            r#"{"type":"user","uuid":"u3","timestamp":"2026-10-26T10:00:00Z","message":{"role":"user","content":"<local-command-caveat>Caveat</local-command-caveat>"}}"#,
            r#"{"type":"assistant","uuid":"a1","timestamp":"2026-10-26T10:00:01Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":""},{"type":"text","text":"On it."},{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","uuid":"u4","timestamp":"2026-10-26T10:00:02Z","message":{"role":"user","content":[{"type":"tool_result","content":[{"type":"text","text":"Cargo.toml\nsrc"}]}]}}"#,
            r#"{"type":"assistant","uuid":"a2","timestamp":"2026-10-26T10:00:03Z","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"subagent chatter"}]}}"#,
            r#"{"type":"user","uuid":"u5","timestamp":"2026-10-26T10:00:04Z","message":{"role":"user","content":"<bash-stdout>ok</bash-stdout>"}}"#,
            r#"not json"#,
        ]
        .join("\n");
        let parsed = claude_code(&lines);
        assert_eq!(parsed.cwd.as_deref(), Some("/Users/p/code/memory"));
        let got: Vec<(&str, Role, &str)> = parsed
            .episodes
            .iter()
            .map(|e| (e.key.as_str(), e.role, e.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("u1", Role::User, "start the server branch"),
                ("a1:1", Role::Assistant, "On it."),
                ("a1:2", Role::Tool, r#"[Bash] {"command":"ls"}"#),
                ("u4:0", Role::Tool, "Cargo.toml\nsrc"),
                ("u5", Role::Tool, "<bash-stdout>ok</bash-stdout>"),
            ]
        );
    }

    #[test]
    fn claude_ai_export_maps_conversations() -> Result<()> {
        let json = r#"[
          {"uuid":"c1","name":"Bike fit","created_at":"2026-10-01T00:00:00Z","chat_messages":[
            {"uuid":"m1","sender":"human","created_at":"2026-10-01T00:00:00Z","text":"my saddle is too high",
             "content":[{"type":"text","text":"my saddle is too high"}],
             "attachments":[{"file_name":"fit.txt","extracted_content":"stack 580"}]},
            {"uuid":"m2","sender":"assistant","created_at":"2026-10-01T00:00:05Z","text":"",
             "content":[{"type":"text","text":"Lower it 5 mm."},{"type":"tool_use","name":"web_search","input":{"q":"saddle"}}]},
            {"uuid":"m3","sender":"human","created_at":"2026-10-01T00:01:00Z","text":"thanks","content":[]}
          ]},
          {"uuid":"c2","name":"","chat_messages":[]}
        ]"#;
        let uploads = claude_ai_export(json)?;
        assert_eq!(uploads.len(), 1);
        let u = &uploads[0];
        assert_eq!(
            (u.session.as_str(), u.title.as_deref()),
            ("c1", Some("Bike fit"))
        );
        let got: Vec<(&str, Role, &str)> = u
            .episodes
            .iter()
            .map(|e| (e.key.as_str(), e.role, e.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("m1:0", Role::User, "my saddle is too high"),
                ("m1:a0", Role::Tool, "[fit.txt] stack 580"),
                ("m2:0", Role::Assistant, "Lower it 5 mm."),
                ("m2:1", Role::Tool, r#"[web_search] {"q":"saddle"}"#),
                ("m3", Role::User, "thanks"),
            ]
        );
        assert!(claude_ai_export("{}").is_err());
        Ok(())
    }
}
