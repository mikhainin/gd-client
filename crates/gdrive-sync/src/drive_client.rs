//! Minimal Google Drive REST v3 client built directly on `reqwest`.
//!
//! We talk to the REST API directly (rather than depending on a generated
//! `google-drive3`-style SDK) to keep the dependency tree small and
//! predictable. Only the subset of endpoints needed for folder sync is
//! implemented.

use std::{sync::RwLock, time::Duration};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::error::SyncError;

const API_BASE: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD_BASE: &str = "https://www.googleapis.com/upload/drive/v3";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
// Transfers share this client, so the total request/body budget must allow
// substantially more time than the interactive folder picker.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const FOLDER_LIST_TIMEOUT: Duration = Duration::from_secs(25);

pub struct DriveClient {
    http: reqwest::Client,
    // A `RwLock` (rather than a plain `String`) so a freshly signed-in
    // access token can be swapped in after construction (see
    // `SyncEngine::set_access_token`), without needing to rebuild the
    // client or the `Arc`s already handed out to running sync tasks.
    access_token: RwLock<String>,
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
            access_token: RwLock::new(access_token.into()),
            api_base: API_BASE.to_owned(),
            folder_list_timeout: FOLDER_LIST_TIMEOUT,
        }
    }

    /// Replaces the access token used for subsequent requests, e.g. after a
    /// fresh sign-in via the D-Bus `SignIn` call.
    pub fn set_access_token(&self, access_token: impl Into<String>) {
        *self.access_token.write().unwrap() = access_token.into();
    }

    fn auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let token = self.access_token.read().unwrap().clone();
        builder.bearer_auth(token)
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
        if self.access_token.read().unwrap().is_empty() {
            return Err(SyncError::NotAuthenticated);
        }
        let mut files = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut request = self.http.get(format!("{}/files", self.api_base)).query(&[
                ("q", query),
                ("fields", fields),
                ("pageSize", "1000"),
            ]);
            if let Some(token) = &page_token {
                request = request.query(&[("pageToken", token.as_str())]);
            }

            let response = self.auth(request).send().await?;
            let response = check_status(response).await?;
            let parsed: FileListResponse<T> = response.json().await?;
            files.extend(parsed.files);

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
        let response = self.auth(request).send().await?;
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
            let response = self.auth(request).send().await?;
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
        let response = self.auth(request).send().await?;
        let response = check_status(response).await?;
        Ok(response.json().await?)
    }

    /// Downloads a file's raw content.
    pub async fn download_file(&self, file_id: &str) -> Result<bytes::Bytes, SyncError> {
        let request = self
            .http
            .get(format!("{API_BASE}/files/{file_id}"))
            .query(&[("alt", "media")]);
        let response = self.auth(request).send().await?;
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
                let response = self.auth(request).send().await?;
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
                let response = self.auth(request).send().await?;
                let response = check_status(response).await?;
                Ok(response.json().await?)
            }
        }
    }

    /// Permanently deletes a file/folder from Drive (bypassing the trash).
    pub async fn delete_file(&self, file_id: &str) -> Result<(), SyncError> {
        let request = self.http.delete(format!("{API_BASE}/files/{file_id}"));
        let response = self.auth(request).send().await?;
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
    }

    impl Reply {
        fn json(body: &'static str) -> Self {
            Self {
                status: 200,
                body,
                header_delay: Duration::ZERO,
                body_delay: Duration::ZERO,
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
                access_token: RwLock::new("test-token".into()),
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
