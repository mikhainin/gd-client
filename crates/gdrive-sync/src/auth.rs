//! Google OAuth2 authentication (desktop / installed-app flow with PKCE and
//! a local loopback redirect listener).
//!
//! Google Cloud OAuth client credentials are required for real
//! authentication. Per project decision (see AGENTS.md Change Log), this is
//! stubbed out for now: [`OAuthConfig::client_id`] / `client_secret` must be
//! supplied via configuration or the `GDRIVE_CLIENT_ID` / `GDRIVE_CLIENT_SECRET`
//! environment variables before [`authenticate`] can complete.

use std::path::{Path, PathBuf};

use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge, RedirectUrl,
    Scope, TokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};

use crate::error::SyncError;

/// The Drive scope requested. `drive.file` limits access to files/folders
/// the user explicitly opens or creates with this app; switch to the
/// broader `drive` scope only if full-Drive sync is required.
pub const DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive";

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Default OAuth client credentials baked into the binary at *build* time
/// via the `GDRIVE_BUILD_CLIENT_ID` / `GDRIVE_BUILD_CLIENT_SECRET`
/// environment variables (read with `option_env!`, so nothing is ever
/// hardcoded/committed to source). This lets a maintainer/packager ship a
/// pre-configured build where end users just click "Sign in" with no setup
/// of their own, following the standard "installed application" OAuth
/// pattern used by e.g. `rclone` (the client secret isn't treated as
/// confidential for this flow - PKCE protects it).
///
/// Runtime `GDRIVE_CLIENT_ID` / `GDRIVE_CLIENT_SECRET` environment
/// variables still take priority over these, so anyone can override with
/// their own OAuth client (e.g. for separate API quota) without rebuilding.
const BUILT_IN_CLIENT_ID: Option<&str> = option_env!("GDRIVE_BUILD_CLIENT_ID");
const BUILT_IN_CLIENT_SECRET: Option<&str> = option_env!("GDRIVE_BUILD_CLIENT_SECRET");

/// OAuth client credentials for the "installed application" (desktop) flow.
///
/// These identify *this application* to Google, not the end user. Obtain
/// them by creating an OAuth client ID of type "Desktop app" in the Google
/// Cloud Console for a project with the Drive API enabled.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: String,
}

impl OAuthConfig {
    /// Loads credentials, preferring the `GDRIVE_CLIENT_ID` /
    /// `GDRIVE_CLIENT_SECRET` runtime environment variables (so users can
    /// always bring their own OAuth client), then falling back to whatever
    /// was baked in at build time via [`BUILT_IN_CLIENT_ID`] /
    /// [`BUILT_IN_CLIENT_SECRET`]. Returns `None` if neither source
    /// provides both values, in which case the caller should surface a
    /// "not configured yet" state in the UI rather than attempting to
    /// authenticate.
    pub fn from_env() -> Option<Self> {
        if let (Ok(client_id), Ok(client_secret)) = (
            std::env::var("GDRIVE_CLIENT_ID"),
            std::env::var("GDRIVE_CLIENT_SECRET"),
        ) {
            return Some(Self {
                client_id,
                client_secret,
            });
        }

        let client_id = BUILT_IN_CLIENT_ID.filter(|s| !s.is_empty())?;
        let client_secret = BUILT_IN_CLIENT_SECRET.filter(|s| !s.is_empty())?;
        Some(Self {
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
        })
    }
}

/// A persisted OAuth2 token set, cached at [`gdrive_common::paths::oauth_token_file`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix timestamp (seconds) after which `access_token` should be refreshed.
    pub expires_at: Option<i64>,
}

pub fn load_token(path: &Path) -> Result<Option<StoredToken>, SyncError> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    Ok(Some(serde_json::from_str(&text).map_err(|e| {
        SyncError::OAuth(format!("failed to parse cached token: {e}"))
    })?))
}

pub fn save_token(path: &Path, token: &StoredToken) -> Result<(), SyncError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(token)
        .map_err(|e| SyncError::OAuth(format!("failed to serialize token: {e}")))?;
    std::fs::write(path, text)?;
    Ok(())
}

/// Runs the interactive "installed app" OAuth2 flow: opens the consent URL
/// (the caller is responsible for showing/opening it, e.g. from the Qt UI),
/// listens on a local loopback port for the redirect, exchanges the
/// authorization code for tokens, and persists them to `token_path`.
///
/// This is a placeholder end-to-end implementation: it requires
/// [`OAuthConfig::from_env`] to return `Some`, and a real Google Cloud OAuth
/// client to be configured before it will succeed.
pub async fn authenticate(
    config: &OAuthConfig,
    token_path: &Path,
    show_auth_url: impl FnOnce(&str),
) -> Result<StoredToken, SyncError> {
    let redirect_port = 8734;
    let redirect_url = format!("http://127.0.0.1:{redirect_port}");

    let client = BasicClient::new(ClientId::new(config.client_id.clone()))
        .set_client_secret(ClientSecret::new(config.client_secret.clone()))
        .set_auth_uri(AuthUrl::new(GOOGLE_AUTH_URL.to_string()).expect("static URL is valid"))
        .set_token_uri(TokenUrl::new(GOOGLE_TOKEN_URL.to_string()).expect("static URL is valid"))
        .set_redirect_uri(RedirectUrl::new(redirect_url).expect("static URL is valid"));

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let (auth_url, _csrf_token) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new(DRIVE_SCOPE.to_string()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    show_auth_url(auth_url.as_str());

    let code = wait_for_redirect_code(redirect_port).await?;

    let http_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        // Bound the token-exchange request too, per AGENTS.md's "every
        // network call must have a timeout" rule - a stalled connection to
        // Google's token endpoint should fail, not hang the sign-in flow.
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let token_result = client
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(pkce_verifier)
        .request_async(&reqwest_http_client(http_client))
        .await
        .map_err(|e| SyncError::OAuth(e.to_string()))?;

    let stored = StoredToken {
        access_token: token_result.access_token().secret().clone(),
        refresh_token: token_result.refresh_token().map(|t| t.secret().clone()),
        expires_at: token_result
            .expires_in()
            .map(|d| now_unix_secs() + d.as_secs() as i64),
    };

    save_token(token_path, &stored)?;
    Ok(stored)
}

fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Blocks (asynchronously) until the OAuth redirect hits our loopback
/// listener, then returns the `code` query parameter from it.
async fn wait_for_redirect_code(port: u16) -> Result<String, SyncError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let (mut socket, _) = listener.accept().await?;

    let mut buf = [0u8; 4096];
    let n = socket.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let code = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|path| path.split_once('?'))
        .map(|(_, query)| query)
        .and_then(|query| {
            query
                .split('&')
                .find_map(|kv| kv.strip_prefix("code=").map(str::to_string))
        })
        .ok_or_else(|| SyncError::OAuth("redirect did not contain an authorization code".into()))?;

    let body = "Authentication complete. You can close this window and return to the app.";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = socket.write_all(response.as_bytes()).await;

    Ok(code)
}

/// Convenience wrapper resolving the default cached-token path from
/// `gdrive-common`.
pub fn default_token_path() -> Result<PathBuf, SyncError> {
    Ok(gdrive_common::paths::oauth_token_file()?)
}

/// Errors from [`reqwest_http_client`]'s adapter between `reqwest` and
/// `oauth2`'s HTTP client trait.
#[derive(Debug, thiserror::Error)]
pub enum ReqwestOAuthError {
    #[error("HTTP request failed: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("invalid HTTP request/response: {0}")]
    Http(#[from] oauth2::http::Error),
}

/// Adapts our workspace-pinned `reqwest::Client` to `oauth2`'s
/// [`oauth2::AsyncHttpClient`] trait.
///
/// `oauth2` optionally bundles its own vendored `reqwest` dependency, but we
/// disable that (`default-features = false` on the `oauth2` dependency in
/// the workspace manifest) because it pulls in a `reqwest` major version
/// that conflicts with the one used elsewhere in this crate. This adapter
/// lets `oauth2` drive requests through our own client instead, following
/// the same request/response translation as `oauth2`'s own (disabled)
/// `reqwest` integration.
fn reqwest_http_client(
    http_client: reqwest::Client,
) -> impl Fn(
    oauth2::HttpRequest,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<oauth2::HttpResponse, ReqwestOAuthError>> + Send>,
> {
    move |request| {
        let http_client = http_client.clone();
        Box::pin(async move {
            let request = reqwest::Request::try_from(request)?;
            let response = http_client.execute(request).await?;

            let mut builder = oauth2::http::Response::builder().status(response.status());
            for (name, value) in response.headers().iter() {
                builder = builder.header(name, value);
            }
            let body = response.bytes().await?.to_vec();
            Ok(builder.body(body)?)
        })
    }
}
