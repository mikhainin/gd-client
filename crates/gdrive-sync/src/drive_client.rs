//! Minimal Google Drive REST v3 client built directly on `reqwest`.
//!
//! We talk to the REST API directly (rather than depending on a generated
//! `google-drive3`-style SDK) to keep the dependency tree small and
//! predictable. Only the subset of endpoints needed for folder sync is
//! implemented.

use serde::{Deserialize, Serialize};

use crate::error::SyncError;

const API_BASE: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD_BASE: &str = "https://www.googleapis.com/upload/drive/v3";

pub struct DriveClient {
    http: reqwest::Client,
    access_token: String,
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
struct FileListResponse {
    files: Vec<DriveFile>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
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
            http: reqwest::Client::new(),
            access_token: access_token.into(),
        }
    }

    fn auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder.bearer_auth(&self.access_token)
    }

    /// Lists the direct (non-trashed) children of a Drive folder.
    /// Pass `folder_id = "root"` for the top level of "My Drive".
    pub async fn list_children(&self, folder_id: &str) -> Result<Vec<DriveFile>, SyncError> {
        let mut files = Vec::new();
        let mut page_token: Option<String> = None;
        let query = format!("'{folder_id}' in parents and trashed = false");

        loop {
            let mut request = self.http.get(format!("{API_BASE}/files")).query(&[
                ("q", query.as_str()),
                (
                    "fields",
                    "nextPageToken, files(id, name, mimeType, parents, modifiedTime, trashed, md5Checksum, size)",
                ),
                ("pageSize", "1000"),
            ]);
            if let Some(token) = &page_token {
                request = request.query(&[("pageToken", token.as_str())]);
            }

            let response = self.auth(request).send().await?;
            let response = check_status(response).await?;
            let parsed: FileListResponse = response.json().await?;
            files.extend(parsed.files);

            page_token = parsed.next_page_token;
            if page_token.is_none() {
                break;
            }
        }

        Ok(files)
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
            .query(&[("fields", "id, name, mimeType, parents, modifiedTime, trashed")])
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
                body.extend_from_slice(format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n").as_bytes());
                body.extend_from_slice(metadata.to_string().as_bytes());
                body.extend_from_slice(format!("\r\n--{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes());
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

async fn check_status(response: reqwest::Response) -> Result<reqwest::Response, SyncError> {
    if response.status().is_success() {
        Ok(response)
    } else {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        Err(SyncError::Api(format!("HTTP {status}: {text}")))
    }
}
