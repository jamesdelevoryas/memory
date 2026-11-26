//! OAuth 2.1 for MCP clients that can't send a static `Authorization` header
//! (Gemini, ChatGPT, and other hosted apps), per the MCP authorization spec:
//! protected-resource and authorization-server metadata, dynamic client
//! registration, and the authorization-code flow with PKCE.
//!
//! The access token a client ends up with is one of the ordinary bearer
//! tokens from the tokens file: on the approval page the owner pastes the
//! token minted for that client (`deploy/token.sh gemini consolidate`), and
//! the client receives it. So OAuth only changes how a token is delivered;
//! sources, levels and revocation work exactly as for every other client, and
//! nothing new is stored on the server.
//!
//! Clients are either pre-registered in the OAuth clients file (one per line:
//! `<client-id> <sha256-of-secret> [redirect-uri...]`, for apps like Gemini
//! that ask for a client ID and secret), or register themselves (RFC 7591).
//! Self-registered clients are stateless: their redirect URIs are carried in
//! the client ID and signed with a key kept next to the database, so a restart
//! forgets nothing.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use axum::{
    Form, Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::auth::{self, Tokens};

/// How long an authorization code stays redeemable.
const CODE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone)]
pub struct OAuth(Arc<Inner>);

struct Inner {
    public_url: Option<String>,
    tokens: Tokens,
    clients_path: PathBuf,
    clients: RwLock<HashMap<String, Registered>>,
    key: [u8; 32],
    codes: Mutex<HashMap<String, Grant>>,
}

#[derive(Clone)]
struct Registered {
    secret_hash: String,
    redirect_uris: Vec<String>,
}

struct Grant {
    client_id: String,
    redirect_uri: String,
    challenge: Option<String>,
    token: String,
    expires: Instant,
}

/// A client as seen by the flow, whichever way it was registered.
struct ClientInfo {
    name: String,
    /// Empty only for a pre-registered client that pinned none: then any
    /// https (or loopback) URI is accepted, since redeeming the code still
    /// takes the client secret.
    redirect_uris: Vec<String>,
    secret: Secret,
}

enum Secret {
    /// A public client: no secret, so PKCE is required instead.
    None,
    Hash(String),
}

/// What a self-registered client ID carries.
#[derive(Serialize, Deserialize)]
struct Dynamic {
    #[serde(rename = "n")]
    name: String,
    #[serde(rename = "r")]
    redirect_uris: Vec<String>,
    /// Whether the client asked for a secret.
    #[serde(rename = "c")]
    confidential: bool,
}

impl OAuth {
    pub fn new(
        public_url: Option<String>,
        tokens: Tokens,
        clients_path: &Path,
        data_dir: &Path,
    ) -> Result<Self> {
        let clients = parse_clients(clients_path)?;
        Ok(Self(Arc::new(Inner {
            public_url,
            tokens,
            clients_path: clients_path.to_path_buf(),
            clients: RwLock::new(clients),
            key: load_key(&data_dir.join("oauth-key"))?,
            codes: Mutex::new(HashMap::new()),
        })))
    }

    /// Re-reads the OAuth clients file. On error the current clients stay.
    pub fn reload(&self) -> Result<usize> {
        let clients = parse_clients(&self.0.clients_path)?;
        let n = clients.len();
        *self.0.clients.write().unwrap() = clients;
        Ok(n)
    }

    fn base(&self, headers: &HeaderMap) -> String {
        auth::base_url(self.0.public_url.as_deref(), headers)
    }

    fn client(&self, id: &str) -> Option<ClientInfo> {
        if let Some(reg) = self.0.clients.read().unwrap().get(id) {
            return Some(ClientInfo {
                name: id.to_string(),
                redirect_uris: reg.redirect_uris.clone(),
                secret: Secret::Hash(reg.secret_hash.clone()),
            });
        }
        let rest = id.strip_prefix("dyn.")?;
        let (payload, sig) = rest.split_once('.')?;
        let expected = B64.encode(hmac(&self.0.key, payload.as_bytes()));
        if !ct_eq(sig.as_bytes(), expected.as_bytes()) {
            return None;
        }
        let dynamic: Dynamic = serde_json::from_slice(&B64.decode(payload).ok()?).ok()?;
        Some(ClientInfo {
            name: dynamic.name,
            redirect_uris: dynamic.redirect_uris,
            secret: if dynamic.confidential {
                Secret::Hash(auth::hash(&self.dynamic_secret(id)))
            } else {
                Secret::None
            },
        })
    }

    fn dynamic_secret(&self, client_id: &str) -> String {
        B64.encode(hmac(&self.0.key, format!("secret:{client_id}").as_bytes()))
    }
}

pub fn router(oauth: OAuth) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
        .route(
            "/.well-known/openid-configuration",
            get(authorization_server),
        )
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize_page).post(authorize))
        .route("/oauth/token", post(token))
        .with_state(oauth)
}

async fn protected_resource(State(oauth): State<OAuth>, headers: HeaderMap) -> Response {
    let base = oauth.base(&headers);
    Json(json!({
        "resource": format!("{base}/mcp"),
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
        "resource_name": "Memory",
    }))
    .into_response()
}

async fn authorization_server(State(oauth): State<OAuth>, headers: HeaderMap) -> Response {
    let base = oauth.base(&headers);
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported":
            ["client_secret_basic", "client_secret_post", "none"],
        "scopes_supported": ["memory"],
        "subject_types_supported": ["public"],
    }))
    .into_response()
}

// --- Dynamic client registration (RFC 7591) ---

#[derive(Deserialize)]
struct RegisterRequest {
    #[serde(default)]
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    token_endpoint_auth_method: Option<String>,
}

async fn register(State(oauth): State<OAuth>, Json(req): Json<RegisterRequest>) -> Response {
    if req.redirect_uris.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "at least one redirect_uri is required",
        );
    }
    if let Some(bad) = req.redirect_uris.iter().find(|u| !acceptable_redirect(u)) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            &format!("redirect_uri must be https (or http on loopback): {bad}"),
        );
    }
    let method = req
        .token_endpoint_auth_method
        .unwrap_or_else(|| "client_secret_basic".into());
    let confidential = match method.as_str() {
        "none" => false,
        "client_secret_basic" | "client_secret_post" => true,
        _ => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_client_metadata",
                "token_endpoint_auth_method must be none, client_secret_basic or client_secret_post",
            );
        }
    };
    let name: String = req
        .client_name
        .unwrap_or_else(|| "an unnamed app".into())
        .chars()
        .take(100)
        .collect();
    let dynamic = Dynamic {
        name: name.clone(),
        redirect_uris: req.redirect_uris.clone(),
        confidential,
    };
    let payload = B64.encode(serde_json::to_vec(&dynamic).expect("serializing client"));
    let sig = B64.encode(hmac(&oauth.0.key, payload.as_bytes()));
    let client_id = format!("dyn.{payload}.{sig}");
    tracing::info!(client = %name, "registered an OAuth client");

    let mut body = json!({
        "client_id": client_id,
        "client_name": name,
        "redirect_uris": req.redirect_uris,
        "token_endpoint_auth_method": method,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "client_id_issued_at": chrono::Utc::now().timestamp(),
    });
    if confidential {
        body["client_secret"] = oauth.dynamic_secret(&client_id).into();
        body["client_secret_expires_at"] = 0.into();
    }
    (StatusCode::CREATED, Json(body)).into_response()
}

// --- Authorization ---

#[derive(Deserialize, Default)]
struct AuthorizeParams {
    #[serde(default)]
    response_type: String,
    #[serde(default)]
    client_id: String,
    redirect_uri: Option<String>,
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    scope: Option<String>,
    resource: Option<String>,
}

#[derive(Deserialize)]
struct AuthorizeForm {
    #[serde(flatten)]
    params: AuthorizeParams,
    #[serde(default)]
    token: String,
    #[serde(default)]
    decision: String,
}

/// The checks that must pass before anything is sent to the redirect URI.
/// Errors here are shown on the page, never redirected.
fn validate(oauth: &OAuth, p: &AuthorizeParams) -> Result<(ClientInfo, String), String> {
    let Some(client) = oauth.client(&p.client_id) else {
        return Err(format!("Unknown client ID {:?}.", p.client_id));
    };
    let redirect = match (&p.redirect_uri, client.redirect_uris.as_slice()) {
        (Some(uri), []) if acceptable_redirect(uri) => uri.clone(),
        (Some(uri), uris) if uris.contains(uri) => uri.clone(),
        (None, [only]) => only.clone(),
        (Some(uri), _) => {
            return Err(format!(
                "Redirect URI {uri:?} isn't registered for this client."
            ));
        }
        (None, _) => return Err("The request has no redirect_uri.".into()),
    };
    if p.response_type != "code" {
        return Err("Only response_type=code is supported.".into());
    }
    match (&p.code_challenge, p.code_challenge_method.as_deref()) {
        (Some(_), Some("S256") | None) => {}
        (Some(_), Some(m)) => {
            return Err(format!(
                "Unsupported code_challenge_method {m:?}; use S256."
            ));
        }
        (None, _) if matches!(client.secret, Secret::None) => {
            return Err("Public clients must use PKCE (code_challenge).".into());
        }
        (None, _) => {}
    }
    Ok((client, redirect))
}

async fn authorize_page(
    State(oauth): State<OAuth>,
    Query(params): Query<AuthorizeParams>,
) -> Response {
    match validate(&oauth, &params) {
        Ok((client, redirect)) => page(&params, &client, &redirect, None),
        Err(e) => error_page(&e),
    }
}

async fn authorize(State(oauth): State<OAuth>, Form(form): Form<AuthorizeForm>) -> Response {
    let p = &form.params;
    let (client, redirect) = match validate(&oauth, p) {
        Ok(v) => v,
        Err(e) => return error_page(&e),
    };
    let mut url = match url::Url::parse(&redirect) {
        Ok(u) => u,
        Err(_) => return error_page("The redirect URI isn't a valid URL."),
    };
    if form.decision != "approve" {
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("error", "access_denied");
            if let Some(state) = &p.state {
                q.append_pair("state", state);
            }
        }
        return Redirect::to(url.as_str()).into_response();
    }
    let token = form.token.trim();
    let Some(holder) = oauth.0.tokens.lookup(token) else {
        tracing::warn!(client = %client.name, "OAuth approval with an invalid token");
        return page(
            p,
            &client,
            &redirect,
            Some("That isn't a valid memory token."),
        );
    };
    let code = auth::generate();
    {
        let mut codes = oauth.0.codes.lock().unwrap();
        let now = Instant::now();
        codes.retain(|_, g| g.expires > now);
        codes.insert(
            code.clone(),
            Grant {
                client_id: p.client_id.clone(),
                redirect_uri: redirect.clone(),
                challenge: p.code_challenge.clone(),
                token: token.to_string(),
                expires: now + CODE_TTL,
            },
        );
    }
    tracing::info!(
        client = %client.name,
        source = %holder.source,
        "approved OAuth client"
    );
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("code", &code);
        if let Some(state) = &p.state {
            q.append_pair("state", state);
        }
    }
    Redirect::to(url.as_str()).into_response()
}

// --- Token endpoint ---

#[derive(Deserialize)]
struct TokenRequest {
    #[serde(default)]
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
}

async fn token(
    State(oauth): State<OAuth>,
    headers: HeaderMap,
    Form(req): Form<TokenRequest>,
) -> Response {
    // Client authentication: HTTP Basic, or client_id/client_secret in the body.
    let basic = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| {
            base64::engine::general_purpose::STANDARD
                .decode(b.trim())
                .ok()
        })
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| {
            let (id, secret) = s.split_once(':')?;
            Some((form_decode(id), form_decode(secret)))
        });
    let (client_id, secret) = match basic {
        Some((id, secret)) => (id, Some(secret)),
        None => (
            req.client_id.clone().unwrap_or_default(),
            req.client_secret.clone(),
        ),
    };
    let Some(client) = oauth.client(&client_id) else {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", "unknown client");
    };
    if let Secret::Hash(expected) = &client.secret {
        let given = secret.as_deref().map(auth::hash).unwrap_or_default();
        if !ct_eq(given.as_bytes(), expected.as_bytes()) {
            tracing::warn!(client = %client.name, "OAuth token request with a bad client secret");
            return oauth_error(
                StatusCode::UNAUTHORIZED,
                "invalid_client",
                "bad client secret",
            );
        }
    }

    let token = match req.grant_type.as_str() {
        "authorization_code" => {
            let Some(grant) = req
                .code
                .as_deref()
                .and_then(|c| oauth.0.codes.lock().unwrap().remove(c))
            else {
                return invalid_grant("unknown or already used code");
            };
            if grant.expires < Instant::now() {
                return invalid_grant("the code expired");
            }
            if grant.client_id != client_id {
                return invalid_grant("the code was issued to another client");
            }
            if req
                .redirect_uri
                .as_ref()
                .is_some_and(|r| *r != grant.redirect_uri)
            {
                return invalid_grant("redirect_uri doesn't match");
            }
            if let Some(challenge) = &grant.challenge {
                let Some(verifier) = &req.code_verifier else {
                    return invalid_grant("code_verifier is required");
                };
                let computed = B64.encode(Sha256::digest(verifier.as_bytes()));
                if !ct_eq(computed.as_bytes(), challenge.as_bytes()) {
                    return invalid_grant("code_verifier doesn't match");
                }
            }
            grant.token
        }
        // The refresh token is the access token itself, which never expires;
        // refreshing just confirms it hasn't been revoked.
        "refresh_token" => match req.refresh_token {
            Some(t) if oauth.0.tokens.lookup(&t).is_some() => t,
            _ => return invalid_grant("the token was revoked"),
        },
        other => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                &format!("unsupported grant_type {other:?}"),
            );
        }
    };
    tracing::info!(client = %client.name, grant = %req.grant_type, "issued OAuth token");
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "access_token": token,
            "token_type": "Bearer",
            "refresh_token": token,
            "scope": "memory",
        })),
    )
        .into_response()
}

fn invalid_grant(why: &str) -> Response {
    oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", why)
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        Json(json!({ "error": error, "error_description": description })),
    )
        .into_response()
}

// --- Pages ---

fn page(p: &AuthorizeParams, client: &ClientInfo, redirect: &str, error: Option<&str>) -> Response {
    let host = url::Url::parse(redirect)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| redirect.to_string());
    let hidden: String = [
        ("response_type", Some(p.response_type.as_str())),
        ("client_id", Some(p.client_id.as_str())),
        ("redirect_uri", Some(redirect)),
        ("state", p.state.as_deref()),
        ("code_challenge", p.code_challenge.as_deref()),
        ("code_challenge_method", p.code_challenge_method.as_deref()),
        ("scope", p.scope.as_deref()),
        ("resource", p.resource.as_deref()),
    ]
    .iter()
    .filter_map(|(k, v)| {
        v.map(|v| format!(r#"<input type="hidden" name="{k}" value="{}">"#, esc(v)))
    })
    .collect();
    let error = error
        .map(|e| format!(r#"<p class="error">{}</p>"#, esc(e)))
        .unwrap_or_default();
    let body = format!(
        r#"<h1>Connect to your memory</h1>
<p><strong>{name}</strong> wants to read and write your memory. Approving sends it back to <strong>{host}</strong>.</p>
<p>Paste the memory token minted for this app. It gets that token's access, and revoking the token disconnects it.</p>
{error}
<form method="post" action="/oauth/authorize">
{hidden}
<input type="password" name="token" placeholder="mem_…" autocomplete="off" autofocus>
<div class="buttons">
<button type="submit" name="decision" value="approve">Approve</button>
<button type="submit" name="decision" value="deny" class="secondary" formnovalidate>Deny</button>
</div>
</form>"#,
        name = esc(&client.name),
        host = esc(&host),
    );
    html(StatusCode::OK, &body)
}

fn error_page(message: &str) -> Response {
    html(
        StatusCode::BAD_REQUEST,
        &format!(
            "<h1>Can't connect</h1><p class=\"error\">{}</p>",
            esc(message)
        ),
    )
}

fn html(status: StatusCode, body: &str) -> Response {
    let page = format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="no-referrer">
<title>Memory</title>
<style>
:root {{ --bg: #f4efe6; --fg: #3b342b; --muted: #6f6556; --accent: #7a5c3a; --line: #d8cdbb; --err: #9a4a2f; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg: #26221d; --fg: #e6dccb; --muted: #ad9f8a; --accent: #c9a679; --line: #463e33; --err: #d98a6a; }} }}
body {{ background: var(--bg); color: var(--fg); font: 16px/1.5 system-ui, sans-serif; margin: 0; padding: 48px 16px; }}
main {{ max-width: 440px; margin: 0 auto; }}
h1 {{ font-size: 22px; font-weight: 600; margin: 0 0 16px; }}
p {{ color: var(--muted); }} strong {{ color: var(--fg); }}
.error {{ color: var(--err); }}
input[type=password] {{ box-sizing: border-box; width: 100%; padding: 10px 12px; font: inherit; color: var(--fg); background: transparent; border: 1px solid var(--line); border-radius: 8px; }}
.buttons {{ display: flex; gap: 8px; margin-top: 16px; }}
button {{ font: inherit; padding: 8px 16px; border-radius: 8px; border: 1px solid var(--accent); background: var(--accent); color: var(--bg); cursor: pointer; }}
button.secondary {{ background: transparent; color: var(--fg); border-color: var(--line); }}
</style></head>
<body><main>{body}</main></body></html>"#
    );
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::X_FRAME_OPTIONS, "DENY"),
        ],
        Html(page),
    )
        .into_response()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

// --- Helpers ---

fn acceptable_redirect(uri: &str) -> bool {
    let Ok(url) = url::Url::parse(uri) else {
        return false;
    };
    match url.scheme() {
        "https" => true,
        "http" => matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        _ => false,
    }
}

fn form_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={s}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

fn hmac(key: &[u8; 32], msg: &[u8]) -> [u8; 32] {
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for (i, k) in key.iter().enumerate() {
        ipad[i] ^= k;
        opad[i] ^= k;
    }
    let inner = Sha256::new()
        .chain_update(ipad)
        .chain_update(msg)
        .finalize();
    Sha256::new()
        .chain_update(opad)
        .chain_update(inner)
        .finalize()
        .into()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The key that signs self-registered client IDs; made on first start.
fn load_key(path: &Path) -> Result<[u8; 32]> {
    if let Ok(bytes) = std::fs::read(path) {
        return bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("{} must be exactly 32 bytes", path.display()));
    }
    let key: [u8; 32] = rand::random();
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(&key))
        .with_context(|| format!("creating {}", path.display()))?;
    tracing::info!("created OAuth signing key at {}", path.display());
    Ok(key)
}

fn parse_clients(path: &Path) -> Result<HashMap<String, Registered>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut map = HashMap::new();
    for (n, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(id), Some(hash)) = (fields.next(), fields.next()) else {
            bail!(
                "{}:{}: expected `<client-id> <sha256-of-secret> [redirect-uri...]`",
                path.display(),
                n + 1
            );
        };
        let redirect_uris: Vec<String> = fields.map(str::to_string).collect();
        if let Some(bad) = redirect_uris.iter().find(|u| !acceptable_redirect(u)) {
            bail!("{}:{}: bad redirect URI {bad}", path.display(), n + 1);
        }
        let client = Registered {
            secret_hash: hash.to_lowercase(),
            redirect_uris,
        };
        if map.insert(id.to_string(), client).is_some() {
            bail!("{}:{}: duplicate client {id}", path.display(), n + 1);
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    fn setup(clients: &str) -> (OAuth, PathBuf) {
        let dir = std::env::temp_dir().join(format!("memory-oauth-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tokens"),
            format!("{} gemini consolidate\n", auth::hash("mem_t")),
        )
        .unwrap();
        std::fs::write(dir.join("clients"), clients).unwrap();
        let tokens = Tokens::load(&dir.join("tokens")).unwrap();
        let oauth = OAuth::new(
            Some("https://memory.example.com".into()),
            tokens,
            &dir.join("clients"),
            &dir,
        )
        .unwrap();
        (oauth, dir)
    }

    async fn json_of(r: Response) -> serde_json::Value {
        serde_json::from_slice(&to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap()
    }

    fn location(r: &Response) -> url::Url {
        url::Url::parse(r.headers()[header::LOCATION].to_str().unwrap()).unwrap()
    }

    fn query(u: &url::Url, k: &str) -> Option<String> {
        u.query_pairs()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.into_owned())
    }

    fn params(client_id: &str, challenge: Option<&str>) -> AuthorizeParams {
        AuthorizeParams {
            response_type: "code".into(),
            client_id: client_id.into(),
            redirect_uri: Some("https://app.example/cb".into()),
            state: Some("st".into()),
            code_challenge: challenge.map(str::to_string),
            code_challenge_method: challenge.map(|_| "S256".into()),
            ..Default::default()
        }
    }

    fn token_req(
        client_id: &str,
        secret: Option<&str>,
        code: &str,
        verifier: Option<&str>,
    ) -> TokenRequest {
        TokenRequest {
            grant_type: "authorization_code".into(),
            code: Some(code.into()),
            redirect_uri: Some("https://app.example/cb".into()),
            code_verifier: verifier.map(str::to_string),
            refresh_token: None,
            client_id: Some(client_id.into()),
            client_secret: secret.map(str::to_string),
        }
    }

    async fn approve(oauth: &OAuth, p: AuthorizeParams, token: &str) -> Response {
        authorize(
            State(oauth.clone()),
            Form(AuthorizeForm {
                params: p,
                token: token.into(),
                decision: "approve".into(),
            }),
        )
        .await
    }

    #[tokio::test]
    async fn preregistered_client_gets_the_pasted_token() {
        let (oauth, _dir) = setup(&format!("gemini {}\n", auth::hash("s3cret")));

        // A wrong token re-renders the page instead of redirecting.
        let r = approve(&oauth, params("gemini", None), "mem_wrong").await;
        assert_eq!(r.status(), StatusCode::OK);

        let r = approve(&oauth, params("gemini", None), "mem_t").await;
        let loc = location(&r);
        assert_eq!(query(&loc, "state").as_deref(), Some("st"));
        let code = query(&loc, "code").unwrap();

        // Wrong secret, then right secret; the code is single-use.
        let r = token(
            State(oauth.clone()),
            HeaderMap::new(),
            Form(token_req("gemini", Some("nope"), &code, None)),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let r = token(
            State(oauth.clone()),
            HeaderMap::new(),
            Form(token_req("gemini", Some("s3cret"), &code, None)),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let body = json_of(r).await;
        assert_eq!(body["access_token"], "mem_t");
        let r = token(
            State(oauth.clone()),
            HeaderMap::new(),
            Form(token_req("gemini", Some("s3cret"), &code, None)),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);

        // HTTP Basic client auth works for refreshing.
        let mut headers = HeaderMap::new();
        let basic = base64::engine::general_purpose::STANDARD.encode("gemini:s3cret");
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {basic}").parse().unwrap(),
        );
        let r = token(
            State(oauth.clone()),
            headers,
            Form(TokenRequest {
                grant_type: "refresh_token".into(),
                refresh_token: Some("mem_t".into()),
                ..token_req("", None, "", None)
            }),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn dynamic_public_client_needs_pkce() {
        let (oauth, _dir) = setup("");
        let r = register(
            State(oauth.clone()),
            Json(RegisterRequest {
                redirect_uris: vec!["https://app.example/cb".into()],
                client_name: Some("Test".into()),
                token_endpoint_auth_method: Some("none".into()),
            }),
        )
        .await;
        assert_eq!(r.status(), StatusCode::CREATED);
        let id = json_of(r).await["client_id"].as_str().unwrap().to_string();

        // Tampering with the signed client ID is rejected.
        assert!(oauth.client(&format!("{id}x")).is_none());
        // No PKCE: refused on the page, nothing redirected.
        let r = approve(&oauth, params(&id, None), "mem_t").await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        // Unregistered redirect URI: refused.
        let mut p = params(&id, Some("x"));
        p.redirect_uri = Some("https://evil.example/cb".into());
        assert!(validate(&oauth, &p).is_err());

        let verifier = "a-verifier-that-is-long-enough-to-be-valid-0123456789";
        let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
        let r = approve(&oauth, params(&id, Some(&challenge)), "mem_t").await;
        let code = query(&location(&r), "code").unwrap();
        let r = token(
            State(oauth.clone()),
            HeaderMap::new(),
            Form(token_req(&id, None, &code, Some("wrong"))),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);

        let r = approve(&oauth, params(&id, Some(&challenge)), "mem_t").await;
        let code = query(&location(&r), "code").unwrap();
        let r = token(
            State(oauth.clone()),
            HeaderMap::new(),
            Form(token_req(&id, None, &code, Some(verifier))),
        )
        .await;
        assert_eq!(json_of(r).await["access_token"], "mem_t");
    }

    #[tokio::test]
    async fn dynamic_confidential_client_gets_a_working_secret() {
        let (oauth, _dir) = setup("");
        let r = register(
            State(oauth.clone()),
            Json(RegisterRequest {
                redirect_uris: vec!["https://app.example/cb".into()],
                client_name: None,
                token_endpoint_auth_method: None,
            }),
        )
        .await;
        let body = json_of(r).await;
        let id = body["client_id"].as_str().unwrap();
        let secret = body["client_secret"].as_str().unwrap();
        let r = approve(&oauth, params(id, None), "mem_t").await;
        let code = query(&location(&r), "code").unwrap();
        let r = token(
            State(oauth.clone()),
            HeaderMap::new(),
            Form(token_req(id, Some(secret), &code, None)),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
    }
}
