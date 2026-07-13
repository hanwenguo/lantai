use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::catalog::{Catalog, CatalogItem, CheckReport, ItemSummary};
use crate::client::{ApiClient, ApiHealth};
use crate::config::{
    Config, DEFAULT_ATTACHMENT_LIMIT_BYTES, absolutize, default_config_path, resolve_library,
};
use crate::library::{
    AddedItem, AttachedFile, DetachedFile, FormatResult, ItemPatch, LibraryLayout, LibraryStore,
    MutationResult, NewItem, RemovedItem, TrashEntry,
};
use crate::{Error, Result};

#[derive(Debug, Parser)]
#[command(
    name = "lantai",
    version,
    about = "A BibLaTeX-backed headless reference manager"
)]
pub struct Cli {
    /// Use this bibliography instead of LANTAI_LIBRARY or the configured path.
    #[arg(long, global = true, value_name = "PATH")]
    library: Option<PathBuf>,

    /// Emit stable machine-readable output.
    #[arg(long, global = true)]
    json: bool,

    /// Override the platform configuration path (primarily for testing).
    #[arg(long, global = true, value_name = "PATH", hide = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create or adopt a library and write its configuration.
    Init {
        /// Replace an existing configuration file. The bibliography is never truncated.
        #[arg(long)]
        force: bool,

        /// Store managed files here instead of in the adjacent <stem>.files directory.
        #[arg(long, value_name = "PATH")]
        attachments: Option<PathBuf>,
    },

    /// Report whether the configured library and attachment directory are accessible.
    Health,

    /// Run the authenticated local REST API.
    Serve,

    /// List bibliography entries.
    List {
        /// Match citation key, title, or field text.
        query: Option<String>,

        /// Include only this entry type.
        #[arg(long = "type")]
        entry_type: Option<String>,

        /// Include only entries with this tag.
        #[arg(long)]
        tag: Option<String>,
    },

    /// Add a bibliography entry.
    Add {
        /// BibLaTeX entry type, such as article, book, or online.
        #[arg(
            long = "type",
            required_unless_present = "from",
            conflicts_with = "from"
        )]
        entry_type: Option<String>,

        /// Set a field as NAME=VALUE. May be repeated.
        #[arg(long = "field", value_name = "NAME=VALUE", conflicts_with = "from")]
        fields: Vec<String>,

        /// Use this citation key instead of generating AuthorYearTitle.
        #[arg(long, conflicts_with = "from")]
        key: Option<String>,

        /// Import one or more BibLaTeX entries from a file, or use - for standard input.
        #[arg(long, value_name = "FILE")]
        from: Option<String>,
    },

    /// Show one entry by UUID or unambiguous citation key.
    Show { id: String },

    /// Set literal fields and optionally rename the citation key.
    Set {
        id: String,

        /// Set a field as NAME=VALUE. May be repeated.
        #[arg(value_name = "NAME=VALUE", required_unless_present = "key")]
        fields: Vec<String>,

        /// Rename the citation key.
        #[arg(long)]
        key: Option<String>,
    },

    /// Set exact BibTeX value expressions without normalizing their syntax.
    SetRaw {
        id: String,

        /// Set a field as NAME=EXPRESSION. May be repeated.
        #[arg(value_name = "NAME=EXPRESSION", required = true)]
        fields: Vec<String>,
    },

    /// Remove fields from an entry.
    Unset {
        id: String,

        #[arg(required = true)]
        fields: Vec<String>,
    },

    /// Add or remove item tags.
    Tag {
        #[command(subcommand)]
        action: TagAction,
    },

    /// Remove an entry and move its managed attachments to trash.
    Remove { id: String },

    /// Detach a managed attachment and move its file to trash.
    Detach {
        id: String,
        attachment_id: uuid::Uuid,
    },

    /// Inspect or empty the managed attachment trash.
    Trash {
        #[command(subcommand)]
        action: TrashAction,
    },

    /// Copy a file into the managed attachment store.
    Attach {
        id: String,
        file: PathBuf,

        /// Attachment display title; defaults to the source filename.
        #[arg(long)]
        title: Option<String>,

        /// Media type; inferred from the filename when omitted.
        #[arg(long = "mime")]
        media_type: Option<String>,
    },

    /// Export the canonical bibliography or selected entries.
    Export {
        /// UUIDs or citation keys to include; omit to export the complete library.
        ids: Vec<String>,

        /// Write to this file instead of standard output. Use - for standard output.
        #[arg(short, long, value_name = "PATH")]
        output: Option<PathBuf>,
    },

    /// Canonicalize managed syntax and assign missing stable IDs.
    Format,

    /// Diagnose the bibliography without changing it.
    Check,
}

#[derive(Debug, Subcommand)]
enum TagAction {
    /// Add one or more tags.
    Add {
        id: String,
        #[arg(required = true)]
        tags: Vec<String>,
    },

    /// Remove one or more tags, matching case-insensitively.
    Remove {
        id: String,
        #[arg(required = true)]
        tags: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum TrashAction {
    /// List trashed files.
    List,
    /// Permanently delete every trashed file.
    Purge,
}

#[derive(Serialize)]
struct InitOutput<'a> {
    status: &'a str,
    library: &'a std::path::Path,
    attachments: &'a std::path::Path,
    config: &'a std::path::Path,
}

#[derive(Serialize)]
struct HealthOutput<'a> {
    status: &'a str,
    library: &'a std::path::Path,
    attachments: &'a std::path::Path,
    entries: usize,
    warnings: usize,
    errors: usize,
}

struct Backend {
    layout: LibraryLayout,
    attachment_limit_bytes: u64,
    mode: BackendMode,
}

enum BackendMode {
    Daemon(ApiClient),
    Direct(LibraryStore),
}

impl Backend {
    fn load(library: Option<&std::path::Path>, config_path: &std::path::Path) -> Result<Self> {
        let library = resolve_library(library, config_path)?;
        let config = config_path
            .is_file()
            .then(|| Config::load(config_path))
            .transpose()?;
        let layout = configured_layout(library.clone(), config.as_ref())?;
        let attachment_limit_bytes = config
            .as_ref()
            .map_or(DEFAULT_ATTACHMENT_LIMIT_BYTES, |config| {
                config.attachment_limit_bytes
            });
        let daemon = if let Some(config) = config.as_ref() {
            let configured_library = absolutize(&config.library)?;
            if configured_library == library {
                ApiClient::connect(config)?
            } else {
                None
            }
        } else {
            None
        };
        let mode = daemon.map_or_else(
            || BackendMode::Direct(LibraryStore::new(layout.clone())),
            BackendMode::Daemon,
        );
        Ok(Self {
            layout,
            attachment_limit_bytes,
            mode,
        })
    }

    fn health(&mut self) -> Result<ApiHealth> {
        match &mut self.mode {
            BackendMode::Daemon(client) => client.health(),
            BackendMode::Direct(store) => {
                let report = store.check()?;
                Ok(ApiHealth {
                    status: if report.errors == 0 { "ok" } else { "degraded" }.to_owned(),
                    revision: String::new(),
                    entries: report.entries,
                    warnings: report.warnings,
                    errors: report.errors,
                    disk_error: None,
                })
            }
        }
    }

    fn list(
        &mut self,
        query: Option<&str>,
        entry_type: Option<&str>,
        tag: Option<&str>,
    ) -> Result<Vec<ItemSummary>> {
        match &mut self.mode {
            BackendMode::Daemon(client) => client.list(query, entry_type, tag),
            BackendMode::Direct(_) => {
                let contents = self.layout.read_utf8()?;
                let catalog = Catalog::parse(&self.layout.bibliography, &contents)?;
                Ok(catalog
                    .items()
                    .filter(|item| matches_item(item, query, entry_type, tag))
                    .map(ItemSummary::from)
                    .collect())
            }
        }
    }

    fn add(&mut self, item: NewItem) -> Result<AddedItem> {
        match &mut self.mode {
            BackendMode::Daemon(client) => {
                client.create_item(&item.entry_type, item.citation_key.as_deref(), &item.fields)
            }
            BackendMode::Direct(store) => store.add_item(item),
        }
    }

    fn import(&mut self, source: &str) -> Result<Vec<AddedItem>> {
        match &mut self.mode {
            BackendMode::Daemon(client) => client.import_biblatex(source),
            BackendMode::Direct(store) => store.import_biblatex(source),
        }
    }

    fn get(&mut self, id: &str) -> Result<CatalogItem> {
        match &mut self.mode {
            BackendMode::Daemon(client) => client.get_item(id),
            BackendMode::Direct(_) => {
                let contents = self.layout.read_utf8()?;
                Catalog::parse(&self.layout.bibliography, &contents)?.find(id)
            }
        }
    }

    fn patch(&mut self, id: &str, patch: ItemPatch) -> Result<MutationResult> {
        match &mut self.mode {
            BackendMode::Daemon(client) => client.patch_item(id, &patch),
            BackendMode::Direct(store) => store.patch_item(id, patch),
        }
    }

    fn change_tags(&mut self, id: &str, tags: &[String], add: bool) -> Result<MutationResult> {
        match &mut self.mode {
            BackendMode::Daemon(client) => {
                let item = client.get_item(id)?;
                let mut updated = item.tags;
                if add {
                    for tag in tags {
                        if !updated
                            .iter()
                            .any(|candidate| candidate.eq_ignore_ascii_case(tag))
                        {
                            updated.push(tag.clone());
                        }
                    }
                } else {
                    updated.retain(|candidate| {
                        !tags.iter().any(|tag| candidate.eq_ignore_ascii_case(tag))
                    });
                }
                client.patch_item(
                    id,
                    &ItemPatch {
                        tags: Some(updated),
                        ..ItemPatch::default()
                    },
                )
            }
            BackendMode::Direct(store) if add => store.add_tags(id, tags),
            BackendMode::Direct(store) => store.remove_tags(id, tags),
        }
    }

    fn remove(&mut self, id: &str) -> Result<RemovedItem> {
        match &mut self.mode {
            BackendMode::Daemon(client) => {
                let item = client.get_item(id)?;
                client.delete_item(id)?;
                Ok(RemovedItem {
                    uuid: item.uuid,
                    citation_key: item.citation_key,
                })
            }
            BackendMode::Direct(store) => store.remove_item(id),
        }
    }

    fn attach(
        &mut self,
        id: &str,
        file: &std::path::Path,
        title: Option<&str>,
        media_type: Option<&str>,
    ) -> Result<AttachedFile> {
        match &mut self.mode {
            BackendMode::Daemon(client) => client.attach_file(id, file, title, media_type),
            BackendMode::Direct(store) => {
                store.attach_file(id, file, title, media_type, self.attachment_limit_bytes)
            }
        }
    }

    fn detach(&mut self, id: &str, attachment_id: uuid::Uuid) -> Result<DetachedFile> {
        match &mut self.mode {
            BackendMode::Daemon(client) => client.detach_attachment(id, attachment_id),
            BackendMode::Direct(store) => store.detach_attachment(id, attachment_id),
        }
    }

    fn trash_entries(&mut self) -> Result<Vec<TrashEntry>> {
        match &mut self.mode {
            BackendMode::Daemon(client) => client.trash_entries(),
            BackendMode::Direct(store) => store.trash_entries(),
        }
    }

    fn purge_trash(&mut self) -> Result<usize> {
        match &mut self.mode {
            BackendMode::Daemon(client) => client.purge_trash(),
            BackendMode::Direct(store) => store.purge_trash(),
        }
    }

    fn export(&self, ids: &[String]) -> Result<String> {
        match &self.mode {
            BackendMode::Daemon(client) => client.export(ids),
            BackendMode::Direct(store) => store.export_biblatex(ids),
        }
    }

    fn format(&mut self) -> Result<FormatResult> {
        match &mut self.mode {
            BackendMode::Daemon(client) => client.format(),
            BackendMode::Direct(store) => store.format(),
        }
    }

    fn check(&self) -> Result<CheckReport> {
        match &self.mode {
            BackendMode::Daemon(client) => client.check(),
            BackendMode::Direct(store) => store.check(),
        }
    }
}

pub fn run() -> Result<()> {
    run_cli(Cli::parse())
}

fn run_cli(cli: Cli) -> Result<()> {
    let config_path = match cli.config {
        Some(path) => absolutize(&path)?,
        None => default_config_path()?,
    };

    match cli.command {
        Command::Init { force, attachments } => {
            init(cli.library, attachments, cli.json, &config_path, force)
        }
        Command::Health => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            if !backend.layout.attachments.is_dir() {
                return Err(Error::LibraryNotFile {
                    path: backend.layout.attachments,
                });
            }
            let health = backend.health()?;
            let output = HealthOutput {
                status: &health.status,
                library: &backend.layout.bibliography,
                attachments: &backend.layout.attachments,
                entries: health.entries,
                warnings: health.warnings,
                errors: health.errors,
            };
            if cli.json {
                print_json(&output)
            } else {
                println!(
                    "{}: {}",
                    health.status,
                    backend.layout.bibliography.display()
                );
                Ok(())
            }
        }
        Command::Serve => {
            let mut config = Config::load(&config_path)?;
            let configured_library = absolutize(&config.library)?;
            let library = resolve_library(cli.library.as_deref(), &config_path)?;
            if configured_library != library {
                config.attachment_root = None;
            }
            config.library = library.clone();
            let layout = configured_layout(library, Some(&config))?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|source| Error::Listen {
                    address: config.api_address.clone(),
                    source,
                })?;
            runtime.block_on(crate::server::serve(config, layout))
        }
        Command::List {
            query,
            entry_type,
            tag,
        } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let summaries =
                backend.list(query.as_deref(), entry_type.as_deref(), tag.as_deref())?;
            if cli.json {
                print_json(&summaries)
            } else {
                for item in summaries {
                    println!(
                        "{}\t{}\t{}",
                        item.citation_key,
                        item.entry_type,
                        item.title.as_deref().unwrap_or("")
                    );
                }
                Ok(())
            }
        }
        Command::Add {
            entry_type,
            fields,
            key,
            from,
        } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            if let Some(from) = from {
                let source = read_import_source(&from)?;
                let added = backend.import(&source)?;
                if cli.json {
                    return print_json(&added);
                }
                for item in added {
                    println!("Added {} ({})", item.citation_key, item.uuid);
                }
                return Ok(());
            }
            let fields = fields
                .into_iter()
                .map(parse_field_argument)
                .collect::<Result<Vec<_>>>()?;
            let added = backend.add(NewItem {
                entry_type: entry_type.expect("clap requires --type unless --from is present"),
                citation_key: key,
                fields,
            })?;
            if cli.json {
                print_json(&added)
            } else {
                println!("Added {} ({})", added.citation_key, added.uuid);
                Ok(())
            }
        }
        Command::Show { id } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let item = backend.get(&id)?;
            if cli.json {
                print_json(&item)
            } else {
                print_item(&item);
                Ok(())
            }
        }
        Command::Set { id, fields, key } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let fields = fields
                .into_iter()
                .map(parse_field_argument)
                .collect::<Result<Vec<_>>>()?;
            let result = backend.patch(
                &id,
                ItemPatch {
                    set: fields_to_map(fields)?,
                    citation_key: key,
                    ..ItemPatch::default()
                },
            )?;
            print_mutation_result("Updated", &result.citation_key, result.uuid, cli.json)
        }
        Command::SetRaw { id, fields } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let fields = fields
                .into_iter()
                .map(parse_field_argument)
                .collect::<Result<Vec<_>>>()?;
            let result = backend.patch(
                &id,
                ItemPatch {
                    set_raw: fields_to_map(fields)?,
                    ..ItemPatch::default()
                },
            )?;
            print_mutation_result("Updated", &result.citation_key, result.uuid, cli.json)
        }
        Command::Unset { id, fields } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let result = backend.patch(
                &id,
                ItemPatch {
                    unset: fields,
                    ..ItemPatch::default()
                },
            )?;
            print_mutation_result("Updated", &result.citation_key, result.uuid, cli.json)
        }
        Command::Tag { action } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let result = match action {
                TagAction::Add { id, tags } => backend.change_tags(&id, &tags, true)?,
                TagAction::Remove { id, tags } => backend.change_tags(&id, &tags, false)?,
            };
            print_mutation_result("Updated", &result.citation_key, result.uuid, cli.json)
        }
        Command::Remove { id } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let result = backend.remove(&id)?;
            if cli.json {
                print_json(&result)
            } else {
                println!("Removed {}", result.citation_key);
                Ok(())
            }
        }
        Command::Detach { id, attachment_id } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let result = backend.detach(&id, attachment_id)?;
            if cli.json {
                print_json(&result)
            } else {
                println!(
                    "Detached {} from {}",
                    result.attachment_uuid, result.citation_key
                );
                Ok(())
            }
        }
        Command::Trash { action } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            match action {
                TrashAction::List => {
                    let entries = backend.trash_entries()?;
                    if cli.json {
                        print_json(&entries)
                    } else {
                        for entry in entries {
                            println!("{}\t{}", entry.size, entry.path.display());
                        }
                        Ok(())
                    }
                }
                TrashAction::Purge => {
                    let purged = backend.purge_trash()?;
                    if cli.json {
                        print_json(&serde_json::json!({ "purged": purged }))
                    } else {
                        println!("Purged {purged} trashed files");
                        Ok(())
                    }
                }
            }
        }
        Command::Attach {
            id,
            file,
            title,
            media_type,
        } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let result = backend.attach(&id, &file, title.as_deref(), media_type.as_deref())?;
            if cli.json {
                print_json(&result)
            } else {
                println!(
                    "Attached {} to {} ({})",
                    result.title, result.citation_key, result.attachment_uuid
                );
                Ok(())
            }
        }
        Command::Export { ids, output } => {
            let backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let contents = backend.export(&ids)?;
            write_export(output.as_deref(), &contents)
        }
        Command::Format => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let result = backend.format()?;
            if cli.json {
                print_json(&result)
            } else {
                println!(
                    "Formatted library ({} missing IDs assigned)",
                    result.assigned_ids
                );
                Ok(())
            }
        }
        Command::Check => {
            let backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let report = backend.check()?;
            if cli.json {
                print_json(&report)?;
            } else {
                print_check(&report);
            }
            if report.errors > 0 {
                Err(Error::CheckFailed {
                    errors: report.errors,
                })
            } else {
                Ok(())
            }
        }
    }
}

fn init(
    library: Option<PathBuf>,
    attachments: Option<PathBuf>,
    json: bool,
    config_path: &std::path::Path,
    force: bool,
) -> Result<()> {
    if config_path.exists() && !force {
        return Err(Error::ConfigAlreadyExists {
            path: config_path.to_owned(),
        });
    }
    let library = library.ok_or(Error::LibraryNotConfigured)?;
    let library = absolutize(&library)?;
    let attachments = attachments.map(|path| absolutize(&path)).transpose()?;
    let layout = match attachments.clone() {
        Some(attachments) => LibraryLayout::with_attachments(library.clone(), attachments)?,
        None => LibraryLayout::new(library.clone())?,
    };
    layout.initialize()?;

    let mut config = Config::new(library);
    config.attachment_root = attachments;
    config.write(config_path, force)?;

    let output = InitOutput {
        status: "initialized",
        library: &layout.bibliography,
        attachments: &layout.attachments,
        config: config_path,
    };
    if json {
        print_json(&output)
    } else {
        println!("Initialized {}", layout.bibliography.display());
        println!("Attachments: {}", layout.attachments.display());
        println!("Config: {}", config_path.display());
        Ok(())
    }
}

fn configured_layout(library: PathBuf, config: Option<&Config>) -> Result<LibraryLayout> {
    let attachments = config
        .filter(|config| absolutize(&config.library).is_ok_and(|path| path == library))
        .and_then(|config| config.attachment_root.as_deref())
        .map(absolutize)
        .transpose()?;
    match attachments {
        Some(attachments) => LibraryLayout::with_attachments(library, attachments),
        None => LibraryLayout::new(library),
    }
}

fn matches_item(
    item: &CatalogItem,
    query: Option<&str>,
    entry_type: Option<&str>,
    tag: Option<&str>,
) -> bool {
    if entry_type.is_some_and(|entry_type| !item.entry_type.eq_ignore_ascii_case(entry_type)) {
        return false;
    }
    if tag.is_some_and(|tag| {
        !item
            .tags
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(tag))
    }) {
        return false;
    }
    query.is_none_or(|query| {
        let query = query.to_lowercase();
        item.citation_key.to_lowercase().contains(&query)
            || item
                .fields
                .iter()
                .any(|field| field.value.to_lowercase().contains(&query))
    })
}

fn parse_field_argument(argument: String) -> Result<(String, String)> {
    let Some((name, value)) = argument.split_once('=') else {
        return Err(Error::InvalidFieldArgument { argument });
    };
    if name.is_empty() {
        return Err(Error::EmptyFieldName);
    }
    Ok((name.to_owned(), value.to_owned()))
}

fn read_import_source(from: &str) -> Result<String> {
    if from == "-" {
        let mut source = String::new();
        io::stdin()
            .read_to_string(&mut source)
            .map_err(|source| Error::Read {
                path: PathBuf::from("<stdin>"),
                source,
            })?;
        Ok(source)
    } else {
        std::fs::read_to_string(from).map_err(|source| Error::Read {
            path: PathBuf::from(from),
            source,
        })
    }
}

fn write_export(path: Option<&std::path::Path>, contents: &str) -> Result<()> {
    if path.is_none_or(|path| path == std::path::Path::new("-")) {
        print!("{contents}");
        io::stdout().flush().map_err(|source| Error::Write {
            path: PathBuf::from("<stdout>"),
            source,
        })
    } else {
        let path = path.expect("non-stdout export has a path");
        std::fs::write(path, contents).map_err(|source| Error::Write {
            path: path.to_owned(),
            source,
        })
    }
}

fn fields_to_map(fields: Vec<(String, String)>) -> Result<BTreeMap<String, String>> {
    let mut mapped = BTreeMap::new();
    for (name, value) in fields {
        if mapped
            .keys()
            .any(|candidate: &String| candidate.eq_ignore_ascii_case(&name))
        {
            return Err(Error::DuplicateField { field: name });
        }
        mapped.insert(name, value);
    }
    Ok(mapped)
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn print_mutation_result(
    action: &str,
    citation_key: &str,
    uuid: uuid::Uuid,
    json: bool,
) -> Result<()> {
    if json {
        print_json(&serde_json::json!({
            "uuid": uuid,
            "citation_key": citation_key,
        }))
    } else {
        println!("{action} {citation_key} ({uuid})");
        Ok(())
    }
}

fn print_item(item: &CatalogItem) {
    println!("@{}{{{}}}", item.entry_type, item.citation_key);
    if let Some(uuid) = item.uuid {
        println!("UUID: {uuid}");
    }
    for field in &item.fields {
        println!("{}: {}", field.name, field.value);
    }
}

fn print_check(report: &CheckReport) {
    println!(
        "{} entries, {} warnings, {} errors",
        report.entries, report.warnings, report.errors
    );
    for issue in &report.issues {
        let location = match (issue.line, issue.column) {
            (Some(line), Some(column)) => format!(" at {line}:{column}"),
            _ => String::new(),
        };
        println!(
            "{:?} [{}]{}: {}",
            issue.severity, issue.code, location, issue.message
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backend_uses_daemon_then_falls_back_to_the_same_library() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let attachment = directory.path().join("paper.pdf");
        std::fs::write(&attachment, b"PDF bytes").unwrap();
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();
        layout.initialize().unwrap();
        let config_path = directory.path().join("config.toml");
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = probe.local_addr().unwrap();
        drop(probe);
        let mut config = Config::new(bibliography.clone());
        config.api_address = address.to_string();
        config.write(&config_path, false).unwrap();

        let listener = tokio::net::TcpListener::bind(address).await.unwrap();
        let state = crate::server::AppState::new(config.clone(), layout.clone()).unwrap();
        let app = crate::server::native_router(state);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut reachable = false;
        for _ in 0..40 {
            let attempt = config.clone();
            reachable = tokio::task::spawn_blocking(move || {
                ApiClient::connect(&attempt).unwrap().is_some()
            })
            .await
            .unwrap();
            if reachable {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(reachable, "test daemon did not become reachable");

        let wrong_config_path = directory.path().join("wrong-token.toml");
        let mut wrong_config = config.clone();
        wrong_config.api_token = "wrong-token".to_owned();
        wrong_config.write(&wrong_config_path, false).unwrap();
        let auth_error = tokio::task::spawn_blocking(move || {
            Backend::load(None, &wrong_config_path).err().unwrap()
        })
        .await
        .unwrap();
        assert!(matches!(auth_error, Error::Api { status: 401, .. }));

        let daemon_config_path = config_path.clone();
        let daemon_attachment = attachment.clone();
        let (item_uuid, attachment_uuid) = tokio::task::spawn_blocking(move || {
            let mut backend = Backend::load(None, &daemon_config_path).unwrap();
            assert!(matches!(backend.mode, BackendMode::Daemon(_)));
            let added = backend
                .add(NewItem {
                    entry_type: "article".to_owned(),
                    citation_key: Some("parity".to_owned()),
                    fields: vec![("title".to_owned(), "Before".to_owned())],
                })
                .unwrap();
            backend
                .change_tags(&added.uuid.to_string(), &["remote".to_owned()], true)
                .unwrap();
            let attached = backend
                .attach(
                    &added.uuid.to_string(),
                    &daemon_attachment,
                    Some("Paper"),
                    Some("application/pdf"),
                )
                .unwrap();
            backend
                .patch(
                    &added.uuid.to_string(),
                    ItemPatch {
                        set: BTreeMap::from([("title".to_owned(), "After".to_owned())]),
                        ..ItemPatch::default()
                    },
                )
                .unwrap();
            (added.uuid, attached.attachment_uuid)
        })
        .await
        .unwrap();

        server.abort();
        let _ = server.await;

        let mut direct = Backend::load(None, &config_path).unwrap();
        assert!(matches!(direct.mode, BackendMode::Direct(_)));
        let item = direct.get(&item_uuid.to_string()).unwrap();
        assert_eq!(
            item.fields
                .iter()
                .find(|field| field.name.eq_ignore_ascii_case("title"))
                .map(|field| field.value.as_str()),
            Some("After")
        );
        assert_eq!(item.tags, vec!["remote"]);
        assert_eq!(item.attachments[0].uuid, Some(attachment_uuid));
        direct
            .patch(
                &item_uuid.to_string(),
                ItemPatch {
                    set_raw: BTreeMap::from([(
                        "abstract".to_owned(),
                        "\"direct \" # {fallback}".to_owned(),
                    )]),
                    ..ItemPatch::default()
                },
            )
            .unwrap();
        assert!(
            std::fs::read_to_string(bibliography)
                .unwrap()
                .contains("abstract = \"direct \" # {fallback}")
        );
    }

    #[test]
    fn cli_initializes_custom_root_imports_and_exports_a_selection() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let attachments = directory.path().join("custom-files");
        let config_path = directory.path().join("config.toml");
        let import_path = directory.path().join("import.bib");
        let export_path = directory.path().join("selected.bib");
        std::fs::write(
            &import_path,
            "@misc{first, title={First}}\n@misc{second, title={Second}}\n",
        )
        .unwrap();

        run_cli(
            Cli::try_parse_from([
                "lantai",
                "--config",
                config_path.to_str().unwrap(),
                "--library",
                bibliography.to_str().unwrap(),
                "init",
                "--attachments",
                attachments.to_str().unwrap(),
            ])
            .unwrap(),
        )
        .unwrap();
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let unavailable = probe.local_addr().unwrap();
        drop(probe);
        let mut config = Config::load(&config_path).unwrap();
        config.api_address = unavailable.to_string();
        config.write(&config_path, true).unwrap();

        run_cli(
            Cli::try_parse_from([
                "lantai",
                "--config",
                config_path.to_str().unwrap(),
                "--library",
                bibliography.to_str().unwrap(),
                "add",
                "--from",
                import_path.to_str().unwrap(),
            ])
            .unwrap(),
        )
        .unwrap();
        run_cli(
            Cli::try_parse_from([
                "lantai",
                "--config",
                config_path.to_str().unwrap(),
                "--library",
                bibliography.to_str().unwrap(),
                "export",
                "first",
                "--output",
                export_path.to_str().unwrap(),
            ])
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            Config::load(&config_path).unwrap().attachment_root,
            Some(attachments)
        );
        let exported = std::fs::read_to_string(export_path).unwrap();
        assert!(exported.contains("@misc{first,"));
        assert!(!exported.contains("@misc{second,"));
    }
}
