use std::path::PathBuf;
use std::process::{Command, Output};

use lantai::catalog::ItemView;
use lantai::config::{Config, PostSaveHookConfig};
use lantai::library::LibraryLayout;

const ITEM_UUID: &str = "cc9e50c4-55ee-4471-b17c-c41684f64bf9";

struct Fixture {
    _directory: tempfile::TempDir,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();
        layout.initialize().unwrap();
        std::fs::write(
            &bibliography,
            format!(
                concat!(
                    "@article{{rich,\n",
                    "  title = {{A Rich Item}},\n",
                    "  author = \"Ada \" # {{Lovelace}},\n",
                    "  keywords = {{history, Computing}},\n",
                    "  file = {{:/tmp/paper.pdf:application/pdf}},\n",
                    "  lantaiid = {{{}}}\n",
                    "}}\n"
                ),
                ITEM_UUID
            ),
        )
        .unwrap();

        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = probe.local_addr().unwrap();
        drop(probe);
        let config_path = directory.path().join("config.toml");
        let mut config = Config::new(bibliography);
        config.api_address = address.to_string();
        config.write(&config_path, false).unwrap();

        Self {
            _directory: directory,
            config: config_path,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_lantai"))
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .output()
            .unwrap()
    }
}

fn stdout(output: &Output) -> &str {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    std::str::from_utf8(&output.stdout).unwrap()
}

#[test]
fn list_and_show_default_to_complete_json_views() {
    let fixture = Fixture::new();

    let output = fixture.run(&["list"]);
    let listed: Vec<ItemView> = serde_json::from_str(stdout(&output)).unwrap();
    assert_eq!(listed.len(), 1);
    let item = &listed[0];
    assert_eq!(item.uuid.unwrap().to_string(), ITEM_UUID);
    assert_eq!(item.title.as_deref(), Some("A Rich Item"));
    assert_eq!(item.collections, ["Computing", "history"]);
    assert_eq!(item.attachments.len(), 1);
    assert_eq!(item.attachments[0].uuid, None);
    assert_eq!(item.attachments[0].title, None);
    assert_eq!(item.attachments[0].path, "/tmp/paper.pdf");
    assert_eq!(
        item.fields
            .iter()
            .find(|field| field.name == "author")
            .and_then(|field| field.raw.as_deref()),
        Some("\"Ada \" # {Lovelace}")
    );

    let output = fixture.run(&["show", "rich"]);
    let shown: ItemView = serde_json::from_str(stdout(&output)).unwrap();
    assert_eq!(shown, *item);

    let output = fixture.run(&["list", "does-not-match"]);
    assert_eq!(stdout(&output), "[]\n");
}

/// JSON lists the complete names `--collection` takes back, including the
/// ancestor the tree synthesizes; human mode nests them instead.
#[test]
fn collection_list_defaults_to_flat_json_paths() {
    let fixture = Fixture::new();
    stdout(&fixture.run(&["collection", "add", "rich", "Projects/IfT"]));

    let output = fixture.run(&["collection", "list"]);
    let listed: Vec<String> = serde_json::from_str(stdout(&output)).unwrap();
    assert_eq!(listed, ["Computing", "history", "Projects", "Projects/IfT"]);

    let output = fixture.run(&["collection", "list", "--format", "human"]);
    assert_eq!(
        stdout(&output),
        "Computing\nhistory\nProjects\n  IfT\n".to_owned()
    );
}

#[test]
fn human_format_remains_available_without_a_json_alias() {
    let fixture = Fixture::new();

    let output = fixture.run(&["list", "--format", "human"]);
    assert_eq!(stdout(&output), "rich\tarticle\tA Rich Item\n");

    let output = fixture.run(&["show", "rich", "--format", "human"]);
    let shown = stdout(&output);
    assert!(shown.starts_with("@article{rich}\nUUID: "));
    assert!(shown.contains("title: A Rich Item\n"));
    assert!(shown.contains("author: Ada Lovelace\n"));

    for args in [
        ["list", "--json"].as_slice(),
        ["show", "rich", "--json"].as_slice(),
        ["collection", "list", "--json"].as_slice(),
    ] {
        let output = fixture.run(args);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument '--json'"));
    }
}

#[cfg(unix)]
#[test]
fn direct_post_save_hook_preserves_json_stdout_and_suppresses_nested_hooks() {
    let directory = tempfile::tempdir().unwrap();
    let bibliography = directory.path().join("references.bib");
    let layout = LibraryLayout::new(bibliography.clone()).unwrap();
    layout.initialize().unwrap();
    std::fs::write(
        &bibliography,
        format!("@article{{rich,title={{Before}},lantaiid={{{ITEM_UUID}}}}}\n"),
    )
    .unwrap();
    let event = directory.path().join("event.json");
    let calls = directory.path().join("calls");
    let config_path = directory.path().join("config.toml");
    let mut config = Config::new(directory.path().join("not-the-selected-library.bib"));
    config.post_save_hook = Some(PostSaveHookConfig {
        command: "/bin/sh".to_owned(),
        args: vec![
            "-c".to_owned(),
            concat!(
                "cat > \"$1\"; ",
                "\"$LANTAI\" --config \"$LANTAI_CONFIG\" ",
                "collection add rich nested --json; ",
                "printf x >> \"$2\""
            )
            .to_owned(),
            "lantai-hook".to_owned(),
            event.display().to_string(),
            calls.display().to_string(),
        ],
        timeout_seconds: 30,
    });
    config.write(&config_path, false).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_lantai"))
        .arg("--config")
        .arg(&config_path)
        .args(["set", "rich", "title=Changed", "--json"])
        .env("LANTAI_LIBRARY", &bibliography)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["uuid"], ITEM_UUID);
    let hook_event: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(event).unwrap()).unwrap();
    assert_eq!(hook_event["operation"], "item.update");
    assert_eq!(hook_event["origin"], "cli");
    assert_eq!(std::fs::read_to_string(calls).unwrap(), "x");

    let shown = Command::new(env!("CARGO_BIN_EXE_lantai"))
        .arg("--config")
        .arg(&config_path)
        .args(["show", "rich"])
        .env("LANTAI_LIBRARY", &bibliography)
        .output()
        .unwrap();
    let shown: ItemView = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown.collections, ["nested"]);
}
