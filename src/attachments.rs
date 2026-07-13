use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Error, Result};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Attachment {
    pub uuid: Option<Uuid>,
    pub title: String,
    pub path: String,
    pub media_type: String,
}

impl Attachment {
    pub fn managed(
        uuid: Uuid,
        title: impl Into<String>,
        path: impl Into<String>,
        media_type: impl Into<String>,
    ) -> Self {
        Self {
            uuid: Some(uuid),
            title: title.into(),
            path: path.into(),
            media_type: media_type.into(),
        }
    }
}

pub fn parse_file_field(input: &str) -> Result<Vec<Attachment>> {
    split_escaped(input, ';')
        .into_iter()
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let components = split_escaped_raw(entry, ':');
            if components.len() != 3 {
                return Err(Error::InvalidFileField {
                    message: format!("expected title:path:MIME, got {entry:?}"),
                });
            }
            let title = decode_component(components[0])?;
            let path = decode_component(components[1])?;
            let media_type = decode_component(components[2])?;
            if path.is_empty() {
                return Err(Error::InvalidFileField {
                    message: "attachment path is empty".to_owned(),
                });
            }
            Ok(Attachment {
                uuid: attachment_uuid_from_path(&path),
                title,
                path,
                media_type,
            })
        })
        .collect()
}

pub fn encode_file_field(attachments: &[Attachment]) -> String {
    attachments
        .iter()
        .map(|attachment| {
            format!(
                "{}:{}:{}",
                encode_component(&attachment.title),
                encode_component(&attachment.path),
                encode_component(&attachment.media_type)
            )
        })
        .collect::<Vec<_>>()
        .join(";")
}

pub fn encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | ':' | ';' | '{' | '}' | '$') {
            encoded.push('\\');
        }
        encoded.push(character);
    }
    encoded
}

pub fn sanitize_filename(path: &Path) -> String {
    let original = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("attachment");
    let mut sanitized = String::new();
    for character in original.chars() {
        if character.is_control() || "<>:\"/\\|?*".contains(character) {
            sanitized.push('_');
        } else {
            sanitized.push(character);
        }
        if sanitized.len() >= 180 {
            break;
        }
    }
    let sanitized = sanitized.trim_matches([' ', '.']);
    if sanitized.is_empty() || matches!(sanitized, "." | "..") {
        "attachment".to_owned()
    } else {
        sanitized.to_owned()
    }
}

pub fn safe_relative_path(path: &Path) -> bool {
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

pub fn safe_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::Normal(_)
            )
        })
}

pub fn path_to_bibtex(path: &Path) -> Result<String> {
    if !safe_relative_path(path) {
        return Err(Error::UnsafeAttachmentPath {
            path: path.to_owned(),
        });
    }
    Ok(path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/"))
}

pub fn attachment_uuid_from_path(path: &str) -> Option<Uuid> {
    let filename = Path::new(path).file_name()?.to_str()?;
    let candidate = filename.get(..36)?;
    (filename.as_bytes().get(36) == Some(&b'-'))
        .then(|| Uuid::parse_str(candidate).ok())
        .flatten()
}

fn split_escaped(input: &str, separator: char) -> Vec<&str> {
    split_escaped_raw(input, separator)
}

fn split_escaped_raw(input: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut escaped = false;
    for (index, character) in input.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == separator {
            parts.push(&input[start..index]);
            start = index + character.len_utf8();
        }
    }
    parts.push(&input[start..]);
    parts
}

fn decode_component(input: &str) -> Result<String> {
    let mut decoded = String::with_capacity(input.len());
    let mut characters = input.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            let escaped = characters.next().ok_or_else(|| Error::InvalidFileField {
                message: "component ends with an escape character".to_owned(),
            })?;
            decoded.push(escaped);
        } else {
            decoded.push(character);
        }
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_field_round_trips_zotero_escapes() {
        let uuid = Uuid::parse_str("cc9e50c4-55ee-4471-b17c-c41684f64bf9").unwrap();
        let attachments = vec![Attachment::managed(
            uuid,
            "Title: $5; {draft}",
            format!("references.files/item/{uuid}-a\\b.pdf"),
            "application/pdf",
        )];
        let encoded = encode_file_field(&attachments);

        assert!(encoded.contains("Title\\: \\$5\\; \\{draft\\}"));
        assert_eq!(parse_file_field(&encoded).unwrap(), attachments);
    }

    #[test]
    fn uuid_is_recovered_from_managed_filename() {
        let uuid = Uuid::parse_str("cc9e50c4-55ee-4471-b17c-c41684f64bf9").unwrap();
        assert_eq!(
            attachment_uuid_from_path(&format!("root/{uuid}-paper.pdf")),
            Some(uuid)
        );
        assert_eq!(attachment_uuid_from_path("root/external.pdf"), None);
    }

    #[test]
    fn filename_and_relative_path_reject_unsafe_components() {
        assert_eq!(
            sanitize_filename(Path::new("../../bad:name?.pdf")),
            "bad_name_.pdf"
        );
        assert!(safe_relative_path(Path::new("root/item/file.pdf")));
        assert!(!safe_relative_path(Path::new("../file.pdf")));
        assert!(!safe_relative_path(Path::new("/tmp/file.pdf")));
        assert!(safe_absolute_path(Path::new("/tmp/root/file.pdf")));
        assert!(!safe_absolute_path(Path::new("/tmp/root/../file.pdf")));
    }
}
