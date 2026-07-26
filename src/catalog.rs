use std::collections::BTreeMap;
use std::path::Path;

use bibtex_parser::{DiagnosticSeverity, ParseStatus, ParsedEntry, Parser};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::attachments::{Attachment, parse_file_field};
use crate::{Error, Result};

pub const ID_FIELD: &str = "lantaiid";

#[derive(Debug)]
pub struct Catalog<'a> {
    document: bibtex_parser::ParsedDocument<'a>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogItem {
    pub uuid: Option<Uuid>,
    pub citation_key: String,
    pub entry_type: String,
    pub fields: Vec<CatalogField>,
    pub collections: Vec<String>,
    pub attachments: Vec<Attachment>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogField {
    pub name: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ItemView {
    pub uuid: Option<Uuid>,
    pub citation_key: String,
    pub entry_type: String,
    pub title: Option<String>,
    pub fields: Vec<CatalogField>,
    pub collections: Vec<String>,
    pub attachments: Vec<AttachmentView>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttachmentView {
    pub uuid: Option<Uuid>,
    pub title: Option<String>,
    pub path: String,
    pub media_type: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Degraded,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueSeverity {
    Warning,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckIssue {
    pub severity: IssueSeverity,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citation_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckReport {
    pub status: CheckStatus,
    pub entries: usize,
    pub warnings: usize,
    pub errors: usize,
    pub issues: Vec<CheckIssue>,
}

impl<'a> Catalog<'a> {
    pub fn parse(path: &Path, source: &'a str) -> Result<Self> {
        let document = Parser::new()
            .tolerant()
            .capture_source()
            .preserve_raw()
            .expand_values()
            .parse_source(path.display().to_string(), source)
            .map_err(|source| Error::ParseBibliography {
                path: path.to_owned(),
                source,
            })?;
        Ok(Self { document })
    }

    pub fn items(&self) -> impl Iterator<Item = CatalogItem> + '_ {
        self.document.entries().iter().map(CatalogItem::from)
    }

    pub fn is_syntactically_valid(&self) -> bool {
        self.document.status() == ParseStatus::Ok
    }

    pub fn views(&self) -> impl Iterator<Item = ItemView> + '_ {
        self.items().map(ItemView::from)
    }

    pub fn find(&self, id: &str) -> Result<CatalogItem> {
        let parsed_uuid = Uuid::parse_str(id).ok();
        let matches = self
            .document
            .entries()
            .iter()
            .filter(|entry| {
                parsed_uuid.is_some_and(|uuid| entry_uuid(entry) == Some(uuid)) || entry.key() == id
            })
            .collect::<Vec<_>>();

        match matches.as_slice() {
            [] => Err(Error::ItemNotFound { id: id.to_owned() }),
            [entry] => Ok(CatalogItem::from(*entry)),
            _ => Err(Error::AmbiguousItem { id: id.to_owned() }),
        }
    }

    pub fn check(&self) -> CheckReport {
        let mut issues = Vec::new();

        for diagnostic in self.document.diagnostics() {
            let (line, column) = diagnostic.source.map_or((None, None), |source| {
                (Some(source.line), Some(source.column))
            });
            issues.push(CheckIssue {
                severity: match diagnostic.severity {
                    DiagnosticSeverity::Error => IssueSeverity::Error,
                    DiagnosticSeverity::Warning | DiagnosticSeverity::Info => {
                        IssueSeverity::Warning
                    }
                },
                code: diagnostic.code.to_string(),
                message: diagnostic.message.clone(),
                citation_key: None,
                line,
                column,
            });
        }

        let mut keys: BTreeMap<&str, Vec<&ParsedEntry<'_>>> = BTreeMap::new();
        let mut uuids: BTreeMap<Uuid, Vec<&ParsedEntry<'_>>> = BTreeMap::new();
        for entry in self.document.entries() {
            keys.entry(entry.key()).or_default().push(entry);
            match entry.get_as_string_ignore_case(ID_FIELD) {
                None => issues.push(entry_issue(
                    IssueSeverity::Warning,
                    "missing-lantaiid",
                    "entry has no stable Lantai UUID",
                    entry,
                )),
                Some(value) => match Uuid::parse_str(value.trim()) {
                    Ok(uuid) => uuids.entry(uuid).or_default().push(entry),
                    Err(_) => issues.push(entry_issue(
                        IssueSeverity::Error,
                        "invalid-lantaiid",
                        format!("invalid Lantai UUID: {value}"),
                        entry,
                    )),
                },
            }
            if let Some(value) = entry.get_as_string_ignore_case("file")
                && let Err(error) = parse_file_field(&value)
            {
                issues.push(entry_issue(
                    IssueSeverity::Error,
                    "invalid-file-field",
                    error.to_string(),
                    entry,
                ));
            }
        }

        for (key, entries) in keys.into_iter().filter(|(_, entries)| entries.len() > 1) {
            issues.push(CheckIssue {
                severity: IssueSeverity::Error,
                code: "duplicate-citation-key".to_owned(),
                message: format!("citation key {key:?} occurs {} times", entries.len()),
                citation_key: Some(key.to_owned()),
                line: entries[0].source.map(|source| source.line),
                column: entries[0].source.map(|source| source.column),
            });
        }

        for (uuid, entries) in uuids.into_iter().filter(|(_, entries)| entries.len() > 1) {
            issues.push(CheckIssue {
                severity: IssueSeverity::Error,
                code: "duplicate-lantaiid".to_owned(),
                message: format!("Lantai UUID {uuid} occurs {} times", entries.len()),
                citation_key: Some(entries[0].key().to_owned()),
                line: entries[0].source.map(|source| source.line),
                column: entries[0].source.map(|source| source.column),
            });
        }

        let warnings = issues
            .iter()
            .filter(|issue| issue.severity == IssueSeverity::Warning)
            .count();
        let errors = issues
            .iter()
            .filter(|issue| issue.severity == IssueSeverity::Error)
            .count();
        let degraded = self.document.status() != ParseStatus::Ok || errors > 0;

        CheckReport {
            status: if degraded {
                CheckStatus::Degraded
            } else {
                CheckStatus::Ok
            },
            entries: self.document.entries().len(),
            warnings,
            errors,
            issues,
        }
    }
}

impl From<&ParsedEntry<'_>> for CatalogItem {
    fn from(entry: &ParsedEntry<'_>) -> Self {
        let fields = entry
            .fields
            .iter()
            .map(|field| CatalogField {
                name: field.name.to_string(),
                value: field
                    .value
                    .expanded_text()
                    .map_or_else(|| field.value.plain_text(), std::borrow::ToOwned::to_owned),
                raw: field.value.raw_text().map(str::to_owned),
            })
            .collect();

        Self {
            uuid: entry_uuid(entry),
            citation_key: entry.key().to_owned(),
            entry_type: entry.ty.to_string(),
            fields,
            collections: entry
                .get_as_string_ignore_case("keywords")
                .map_or_else(Vec::new, |keywords| {
                    crate::collections::normalize(keywords.split(','))
                }),
            attachments: entry
                .get_as_string_ignore_case("file")
                .and_then(|value| parse_file_field(&value).ok())
                .unwrap_or_default(),
        }
    }
}

impl From<CatalogItem> for ItemView {
    fn from(item: CatalogItem) -> Self {
        let title = item
            .fields
            .iter()
            .find(|field| field.name.eq_ignore_ascii_case("title"))
            .map(|field| field.value.clone());
        Self {
            uuid: item.uuid,
            citation_key: item.citation_key,
            entry_type: item.entry_type,
            title,
            fields: item.fields,
            collections: item.collections,
            attachments: item
                .attachments
                .into_iter()
                .map(AttachmentView::from)
                .collect(),
        }
    }
}

impl From<ItemView> for CatalogItem {
    fn from(view: ItemView) -> Self {
        Self {
            uuid: view.uuid,
            citation_key: view.citation_key,
            entry_type: view.entry_type,
            fields: view.fields,
            collections: view.collections,
            attachments: view.attachments.into_iter().map(Attachment::from).collect(),
        }
    }
}

impl From<Attachment> for AttachmentView {
    fn from(attachment: Attachment) -> Self {
        Self {
            uuid: attachment.uuid,
            title: (!attachment.title.is_empty()).then_some(attachment.title),
            path: attachment.path,
            media_type: attachment.media_type,
        }
    }
}

impl From<AttachmentView> for Attachment {
    fn from(view: AttachmentView) -> Self {
        Self {
            uuid: view.uuid,
            title: view.title.unwrap_or_default(),
            path: view.path,
            media_type: view.media_type,
        }
    }
}

fn entry_uuid(entry: &ParsedEntry<'_>) -> Option<Uuid> {
    entry
        .get_as_string_ignore_case(ID_FIELD)
        .and_then(|value| Uuid::parse_str(value.trim()).ok())
}

fn entry_issue(
    severity: IssueSeverity,
    code: &str,
    message: impl Into<String>,
    entry: &ParsedEntry<'_>,
) -> CheckIssue {
    CheckIssue {
        severity,
        code: code.to_owned(),
        message: message.into(),
        citation_key: Some(entry.key().to_owned()),
        line: entry.source.map(|source| source.line),
        column: entry.source.map(|source| source.column),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    const UUID: &str = "cc9e50c4-55ee-4471-b17c-c41684f64bf9";

    #[test]
    fn catalog_projects_fields_raw_values_and_collections() {
        let source = format!(
            r#"@article{{lovelace1843sketch,
  title = {{A Sketch of the Analytical Engine}},
  keywords = {{history, Computing, history}},
  abstract = "raw " # {{value}},
  lantaiid = {{{UUID}}}
}}"#
        );
        let catalog = Catalog::parse(Path::new("references.bib"), &source).unwrap();
        let item = catalog.find(UUID).unwrap();

        assert_eq!(item.citation_key, "lovelace1843sketch");
        assert_eq!(item.collections, ["Computing", "history"]);
        assert_eq!(
            ItemView::from(item.clone()).title.as_deref(),
            Some("A Sketch of the Analytical Engine")
        );
        let abstract_field = item
            .fields
            .iter()
            .find(|field| field.name == "abstract")
            .unwrap();
        assert_eq!(abstract_field.raw.as_deref(), Some("\"raw \" # {value}"));
    }

    #[test]
    fn rich_item_view_preserves_all_fields_and_attachment_kinds() {
        let managed_attachment = "5025cd5a-ead6-47c0-bb9e-b5399556af98-paper.pdf";
        let source = format!(
            r#"@misc{{external,
  note = "raw " # {{value}},
  keywords = {{Zeta, alpha}},
  file = {{Managed:library.files/item/{managed_attachment}:application/pdf;:/tmp/external.pdf:application/pdf}}
}}"#
        );
        let catalog = Catalog::parse(Path::new("references.bib"), &source).unwrap();
        let item = catalog.find("external").unwrap();
        let view = ItemView::from(item.clone());

        assert_eq!(view.uuid, None);
        assert_eq!(view.title, None);
        assert_eq!(
            view.fields
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>(),
            ["note", "keywords", "file"]
        );
        assert_eq!(view.fields[0].raw.as_deref(), Some("\"raw \" # {value}"));
        assert_eq!(view.collections, ["alpha", "Zeta"]);
        assert_eq!(view.attachments.len(), 2);
        assert_eq!(
            view.attachments[0].uuid,
            Some(Uuid::parse_str("5025cd5a-ead6-47c0-bb9e-b5399556af98").unwrap())
        );
        assert_eq!(view.attachments[0].title.as_deref(), Some("Managed"));
        assert_eq!(view.attachments[1].uuid, None);
        assert_eq!(view.attachments[1].title, None);
        assert_eq!(CatalogItem::from(view), item);
    }

    #[test]
    fn check_reports_missing_invalid_and_duplicate_identities() {
        let source = format!(
            r#"@book{{duplicate, title = {{First}}, lantaiid = {{{UUID}}}}}
@book{{duplicate, title = {{Second}}, lantaiid = {{{UUID}}}}}
@misc{{bad, lantaiid = {{not-a-uuid}}}}
@online{{missing, title = {{No UUID}}}}"#
        );
        let catalog = Catalog::parse(Path::new("references.bib"), &source).unwrap();
        let report = catalog.check();
        let codes = report
            .issues
            .iter()
            .map(|issue| issue.code.as_str())
            .collect::<HashSet<_>>();

        assert_eq!(report.status, CheckStatus::Degraded);
        assert!(codes.contains("missing-lantaiid"));
        assert!(codes.contains("invalid-lantaiid"));
        assert!(codes.contains("duplicate-citation-key"));
        assert!(codes.contains("duplicate-lantaiid"));
    }

    #[test]
    fn malformed_input_is_retained_as_a_check_issue() {
        let source = "@book{broken, title = {unterminated";
        let catalog = Catalog::parse(Path::new("references.bib"), source).unwrap();
        let report = catalog.check();

        assert_eq!(report.status, CheckStatus::Degraded);
        assert!(report.errors > 0);
        assert!(report.issues.iter().any(|issue| issue.line == Some(1)));
    }
}
