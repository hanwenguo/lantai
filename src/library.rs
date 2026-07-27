use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use bibtex_parser::{
    Entry, EntryType, Field, ParseStatus, ParsedBlock, ParsedDocument, ParsedEntry, Parser,
    ResourceKind, Value, classify_resource_field, document_to_string, parse_date_parts,
    parse_names,
};
use fs4::FileExt;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use uuid::Uuid;

use crate::attachments::{
    Attachment, encode_file_field, parse_file_field, path_to_bibtex, safe_relative_path,
    sanitize_filename,
};
use crate::catalog::{Catalog, CheckIssue, CheckReport, CheckStatus, ID_FIELD, IssueSeverity};
use crate::config::create_dir_all;
use crate::keys::generate_citation_key;
use crate::{Error, Result};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LibraryLayout {
    pub bibliography: PathBuf,
    pub attachments: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewItem {
    pub entry_type: String,
    pub citation_key: Option<String>,
    pub fields: Vec<(String, String)>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AddedItem {
    pub uuid: Uuid,
    pub citation_key: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MutationResult {
    pub uuid: Uuid,
    pub citation_key: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ItemPatch {
    pub set: BTreeMap<String, String>,
    pub set_raw: BTreeMap<String, String>,
    pub unset: Vec<String>,
    pub collections: Option<Vec<String>>,
    pub citation_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RemovedItem {
    pub uuid: Option<Uuid>,
    pub citation_key: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FormatResult {
    pub changed: bool,
    pub assigned_ids: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttachedFile {
    pub item_uuid: Uuid,
    pub attachment_uuid: Uuid,
    pub citation_key: String,
    pub title: String,
    pub path: String,
    pub media_type: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DetachedFile {
    pub item_uuid: Uuid,
    pub attachment_uuid: Uuid,
    pub citation_key: String,
    pub trashed_to: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TrashEntry {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct LibraryStore {
    layout: LibraryLayout,
}

#[derive(Debug)]
struct StagedTrash {
    directory: PathBuf,
    files: Vec<(PathBuf, PathBuf)>,
}

impl LibraryLayout {
    pub fn new(bibliography: PathBuf) -> Result<Self> {
        let file_stem = bibliography
            .file_stem()
            .ok_or_else(|| Error::InvalidLibraryPath {
                path: bibliography.clone(),
            })?;
        let mut attachments = bibliography.clone();
        let mut attachment_name = file_stem.to_os_string();
        attachment_name.push(".files");
        attachments.set_file_name(attachment_name);

        Ok(Self {
            bibliography,
            attachments,
        })
    }

    pub fn with_attachments(bibliography: PathBuf, attachments: PathBuf) -> Result<Self> {
        if bibliography.file_name().is_none() {
            return Err(Error::InvalidLibraryPath { path: bibliography });
        }
        Ok(Self {
            bibliography,
            attachments,
        })
    }

    pub fn initialize(&self) -> Result<()> {
        if let Some(parent) = self.bibliography.parent() {
            create_dir_all(parent)?;
        }

        if self.bibliography.exists() {
            if !self.bibliography.is_file() {
                return Err(Error::LibraryNotFile {
                    path: self.bibliography.clone(),
                });
            }
        } else {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.bibliography)
                .and_then(|file| file.sync_all())
                .map_err(|source| Error::Write {
                    path: self.bibliography.clone(),
                    source,
                })?;
        }

        create_dir_all(&self.attachments)?;
        create_dir_all(&self.attachments.join(".trash"))?;
        Ok(())
    }

    pub fn read_utf8(&self) -> Result<String> {
        let bytes = fs::read(&self.bibliography).map_err(|source| Error::Read {
            path: self.bibliography.clone(),
            source,
        })?;
        String::from_utf8(bytes).map_err(|_| Error::LibraryNotUtf8 {
            path: self.bibliography.clone(),
        })
    }
}

impl LibraryStore {
    pub const fn new(layout: LibraryLayout) -> Self {
        Self { layout }
    }

    pub fn add_item(&self, item: NewItem) -> Result<AddedItem> {
        validate_new_item(&item)?;
        let uuid = Uuid::new_v4();
        self.mutate_document(|document| {
            let existing_keys = document
                .entries()
                .iter()
                .map(ParsedEntry::key)
                .collect::<Vec<_>>();
            let citation_key = match &item.citation_key {
                Some(key) => {
                    if existing_keys.contains(&key.as_str()) {
                        return Err(Error::DuplicateCitationKey { key: key.clone() });
                    }
                    key.clone()
                }
                None => generate_citation_key(
                    field_value(&item.fields, "author"),
                    field_value(&item.fields, "date").or_else(|| field_value(&item.fields, "year")),
                    field_value(&item.fields, "title"),
                    existing_keys,
                ),
            };

            let entry_type = EntryType::parse(&item.entry_type).into_owned();
            let mut fields = item
                .fields
                .iter()
                .map(|(name, value)| Field {
                    name: Cow::Owned(name.to_ascii_lowercase()),
                    value: Value::Literal(Cow::Owned(value.clone())),
                })
                .collect::<Vec<_>>();
            fields.push(Field {
                name: Cow::Borrowed(ID_FIELD),
                value: Value::Literal(Cow::Owned(uuid.to_string())),
            });
            document.push_entry(ParsedEntry::from_entry(
                Entry {
                    ty: entry_type,
                    key: Cow::Owned(citation_key.clone()),
                    fields,
                },
                None,
            ));

            Ok(AddedItem { uuid, citation_key })
        })
    }

    pub fn import_biblatex(&self, imported: &str) -> Result<Vec<AddedItem>> {
        let imported = imported.trim();
        let initial = self.parse_import(imported)?;
        if initial.entries().is_empty() {
            return Err(Error::ImportHasNoEntries);
        }

        self.mutate_source(|source| {
            let current = self.parse_for_write(source)?;
            let imported_document = self.parse_import(imported)?;
            let mut keys = current
                .entries()
                .iter()
                .map(|entry| entry.key().to_owned())
                .collect::<HashSet<_>>();
            let mut uuids = HashSet::new();
            for entry in current.entries() {
                if let Some(uuid) = entry_uuid(entry)? {
                    uuids.insert(uuid);
                }
            }
            let mut replacements = Vec::new();
            for entry in imported_document.entries() {
                if !keys.insert(entry.key().to_owned()) {
                    return Err(Error::DuplicateCitationKey {
                        key: entry.key().to_owned(),
                    });
                }
                match entry_uuid(entry)? {
                    Some(uuid) if !uuids.insert(uuid) => {
                        return Err(Error::DuplicateUuid { uuid });
                    }
                    Some(_) => {}
                    None => {
                        let uuid = Uuid::new_v4();
                        uuids.insert(uuid);
                        let (offset, insertion) = field_insertion(
                            imported,
                            entry,
                            &[(ID_FIELD.to_owned(), format!("{{{uuid}}}"))],
                        )?;
                        replacements.push((offset, offset, insertion));
                    }
                }
            }
            let imported = apply_replacements(imported, replacements)?;
            let imported_document = self.parse_import(&imported)?;
            let added = imported_document
                .entries()
                .iter()
                .map(|entry| {
                    Ok(AddedItem {
                        uuid: entry_uuid(entry)?.ok_or_else(|| Error::InvalidItemIdentity {
                            key: entry.key().to_owned(),
                            field: ID_FIELD,
                        })?,
                        citation_key: entry.key().to_owned(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let mut output = source.trim_end().to_owned();
            if !output.is_empty() {
                output.push_str("\n\n");
            }
            output.push_str(imported.trim());
            output.push('\n');
            Ok((output, added))
        })
    }

    pub fn set_fields(
        &self,
        id: &str,
        fields: &[(String, String)],
        citation_key: Option<&str>,
    ) -> Result<MutationResult> {
        validate_field_pairs(fields)?;
        if let Some(key) = citation_key {
            validate_citation_key(key)?;
        }
        self.mutate_item(id, |document, index, uuid| {
            if let Some(key) = citation_key {
                ensure_key_available(document, index, key)?;
            }
            let entry = &mut document.entries_mut()[index];
            for (name, value) in fields {
                set_literal_field(entry, name, value);
            }
            if let Some(key) = citation_key {
                entry.rename_key(Cow::Owned(key.to_owned()));
            }
            Ok(MutationResult {
                uuid,
                citation_key: entry.key().to_owned(),
            })
        })
    }

    pub fn set_raw_fields(&self, id: &str, fields: &[(String, String)]) -> Result<MutationResult> {
        validate_field_pairs(fields)?;
        let fields = fields
            .iter()
            .map(|(name, expression)| {
                let expression = validate_raw_expression(name, expression)?;
                Ok((name.to_ascii_lowercase(), expression))
            })
            .collect::<Result<Vec<_>>>()?;

        self.mutate_source(|source| {
            let (working_source, uuid) = self.adopt_identity(source, id)?;
            let document = self.parse_for_write(&working_source)?;
            let index = find_entry_index(&document, &uuid.to_string())?;
            let entry = &document.entries()[index];
            let citation_key = entry.key().to_owned();
            let output = patch_raw_fields(&working_source, entry, &fields)?;
            Ok((output, MutationResult { uuid, citation_key }))
        })
    }

    pub fn patch_item(&self, id: &str, patch: ItemPatch) -> Result<MutationResult> {
        let set = patch.set.into_iter().collect::<Vec<_>>();
        let set_raw = patch
            .set_raw
            .into_iter()
            .map(|(name, expression)| {
                validate_mutable_field_name(&name)?;
                Ok((
                    name.to_ascii_lowercase(),
                    validate_raw_expression(&name, &expression)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        validate_field_pairs(&set)?;
        for name in &patch.unset {
            validate_mutable_field_name(name)?;
        }
        let mut touched = HashSet::new();
        for name in set
            .iter()
            .map(|(name, _)| name)
            .chain(set_raw.iter().map(|(name, _)| name))
            .chain(patch.unset.iter())
        {
            if !touched.insert(name.to_ascii_lowercase()) {
                return Err(Error::DuplicateField {
                    field: name.clone(),
                });
            }
        }
        if let Some(key) = patch.citation_key.as_deref() {
            validate_citation_key(key)?;
        }
        let collections = patch.collections.map(|collections| {
            crate::collections::normalize(collections.iter().map(String::as_str))
        });

        self.mutate_source(|source| {
            let (mut working_source, uuid) = self.adopt_identity(source, id)?;
            if !set_raw.is_empty() {
                let document = self.parse_for_write(&working_source)?;
                let index = find_entry_index(&document, &uuid.to_string())?;
                working_source =
                    patch_raw_fields(&working_source, &document.entries()[index], &set_raw)?;
            }

            let mut document = self.parse_for_write(&working_source)?;
            let index = find_entry_index(&document, &uuid.to_string())?;
            if let Some(key) = patch.citation_key.as_deref() {
                ensure_key_available(&document, index, key)?;
            }
            let entry = &mut document.entries_mut()[index];
            for (name, value) in &set {
                set_literal_field(entry, name, value);
            }
            if let Some(collections) = collections
                .as_ref()
                .filter(|collections| !collections.is_empty())
            {
                set_literal_field(entry, "keywords", &collections.join(", "));
            }
            if let Some(key) = patch.citation_key.as_deref() {
                entry.rename_key(Cow::Owned(key.to_owned()));
            }
            let citation_key = entry.key().to_owned();
            let stage_one = render_preserved_document(&document, &self.layout.bibliography)?;

            let remove_keywords = collections.as_ref().is_some_and(Vec::is_empty);
            if patch.unset.is_empty() && !remove_keywords {
                return Ok((stage_one, MutationResult { uuid, citation_key }));
            }
            let mut document = self.parse_for_write(&stage_one)?;
            let index = find_entry_index(&document, &uuid.to_string())?;
            let entry = &mut document.entries_mut()[index];
            for name in &patch.unset {
                remove_fields_ignore_case(entry, name);
            }
            if remove_keywords {
                remove_fields_ignore_case(entry, "keywords");
            }
            let output = render_preserved_document(&document, &self.layout.bibliography)?;
            Ok((output, MutationResult { uuid, citation_key }))
        })
    }

    pub fn unset_fields(&self, id: &str, fields: &[String]) -> Result<MutationResult> {
        for name in fields {
            validate_mutable_field_name(name)?;
        }
        self.mutate_item(id, |document, index, uuid| {
            let entry = &mut document.entries_mut()[index];
            for name in fields {
                remove_fields_ignore_case(entry, name);
            }
            Ok(MutationResult {
                uuid,
                citation_key: entry.key().to_owned(),
            })
        })
    }

    pub fn add_collections(&self, id: &str, collections: &[String]) -> Result<MutationResult> {
        self.change_collections(id, collections, true)
    }

    pub fn remove_collections(&self, id: &str, collections: &[String]) -> Result<MutationResult> {
        self.change_collections(id, collections, false)
    }

    /// Replace session-owned collections while retaining any added by translators or external edits.
    pub fn rebase_collections(
        &self,
        id: &str,
        previous: &[String],
        replacement: &[String],
    ) -> Result<MutationResult> {
        let previous = previous
            .iter()
            .map(|collection| collection.trim().to_ascii_lowercase())
            .collect::<HashSet<_>>();
        let replacement = crate::collections::normalize(replacement.iter().map(String::as_str));
        self.mutate_item(id, |document, index, uuid| {
            let entry = &mut document.entries_mut()[index];
            let mut collections = entry
                .get_as_string_ignore_case("keywords")
                .map_or_else(Vec::new, |value| {
                    crate::collections::normalize(value.split(','))
                });
            collections.retain(|collection| !previous.contains(&collection.to_ascii_lowercase()));
            collections.extend(replacement.iter().cloned());
            collections = crate::collections::normalize(collections.iter().map(String::as_str));
            if collections.is_empty() {
                remove_fields_ignore_case(entry, "keywords");
            } else {
                set_literal_field(entry, "keywords", &collections.join(", "));
            }
            Ok(MutationResult {
                uuid,
                citation_key: entry.key().to_owned(),
            })
        })
    }

    pub fn remove_item(&self, id: &str) -> Result<RemovedItem> {
        self.mutate_with_trashed_files(|source| {
            let document = self.parse_for_write(source)?;
            let index = find_entry_index(&document, id)?;
            let entry = &document.entries()[index];
            let attachments = entry
                .get_as_string_ignore_case("file")
                .map_or_else(|| Ok(Vec::new()), |value| parse_file_field(&value))?;
            let managed = attachments
                .iter()
                .filter_map(|attachment| {
                    let uuid = attachment.uuid?;
                    self.managed_attachment_path(attachment)
                        .ok()
                        .map(|path| (uuid, path))
                })
                .collect::<Vec<_>>();
            let span = entry.source.ok_or_else(|| Error::DegradedBibliography {
                path: self.layout.bibliography.clone(),
                message: format!("entry {} has no source location", entry.key()),
            })?;
            let mut output = source.to_owned();
            output.replace_range(span.byte_start..span.byte_end, "");
            Ok((
                output,
                RemovedItem {
                    uuid: entry_uuid(entry)?,
                    citation_key: entry.key().to_owned(),
                },
                managed,
            ))
        })
        .map(|(removed, _)| removed)
    }

    pub fn format(&self) -> Result<FormatResult> {
        self.mutate_source(|source| {
            let mut document = self.parse_for_write(source)?;
            let mut assigned_ids = 0;
            for entry in document.entries_mut() {
                match entry_uuid(entry)? {
                    Some(_) => {}
                    None => {
                        set_literal_field(entry, ID_FIELD, &Uuid::new_v4().to_string());
                        assigned_ids += 1;
                    }
                }
                normalize_managed_fields(entry)?;
                normalize_keywords(entry);
            }
            let output = render_canonical_document(&document);
            Ok((
                output.clone(),
                FormatResult {
                    changed: output != source,
                    assigned_ids,
                },
            ))
        })
    }

    pub fn export_biblatex(&self, ids: &[String]) -> Result<String> {
        let source = self.layout.read_utf8()?;
        self.export_biblatex_from(&source, ids)
    }

    pub fn export_biblatex_from(&self, source: &str, ids: &[String]) -> Result<String> {
        let document = self.parse_for_write(source)?;
        if ids.is_empty() {
            return Ok(render_canonical_document(&document));
        }
        let mut selected = HashSet::new();
        for id in ids {
            selected.insert(find_entry_index(&document, id)?);
        }
        Ok(render_canonical_document_selected(&document, &selected))
    }

    pub fn attach_file(
        &self,
        id: &str,
        source_path: &Path,
        title: Option<&str>,
        media_type: Option<&str>,
        limit: u64,
    ) -> Result<AttachedFile> {
        let source_name = source_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("attachment");
        self.attach_file_named(id, source_path, source_name, title, media_type, limit)
    }

    pub fn attach_file_named(
        &self,
        id: &str,
        source_path: &Path,
        source_name: &str,
        title: Option<&str>,
        media_type: Option<&str>,
        limit: u64,
    ) -> Result<AttachedFile> {
        if !source_path.is_file() {
            return Err(Error::AttachmentNotFile {
                path: source_path.to_owned(),
            });
        }
        if fs::metadata(source_path)
            .map_err(|source| Error::Read {
                path: source_path.to_owned(),
                source,
            })?
            .len()
            > limit
        {
            return Err(Error::AttachmentTooLarge { limit });
        }

        let identity = self.set_fields(id, &[], None)?;
        let attachment_uuid = Uuid::new_v4();
        let filename = format!(
            "{attachment_uuid}-{}",
            sanitize_filename(Path::new(source_name))
        );
        let item_directory = self.layout.attachments.join(identity.uuid.to_string());
        create_dir_all(&item_directory)?;
        let destination = item_directory.join(filename);
        let mut temporary =
            NamedTempFile::new_in(&item_directory).map_err(|source| Error::Write {
                path: destination.clone(),
                source,
            })?;
        let mut input = fs::File::open(source_path).map_err(|source| Error::Read {
            path: source_path.to_owned(),
            source,
        })?;
        let size = copy_with_limit(&mut input, temporary.as_file_mut(), limit)?;
        temporary
            .as_file_mut()
            .sync_all()
            .map_err(|source| Error::Write {
                path: destination.clone(),
                source,
            })?;
        temporary
            .persist_noclobber(&destination)
            .map_err(|error| Error::Write {
                path: destination.clone(),
                source: error.error,
            })?;
        sync_parent(&item_directory, &destination)?;

        let relative = attachment_reference(&self.layout.bibliography, &destination)?;
        let title = title.unwrap_or(source_name).to_owned();
        let media_type = media_type.map_or_else(
            || {
                mime_guess::from_path(source_path)
                    .first_or_octet_stream()
                    .essence_str()
                    .to_owned()
            },
            str::to_owned,
        );
        let attachment = Attachment::managed(
            attachment_uuid,
            title.clone(),
            relative.clone(),
            media_type.clone(),
        );

        let mutation = self.mutate_item(&identity.uuid.to_string(), |document, index, _| {
            let entry = &mut document.entries_mut()[index];
            let mut attachments = entry
                .get_as_string_ignore_case("file")
                .map_or_else(|| Ok(Vec::new()), |value| parse_file_field(&value))?;
            attachments.push(attachment.clone());
            set_literal_field(entry, "file", &encode_file_field(&attachments));
            Ok(())
        });
        if let Err(error) = mutation {
            let _ = fs::remove_file(&destination);
            return Err(error);
        }

        Ok(AttachedFile {
            item_uuid: identity.uuid,
            attachment_uuid,
            citation_key: identity.citation_key,
            title,
            path: relative,
            media_type,
            size,
        })
    }

    pub fn attachment_file(
        &self,
        id: &str,
        attachment_uuid: Uuid,
    ) -> Result<(Attachment, PathBuf)> {
        let source = self.layout.read_utf8()?;
        let catalog = Catalog::parse(&self.layout.bibliography, &source)?;
        let item = catalog.find(id)?;
        let matches = item
            .attachments
            .into_iter()
            .filter(|attachment| attachment.uuid == Some(attachment_uuid))
            .collect::<Vec<_>>();
        let attachment = match matches.as_slice() {
            [] => {
                return Err(Error::AttachmentNotFound {
                    id: attachment_uuid.to_string(),
                });
            }
            [attachment] => attachment.clone(),
            _ => {
                return Err(Error::InvalidFileField {
                    message: format!("attachment UUID {attachment_uuid} occurs more than once"),
                });
            }
        };
        let path = self.managed_attachment_path(&attachment)?;
        if !path.is_file() {
            return Err(Error::AttachmentNotFound {
                id: attachment_uuid.to_string(),
            });
        }
        Ok((attachment, path))
    }

    pub fn detach_attachment(&self, id: &str, attachment_uuid: Uuid) -> Result<DetachedFile> {
        let (mut detached, destinations) = self.mutate_with_trashed_files(|source| {
            let (working_source, item_uuid) = self.adopt_identity(source, id)?;
            let mut document = self.parse_for_write(&working_source)?;
            let index = find_entry_index(&document, &item_uuid.to_string())?;
            let entry = &mut document.entries_mut()[index];
            let citation_key = entry.key().to_owned();
            let mut attachments = entry
                .get_as_string_ignore_case("file")
                .map_or_else(|| Ok(Vec::new()), |value| parse_file_field(&value))?;
            let matches = attachments
                .iter()
                .enumerate()
                .filter(|(_, attachment)| attachment.uuid == Some(attachment_uuid))
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            let attachment_index = match matches.as_slice() {
                [] => {
                    return Err(Error::AttachmentNotFound {
                        id: attachment_uuid.to_string(),
                    });
                }
                [index] => *index,
                _ => {
                    return Err(Error::InvalidFileField {
                        message: format!("attachment UUID {attachment_uuid} occurs more than once"),
                    });
                }
            };
            let attachment = attachments.remove(attachment_index);
            let managed_path = self.managed_attachment_path(&attachment)?;
            if attachments.is_empty() {
                remove_fields_ignore_case(entry, "file");
            } else {
                set_literal_field(entry, "file", &encode_file_field(&attachments));
            }
            let output = render_preserved_document(&document, &self.layout.bibliography)?;
            Ok((
                output,
                DetachedFile {
                    item_uuid,
                    attachment_uuid,
                    citation_key,
                    trashed_to: None,
                },
                vec![(attachment_uuid, managed_path)],
            ))
        })?;
        detached.trashed_to = destinations.into_iter().next();
        Ok(detached)
    }

    pub fn trash_entries(&self) -> Result<Vec<TrashEntry>> {
        let root = self.layout.attachments.join(".trash");
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        collect_trash_entries(&root, &root, &mut entries)?;
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(entries)
    }

    pub fn purge_trash(&self) -> Result<usize> {
        let process_lock = process_lock(&self.layout.bibliography);
        let _process_guard = process_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lock_path = lock_path(&self.layout.bibliography);
        let _lock_file = open_exclusive_lock(&lock_path)?;
        let root = self.layout.attachments.join(".trash");
        if !root.exists() {
            return Ok(0);
        }
        let entries = self.trash_entries()?;
        for child in fs::read_dir(&root).map_err(|source| Error::Read {
            path: root.clone(),
            source,
        })? {
            let child = child.map_err(|source| Error::Read {
                path: root.clone(),
                source,
            })?;
            let path = child.path();
            if path.is_dir() {
                fs::remove_dir_all(&path).map_err(|source| Error::Write { path, source })?;
            } else {
                fs::remove_file(&path).map_err(|source| Error::Write { path, source })?;
            }
        }
        sync_parent(&self.layout.attachments, &root)?;
        Ok(entries.len())
    }

    pub fn check(&self) -> Result<CheckReport> {
        let source = self.layout.read_utf8()?;
        let catalog = Catalog::parse(&self.layout.bibliography, &source)?;
        let mut report = catalog.check();
        let parent =
            self.layout
                .bibliography
                .parent()
                .ok_or_else(|| Error::InvalidLibraryPath {
                    path: self.layout.bibliography.clone(),
                })?;
        let mut referenced_managed = HashSet::new();

        for item in catalog.items() {
            for attachment in item.attachments {
                let field_path = Path::new(&attachment.path);
                let absolute = if crate::attachments::safe_absolute_path(field_path) {
                    field_path.to_owned()
                } else if crate::attachments::safe_relative_path(field_path) {
                    parent.join(field_path)
                } else {
                    report.issues.push(CheckIssue {
                        severity: IssueSeverity::Error,
                        code: "unsafe-attachment-path".to_owned(),
                        message: format!("unsafe attachment path: {}", attachment.path),
                        citation_key: Some(item.citation_key.clone()),
                        line: None,
                        column: None,
                    });
                    continue;
                };
                if absolute.starts_with(&self.layout.attachments) {
                    referenced_managed.insert(absolute.clone());
                }
                if !absolute.is_file() {
                    report.issues.push(CheckIssue {
                        severity: IssueSeverity::Error,
                        code: "missing-attachment".to_owned(),
                        message: format!("attachment does not exist: {}", attachment.path),
                        citation_key: Some(item.citation_key.clone()),
                        line: None,
                        column: None,
                    });
                }
            }
        }

        if self.layout.attachments.is_dir() {
            let mut managed_files = Vec::new();
            collect_managed_files(
                &self.layout.attachments,
                &self.layout.attachments,
                &mut managed_files,
            )?;
            for path in managed_files {
                if !referenced_managed.contains(&path) {
                    let temporary = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(".tmp"));
                    report.issues.push(CheckIssue {
                        severity: IssueSeverity::Warning,
                        code: if temporary {
                            "stale-temporary-attachment".to_owned()
                        } else {
                            "orphan-managed-attachment".to_owned()
                        },
                        message: format!(
                            "unreferenced file in managed attachment store: {}",
                            path.display()
                        ),
                        citation_key: None,
                        line: None,
                        column: None,
                    });
                }
            }
        }

        report.warnings = report
            .issues
            .iter()
            .filter(|issue| issue.severity == IssueSeverity::Warning)
            .count();
        report.errors = report
            .issues
            .iter()
            .filter(|issue| issue.severity == IssueSeverity::Error)
            .count();
        if report.errors > 0 {
            report.status = CheckStatus::Degraded;
        }
        Ok(report)
    }

    fn change_collections(
        &self,
        id: &str,
        changed: &[String],
        add: bool,
    ) -> Result<MutationResult> {
        let changed = crate::collections::normalize(changed.iter().map(String::as_str));
        self.mutate_item(id, |document, index, uuid| {
            let entry = &mut document.entries_mut()[index];
            let mut collections = entry
                .get_as_string_ignore_case("keywords")
                .map_or_else(Vec::new, |value| {
                    crate::collections::normalize(value.split(','))
                });
            if add {
                collections.extend(changed.iter().cloned());
            } else {
                collections.retain(|collection| {
                    !changed
                        .iter()
                        .any(|removed| removed.eq_ignore_ascii_case(collection))
                });
            }
            collections = crate::collections::normalize(collections.iter().map(String::as_str));
            if collections.is_empty() {
                remove_fields_ignore_case(entry, "keywords");
            } else {
                set_literal_field(entry, "keywords", &collections.join(", "));
            }
            Ok(MutationResult {
                uuid,
                citation_key: entry.key().to_owned(),
            })
        })
    }

    fn mutate_item<T>(
        &self,
        id: &str,
        mut mutation: impl for<'a> FnMut(&mut ParsedDocument<'a>, usize, Uuid) -> Result<T>,
    ) -> Result<T> {
        self.mutate_source(|source| {
            let (working_source, uuid) = self.adopt_identity(source, id)?;
            let mut document = self.parse_for_write(&working_source)?;
            let index = find_entry_index(&document, &uuid.to_string())?;
            let result = mutation(&mut document, index, uuid)?;
            let output = render_preserved_document(&document, &self.layout.bibliography)?;
            Ok((output, result))
        })
    }

    fn mutate_document<T>(
        &self,
        mut mutation: impl for<'a> FnMut(&mut ParsedDocument<'a>) -> Result<T>,
    ) -> Result<T> {
        self.mutate_source(|source| {
            let mut document = self.parse_for_write(source)?;
            let result = mutation(&mut document)?;
            let output = render_preserved_document(&document, &self.layout.bibliography)?;
            Ok((output, result))
        })
    }

    fn mutate_source<T>(&self, mut mutation: impl FnMut(&str) -> Result<(String, T)>) -> Result<T> {
        for attempt in 0..3 {
            let process_lock = process_lock(&self.layout.bibliography);
            let _process_guard = process_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let lock_path = lock_path(&self.layout.bibliography);
            let _lock_file = open_exclusive_lock(&lock_path)?;

            let source = self.layout.read_utf8()?;
            let (mut output, result) = mutation(&source)?;
            if !output.ends_with('\n') && !output.is_empty() {
                output.push('\n');
            }
            validate_output(&self.layout.bibliography, &output)?;
            match atomic_replace(
                &self.layout.bibliography,
                source.as_bytes(),
                output.as_bytes(),
            ) {
                Err(Error::SourceChanged { .. }) if attempt < 2 => continue,
                Err(error) => return Err(error),
                Ok(()) => return Ok(result),
            }
        }
        unreachable!("the final transaction attempt always returns")
    }

    fn mutate_with_trashed_files<T>(
        &self,
        mut mutation: impl FnMut(&str) -> Result<(String, T, Vec<(Uuid, PathBuf)>)>,
    ) -> Result<(T, Vec<PathBuf>)> {
        for attempt in 0..3 {
            let process_lock = process_lock(&self.layout.bibliography);
            let _process_guard = process_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let lock_path = lock_path(&self.layout.bibliography);
            let _lock_file = open_exclusive_lock(&lock_path)?;
            let source = self.layout.read_utf8()?;
            let (mut output, result, files) = mutation(&source)?;
            if !output.ends_with('\n') && !output.is_empty() {
                output.push('\n');
            }
            validate_output(&self.layout.bibliography, &output)?;
            let staged = self.stage_trash(&files)?;
            match atomic_replace(
                &self.layout.bibliography,
                source.as_bytes(),
                output.as_bytes(),
            ) {
                Err(Error::SourceChanged { .. }) if attempt < 2 => {
                    cleanup_staged_trash(staged.as_ref());
                    continue;
                }
                Err(error) => {
                    cleanup_staged_trash(staged.as_ref());
                    return Err(error);
                }
                Ok(()) => {
                    let destinations = staged.as_ref().map_or_else(Vec::new, |staged| {
                        staged
                            .files
                            .iter()
                            .map(|(_, destination)| destination.clone())
                            .collect()
                    });
                    if let Some(staged) = staged {
                        finish_staged_trash(&staged, &self.layout.attachments)?;
                    }
                    return Ok((result, destinations));
                }
            }
        }
        unreachable!("the final transaction attempt always returns")
    }

    fn parse_for_write<'a>(&self, source: &'a str) -> Result<ParsedDocument<'a>> {
        let document = Parser::new()
            .capture_source()
            .preserve_raw()
            .parse_source(self.layout.bibliography.display().to_string(), source)
            .map_err(|source| Error::ParseBibliography {
                path: self.layout.bibliography.clone(),
                source,
            })?;
        if document.status() != ParseStatus::Ok {
            let message = document.diagnostics().first().map_or_else(
                || "BibLaTeX parsing did not complete".to_owned(),
                |diagnostic| diagnostic.message.clone(),
            );
            return Err(Error::DegradedBibliography {
                path: self.layout.bibliography.clone(),
                message,
            });
        }
        Ok(document)
    }

    fn parse_import<'a>(&self, source: &'a str) -> Result<ParsedDocument<'a>> {
        let document = Parser::new()
            .capture_source()
            .preserve_raw()
            .parse_source("<import>", source)
            .map_err(|source| Error::ParseBibliography {
                path: PathBuf::from("<import>"),
                source,
            })?;
        if document.status() != ParseStatus::Ok {
            let message = document.diagnostics().first().map_or_else(
                || "BibLaTeX import did not parse completely".to_owned(),
                |diagnostic| diagnostic.message.clone(),
            );
            return Err(Error::DegradedBibliography {
                path: PathBuf::from("<import>"),
                message,
            });
        }
        Ok(document)
    }

    fn adopt_identity(&self, source: &str, id: &str) -> Result<(String, Uuid)> {
        let document = self.parse_for_write(source)?;
        let index = find_entry_index(&document, id)?;
        let entry = &document.entries()[index];
        if let Some(uuid) = entry_uuid(entry)? {
            return Ok((source.to_owned(), uuid));
        }

        let uuid = Uuid::new_v4();
        let output = insert_raw_fields(
            source,
            entry,
            &[(ID_FIELD.to_owned(), format!("{{{uuid}}}"))],
        )?;
        Ok((output, uuid))
    }

    fn managed_attachment_path(&self, attachment: &Attachment) -> Result<PathBuf> {
        let relative = Path::new(&attachment.path);
        let parent =
            self.layout
                .bibliography
                .parent()
                .ok_or_else(|| Error::InvalidLibraryPath {
                    path: self.layout.bibliography.clone(),
                })?;
        let absolute = if crate::attachments::safe_absolute_path(relative) {
            relative.to_owned()
        } else if crate::attachments::safe_relative_path(relative) {
            parent.join(relative)
        } else {
            return Err(Error::UnsafeAttachmentPath {
                path: relative.to_owned(),
            });
        };
        if !absolute.starts_with(&self.layout.attachments) {
            return Err(Error::UnsafeAttachmentPath { path: absolute });
        }
        if absolute.exists() {
            let canonical_root =
                fs::canonicalize(&self.layout.attachments).map_err(|source| Error::Read {
                    path: self.layout.attachments.clone(),
                    source,
                })?;
            let canonical_path = fs::canonicalize(&absolute).map_err(|source| Error::Read {
                path: absolute.clone(),
                source,
            })?;
            if !canonical_path.starts_with(canonical_root) {
                return Err(Error::UnsafeAttachmentPath { path: absolute });
            }
        }
        Ok(absolute)
    }

    fn stage_trash(&self, files: &[(Uuid, PathBuf)]) -> Result<Option<StagedTrash>> {
        let mut unique = HashSet::new();
        let files = files
            .iter()
            .filter(|(_, source)| source.exists() && unique.insert(source.clone()))
            .collect::<Vec<_>>();
        if files.is_empty() {
            return Ok(None);
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let directory = self
            .layout
            .attachments
            .join(".trash")
            .join(format!("{timestamp}-{}", Uuid::new_v4()));
        create_dir_all(&directory)?;
        let mut staged = StagedTrash {
            directory,
            files: Vec::new(),
        };
        let stage_result = (|| {
            for (attachment_uuid, source) in files {
                let destination = staged.directory.join(format!(
                    "{attachment_uuid}-{}",
                    source.file_name().unwrap_or_default().to_string_lossy()
                ));
                let is_symlink = fs::symlink_metadata(source)
                    .map_err(|source_error| Error::Read {
                        path: source.clone(),
                        source: source_error,
                    })?
                    .file_type()
                    .is_symlink();
                let link_result = if is_symlink {
                    Err(std::io::Error::other("source is a symbolic link"))
                } else {
                    fs::hard_link(source, &destination)
                };
                if let Err(link_error) = link_result {
                    fs::copy(source, &destination).map_err(|copy_error| Error::Write {
                        path: destination.clone(),
                        source: std::io::Error::new(
                            copy_error.kind(),
                            format!("hard-link failed ({link_error}); copy failed ({copy_error})"),
                        ),
                    })?;
                }
                fs::File::open(&destination)
                    .and_then(|file| file.sync_all())
                    .map_err(|source| Error::Write {
                        path: destination.clone(),
                        source,
                    })?;
                sync_parent(&staged.directory, &destination)?;
                staged.files.push((source.clone(), destination));
            }
            Ok(())
        })();
        if let Err(error) = stage_result {
            cleanup_staged_trash(Some(&staged));
            return Err(error);
        }
        Ok(Some(staged))
    }
}

fn cleanup_staged_trash(staged: Option<&StagedTrash>) {
    if let Some(staged) = staged {
        let _ = fs::remove_dir_all(&staged.directory);
    }
}

fn finish_staged_trash(staged: &StagedTrash, attachment_root: &Path) -> Result<()> {
    for (source, _) in &staged.files {
        match fs::remove_file(source) {
            Ok(()) => {
                if let Some(parent) = source.parent() {
                    sync_parent(parent, source)?;
                }
                prune_empty_parent(source.parent(), attachment_root);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source_error) => {
                return Err(Error::Write {
                    path: source.clone(),
                    source: source_error,
                });
            }
        }
    }
    Ok(())
}

fn attachment_reference(bibliography: &Path, destination: &Path) -> Result<String> {
    let parent = bibliography
        .parent()
        .ok_or_else(|| Error::InvalidLibraryPath {
            path: bibliography.to_owned(),
        })?;
    if let Ok(relative) = destination.strip_prefix(parent) {
        path_to_bibtex(relative)
    } else if destination.is_absolute() {
        Ok(destination.to_string_lossy().replace('\\', "/"))
    } else {
        Err(Error::UnsafeAttachmentPath {
            path: destination.to_owned(),
        })
    }
}

fn find_entry_index(document: &ParsedDocument<'_>, id: &str) -> Result<usize> {
    let parsed_uuid = Uuid::parse_str(id).ok();
    let matches = document
        .entries()
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            parsed_uuid.is_some_and(|uuid| entry_uuid(entry).ok().flatten() == Some(uuid))
                || entry.key() == id
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Err(Error::ItemNotFound { id: id.to_owned() }),
        [index] => Ok(*index),
        _ => Err(Error::AmbiguousItem { id: id.to_owned() }),
    }
}

fn entry_uuid(entry: &ParsedEntry<'_>) -> Result<Option<Uuid>> {
    let fields = entry
        .fields
        .iter()
        .filter(|field| field.name.eq_ignore_ascii_case(ID_FIELD))
        .collect::<Vec<_>>();
    match fields.as_slice() {
        [] => Ok(None),
        [field] => Uuid::parse_str(field.value.plain_text().trim())
            .map(Some)
            .map_err(|_| Error::InvalidItemIdentity {
                key: entry.key().to_owned(),
                field: ID_FIELD,
            }),
        _ => Err(Error::InvalidItemIdentity {
            key: entry.key().to_owned(),
            field: ID_FIELD,
        }),
    }
}

fn ensure_key_available(document: &ParsedDocument<'_>, current: usize, key: &str) -> Result<()> {
    if document
        .entries()
        .iter()
        .enumerate()
        .any(|(index, entry)| index != current && entry.key() == key)
    {
        Err(Error::DuplicateCitationKey {
            key: key.to_owned(),
        })
    } else {
        Ok(())
    }
}

fn set_literal_field(entry: &mut ParsedEntry<'_>, name: &str, value: &str) {
    if let Some(field) = entry
        .fields
        .iter_mut()
        .find(|field| field.name.eq_ignore_ascii_case(name))
    {
        field.name = Cow::Owned(name.to_ascii_lowercase());
        field.value.value = Value::Literal(Cow::Owned(value.to_owned()));
        field.value.raw = None;
        field.value.expanded = None;
        field.raw = None;
    } else {
        entry.add_field(
            Cow::Owned(name.to_ascii_lowercase()),
            Value::Literal(Cow::Owned(value.to_owned())),
        );
    }
}

fn remove_fields_ignore_case(entry: &mut ParsedEntry<'_>, name: &str) {
    let indexes = entry
        .fields
        .iter()
        .enumerate()
        .filter(|(_, field)| field.name.eq_ignore_ascii_case(name))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    for index in indexes.into_iter().rev() {
        let removed = entry.remove_field_by_index(index);
        debug_assert!(removed);
    }
}

fn validate_field_pairs(fields: &[(String, String)]) -> Result<()> {
    let mut names = HashSet::new();
    for (name, _) in fields {
        validate_mutable_field_name(name)?;
        let normalized = name.to_ascii_lowercase();
        if !names.insert(normalized) {
            return Err(Error::DuplicateField {
                field: name.clone(),
            });
        }
    }
    Ok(())
}

fn validate_mutable_field_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::EmptyFieldName);
    }
    if !valid_name(name) {
        return Err(Error::InvalidFieldArgument {
            argument: name.to_owned(),
        });
    }
    if name.eq_ignore_ascii_case(ID_FIELD) {
        return Err(Error::ReservedField {
            field: name.to_owned(),
        });
    }
    Ok(())
}

fn validate_citation_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key
            .chars()
            .any(|character| character.is_whitespace() || ",{}()=\\\"#%".contains(character))
    {
        Err(Error::InvalidCitationKey {
            key: key.to_owned(),
        })
    } else {
        Ok(())
    }
}

fn validate_raw_expression(name: &str, expression: &str) -> Result<String> {
    let expression = expression.trim();
    if expression.is_empty() {
        return Err(Error::InvalidRawExpression {
            field: name.to_owned(),
            message: "expression is empty".to_owned(),
        });
    }
    let synthetic = format!("@misc{{raw,\n  value = {expression}\n}}\n");
    let document = Parser::new()
        .capture_source()
        .preserve_raw()
        .parse_document(&synthetic)
        .map_err(|error| Error::InvalidRawExpression {
            field: name.to_owned(),
            message: error.to_string(),
        })?;
    let valid = document.status() == ParseStatus::Ok
        && document.entries().len() == 1
        && document.entries()[0].fields.len() == 1
        && document.entries()[0].fields[0].value.raw_text() == Some(expression);
    if !valid {
        let message = document.diagnostics().first().map_or_else(
            || "expression does not form exactly one BibTeX value".to_owned(),
            |diagnostic| diagnostic.message.clone(),
        );
        return Err(Error::InvalidRawExpression {
            field: name.to_owned(),
            message,
        });
    }
    Ok(expression.to_owned())
}

fn patch_raw_fields(
    source: &str,
    entry: &ParsedEntry<'_>,
    fields: &[(String, String)],
) -> Result<String> {
    let mut replacements = Vec::new();
    let mut additions = Vec::new();
    for (name, expression) in fields {
        let matches = entry
            .fields
            .iter()
            .filter(|field| field.name.eq_ignore_ascii_case(name))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => additions.push((name.clone(), expression.clone())),
            [field] => {
                let span = field
                    .value_source
                    .ok_or_else(|| Error::DegradedBibliography {
                        path: PathBuf::from("<bibliography>"),
                        message: format!("field {name} has no source location"),
                    })?;
                replacements.push((span.byte_start, span.byte_end, expression.clone()));
            }
            _ => {
                return Err(Error::DuplicateField {
                    field: name.clone(),
                });
            }
        }
    }
    if !additions.is_empty() {
        let (offset, insertion) = field_insertion(source, entry, &additions)?;
        replacements.push((offset, offset, insertion));
    }
    apply_replacements(source, replacements)
}

fn insert_raw_fields(
    source: &str,
    entry: &ParsedEntry<'_>,
    fields: &[(String, String)],
) -> Result<String> {
    let (offset, insertion) = field_insertion(source, entry, fields)?;
    apply_replacements(source, vec![(offset, offset, insertion)])
}

fn field_insertion(
    source: &str,
    entry: &ParsedEntry<'_>,
    fields: &[(String, String)],
) -> Result<(usize, String)> {
    let entry_span = entry.source.ok_or_else(|| Error::DegradedBibliography {
        path: PathBuf::from("<bibliography>"),
        message: format!("entry {} has no source location", entry.key()),
    })?;
    let base = entry
        .fields
        .last()
        .and_then(|field| field.source)
        .or(entry.key_source);
    let base = base.ok_or_else(|| Error::DegradedBibliography {
        path: PathBuf::from("<bibliography>"),
        message: format!("entry {} has no insertion location", entry.key()),
    })?;
    let tail = source
        .get(base.byte_end..entry_span.byte_end)
        .ok_or_else(|| Error::DegradedBibliography {
            path: PathBuf::from("<bibliography>"),
            message: format!("entry {} has invalid source locations", entry.key()),
        })?;
    let rendered = fields
        .iter()
        .map(|(name, expression)| format!("{} = {expression}", name.to_ascii_lowercase()))
        .collect::<Vec<_>>()
        .join(",\n  ");

    if let Some(comma) = tail.find(',') {
        let offset = base.byte_end + comma + 1;
        Ok((offset, format!("\n  {rendered},")))
    } else {
        Ok((base.byte_end, format!(",\n  {rendered}")))
    }
}

fn apply_replacements(
    source: &str,
    mut replacements: Vec<(usize, usize, String)>,
) -> Result<String> {
    replacements.sort_by_key(|replacement| std::cmp::Reverse(replacement.0));
    let mut output = source.to_owned();
    let mut previous_start = source.len();
    for (start, end, replacement) in replacements {
        if start > end
            || end > previous_start
            || !source.is_char_boundary(start)
            || !source.is_char_boundary(end)
        {
            return Err(Error::DegradedBibliography {
                path: PathBuf::from("<bibliography>"),
                message: "overlapping or invalid source replacements".to_owned(),
            });
        }
        output.replace_range(start..end, &replacement);
        previous_start = start;
    }
    Ok(output)
}

fn normalize_managed_fields(entry: &mut ParsedEntry<'_>) -> Result<()> {
    normalize_field_aliases_and_legacy_date(entry);
    let mut replacements = BTreeMap::new();
    for (index, field) in entry.fields.iter().enumerate() {
        if !literal_or_number(&field.value.value) {
            continue;
        }
        let name = field.name.to_ascii_lowercase();
        let value = field.value.plain_text();
        let normalized = if is_creator_field(&name) {
            canonical_creator_list(&value)
        } else if is_date_field(&name) {
            canonical_date(&value)
        } else if name == "file" {
            let mut attachments = parse_file_field(&value)?;
            for attachment in &mut attachments {
                attachment.title = attachment.title.trim().to_owned();
                let path = attachment.path.trim().replace('\\', "/");
                attachment.path =
                    if attachment.uuid.is_some() && safe_relative_path(Path::new(&path)) {
                        path_to_bibtex(Path::new(&path))?
                    } else {
                        path
                    };
                attachment.media_type = attachment.media_type.trim().to_ascii_lowercase();
            }
            Some(encode_file_field(&attachments))
        } else {
            None
        };
        if let Some(normalized) = normalized
            && normalized != value
        {
            replacements.insert(index, normalized);
        }
    }

    let resource_indexes = entry
        .fields
        .iter()
        .enumerate()
        .filter(|(_, field)| classify_resource_field(&field.name).is_some())
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    for (index, resource) in resource_indexes.into_iter().zip(entry.resource_fields()) {
        if !literal_or_number(&entry.fields[index].value.value)
            || !matches!(
                resource.kind,
                ResourceKind::Doi
                    | ResourceKind::Pmid
                    | ResourceKind::Pmcid
                    | ResourceKind::Isbn
                    | ResourceKind::Issn
                    | ResourceKind::Arxiv
            )
        {
            continue;
        }
        if let Some(normalized) = resource.normalized
            && normalized != entry.fields[index].value.plain_text()
        {
            replacements.insert(index, normalized);
        }
    }

    for (index, value) in replacements {
        set_field_literal(&mut entry.fields[index], value);
    }
    Ok(())
}

fn normalize_field_aliases_and_legacy_date(entry: &mut ParsedEntry<'_>) {
    let mut names = entry
        .fields
        .iter()
        .map(|field| field.name.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    for (alias, canonical) in [
        ("address", "location"),
        ("journal", "journaltitle"),
        ("school", "institution"),
        ("primaryclass", "eprintclass"),
        ("archiveprefix", "eprinttype"),
    ] {
        if names.contains(canonical) {
            continue;
        }
        if let Some(field) = entry
            .fields
            .iter_mut()
            .find(|field| field.name.eq_ignore_ascii_case(alias))
        {
            field.name = Cow::Borrowed(canonical);
            field.raw = None;
            names.insert(canonical.to_owned());
        }
    }

    if names.contains("date") {
        return;
    }
    let Some(year_index) = entry
        .fields
        .iter()
        .position(|field| field.name.eq_ignore_ascii_case("year"))
    else {
        return;
    };
    if !literal_or_number(&entry.fields[year_index].value.value) {
        return;
    }
    let Some(year) = canonical_date(&entry.fields[year_index].value.plain_text()) else {
        return;
    };
    if year.len() != 4 {
        return;
    }
    let month_index = entry
        .fields
        .iter()
        .position(|field| field.name.eq_ignore_ascii_case("month"));
    let month = match month_index {
        Some(index) => match canonical_month(&entry.fields[index].value.plain_text()) {
            Some(month) => Some(month),
            None => return,
        },
        None => None,
    };
    entry.fields[year_index].name = Cow::Borrowed("date");
    let date = month.map_or(year.clone(), |month| format!("{year}-{month:02}"));
    set_field_literal(&mut entry.fields[year_index], date);
    if let Some(month_index) = month_index {
        let removed = entry.remove_field_by_index(month_index);
        debug_assert!(removed);
    }
}

fn canonical_month(value: &str) -> Option<u8> {
    match value
        .trim()
        .trim_matches(['{', '}', '"'])
        .to_ascii_lowercase()
        .as_str()
    {
        "1" | "01" | "jan" | "january" => Some(1),
        "2" | "02" | "feb" | "february" => Some(2),
        "3" | "03" | "mar" | "march" => Some(3),
        "4" | "04" | "apr" | "april" => Some(4),
        "5" | "05" | "may" => Some(5),
        "6" | "06" | "jun" | "june" => Some(6),
        "7" | "07" | "jul" | "july" => Some(7),
        "8" | "08" | "aug" | "august" => Some(8),
        "9" | "09" | "sep" | "sept" | "september" => Some(9),
        "10" | "oct" | "october" => Some(10),
        "11" | "nov" | "november" => Some(11),
        "12" | "dec" | "december" => Some(12),
        _ => None,
    }
}

fn literal_or_number(value: &Value<'_>) -> bool {
    matches!(value, Value::Literal(_) | Value::Number(_))
}

fn is_creator_field(name: &str) -> bool {
    matches!(
        name,
        "author"
            | "bookauthor"
            | "editor"
            | "editora"
            | "editorb"
            | "editorc"
            | "translator"
            | "commentator"
            | "annotator"
            | "introduction"
            | "foreword"
            | "afterword"
    )
}

fn canonical_creator_list(value: &str) -> Option<String> {
    let names = parse_names(value);
    if names.is_empty() {
        return None;
    }
    Some(
        names
            .into_iter()
            .map(|name| {
                if let Some(literal) = name.literal {
                    return format!("{{{}}}", literal.trim());
                }
                let family = [name.von.trim(), name.last.trim()]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                if family.is_empty() {
                    return name.raw.trim().to_owned();
                }
                match (name.jr.trim(), name.first.trim()) {
                    ("", "") => family,
                    ("", first) => format!("{family}, {first}"),
                    (suffix, "") => format!("{family}, {suffix}"),
                    (suffix, first) => format!("{family}, {suffix}, {first}"),
                }
            })
            .collect::<Vec<_>>()
            .join(" and "),
    )
}

fn is_date_field(name: &str) -> bool {
    matches!(name, "date" | "eventdate" | "origdate" | "urldate")
}

fn canonical_date(value: &str) -> Option<String> {
    let value = value.trim();
    let candidate = if value.contains('/') && !value.contains('-') {
        value.replace('/', "-")
    } else {
        value.to_owned()
    };
    let parts = parse_date_parts(&candidate).ok()?;
    if !(0..=9999).contains(&parts.year) {
        return None;
    }
    let mut output = format!("{:04}", parts.year);
    if let Some(month) = parts.month {
        output.push_str(&format!("-{month:02}"));
    }
    if let Some(day) = parts.day {
        output.push_str(&format!("-{day:02}"));
    }
    Some(output)
}

fn set_field_literal(field: &mut bibtex_parser::ParsedField<'_>, value: String) {
    field.value.value = Value::Literal(Cow::Owned(value));
    field.value.raw = None;
    field.value.expanded = None;
    field.value.delimiter = None;
    field.raw = None;
}

fn normalize_keywords(entry: &mut ParsedEntry<'_>) {
    let Some(value) = entry.get_as_string_ignore_case("keywords") else {
        return;
    };
    let collections = crate::collections::normalize(value.split(','));
    if collections.is_empty() {
        remove_fields_ignore_case(entry, "keywords");
    } else {
        set_literal_field(entry, "keywords", &collections.join(", "));
    }
}

fn copy_with_limit(reader: &mut impl Read, writer: &mut impl Write, limit: u64) -> Result<u64> {
    let mut buffer = [0_u8; 64 * 1024];
    let mut written = 0_u64;
    loop {
        let count = reader.read(&mut buffer).map_err(|source| Error::Read {
            path: PathBuf::from("<attachment stream>"),
            source,
        })?;
        if count == 0 {
            return Ok(written);
        }
        written = written
            .checked_add(count as u64)
            .ok_or(Error::AttachmentTooLarge { limit })?;
        if written > limit {
            return Err(Error::AttachmentTooLarge { limit });
        }
        writer
            .write_all(&buffer[..count])
            .map_err(|source| Error::Write {
                path: PathBuf::from("<attachment stream>"),
                source,
            })?;
    }
}

fn render_preserved_document(document: &ParsedDocument<'_>, path: &Path) -> Result<String> {
    document_to_string(document).map_err(|source| Error::ParseBibliography {
        path: path.to_owned(),
        source,
    })
}

fn validate_output(path: &Path, output: &str) -> Result<()> {
    let document =
        Parser::new()
            .parse_document(output)
            .map_err(|source| Error::ParseBibliography {
                path: path.to_owned(),
                source,
            })?;
    if document.status() == ParseStatus::Ok {
        Ok(())
    } else {
        Err(Error::DegradedBibliography {
            path: path.to_owned(),
            message: "the proposed mutation would produce invalid BibLaTeX".to_owned(),
        })
    }
}

fn render_canonical_document(document: &ParsedDocument<'_>) -> String {
    let selected = (0..document.entries().len()).collect::<HashSet<_>>();
    render_canonical_document_selected(document, &selected)
}

fn render_canonical_document_selected(
    document: &ParsedDocument<'_>,
    selected: &HashSet<usize>,
) -> String {
    let mut blocks = Vec::new();
    for block in document.blocks() {
        let rendered = match *block {
            ParsedBlock::Entry(index) if selected.contains(&index) => {
                render_canonical_entry(&document.entries()[index])
            }
            ParsedBlock::Entry(_) => continue,
            ParsedBlock::String(index) => {
                let value = &document.strings()[index];
                value.raw.as_ref().map_or_else(
                    || {
                        format!(
                            "@string{{{} = {}}}",
                            value.name,
                            value.value.value.to_bibtex_source()
                        )
                    },
                    |raw| raw.trim().to_owned(),
                )
            }
            ParsedBlock::Preamble(index) => {
                let value = &document.preambles()[index];
                value.raw.as_ref().map_or_else(
                    || format!("@preamble{{{}}}", value.value.value.to_bibtex_source()),
                    |raw| raw.trim().to_owned(),
                )
            }
            ParsedBlock::Comment(index) => {
                let comment = &document.comments()[index];
                comment.raw.as_ref().map_or_else(
                    || format!("@comment{{{}}}", comment.text),
                    |raw| raw.trim().to_owned(),
                )
            }
            ParsedBlock::Failed(_) => continue,
        };
        if !rendered.is_empty() {
            blocks.push(rendered);
        }
    }
    let mut output = blocks.join("\n\n");
    if !output.is_empty() {
        output.push('\n');
    }
    output
}

fn render_canonical_entry(entry: &ParsedEntry<'_>) -> String {
    let mut fields = entry.fields.iter().enumerate().collect::<Vec<_>>();
    fields.sort_by_key(|(index, field)| (field_rank(&field.name), *index));
    let mut output = format!("@{}{{{},\n", entry.ty, entry.key());
    for (position, (_, field)) in fields.iter().enumerate() {
        let name = field.name.to_ascii_lowercase();
        let value = if is_raw_field(&name) {
            field
                .value
                .raw_text()
                .map_or_else(|| field.value.value.to_bibtex_source(), str::to_owned)
        } else {
            field.value.value.to_bibtex_source()
        };
        output.push_str("  ");
        output.push_str(&name);
        output.push_str(" = ");
        output.push_str(&value);
        if position + 1 != fields.len() {
            output.push(',');
        }
        output.push('\n');
    }
    output.push('}');
    output
}

fn is_raw_field(name: &str) -> bool {
    matches!(name, "abstract" | "annotation" | "note") || !is_managed_field(name)
}

fn is_managed_field(name: &str) -> bool {
    name == ID_FIELD
        || name.starts_with("zotero-")
        || matches!(
            name,
            "addendum"
                | "address"
                | "afterword"
                | "annotator"
                | "archiveprefix"
                | "author"
                | "bookauthor"
                | "booksubtitle"
                | "booktitle"
                | "booktitleaddon"
                | "chapter"
                | "commentator"
                | "crossref"
                | "date"
                | "doi"
                | "edition"
                | "editor"
                | "editora"
                | "editorb"
                | "editorc"
                | "eid"
                | "eprint"
                | "eprintclass"
                | "eprinttype"
                | "eventdate"
                | "eventtitle"
                | "eventtitleaddon"
                | "file"
                | "foreword"
                | "holder"
                | "howpublished"
                | "ids"
                | "indexsorttitle"
                | "institution"
                | "introduction"
                | "isbn"
                | "isrn"
                | "issn"
                | "issue"
                | "issuetitle"
                | "issuesubtitle"
                | "journalsubtitle"
                | "journaltitle"
                | "journal"
                | "keywords"
                | "label"
                | "langid"
                | "language"
                | "library"
                | "location"
                | "maintitle"
                | "maintitleaddon"
                | "mainsubtitle"
                | "month"
                | "nameaddon"
                | "number"
                | "organization"
                | "origdate"
                | "origlanguage"
                | "origlocation"
                | "origpublisher"
                | "origtitle"
                | "pages"
                | "pagetotal"
                | "part"
                | "pmcid"
                | "pmid"
                | "primaryclass"
                | "publisher"
                | "pubstate"
                | "related"
                | "relatedtype"
                | "school"
                | "series"
                | "shortauthor"
                | "shorteditor"
                | "shortjournal"
                | "shortseries"
                | "shorttitle"
                | "shorthand"
                | "sortkey"
                | "sortname"
                | "sorttitle"
                | "sortyear"
                | "subtitle"
                | "title"
                | "titleaddon"
                | "translator"
                | "type"
                | "url"
                | "urldate"
                | "venue"
                | "version"
                | "volume"
                | "volumes"
                | "xdata"
                | "year"
        )
}

fn field_rank(name: &str) -> usize {
    match name.to_ascii_lowercase().as_str() {
        "author" | "editor" | "translator" => 10,
        "title" | "subtitle" | "titleaddon" => 20,
        "booktitle" | "journaltitle" | "journal" | "series" => 30,
        "date" | "year" | "month" => 40,
        "volume" | "number" | "issue" | "pages" | "chapter" => 50,
        "publisher" | "institution" | "organization" | "location" | "address" => 60,
        "doi" | "isbn" | "issn" | "url" | "urldate" | "eprint" => 70,
        "keywords" => 80,
        "file" => 90,
        ID_FIELD => 1000,
        _ => 500,
    }
}

pub fn is_bibliography_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("bib"))
}

fn validate_new_item(item: &NewItem) -> Result<()> {
    if !valid_name(&item.entry_type) {
        return Err(Error::InvalidEntryType {
            entry_type: item.entry_type.clone(),
        });
    }
    if let Some(key) = &item.citation_key
        && (key.is_empty()
            || key
                .chars()
                .any(|character| character.is_whitespace() || ",{}()=\\\"#%".contains(character)))
    {
        return Err(Error::InvalidCitationKey { key: key.clone() });
    }
    for (name, _) in &item.fields {
        if name.is_empty() {
            return Err(Error::EmptyFieldName);
        }
        if !valid_name(name) {
            return Err(Error::InvalidFieldArgument {
                argument: name.clone(),
            });
        }
        if name.eq_ignore_ascii_case(ID_FIELD) {
            return Err(Error::ReservedField {
                field: name.clone(),
            });
        }
    }
    Ok(())
}

fn valid_name(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}

fn field_value<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn lock_path(bibliography: &Path) -> PathBuf {
    let mut path = bibliography.as_os_str().to_os_string();
    path.push(".lock");
    PathBuf::from(path)
}

fn process_lock(bibliography: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(bibliography).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(bibliography.to_owned(), Arc::downgrade(&lock));
    lock
}

fn open_exclusive_lock(path: &Path) -> Result<fs::File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|source| Error::Lock {
            path: path.to_owned(),
            source,
        })?;
    <fs::File as FileExt>::lock(&file).map_err(|source| Error::Lock {
        path: path.to_owned(),
        source,
    })?;
    Ok(file)
}

fn collect_trash_entries(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<TrashEntry>,
) -> Result<()> {
    for child in fs::read_dir(directory).map_err(|source| Error::Read {
        path: directory.to_owned(),
        source,
    })? {
        let child = child.map_err(|source| Error::Read {
            path: directory.to_owned(),
            source,
        })?;
        let path = child.path();
        let file_type = child.file_type().map_err(|source| Error::Read {
            path: path.clone(),
            source,
        })?;
        if file_type.is_dir() {
            collect_trash_entries(root, &path, entries)?;
        } else {
            let size = child.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            entries.push(TrashEntry {
                path: path.strip_prefix(root).unwrap_or(&path).to_owned(),
                size,
            });
        }
    }
    Ok(())
}

fn collect_managed_files(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for child in fs::read_dir(directory).map_err(|source| Error::Read {
        path: directory.to_owned(),
        source,
    })? {
        let child = child.map_err(|source| Error::Read {
            path: directory.to_owned(),
            source,
        })?;
        let path = child.path();
        if path == root.join(".trash") {
            continue;
        }
        let file_type = child.file_type().map_err(|source| Error::Read {
            path: path.clone(),
            source,
        })?;
        if file_type.is_dir() {
            collect_managed_files(root, &path, files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn prune_empty_parent(parent: Option<&Path>, root: &Path) {
    let Some(parent) = parent else {
        return;
    };
    if parent != root
        && parent.starts_with(root)
        && fs::read_dir(parent).is_ok_and(|mut entries| entries.next().is_none())
    {
        let _ = fs::remove_dir(parent);
    }
}

fn atomic_replace(path: &Path, expected: &[u8], replacement: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| Error::InvalidLibraryPath {
        path: path.to_owned(),
    })?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| Error::Write {
        path: path.to_owned(),
        source,
    })?;

    if let Ok(metadata) = fs::metadata(path) {
        temporary
            .as_file()
            .set_permissions(metadata.permissions())
            .map_err(|source| Error::Write {
                path: path.to_owned(),
                source,
            })?;
    }
    temporary
        .write_all(replacement)
        .and_then(|()| temporary.as_file_mut().sync_all())
        .map_err(|source| Error::Write {
            path: path.to_owned(),
            source,
        })?;

    let current = fs::read(path).map_err(|source| Error::Read {
        path: path.to_owned(),
        source,
    })?;
    if current != expected {
        return Err(Error::SourceChanged {
            path: path.to_owned(),
        });
    }

    temporary.persist(path).map_err(|error| Error::Write {
        path: path.to_owned(),
        source: error.error,
    })?;
    sync_parent(parent, path)
}

#[cfg(unix)]
fn sync_parent(parent: &Path, path: &Path) -> Result<()> {
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| Error::Write {
            path: path.to_owned(),
            source,
        })
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path, _path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[derive(Debug, Eq, PartialEq)]
    struct SemanticItem {
        uuid: Option<Uuid>,
        citation_key: String,
        entry_type: String,
        fields: BTreeMap<String, String>,
        collections: Vec<String>,
        attachments: Vec<Attachment>,
    }

    fn semantic_items(bibliography: &Path, source: &str) -> Vec<SemanticItem> {
        Catalog::parse(bibliography, source)
            .unwrap()
            .items()
            .map(|item| {
                let mut fields = item
                    .fields
                    .into_iter()
                    .filter(|field| {
                        !field.name.eq_ignore_ascii_case(ID_FIELD)
                            && !field.name.eq_ignore_ascii_case("keywords")
                            && !field.name.eq_ignore_ascii_case("file")
                    })
                    .map(|field| (field.name.to_ascii_lowercase(), field.value))
                    .collect::<BTreeMap<_, _>>();
                for (alias, canonical) in [
                    ("address", "location"),
                    ("journal", "journaltitle"),
                    ("school", "institution"),
                    ("primaryclass", "eprintclass"),
                    ("archiveprefix", "eprinttype"),
                ] {
                    if !fields.contains_key(canonical)
                        && let Some(value) = fields.remove(alias)
                    {
                        fields.insert(canonical.to_owned(), value);
                    }
                }
                if !fields.contains_key("date")
                    && let Some(year) = fields.remove("year")
                {
                    let date = fields
                        .remove("month")
                        .and_then(|month| canonical_month(&month))
                        .map_or(year.clone(), |month| format!("{year}-{month:02}"));
                    fields.insert("date".to_owned(), date);
                }
                SemanticItem {
                    uuid: item.uuid,
                    citation_key: item.citation_key,
                    entry_type: item.entry_type,
                    fields,
                    collections: item.collections,
                    attachments: item.attachments,
                }
            })
            .collect()
    }

    fn new_article() -> NewItem {
        NewItem {
            entry_type: "article".to_owned(),
            citation_key: None,
            fields: vec![
                ("author".to_owned(), "Lovelace, Ada".to_owned()),
                ("date".to_owned(), "1843".to_owned()),
                (
                    "title".to_owned(),
                    "A Sketch of the Analytical Engine".to_owned(),
                ),
            ],
        }
    }

    #[test]
    fn attachment_directory_is_adjacent_to_bibliography() {
        let layout = LibraryLayout::new(PathBuf::from("/tmp/my.references.bib")).unwrap();
        assert_eq!(
            layout.attachments,
            PathBuf::from("/tmp/my.references.files")
        );
    }

    #[test]
    fn configured_attachment_root_outside_library_uses_safe_absolute_references() {
        let library_directory = tempfile::tempdir().unwrap();
        let attachment_directory = tempfile::tempdir().unwrap();
        let bibliography = library_directory.path().join("references.bib");
        let root = attachment_directory.path().join("managed");
        let source_file = library_directory.path().join("paper.pdf");
        fs::write(&source_file, b"PDF").unwrap();
        let layout = LibraryLayout::with_attachments(bibliography.clone(), root.clone()).unwrap();
        layout.initialize().unwrap();
        let store = LibraryStore::new(layout);
        let item = store.add_item(new_article()).unwrap();

        let attached = store
            .attach_file(&item.uuid.to_string(), &source_file, None, None, 1024)
            .unwrap();

        assert!(Path::new(&attached.path).is_absolute());
        assert!(Path::new(&attached.path).starts_with(&root));
        assert_eq!(store.check().unwrap().warnings, 0);
        store
            .detach_attachment(&item.uuid.to_string(), attached.attachment_uuid)
            .unwrap();
        assert_eq!(store.trash_entries().unwrap().len(), 1);
    }

    #[test]
    fn initialize_creates_layout_without_truncating_existing_library() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        fs::write(&bibliography, "@book{existing}\n").unwrap();
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();

        layout.initialize().unwrap();

        assert_eq!(
            fs::read_to_string(bibliography).unwrap(),
            "@book{existing}\n"
        );
        assert!(layout.attachments.is_dir());
        assert!(layout.attachments.join(".trash").is_dir());
    }

    #[test]
    fn add_generates_identity_and_preserves_existing_raw_source() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let original = concat!(
            "% retained comment\n",
            "@string{suffix = {value}}\n",
            "@book{existing,\n",
            "  title = {Existing},\n",
            "  abstract = \"raw \" # suffix,\n",
            "  lantaiid = {cc9e50c4-55ee-4471-b17c-c41684f64bf9}\n",
            "}\n"
        );
        fs::write(&bibliography, original).unwrap();
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();
        layout.initialize().unwrap();

        let added = LibraryStore::new(layout).add_item(new_article()).unwrap();
        let output = fs::read_to_string(bibliography).unwrap();

        assert_eq!(added.citation_key, "Lov43");
        assert!(output.contains("% retained comment"));
        assert!(output.contains("abstract = \"raw \" # suffix"));
        assert!(output.contains(&format!("lantaiid = {{{}}}", added.uuid)));
    }

    #[test]
    fn add_uses_collision_suffix_and_rejects_explicit_duplicate() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let layout = LibraryLayout::new(bibliography).unwrap();
        layout.initialize().unwrap();
        let store = LibraryStore::new(layout);

        assert_eq!(store.add_item(new_article()).unwrap().citation_key, "Lov43");
        assert_eq!(
            store.add_item(new_article()).unwrap().citation_key,
            "Lov43a"
        );

        let mut duplicate = new_article();
        duplicate.citation_key = Some("Lov43".to_owned());
        assert!(matches!(
            store.add_item(duplicate),
            Err(Error::DuplicateCitationKey { .. })
        ));
    }

    #[test]
    fn biblatex_import_preserves_source_and_assigns_missing_identities_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        fs::write(
            &bibliography,
            "% existing\n@book{existing, title={Existing}, lantaiid={cc9e50c4-55ee-4471-b17c-c41684f64bf9}}\n",
        )
        .unwrap();
        let store = LibraryStore::new(LibraryLayout::new(bibliography.clone()).unwrap());
        let imported = concat!(
            "% imported comment\n",
            "@string{suffix = {expression}}\n",
            "@article{first, title={First}, abstract=\"raw \" # suffix}\n\n",
            "@online{second, title={Second}, custom = foo # {bar}}\n"
        );

        let added = store.import_biblatex(imported).unwrap();
        let output = fs::read_to_string(&bibliography).unwrap();

        assert_eq!(added.len(), 2);
        assert_eq!(added[0].citation_key, "first");
        assert_eq!(added[1].citation_key, "second");
        assert!(output.contains("% existing"));
        assert!(output.contains("% imported comment"));
        assert!(output.contains("@string{suffix = {expression}}"));
        assert!(output.contains("abstract=\"raw \" # suffix"));
        assert!(output.contains("custom = foo # {bar}"));
        assert!(
            added
                .iter()
                .all(|item| output.contains(&item.uuid.to_string()))
        );

        let before_duplicate = output;
        assert!(matches!(
            store.import_biblatex("@misc{first, title={Duplicate}}"),
            Err(Error::DuplicateCitationKey { .. })
        ));
        assert_eq!(fs::read_to_string(bibliography).unwrap(), before_duplicate);
    }

    #[test]
    fn malformed_library_is_never_rewritten() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let malformed = "@book{broken, title = {unterminated";
        fs::write(&bibliography, malformed).unwrap();
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();

        assert!(LibraryStore::new(layout).add_item(new_article()).is_err());
        assert_eq!(fs::read_to_string(bibliography).unwrap(), malformed);
    }

    #[test]
    fn set_adopts_external_entry_and_preserves_unrelated_raw_field() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let original = concat!(
            "% keep me\n",
            "@book{external,\n",
            "  title = {Old title},\n",
            "  abstract = \"raw \" # {expression}\n",
            "}\n"
        );
        fs::write(&bibliography, original).unwrap();
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();
        let result = LibraryStore::new(layout)
            .set_fields(
                "external",
                &[("title".to_owned(), "New title".to_owned())],
                Some("renamed"),
            )
            .unwrap();
        let output = fs::read_to_string(&bibliography).unwrap();

        assert_eq!(result.citation_key, "renamed");
        assert!(output.contains("% keep me"));
        assert!(output.contains("abstract = \"raw \" # {expression}"));
        assert!(output.contains("title = {New title}"));
        assert!(output.contains(&result.uuid.to_string()));
        let catalog = crate::catalog::Catalog::parse(&bibliography, &output).unwrap();
        assert_eq!(
            catalog.find(&result.uuid.to_string()).unwrap().citation_key,
            "renamed"
        );
    }

    #[test]
    fn set_raw_preserves_exact_replacement_and_added_expression() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        fs::write(
            &bibliography,
            concat!(
                "@misc{raw,\n",
                "  title = {Keep},\n",
                "  abstract = {old},\n",
                "  lantaiid = {cc9e50c4-55ee-4471-b17c-c41684f64bf9}\n",
                "}\n"
            ),
        )
        .unwrap();
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();

        LibraryStore::new(layout)
            .set_raw_fields(
                "raw",
                &[
                    ("abstract".to_owned(), "\"new \" # {expression}".to_owned()),
                    ("custom".to_owned(), "foo # \"bar\"".to_owned()),
                ],
            )
            .unwrap();
        let output = fs::read_to_string(bibliography).unwrap();

        assert!(output.contains("abstract = \"new \" # {expression}"));
        assert!(output.contains("custom = foo # \"bar\""));
        assert!(output.contains("title = {Keep}"));
    }

    #[test]
    fn unset_and_collections_share_the_safe_mutation_path() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        fs::write(
            &bibliography,
            concat!(
                "@misc{tagged,\n",
                "  title = {Remove me},\n",
                "  keywords = {zeta, Alpha},\n",
                "  lantaiid = {cc9e50c4-55ee-4471-b17c-c41684f64bf9}\n",
                "}\n"
            ),
        )
        .unwrap();
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();
        let store = LibraryStore::new(layout);

        store
            .add_collections("tagged", &["beta".to_owned(), "Alpha".to_owned()])
            .unwrap();
        store
            .remove_collections("tagged", &["ZETA".to_owned()])
            .unwrap();
        store.unset_fields("tagged", &["title".to_owned()]).unwrap();
        let output = fs::read_to_string(bibliography).unwrap();

        assert!(output.contains("keywords = {Alpha, beta}"));
        assert!(!output.contains("title ="));
    }

    #[test]
    fn collection_rebase_retains_translator_and_external_names() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        fs::write(
            &bibliography,
            concat!(
                "@misc{tagged,\n",
                "  keywords = {automatic, previous, external},\n",
                "  lantaiid = {cc9e50c4-55ee-4471-b17c-c41684f64bf9}\n",
                "}\n"
            ),
        )
        .unwrap();
        let store = LibraryStore::new(LibraryLayout::new(bibliography.clone()).unwrap());

        store
            .rebase_collections(
                "tagged",
                &["previous".to_owned()],
                &["replacement".to_owned()],
            )
            .unwrap();

        let output = fs::read_to_string(bibliography).unwrap();
        assert!(output.contains("keywords = {automatic, external, replacement}"));
    }

    #[test]
    fn patch_applies_raw_literal_unset_collections_and_key_in_one_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        fs::write(
            &bibliography,
            concat!(
                "% keep this comment\n",
                "@misc{before,\n",
                "  title = {Old title},\n",
                "  note = {remove me},\n",
                "  abstract = {old},\n",
                "  lantaiid = {cc9e50c4-55ee-4471-b17c-c41684f64bf9}\n",
                "}\n"
            ),
        )
        .unwrap();
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();
        let store = LibraryStore::new(layout);

        let result = store
            .patch_item(
                "before",
                ItemPatch {
                    set: BTreeMap::from([("title".to_owned(), "New title".to_owned())]),
                    set_raw: BTreeMap::from([(
                        "abstract".to_owned(),
                        "\"new \" # {expression}".to_owned(),
                    )]),
                    unset: vec!["note".to_owned()],
                    collections: Some(vec!["zeta".to_owned(), "Alpha".to_owned()]),
                    citation_key: Some("after".to_owned()),
                },
            )
            .unwrap();
        let output = fs::read_to_string(&bibliography).unwrap();

        assert_eq!(result.citation_key, "after");
        assert!(output.contains("% keep this comment"));
        assert!(output.contains("title = {New title}"));
        assert!(output.contains("abstract = \"new \" # {expression}"));
        assert!(output.contains("keywords = {Alpha, zeta}"));
        assert!(!output.contains("note ="));
        assert!(output.contains("@misc{after,"));
    }

    #[test]
    fn detach_moves_managed_file_to_trash_and_purge_removes_it() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let source_file = directory.path().join("paper.pdf");
        fs::write(&source_file, b"PDF").unwrap();
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();
        layout.initialize().unwrap();
        let store = LibraryStore::new(layout);
        let item = store.add_item(new_article()).unwrap();
        let attached = store
            .attach_file(&item.uuid.to_string(), &source_file, None, None, 1024)
            .unwrap();
        let managed_path = directory.path().join(&attached.path);

        let detached = store
            .detach_attachment(&item.uuid.to_string(), attached.attachment_uuid)
            .unwrap();

        assert!(!managed_path.exists());
        assert!(detached.trashed_to.as_ref().unwrap().is_file());
        assert_eq!(store.trash_entries().unwrap().len(), 1);
        assert!(
            !fs::read_to_string(&bibliography)
                .unwrap()
                .contains("file =")
        );
        assert_eq!(store.purge_trash().unwrap(), 1);
        assert!(store.trash_entries().unwrap().is_empty());
    }

    #[test]
    fn format_is_idempotent_and_preserves_raw_values() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        fs::write(
            &bibliography,
            concat!(
                "% retained\n",
                "@misc{external, title={Title}, custom = \"a \" # {b}, keywords={z, A, z}}\n"
            ),
        )
        .unwrap();
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();
        let store = LibraryStore::new(layout);

        let first = store.format().unwrap();
        let once = fs::read_to_string(&bibliography).unwrap();
        let second = store.format().unwrap();
        let twice = fs::read_to_string(bibliography).unwrap();

        assert!(first.changed);
        assert_eq!(first.assigned_ids, 1);
        assert!(!second.changed);
        assert_eq!(second.assigned_ids, 0);
        assert_eq!(once, twice);
        assert!(once.contains("% retained"));
        assert!(once.contains("custom = \"a \" # {b}"));
        assert!(once.contains("keywords = {A, z}"));
        assert!(once.contains("lantaiid = {"));
    }

    #[test]
    fn format_canonicalizes_creators_dates_identifiers_and_attachment_references() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        fs::write(
            &bibliography,
            concat!(
                "@article{external,\n",
                "  author = {Ludwig van Beethoven and {Research and Development Group}},\n",
                "  date = {2026/7/3},\n",
                "  doi = {https://doi.org/10.5555/ABC.},\n",
                "  isbn = {978-1-4028-9462-6},\n",
                "  issn = {2049-3630},\n",
                "  pmcid = {1234},\n",
                "  file = { Paper :references.files/item/cc9e50c4-55ee-4471-b17c-c41684f64bf9-paper.pdf:Application/PDF },\n",
                "  abstract = \"raw \" # {expression},\n",
                "  custom = \"a \" # {b},\n",
                "  lantaiid = {5a45466b-d74f-4072-b026-dad615c7dcec}\n",
                "}\n",
                "@article{legacy,\n",
                "  journal = {Legacy Journal},\n",
                "  address = {New York},\n",
                "  school = {Example University},\n",
                "  year = 2024,\n",
                "  month = jul,\n",
                "  lantaiid = {2b5aa0af-87e6-4f43-838a-703ef36b11a2}\n",
                "}\n"
            ),
        )
        .unwrap();
        let store = LibraryStore::new(LibraryLayout::new(bibliography.clone()).unwrap());

        let first = store.format().unwrap();
        let once = fs::read_to_string(&bibliography).unwrap();
        let second = store.format().unwrap();
        let twice = fs::read_to_string(&bibliography).unwrap();

        assert!(first.changed);
        assert!(!second.changed);
        assert_eq!(once, twice);
        assert!(
            once.contains("author = {van Beethoven, Ludwig and {Research and Development Group}}")
        );
        assert!(once.contains("date = {2026-07-03}"));
        assert!(once.contains("doi = {10.5555/abc}"));
        assert!(once.contains("isbn = {9781402894626}"));
        assert!(once.contains("issn = {20493630}"));
        assert!(once.contains("pmcid = {PMC1234}"));
        assert!(once.contains(concat!(
            "file = {Paper:references.files/item/",
            "cc9e50c4-55ee-4471-b17c-c41684f64bf9-paper.pdf:application/pdf}"
        )));
        assert!(once.contains("abstract = \"raw \" # {expression}"));
        assert!(once.contains("custom = \"a \" # {b}"));
        assert!(once.contains("journaltitle = {Legacy Journal}"));
        assert!(once.contains("location = {New York}"));
        assert!(once.contains("institution = {Example University}"));
        assert!(once.contains("date = {2024-07}"));
        assert!(!once.contains("journal ="));
        assert!(!once.contains("year ="));
        assert!(!once.contains("month ="));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn formatting_is_idempotent_and_semantically_stable_for_generated_documents(
            entries in prop::collection::vec(
                (
                    any::<u128>(),
                    "[A-Za-z0-9 ]{1,40}",
                    1000_u16..3000,
                    prop::collection::btree_set("[a-z]{1,8}", 0..5),
                    "[a-z]{1,12}",
                ),
                1..8,
            )
        ) {
            let directory = tempfile::tempdir().unwrap();
            let bibliography = directory.path().join("references.bib");
            let mut source = String::from(
                "% generated fixture\n@string{generated = {Generated}}\n@preamble{{fixture}}\n",
            );
            for (index, (uuid, title, year, tags, raw_suffix)) in entries.into_iter().enumerate() {
                let tags = tags.into_iter().collect::<Vec<_>>().join(", ");
                source.push_str(&format!(
                    concat!(
                        "@misc{{item{index},",
                        "title=generated # {{{title}}},",
                        "year={{{year}}},",
                        "keywords={{{tags}}},",
                        "abstract=\"raw \" # {{{raw_suffix}}},",
                        "lantaiid={{{uuid}}}}}\n"
                    ),
                    index = index,
                    title = title,
                    year = year,
                    tags = tags,
                    raw_suffix = raw_suffix,
                    uuid = Uuid::from_u128(uuid),
                ));
            }
            fs::write(&bibliography, &source).unwrap();
            let before = semantic_items(&bibliography, &source);
            let store = LibraryStore::new(LibraryLayout::new(bibliography.clone()).unwrap());

            store.format().unwrap();
            let once = fs::read_to_string(&bibliography).unwrap();
            prop_assert_eq!(semantic_items(&bibliography, &once), before);
            let second = store.format().unwrap();
            let twice = fs::read_to_string(&bibliography).unwrap();

            prop_assert!(!second.changed);
            prop_assert_eq!(once, twice);
        }
    }

    #[test]
    fn export_is_canonical_and_can_filter_entries_without_losing_support_blocks() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        fs::write(
            &bibliography,
            concat!(
                "% retained\n",
                "@string{suffix={value}}\n",
                "@misc{first,title={First},custom=\"a \" # suffix,lantaiid={cc9e50c4-55ee-4471-b17c-c41684f64bf9}}\n",
                "@misc{second,title={Second},lantaiid={5a45466b-d74f-4072-b026-dad615c7dcec}}\n"
            ),
        )
        .unwrap();
        let store = LibraryStore::new(LibraryLayout::new(bibliography).unwrap());

        let selected = store.export_biblatex(&["first".to_owned()]).unwrap();

        assert!(selected.contains("% retained"));
        assert!(selected.contains("@string{suffix={value}}"));
        assert!(selected.contains("@misc{first,"));
        assert!(selected.contains("custom = \"a \" # suffix"));
        assert!(!selected.contains("@misc{second,"));
        assert_eq!(
            selected,
            store
                .export_biblatex_from(&selected, &["first".to_owned()])
                .unwrap()
        );
    }

    #[test]
    fn attach_copies_file_and_updates_catalog_after_copy_completes() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let source_file = directory.path().join("paper:final?.pdf");
        fs::write(&source_file, b"PDF bytes").unwrap();
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();
        layout.initialize().unwrap();
        let store = LibraryStore::new(layout.clone());
        let item = store.add_item(new_article()).unwrap();

        let attached = store
            .attach_file(
                &item.uuid.to_string(),
                &source_file,
                Some("Paper: final"),
                Some("application/pdf"),
                1024,
            )
            .unwrap();

        assert_eq!(attached.size, 9);
        let stored = directory.path().join(&attached.path);
        assert_eq!(fs::read(stored).unwrap(), b"PDF bytes");
        assert!(attached.path.contains("paper_final_.pdf"));
        let contents = fs::read_to_string(&bibliography).unwrap();
        let catalog = crate::catalog::Catalog::parse(&bibliography, &contents).unwrap();
        let catalog_item = catalog.find(&item.uuid.to_string()).unwrap();
        assert_eq!(catalog_item.attachments.len(), 1);
        assert_eq!(
            catalog_item.attachments[0].uuid,
            Some(attached.attachment_uuid)
        );
        assert_eq!(catalog_item.attachments[0].title, "Paper: final");
    }

    #[test]
    fn attachment_limit_failure_leaves_catalog_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let source_file = directory.path().join("large.pdf");
        fs::write(&source_file, b"too large").unwrap();
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();
        layout.initialize().unwrap();
        let store = LibraryStore::new(layout);
        let item = store.add_item(new_article()).unwrap();

        assert!(matches!(
            store.attach_file(&item.uuid.to_string(), &source_file, None, None, 2),
            Err(Error::AttachmentTooLarge { .. })
        ));
        assert!(!fs::read_to_string(bibliography).unwrap().contains("file ="));
        assert!(
            fs::read_dir(store.layout.attachments.join(item.uuid.to_string()))
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true)
        );
    }

    #[test]
    fn removing_item_trashes_managed_files_but_never_external_references() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let managed_source = directory.path().join("managed.pdf");
        let external = directory.path().join("external.pdf");
        fs::write(&managed_source, b"managed").unwrap();
        fs::write(&external, b"external").unwrap();
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();
        layout.initialize().unwrap();
        let store = LibraryStore::new(layout);
        let item = store.add_item(new_article()).unwrap();
        store
            .attach_file(&item.uuid.to_string(), &managed_source, None, None, 1024)
            .unwrap();
        let contents = fs::read_to_string(&bibliography).unwrap();
        let catalog = crate::catalog::Catalog::parse(&bibliography, &contents).unwrap();
        let mut attachments = catalog.find(&item.uuid.to_string()).unwrap().attachments;
        attachments.push(Attachment {
            uuid: None,
            title: "External".to_owned(),
            path: external.to_string_lossy().into_owned(),
            media_type: "application/pdf".to_owned(),
        });
        store
            .set_fields(
                &item.uuid.to_string(),
                &[("file".to_owned(), encode_file_field(&attachments))],
                None,
            )
            .unwrap();

        store.remove_item(&item.uuid.to_string()).unwrap();

        assert!(external.is_file());
        assert_eq!(store.trash_entries().unwrap().len(), 1);
        assert!(
            !fs::read_to_string(bibliography)
                .unwrap()
                .contains("@article")
        );
    }

    #[test]
    fn check_reports_missing_and_orphan_managed_attachments() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let source_file = directory.path().join("source.pdf");
        fs::write(&source_file, b"content").unwrap();
        let layout = LibraryLayout::new(bibliography).unwrap();
        layout.initialize().unwrap();
        let store = LibraryStore::new(layout.clone());
        let item = store.add_item(new_article()).unwrap();
        let attached = store
            .attach_file(&item.uuid.to_string(), &source_file, None, None, 1024)
            .unwrap();
        let attached_path = directory.path().join(attached.path);
        fs::remove_file(&attached_path).unwrap();
        let orphan = layout
            .attachments
            .join(item.uuid.to_string())
            .join("orphan.pdf");
        fs::write(&orphan, b"orphan").unwrap();
        let interrupted = layout
            .attachments
            .join(item.uuid.to_string())
            .join(".tmp-interrupted");
        fs::write(&interrupted, b"partial").unwrap();

        let report = store.check().unwrap();
        let codes = report
            .issues
            .iter()
            .map(|issue| issue.code.as_str())
            .collect::<HashSet<_>>();

        assert!(codes.contains("missing-attachment"));
        assert!(codes.contains("orphan-managed-attachment"));
        assert!(codes.contains("stale-temporary-attachment"));
        assert_eq!(report.status, CheckStatus::Degraded);
    }

    #[test]
    fn concurrent_lantai_writers_are_serialized_without_lost_updates() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();
        layout.initialize().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(12));
        let mut threads = Vec::new();
        for index in 0..12 {
            let barrier = barrier.clone();
            let layout = layout.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                LibraryStore::new(layout)
                    .add_item(NewItem {
                        entry_type: "misc".to_owned(),
                        citation_key: Some(format!("item{index}")),
                        fields: vec![("title".to_owned(), format!("Item {index}"))],
                    })
                    .unwrap()
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }

        let source = fs::read_to_string(&bibliography).unwrap();
        let catalog = Catalog::parse(&bibliography, &source).unwrap();
        assert_eq!(catalog.items().count(), 12);
        assert_eq!(LibraryStore::new(layout).check().unwrap().errors, 0);
    }

    #[test]
    fn stale_expected_hash_never_replaces_a_racing_external_edit() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        fs::write(&bibliography, b"original\n").unwrap();
        let expected = fs::read(&bibliography).unwrap();
        fs::write(&bibliography, b"external edit\n").unwrap();

        assert!(matches!(
            atomic_replace(&bibliography, &expected, b"lantai replacement\n"),
            Err(Error::SourceChanged { .. })
        ));
        assert_eq!(fs::read(&bibliography).unwrap(), b"external edit\n");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
