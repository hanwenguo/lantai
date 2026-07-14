use std::env;
#[cfg(target_os = "macos")]
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(not(target_os = "macos"))]
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Error, Result};

pub const LIBRARY_ENV: &str = "LANTAI_LIBRARY";
pub const DEFAULT_API_ADDRESS: &str = "127.0.0.1:23120";
pub const DEFAULT_ATTACHMENT_LIMIT_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_POST_SAVE_HOOK_TIMEOUT_SECONDS: u64 = 30;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PostSaveHookConfig {
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default = "default_post_save_hook_timeout_seconds")]
    pub timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub library: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment_root: Option<PathBuf>,
    pub api_address: String,
    pub api_token: String,
    pub attachment_limit_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_save_hook: Option<PostSaveHookConfig>,
}

impl Config {
    pub fn new(library: PathBuf) -> Self {
        Self {
            library,
            attachment_root: None,
            api_address: DEFAULT_API_ADDRESS.to_owned(),
            api_token: generate_token(),
            attachment_limit_bytes: DEFAULT_ATTACHMENT_LIMIT_BYTES,
            post_save_hook: None,
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_owned(),
            source,
        })?;

        let config: Self = toml::from_str(&contents).map_err(|source| Error::ParseConfig {
            path: path.to_owned(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn write(&self, path: &Path, force: bool) -> Result<()> {
        self.validate()?;
        if path.exists() && !force {
            return Err(Error::ConfigAlreadyExists {
                path: path.to_owned(),
            });
        }

        if let Some(parent) = path.parent() {
            create_dir_all(parent)?;
        }

        let contents = toml::to_string_pretty(self)?;
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        set_user_only_mode(&mut options);

        let mut file = options.open(path).map_err(|source| Error::Write {
            path: path.to_owned(),
            source,
        })?;
        file.write_all(contents.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|source| Error::Write {
                path: path.to_owned(),
                source,
            })?;
        enforce_user_only_permissions(path)?;
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if let Some(hook) = &self.post_save_hook {
            if hook.command.trim().is_empty() {
                return Err(Error::InvalidPostSaveHook {
                    message: "command cannot be empty".to_owned(),
                });
            }
            if hook.timeout_seconds == 0 {
                return Err(Error::InvalidPostSaveHook {
                    message: "timeout_seconds must be greater than zero".to_owned(),
                });
            }
        }
        Ok(())
    }
}

const fn default_post_save_hook_timeout_seconds() -> u64 {
    DEFAULT_POST_SAVE_HOOK_TIMEOUT_SECONDS
}

pub fn default_config_path() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        macos_default_config_path(
            env::var_os("XDG_CONFIG_HOME").as_deref(),
            env::var_os("HOME").as_deref(),
        )
    }

    #[cfg(not(target_os = "macos"))]
    {
        let dirs = ProjectDirs::from("org", "lantai", "Lantai")
            .ok_or(Error::ConfigDirectoryUnavailable)?;
        Ok(dirs.config_dir().join("config.toml"))
    }
}

#[cfg(target_os = "macos")]
fn macos_default_config_path(
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf> {
    let directory = xdg_config_home
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|path| !path.is_empty())
                .map(|path| Path::new(path).join(".config"))
        })
        .ok_or(Error::ConfigDirectoryUnavailable)?;

    Ok(directory.join("lantai").join("config.toml"))
}

pub fn resolve_library(cli_library: Option<&Path>, config_path: &Path) -> Result<PathBuf> {
    if let Some(path) = cli_library {
        return absolutize(path);
    }

    if let Some(path) = env::var_os(LIBRARY_ENV) {
        return absolutize(Path::new(&path));
    }

    if !config_path.is_file() {
        return Err(Error::LibraryNotConfigured);
    }
    let config = Config::load(config_path)?;
    absolutize(&config.library)
}

pub fn absolutize(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_owned());
    }

    let cwd = env::current_dir().map_err(|source| Error::Read {
        path: PathBuf::from("."),
        source,
    })?;
    Ok(cwd.join(path))
}

pub fn create_dir_all(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|source| Error::CreateDirectory {
        path: path.to_owned(),
        source,
    })
}

fn generate_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

#[cfg(unix)]
fn set_user_only_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_user_only_mode(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn enforce_user_only_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| Error::Write {
        path: path.to_owned(),
        source,
    })
}

#[cfg(not(unix))]
fn enforce_user_only_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_config_has_expected_defaults_and_random_token() {
        let first = Config::new(PathBuf::from("references.bib"));
        let second = Config::new(PathBuf::from("references.bib"));

        assert_eq!(first.api_address, DEFAULT_API_ADDRESS);
        assert_eq!(first.attachment_root, None);
        assert_eq!(first.attachment_limit_bytes, 512 * 1024 * 1024);
        assert_eq!(first.post_save_hook, None);
        assert_eq!(first.api_token.len(), 64);
        assert_ne!(first.api_token, second.api_token);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_default_config_path_uses_xdg_then_home_fallback() {
        assert_eq!(
            macos_default_config_path(
                Some(OsStr::new("/tmp/xdg-config")),
                Some(OsStr::new("/Users/example")),
            )
            .unwrap(),
            PathBuf::from("/tmp/xdg-config/lantai/config.toml")
        );
        assert_eq!(
            macos_default_config_path(Some(OsStr::new("")), Some(OsStr::new("/Users/example")),)
                .unwrap(),
            PathBuf::from("/Users/example/.config/lantai/config.toml")
        );
        assert!(matches!(
            macos_default_config_path(None, None),
            Err(Error::ConfigDirectoryUnavailable)
        ));
    }

    #[test]
    fn config_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let config = Config::new(directory.path().join("references.bib"));

        config.write(&path, false).unwrap();

        assert_eq!(Config::load(&path).unwrap(), config);
    }

    #[test]
    fn legacy_config_without_attachment_root_still_loads() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            concat!(
                "library = \"/tmp/references.bib\"\n",
                "api_address = \"127.0.0.1:23120\"\n",
                "api_token = \"token\"\n",
                "attachment_limit_bytes = 1024\n"
            ),
        )
        .unwrap();

        assert_eq!(Config::load(&path).unwrap().attachment_root, None);
        assert_eq!(Config::load(&path).unwrap().post_save_hook, None);
    }

    #[test]
    fn post_save_hook_round_trips_and_defaults_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            concat!(
                "library = \"/tmp/references.bib\"\n",
                "api_address = \"127.0.0.1:23120\"\n",
                "api_token = \"token\"\n",
                "attachment_limit_bytes = 1024\n",
                "[post_save_hook]\n",
                "command = \"refresh-index\"\n",
                "args = [\"--all\"]\n"
            ),
        )
        .unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(
            config.post_save_hook,
            Some(PostSaveHookConfig {
                command: "refresh-index".to_owned(),
                args: vec!["--all".to_owned()],
                timeout_seconds: 30,
            })
        );
    }

    #[test]
    fn post_save_hook_rejects_empty_command_and_zero_timeout() {
        let mut config = Config::new(PathBuf::from("references.bib"));
        config.post_save_hook = Some(PostSaveHookConfig {
            command: String::new(),
            args: Vec::new(),
            timeout_seconds: 30,
        });
        let path = tempfile::tempdir().unwrap().path().join("config.toml");
        assert!(matches!(
            config.write(&path, false),
            Err(Error::InvalidPostSaveHook { .. })
        ));
        config.post_save_hook.as_mut().unwrap().command = "hook".to_owned();
        config.post_save_hook.as_mut().unwrap().timeout_seconds = 0;
        assert!(matches!(
            config.write(&path, false),
            Err(Error::InvalidPostSaveHook { .. })
        ));
    }

    #[test]
    fn config_is_not_replaced_without_force() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let first = Config::new(PathBuf::from("first.bib"));
        let second = Config::new(PathBuf::from("second.bib"));
        first.write(&path, false).unwrap();

        assert!(matches!(
            second.write(&path, false),
            Err(Error::ConfigAlreadyExists { .. })
        ));
        assert_eq!(Config::load(&path).unwrap(), first);
    }

    #[cfg(unix)]
    #[test]
    fn config_is_user_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::new(PathBuf::from("references.bib"))
            .write(&path, false)
            .unwrap();

        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
