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

/// What the first lines of a Codex rollout file say about its session.
pub struct CodexMeta {
    pub session: String,
    pub cwd: Option<String>,
    /// Codex copies other agents' sessions (Claude Code's) into rollout files
    /// of its own; those are already uploaded by their own client.
    pub imported: bool,
}

/// Reads a Codex rollout's `session_meta` and first turn from its first lines.
pub fn codex_meta(first_lines: &str) -> Option<CodexMeta> {
    let mut meta: Option<CodexMeta> = None;
    for line in first_lines.lines().take(20) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let p = &v["payload"];
        match (v["type"].as_str(), p["type"].as_str()) {
            (Some("session_meta"), _) => {
                meta = Some(CodexMeta {
                    session: p["id"].as_str().or(p["session_id"].as_str())?.to_string(),
                    cwd: p["cwd"].as_str().map(str::to_string),
                    imported: p["history_mode"] == "legacy" && p["thread_source"].is_null(),
                });
            }
            (Some("event_msg"), Some("task_started")) => {
                let imported = p["turn_id"]
                    .as_str()
                    .is_some_and(|t| t.starts_with("external-import"));
                if let Some(m) = &mut meta {
                    m.imported |= imported;
                }
                break;
            }
            _ => {}
        }
    }
    meta
}

/// Parses complete lines of a Codex rollout file
/// (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`), the first of which starts
/// at byte `base` of the file. Newer files record each finished item
/// (`item_completed`, keyed by the item's id); older ones record messages as
/// `user_message` and `agent_message` events, keyed by their offset.
pub fn codex(lines: &str, base: u64) -> Vec<NewEpisode> {
    let mut out = Vec::new();
    let mut offset = base;
    for line in lines.split_inclusive('\n') {
        let at_offset = offset;
        offset += line.len() as u64;
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(at) = v["timestamp"].as_str() else {
            continue;
        };
        let p = &v["payload"];
        if v["type"] != "event_msg" {
            continue;
        }
        let found = match p["type"].as_str() {
            Some("item_completed") => codex_item(&p["item"]),
            Some("user_message") => p["message"]
                .as_str()
                .and_then(|m| spoken(Role::User, m))
                .map(|e| (format!("@{at_offset}"), e)),
            Some("agent_message") => p["message"]
                .as_str()
                .and_then(|m| spoken(Role::Assistant, m))
                .map(|e| (format!("@{at_offset}"), e)),
            _ => None,
        };
        if let Some((key, (role, text))) = found
            && !text.trim().is_empty()
        {
            out.push(episode(key, role, text, at));
        }
    }
    out
}

/// One finished item of a Codex turn: what was said, or a short line for a
/// tool call. Reasoning is dropped.
fn codex_item(item: &Value) -> Option<(String, (Role, String))> {
    let id = item["id"].as_str()?.to_string();
    let texts = || {
        item["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let found = match item["type"].as_str()? {
        "UserMessage" => spoken(Role::User, &texts()),
        "AgentMessage" => spoken(Role::Assistant, &texts()),
        "Reasoning" => None,
        "McpToolCall" => Some((
            Role::Tool,
            format!(
                "[{}.{}] {}\n{}",
                item["server"].as_str().unwrap_or("mcp"),
                item["tool"].as_str().unwrap_or("tool"),
                item["arguments"],
                item["result"]
            ),
        )),
        "CommandExecution" => Some((
            Role::Tool,
            format!(
                "[exec] {}\n{}",
                item["command"],
                item["aggregated_output"].as_str().unwrap_or_default()
            ),
        )),
        other => Some((Role::Tool, format!("[{other}] {item}"))),
    };
    found.map(|f| (id, f))
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

    #[test]
    fn codex_keeps_the_conversation_and_spots_imports() {
        let lines = [
            r#"{"timestamp":"2026-11-08T01:10:08Z","type":"session_meta","payload":{"session_id":"s1","id":"s1","cwd":"/Users/p/code","history_mode":"paginated","thread_source":"user"}}"#,
            r#"{"timestamp":"2026-11-08T01:10:08Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#,
            r#"{"timestamp":"2026-11-08T01:10:09Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"You are Codex"}]}}"#,
            r#"{"timestamp":"2026-11-08T01:10:09Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","id":"i1","content":[{"type":"text","text":"how is the transcript rendered?\n"}]}}}"#,
            r#"{"timestamp":"2026-11-08T01:10:10Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"Reasoning","id":"i2","summary_text":[]}}}"#,
            r#"{"timestamp":"2026-11-08T01:10:11Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"McpToolCall","id":"i3","server":"memory","tool":"memory_index","arguments":{},"result":"ok"}}}"#,
            r#"{"timestamp":"2026-11-08T01:10:12Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","id":"i4","command":["ls"],"aggregated_output":"src\n"}}}"#,
            r#"{"timestamp":"2026-11-08T01:10:13Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"i5","content":[{"type":"Text","text":"A summary, not a dump."}],"phase":"final_answer"}}}"#,
            r#"{"timestamp":"2026-11-08T01:10:14Z","type":"event_msg","payload":{"type":"user_message","message":"older format"}}"#,
            r#"{"timestamp":"2026-11-08T01:10:14Z","type":"event_msg","payload":{"type":"token_count"}}"#,
        ]
        .join("\n")
            + "\n";
        let meta = codex_meta(&lines).unwrap();
        assert_eq!(
            (meta.session.as_str(), meta.cwd.as_deref(), meta.imported),
            ("s1", Some("/Users/p/code"), false)
        );
        let episodes = codex(&lines, 100);
        let got: Vec<(&str, Role, &str)> = episodes
            .iter()
            .map(|e| (e.key.as_str(), e.role, e.text.as_str()))
            .collect();
        let older = format!("@{}", 100 + lines.find(r#"{"timestamp":"2026-11-08T01:10:14Z","type":"event_msg","payload":{"type":"user_message""#).unwrap());
        assert_eq!(
            got,
            vec![
                ("i1", Role::User, "how is the transcript rendered?"),
                ("i3", Role::Tool, "[memory.memory_index] {}\n\"ok\""),
                ("i4", Role::Tool, "[exec] [\"ls\"]\nsrc\n"),
                ("i5", Role::Assistant, "A summary, not a dump."),
                (older.as_str(), Role::User, "older format"),
            ]
        );

        let imported = [
            r#"{"timestamp":"2026-11-08T01:09:22Z","type":"session_meta","payload":{"id":"s2","cwd":"/Users/p","history_mode":"legacy"}}"#,
            r#"{"timestamp":"2026-11-08T01:09:22Z","type":"event_msg","payload":{"type":"task_started","turn_id":"external-import-turn-1"}}"#,
        ]
        .join("\n");
        assert!(codex_meta(&imported).unwrap().imported);
    }
}
