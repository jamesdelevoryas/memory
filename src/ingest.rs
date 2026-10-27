//! Plain HTTP routes for clients that aren't agents: hooks and importers
//! upload transcripts, and the dreamer's systemd units check for work and ask
//! for a run. Behind the same bearer-token auth as /mcp.

use std::path::PathBuf;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;

use crate::{
    auth::{Client, Level},
    episodes::{Upload, Via},
    store::{Store, WriteError},
};

#[derive(Clone)]
pub struct Ingest {
    pub store: Store,
    pub settings: Settings,
    /// Written on `POST /dream`; a systemd path unit watches it and starts the dreamer.
    pub dream_trigger: Option<PathBuf>,
}

#[derive(Clone, Copy)]
pub struct Settings {
    /// Episodes older than this many days are dropped once distilled, and
    /// not accepted; 0 keeps them forever.
    pub retention_days: u32,
    /// A session waits for the dreamer until it's been quiet this long.
    pub quiet_minutes: u32,
}

impl Settings {
    pub fn not_before(&self) -> Option<String> {
        (self.retention_days > 0).then(|| ago(chrono::Duration::days(self.retention_days.into())))
    }

    pub fn quiet_since(&self) -> String {
        ago(chrono::Duration::minutes(self.quiet_minutes.into()))
    }
}

fn ago(d: chrono::Duration) -> String {
    (chrono::Utc::now() - d).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn router(ingest: Ingest) -> Router {
    Router::new()
        .route(
            "/episodes",
            post(upload).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .route("/episodes/pending", get(pending))
        .route("/dream", post(dream))
        .with_state(ingest)
}

async fn upload(
    State(ingest): State<Ingest>,
    Extension(client): Extension<Client>,
    Json(upload): Json<Upload>,
) -> Response {
    if let Some(e) = forbidden(&client, Level::Add) {
        return e;
    }
    let not_before = ingest.settings.not_before();
    match ingest
        .store
        .upload(
            &client.source,
            Via::Transcript,
            upload,
            not_before.as_deref(),
        )
        .await
    {
        Ok(up) => {
            tracing::info!(
                source = %client.source,
                session = %up.session,
                added = up.added,
                skipped = up.skipped,
                "episodes uploaded"
            );
            Json(up).into_response()
        }
        Err(WriteError::Other(e)) => internal(e),
        Err(e @ WriteError::Conflict(_)) => (StatusCode::CONFLICT, e.to_string()).into_response(),
        Err(e @ WriteError::Invalid(_)) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

/// How many sessions the dreamer would read now.
async fn pending(State(ingest): State<Ingest>, Extension(client): Extension<Client>) -> Response {
    if let Some(e) = forbidden(&client, Level::Read) {
        return e;
    }
    let quiet = ingest.settings.quiet_since();
    match ingest.store.undistilled(Some(&quiet), 1000).await {
        Ok(sessions) => Json(json!({ "sessions": sessions.len() })).into_response(),
        Err(e) => internal(e),
    }
}

/// Asks for a dreamer run now, e.g. when a new agent session starts.
async fn dream(State(ingest): State<Ingest>, Extension(client): Extension<Client>) -> Response {
    if let Some(e) = forbidden(&client, Level::Add) {
        return e;
    }
    let Some(trigger) = &ingest.dream_trigger else {
        return (
            StatusCode::NOT_FOUND,
            "no dreamer is set up on this server\n",
        )
            .into_response();
    };
    let stamp = chrono::Utc::now().to_rfc3339();
    if let Err(e) = tokio::fs::write(trigger, format!("{stamp} {}\n", client.source)).await {
        return internal(anyhow::Error::new(e).context(format!("writing {}", trigger.display())));
    }
    tracing::info!(source = %client.source, "dream requested");
    (StatusCode::ACCEPTED, Json(json!({ "requested": stamp }))).into_response()
}

/// A 403 if `client`'s token is below `level`.
fn forbidden(client: &Client, level: Level) -> Option<Response> {
    if client.level >= level {
        return None;
    }
    let message = format!(
        "token for {} has {:?} access; this needs {level:?}\n",
        client.source, client.level
    );
    Some((StatusCode::FORBIDDEN, message).into_response())
}

fn internal(e: anyhow::Error) -> Response {
    tracing::error!("{e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}\n")).into_response()
}
