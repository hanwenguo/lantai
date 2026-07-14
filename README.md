# Lantai

Lantai (兰台, “orchid platform”) is a small, headless reference manager whose
library is a human-editable BibLaTeX file. It accepts captures from the
unmodified Zotero browser Connector and provides a CLI and authenticated local
REST API for managing bibliography entries and attachments.

Lantai 0.2.0 supports:

- source-aware BibLaTeX CRUD, validation, canonical formatting, and export;
- stable item UUIDs, editable citation keys, normalized tags, and managed files;
- safe direct-file CLI access or daemon-backed operation;
- an authenticated, loopback-only REST API with revision preconditions;
- modern Zotero Connector item, PDF, snapshot, standalone-file, and tag flows;
- Git-style custom commands and synchronous post-save hooks.

Lantai intentionally provides one library and no GUI. It does not implement
collections, notes, a PDF/EPUB reader, word-processor integration, cloud sync,
a plugin system, CSL-formatted bibliography or citation output, metadata
recognition, or automatic deduplication. CSL-formatted bibliography export is
part of the intended core scope, but is not available in Lantai 0.2.0. Lantai
is not a Zotero profile or database replacement.

## Install from source

Lantai requires Rust 1.88 or newer:

```sh
git clone https://github.com/hanwenguo/lantai.git
cd lantai
cargo install --path .
lantai --version
```

During development, replace `lantai` below with `cargo run --`.

## Five-minute start

```sh
lantai --library "$HOME/references.bib" init

lantai add --type article \
  --field 'author=Lovelace, Ada' \
  --field date=1843 \
  --field 'title=A Sketch of the Analytical Engine'

lantai list
lantai show lovelace1843sketch
lantai attach lovelace1843sketch ./paper.pdf --mime application/pdf
lantai check
```

`init` prints the bibliography, attachment, and configuration paths. New
libraries use an adjacent `<bibliography-stem>.files/` directory unless
`init --attachments PATH` is supplied.

Run the daemon when using the REST API or browser Connector:

```sh
lantai serve
```

The native API listens on `127.0.0.1:23120`. The Zotero-compatible endpoint
must own `127.0.0.1:23119`, so quit Zotero before starting Lantai. The
unmodified Chromium Manifest V3 Zotero Connector at commit `e168391` has been
acceptance-tested with translated items, PDFs, SingleFile snapshots,
standalone PDFs, and popup tag updates.

## User manual

Start with the [Lantai user manual](docs/index.md):

- [installation and configuration](docs/getting-started.md);
- [library and storage model](docs/library-model.md);
- [complete CLI reference](docs/cli-reference.md);
- [native REST API reference](docs/rest-api.md);
- [Zotero Connector setup](docs/zotero-connector.md);
- [backups, recovery, security, and troubleshooting](docs/operations.md);
- [jq, fzf, and official extension workflows](docs/cli-workflows.md);
- [custom post-save hooks](docs/post-save-hooks.md).

Developer references are kept separately in the [implementation
plan](docs/PLAN.md) and [Zotero Connector protocol analysis](docs/zotero-connector-protocol.md).

## Development verification

```sh
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

The repository-wide contribution and release rules are in `AGENTS.md`.
