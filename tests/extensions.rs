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
const EXTENSIONS: &[&str] = &[
    "lantai-table",
    "lantai-query",
    "lantai-pick",
    "lantai-open",
    "lantai-batch-collection",
    "lantai-api-list",
];

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
    }
}

struct WorkflowFixture {
    _directory: tempfile::TempDir,
    bibliography: PathBuf,
    config: PathBuf,
    path: OsString,
    tools: PathBuf,
    curl_log: PathBuf,
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
                    "  title = {{Other Item}}\n",
                    "}}\n"
                ),
                ITEM_UUID, ATTACHMENT_UUID, ITEM_UUID
            ),
        )
        .unwrap();

        let tools = directory.path().join("tools");
        fs::create_dir(&tools).unwrap();
        write_executable(
            &tools.join("fzf"),
            r#"#!/bin/sh
first=
while IFS= read -r line; do
  if [ -z "$first" ]; then
    first=$line
  fi
done
if [ -n "$first" ]; then
  printf '%s\n' "$first"
  exit 0
fi
exit 1
"#,
        );
        let curl_log = directory.path().join("curl.log");
        write_executable(
            &tools.join("curl"),
            r#"#!/bin/sh
printf '%s\n' "$@" >"$CURL_LOG"
printf '%s\n' '{"items":[{"citation_key":"api"}],"revision":"rest-revision"}'
"#,
        );

        let extension = Path::new(env!("CARGO_MANIFEST_DIR")).join("extension");
        Self {
            config: directory.path().join("unused-config.toml"),
            path: path_with([tools.clone(), extension]),
            tools,
            curl_log,
            bibliography,
            _directory: directory,
        }
    }

    fn run(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_lantai"))
            .args(["--library"])
            .arg(&self.bibliography)
            .args(["--config"])
            .arg(&self.config)
            .args(arguments)
            .env("PATH", &self.path)
            .env("CURL_LOG", &self.curl_log)
            .output()
            .unwrap()
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

    let table = fixture.run(&["table", "rich"]);
    assert!(
        table.status.success(),
        "{}",
        String::from_utf8_lossy(&table.stderr)
    );
    let table = String::from_utf8(table.stdout).unwrap();
    assert!(table.contains("KEY"));
    assert!(table.contains("rich"));
    assert!(table.contains("A Rich Item"));

    let query = fixture.run(&[
        "query",
        "($fields.author // \"\") | test(\"ada\"; \"i\")",
        "--",
        "--type",
        "article",
    ]);
    assert!(
        query.status.success(),
        "{}",
        String::from_utf8_lossy(&query.stderr)
    );
    let queried: Vec<ItemView> = serde_json::from_slice(&query.stdout).unwrap();
    assert_eq!(
        queried
            .iter()
            .map(|item| item.citation_key.as_str())
            .collect::<Vec<_>>(),
        ["rich", "missing"]
    );

    let picked = fixture.run(&["pick", "--id-only", "--", "rich"]);
    assert!(picked.status.success());
    assert_eq!(
        String::from_utf8(picked.stdout).unwrap(),
        format!("{ITEM_UUID}\n")
    );

    let opened = fixture.run(&["open", "--print", "--", "rich"]);
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

    write_executable(
        &fixture.tools.join("fzf"),
        "#!/bin/sh\nwhile IFS= read -r line; do :; done\nexit 130\n",
    );
    for command in [["pick"].as_slice(), ["open"].as_slice()] {
        let cancelled = fixture.run(command);
        assert!(cancelled.status.success());
        assert!(cancelled.stdout.is_empty());
    }

    let refused = fixture.run(&[
        "batch-collection",
        "--apply",
        "blocked",
        ".entry_type == \"article\"",
    ]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("selected item has no UUID"));
    assert!(
        !fixture
            .show("rich")
            .collections
            .contains(&"blocked".to_owned())
    );

    let preview = fixture.run(&["batch-collection", "Reviewed", ".citation_key == \"rich\""]);
    assert!(preview.status.success());
    assert!(String::from_utf8_lossy(&preview.stderr).contains("Preview only"));
    assert!(
        !fixture
            .show("rich")
            .collections
            .contains(&"Reviewed".to_owned())
    );

    let applied = fixture.run(&[
        "batch-collection",
        "--apply",
        "Reviewed",
        ".citation_key == \"rich\"",
    ]);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert!(
        fixture
            .show("rich")
            .collections
            .contains(&"Reviewed".to_owned())
    );

    let api = Command::new(env!("CARGO_BIN_EXE_lantai"))
        .args([
            "api-list",
            "attention",
            "--type",
            "ONLINE",
            "--collection",
            "Keep",
        ])
        .env("PATH", &fixture.path)
        .env("CURL_LOG", &fixture.curl_log)
        .env("LANTAI_TOKEN", "test-token")
        .env("LANTAI_API_URL", "http://127.0.0.1:9999/")
        .output()
        .unwrap();
    assert!(
        api.status.success(),
        "{}",
        String::from_utf8_lossy(&api.stderr)
    );
    let api_body: serde_json::Value = serde_json::from_slice(&api.stdout).unwrap();
    assert_eq!(api_body["revision"], "rest-revision");
    let curl_arguments = fs::read_to_string(&fixture.curl_log).unwrap();
    assert!(curl_arguments.contains("Authorization: Bearer test-token"));
    assert!(curl_arguments.contains("q=attention"));
    assert!(curl_arguments.contains("type=ONLINE"));
    assert!(curl_arguments.contains("collection=Keep"));
    assert!(curl_arguments.contains("http://127.0.0.1:9999/api/v1/items"));

    let missing_token = Command::new(env!("CARGO_BIN_EXE_lantai"))
        .arg("api-list")
        .env("PATH", &fixture.path)
        .env("CURL_LOG", &fixture.curl_log)
        .env_remove("LANTAI_TOKEN")
        .output()
        .unwrap();
    assert_eq!(missing_token.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&missing_token.stderr)
            .contains("lantai api-list: LANTAI_TOKEN is required")
    );
}
