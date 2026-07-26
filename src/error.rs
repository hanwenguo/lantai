use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not determine the platform configuration directory")]
    ConfigDirectoryUnavailable,

    #[error("invalid socket address {address:?}: {message}")]
    InvalidSocketAddress { address: String, message: String },

    #[error("server address must be loopback-only: {address}")]
    NonLoopbackAddress { address: String },

    #[error("failed to listen on {address}: {source}")]
    Listen {
        address: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to watch {path}: {message}")]
    Watch { path: PathBuf, message: String },

    #[error("configuration file {path} already exists (use --force to replace it)")]
    ConfigAlreadyExists { path: PathBuf },

    #[error("no library configured; run `lantai init`, or pass --library or LANTAI_LIBRARY")]
    LibraryNotConfigured,

    #[error(
        "init needs a library; pass --library PATH, or run `lantai init` in an interactive terminal"
    )]
    InitLibraryRequired,

    #[error(
        "the running daemon is version {daemon} but this is lantai {cli}; restart `lantai serve`"
    )]
    DaemonVersionMismatch { daemon: String, cli: String },

    #[error("could not read the answer: {source}")]
    Prompt {
        #[source]
        source: inquire::InquireError,
    },

    #[error("library path has no file name: {path}")]
    InvalidLibraryPath { path: PathBuf },

    #[error("library path is not a regular file: {path}")]
    LibraryNotFile { path: PathBuf },

    #[error("library is not valid UTF-8: {path}")]
    LibraryNotUtf8 { path: PathBuf },

    #[error("failed to parse bibliography {path}: {source}")]
    ParseBibliography {
        path: PathBuf,
        #[source]
        source: bibtex_parser::Error,
    },

    #[error("bibliography {path} is degraded and cannot be changed: {message}")]
    DegradedBibliography { path: PathBuf, message: String },

    #[error("failed to parse Zotero RDF {path}: {message}")]
    ParseZoteroRdf { path: PathBuf, message: String },

    #[error("Zotero RDF export {path} contains no items")]
    ZoteroRdfHasNoItems { path: PathBuf },

    #[error("item not found: {id}")]
    ItemNotFound { id: String },

    #[error("item identifier is ambiguous: {id}")]
    AmbiguousItem { id: String },

    #[error("invalid query term {term:?}: {message}")]
    InvalidQueryTerm { term: String, message: String },

    #[error("invalid sort key {key:?}; expected key, type, title, year, or a field name")]
    InvalidSortKey { key: String },

    #[error("invalid field argument {argument:?}; expected NAME=VALUE")]
    InvalidFieldArgument { argument: String },

    #[error("field name cannot be empty")]
    EmptyFieldName,

    #[error("field is specified more than once: {field}")]
    DuplicateField { field: String },

    #[error("invalid BibLaTeX entry type: {entry_type:?}")]
    InvalidEntryType { entry_type: String },

    #[error("invalid citation key: {key:?}")]
    InvalidCitationKey { key: String },

    #[error("item {key} has an invalid or duplicate {field} field")]
    InvalidItemIdentity { key: String, field: &'static str },

    #[error("invalid raw expression for {field}: {message}")]
    InvalidRawExpression { field: String, message: String },

    #[error("attachment source is not a regular file: {path}")]
    AttachmentNotFile { path: PathBuf },

    #[error("attachment exceeds the configured limit of {limit} bytes")]
    AttachmentTooLarge { limit: u64 },

    #[error("invalid BibLaTeX file field: {message}")]
    InvalidFileField { message: String },

    #[error("attachment not found: {id}")]
    AttachmentNotFound { id: String },

    #[error("unsafe attachment path: {path}")]
    UnsafeAttachmentPath { path: PathBuf },

    #[error("{field} is managed by Lantai and cannot be supplied as an ordinary field")]
    ReservedField { field: String },

    #[error("citation key already exists: {key}")]
    DuplicateCitationKey { key: String },

    #[error("stable UUID already exists: {uuid}")]
    DuplicateUuid { uuid: uuid::Uuid },

    #[error("BibLaTeX import contains no entries")]
    ImportHasNoEntries,

    #[error("library changed during a write transaction: {path}")]
    SourceChanged { path: PathBuf },

    #[error("failed to lock {path}: {source}")]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("library check failed with {errors} error(s)")]
    CheckFailed { errors: usize },

    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to create directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse configuration {path}: {source}")]
    ParseConfig {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("invalid post-save hook configuration: {message}")]
    InvalidPostSaveHook { message: String },

    #[error("failed to serialize configuration: {0}")]
    SerializeConfig(#[from] toml::ser::Error),

    #[error("failed to serialize JSON output: {0}")]
    SerializeJson(#[from] serde_json::Error),

    #[error("invalid custom subcommand name: {name:?}")]
    InvalidExtensionName { name: String },

    #[error("{name:?} is not a lantai command; install `lantai-{name}` on PATH")]
    ExtensionNotFound { name: String },

    #[error("failed to launch custom subcommand {executable}: {source}")]
    LaunchExtension {
        executable: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to launch post-save hook {executable}: {source}")]
    LaunchPostSaveHook {
        executable: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write the post-save event: {source}")]
    WritePostSaveHook {
        #[source]
        source: std::io::Error,
    },

    #[error("failed to wait for the post-save hook: {source}")]
    WaitPostSaveHook {
        #[source]
        source: std::io::Error,
    },

    #[error("post-save hook exited unsuccessfully{status_message}", status_message = status.map_or_else(|| "".to_owned(), |code| format!(" with status {code}")))]
    PostSaveHookExit { status: Option<i32> },

    #[error("post-save hook timed out after {seconds} seconds")]
    PostSaveHookTimeout { seconds: u64 },

    #[error("failed to locate the current Lantai executable: {source}")]
    CurrentExecutable {
        #[source]
        source: std::io::Error,
    },

    #[error("Lantai daemon at {address} returned {status} {code}: {message}")]
    Api {
        address: String,
        status: u16,
        code: String,
        message: String,
    },

    #[error("failed to communicate with Lantai daemon at {address}: {message}")]
    Daemon { address: String, message: String },
}

pub type Result<T> = std::result::Result<T, Error>;
