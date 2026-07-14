use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::catalog::{Catalog, ItemView};
use crate::config::PostSaveHookConfig;
use crate::library::{LibraryLayout, RemovedItem};
use crate::{Error, Result};

pub const HOOK_ACTIVE_ENV: &str = "LANTAI_POST_SAVE";
pub const SUPPRESS_HOOK_HEADER: &str = "X-Lantai-Suppress-Post-Save";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookOrigin {
    Cli,
    Rest,
    Connector,
}

impl HookOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Rest => "rest",
            Self::Connector => "connector",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum HookOperation {
    #[serde(rename = "item.create")]
    ItemCreate,
    #[serde(rename = "item.import")]
    ItemImport,
    #[serde(rename = "item.update")]
    ItemUpdate,
    #[serde(rename = "item.delete")]
    ItemDelete,
    #[serde(rename = "attachment.create")]
    AttachmentCreate,
    #[serde(rename = "attachment.delete")]
    AttachmentDelete,
    #[serde(rename = "library.format")]
    LibraryFormat,
}

impl HookOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::ItemCreate => "item.create",
            Self::ItemImport => "item.import",
            Self::ItemUpdate => "item.update",
            Self::ItemDelete => "item.delete",
            Self::AttachmentCreate => "attachment.create",
            Self::AttachmentDelete => "attachment.delete",
            Self::LibraryFormat => "library.format",
        }
    }
}

#[derive(Clone, Debug)]
pub enum HookItems {
    Uuids(Vec<uuid::Uuid>),
    All,
}

#[derive(Debug, Serialize)]
pub struct PostSaveEvent {
    schema_version: u32,
    event: &'static str,
    operation: HookOperation,
    origin: HookOrigin,
    library: PathBuf,
    revision: String,
    items: Vec<ItemView>,
    removed_items: Vec<RemovedItem>,
}

#[derive(Clone)]
pub struct PostSaveHook {
    inner: Option<Arc<PostSaveHookInner>>,
}

pub struct PreparedPostSaveHook {
    inner: Arc<PostSaveHookInner>,
    event: PostSaveEvent,
}

struct PostSaveHookInner {
    command: PathBuf,
    args: Vec<String>,
    timeout: Duration,
    config_path: PathBuf,
    layout: LibraryLayout,
    serial: Mutex<()>,
}

impl PostSaveHook {
    pub fn new(
        config: Option<&PostSaveHookConfig>,
        config_path: &Path,
        layout: LibraryLayout,
    ) -> Self {
        let inner = config.map(|config| {
            let configured = PathBuf::from(&config.command);
            let command = if configured.is_relative() && configured.components().count() > 1 {
                config_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(configured)
            } else {
                configured
            };
            Arc::new(PostSaveHookInner {
                command,
                args: config.args.clone(),
                timeout: Duration::from_secs(config.timeout_seconds),
                config_path: config_path.to_owned(),
                layout,
                serial: Mutex::new(()),
            })
        });
        Self { inner }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some() && env::var_os(HOOK_ACTIVE_ENV).is_none()
    }

    pub fn revision_before_save(&self) -> Result<Option<String>> {
        if !self.is_enabled() {
            return Ok(None);
        }
        self.inner
            .as_ref()
            .map(|inner| inner.layout.read_utf8().map(|source| revision(&source)))
            .transpose()
    }

    pub fn emit(
        &self,
        before: Option<String>,
        operation: HookOperation,
        origin: HookOrigin,
        affected: HookItems,
        removed_items: Vec<RemovedItem>,
    ) {
        if let Some(prepared) = self.prepare(before, operation, origin, affected, removed_items) {
            prepared.run();
        }
    }

    pub fn prepare(
        &self,
        before: Option<String>,
        operation: HookOperation,
        origin: HookOrigin,
        affected: HookItems,
        removed_items: Vec<RemovedItem>,
    ) -> Option<PreparedPostSaveHook> {
        if !self.is_enabled() {
            return None;
        }
        let inner = self.inner.as_ref()?;
        let before = before?;
        match inner.prepare(before, operation, origin, affected, removed_items) {
            Ok(event) => event.map(|event| PreparedPostSaveHook {
                inner: inner.clone(),
                event,
            }),
            Err(error) => {
                eprintln!("warning: could not prepare post-save hook: {error}");
                None
            }
        }
    }
}

impl PreparedPostSaveHook {
    pub fn run(self) {
        if let Err(error) = self.inner.run(self.event) {
            eprintln!("warning: post-save hook failed: {error}");
        }
    }
}

impl PostSaveHookInner {
    fn prepare(
        &self,
        before: String,
        operation: HookOperation,
        origin: HookOrigin,
        affected: HookItems,
        removed_items: Vec<RemovedItem>,
    ) -> Result<Option<PostSaveEvent>> {
        let source = self.layout.read_utf8()?;
        let after = revision(&source);
        if before == after {
            return Ok(None);
        }
        let catalog = Catalog::parse(&self.layout.bibliography, &source)?;
        let items = match affected {
            HookItems::All => catalog.views().collect(),
            HookItems::Uuids(uuids) => catalog
                .views()
                .filter(|item| item.uuid.is_some_and(|uuid| uuids.contains(&uuid)))
                .collect(),
        };
        Ok(Some(PostSaveEvent {
            schema_version: 1,
            event: "post-save",
            operation,
            origin,
            library: self.layout.bibliography.clone(),
            revision: after.clone(),
            items,
            removed_items,
        }))
    }

    fn run(&self, event: PostSaveEvent) -> Result<()> {
        let after = event.revision.clone();
        let operation = event.operation;
        let origin = event.origin;
        let input = serde_json::to_vec(&event)?;
        let _serial = self
            .serial
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let executable =
            env::current_exe().map_err(|source| Error::CurrentExecutable { source })?;
        let working_directory =
            self.layout
                .bibliography
                .parent()
                .ok_or_else(|| Error::InvalidLibraryPath {
                    path: self.layout.bibliography.clone(),
                })?;
        let mut child = Command::new(&self.command)
            .args(&self.args)
            .current_dir(working_directory)
            .env("LANTAI", executable)
            .env("LANTAI_LIBRARY", &self.layout.bibliography)
            .env("LANTAI_CONFIG", &self.config_path)
            .env(HOOK_ACTIVE_ENV, "1")
            .env("LANTAI_OPERATION", operation.as_str())
            .env("LANTAI_ORIGIN", origin.as_str())
            .env("LANTAI_REVISION", &after)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|source| Error::LaunchPostSaveHook {
                executable: self.command.clone(),
                source,
            })?;
        if let Some(mut stdin) = child.stdin.take()
            && let Err(source) = stdin.write_all(&input)
        {
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::WritePostSaveHook { source });
        }
        let started = Instant::now();
        loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|source| Error::WaitPostSaveHook { source })?
            {
                if status.success() {
                    return Ok(());
                }
                return Err(Error::PostSaveHookExit {
                    status: status.code(),
                });
            }
            if started.elapsed() >= self.timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::PostSaveHookTimeout {
                    seconds: self.timeout.as_secs(),
                });
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

pub fn revision(source: &str) -> String {
    blake3::hash(source.as_bytes()).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PostSaveHookConfig;
    use crate::library::{LibraryStore, NewItem};

    #[cfg(unix)]
    #[test]
    fn hook_receives_full_event_environment_and_working_directory() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();
        layout.initialize().unwrap();
        let event_path = directory.path().join("event.json");
        let environment_path = directory.path().join("environment.txt");
        let config_path = directory.path().join("config.toml");
        let script = concat!(
            "cat > \"$1\"; ",
            "printf '%s\\n%s\\n%s\\n%s\\n%s\\n%s\\n' ",
            "\"$PWD\" \"$LANTAI_LIBRARY\" \"$LANTAI_CONFIG\" ",
            "\"$LANTAI_OPERATION\" \"$LANTAI_ORIGIN\" \"$LANTAI_POST_SAVE\" > \"$2\""
        );
        let config = PostSaveHookConfig {
            command: "/bin/sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                script.to_owned(),
                "lantai-hook".to_owned(),
                event_path.display().to_string(),
                environment_path.display().to_string(),
            ],
            timeout_seconds: 30,
        };
        let hook = PostSaveHook::new(Some(&config), &config_path, layout.clone());
        let before = hook.revision_before_save().unwrap();
        let added = LibraryStore::new(layout)
            .add_item(NewItem {
                entry_type: "article".to_owned(),
                citation_key: Some("hooked".to_owned()),
                fields: vec![("title".to_owned(), "Hooked item".to_owned())],
            })
            .unwrap();

        hook.emit(
            before,
            HookOperation::ItemCreate,
            HookOrigin::Cli,
            HookItems::Uuids(vec![added.uuid]),
            Vec::new(),
        );

        let event: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(event_path).unwrap()).unwrap();
        assert_eq!(event["schema_version"], 1);
        assert_eq!(event["event"], "post-save");
        assert_eq!(event["operation"], "item.create");
        assert_eq!(event["origin"], "cli");
        assert_eq!(event["library"], bibliography.display().to_string());
        assert_eq!(event["items"][0]["uuid"], added.uuid.to_string());
        assert_eq!(event["items"][0]["title"], "Hooked item");
        assert_eq!(event["removed_items"], serde_json::json!([]));
        let environment = std::fs::read_to_string(environment_path).unwrap();
        let lines = environment.lines().collect::<Vec<_>>();
        assert_eq!(
            std::fs::canonicalize(lines[0]).unwrap(),
            std::fs::canonicalize(directory.path()).unwrap()
        );
        assert_eq!(lines[1], bibliography.display().to_string());
        assert_eq!(lines[2], config_path.display().to_string());
        assert_eq!(lines[3..], ["item.create", "cli", "1"]);
        assert_eq!(
            event["revision"],
            revision(&std::fs::read_to_string(bibliography).unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn identical_revision_does_not_launch_hook() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let layout = LibraryLayout::new(bibliography).unwrap();
        layout.initialize().unwrap();
        let marker = directory.path().join("marker");
        let config = PostSaveHookConfig {
            command: "/bin/sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "touch \"$1\"".to_owned(),
                "lantai-hook".to_owned(),
                marker.display().to_string(),
            ],
            timeout_seconds: 30,
        };
        let hook = PostSaveHook::new(Some(&config), &directory.path().join("config.toml"), layout);
        let before = hook.revision_before_save().unwrap();
        hook.emit(
            before,
            HookOperation::LibraryFormat,
            HookOrigin::Cli,
            HookItems::All,
            Vec::new(),
        );
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_and_timed_out_hooks_return_specific_failures() {
        fn prepared(
            command: &str,
            timeout_seconds: u64,
        ) -> (PreparedPostSaveHook, tempfile::TempDir) {
            let directory = tempfile::tempdir().unwrap();
            let bibliography = directory.path().join("references.bib");
            let layout = LibraryLayout::new(bibliography).unwrap();
            layout.initialize().unwrap();
            let config = PostSaveHookConfig {
                command: "/bin/sh".to_owned(),
                args: vec!["-c".to_owned(), command.to_owned()],
                timeout_seconds,
            };
            let hook = PostSaveHook::new(
                Some(&config),
                &directory.path().join("config.toml"),
                layout.clone(),
            );
            let before = hook.revision_before_save().unwrap();
            let added = LibraryStore::new(layout)
                .add_item(NewItem {
                    entry_type: "misc".to_owned(),
                    citation_key: Some("failure".to_owned()),
                    fields: Vec::new(),
                })
                .unwrap();
            (
                hook.prepare(
                    before,
                    HookOperation::ItemCreate,
                    HookOrigin::Cli,
                    HookItems::Uuids(vec![added.uuid]),
                    Vec::new(),
                )
                .unwrap(),
                directory,
            )
        }

        let (PreparedPostSaveHook { inner, event }, _directory) = prepared("exit 7", 30);
        assert!(matches!(
            inner.run(event),
            Err(Error::PostSaveHookExit { status: Some(7) })
        ));

        let (PreparedPostSaveHook { inner, event }, _directory) = prepared("sleep 2", 1);
        assert!(matches!(
            inner.run(event),
            Err(Error::PostSaveHookTimeout { seconds: 1 })
        ));
    }

    #[test]
    fn relative_command_paths_are_based_at_the_config_directory() {
        let directory = tempfile::tempdir().unwrap();
        let layout = LibraryLayout::new(directory.path().join("references.bib")).unwrap();
        let config_path = directory.path().join("config.toml");
        let config = PostSaveHookConfig {
            command: "./hooks/reindex".to_owned(),
            args: Vec::new(),
            timeout_seconds: 30,
        };
        let hook = PostSaveHook::new(Some(&config), &config_path, layout);
        assert_eq!(
            hook.inner.unwrap().command,
            directory.path().join("./hooks/reindex")
        );
    }
}
