//! Google OAuth2 authentication (desktop / installed-app flow with PKCE and
//! a local loopback redirect listener).
//!
//! Google Cloud OAuth client credentials are required for real
//! authentication and token refresh. [`OAuthConfig::from_env`] loads runtime
//! overrides or build-time application credentials. The Drive client retains
//! refresh metadata and renews access tokens without browser interaction.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
pub(crate) const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

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

impl StoredToken {
    pub(crate) fn needs_refresh(&self) -> bool {
        self.expires_at
            .is_some_and(|expires_at| expires_at <= now_unix_secs() + 30)
    }
}

pub(crate) fn refresh_http_client() -> reqwest::Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15));
    #[cfg(test)]
    let builder = builder.no_proxy();
    builder.build()
}

pub(crate) async fn refresh_access_token(
    config: &OAuthConfig,
    previous: &StoredToken,
    token_url: &str,
    http: reqwest::Client,
) -> Result<StoredToken, SyncError> {
    use oauth2::{basic::BasicErrorResponseType, RefreshToken, RequestTokenError};

    let refresh_token = previous
        .refresh_token
        .as_ref()
        .filter(|token| !token.is_empty())
        .ok_or(SyncError::ReauthenticationRequired)?;
    let client = BasicClient::new(ClientId::new(config.client_id.clone()))
        .set_client_secret(ClientSecret::new(config.client_secret.clone()))
        .set_token_uri(
            TokenUrl::new(token_url.to_owned())
                .map_err(|_| SyncError::OAuth("invalid OAuth token endpoint".into()))?,
        );
    let adapter = reqwest_http_client(http);
    let refresh = RefreshToken::new(refresh_token.clone());
    let request = client.exchange_refresh_token(&refresh);
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        request.request_async(&adapter),
    )
    .await
    .map_err(|source| SyncError::Timeout {
        operation: "refreshing the OAuth access token",
        source: Box::new(source),
    })?
    .map_err(|error| match error {
        RequestTokenError::ServerResponse(response)
            if response.error() == &BasicErrorResponseType::InvalidGrant =>
        {
            SyncError::ReauthenticationRequired
        }
        RequestTokenError::ServerResponse(_) => SyncError::OAuth(
            "token refresh rejected; check the daemon's OAuth client configuration".into(),
        ),
        RequestTokenError::Request(ReqwestOAuthError::Reqwest(source)) if source.is_timeout() => {
            SyncError::Timeout {
                operation: "refreshing the OAuth access token",
                source: Box::new(source.without_url()),
            }
        }
        RequestTokenError::Request(ReqwestOAuthError::Reqwest(source)) => {
            SyncError::OAuthTransport(ReqwestOAuthError::Reqwest(source.without_url()))
        }
        RequestTokenError::Request(ReqwestOAuthError::Body(source)) if source.is_timeout() => {
            SyncError::Timeout {
                operation: "reading the OAuth access token response",
                source: Box::new(source.without_url()),
            }
        }
        RequestTokenError::Request(ReqwestOAuthError::Body(source)) => {
            SyncError::OAuthTransport(ReqwestOAuthError::Body(source.without_url()))
        }
        RequestTokenError::Request(source) => SyncError::OAuthTransport(source),
        _ => SyncError::OAuth("invalid response from the OAuth token endpoint".into()),
    })?;
    if response.access_token().secret().is_empty()
        || response
            .refresh_token()
            .is_some_and(|token| token.secret().is_empty())
    {
        return Err(SyncError::OAuth(
            "OAuth token endpoint returned an empty token".into(),
        ));
    }
    Ok(StoredToken {
        access_token: response.access_token().secret().clone(),
        refresh_token: response
            .refresh_token()
            .map(|token| token.secret().clone())
            .or_else(|| previous.refresh_token.clone()),
        expires_at: response
            .expires_in()
            .map(|duration| now_unix_secs() + duration.as_secs() as i64),
    })
}

pub fn load_token(path: &Path) -> Result<Option<StoredToken>, SyncError> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    Ok(Some(
        serde_json::from_str(&text).map_err(SyncError::OAuthCache)?,
    ))
}

pub fn save_token(path: &Path, token: &StoredToken) -> Result<(), SyncError> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let text = serde_json::to_string_pretty(token).map_err(SyncError::OAuthCache)?;
    let temporary = parent.join(format!(".oauth-token-{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        if let Err(error) = std::fs::remove_file(&temporary) {
            tracing::warn!(%error, "failed to remove temporary OAuth token cache");
        }
    }
    result
}

/// Runs the interactive "installed app" OAuth2 flow: opens the consent URL
/// (the caller is responsible for showing/opening it, e.g. from the Qt UI),
/// listens on a local loopback port for the redirect, exchanges the
/// authorization code for tokens, and persists them to `token_path`.
///
/// This requires
/// [`OAuthConfig::from_env`] to return `Some`, and a real Google Cloud OAuth
/// client to be configured before it will succeed.
pub async fn authenticate(
    config: &OAuthConfig,
    token_path: &Path,
    show_auth_url: impl FnOnce(&str),
) -> Result<StoredToken, SyncError> {
    let stored = authorize(config, show_auth_url).await?;
    save_token(token_path, &stored)?;
    Ok(stored)
}

/// Obtains tokens without persisting them, allowing the caller to coordinate
/// persistence with cancellation and changes to its authentication state.
pub async fn authorize(
    config: &OAuthConfig,
    show_auth_url: impl FnOnce(&str),
) -> Result<StoredToken, SyncError> {
    let redirect_port = 8734;
    let redirect_url = format!("http://127.0.0.1:{redirect_port}");
    let listener = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpListener::bind(("127.0.0.1", redirect_port)),
    )
    .await
    .map_err(|source| SyncError::Timeout {
        operation: "opening the OAuth redirect listener",
        source: Box::new(source),
    })??;

    let client = BasicClient::new(ClientId::new(config.client_id.clone()))
        .set_client_secret(ClientSecret::new(config.client_secret.clone()))
        .set_auth_uri(AuthUrl::new(GOOGLE_AUTH_URL.to_string()).expect("static URL is valid"))
        .set_token_uri(TokenUrl::new(GOOGLE_TOKEN_URL.to_string()).expect("static URL is valid"))
        .set_redirect_uri(RedirectUrl::new(redirect_url).expect("static URL is valid"));

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let (auth_url, csrf_token) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new(DRIVE_SCOPE.to_string()))
        .add_extra_param("access_type", "offline")
        .add_extra_param("prompt", "consent")
        .set_pkce_challenge(pkce_challenge)
        .url();

    show_auth_url(auth_url.as_str());

    let code = wait_for_redirect_code(
        listener,
        csrf_token.secret(),
        Duration::from_secs(8 * 60),
        Duration::from_secs(5),
    )
    .await?;

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
        .map_err(token_exchange_error)?;

    let stored = StoredToken {
        access_token: token_result.access_token().secret().clone(),
        refresh_token: token_result.refresh_token().map(|t| t.secret().clone()),
        expires_at: token_result
            .expires_in()
            .map(|d| now_unix_secs() + d.as_secs() as i64),
    };

    if stored
        .refresh_token
        .as_ref()
        .is_none_or(|token| token.is_empty())
    {
        return Err(SyncError::OAuth(
            "Google did not grant offline access; sign in again and complete consent".into(),
        ));
    }
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
async fn wait_for_redirect_code(
    listener: tokio::net::TcpListener,
    state: &str,
    deadline: Duration,
    io_timeout: Duration,
) -> Result<String, SyncError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    tokio::time::timeout(deadline, async {
        loop {
            let (mut socket, _) = listener.accept().await?;
            let request = tokio::time::timeout(io_timeout, async {
                let mut request = Vec::new();
                let mut buf = [0u8; 512];
                while !request.ends_with(b"\r\n\r\n") {
                    let size = socket.read(&mut buf).await?;
                    if size == 0 || request.len() + size > 8192 {
                        return Err(SyncError::OAuth("invalid OAuth redirect request".into()));
                    }
                    request.extend_from_slice(&buf[..size]);
                }
                String::from_utf8(request)
                    .map_err(|source| SyncError::OAuthRedirect(Box::new(source)))
            }).await.map_err(|source| SyncError::Timeout {
                operation: "reading the OAuth redirect", source: Box::new(source),
            })??;
            let result = redirect_code(&request, state);
            let (status, body) = match &result {
                Ok(Some(_)) => ("200 OK", "Authentication received. Return to the app to finish sign-in."),
                Ok(None) => ("404 Not Found", "Not an OAuth callback."),
                Err(_) => ("400 Bad Request", "Authentication failed. Return to the app and retry."),
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
            tokio::time::timeout(io_timeout, async {
                socket.write_all(response.as_bytes()).await?;
                socket.shutdown().await
            }).await.map_err(|source| SyncError::Timeout {
                operation: "replying to the OAuth redirect", source: Box::new(source),
            })??;
            if let Some(code) = result? { return Ok(code); }
        }
    }).await.map_err(|source| SyncError::Timeout {
        operation: "waiting for OAuth browser consent", source: Box::new(source),
    })?
}

fn redirect_code(request: &str, state: &str) -> Result<Option<String>, SyncError> {
    let mut parts = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    if parts.next() != Some("GET") {
        return Err(SyncError::OAuth(
            "OAuth redirect must be an HTTP GET".into(),
        ));
    }
    let target = parts
        .next()
        .filter(|target| target.starts_with('/') && !target.starts_with("//"))
        .ok_or_else(|| SyncError::OAuth("invalid OAuth redirect target".into()))?;
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|source| SyncError::OAuthRedirect(Box::new(source)))?;
    if url.path() != "/" {
        return Ok(None);
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    let states: Vec<_> = pairs.iter().filter(|(key, _)| key == "state").collect();
    if states.len() != 1 || states[0].1 != state {
        return Err(SyncError::OAuth(
            "OAuth redirect state did not match; retry sign-in".into(),
        ));
    }
    if pairs.iter().any(|(key, _)| key == "error") {
        return Err(SyncError::OAuth(
            "Google authorization was declined or failed".into(),
        ));
    }
    let codes: Vec<_> = pairs.iter().filter(|(key, _)| key == "code").collect();
    if codes.len() != 1 || codes[0].1.is_empty() {
        return Err(SyncError::OAuth(
            "OAuth redirect did not contain one authorization code".into(),
        ));
    }
    Ok(Some(codes[0].1.to_string()))
}

fn token_exchange_error(
    error: oauth2::RequestTokenError<ReqwestOAuthError, oauth2::basic::BasicErrorResponse>,
) -> SyncError {
    use oauth2::RequestTokenError;
    match error {
        RequestTokenError::Request(ReqwestOAuthError::Reqwest(source))
        | RequestTokenError::Request(ReqwestOAuthError::Body(source))
            if source.is_timeout() =>
        {
            SyncError::Timeout {
                operation: "exchanging the OAuth authorization code",
                source: Box::new(source.without_url()),
            }
        }
        RequestTokenError::Request(ReqwestOAuthError::Reqwest(source)) => {
            SyncError::OAuthTransport(ReqwestOAuthError::Reqwest(source.without_url()))
        }
        RequestTokenError::Request(ReqwestOAuthError::Body(source)) => {
            SyncError::OAuthTransport(ReqwestOAuthError::Body(source.without_url()))
        }
        RequestTokenError::Request(source) => SyncError::OAuthTransport(source),
        source => SyncError::OAuthExchange(Box::new(source)),
    }
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
    #[error("HTTP response body transfer failed")]
    Body(#[source] reqwest::Error),
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
            let body = response
                .bytes()
                .await
                .map_err(ReqwestOAuthError::Body)?
                .to_vec();
            Ok(builder.body(body)?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn redirect_requires_matching_state_and_decodes_exactly_one_code() {
        assert_eq!(
            redirect_code("GET /?code=a%2Bb%2Fc&state=nonce HTTP/1.1\r\n\r\n", "nonce")
                .unwrap()
                .as_deref(),
            Some("a+b/c")
        );
        for target in [
            "/?code=secret&state=wrong",
            "/?code=secret",
            "/?code=one&code=two&state=nonce",
            "/?code=secret&state=nonce&state=nonce",
            "/?error=access_denied&error_description=private&state=nonce",
        ] {
            let error =
                redirect_code(&format!("GET {target} HTTP/1.1\r\n\r\n"), "nonce").unwrap_err();
            assert!(!error.to_string().contains("secret"));
            assert!(!error.to_string().contains("private"));
        }
        assert!(redirect_code("GET /favicon.ico HTTP/1.1\r\n\r\n", "nonce")
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn redirect_accept_and_stalled_request_time_out_with_sources() {
        for connect in [false, true] {
            let listener = tokio::time::timeout(
                Duration::from_secs(1),
                tokio::net::TcpListener::bind("127.0.0.1:0"),
            )
            .await
            .unwrap()
            .unwrap();
            let _socket = if connect {
                Some(
                    tokio::time::timeout(
                        Duration::from_secs(1),
                        tokio::net::TcpStream::connect(listener.local_addr().unwrap()),
                    )
                    .await
                    .unwrap()
                    .unwrap(),
                )
            } else {
                None
            };
            let error = wait_for_redirect_code(
                listener,
                "nonce",
                Duration::from_millis(100),
                Duration::from_millis(30),
            )
            .await
            .unwrap_err();
            assert!(matches!(error, SyncError::Timeout { .. }));
            assert!(std::error::Error::source(&error).is_some());
        }
    }

    #[tokio::test]
    async fn fragmented_redirect_is_read_completely_and_responded_to() {
        let listener = tokio::time::timeout(
            Duration::from_secs(1),
            tokio::net::TcpListener::bind("127.0.0.1:0"),
        )
        .await
        .unwrap()
        .unwrap();
        let address = listener.local_addr().unwrap();
        let browser = async {
            tokio::time::timeout(Duration::from_secs(1), async {
                let mut socket = tokio::net::TcpStream::connect(address).await?;
                socket.write_all(b"GET /?code=a%2B").await?;
                tokio::task::yield_now().await;
                socket
                    .write_all(b"b&state=nonce HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    .await?;
                let mut response = Vec::new();
                socket.read_to_end(&mut response).await?;
                Ok::<_, std::io::Error>(response)
            })
            .await
            .unwrap()
            .unwrap()
        };
        let (code, response) = tokio::join!(
            wait_for_redirect_code(
                listener,
                "nonce",
                Duration::from_secs(2),
                Duration::from_secs(1)
            ),
            browser,
        );
        assert_eq!(code.unwrap(), "a+b");
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));
    }

    #[tokio::test]
    async fn exchange_failures_keep_timeout_classification_and_sanitized_sources() {
        use crate::drive_client::tests::{Reply, Server};
        for timeout in [false, true] {
            let mut reply = Reply::json("private-auth-response");
            if timeout {
                reply.header_delay = Duration::from_millis(150);
            }
            let server = Server::start(vec![reply]);
            let client = BasicClient::new(ClientId::new("fixture".into()))
                .set_token_uri(TokenUrl::new(format!("{}/token", server.base)).unwrap());
            let http = reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(Duration::from_millis(50))
                .timeout(Duration::from_millis(75))
                .build()
                .unwrap();
            let error = client
                .exchange_code(AuthorizationCode::new("fixture-code".into()))
                .request_async(&reqwest_http_client(http))
                .await
                .map_err(token_exchange_error)
                .unwrap_err();
            assert_eq!(matches!(error, SyncError::Timeout { .. }), timeout);
            assert!(std::error::Error::source(&error).is_some());
            assert!(!error.to_string().contains("private-auth-response"));
            assert!(!error.to_string().contains("fixture-code"));
        }
    }
}
