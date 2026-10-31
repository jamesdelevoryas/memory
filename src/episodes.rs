//! Episodes: the raw turns of conversations, uploaded while they happen by
//! hooks, importers, and (for claude.ai) the model itself. They're the
//! episodic layer under the notes: agents search them with memory_recall, and
//! the dreamer reads each one once, distills what's worth keeping into notes,
//! and records how far it got in each session.

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use turso::{Value, transaction::TransactionBehavior};

use crate::store::{
    Origin, Store, WriteError, enum_str, now, opt_text, parse_enum, required, text,
};

// Episode ids are UUIDv7s assigned on arrival, so they sort in arrival order;
// a session's `distilled_through` is the last one the dreamer has read.
// Episodes are unique per source by the client's own message id (`key`), so
// re-uploading a transcript, or a resumed session that copies old messages
// into a new file, adds nothing.
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS sessions (
  id                TEXT PRIMARY KEY,  -- <source>:<the client's session id>
  source            TEXT NOT NULL,
  via               TEXT NOT NULL,     -- transcript | model
  title             TEXT,
  distilled_through TEXT,
  summary           TEXT,              -- the dreamer's running summary
  created_at        TEXT NOT NULL,
  updated_at        TEXT NOT NULL      -- when episodes last arrived
);
CREATE TABLE IF NOT EXISTS episodes (
  id       TEXT PRIMARY KEY,
  session  TEXT NOT NULL,
  source   TEXT NOT NULL,
  key      TEXT NOT NULL,
  role     TEXT NOT NULL,             -- user | assistant | tool
  text     TEXT NOT NULL,
  at       TEXT NOT NULL              -- when it was said
);
";

pub(crate) const INDEXES: &str = "
CREATE UNIQUE INDEX IF NOT EXISTS episodes_key ON episodes (source, key);
CREATE INDEX IF NOT EXISTS episodes_session ON episodes (session, id);
CREATE INDEX IF NOT EXISTS episodes_fts ON episodes USING fts (text);
";

const SESSION_COLUMNS: &str = "s.id, s.source, s.via, s.title, s.distilled_through, s.summary, \
    s.created_at, s.updated_at, \
    (SELECT count(*) FROM episodes e WHERE e.session = s.id), \
    (SELECT count(*) FROM episodes e WHERE e.session = s.id \
       AND e.id > coalesce(s.distilled_through, ''))";

const UNDISTILLED: &str = "EXISTS (SELECT 1 FROM episodes e WHERE e.session = s.id \
    AND e.id > coalesce(s.distilled_through, ''))";

pub const MAX_SESSION_CHARS: usize = 128;
pub const MAX_KEY_CHARS: usize = 200;
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_EPISODE_CHARS: usize = 20_000;
pub const MAX_SUMMARY_CHARS: usize = 2_000;
pub const MAX_UPLOAD_EPISODES: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Something the user said.
    User,
    /// The assistant's reply.
    Assistant,
    /// A tool call, a tool's output, or an attachment.
    Tool,
}

/// How a session's episodes arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    /// Copied from the client's own transcript by a hook or an import: exact.
    Transcript,
    /// Reported by the model through memory_log_turn: may be paraphrased.
    Model,
}

/// One message as a client uploads it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewEpisode {
    /// The client's id for the message, unique for its source.
    pub key: String,
    pub role: Role,
    pub text: String,
    /// When it was said (RFC 3339).
    pub at: String,
}

/// A batch of new messages from one session: the body of `POST /episodes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Upload {
    /// The client's id for the session.
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub episodes: Vec<NewEpisode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Uploaded {
    /// The session's full id, `<source>:<client session id>`.
    pub session: String,
    pub added: u64,
    /// Already uploaded, or older than the retention window.
    pub skipped: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Episode {
    pub id: String,
    pub session: String,
    pub role: Role,
    /// The most a fact taken from this episode can be trusted: only the
    /// user's own words in an exact transcript are `user_stated`.
    pub origin: Origin,
    pub at: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Session {
    pub id: String,
    pub source: String,
    pub via: Via,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Episodes stored for this session.
    pub episodes: i64,
    /// Episodes the dreamer hasn't read yet.
    pub undistilled: i64,
    /// The last episode the dreamer has read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distilled_through: Option<String>,
    /// The dreamer's running summary of the session so far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub created_at: String,
    /// When episodes last arrived.
    pub updated_at: String,
}

pub fn origin(role: Role, via: Via) -> Origin {
    match (role, via) {
        (Role::User, Via::Transcript) => Origin::UserStated,
        (Role::User, Via::Model) | (Role::Assistant, _) => Origin::AgentInferred,
        (Role::Tool, _) => Origin::External,
    }
}

impl Store {
    /// Stores a batch of messages from one session, creating the session on
    /// its first message. Messages already stored for `source`, and messages
    /// said before `not_before`, are skipped.
    pub async fn upload(
        &self,
        source: &str,
        via: Via,
        upload: Upload,
        not_before: Option<&str>,
    ) -> Result<Uploaded, WriteError> {
        let client_session = upload.session.trim();
        if client_session.is_empty()
            || client_session.chars().count() > MAX_SESSION_CHARS
            || client_session.contains(char::is_whitespace)
        {
            return Err(WriteError::Invalid(format!(
                "session must be a non-empty id without spaces, at most {MAX_SESSION_CHARS} characters"
            )));
        }
        if upload.episodes.len() > MAX_UPLOAD_EPISODES {
            return Err(WriteError::Invalid(format!(
                "at most {MAX_UPLOAD_EPISODES} episodes per upload"
            )));
        }
        let id = format!("{source}:{client_session}");
        let title = upload
            .title
            .map(|t| {
                truncate(
                    &t.split_whitespace().collect::<Vec<_>>().join(" "),
                    MAX_TITLE_CHARS,
                )
            })
            .filter(|t| !t.is_empty());
        // Empty messages (a tool that printed nothing) are skipped rather than
        // rejected, so one can't wedge a hook that resends until it succeeds.
        let mut episodes = Vec::new();
        let mut empty = 0;
        for e in upload.episodes {
            if e.text.trim().is_empty() {
                empty += 1;
                continue;
            }
            episodes.push(check_episode(e).map_err(|e| WriteError::Invalid(format!("{e:#}")))?);
        }

        let mut conn = self.conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await?;
        let mut rows = tx
            .query(
                "SELECT via FROM sessions WHERE id = ?1",
                vec![Value::Text(id.clone())],
            )
            .await?;
        let mut exists = match rows.next().await? {
            Some(row) => {
                let existing: Via = parse_enum(&required(&row, 0)?)?;
                if existing != via {
                    return Err(WriteError::Invalid(format!(
                        "session {id:?} holds {} episodes; it can't also take {}",
                        enum_str(&existing),
                        enum_str(&via)
                    )));
                }
                true
            }
            None => false,
        };
        drop(rows);

        let now = now();
        let (mut added, mut skipped) = (0, empty);
        for e in episodes {
            if not_before.is_some_and(|nb| e.at.as_str() < nb) {
                skipped += 1;
                continue;
            }
            let mut rows = tx
                .query(
                    "SELECT 1 FROM episodes WHERE source = ?1 AND key = ?2",
                    vec![Value::Text(source.to_string()), Value::Text(e.key.clone())],
                )
                .await?;
            let duplicate = rows.next().await?.is_some();
            drop(rows);
            if duplicate {
                skipped += 1;
                continue;
            }
            if !exists {
                tx.execute(
                    "INSERT INTO sessions (id, source, via, title, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                    vec![
                        Value::Text(id.clone()),
                        Value::Text(source.to_string()),
                        Value::Text(enum_str(&via)),
                        opt_text(title.clone()),
                        Value::Text(now.clone()),
                    ],
                )
                .await?;
                exists = true;
            }
            tx.execute(
                "INSERT INTO episodes (id, session, source, key, role, text, at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                vec![
                    Value::Text(uuid::Uuid::now_v7().to_string()),
                    Value::Text(id.clone()),
                    Value::Text(source.to_string()),
                    Value::Text(e.key),
                    Value::Text(enum_str(&e.role)),
                    Value::Text(e.text),
                    Value::Text(e.at),
                ],
            )
            .await?;
            added += 1;
        }
        if added > 0 {
            tx.execute(
                "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                vec![Value::Text(id.clone()), Value::Text(now)],
            )
            .await?;
        }
        if exists && let Some(title) = title {
            tx.execute(
                "UPDATE sessions SET title = ?2 WHERE id = ?1",
                vec![Value::Text(id.clone()), Value::Text(title)],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(Uploaded {
            session: id,
            added,
            skipped,
        })
    }

    /// Full-text search over episodes, best match first. `sources` empty
    /// means all.
    pub async fn recall(
        &self,
        query: &str,
        sources: &[String],
        limit: u32,
    ) -> Result<Vec<Episode>> {
        let conn = self.conn()?;
        let mut sql = "SELECT id, session, role, text, at, fts_score(text, ?1) AS score \
             FROM episodes WHERE fts_match(text, ?1)"
            .to_string();
        let mut params = vec![Value::Text(query.to_string())];
        if !sources.is_empty() {
            let mut placeholders = Vec::new();
            for s in sources {
                params.push(Value::Text(s.clone()));
                placeholders.push(format!("?{}", params.len()));
            }
            sql.push_str(&format!(" AND source IN ({})", placeholders.join(", ")));
        }
        sql.push_str(&format!(" ORDER BY score DESC LIMIT {limit}"));
        let mut rows = conn.query(sql, params).await?;
        let mut found = Vec::new();
        while let Some(row) = rows.next().await? {
            found.push((
                required(&row, 0)?,
                required(&row, 1)?,
                parse_enum::<Role>(&required(&row, 2)?)?,
                required(&row, 3)?,
                required(&row, 4)?,
            ));
        }
        drop(rows);

        let mut vias: Vec<(String, Via)> = Vec::new();
        let mut out = Vec::new();
        for (id, session, role, text, at) in found {
            let via = match vias.iter().find(|(s, _)| *s == session) {
                Some((_, via)) => *via,
                None => {
                    let via = session_via(&conn, &session).await?;
                    vias.push((session.clone(), via));
                    via
                }
            };
            out.push(Episode {
                id,
                session,
                role,
                origin: origin(role, via),
                at,
                text,
            });
        }
        Ok(out)
    }

    /// A session and its episodes in arrival order, after the episode `after`.
    pub async fn session(
        &self,
        id: &str,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Option<(Session, Vec<Episode>)>> {
        let conn = self.conn()?;
        let mut rows = conn
            .query(
                format!("SELECT {SESSION_COLUMNS} FROM sessions s WHERE s.id = ?1"),
                vec![Value::Text(id.to_string())],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        let session = row_to_session(&row)?;
        drop(rows);
        let mut rows = conn
            .query(
                format!(
                    "SELECT id, role, text, at FROM episodes WHERE session = ?1 AND id > ?2 \
                     ORDER BY id LIMIT {limit}"
                ),
                vec![
                    Value::Text(id.to_string()),
                    Value::Text(after.unwrap_or_default().to_string()),
                ],
            )
            .await?;
        let mut episodes = Vec::new();
        while let Some(row) = rows.next().await? {
            let role: Role = parse_enum(&required(&row, 1)?)?;
            episodes.push(Episode {
                id: required(&row, 0)?,
                session: session.id.clone(),
                role,
                origin: origin(role, session.via),
                at: required(&row, 3)?,
                text: required(&row, 2)?,
            });
        }
        Ok(Some((session, episodes)))
    }

    /// Sessions with episodes the dreamer hasn't read, most recently active
    /// first. With `quiet_since`, only sessions with nothing new since then,
    /// so the dreamer doesn't distill a conversation mid-thought.
    pub async fn undistilled(&self, quiet_since: Option<&str>, limit: u32) -> Result<Vec<Session>> {
        let mut sql = format!("SELECT {SESSION_COLUMNS} FROM sessions s WHERE {UNDISTILLED}");
        let mut params = Vec::new();
        if let Some(q) = quiet_since {
            params.push(Value::Text(q.to_string()));
            sql.push_str(" AND s.updated_at <= ?1");
        }
        sql.push_str(&format!(" ORDER BY s.updated_at DESC LIMIT {limit}"));
        let mut rows = self.conn()?.query(sql, params).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_session(&row)?);
        }
        Ok(out)
    }

    /// Records that the dreamer has read `session` through the episode
    /// `through`, with its running summary. The mark only moves forward.
    pub async fn mark_distilled(
        &self,
        session: &str,
        through: &str,
        summary: Option<String>,
    ) -> Result<(), WriteError> {
        let summary = summary
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if summary
            .as_ref()
            .is_some_and(|s| s.chars().count() > MAX_SUMMARY_CHARS)
        {
            return Err(WriteError::Invalid(format!(
                "summary is longer than {MAX_SUMMARY_CHARS} characters"
            )));
        }
        let mut conn = self.conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await?;
        let mut rows = tx
            .query(
                "SELECT distilled_through FROM sessions WHERE id = ?1",
                vec![Value::Text(session.to_string())],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Err(WriteError::Invalid(format!(
                "there is no session {session:?}"
            )));
        };
        let current = text(&row, 0)?;
        drop(rows);
        let mut rows = tx
            .query(
                "SELECT 1 FROM episodes WHERE session = ?1 AND id = ?2",
                vec![
                    Value::Text(session.to_string()),
                    Value::Text(through.to_string()),
                ],
            )
            .await?;
        if rows.next().await?.is_none() {
            return Err(WriteError::Invalid(format!(
                "session {session:?} has no episode {through:?}"
            )));
        }
        drop(rows);
        if let Some(current) = current
            && through < current.as_str()
        {
            return Err(WriteError::Conflict(format!(
                "session {session:?} is already distilled through {current}, past {through}"
            )));
        }
        tx.execute(
            "UPDATE sessions SET distilled_through = ?2, summary = coalesce(?3, summary) \
             WHERE id = ?1",
            vec![
                Value::Text(session.to_string()),
                Value::Text(through.to_string()),
                opt_text(summary),
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Deletes episodes said before `cutoff` that the dreamer has read, then
    /// sessions left empty. Episodes it hasn't read yet are kept.
    pub async fn expire_episodes(&self, cutoff: &str) -> Result<u64> {
        let conn = self.conn()?;
        let deleted = conn
            .execute(
                "DELETE FROM episodes WHERE at < ?1 AND id <= coalesce(
                   (SELECT s.distilled_through FROM sessions s WHERE s.id = episodes.session), '')",
                vec![Value::Text(cutoff.to_string())],
            )
            .await?;
        conn.execute(
            "DELETE FROM sessions WHERE updated_at < ?1
               AND NOT EXISTS (SELECT 1 FROM episodes e WHERE e.session = sessions.id)",
            vec![Value::Text(cutoff.to_string())],
        )
        .await?;
        Ok(deleted)
    }
}

async fn session_via(conn: &turso::Connection, id: &str) -> Result<Via> {
    let mut rows = conn
        .query(
            "SELECT via FROM sessions WHERE id = ?1",
            vec![Value::Text(id.to_string())],
        )
        .await?;
    let row = rows
        .next()
        .await?
        .with_context(|| format!("episode belongs to missing session {id:?}"))?;
    parse_enum(&required(&row, 0)?)
}

fn check_episode(e: NewEpisode) -> Result<NewEpisode> {
    let key = e.key.trim().to_string();
    if key.is_empty() || key.chars().count() > MAX_KEY_CHARS {
        bail!("episode key must be non-empty and at most {MAX_KEY_CHARS} characters");
    }
    let at = chrono::DateTime::parse_from_rfc3339(e.at.trim())
        .with_context(|| format!("episode {key:?}: at must be RFC 3339, got {:?}", e.at))?
        .with_timezone(&chrono::Utc)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    Ok(NewEpisode {
        key,
        role: e.role,
        text: truncate(e.text.trim(), MAX_EPISODE_CHARS),
        at,
    })
}

/// `s` cut to at most `max` characters, marked when cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(2)).collect();
    out.push_str(" …");
    out
}

fn integer(row: &turso::Row, i: usize) -> Result<i64> {
    match row.get_value(i)? {
        Value::Integer(n) => Ok(n),
        other => bail!("column {i}: expected an integer, got {other:?}"),
    }
}

fn row_to_session(row: &turso::Row) -> Result<Session> {
    Ok(Session {
        id: required(row, 0)?,
        source: required(row, 1)?,
        via: parse_enum(&required(row, 2)?)?,
        title: text(row, 3)?,
        distilled_through: text(row, 4)?,
        summary: text(row, 5)?,
        created_at: required(row, 6)?,
        updated_at: required(row, 7)?,
        episodes: integer(row, 8)?,
        undistilled: integer(row, 9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_store() -> Result<Store> {
        let dir = std::env::temp_dir().join(format!("memory-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir)?;
        Store::open(dir.join("t.db").to_str().unwrap()).await
    }

    fn ep(key: &str, role: Role, text: &str, at: &str) -> NewEpisode {
        NewEpisode {
            key: key.into(),
            role,
            text: text.into(),
            at: at.into(),
        }
    }

    fn upload(session: &str, episodes: Vec<NewEpisode>) -> Upload {
        Upload {
            session: session.into(),
            title: Some("~/code/memory".into()),
            episodes,
        }
    }

    #[tokio::test]
    async fn upload_dedupes_and_recall_finds() -> Result<()> {
        let store = temp_store().await?;
        let first = store
            .upload(
                "claude-code",
                Via::Transcript,
                upload(
                    "s1",
                    vec![
                        ep(
                            "m1",
                            Role::User,
                            "I moved to Munich last week",
                            "2026-10-20T10:00:00Z",
                        ),
                        ep(
                            "m2",
                            Role::Assistant,
                            "Noted: Munich.",
                            "2026-10-20T10:00:05+00:00",
                        ),
                        ep(
                            "m3",
                            Role::Tool,
                            "curl output mentioning Munich",
                            "2026-10-20T10:00:06Z",
                        ),
                    ],
                ),
                None,
            )
            .await?;
        assert_eq!(
            (first.session.as_str(), first.added, first.skipped),
            ("claude-code:s1", 3, 0)
        );

        // The same messages again, even under another session id, add nothing.
        let again = store
            .upload(
                "claude-code",
                Via::Transcript,
                upload(
                    "s2",
                    vec![
                        ep(
                            "m1",
                            Role::User,
                            "I moved to Munich last week",
                            "2026-10-20T10:00:00Z",
                        ),
                        ep(
                            "m4",
                            Role::User,
                            "The bike is a Trek",
                            "2026-10-20T10:01:00Z",
                        ),
                    ],
                ),
                None,
            )
            .await?;
        assert_eq!((again.added, again.skipped), (1, 1));
        // Another source has its own key space.
        let other = store
            .upload(
                "codex",
                Via::Transcript,
                upload(
                    "s1",
                    vec![ep("m1", Role::User, "hello there", "2026-10-20T10:00:00Z")],
                ),
                None,
            )
            .await?;
        assert_eq!(other.added, 1);

        let hits = store.recall("munich", &[], 10).await?;
        assert_eq!(hits.len(), 3);
        let origins: Vec<(Role, Origin)> = hits.iter().map(|e| (e.role, e.origin)).collect();
        assert!(origins.contains(&(Role::User, Origin::UserStated)));
        assert!(origins.contains(&(Role::Assistant, Origin::AgentInferred)));
        assert!(origins.contains(&(Role::Tool, Origin::External)));
        assert_eq!(hits[0].at.len(), "2026-10-20T10:00:00.000Z".len());
        assert_eq!(store.recall("hello", &["codex".into()], 10).await?.len(), 1);
        assert!(
            store
                .recall("hello", &["claude-code".into()], 10)
                .await?
                .is_empty()
        );

        // Bad input is rejected whole.
        for bad in [
            upload("has space", vec![]),
            upload("s3", vec![ep("", Role::User, "x", "2026-10-20T10:00:00Z")]),
            upload("s3", vec![ep("k", Role::User, "x", "yesterday")]),
        ] {
            let r = store
                .upload("claude-code", Via::Transcript, bad, None)
                .await;
            assert!(matches!(r, Err(WriteError::Invalid(_))));
        }
        // Empty messages are skipped, not rejected, so they can't wedge a hook.
        let blank = store
            .upload(
                "claude-code",
                Via::Transcript,
                upload(
                    "s3",
                    vec![
                        ep("k1", Role::Tool, "  ", "2026-10-20T10:00:00Z"),
                        ep("k2", Role::User, "real", "2026-10-20T10:00:00Z"),
                    ],
                ),
                None,
            )
            .await?;
        assert_eq!((blank.added, blank.skipped), (1, 1));
        // A session's episodes all arrive the same way.
        let mixed = store
            .upload(
                "claude-code",
                Via::Model,
                upload(
                    "s1",
                    vec![ep("m9", Role::User, "x", "2026-10-20T10:00:00Z")],
                ),
                None,
            )
            .await;
        assert!(matches!(mixed, Err(WriteError::Invalid(_))));
        Ok(())
    }

    #[tokio::test]
    async fn model_reported_turns_are_never_user_stated() -> Result<()> {
        let store = temp_store().await?;
        store
            .upload(
                "claude-ai",
                Via::Model,
                upload(
                    "c1",
                    vec![ep(
                        "k1",
                        Role::User,
                        "my sister is called Ana",
                        "2026-10-20T10:00:00Z",
                    )],
                ),
                None,
            )
            .await?;
        let hits = store.recall("sister", &[], 10).await?;
        assert_eq!(hits[0].origin, Origin::AgentInferred);
        Ok(())
    }

    #[tokio::test]
    async fn dreamer_reads_each_episode_once() -> Result<()> {
        let store = temp_store().await?;
        let at = "2026-10-20T10:00:00Z";
        store
            .upload(
                "claude-code",
                Via::Transcript,
                upload(
                    "s1",
                    vec![
                        ep("a", Role::User, "one", at),
                        ep("b", Role::Assistant, "two", at),
                    ],
                ),
                None,
            )
            .await?;
        assert!(store.session("claude-code:nope", None, 10).await?.is_none());
        let (session, episodes) = store.session("claude-code:s1", None, 10).await?.unwrap();
        assert_eq!(
            (
                session.episodes,
                session.undistilled,
                session.title.as_deref()
            ),
            (2, 2, Some("~/code/memory"))
        );
        assert_eq!(
            episodes.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            vec!["one", "two"]
        );
        // Paging by the last id seen.
        let (_, rest) = store
            .session("claude-code:s1", Some(&episodes[0].id), 10)
            .await?
            .unwrap();
        assert_eq!(rest.len(), 1);

        // Active sessions wait for the quiet period; `None` shows them all.
        let long_ago = "2026-01-01T00:00:00.000Z";
        assert!(store.undistilled(Some(long_ago), 10).await?.is_empty());
        assert_eq!(store.undistilled(Some(&now()), 10).await?.len(), 1);
        assert_eq!(store.undistilled(None, 10).await?.len(), 1);

        store
            .mark_distilled(
                "claude-code:s1",
                &episodes[0].id,
                Some("talked about one".into()),
            )
            .await?;
        let (session, _) = store.session("claude-code:s1", None, 10).await?.unwrap();
        assert_eq!(session.undistilled, 1);
        assert_eq!(session.summary.as_deref(), Some("talked about one"));
        store
            .mark_distilled("claude-code:s1", &episodes[1].id, None)
            .await?;
        assert!(store.undistilled(None, 10).await?.is_empty());
        let (session, _) = store.session("claude-code:s1", None, 10).await?.unwrap();
        assert_eq!(session.summary.as_deref(), Some("talked about one"));

        // The mark doesn't move backwards, and must name one of the session's episodes.
        assert!(matches!(
            store
                .mark_distilled("claude-code:s1", &episodes[0].id, None)
                .await,
            Err(WriteError::Conflict(_))
        ));
        assert!(matches!(
            store.mark_distilled("claude-code:s1", "bogus", None).await,
            Err(WriteError::Invalid(_))
        ));

        // New turns in the same session are pending again.
        store
            .upload(
                "claude-code",
                Via::Transcript,
                upload("s1", vec![ep("c", Role::User, "three", at)]),
                None,
            )
            .await?;
        let pending = store.undistilled(None, 10).await?;
        assert_eq!((pending.len(), pending[0].undistilled), (1, 1));
        Ok(())
    }

    #[tokio::test]
    async fn retention_drops_only_distilled_old_episodes() -> Result<()> {
        let store = temp_store().await?;
        let old = "2026-01-01T00:00:00Z";
        let new = "2026-10-20T00:00:00Z";
        let cutoff = "2026-06-01T00:00:00.000Z";
        let up = store
            .upload(
                "claude-code",
                Via::Transcript,
                upload(
                    "s1",
                    vec![
                        ep("a", Role::User, "ancient fact", old),
                        ep("b", Role::User, "older fact", old),
                        ep("c", Role::User, "fresh fact", new),
                    ],
                ),
                None,
            )
            .await?;
        assert_eq!(up.added, 3);
        // Nothing is distilled yet, so nothing expires.
        assert_eq!(store.expire_episodes(cutoff).await?, 0);
        let (_, episodes) = store.session("claude-code:s1", None, 10).await?.unwrap();
        store
            .mark_distilled("claude-code:s1", &episodes[0].id, None)
            .await?;
        assert_eq!(store.expire_episodes(cutoff).await?, 1);
        assert!(store.recall("ancient", &[], 10).await?.is_empty());
        assert_eq!(store.recall("older OR fresh", &[], 10).await?.len(), 2);

        // Uploads older than the window are skipped.
        let late = store
            .upload(
                "claude-code",
                Via::Transcript,
                upload("s9", vec![ep("z", Role::User, "stale", old)]),
                Some(cutoff),
            )
            .await?;
        assert_eq!((late.added, late.skipped), (0, 1));
        assert!(store.session("claude-code:s9", None, 10).await?.is_none());
        Ok(())
    }
}
