use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, ExitStatus, Stdio};

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use directories::BaseDirs;
use serde::Serialize;

use crate::catalog::{
    Catalog, CatalogItem, CheckIssue, CheckReport, CheckStatus, IssueSeverity, ItemView,
};
use crate::client::ApiClient;
use crate::collections;
use crate::config::{
    Config, DEFAULT_ATTACHMENT_LIMIT_BYTES, LIBRARY_ENV, absolutize, default_config_path,
    resolve_library,
};
use crate::hook::{HookItems, HookOperation, HookOrigin, PostSaveHook};
use crate::library::{
    AddedItem, AttachedFile, DetachedFile, FormatResult, ItemPatch, LibraryLayout, LibraryStore,
    MutationResult, NewItem, RemovedItem, TrashEntry,
};
use crate::zotero::map_item;
use crate::zotero_rdf::{RdfImport, SkippedAttachment};
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

        #[command(flatten)]
        output: JsonOutput,
    },

    /// Run the authenticated local REST API.
    Serve,

    /// List bibliography entries.
    List {
        /// Match citation key, title, or field text.
        query: Option<String>,

        /// Include only entries in this collection.
        #[arg(long)]
        collection: Option<String>,

        /// Select JSON or the legacy tab-separated display (defaults to JSON).
        #[arg(long, value_enum)]
        format: Option<OutputFormat>,
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

        #[command(flatten)]
        output: JsonOutput,
    },

    /// Import a Zotero RDF export, including its files and collections.
    Import {
        /// The exported .rdf document. Attachment files are read beside it.
        file: PathBuf,

        /// Resolve linked files against Zotero's attachment base directory.
        #[arg(long, value_name = "PATH")]
        attachment_base: Option<PathBuf>,

        /// Report what would be imported without changing the library.
        #[arg(long)]
        dry_run: bool,

        #[command(flatten)]
        output: JsonOutput,
    },

    /// Show one entry by UUID or unambiguous citation key.
    Show {
        id: String,

        /// Select JSON or the legacy field-oriented display (defaults to JSON).
        #[arg(long, value_enum)]
        format: Option<OutputFormat>,
    },

    /// Set literal fields and optionally rename the citation key.
    Set {
        id: String,

        /// Set a field as NAME=VALUE. May be repeated.
        #[arg(value_name = "NAME=VALUE", required_unless_present = "key")]
        fields: Vec<String>,

        /// Rename the citation key.
        #[arg(long)]
        key: Option<String>,

        #[command(flatten)]
        output: JsonOutput,
    },

    /// Set exact BibTeX value expressions without normalizing their syntax.
    SetRaw {
        id: String,

        /// Set a field as NAME=EXPRESSION. May be repeated.
        #[arg(value_name = "NAME=EXPRESSION", required = true)]
        fields: Vec<String>,

        #[command(flatten)]
        output: JsonOutput,
    },

    /// Remove fields from an entry.
    Unset {
        id: String,

        #[arg(required = true)]
        fields: Vec<String>,

        #[command(flatten)]
        output: JsonOutput,
    },

    /// List the library's collections, or change an item's membership.
    Collection {
        #[command(subcommand)]
        action: CollectionAction,

        #[command(flatten)]
        output: JsonOutput,
    },

    /// Remove an entry and move its managed attachments to trash.
    Remove {
        id: String,

        #[command(flatten)]
        output: JsonOutput,
    },

    /// Detach a managed attachment and move its file to trash.
    Detach {
        id: String,
        attachment_id: uuid::Uuid,

        #[command(flatten)]
        output: JsonOutput,
    },

    /// Inspect or empty the managed attachment trash.
    Trash {
        #[command(subcommand)]
        action: TrashAction,

        #[command(flatten)]
        output: JsonOutput,
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

        #[command(flatten)]
        output: JsonOutput,
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
    Format {
        #[command(flatten)]
        output: JsonOutput,
    },

    /// Diagnose the bibliography without changing it.
    Check {
        #[command(flatten)]
        output: JsonOutput,
    },

    #[command(external_subcommand)]
    External(Vec<OsString>),
}

#[derive(Debug, Subcommand)]
enum CollectionAction {
    /// List every collection in the library.
    List,

    /// Add the item to one or more collections.
    Add {
        id: String,
        #[arg(required = true, value_name = "COLLECTION")]
        collections: Vec<String>,
    },

    /// Remove the item from one or more collections, matching case-insensitively.
    Remove {
        id: String,
        #[arg(required = true, value_name = "COLLECTION")]
        collections: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum TrashAction {
    /// List trashed files.
    List,
    /// Permanently delete every trashed file.
    Purge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    Json,
    Human,
}

#[derive(Debug, Args)]
struct JsonOutput {
    /// Emit JSON instead of the human-readable display.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Serialize)]
struct InitOutput<'a> {
    status: &'a str,
    library: &'a std::path::Path,
    attachments: &'a std::path::Path,
    config: &'a std::path::Path,
}

/// `check` reports the selected paths alongside the diagnostic report, so one
/// command answers both "which library am I talking to" and "is it intact".
///
/// The report is flattened rather than copied field by field, so anything added
/// to `CheckReport` reaches this output too.
#[derive(Serialize)]
struct CheckOutput<'a> {
    library: &'a std::path::Path,
    attachments: &'a std::path::Path,
    #[serde(flatten)]
    report: &'a CheckReport,
}

struct Backend {
    layout: LibraryLayout,
    attachment_limit_bytes: u64,
    mode: BackendMode,
    hook: PostSaveHook,
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
        let hook = PostSaveHook::new(
            config
                .as_ref()
                .and_then(|config| config.post_save_hook.as_ref()),
            config_path,
            layout.clone(),
        );
        Ok(Self {
            layout,
            attachment_limit_bytes,
            mode,
            hook,
        })
    }

    fn before_save(&self) -> Result<Option<String>> {
        match self.mode {
            BackendMode::Direct(_) => self.hook.revision_before_save(),
            BackendMode::Daemon(_) => Ok(None),
        }
    }

    fn after_save(
        &self,
        before: Option<String>,
        operation: HookOperation,
        affected: HookItems,
        removed: Vec<RemovedItem>,
    ) {
        if matches!(self.mode, BackendMode::Direct(_)) {
            self.hook
                .emit(before, operation, HookOrigin::Cli, affected, removed);
        }
    }

    fn list(&mut self, query: Option<&str>, collection: Option<&str>) -> Result<Vec<ItemView>> {
        match &mut self.mode {
            BackendMode::Daemon(client) => client.list(query, collection),
            BackendMode::Direct(_) => {
                let contents = self.layout.read_utf8()?;
                let catalog = Catalog::parse(&self.layout.bibliography, &contents)?;
                Ok(catalog
                    .items()
                    .filter(|item| matches_item(item, query, collection))
                    .map(ItemView::from)
                    .collect())
            }
        }
    }

    /// Every collection in the library, deduped and ordered.
    fn collections(&mut self) -> Result<BTreeSet<String>> {
        Ok(collections::of_items(
            self.list(None, None)?
                .into_iter()
                .map(|item| item.collections),
        ))
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

    fn get(&mut self, id: &str) -> Result<ItemView> {
        match &mut self.mode {
            BackendMode::Daemon(client) => client.get_item(id),
            BackendMode::Direct(_) => {
                let contents = self.layout.read_utf8()?;
                Catalog::parse(&self.layout.bibliography, &contents)?
                    .find(id)
                    .map(ItemView::from)
            }
        }
    }

    fn patch(&mut self, id: &str, patch: ItemPatch) -> Result<MutationResult> {
        match &mut self.mode {
            BackendMode::Daemon(client) => client.patch_item(id, &patch),
            BackendMode::Direct(store) => store.patch_item(id, patch),
        }
    }

    fn change_collections(
        &mut self,
        id: &str,
        changed: &[String],
        add: bool,
    ) -> Result<MutationResult> {
        // Normalizing first is what keeps this arm's result identical to the
        // direct arm, which normalizes inside the store.
        let changed = collections::normalize(changed.iter().map(String::as_str));
        match &mut self.mode {
            BackendMode::Daemon(client) => {
                let item = client.get_item(id)?;
                let mut updated = item.collections;
                if add {
                    updated.extend(changed.iter().cloned());
                    updated = collections::normalize(updated.iter().map(String::as_str));
                } else {
                    updated.retain(|candidate| {
                        !changed
                            .iter()
                            .any(|collection| candidate.eq_ignore_ascii_case(collection))
                    });
                }
                client.patch_item(
                    id,
                    &ItemPatch {
                        collections: Some(updated),
                        ..ItemPatch::default()
                    },
                )
            }
            BackendMode::Direct(store) if add => store.add_collections(id, &changed),
            BackendMode::Direct(store) => store.remove_collections(id, &changed),
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

    fn check(&mut self) -> Result<CheckReport> {
        match &mut self.mode {
            BackendMode::Daemon(client) => {
                let mut report = client.check()?;
                // `/api/v1/check` recomputes from disk, so it cannot see the
                // daemon's own trouble: after a failed write or a dead watcher
                // the process serves a stale revision and refuses mutations
                // while the library on disk still looks perfectly fine.
                if let Some(disk_error) = client.health()?.disk_error {
                    report.issues.push(CheckIssue {
                        severity: IssueSeverity::Error,
                        code: "daemon-disk-error".to_owned(),
                        message: format!("the running daemon reported: {disk_error}"),
                        citation_key: None,
                        line: None,
                        column: None,
                    });
                    report.errors += 1;
                    report.status = CheckStatus::Degraded;
                }
                Ok(report)
            }
            BackendMode::Direct(store) => store.check(),
        }
    }
}

/// The built-in commands, grouped for `--help`.
///
/// clap renders one flat list, which for eighteen commands tells a newcomer
/// nothing. Only the names live here; each description is read back from the
/// parser, and a test keeps the two in step.
/// A custom command is an executable named `lantai-NAME` on `PATH`; both the
/// dispatcher and the help listing derive the spelling from here.
const EXTENSION_PREFIX: &str = "lantai-";

const COMMAND_GROUPS: &[(&str, &[&str])] = &[
    ("Setup and status", &["init", "serve", "check"]),
    ("Search", &["list", "show", "collection"]),
    (
        "Edit",
        &["add", "import", "set", "set-raw", "unset", "remove"],
    ),
    ("Attachments", &["attach", "detach", "trash"]),
    ("Output", &["export", "format"]),
];

pub fn run() -> Result<i32> {
    // Hiding every built-in leaves clap's own command list empty, so the
    // grouped listing below is the only one; doing it here rather than with
    // eighteen attributes means a new subcommand cannot be half-hidden.
    let mut command = Cli::command().mut_subcommands(|subcommand| subcommand.hide(true));
    if wants_help(env::args_os().skip(1)) {
        let help = grouped_help(&command);
        command = command.after_help(help);
    }
    let matches = command.get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    run_parsed(cli)
}

/// Whether this invocation is going to print the *top-level* help.
///
/// Discovering extensions reads every `PATH` directory, so it is worth doing
/// only when the listing is about to be displayed. The grouped help hangs off
/// the root command, so a subcommand's own `--help` never shows it — stopping
/// at the first subcommand keeps `lantai list --help`, the form this very help
/// text recommends, from paying for a scan nobody sees.
fn wants_help(arguments: impl IntoIterator<Item = OsString>) -> bool {
    let mut arguments = arguments.into_iter().peekable();
    if arguments.peek().is_none() {
        return true;
    }
    let mut expects_value = false;
    for argument in arguments {
        if std::mem::take(&mut expects_value) {
            continue;
        }
        match argument.to_str() {
            Some("-h" | "--help" | "help") => return true,
            // Global options precede the subcommand and take a value.
            Some("--library" | "--config") => expects_value = true,
            // Anything else is the subcommand (or its arguments), whose help
            // clap renders on its own.
            Some(value) if !value.starts_with('-') => return false,
            _ => {}
        }
    }
    false
}

fn grouped_help(command: &clap::Command) -> String {
    let describe = |name: &str| {
        command
            .get_subcommands()
            .find(|subcommand| subcommand.get_name() == name)
            .and_then(clap::Command::get_about)
            .map(ToString::to_string)
            .unwrap_or_default()
    };
    let builtins = COMMAND_GROUPS
        .iter()
        .flat_map(|(_, names)| names.iter().copied())
        .collect::<BTreeSet<_>>();
    let extensions = discover_extensions(env::var_os("PATH").as_deref(), &builtins);
    let width = builtins
        .iter()
        .map(|name| name.len())
        .chain(extensions.keys().map(String::len))
        .max()
        .unwrap_or(0);

    // Borrow clap's own styles so these sections are indistinguishable from the
    // ones it renders itself. clap prints help through anstream, which drops
    // the escapes again when stdout is not a terminal.
    let styles = command.get_styles();
    let header = styles.get_header();
    let literal = styles.get_literal();
    // Pad separately: a width specifier would count the escape bytes as
    // characters and stagger the descriptions.
    let pad = |name: &str| " ".repeat(width.saturating_sub(name.chars().count()));

    let mut help = String::new();
    for (heading, names) in COMMAND_GROUPS {
        let _ = writeln!(help, "{header}{heading}:{header:#}");
        for name in *names {
            let _ = writeln!(
                help,
                "  {literal}{name}{literal:#}{}  {}",
                pad(name),
                describe(name).trim_end_matches('.')
            );
        }
        help.push('\n');
    }

    let _ = writeln!(help, "{header}Custom commands:{header:#}");
    if extensions.is_empty() {
        help.push_str("  none on PATH; any executable named lantai-NAME becomes `lantai NAME`\n");
    } else {
        for (name, executable) in &extensions {
            let _ = writeln!(
                help,
                "  {literal}{name}{literal:#}{}  {}",
                pad(name),
                executable.display()
            );
        }
    }
    help.push_str("\nRun `lantai COMMAND --help` for a command's own options.");
    help
}

/// Git-style custom commands reachable on `PATH`.
///
/// The first `lantai-NAME` on `PATH` wins, matching the executable that would
/// actually run. A built-in of the same name always wins, so listing one here
/// would be a lie. Unreadable directories are skipped: help must never fail.
fn discover_extensions(
    path: Option<&OsStr>,
    builtins: &BTreeSet<&str>,
) -> BTreeMap<String, PathBuf> {
    let mut found = BTreeMap::new();
    let Some(path) = path else {
        return found;
    };
    for directory in env::split_paths(path) {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name
                .to_str()
                .and_then(|name| name.strip_prefix(EXTENSION_PREFIX))
            else {
                continue;
            };
            if name.is_empty() || builtins.contains(name) {
                continue;
            }
            // Follow symlinks: installing an extension by linking it into a
            // PATH directory is normal, and dispatch resolves the link too, so
            // inspecting the link itself would hide a command that works.
            if !std::fs::metadata(entry.path()).is_ok_and(|metadata| is_executable(&metadata)) {
                continue;
            }
            found.entry(name.to_owned()).or_insert_with(|| entry.path());
        }
    }
    found
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    metadata.is_file()
}

fn run_parsed(cli: Cli) -> Result<i32> {
    if let Command::External(arguments) = &cli.command {
        return run_extension(cli.library.as_deref(), cli.config.as_deref(), arguments);
    }
    run_cli(cli)?;
    Ok(0)
}

fn run_cli(cli: Cli) -> Result<()> {
    let config_path = match cli.config {
        Some(path) => absolutize(&path)?,
        None => default_config_path()?,
    };

    match cli.command {
        Command::Init {
            force,
            attachments,
            output,
        } => init(cli.library, attachments, output.json, &config_path, force),
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
            runtime.block_on(crate::server::serve(config, layout, config_path))
        }
        Command::List {
            query,
            collection,
            format,
        } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let summaries = backend.list(query.as_deref(), collection.as_deref())?;
            if item_output_is_json(format) {
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
            output,
        } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            if let Some(from) = from {
                let source = read_import_source(&from)?;
                let before = backend.before_save()?;
                let added = backend.import(&source)?;
                backend.after_save(
                    before,
                    HookOperation::ItemImport,
                    HookItems::Uuids(added.iter().map(|item| item.uuid).collect()),
                    Vec::new(),
                );
                if output.json {
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
            let before = backend.before_save()?;
            let added = backend.add(NewItem {
                entry_type: entry_type.expect("clap requires --type unless --from is present"),
                citation_key: key,
                fields,
            })?;
            backend.after_save(
                before,
                HookOperation::ItemCreate,
                HookItems::Uuids(vec![added.uuid]),
                Vec::new(),
            );
            if output.json {
                print_json(&added)
            } else {
                println!("Added {} ({})", added.citation_key, added.uuid);
                Ok(())
            }
        }
        Command::Import {
            file,
            attachment_base,
            dry_run,
            output,
        } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let source = std::fs::read_to_string(&file).map_err(|source| Error::Read {
                path: file.clone(),
                source,
            })?;
            let import = crate::zotero_rdf::parse(&file, &source, attachment_base.as_deref())?;
            let summary = if dry_run {
                ImportSummary::preview(&import)
            } else {
                run_import(&mut backend, import)?
            };
            if output.json {
                print_json(&summary)
            } else {
                print_import(&summary, dry_run);
                Ok(())
            }
        }
        Command::Show { id, format } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let item = backend.get(&id)?;
            if item_output_is_json(format) {
                print_json(&item)
            } else {
                print_item(&item);
                Ok(())
            }
        }
        Command::Set {
            id,
            fields,
            key,
            output,
        } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let fields = fields
                .into_iter()
                .map(parse_field_argument)
                .collect::<Result<Vec<_>>>()?;
            let before = backend.before_save()?;
            let result = backend.patch(
                &id,
                ItemPatch {
                    set: fields_to_map(fields)?,
                    citation_key: key,
                    ..ItemPatch::default()
                },
            )?;
            backend.after_save(
                before,
                HookOperation::ItemUpdate,
                HookItems::Uuids(vec![result.uuid]),
                Vec::new(),
            );
            print_mutation_result("Updated", &result.citation_key, result.uuid, output.json)
        }
        Command::SetRaw { id, fields, output } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let fields = fields
                .into_iter()
                .map(parse_field_argument)
                .collect::<Result<Vec<_>>>()?;
            let before = backend.before_save()?;
            let result = backend.patch(
                &id,
                ItemPatch {
                    set_raw: fields_to_map(fields)?,
                    ..ItemPatch::default()
                },
            )?;
            backend.after_save(
                before,
                HookOperation::ItemUpdate,
                HookItems::Uuids(vec![result.uuid]),
                Vec::new(),
            );
            print_mutation_result("Updated", &result.citation_key, result.uuid, output.json)
        }
        Command::Unset { id, fields, output } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let before = backend.before_save()?;
            let result = backend.patch(
                &id,
                ItemPatch {
                    unset: fields,
                    ..ItemPatch::default()
                },
            )?;
            backend.after_save(
                before,
                HookOperation::ItemUpdate,
                HookItems::Uuids(vec![result.uuid]),
                Vec::new(),
            );
            print_mutation_result("Updated", &result.citation_key, result.uuid, output.json)
        }
        Command::Collection { action, output } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let (id, changed, add) = match action {
                CollectionAction::List => return list_collections(&mut backend, output.json),
                CollectionAction::Add { id, collections } => (id, collections, true),
                CollectionAction::Remove { id, collections } => (id, collections, false),
            };
            let before = backend.before_save()?;
            let result = backend.change_collections(&id, &changed, add)?;
            backend.after_save(
                before,
                HookOperation::ItemUpdate,
                HookItems::Uuids(vec![result.uuid]),
                Vec::new(),
            );
            print_mutation_result("Updated", &result.citation_key, result.uuid, output.json)
        }
        Command::Remove { id, output } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let before = backend.before_save()?;
            let result = backend.remove(&id)?;
            backend.after_save(
                before,
                HookOperation::ItemDelete,
                HookItems::Uuids(Vec::new()),
                vec![result.clone()],
            );
            if output.json {
                print_json(&result)
            } else {
                println!("Removed {}", result.citation_key);
                Ok(())
            }
        }
        Command::Detach {
            id,
            attachment_id,
            output,
        } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let before = backend.before_save()?;
            let result = backend.detach(&id, attachment_id)?;
            backend.after_save(
                before,
                HookOperation::AttachmentDelete,
                HookItems::Uuids(vec![result.item_uuid]),
                Vec::new(),
            );
            if output.json {
                print_json(&result)
            } else {
                println!(
                    "Detached {} from {}",
                    result.attachment_uuid, result.citation_key
                );
                Ok(())
            }
        }
        Command::Trash { action, output } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            match action {
                TrashAction::List => {
                    let entries = backend.trash_entries()?;
                    if output.json {
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
                    if output.json {
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
            output,
        } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let before = backend.before_save()?;
            let result = backend.attach(&id, &file, title.as_deref(), media_type.as_deref())?;
            backend.after_save(
                before,
                HookOperation::AttachmentCreate,
                HookItems::Uuids(vec![result.item_uuid]),
                Vec::new(),
            );
            if output.json {
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
        Command::Format { output } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let before = backend.before_save()?;
            let result = backend.format()?;
            backend.after_save(
                before,
                HookOperation::LibraryFormat,
                HookItems::All,
                Vec::new(),
            );
            if output.json {
                print_json(&result)
            } else {
                println!(
                    "Formatted library ({} missing IDs assigned)",
                    result.assigned_ids
                );
                Ok(())
            }
        }
        Command::Check { output } => {
            let mut backend = Backend::load(cli.library.as_deref(), &config_path)?;
            let report = backend.check()?;
            if output.json {
                print_json(&CheckOutput {
                    library: &backend.layout.bibliography,
                    attachments: &backend.layout.attachments,
                    report: &report,
                })?;
            } else {
                print_check(&backend.layout, &report);
            }
            if report.errors > 0 {
                Err(Error::CheckFailed {
                    errors: report.errors,
                })
            } else {
                Ok(())
            }
        }
        Command::External(_) => unreachable!("external commands are dispatched before built-ins"),
    }
}

fn run_extension(
    library: Option<&std::path::Path>,
    config: Option<&std::path::Path>,
    arguments: &[OsString],
) -> Result<i32> {
    let (name, arguments) = arguments
        .split_first()
        .expect("clap external subcommands always include a name");
    let executable = extension_executable(name)?;
    let current_executable =
        env::current_exe().map_err(|source| Error::CurrentExecutable { source })?;

    let mut command = ProcessCommand::new(&executable);
    command
        .args(arguments)
        .env("LANTAI", current_executable)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(library) = library {
        command.env(LIBRARY_ENV, library);
    }
    if let Some(config) = config {
        command.env("LANTAI_CONFIG", config);
    } else {
        command.env_remove("LANTAI_CONFIG");
    }

    match command.status() {
        Ok(status) => Ok(extension_status_code(status)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Err(Error::ExtensionNotFound {
            name: name.to_string_lossy().into_owned(),
        }),
        Err(source) => Err(Error::LaunchExtension {
            executable: PathBuf::from(executable),
            source,
        }),
    }
}

fn extension_executable(name: &OsStr) -> Result<OsString> {
    let display = name.to_string_lossy();
    if display.is_empty() || display.contains('/') || display.contains('\\') {
        return Err(Error::InvalidExtensionName {
            name: display.into_owned(),
        });
    }
    let mut executable = OsString::from(EXTENSION_PREFIX);
    executable.push(name);
    Ok(executable)
}

fn extension_status_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map_or(1, |signal| 128 + signal)
    }
    #[cfg(not(unix))]
    {
        1
    }
}

fn init(
    library: Option<PathBuf>,
    attachments: Option<PathBuf>,
    json: bool,
    config_path: &std::path::Path,
    force: bool,
) -> Result<()> {
    // Asking is the default. Supplying --library is the scripted contract, and
    // --json or any non-terminal stream means nobody is there to answer.
    // inquire draws on stderr, so that stream has to be a terminal too or the
    // prompts would be written somewhere the user cannot see.
    let interactive = library.is_none()
        && !json
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && io::stderr().is_terminal();
    if interactive {
        return init_interactively(attachments, config_path, force);
    }

    if config_path.exists() && !force {
        return Err(Error::ConfigAlreadyExists {
            path: config_path.to_owned(),
        });
    }
    let library = library.ok_or(Error::InitLibraryRequired)?;
    let layout = write_initialized_library(library, attachments, config_path, force)?;

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

/// Ask for the values the configuration has no answer for, then write it.
///
/// Abandoning a prompt is an answer too, so it leaves the library untouched
/// rather than reporting a failure.
fn init_interactively(
    attachments: Option<PathBuf>,
    config_path: &std::path::Path,
    force: bool,
) -> Result<()> {
    match prompt_and_initialize(attachments, config_path, force) {
        Ok(None)
        | Err(Error::Prompt {
            source:
                inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        }) => {
            println!("Nothing changed.");
            Ok(())
        }
        Ok(Some(())) => Ok(()),
        Err(error) => Err(error),
    }
}

/// `Ok(None)` means the user declined; nothing has been written.
fn prompt_and_initialize(
    attachments: Option<PathBuf>,
    config_path: &std::path::Path,
    force: bool,
) -> Result<Option<()>> {
    let mut replace_confirmed = force;
    if config_path.exists() && !force {
        println!("{} already configures a library.", config_path.display());
        if !confirm("Replace that configuration?", false)? {
            return Ok(None);
        }
        replace_confirmed = true;
    }

    // An exported LANTAI_LIBRARY outranks the configuration, so offering
    // anything else here would configure a library every later command ignores.
    let environment_library = env::var_os(LIBRARY_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    if let Some(path) = &environment_library {
        println!("{LIBRARY_ENV} is set to {}.", path.display());
    }
    let default_library = environment_library
        .as_ref()
        .map_or_else(default_library_path, |path| path.display().to_string());

    let library = loop {
        let answer = ask("Path to the bibliography", &default_library)?;
        let path = absolutize(&expand_home(answer.trim()))?;
        // Catch here what `initialize` would only reject after every remaining
        // question had been answered.
        if path.exists() && !path.is_file() {
            eprintln!("{}", Error::LibraryNotFile { path });
            continue;
        }
        match LibraryLayout::new(path.clone()) {
            Ok(_) => break path,
            Err(error) => eprintln!("{error}"),
        }
    };
    if library.is_file() {
        println!("Adopting the existing bibliography; it is never truncated.");
    }
    if let Some(parent) = library.parent().filter(|parent| !parent.is_dir())
        && !confirm(&format!("Create {}?", parent.display()), true)?
    {
        return Ok(None);
    }

    let derived_attachments = LibraryLayout::new(library.clone())?.attachments;
    let attachments = match attachments {
        Some(attachments) => Some(attachments),
        None => {
            println!(
                "Attachments will live in {}.",
                derived_attachments.display()
            );
            if confirm("Store them somewhere else?", false)? {
                // The derived path is the default, so an empty answer keeps it
                // rather than resolving to the current directory.
                let answer = ask(
                    "Attachment directory",
                    &derived_attachments.display().to_string(),
                )?;
                let answer = answer.trim();
                (!answer.is_empty()).then(|| expand_home(answer))
            } else {
                None
            }
        }
    };

    println!();
    println!("Bibliography: {}", library.display());
    println!(
        "Attachments:  {}",
        attachments
            .as_ref()
            .unwrap_or(&derived_attachments)
            .display()
    );
    println!("Config:       {}", config_path.display());
    println!("The configuration holds a new random REST token, readable only by you.");
    if !confirm("Initialize?", true)? {
        return Ok(None);
    }
    if let Some(path) = &environment_library
        && absolutize(path)? != library
    {
        println!();
        println!(
            "Note: {LIBRARY_ENV} still points at {}, which takes precedence over this configuration.",
            path.display()
        );
    }

    // Only replace a configuration the user agreed to replace: one that
    // appeared while the questions were being answered is not that one.
    let layout = write_initialized_library(library, attachments, config_path, replace_confirmed)?;
    println!();
    println!("Initialized {}", layout.bibliography.display());
    println!("Attachments: {}", layout.attachments.display());
    println!();
    println!("Next:");
    println!("  lantai add --type article --field 'title=...'   add an entry");
    println!("  lantai import LIBRARY.rdf                       import from Zotero");
    println!("  lantai serve                                    REST API and browser capture");
    Ok(Some(()))
}

/// Create or adopt the bibliography and write the configuration beside it.
fn write_initialized_library(
    library: PathBuf,
    attachments: Option<PathBuf>,
    config_path: &std::path::Path,
    force: bool,
) -> Result<LibraryLayout> {
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
    Ok(layout)
}

/// The bibliography path offered when the user has expressed no preference.
fn default_library_path() -> String {
    BaseDirs::new().map_or_else(
        || "references.bib".to_owned(),
        |dirs| dirs.home_dir().join("references.bib").display().to_string(),
    )
}

/// Expand a leading `~`, the way a shell would have before the value reached us.
///
/// Only `~` alone and `~/...` mean "my home". `~alice/notes.bib` names another
/// user's home, which we cannot resolve, and `~draft.bib` is simply a filename;
/// both are left exactly as typed rather than silently relocated under `$HOME`.
fn expand_home(path: &str) -> PathBuf {
    let rest = match path.strip_prefix('~') {
        Some("") => "",
        Some(rest) if rest.starts_with('/') => rest.trim_start_matches('/'),
        _ => return PathBuf::from(path),
    };
    BaseDirs::new().map_or_else(
        || PathBuf::from(path),
        |dirs| {
            if rest.is_empty() {
                dirs.home_dir().to_path_buf()
            } else {
                dirs.home_dir().join(rest)
            }
        },
    )
}

fn ask(message: &str, default: &str) -> Result<String> {
    inquire::Text::new(message)
        .with_default(default)
        .prompt()
        .map_err(|source| Error::Prompt { source })
}

fn confirm(message: &str, default: bool) -> Result<bool> {
    inquire::Confirm::new(message)
        .with_default(default)
        .prompt()
        .map_err(|source| Error::Prompt { source })
}

/// Print the collection tree, or its paths as JSON.
///
/// Human output nests on `/` the way the Connector picker does; JSON stays flat
/// because a script wants the name it would pass back to `--collection`. Every
/// path listed does match there, including a synthesized ancestor that no item
/// belongs to directly, because `--collection` matches nested collections too.
fn list_collections(backend: &mut Backend, json: bool) -> Result<()> {
    let tree = collections::tree(backend.collections()?);
    if json {
        return print_json(
            &tree
                .iter()
                .map(|target| target.path.as_str())
                .collect::<Vec<_>>(),
        );
    }
    for target in tree {
        // Level 1 is the shallowest collection; the library root is not a row.
        println!("{}{}", "  ".repeat(target.level - 1), target.name);
    }
    Ok(())
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

fn matches_item(item: &CatalogItem, query: Option<&str>, collection: Option<&str>) -> bool {
    if collection.is_some_and(|collection| {
        !item
            .collections
            .iter()
            .any(|candidate| collections::matches(candidate, collection))
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

#[derive(Debug, Serialize)]
struct ImportSummary {
    imported: usize,
    attachments: usize,
    collections: Vec<String>,
    items: Vec<AddedItem>,
    skipped_attachments: Vec<SkippedAttachment>,
}

impl ImportSummary {
    fn preview(import: &RdfImport) -> Self {
        Self {
            imported: import.items.len(),
            attachments: import.items.iter().map(|item| item.attachments.len()).sum(),
            collections: import.collections.clone(),
            items: Vec::new(),
            skipped_attachments: import.skipped.clone(),
        }
    }
}

/// Create every item, then copy its files.
///
/// A rejected item rolls the whole import back, matching `add --from`. A
/// rejected file is reported instead, so one unreadable attachment cannot
/// discard an otherwise correct library.
fn run_import(backend: &mut Backend, import: RdfImport) -> Result<ImportSummary> {
    let before = backend.before_save()?;
    let mut added: Vec<AddedItem> = Vec::with_capacity(import.items.len());
    let mut attachments = 0;
    let mut skipped = import.skipped;

    for source in import.items {
        let label = source
            .item
            .data
            .get("citationKey")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&source.item.id)
            .to_owned();
        let mapped = match map_item(source.item).and_then(|mapped| add_item(backend, mapped.item)) {
            Ok(mapped) => mapped,
            Err(error) => {
                // The library is restored, so no post-save hook is emitted.
                for item in &added {
                    let _ = backend.remove(&item.uuid.to_string());
                }
                return Err(error);
            }
        };
        for attachment in source.attachments {
            match backend.attach(
                &mapped.uuid.to_string(),
                &attachment.path,
                Some(&attachment.title),
                attachment.media_type.as_deref(),
            ) {
                Ok(_) => attachments += 1,
                Err(error) => skipped.push(SkippedAttachment {
                    item: label.clone(),
                    title: attachment.title,
                    reason: error.to_string(),
                }),
            }
        }
        added.push(mapped);
    }

    backend.after_save(
        before,
        HookOperation::ItemImport,
        HookItems::Uuids(added.iter().map(|item| item.uuid).collect()),
        Vec::new(),
    );
    Ok(ImportSummary {
        imported: added.len(),
        attachments,
        collections: import.collections,
        items: added,
        skipped_attachments: skipped,
    })
}

/// Zotero citation keys are reused, but they are not authoritative here: an
/// existing entry already owning the key, or a key Lantai rejects, falls back
/// to the generated AuthorYearTitle form.
fn add_item(backend: &mut Backend, item: NewItem) -> Result<AddedItem> {
    if item.citation_key.is_none() {
        return backend.add(item);
    }
    match backend.add(item.clone()) {
        Err(Error::DuplicateCitationKey { .. } | Error::InvalidCitationKey { .. }) => {
            backend.add(NewItem {
                citation_key: None,
                ..item
            })
        }
        result => result,
    }
}

fn print_import(summary: &ImportSummary, dry_run: bool) {
    let verb = if dry_run { "Would import" } else { "Imported" };
    println!(
        "{verb} {} item(s) and {} file(s) from {} collection(s)",
        summary.imported,
        summary.attachments,
        summary.collections.len()
    );
    if !summary.skipped_attachments.is_empty() {
        println!(
            "Skipped {} attachment(s):",
            summary.skipped_attachments.len()
        );
        for skipped in &summary.skipped_attachments {
            println!("  {}: {} ({})", skipped.item, skipped.title, skipped.reason);
        }
    }
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

fn item_output_is_json(format: Option<OutputFormat>) -> bool {
    format != Some(OutputFormat::Human)
}

fn print_item(item: &ItemView) {
    println!("@{}{{{}}}", item.entry_type, item.citation_key);
    if let Some(uuid) = item.uuid {
        println!("UUID: {uuid}");
    }
    for field in &item.fields {
        println!("{}: {}", field.name, field.value);
    }
}

fn print_check(layout: &LibraryLayout, report: &CheckReport) {
    println!(
        "{}: {}",
        report.status.as_str(),
        layout.bibliography.display()
    );
    println!("Attachments: {}", layout.attachments.display());
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

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn daemon_backed_cli_runs_one_cli_origin_hook() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();
        layout.initialize().unwrap();
        let event_path = directory.path().join("event.json");
        let calls_path = directory.path().join("calls");
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = probe.local_addr().unwrap();
        drop(probe);
        let config_path = directory.path().join("config.toml");
        let mut config = Config::new(bibliography);
        config.api_address = address.to_string();
        config.post_save_hook = Some(crate::config::PostSaveHookConfig {
            command: "/bin/sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "cat > \"$1\"; printf x >> \"$2\"".to_owned(),
                "lantai-hook".to_owned(),
                event_path.display().to_string(),
                calls_path.display().to_string(),
            ],
            timeout_seconds: 30,
        });
        config.write(&config_path, false).unwrap();
        let listener = tokio::net::TcpListener::bind(address).await.unwrap();
        let state = crate::server::AppState::new(config, layout).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, crate::server::native_router(state))
                .await
                .unwrap()
        });

        let command_config = config_path.clone();
        tokio::task::spawn_blocking(move || {
            run_parsed(
                Cli::try_parse_from([
                    "lantai",
                    "--config",
                    command_config.to_str().unwrap(),
                    "add",
                    "--type",
                    "article",
                    "--field",
                    "title=Daemon hook",
                ])
                .unwrap(),
            )
            .unwrap()
        })
        .await
        .unwrap();

        server.abort();
        let _ = server.await;
        assert_eq!(std::fs::read_to_string(calls_path).unwrap(), "x");
        let event: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(event_path).unwrap()).unwrap();
        assert_eq!(event["origin"], "cli");
        assert_eq!(event["operation"], "item.create");
    }

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
        let (item_uuid, attachment_uuid, daemon_item, daemon_list) =
            tokio::task::spawn_blocking(move || {
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
                    .change_collections(&added.uuid.to_string(), &["remote".to_owned()], true)
                    .unwrap();
                // Re-adding under a different spelling must not create a second
                // collection, in either mode.
                backend
                    .change_collections(&added.uuid.to_string(), &[" REMOTE ".to_owned()], true)
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
                let item = backend.get(&added.uuid.to_string()).unwrap();
                let listed = backend.list(Some("After"), Some("REMOTE")).unwrap();
                (added.uuid, attached.attachment_uuid, item, listed)
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
        assert_eq!(
            item.collections,
            vec!["remote"],
            "a case- and space-variant re-add is a no-op keeping the first spelling"
        );
        assert_eq!(item.attachments[0].uuid, Some(attachment_uuid));
        assert_eq!(item, daemon_item);

        // The same re-add through the direct backend must agree.
        direct
            .change_collections(&item_uuid.to_string(), &["Remote".to_owned()], true)
            .unwrap();
        assert_eq!(
            direct.get(&item_uuid.to_string()).unwrap().collections,
            vec!["remote"]
        );
        assert_eq!(
            direct.list(Some("After"), Some("remote")).unwrap(),
            daemon_list
        );
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

    #[test]
    fn item_output_defaults_to_json_without_a_legacy_json_flag() {
        assert!(item_output_is_json(None));
        assert!(item_output_is_json(Some(OutputFormat::Json)));
        assert!(!item_output_is_json(Some(OutputFormat::Human)));

        let parsed = Cli::try_parse_from(["lantai", "list", "--format", "human"]).unwrap();
        assert!(matches!(
            parsed.command,
            Command::List {
                format: Some(OutputFormat::Human),
                ..
            }
        ));
        assert!(Cli::try_parse_from(["lantai", "list", "--json"]).is_err());
        assert!(Cli::try_parse_from(["lantai", "show", "item", "--json"]).is_err());
        assert!(Cli::try_parse_from(["lantai", "check", "--json"]).is_ok());
        assert!(
            Cli::try_parse_from(["lantai", "collection", "add", "item", "Reviewed", "--json"])
                .is_ok()
        );
    }

    #[test]
    fn list_filters_by_collection_and_no_longer_by_type() {
        let parsed =
            Cli::try_parse_from(["lantai", "list", "--collection", "Projects/IfT"]).unwrap();
        assert!(matches!(
            parsed.command,
            Command::List {
                collection: Some(ref collection),
                ..
            } if collection == "Projects/IfT"
        ));
        assert!(Cli::try_parse_from(["lantai", "list", "--type", "article"]).is_err());
        // `add` keeps its own --type; only the filter went away.
        assert!(Cli::try_parse_from(["lantai", "add", "--type", "article"]).is_ok());
    }

    /// Whatever `collection list` prints must find items again.
    #[test]
    fn every_listed_collection_matches_the_items_it_covers() {
        let item = CatalogItem {
            uuid: None,
            citation_key: "filed".to_owned(),
            entry_type: "article".to_owned(),
            fields: Vec::new(),
            collections: vec!["Projects / IfT".to_owned()],
            attachments: Vec::new(),
        };
        // The tree synthesizes "Projects" and trims the spelling to
        // "Projects/IfT"; both are offered to the user, so both must filter.
        for target in collections::tree(collections::of_items([item.collections.clone()])) {
            assert!(
                matches_item(&item, None, Some(&target.path)),
                "{} was listed but matches nothing",
                target.path
            );
        }
        assert!(!matches_item(&item, None, Some("Projects/Other")));
    }

    /// The grouped help is hand-written, so nothing may quietly fall out of it.
    #[test]
    fn every_built_in_command_appears_in_exactly_one_help_group() {
        let command = Cli::command();
        let mut grouped = Vec::new();
        for (_, names) in COMMAND_GROUPS {
            grouped.extend(names.iter().copied());
        }
        let unique = grouped.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), grouped.len(), "a command is grouped twice");

        let parsed = command
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            parsed, unique,
            "the grouped help and the parser disagree about the command set"
        );
        assert!(
            command
                .get_subcommands()
                .all(|subcommand| subcommand.get_about().is_some()),
            "the grouped help reads each description from the parser"
        );
    }

    /// The grouped sections must not look hand-made next to clap's own.
    #[test]
    fn the_grouped_help_wears_claps_styles() {
        let command = Cli::command();
        let styles = command.get_styles();
        let header = styles.get_header().to_string();
        let literal = styles.get_literal().to_string();
        let reset = format!("{:#}", styles.get_header());
        let help = grouped_help(&command);

        assert!(help.contains(&format!("{header}Setup and status:{reset}")));
        assert!(help.contains(&format!("{header}Custom commands:{reset}")));
        assert!(help.contains(&format!("  {literal}init{reset}")));

        // Padding has to sit outside the escapes, or the descriptions stagger
        // by however many bytes the styling happens to take.
        let plain = help
            .replace(&header, "")
            .replace(&literal, "")
            .replace(&reset, "");
        let column = |name: &str| {
            let line = plain
                .lines()
                .find(|line| line.starts_with(&format!("  {name}")))
                .unwrap_or_else(|| panic!("{name} is listed"));
            let rest = &line[2 + name.len()..];
            2 + name.len() + (rest.len() - rest.trim_start().len())
        };
        assert_eq!(
            column("init"),
            column("collection"),
            "descriptions share one column whatever the name's length"
        );
    }

    #[test]
    fn help_is_rendered_only_when_it_will_be_shown() {
        let arguments = |values: &[&str]| {
            values
                .iter()
                .map(|value| OsString::from(*value))
                .collect::<Vec<_>>()
        };
        assert!(wants_help(arguments(&[])));
        assert!(wants_help(arguments(&["--help"])));
        assert!(wants_help(arguments(&["-h"])));
        assert!(wants_help(arguments(&["help"])));
        assert!(wants_help(arguments(&["--library", "refs.bib", "--help"])));
        assert!(!wants_help(arguments(&["list"])));
        assert!(!wants_help(arguments(&["collection", "list"])));
        // The grouped help hangs off the root, so a subcommand's own help is
        // clap's to render and must not trigger a PATH scan.
        assert!(!wants_help(arguments(&["list", "--help"])));
        // `help` first is the root help; a positional that merely spells
        // "help" belongs to its subcommand.
        assert!(!wants_help(arguments(&["show", "help"])));
        // A value that merely looks like a flag's argument stays a value.
        assert!(!wants_help(arguments(&["--library", "help", "list"])));
    }

    #[test]
    fn expanding_a_leading_tilde_only_means_this_user() {
        let home = BaseDirs::new().expect("a home directory");
        assert_eq!(expand_home("~"), home.home_dir());
        assert_eq!(expand_home("~/refs.bib"), home.home_dir().join("refs.bib"));
        // Another user's home is not ours to guess, and a filename that starts
        // with a tilde is just a filename.
        assert_eq!(
            expand_home("~alice/refs.bib"),
            PathBuf::from("~alice/refs.bib")
        );
        assert_eq!(expand_home("~draft.bib"), PathBuf::from("~draft.bib"));
        assert_eq!(expand_home("refs.bib"), PathBuf::from("refs.bib"));
    }

    #[test]
    fn discovery_skips_built_ins_and_non_executables() {
        let directory = tempfile::tempdir().unwrap();
        for name in ["lantai-check", "lantai-mine", "lantai-notes", "unrelated"] {
            std::fs::write(directory.path().join(name), "#!/bin/sh\n").unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in ["lantai-check", "lantai-mine"] {
                std::fs::set_permissions(
                    directory.path().join(name),
                    std::fs::Permissions::from_mode(0o755),
                )
                .unwrap();
            }
            // Installing an extension by symlinking it into a PATH directory
            // is normal, and dispatch follows the link.
            std::os::unix::fs::symlink(
                directory.path().join("lantai-mine"),
                directory.path().join("lantai-linked"),
            )
            .unwrap();
        }

        let builtins = COMMAND_GROUPS
            .iter()
            .flat_map(|(_, names)| names.iter().copied())
            .collect::<BTreeSet<_>>();
        let found = discover_extensions(Some(directory.path().as_os_str()), &builtins);

        assert!(found.contains_key("mine"));
        assert!(!found.contains_key("check"), "a built-in always wins");
        #[cfg(unix)]
        {
            assert!(
                !found.contains_key("notes"),
                "a non-executable file cannot run"
            );
            assert!(
                found.contains_key("linked"),
                "a symlinked extension runs, so it must be listed"
            );
        }
    }
}
