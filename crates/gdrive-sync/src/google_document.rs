//! Browser links for native Google documents, not exported document content.

use crate::drive_client::DriveFile;
use crate::error::SyncError;

pub(crate) fn kind(mime_type: &str) -> Option<(&'static str, &'static str)> {
    match mime_type {
        "application/vnd.google-apps.document" => Some(("gdoc", "document")),
        "application/vnd.google-apps.spreadsheet" => Some(("gsheet", "spreadsheets")),
        "application/vnd.google-apps.presentation" => Some(("gslides", "presentation")),
        _ => None,
    }
}

pub(crate) fn local_name(file: &DriveFile) -> String {
    match kind(&file.mime_type) {
        Some((extension, _)) => format!("{}.{extension}.desktop", file.name),
        None => file.name.clone(),
    }
}

pub(crate) fn content(file: &DriveFile) -> Result<Option<Vec<u8>>, SyncError> {
    let Some((_, application)) = kind(&file.mime_type) else {
        return Ok(None);
    };
    if file.id.is_empty()
        || !file
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(SyncError::Api(
            "invalid Google document ID for a browser link".into(),
        ));
    }
    let name = file
        .name
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    Ok(Some(format!(
        "[Desktop Entry]\nType=Link\nName={name}\nURL=https://docs.google.com/{application}/d/{}/edit\nX-GDrive-Id={}\n",
        file.id, file.id,
    ).into_bytes()))
}

pub(crate) fn linked_id(content: &[u8]) -> Option<&str> {
    std::str::from_utf8(content)
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("X-GDrive-Id="))
}

pub(crate) fn remote_name<'a>(local_name: &'a str, mime_type: &str) -> Result<&'a str, SyncError> {
    let (extension, _) =
        kind(mime_type).ok_or_else(|| SyncError::Api("not a native Google document".into()))?;
    local_name
        .strip_suffix(&format!(".{extension}.desktop"))
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            SyncError::Api("keep the Google document browser-link extension when renaming".into())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_types_have_browser_links_and_cannot_inject_desktop_entries() {
        for (mime, extension, application) in [
            ("document", "gdoc", "document"),
            ("spreadsheet", "gsheet", "spreadsheets"),
            ("presentation", "gslides", "presentation"),
        ] {
            let file = DriveFile {
                id: "doc-id".into(),
                name: "Notes\nExec=untrusted".into(),
                mime_type: format!("application/vnd.google-apps.{mime}"),
                parents: vec![],
                modified_time: None,
                trashed: false,
                md5_checksum: None,
                size: None,
            };
            let bytes = content(&file).unwrap().unwrap();
            let link = std::str::from_utf8(&bytes).unwrap();
            assert!(link.contains(&format!(
                "https://docs.google.com/{application}/d/doc-id/edit"
            )));
            assert!(!link.lines().any(|line| line.starts_with("Exec=")));
            assert_eq!(
                local_name(&file),
                format!("Notes\nExec=untrusted.{extension}.desktop")
            );
            assert_eq!(linked_id(&bytes), Some("doc-id"));
        }
    }
}
