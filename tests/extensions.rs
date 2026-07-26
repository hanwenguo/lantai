#![cfg(unix)]

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use lantai::catalog::ItemView;
use lantai::library::LibraryLayout;

const ITEM_UUID: &str = "cc9e50c4-55ee-4471-b17c-c41684f64bf9";
const ATTACHMENT_UUID: &str = "5025cd5a-ead6-47c0-bb9e-b5399556af98";
const EXTENSIONS: &[&str] = &["lantai-pick", "lantai-open", "lantai-batch-collection"];

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn path_with(entries: impl IntoIterator<Item = PathBuf>) -> OsString {
    let mut paths = entries.into_iter().collect::<Vec<_>>();
    if let Some(existing) = env::var_os("PATH") {
        paths.extend(env::split_paths(&existing));
    }
    env::join_paths(paths).unwrap()
}

#[test]
fn external_subcommand_preserves_arguments_io_environment_and_status() {
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("lantai-probe");
    write_executable(
        &script,
        r#"#!/bin/sh
index=0
for argument do
  printf 'arg[%s]=%s\n' "$index" "$argument"
  index=$((index + 1))
done
IFS= read -r input
printf 'stdin=%s\n' "$input"
printf 'library=%s\n' "$LANTAI_LIBRARY"
printf 'config=%s\n' "$LANTAI_CONFIG"
printf 'lantai=%s\n' "$LANTAI"
printf 'extension stderr\n' >&2
exit 23
"#,
    );

    let library = directory.path().join("library with spaces.bib");
    let config = directory.path().join("config with spaces.toml");
    let mut child = Command::new(env!("CARGO_BIN_EXE_lantai"))
        .args(["--library"])
        .arg(&library)
        .args(["--config"])
        .arg(&config)
        .args(["probe", "alpha beta", "--flag"])
        .env("PATH", directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"input line\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(23));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("arg[0]=alpha beta\narg[1]=--flag\n"));
    assert!(stdout.contains("stdin=input line\n"));
    assert!(stdout.contains(&format!("library={}\n", library.display())));
    assert!(stdout.contains(&format!("config={}\n", config.display())));
    assert!(stdout.contains(&format!("lantai={}\n", env!("CARGO_BIN_EXE_lantai"))));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "extension stderr\n"
    );

    write_executable(
        &directory.path().join("lantai-bytes"),
        "#!/bin/sh\nprintf '%s' \"$1\"\n",
    );
    let raw_argument = vec![b'r', 0x80, b'w'];
    let bytes = Command::new(env!("CARGO_BIN_EXE_lantai"))
        .arg("bytes")
        .arg(OsString::from_vec(raw_argument.clone()))
        .env("PATH", directory.path())
        .output()
        .unwrap();
    assert!(bytes.status.success());
    assert_eq!(bytes.stdout, raw_argument);
}

#[test]
fn extension_lookup_is_path_only_and_built_ins_take_precedence() {
    let directory = tempfile::tempdir().unwrap();
    let empty_path = directory.path().join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    write_executable(
        &directory.path().join("lantai-local"),
        "#!/bin/sh\nprintf 'should not run\\n'\n",
    );

    let missing = Command::new(env!("CARGO_BIN_EXE_lantai"))
        .arg("local")
        .current_dir(directory.path())
        .env("PATH", &empty_path)
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&missing.stderr)
            .contains("\"local\" is not a lantai command; install `lantai-local` on PATH")
    );

    let invalid = Command::new(env!("CARGO_BIN_EXE_lantai"))
        .arg("nested/name")
        .env("PATH", &empty_path)
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&invalid.stderr)
            .contains("invalid custom subcommand name: \"nested/name\"")
    );

    fs::write(empty_path.join("lantai-denied"), "not executable\n").unwrap();
    let denied = Command::new(env!("CARGO_BIN_EXE_lantai"))
        .arg("denied")
        .env("PATH", &empty_path)
        .output()
        .unwrap();
    assert_eq!(denied.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&denied.stderr)
            .contains("failed to launch custom subcommand lantai-denied")
    );

    write_executable(
        &empty_path.join("lantai-list"),
        "#!/bin/sh\nprintf 'external list ran\\n'\nexit 77\n",
    );
    let built_in = Command::new(env!("CARGO_BIN_EXE_lantai"))
        .args(["list", "--help"])
        .env("PATH", &empty_path)
        .output()
        .unwrap();
    assert!(built_in.status.success());
    assert!(String::from_utf8_lossy(&built_in.stdout).contains("List bibliography entries"));
    assert!(!String::from_utf8_lossy(&built_in.stdout).contains("external list ran"));
}

#[test]
fn official_extensions_are_executable_syntax_valid_and_self_documenting() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for name in EXTENSIONS {
        let path = root.join("extension").join(name);
        assert_ne!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o111,
            0,
            "{} is not executable",
            path.display()
        );
        let syntax = Command::new("bash")
            .args(["-n"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            syntax.status.success(),
            "{}",
            String::from_utf8_lossy(&syntax.stderr)
        );
        let help = Command::new(&path).arg("--help").output().unwrap();
        assert!(
            help.status.success(),
            "{}",
            String::from_utf8_lossy(&help.stderr)
        );
        assert!(String::from_utf8_lossy(&help.stdout).starts_with("Usage: lantai "));

        // The marker is what `lantai --help` shows; without it the command is
        // listed with no hint of what it does.
        let source = fs::read_to_string(&path).unwrap();
        let about = source
            .lines()
            .find_map(|line| {
                line.trim_start()
                    .strip_prefix('#')?
                    .trim_start()
                    .strip_prefix("lantai-about:")
            })
            .unwrap_or_else(|| panic!("{} declares no # lantai-about:", path.display()))
            .trim();
        assert!(!about.is_empty(), "{} has an empty description", name);
    }
}

struct WorkflowFixture {
    _directory: tempfile::TempDir,
    bibliography: PathBuf,
    config: PathBuf,
    path: OsString,
    tools: PathBuf,
}

impl WorkflowFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();
        layout.initialize().unwrap();
        fs::write(
            &bibliography,
            format!(
                concat!(
                    "@article{{rich,\n",
                    "  title = {{A Rich Item}},\n",
                    "  author = {{Ada Lovelace}},\n",
                    "  year = {{1843}},\n",
                    "  keywords = {{needs-review}},\n",
                    "  file = {{PDF:references.files/{}/{:}-paper.pdf:application/pdf}},\n",
                    "  lantaiid = {{{}}}\n",
                    "}}\n",
                    "@article{{missing,\n",
                    "  title = {{Missing Identity}},\n",
                    "  keywords = {{needs-review}},\n",
                    "  author = {{Ada Lovelace}}\n",
                    "}}\n",
                    "@book{{other,\n",
                    "  title = {{Other Item}},\n",
                    "  date = {{2019-07}},\n",
                    "  lantaiid = {{5a45466b-d74f-4072-b026-dad615c7dcec}}\n",
                    "}}\n"
                ),
                ITEM_UUID, ATTACHMENT_UUID, ITEM_UUID
            ),
        )
        .unwrap();

        let tools = directory.path().join("tools");
        fs::create_dir(&tools).unwrap();
        // The picker is multi-select, so the stub takes everything it is
        // offered; tests that want one item narrow the query instead.
        write_executable(&tools.join("fzf"), "#!/bin/sh\ncat\n");

        let extension = Path::new(env!("CARGO_MANIFEST_DIR")).join("extension");
        Self {
            config: directory.path().join("unused-config.toml"),
            path: path_with([tools.clone(), extension]),
            tools,
            bibliography,
            _directory: directory,
        }
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command()
            .args(["--config"])
            .arg(&self.config)
            .args(arguments)
            .output()
            .unwrap()
    }

    /// Run without `--config`, which is how every real invocation looks: the
    /// extensions then see no `LANTAI_CONFIG`, and their argument arrays are
    /// empty.
    fn run_without_config(&self, arguments: &[&str]) -> Output {
        self.command().args(arguments).output().unwrap()
    }

    fn command(&self) -> Command {
        let home = self._directory.path();
        let mut command = Command::new(env!("CARGO_BIN_EXE_lantai"));
        command
            .args(["--library"])
            .arg(&self.bibliography)
            .env("PATH", &self.path)
            // Runs without --config fall back to the default configuration
            // path; point that inside the fixture rather than at whatever the
            // developer running the tests has installed.
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home);
        command
    }

    fn show(&self, id: &str) -> ItemView {
        let output = self.run(&["show", id]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn keys(output: &Output) -> Vec<String> {
    let items: Vec<ItemView> = serde_json::from_slice(&output.stdout).unwrap();
    items.into_iter().map(|item| item.citation_key).collect()
}

fn jq_is_available() -> bool {
    Command::new("jq")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

#[test]
fn official_extensions_execute_the_documented_workflows() {
    if !jq_is_available() {
        eprintln!("skipping workflow smoke test because jq is not installed");
        return;
    }

    let fixture = WorkflowFixture::new();

    let picked = fixture.run(&["pick", "--id-only", "key:rich"]);
    assert!(
        picked.status.success(),
        "{}",
        String::from_utf8_lossy(&picked.stderr)
    );
    assert_eq!(
        String::from_utf8(picked.stdout).unwrap(),
        format!("{ITEM_UUID}\n")
    );

    let picked = fixture.run(&["pick", "key:rich"]);
    let selected: Vec<ItemView> = serde_json::from_slice(&picked.stdout).unwrap();
    assert_eq!(
        selected,
        [fixture.show("rich")],
        "whole items, not summaries"
    );

    // The picker's terms are the list language, and several selections come
    // back in bibliography order however the picker offered them.
    let picked = fixture.run(&["pick", "author:lovelace", "collection:needs-review"]);
    assert_eq!(keys(&picked), ["rich", "missing"]);
    let picked = fixture.run(&["pick", "--", "-year:"]);
    assert_eq!(keys(&picked), ["missing"], "negation survives the handoff");

    let empty = fixture.run(&["pick", "key:no-such-item"]);
    assert!(empty.status.success());
    assert!(empty.stdout.is_empty(), "an empty result never opens fzf");

    let opened = fixture.run(&["open", "--print", "key:rich"]);
    assert!(
        opened.status.success(),
        "{}",
        String::from_utf8_lossy(&opened.stderr)
    );
    let expected_path = fixture.bibliography.parent().unwrap().join(format!(
        "references.files/{ITEM_UUID}/{ATTACHMENT_UUID}-paper.pdf"
    ));
    assert_eq!(
        String::from_utf8(opened.stdout).unwrap(),
        format!("{}\n", expected_path.display())
    );

    let selected = fixture.run(&["batch-collection", "Reviewed", "key:rich"]);
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    assert!(
        fixture
            .show("rich")
            .collections
            .contains(&"Reviewed".to_owned())
    );

    let swept = fixture.run(&["batch-collection", "--all", "Swept", "type:book"]);
    assert!(
        swept.status.success(),
        "{}",
        String::from_utf8_lossy(&swept.stderr)
    );
    assert!(
        fixture
            .show("other")
            .collections
            .contains(&"Swept".to_owned())
    );
    let unswept = fixture.run(&[
        "batch-collection",
        "--remove",
        "--all",
        "Swept",
        "type:book",
    ]);
    assert!(
        unswept.status.success(),
        "{}",
        String::from_utf8_lossy(&unswept.stderr)
    );
    assert!(
        !fixture
            .show("other")
            .collections
            .contains(&"Swept".to_owned())
    );

    // An entry with no UUID cannot be addressed, so the whole batch stops
    // rather than half-applying.
    let refused = fixture.run(&["batch-collection", "--all", "blocked", "author:lovelace"]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("no UUID"));
    assert!(
        !fixture
            .show("rich")
            .collections
            .contains(&"blocked".to_owned())
    );

    let none = fixture.run(&["batch-collection", "--all", "blocked", "key:no-such-item"]);
    assert!(none.status.success());
    assert!(String::from_utf8_lossy(&none.stderr).contains("No matching items"));

    // Cancelling the picker is not a failure, and nothing downstream of it runs.
    write_executable(
        &fixture.tools.join("fzf"),
        "#!/bin/sh\nwhile IFS= read -r line; do :; done\nexit 130\n",
    );
    for command in [
        ["pick"].as_slice(),
        ["open"].as_slice(),
        ["batch-collection", "Cancelled"].as_slice(),
    ] {
        let cancelled = fixture.run(command);
        assert!(
            cancelled.status.success(),
            "{command:?}: {}",
            String::from_utf8_lossy(&cancelled.stderr)
        );
        assert!(cancelled.stdout.is_empty(), "{command:?}");
    }
    assert!(
        !fixture
            .show("rich")
            .collections
            .contains(&"Cancelled".to_owned())
    );
}

/// Only `--config` runs put `LANTAI_CONFIG` in an extension's environment, so
/// the ordinary invocation is the one where every forwarded argument list is
/// empty — which older shells treat as unset.
#[test]
fn official_extensions_run_without_a_configuration_override() {
    if !jq_is_available() {
        eprintln!("skipping workflow smoke test because jq is not installed");
        return;
    }

    let fixture = WorkflowFixture::new();
    for command in [
        ["pick", "--id-only"].as_slice(),
        ["open", "--print"].as_slice(),
        ["batch-collection", "--all", "Reviewed", "key:rich"].as_slice(),
    ] {
        let output = fixture.run_without_config(command);
        assert!(
            output.status.success(),
            "{command:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        fixture
            .show("rich")
            .collections
            .contains(&"Reviewed".to_owned())
    );
}
