//! Minimal Google Drive REST v3 client built directly on `reqwest`.
//!
//! We talk to the REST API directly (rather than depending on a generated
//! `google-drive3`-style SDK) to keep the dependency tree small and
//! predictable. Only the subset of endpoints needed for folder sync is
//! implemented.

use std::{
    path::{Path, PathBuf},
    sync::RwLock,
    time::Duration,
};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::auth::{self, OAuthConfig, StoredToken};
use crate::error::SyncError;

const API_BASE: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD_BASE: &str = "https://www.googleapis.com/upload/drive/v3";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
// Transfers share this client, so the total request/body budget must allow
// substantially more time than the interactive folder picker.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const FOLDER_LIST_TIMEOUT: Duration = Duration::from_secs(25);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
struct TokenState {
    token: StoredToken,
    path: Option<PathBuf>,
    generation: u64,
}

impl TokenState {
    fn new(access_token: String) -> Self {
        Self {
            token: StoredToken {
                access_token,
                refresh_token: None,
                expires_at: None,
            },
            path: None,
            generation: 0,
        }
    }

    fn replace(&mut self, token: StoredToken, path: Option<PathBuf>) {
        self.generation = self.generation.wrapping_add(1);
        self.token = token;
        self.path = path;
    }
}

pub struct DriveClient {
    http: reqwest::Client,
    oauth_http: reqwest::Client,
    tokens: RwLock<TokenState>,
    refresh: tokio::sync::Mutex<()>,
    oauth_config: Option<OAuthConfig>,
    token_endpoint: String,
    api_base: String,
    folder_list_timeout: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(rename = "modifiedTime", default)]
    pub modified_time: Option<String>,
    #[serde(default)]
    pub trashed: bool,
    #[serde(rename = "md5Checksum", default)]
    pub md5_checksum: Option<String>,
    #[serde(default)]
    pub size: Option<String>,
}

pub const FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";

#[derive(Debug, Deserialize)]
struct FileListResponse<T> {
    files: Vec<T>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FolderEntry {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct StartPageTokenResponse {
    #[serde(rename = "startPageToken")]
    start_page_token: String,
}

#[derive(Debug, Deserialize)]
struct ChangeListResponse {
    changes: Vec<Change>,
    #[serde(rename = "newStartPageToken")]
    new_start_page_token: Option<String>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Change {
    #[serde(rename = "fileId")]
    pub file_id: String,
    pub removed: bool,
    pub file: Option<DriveFile>,
}

impl DriveClient {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            http: http_client(CONNECT_TIMEOUT, REQUEST_TIMEOUT),
            oauth_http: auth::refresh_http_client().expect("failed to construct OAuth HTTP client"),
            tokens: RwLock::new(TokenState::new(access_token.into())),
            refresh: tokio::sync::Mutex::new(()),
            oauth_config: OAuthConfig::from_env(),
            token_endpoint: auth::GOOGLE_TOKEN_URL.into(),
            api_base: API_BASE.to_owned(),
            folder_list_timeout: FOLDER_LIST_TIMEOUT,
        }
    }

    /// Replaces a bare access token and discards refresh metadata. An empty
    /// token invalidates refreshes in flight; OAuth sessions use save/restore.
    pub fn set_access_token(&self, access_token: impl Into<String>) {
        let token = TokenState::new(access_token.into()).token;
        self.tokens.write().unwrap().replace(token, None);
    }

    /// Restores refresh metadata from the existing cache without rewriting it.
    pub fn restore_token(&self, token: StoredToken, path: PathBuf) {
        self.tokens.write().unwrap().replace(token, Some(path));
    }

    /// Persists and replaces a session atomically with respect to token refresh.
    pub fn save_token(&self, token: StoredToken, path: &Path) -> Result<(), SyncError> {
        let mut state = self.tokens.write().unwrap();
        auth::save_token(path, &token)?;
        state.replace(token, Some(path.to_owned()));
        Ok(())
    }

    async fn access_token(&self, rejected: Option<&TokenState>) -> Result<TokenState, SyncError> {
        tokio::time::timeout(REFRESH_TIMEOUT, self.refresh_if_needed(rejected))
            .await
            .map_err(|_| SyncError::Timeout("refreshing Google Drive credentials"))?
    }

    async fn refresh_if_needed(
        &self,
        rejected: Option<&TokenState>,
    ) -> Result<TokenState, SyncError> {
        let needs_refresh = |state: &TokenState| -> Result<bool, SyncError> {
            if state.token.access_token.is_empty()
                || rejected.is_some_and(|old| old.generation != state.generation)
            {
                return Err(SyncError::NotAuthenticated);
            }
            Ok(state.token.needs_refresh()
                || rejected.is_some_and(|old| old.token.access_token == state.token.access_token))
        };
        let snapshot = self.tokens.read().unwrap().clone();
        if !needs_refresh(&snapshot)? {
            return Ok(snapshot);
        }
        let _refresh = self.refresh.lock().await;
        let snapshot = self.tokens.read().unwrap().clone();
        if !needs_refresh(&snapshot)? {
            return Ok(snapshot);
        }
        if snapshot
            .token
            .refresh_token
            .as_ref()
            .is_none_or(|token| token.is_empty())
        {
            return Err(SyncError::ReauthenticationRequired);
        }
        let config = self.oauth_config.as_ref().ok_or_else(|| SyncError::OAuth(
            "OAuth client credentials are not configured; configure the daemon's OAuth client and retry".into()
        ))?;
        let path = snapshot.path.as_ref().ok_or_else(|| {
            SyncError::OAuth(
                "OAuth token cache path is unavailable; sign out and sign in again".into(),
            )
        })?;
        tracing::debug!("refreshing Google Drive access token");
        let token = auth::refresh_access_token(
            config,
            &snapshot.token,
            &self.token_endpoint,
            self.oauth_http.clone(),
        )
        .await?;
        let mut state = self.tokens.write().unwrap();
        // Sign-out or a newer sign-in must win over a refresh already in flight.
        if state.generation != snapshot.generation {
            return Err(SyncError::NotAuthenticated);
        }
        auth::save_token(path, &token)?;
        state.token = token;
        tracing::debug!("refreshed and persisted Google Drive access token");
        Ok(state.clone())
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response, SyncError> {
        let token = self.access_token(None).await?;
        let retry = request.try_clone();
        let response = request
            .bearer_auth(&token.token.access_token)
            .send()
            .await?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        drop(response);
        let retry = retry.ok_or_else(|| {
            SyncError::OAuth("Drive request cannot be replayed after authentication failed".into())
        })?;
        let refreshed = self.access_token(Some(&token)).await?;
        Ok(retry
            .bearer_auth(&refreshed.token.access_token)
            .send()
            .await?)
    }

    /// Lists the direct (non-trashed) children of a Drive folder.
    /// Pass `folder_id = "root"` for the top level of "My Drive".
    pub async fn list_children(&self, folder_id: &str) -> Result<Vec<DriveFile>, SyncError> {
        let query = format!(
            "'{}' in parents and trashed = false",
            query_literal(folder_id)
        );
        self.list_files(
            &query,
            "nextPageToken, files(id, name, mimeType, parents, modifiedTime, trashed, md5Checksum, size)",
        )
        .await
    }

    async fn list_files<T: DeserializeOwned>(
        &self,
        query: &str,
        fields: &str,
    ) -> Result<Vec<T>, SyncError> {
        let mut files = Vec::new();
        let mut page_token: Option<String> = None;
        let started = std::time::Instant::now();
        let mut page = 0;

        loop {
            page += 1;
            tracing::debug!(page, "requesting Drive listing page");
            let mut request = self.http.get(format!("{}/files", self.api_base)).query(&[
                ("q", query),
                ("fields", fields),
                ("pageSize", "1000"),
            ]);
            if let Some(token) = &page_token {
                request = request.query(&[("pageToken", token.as_str())]);
            }

            let response = self.send(request).await.inspect_err(|_| {
                tracing::warn!(
                    page,
                    elapsed_ms = started.elapsed().as_millis(),
                    "Drive listing failed"
                );
            })?;
            tracing::debug!(
                page,
                status = response.status().as_u16(),
                elapsed_ms = started.elapsed().as_millis(),
                "received Drive listing response"
            );
            let response = check_status(response).await?;
            let parsed: FileListResponse<T> = response.json().await?;
            files.extend(parsed.files);
            tracing::debug!(
                page,
                count = files.len(),
                has_next_page = parsed.next_page_token.is_some(),
                "decoded Drive listing page"
            );

            page_token = parsed.next_page_token;
            if page_token.is_none() {
                break;
            }
        }

        Ok(files)
    }

    /// Lists the direct (non-trashed) sub-folders of a Drive folder, for
    /// the "browse Drive folder" picker in the UI. Pass `folder_id = "root"`
    /// for the top level of "My Drive". Only id/name are fetched; the MIME
    /// type is known from the query and other metadata stays at its default.
    /// The deadline covers every page, including response bodies.
    pub async fn list_child_folders(&self, folder_id: &str) -> Result<Vec<DriveFile>, SyncError> {
        let query = format!(
            "'{}' in parents and trashed = false and mimeType = '{FOLDER_MIME_TYPE}'",
            query_literal(folder_id)
        );
        let folders: Vec<FolderEntry> = tokio::time::timeout(
            self.folder_list_timeout,
            self.list_files(&query, "nextPageToken, files(id, name)"),
        )
        .await
        .map_err(|_| SyncError::Timeout("listing Drive folders"))??;
        Ok(folders
            .into_iter()
            .map(|f| DriveFile {
                id: f.id,
                name: f.name,
                mime_type: FOLDER_MIME_TYPE.to_owned(),
                parents: Vec::new(),
                modified_time: None,
                trashed: false,
                md5_checksum: None,
                size: None,
            })
            .collect())
    }

    /// Fetches a starting page token for the Changes API, to be persisted
    /// and used with [`Self::list_changes`] on subsequent polls.
    pub async fn get_start_page_token(&self) -> Result<String, SyncError> {
        let request = self.http.get(format!("{API_BASE}/changes/startPageToken"));
        let response = self.send(request).await?;
        let response = check_status(response).await?;
        let parsed: StartPageTokenResponse = response.json().await?;
        Ok(parsed.start_page_token)
    }

    /// Lists remote changes since `page_token`, returning the changes and
    /// the new page token to persist for the next poll.
    pub async fn list_changes(&self, page_token: &str) -> Result<(Vec<Change>, String), SyncError> {
        let mut changes = Vec::new();
        let mut token = page_token.to_string();
        let mut new_start_token = None;

        loop {
            let request = self.http.get(format!("{API_BASE}/changes")).query(&[
                ("pageToken", token.as_str()),
                ("fields", "nextPageToken, newStartPageToken, changes(fileId, removed, file(id, name, mimeType, parents, modifiedTime, trashed, md5Checksum, size))"),
            ]);
            let response = self.send(request).await?;
            let response = check_status(response).await?;
            let parsed: ChangeListResponse = response.json().await?;
            changes.extend(parsed.changes);

            if let Some(next) = parsed.next_page_token {
                token = next;
            } else {
                new_start_token = parsed.new_start_page_token;
                break;
            }
        }

        let new_start_token = new_start_token.unwrap_or(token);
        Ok((changes, new_start_token))
    }

    /// Creates a folder on Drive under `parent_id` (or `"root"`).
    pub async fn create_folder(&self, name: &str, parent_id: &str) -> Result<DriveFile, SyncError> {
        let body = serde_json::json!({
            "name": name,
            "mimeType": FOLDER_MIME_TYPE,
            "parents": [parent_id],
        });
        let request = self
            .http
            .post(format!("{API_BASE}/files"))
            .query(&[(
                "fields",
                "id, name, mimeType, parents, modifiedTime, trashed",
            )])
            .json(&body);
        let response = self.send(request).await?;
        let response = check_status(response).await?;
        Ok(response.json().await?)
    }

    /// Downloads a file's raw content.
    pub async fn download_file(&self, file_id: &str) -> Result<bytes::Bytes, SyncError> {
        let request = self
            .http
            .get(format!("{API_BASE}/files/{file_id}"))
            .query(&[("alt", "media")]);
        let response = self.send(request).await?;
        let response = check_status(response).await?;
        Ok(response.bytes().await?)
    }

    /// Uploads (or updates, when `file_id` is `Some`) file content using a
    /// simple (non-resumable) upload. Suitable for small-to-medium files;
    /// large files should use the resumable upload protocol instead (not
    /// yet implemented here).
    pub async fn upload_file(
        &self,
        file_id: Option<&str>,
        name: &str,
        parent_id: &str,
        content: Vec<u8>,
    ) -> Result<DriveFile, SyncError> {
        match file_id {
            Some(id) => {
                let request = self
                    .http
                    .patch(format!("{UPLOAD_BASE}/files/{id}"))
                    .query(&[("uploadType", "media")])
                    .body(content);
                let response = self.send(request).await?;
                let response = check_status(response).await?;
                Ok(response.json().await?)
            }
            None => {
                let metadata = serde_json::json!({
                    "name": name,
                    "parents": [parent_id],
                });
                let boundary = "gdrive_client_boundary";
                let mut body = Vec::new();
                body.extend_from_slice(
                    format!(
                        "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n"
                    )
                    .as_bytes(),
                );
                body.extend_from_slice(metadata.to_string().as_bytes());
                body.extend_from_slice(
                    format!("\r\n--{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n")
                        .as_bytes(),
                );
                body.extend_from_slice(&content);
                body.extend_from_slice(format!("\r\n--{boundary}--").as_bytes());

                let request = self
                    .http
                    .post(format!("{UPLOAD_BASE}/files"))
                    .query(&[("uploadType", "multipart")])
                    .header(
                        "Content-Type",
                        format!("multipart/related; boundary={boundary}"),
                    )
                    .body(body);
                let response = self.send(request).await?;
                let response = check_status(response).await?;
                Ok(response.json().await?)
            }
        }
    }

    /// Permanently deletes a file/folder from Drive (bypassing the trash).
    pub async fn delete_file(&self, file_id: &str) -> Result<(), SyncError> {
        let request = self.http.delete(format!("{API_BASE}/files/{file_id}"));
        let response = self.send(request).await?;
        check_status(response).await?;
        Ok(())
    }
}

fn http_client(connect_timeout: Duration, request_timeout: Duration) -> reqwest::Client {
    let builder = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(request_timeout);
    #[cfg(test)]
    let builder = builder.no_proxy();
    builder
        .build()
        .expect("failed to construct Drive HTTP client")
}

fn query_literal(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

async fn check_status(response: reqwest::Response) -> Result<reqwest::Response, SyncError> {
    if response.status().is_success() {
        Ok(response)
    } else if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        Err(SyncError::ReauthenticationRequired)
    } else {
        let status = response.status();
        let text = response.text().await?;
        Err(SyncError::Api(format!("HTTP {status}: {text}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
        time::Instant,
    };

    struct Reply {
        status: u16,
        body: &'static str,
        header_delay: Duration,
        body_delay: Duration,
        completed: Option<mpsc::Sender<()>>,
    }

    impl Reply {
        fn json(body: &'static str) -> Self {
            Self {
                status: 200,
                body,
                header_delay: Duration::ZERO,
                body_delay: Duration::ZERO,
                completed: None,
            }
        }
    }

    struct Server {
        base: String,
        requests: mpsc::Receiver<String>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl Server {
        fn start(replies: Vec<Reply>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let (sender, requests) = mpsc::channel();
            let worker = thread::spawn(move || {
                for reply in replies {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    let mut stream = loop {
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                if Instant::now() >= deadline {
                                    return;
                                }
                                thread::sleep(Duration::from_millis(1));
                            }
                            Err(e) => panic!("accept failed: {e}"),
                        }
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = Vec::new();
                    let mut byte = [0];
                    while !request.ends_with(b"\r\n\r\n") {
                        if stream.read_exact(&mut byte).is_err() {
                            return;
                        }
                        request.push(byte[0]);
                    }
                    let length = String::from_utf8_lossy(&request)
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                        .unwrap_or(0);
                    let mut body = vec![0; length];
                    if stream.read_exact(&mut body).is_err() {
                        return;
                    }
                    request.extend(body);
                    sender.send(String::from_utf8(request).unwrap()).unwrap();
                    thread::sleep(reply.header_delay);
                    let headers = format!(
                        "HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        reply.status,
                        reply.body.len()
                    );
                    if stream.write_all(headers.as_bytes()).is_err() {
                        continue;
                    }
                    thread::sleep(reply.body_delay);
                    let _ = stream.write_all(reply.body.as_bytes());
                    if let Some(completed) = reply.completed {
                        completed.send(()).unwrap();
                    }
                }
            });
            Self {
                base,
                requests,
                worker: Some(worker),
            }
        }

        fn client(&self, request_timeout: Duration, folder_timeout: Duration) -> DriveClient {
            DriveClient {
                http: http_client(Duration::from_secs(1), request_timeout),
                oauth_http: auth::refresh_http_client().unwrap(),
                tokens: RwLock::new(TokenState::new("test-token".into())),
                refresh: tokio::sync::Mutex::new(()),
                oauth_config: Some(OAuthConfig {
                    client_id: "fixture-client".into(),
                    client_secret: "fixture-client-secret".into(),
                }),
                token_endpoint: format!("{}/token", self.base),
                api_base: self.base.clone(),
                folder_list_timeout: folder_timeout,
            }
        }

        fn normal_client(&self) -> DriveClient {
            self.client(Duration::from_secs(2), Duration::from_secs(2))
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.worker.take().unwrap().join().unwrap();
        }
    }

    fn params(request: &str) -> HashMap<String, String> {
        let target = request.split_whitespace().nth(1).unwrap();
        let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
        assert_eq!(url.path(), "/files");
        url.query_pairs().into_owned().collect()
    }

    const REFRESH_REPLY: &str =
        r#"{"access_token":"fresh-token","token_type":"Bearer","expires_in":3600}"#;

    struct Cache(PathBuf);

    impl Cache {
        fn new(client: &DriveClient, expires_at: Option<i64>) -> Self {
            let path = std::env::temp_dir()
                .join(format!("gdrive-token-test-{}.json", uuid::Uuid::new_v4()));
            client
                .save_token(
                    StoredToken {
                        access_token: "stale-token".into(),
                        refresh_token: Some("fixture-refresh-token".into()),
                        expires_at,
                    },
                    &path,
                )
                .unwrap();
            Self(path)
        }
    }

    impl Drop for Cache {
        fn drop(&mut self) {
            match std::fs::remove_file(&self.0) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("failed to remove fixture token: {error}"),
            }
        }
    }

    #[tokio::test]
    async fn expired_token_refresh_is_shared_by_browsing_and_sync_and_preserves_refresh_token() {
        let server = Server::start(vec![
            Reply::json(REFRESH_REPLY),
            Reply::json(r#"{"files":[]}"#),
            Reply::json(r#"{"files":[]}"#),
        ]);
        let client = server.normal_client();
        let cache = Cache::new(&client, Some(0));
        let (browse, sync) = tokio::join!(
            client.list_child_folders("root"),
            client.list_children("root")
        );
        assert!(browse.unwrap().is_empty());
        assert!(sync.unwrap().is_empty());
        let requests: Vec<_> = server.requests.try_iter().collect();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].starts_with("POST /token "));
        assert!(requests[0].contains("grant_type=refresh_token"));
        assert!(requests[0].contains("refresh_token=fixture-refresh-token"));
        for request in &requests[1..] {
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer fresh-token"));
        }
        let stored = auth::load_token(&cache.0).unwrap().unwrap();
        assert_eq!(stored.access_token, "fresh-token");
        assert_eq!(
            stored.refresh_token.as_deref(),
            Some("fixture-refresh-token")
        );
        assert!(!stored.needs_refresh());
    }

    #[tokio::test]
    async fn unauthorized_request_refreshes_unknown_expiry_and_retries_only_once() {
        let mut rejected = Reply::json(r#"{"error":{"message":"unauthorized"}}"#);
        rejected.status = 401;
        let mut rotation = Reply::json(
            r#"{"access_token":"fresh-token","token_type":"Bearer","expires_in":3600,"refresh_token":"rotated-refresh-token"}"#,
        );
        rotation.status = 200;
        let server = Server::start(vec![
            rejected,
            rotation,
            Reply {
                status: 401,
                ..Reply::json(r#"{"error":{"message":"still unauthorized"}}"#)
            },
        ]);
        let client = server.normal_client();
        let cache = Cache::new(&client, None);
        assert!(matches!(
            client.list_child_folders("root").await,
            Err(SyncError::ReauthenticationRequired)
        ));
        let requests: Vec<_> = server.requests.try_iter().collect();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].starts_with("GET /files?"));
        assert!(requests[1].starts_with("POST /token "));
        assert!(requests[2]
            .to_ascii_lowercase()
            .contains("authorization: bearer fresh-token"));
        assert_eq!(
            auth::load_token(&cache.0)
                .unwrap()
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("rotated-refresh-token")
        );
    }

    #[tokio::test]
    async fn missing_refresh_token_or_client_credentials_is_actionable_without_network_calls() {
        let server = Server::start(vec![]);
        let mut client = server.normal_client();
        let _cache = Cache::new(&client, Some(0));
        client.oauth_config = None;
        let error = client.list_child_folders("root").await.unwrap_err();
        assert!(error
            .to_string()
            .contains("OAuth client credentials are not configured"));
        client.tokens.write().unwrap().token.refresh_token = None;
        assert!(matches!(
            client.list_child_folders("root").await,
            Err(SyncError::ReauthenticationRequired)
        ));
        assert!(server.requests.try_iter().next().is_none());
    }

    #[tokio::test]
    async fn rejected_and_malformed_refreshes_do_not_overwrite_cache_or_expose_bodies() {
        for (status, body) in [
            (
                400,
                r#"{"error":"invalid_grant","error_description":"fixture-sensitive-body"}"#,
            ),
            (200, r#"{"unexpected":"fixture-sensitive-body"}"#),
        ] {
            let server = Server::start(vec![Reply {
                status,
                ..Reply::json(body)
            }]);
            let client = server.normal_client();
            let cache = Cache::new(&client, Some(0));
            let error = client.list_child_folders("root").await.unwrap_err();
            if status == 400 {
                assert!(matches!(error, SyncError::ReauthenticationRequired));
            }
            assert!(!error.to_string().contains("fixture-sensitive-body"));
            assert_eq!(
                auth::load_token(&cache.0).unwrap().unwrap().access_token,
                "stale-token"
            );
            assert_eq!(server.requests.try_iter().count(), 1);
        }
    }

    #[tokio::test]
    async fn browse_deadline_cancels_stalled_refresh_and_retry_can_recover() {
        let (completed, received) = mpsc::channel();
        let server = Server::start(vec![
            Reply {
                body_delay: Duration::from_millis(250),
                completed: Some(completed),
                ..Reply::json(REFRESH_REPLY)
            },
            Reply::json(REFRESH_REPLY),
            Reply::json(r#"{"files":[]}"#),
        ]);
        let mut client = server.client(Duration::from_secs(2), Duration::from_millis(100));
        let cache = Cache::new(&client, Some(0));
        let start = Instant::now();
        assert!(matches!(
            client.list_child_folders("root").await,
            Err(SyncError::Timeout(_))
        ));
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(
            auth::load_token(&cache.0).unwrap().unwrap().access_token,
            "stale-token"
        );
        received.recv_timeout(Duration::from_secs(2)).unwrap();
        client.folder_list_timeout = Duration::from_secs(2);
        assert!(client.list_child_folders("root").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn sign_out_and_new_sign_in_win_over_in_flight_refresh() {
        for sign_out in [true, false] {
            let server = Server::start(vec![Reply {
                body_delay: Duration::from_millis(150),
                ..Reply::json(REFRESH_REPLY)
            }]);
            let client = server.normal_client();
            let cache = Cache::new(&client, Some(0));
            let change_session = async {
                let deadline = Instant::now() + Duration::from_secs(1);
                while server.requests.try_recv().is_err() {
                    assert!(Instant::now() < deadline, "refresh never started");
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                if sign_out {
                    client.set_access_token("");
                    std::fs::remove_file(&cache.0).unwrap();
                } else {
                    client
                        .save_token(
                            StoredToken {
                                access_token: "new-session".into(),
                                refresh_token: Some("new-refresh-token".into()),
                                expires_at: Some(i64::MAX),
                            },
                            &cache.0,
                        )
                        .unwrap();
                }
            };
            let (result, ()) = tokio::join!(client.access_token(None), change_session);
            assert!(matches!(result, Err(SyncError::NotAuthenticated)));
            if sign_out {
                assert!(!cache.0.exists());
            } else {
                assert_eq!(
                    auth::load_token(&cache.0).unwrap().unwrap().access_token,
                    "new-session"
                );
            }
        }
    }

    #[tokio::test]
    async fn refreshed_token_is_not_installed_if_cache_persistence_fails() {
        let server = Server::start(vec![Reply::json(REFRESH_REPLY)]);
        let client = server.normal_client();
        client.restore_token(
            StoredToken {
                access_token: "stale-token".into(),
                refresh_token: Some("fixture-refresh-token".into()),
                expires_at: Some(0),
            },
            std::env::temp_dir(),
        );
        assert!(matches!(
            client.access_token(None).await,
            Err(SyncError::Io(_))
        ));
        assert_eq!(
            client.tokens.read().unwrap().token.access_token,
            "stale-token"
        );
    }

    #[tokio::test]
    async fn folder_query_is_minimal_escaped_and_fully_paginated() {
        let server = Server::start(vec![
            Reply::json(r#"{"files":[{"id":"a","name":"First"}],"nextPageToken":"next + /"}"#),
            Reply::json(r#"{"files":[],"nextPageToken":"third"}"#),
            Reply::json(r#"{"files":[{"id":"b","name":"Second"}]}"#),
        ]);
        let folders = server
            .normal_client()
            .list_child_folders("a'\\b")
            .await
            .unwrap();
        assert_eq!(
            folders.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(folders[1].name, "Second");
        assert!(folders.iter().all(|f| f.mime_type == FOLDER_MIME_TYPE));
        let requests: Vec<_> = server.requests.try_iter().collect();
        assert_eq!(requests.len(), 3);
        for request in &requests {
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-token"));
            let p = params(request);
            assert_eq!(p["q"], "'a\\'\\\\b' in parents and trashed = false and mimeType = 'application/vnd.google-apps.folder'");
            assert_eq!(p["fields"], "nextPageToken, files(id, name)");
            assert_eq!(p["pageSize"], "1000");
        }
        assert!(!params(&requests[0]).contains_key("pageToken"));
        assert_eq!(params(&requests[1])["pageToken"], "next + /");
        assert_eq!(params(&requests[2])["pageToken"], "third");
    }

    #[tokio::test]
    async fn sync_listing_keeps_files_metadata_and_pagination() {
        let server = Server::start(vec![
            Reply::json(
                r#"{"files":[{"id":"file","name":"File","mimeType":"text/plain","parents":["root"],"md5Checksum":"hash","size":"123","modifiedTime":"now"}],"nextPageToken":"more"}"#,
            ),
            Reply::json(
                r#"{"files":[{"id":"dir","name":"Folder","mimeType":"application/vnd.google-apps.folder"}]}"#,
            ),
        ]);
        let files = server.normal_client().list_children("a'\\b").await.unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].mime_type, "text/plain");
        assert_eq!(files[0].parents, ["root"]);
        assert_eq!(files[0].md5_checksum.as_deref(), Some("hash"));
        assert_eq!(files[0].size.as_deref(), Some("123"));
        assert_eq!(files[0].modified_time.as_deref(), Some("now"));
        for request in server.requests.try_iter() {
            let p = params(&request);
            assert_eq!(p["q"], "'a\\'\\\\b' in parents and trashed = false");
            assert_eq!(p["fields"], "nextPageToken, files(id, name, mimeType, parents, modifiedTime, trashed, md5Checksum, size)");
        }
    }

    #[tokio::test]
    async fn successful_empty_listing_is_empty() {
        let server = Server::start(vec![Reply::json(r#"{"files":[]}"#)]);
        assert!(server
            .normal_client()
            .list_child_folders("root")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn missing_authentication_is_an_error_without_a_request() {
        let server = Server::start(vec![]);
        let client = server.normal_client();
        client.set_access_token("");
        assert!(matches!(
            client.list_child_folders("root").await,
            Err(SyncError::NotAuthenticated)
        ));
        assert!(server.requests.try_iter().next().is_none());
    }

    #[tokio::test]
    async fn auth_and_api_errors_preserve_status_and_message() {
        for status in [401, 403, 500] {
            let mut reply = Reply::json(r#"{"error":{"message":"test rejection"}}"#);
            reply.status = status;
            let server = Server::start(vec![reply]);
            let error = server
                .normal_client()
                .list_child_folders("root")
                .await
                .unwrap_err();
            if status == 401 {
                assert!(matches!(error, SyncError::ReauthenticationRequired));
                continue;
            }
            assert!(matches!(error, SyncError::Api(_)));
            let message = error.to_string();
            assert!(message.contains(&status.to_string()));
            assert!(message.contains("test rejection"));
        }
    }

    #[tokio::test]
    async fn malformed_response_is_not_an_empty_success() {
        let server = Server::start(vec![Reply::json(r#"{"unexpected":[]}"#)]);
        assert!(matches!(
            server.normal_client().list_child_folders("root").await,
            Err(SyncError::Http(_))
        ));
    }

    #[tokio::test]
    async fn later_page_error_never_returns_partial_success() {
        let mut rejected = Reply::json("unavailable");
        rejected.status = 503;
        let server = Server::start(vec![
            Reply::json(r#"{"files":[{"id":"a","name":"First"}],"nextPageToken":"next"}"#),
            rejected,
        ]);
        assert!(matches!(
            server.normal_client().list_child_folders("root").await,
            Err(SyncError::Api(_))
        ));
    }

    #[tokio::test]
    async fn http_timeout_covers_stalled_headers_and_success_or_error_bodies() {
        for (headers_stall, status) in [(true, 200), (false, 200), (false, 500)] {
            let mut reply = Reply::json(r#"{"files":[]}"#);
            reply.status = status;
            if headers_stall {
                reply.header_delay = Duration::from_millis(250);
            } else {
                reply.body_delay = Duration::from_millis(250);
            }
            let server = Server::start(vec![reply]);
            let client = server.client(Duration::from_millis(100), Duration::from_secs(2));
            let error = client.list_child_folders("root").await.unwrap_err();
            assert!(
                matches!(error, SyncError::Http(ref e) if e.is_timeout()),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn folder_deadline_covers_stalled_headers_and_bodies() {
        for headers_stall in [true, false] {
            let mut reply = Reply::json(r#"{"files":[]}"#);
            if headers_stall {
                reply.header_delay = Duration::from_millis(250);
            } else {
                reply.body_delay = Duration::from_millis(250);
            }
            let server = Server::start(vec![reply]);
            let client = server.client(Duration::from_secs(2), Duration::from_millis(100));
            assert!(matches!(
                client.list_child_folders("root").await,
                Err(SyncError::Timeout(_))
            ));
        }
    }

    #[tokio::test]
    async fn folder_deadline_is_shared_across_pages() {
        let mut first = Reply::json(r#"{"files":[],"nextPageToken":"next"}"#);
        let mut second = Reply::json(r#"{"files":[{"id":"a","name":"Too late"}]}"#);
        first.body_delay = Duration::from_millis(200);
        second.body_delay = Duration::from_millis(200);
        let server = Server::start(vec![first, second]);
        let client = server.client(Duration::from_secs(2), Duration::from_millis(350));
        assert!(matches!(
            client.list_child_folders("root").await,
            Err(SyncError::Timeout(_))
        ));
        assert_eq!(server.requests.try_iter().count(), 2);
    }
}
