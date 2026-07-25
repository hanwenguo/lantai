use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lantai::catalog::ItemView;
use lantai::config::Config;
use lantai::library::LibraryLayout;

const RDF: &str = concat!(
    "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"",
    " xmlns:z=\"http://www.zotero.org/namespaces/export#\"",
    " xmlns:dc=\"http://purl.org/dc/elements/1.1/\"",
    " xmlns:dcterms=\"http://purl.org/dc/terms/\"",
    " xmlns:bib=\"http://purl.org/net/biblio#\"",
    " xmlns:foaf=\"http://xmlns.com/foaf/0.1/\"",
    " xmlns:link=\"http://purl.org/rss/1.0/modules/link/\"",
    " xmlns:prism=\"http://prismstandard.org/namespaces/1.2/basic/\">",
    "<rdf:Description rdf:about=\"https://example.org/paper\">",
    "<z:itemType>conferencePaper</z:itemType>",
    "<dcterms:isPartOf><bib:Journal>",
    "<dc:title>Proceedings of Everything</dc:title>",
    "<dc:identifier>DOI 10.1145/1</dc:identifier>",
    "</bib:Journal></dcterms:isPartOf>",
    "<bib:authors><rdf:Seq><rdf:li><foaf:Person>",
    "<foaf:surname>Lovelace</foaf:surname><foaf:givenName>Ada</foaf:givenName>",
    "</foaf:Person></rdf:li></rdf:Seq></bib:authors>",
    "<dc:title>A Sketch of the Analytical Engine</dc:title>",
    "<dc:date>October 27, 2024</dc:date>",
    "<bib:pages>10\u{2013}20</bib:pages>",
    "<z:citationKey>lovelace-sketch-2024</z:citationKey>",
    "<dc:subject>zotero-tag</dc:subject>",
    "<link:link rdf:resource=\"#item_10\"/>",
    "<link:link rdf:resource=\"#item_11\"/>",
    "</rdf:Description>",
    "<bib:Book rdf:about=\"#item_2\">",
    "<z:itemType>book</z:itemType>",
    "<dc:title>An Uncollected Book</dc:title>",
    "<dc:date>2019</dc:date>",
    "<z:citationKey>rich</z:citationKey>",
    "</bib:Book>",
    "<z:Attachment rdf:about=\"#item_10\">",
    "<z:itemType>attachment</z:itemType>",
    "<z:path rdf:resource=\"files/10/sketch.pdf\"/>",
    "<dc:title>Full Text PDF</dc:title>",
    "<z:linkMode>2</z:linkMode>",
    "<link:type>application/pdf</link:type>",
    "</z:Attachment>",
    "<z:Attachment rdf:about=\"#item_11\">",
    "<z:itemType>attachment</z:itemType>",
    "<dc:title>Publisher Link</dc:title>",
    "<z:linkMode>3</z:linkMode>",
    "<link:type>text/html</link:type>",
    "</z:Attachment>",
    "<z:Collection rdf:about=\"#collection_1\">",
    "<dc:title>Projects</dc:title>",
    "<dcterms:hasPart rdf:resource=\"#collection_2\"/>",
    "</z:Collection>",
    "<z:Collection rdf:about=\"#collection_2\">",
    "<dc:title>Engines</dc:title>",
    "<dcterms:hasPart rdf:resource=\"https://example.org/paper\"/>",
    "</z:Collection>",
    "</rdf:RDF>",
);

struct Fixture {
    _directory: tempfile::TempDir,
    config: PathBuf,
    bibliography: PathBuf,
    export: PathBuf,
}

impl Fixture {
    /// A library holding one entry that already owns the citation key `rich`.
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let layout = LibraryLayout::new(bibliography.clone()).unwrap();
        layout.initialize().unwrap();
        std::fs::write(
            &bibliography,
            concat!(
                "@article{rich,\n",
                "  title = {An Existing Item},\n",
                "  lantaiid = {cc9e50c4-55ee-4471-b17c-c41684f64bf9}\n",
                "}\n"
            ),
        )
        .unwrap();

        let export = directory.path().join("export");
        std::fs::create_dir_all(export.join("files").join("10")).unwrap();
        std::fs::write(
            export.join("files").join("10").join("sketch.pdf"),
            b"%PDF-1.4\n",
        )
        .unwrap();
        std::fs::write(export.join("library.rdf"), RDF).unwrap();

        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = probe.local_addr().unwrap();
        drop(probe);
        let config_path = directory.path().join("config.toml");
        let mut config = Config::new(bibliography.clone());
        config.api_address = address.to_string();
        config.write(&config_path, false).unwrap();

        Self {
            _directory: directory,
            config: config_path,
            bibliography,
            export,
        }
    }

    fn rdf(&self) -> PathBuf {
        self.export.join("library.rdf")
    }

    fn attachments(&self) -> PathBuf {
        self.bibliography.with_extension("files")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_lantai"))
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .output()
            .unwrap()
    }

    fn import(&self, extra: &[&str]) -> Output {
        let rdf = self.rdf();
        let mut args = vec!["import", rdf.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    fn items(&self) -> Vec<ItemView> {
        serde_json::from_str(stdout(&self.run(&["list"]))).unwrap()
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

fn find<'a>(items: &'a [ItemView], key: &str) -> &'a ItemView {
    items
        .iter()
        .find(|item| item.citation_key == key)
        .unwrap_or_else(|| panic!("no item with citation key {key}"))
}

fn field<'a>(item: &'a ItemView, name: &str) -> Option<&'a str> {
    item.fields
        .iter()
        .find(|field| field.name == name)
        .map(|field| field.value.as_str())
}

#[test]
fn import_creates_items_files_and_collection_tags() {
    let fixture = Fixture::new();

    let output = fixture.import(&["--json"]);
    let summary: serde_json::Value = serde_json::from_str(stdout(&output)).unwrap();
    assert_eq!(summary["imported"], 2);
    assert_eq!(summary["attachments"], 1);
    assert_eq!(
        summary["collections"],
        serde_json::json!(["Projects", "Projects/Engines"])
    );
    assert_eq!(summary["items"].as_array().unwrap().len(), 2);

    let items = fixture.items();
    assert_eq!(items.len(), 3, "the pre-existing entry is preserved");

    let paper = find(&items, "lovelace-sketch-2024");
    assert_eq!(paper.entry_type, "inproceedings");
    assert_eq!(field(paper, "booktitle"), Some("Proceedings of Everything"));
    assert_eq!(
        field(paper, "doi"),
        Some("10.1145/1"),
        "the DOI comes from the container"
    );
    assert_eq!(field(paper, "pages"), Some("10--20"));
    assert_eq!(
        field(paper, "date"),
        Some("2024-10-27"),
        "locale dates become ISO"
    );
    assert_eq!(field(paper, "author"), Some("Lovelace, Ada"));
    assert_eq!(
        paper.tags,
        ["Projects/Engines"],
        "Zotero tags are not imported"
    );

    assert_eq!(paper.attachments.len(), 1);
    let attachment = &paper.attachments[0];
    assert_eq!(attachment.title.as_deref(), Some("Full Text PDF"));
    assert_eq!(attachment.media_type, "application/pdf");
    let stored = fixture
        .attachments()
        .join(paper.uuid.unwrap().to_string())
        .join(format!(
            "{}-sketch.pdf",
            attachment.uuid.expect("a managed attachment has a UUID")
        ));
    assert!(stored.is_file(), "{} was not copied", stored.display());
    assert_eq!(std::fs::read(stored).unwrap(), b"%PDF-1.4\n");
}

#[test]
fn a_taken_citation_key_falls_back_to_a_generated_one() {
    let fixture = Fixture::new();
    stdout(&fixture.import(&["--json"]));

    let items = fixture.items();
    let book = items
        .iter()
        .find(|item| item.title.as_deref() == Some("An Uncollected Book"))
        .expect("the book was imported");
    assert_ne!(
        book.citation_key, "rich",
        "the existing entry keeps the key"
    );
    assert_eq!(book.citation_key, "anon2019uncollected");
    assert!(book.tags.is_empty(), "an item in no collection has no tags");

    let existing = find(&items, "rich");
    assert_eq!(existing.title.as_deref(), Some("An Existing Item"));
}

#[test]
fn a_citation_key_lantai_rejects_falls_back_to_a_generated_one() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.rdf(),
        RDF.replace("lovelace-sketch-2024", "lovelace{sketch}2024"),
    )
    .unwrap();
    stdout(&fixture.import(&["--json"]));

    let items = fixture.items();
    let paper = items
        .iter()
        .find(|item| item.title.as_deref() == Some("A Sketch of the Analytical Engine"))
        .expect("the paper was imported");
    assert_eq!(paper.citation_key, "lovelace2024sketch");
}

#[test]
fn attachments_without_files_are_reported_rather_than_failing() {
    let fixture = Fixture::new();

    let output = fixture.import(&["--json"]);
    let summary: serde_json::Value = serde_json::from_str(stdout(&output)).unwrap();
    let skipped = summary["skipped_attachments"].as_array().unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["item"], "lovelace-sketch-2024");
    assert_eq!(skipped[0]["title"], "Publisher Link");
    assert!(
        skipped[0]["reason"].as_str().unwrap().contains("no file"),
        "unexpected reason: {}",
        skipped[0]["reason"]
    );
}

#[test]
fn a_dry_run_reports_the_import_without_changing_the_library() {
    let fixture = Fixture::new();
    let before = std::fs::read_to_string(&fixture.bibliography).unwrap();

    let output = fixture.import(&["--dry-run"]);
    let report = stdout(&output);
    assert!(report.starts_with("Would import 2 item(s) and 1 file(s) from 2 collection(s)"));

    assert_eq!(
        std::fs::read_to_string(&fixture.bibliography).unwrap(),
        before
    );
    assert_eq!(fixture.items().len(), 1);
    assert!(!fixture.attachments().join("files").exists());
}

#[test]
fn a_linked_base_directory_attachment_resolves_with_attachment_base() {
    let fixture = Fixture::new();
    let base = fixture.export.parent().unwrap().join("zotero-base");
    std::fs::create_dir_all(base.join("Papers")).unwrap();
    std::fs::write(base.join("Papers").join("linked.pdf"), b"%PDF-1.7\n").unwrap();
    std::fs::write(
        fixture.rdf(),
        RDF.replace(
            "<z:path rdf:resource=\"files/10/sketch.pdf\"/>",
            "<z:path rdf:resource=\"attachments:Papers/linked.pdf\"/>",
        ),
    )
    .unwrap();

    let refused: serde_json::Value =
        serde_json::from_str(stdout(&fixture.import(&["--dry-run", "--json"]))).unwrap();
    assert_eq!(refused["attachments"], 0);
    assert!(
        refused["skipped_attachments"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("--attachment-base")
    );

    let resolved: serde_json::Value = serde_json::from_str(stdout(&fixture.import(&[
        "--attachment-base",
        base.to_str().unwrap(),
        "--json",
    ])))
    .unwrap();
    assert_eq!(resolved["attachments"], 1);

    let items = fixture.items();
    let paper = find(&items, "lovelace-sketch-2024");
    assert_eq!(paper.attachments.len(), 1);
    assert!(paper.attachments[0].path.ends_with("-linked.pdf"));
}

#[test]
fn a_malformed_export_leaves_the_library_untouched() {
    let fixture = Fixture::new();
    let before = std::fs::read_to_string(&fixture.bibliography).unwrap();
    std::fs::write(fixture.rdf(), "<rdf:RDF>").unwrap();

    let output = fixture.import(&[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("failed to parse Zotero RDF"));
    assert_eq!(
        std::fs::read_to_string(&fixture.bibliography).unwrap(),
        before
    );
}

#[test]
fn import_is_rejected_when_the_export_has_no_items() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.rdf(),
        RDF.replace("<z:itemType>conferencePaper</z:itemType>", "")
            .replace("<z:itemType>book</z:itemType>", ""),
    )
    .unwrap();

    let output = fixture.import(&[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("contains no items"));
}

#[test]
fn an_imported_library_passes_check_and_formats_idempotently() {
    let fixture = Fixture::new();
    stdout(&fixture.import(&["--json"]));

    let report = stdout(&fixture.run(&["check"])).to_owned();
    assert!(
        report.starts_with("3 entries, 0 warnings, 0 errors"),
        "{report}"
    );

    stdout(&fixture.run(&["format"]));
    let once = std::fs::read_to_string(&fixture.bibliography).unwrap();
    stdout(&fixture.run(&["format"]));
    assert_eq!(
        std::fs::read_to_string(&fixture.bibliography).unwrap(),
        once
    );
}

#[test]
fn the_import_path_must_exist() {
    let fixture = Fixture::new();
    let missing = fixture.export.join("absent.rdf");
    let output = fixture.run(&["import", missing.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("failed to read"));
    assert!(Path::new(&fixture.bibliography).is_file());
}
