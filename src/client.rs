use std::collections::BTreeMap;
use std::io::Read;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value as JsonValue;
use ureq::http::Response;
use ureq::unversioned::multipart::{Form, Part};

use crate::catalog::{CatalogItem, CheckReport, ItemSummary};
use crate::config::Config;
use crate::library::{
    AddedItem, AttachedFile, DetachedFile, FormatResult, ItemPatch, MutationResult, TrashEntry,
};
use crate::{Error, Result};

const AUTHORIZATION: &str = "Authorization";
const ETAG: &str = "ETag";
const IF_MATCH: &str = "If-Match";

#[derive(Debug, Deserialize, Serialize)]
pub struct ApiHealth {
    pub status: String,
    pub revision: String,
    pub entries: usize,
    pub warnings: usize,
    pub errors: usize,
    pub disk_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ItemResponse {
    #[serde(flatten)]
    item: CatalogItem,
    revision: String,
}

#[derive(Debug, Deserialize)]
struct ListResponse {
    items: Vec<ItemSummary>,
    revision: String,
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    error: ApiErrorDetail,
}

#[derive(Debug, Deserialize)]
struct ApiErrorDetail {
    code: String,
    message: String,
}

#[derive(Debug, Serialize)]
struct CreateItemRequest<'a> {
    #[serde(rename = "type")]
    entry_type: &'a str,
    citation_key: Option<&'a str>,
    fields: BTreeMap<&'a str, &'a str>,
}

#[derive(Debug, Serialize)]
struct ImportRequest<'a> {
    source: &'a str,
}

pub struct ApiClient {
    agent: ureq::Agent,
    base: String,
    authorization: String,
    revision: String,
}

impl ApiClient {
    /// Connect to a configured daemon. A refused loopback connection means there is no daemon and
    /// permits the caller to use direct file access; every other response/error is surfaced.
    pub fn connect(config: &Config) -> Result<Option<Self>> {
        let address = config.api_address.parse::<SocketAddr>().map_err(|error| {
            Error::InvalidSocketAddress {
                address: config.api_address.clone(),
                message: error.to_string(),
            }
        })?;
        if !address.ip().is_loopback() {
            return Err(Error::NonLoopbackAddress {
                address: address.to_string(),
            });
        }

        let agent: ureq::Agent = ureq::Agent::config_builder()
            .proxy(None)
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_millis(250)))
            .timeout_recv_response(Some(Duration::from_secs(2)))
            .build()
            .into();
        let mut client = Self {
            agent,
            base: format!("http://{address}"),
            authorization: format!("Bearer {}", config.api_token),
            revision: String::new(),
        };
        let response = match client
            .agent
            .get(client.url("/api/v1/health"))
            .header(AUTHORIZATION, &client.authorization)
            .call()
        {
            Ok(response) => response,
            Err(error) if daemon_absent(&error) => return Ok(None),
            Err(error) => return Err(client.transport_error(error)),
        };
        let health: ApiHealth = client.decode(response)?;
        client.revision = health.revision;
        Ok(Some(client))
    }

    pub fn health(&mut self) -> Result<ApiHealth> {
        let response = self.call(
            self.agent
                .get(self.url("/api/v1/health"))
                .header(AUTHORIZATION, &self.authorization)
                .call(),
        )?;
        let health: ApiHealth = self.decode(response)?;
        self.revision.clone_from(&health.revision);
        Ok(health)
    }

    pub fn list(
        &mut self,
        query: Option<&str>,
        entry_type: Option<&str>,
        tag: Option<&str>,
    ) -> Result<Vec<ItemSummary>> {
        let mut request = self
            .agent
            .get(self.url("/api/v1/items"))
            .header(AUTHORIZATION, &self.authorization);
        if let Some(query) = query {
            request = request.query("q", query);
        }
        if let Some(entry_type) = entry_type {
            request = request.query("type", entry_type);
        }
        if let Some(tag) = tag {
            request = request.query("tag", tag);
        }
        let response = self.call(request.call())?;
        let listed: ListResponse = self.decode(response)?;
        self.revision = listed.revision;
        Ok(listed.items)
    }

    pub fn get_item(&mut self, id: &str) -> Result<CatalogItem> {
        let path = format!("/api/v1/items/{}", encode_path_segment(id));
        let response = self.call(
            self.agent
                .get(self.url(&path))
                .header(AUTHORIZATION, &self.authorization)
                .call(),
        )?;
        let item: ItemResponse = self.decode(response)?;
        self.revision = item.revision;
        Ok(item.item)
    }

    pub fn create_item(
        &mut self,
        entry_type: &str,
        citation_key: Option<&str>,
        fields: &[(String, String)],
    ) -> Result<AddedItem> {
        let fields = fields
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let request = CreateItemRequest {
            entry_type,
            citation_key,
            fields,
        };
        let response = self.call(
            self.agent
                .post(self.url("/api/v1/items"))
                .header(AUTHORIZATION, &self.authorization)
                .header(IF_MATCH, self.if_match())
                .send_json(&request),
        )?;
        let item = self.decode_item_mutation(response)?;
        Ok(AddedItem {
            uuid: self.required_uuid(&item)?,
            citation_key: item.citation_key,
        })
    }

    pub fn import_biblatex(&mut self, source: &str) -> Result<Vec<AddedItem>> {
        let response = self.call(
            self.agent
                .post(self.url("/api/v1/import"))
                .header(AUTHORIZATION, &self.authorization)
                .header(IF_MATCH, self.if_match())
                .send_json(&ImportRequest { source }),
        )?;
        let (added, revision) = self.decode_with_revision(response)?;
        self.adopt_revision(revision);
        Ok(added)
    }

    pub fn patch_item(&mut self, id: &str, patch: &ItemPatch) -> Result<MutationResult> {
        let path = format!("/api/v1/items/{}", encode_path_segment(id));
        let response = self.call(
            self.agent
                .patch(self.url(&path))
                .header(AUTHORIZATION, &self.authorization)
                .header(IF_MATCH, self.if_match())
                .send_json(patch),
        )?;
        let item = self.decode_item_mutation(response)?;
        Ok(MutationResult {
            uuid: self.required_uuid(&item)?,
            citation_key: item.citation_key,
        })
    }

    pub fn delete_item(&mut self, id: &str) -> Result<()> {
        let path = format!("/api/v1/items/{}", encode_path_segment(id));
        let response = self.call(
            self.agent
                .delete(self.url(&path))
                .header(AUTHORIZATION, &self.authorization)
                .header(IF_MATCH, self.if_match())
                .call(),
        )?;
        self.accept_empty_mutation(response)
    }

    pub fn attach_file(
        &mut self,
        id: &str,
        file: &Path,
        title: Option<&str>,
        media_type: Option<&str>,
    ) -> Result<AttachedFile> {
        if !file.is_file() {
            return Err(Error::AttachmentNotFile {
                path: file.to_owned(),
            });
        }
        let mut part = Part::file(file).map_err(|source| Error::Read {
            path: file.to_owned(),
            source,
        })?;
        if let Some(name) = file.file_name().and_then(|name| name.to_str()) {
            part = part.file_name(name);
        }
        if let Some(media_type) = media_type {
            part = part.mime_str(media_type).map_err(|error| Error::Daemon {
                address: self.base.clone(),
                message: format!("invalid attachment media type: {error}"),
            })?;
        }
        let mut form = Form::new().part("file", part);
        if let Some(title) = title {
            form = form.text("title", title);
        }
        if let Some(media_type) = media_type {
            form = form.text("media_type", media_type);
        }
        let path = format!("/api/v1/items/{}/attachments", encode_path_segment(id));
        let response = self.call(
            self.agent
                .post(self.url(&path))
                .header(AUTHORIZATION, &self.authorization)
                .header(IF_MATCH, self.if_match())
                .send(form),
        )?;
        let (attached, revision) = self.decode_with_revision(response)?;
        self.adopt_revision(revision);
        Ok(attached)
    }

    pub fn detach_attachment(
        &mut self,
        id: &str,
        attachment_id: uuid::Uuid,
    ) -> Result<DetachedFile> {
        let path = format!(
            "/api/v1/items/{}/attachments/{attachment_id}",
            encode_path_segment(id)
        );
        let response = self.call(
            self.agent
                .delete(self.url(&path))
                .header(AUTHORIZATION, &self.authorization)
                .header(IF_MATCH, self.if_match())
                .call(),
        )?;
        let (detached, revision) = self.decode_with_revision(response)?;
        self.adopt_revision(revision);
        Ok(detached)
    }

    pub fn trash_entries(&mut self) -> Result<Vec<TrashEntry>> {
        let response = self.call(
            self.agent
                .get(self.url("/api/v1/trash"))
                .header(AUTHORIZATION, &self.authorization)
                .call(),
        )?;
        self.decode(response)
    }

    pub fn purge_trash(&mut self) -> Result<usize> {
        let response = self.call(
            self.agent
                .delete(self.url("/api/v1/trash"))
                .header(AUTHORIZATION, &self.authorization)
                .header(IF_MATCH, self.if_match())
                .call(),
        )?;
        let (body, revision): (JsonValue, _) = self.decode_with_revision(response)?;
        self.adopt_revision(revision);
        body["purged"]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| Error::Daemon {
                address: self.base.clone(),
                message: "daemon returned an invalid trash result".to_owned(),
            })
    }

    pub fn export(&self, ids: &[String]) -> Result<String> {
        let mut request = self
            .agent
            .get(self.url("/api/v1/export"))
            .header(AUTHORIZATION, &self.authorization);
        let ids = ids.join(",");
        if !ids.is_empty() {
            request = request.query("ids", &ids);
        }
        let response = self.call(request.call())?;
        let mut response = self.ensure_success(response)?;
        let mut contents = String::new();
        response
            .body_mut()
            .as_reader()
            .read_to_string(&mut contents)
            .map_err(|error| Error::Daemon {
                address: self.base.clone(),
                message: error.to_string(),
            })?;
        Ok(contents)
    }

    pub fn format(&mut self) -> Result<FormatResult> {
        let response = self.call(
            self.agent
                .post(self.url("/api/v1/format"))
                .header(AUTHORIZATION, &self.authorization)
                .header(IF_MATCH, self.if_match())
                .send_empty(),
        )?;
        let (formatted, revision) = self.decode_with_revision(response)?;
        self.adopt_revision(revision);
        Ok(formatted)
    }

    pub fn check(&self) -> Result<CheckReport> {
        let response = self.call(
            self.agent
                .get(self.url("/api/v1/check"))
                .header(AUTHORIZATION, &self.authorization)
                .call(),
        )?;
        self.decode(response)
    }

    fn decode_item_mutation(&mut self, response: Response<ureq::Body>) -> Result<CatalogItem> {
        let (item, revision): (ItemResponse, _) = self.decode_with_revision(response)?;
        self.revision = item.revision;
        self.adopt_revision(revision);
        Ok(item.item)
    }

    fn accept_empty_mutation(&mut self, response: Response<ureq::Body>) -> Result<()> {
        let response = self.ensure_success(response)?;
        self.adopt_revision(response_revision(&response));
        Ok(())
    }

    fn decode<T: DeserializeOwned>(&self, response: Response<ureq::Body>) -> Result<T> {
        self.decode_with_revision(response).map(|(value, _)| value)
    }

    fn decode_with_revision<T: DeserializeOwned>(
        &self,
        response: Response<ureq::Body>,
    ) -> Result<(T, Option<String>)> {
        let mut response = self.ensure_success(response)?;
        let revision = response_revision(&response);
        let value = response
            .body_mut()
            .read_json()
            .map_err(|error| self.transport_error(error))?;
        Ok((value, revision))
    }

    fn ensure_success(&self, mut response: Response<ureq::Body>) -> Result<Response<ureq::Body>> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let fallback = status
            .canonical_reason()
            .unwrap_or("request failed")
            .to_owned();
        let body = response
            .body_mut()
            .read_json::<ApiErrorBody>()
            .unwrap_or(ApiErrorBody {
                error: ApiErrorDetail {
                    code: "http_error".to_owned(),
                    message: fallback,
                },
            });
        Err(Error::Api {
            address: self.base.clone(),
            status: status.as_u16(),
            code: body.error.code,
            message: body.error.message,
        })
    }

    fn call(
        &self,
        result: std::result::Result<Response<ureq::Body>, ureq::Error>,
    ) -> Result<Response<ureq::Body>> {
        result.map_err(|error| self.transport_error(error))
    }

    fn transport_error(&self, error: ureq::Error) -> Error {
        Error::Daemon {
            address: self.base.clone(),
            message: error.to_string(),
        }
    }

    fn required_uuid(&self, item: &CatalogItem) -> Result<uuid::Uuid> {
        item.uuid.ok_or_else(|| Error::Daemon {
            address: self.base.clone(),
            message: "daemon returned an item without a stable UUID".to_owned(),
        })
    }

    fn if_match(&self) -> String {
        format!("\"{}\"", self.revision)
    }

    fn adopt_revision(&mut self, revision: Option<String>) {
        if let Some(revision) = revision {
            self.revision = revision;
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

fn daemon_absent(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::Io(error) => matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::AddrNotAvailable
        ),
        ureq::Error::ConnectionFailed | ureq::Error::HostNotFound => true,
        _ => false,
    }
}

fn response_revision(response: &Response<ureq::Body>) -> Option<String> {
    response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim_matches('"').to_owned())
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_segments_are_percent_encoded_as_utf8() {
        assert_eq!(encode_path_segment("simple-key"), "simple-key");
        assert_eq!(encode_path_segment("a/b ?é"), "a%2Fb%20%3F%C3%A9");
    }
}
