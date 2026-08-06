# Lantai

Lantai is a small, headless reference manager whose library is a human-editable
BibLaTeX file. It accepts captures from the unmodified Zotero browser Connector
and provides a CLI and authenticated local REST API for managing bibliography
entries and attachments.

About the name: 兰台 (lán tái, orchid platform) is the name of the Royal
Library/Imperial Archives of the Han Dynasty of ancient China.

Lantai supports:

- source-aware BibLaTeX CRUD, validation, canonical formatting, and export;
- a term-based query language with sorting, shared by the CLI and the REST API;
- stable item UUIDs, editable citation keys, normalized collections, and managed files;
- safe direct-file CLI access or daemon-backed operation;
- an authenticated, loopback-only REST API with revision preconditions;
- modern Zotero Connector item, PDF, snapshot, standalone-file, and collection flows;
- bulk import of a Zotero RDF export, with its files and collections;
- Git-style custom commands and synchronous post-save hooks;
- a coding-agent skill for searching, citing from, and maintaining the library.

Lantai intentionally provides one library and no GUI. Collections are one flat
namespace stored in BibLaTeX `keywords`, nested by spelling them with `/`;
there is no separate collection object, no per-collection metadata, and no
second notion of a "tag". Lantai does not implement notes, a PDF/EPUB reader,
word-processor integration, cloud sync, a plugin system, CSL-formatted
bibliography or citation output, metadata recognition, or automatic
deduplication. Importing a Zotero library maps its collection tree onto that
namespace and imports neither its own tags nor its notes; browser capture
likewise discards the keywords a translator scrapes. CSL-formatted
bibliography export is part of the intended core scope, but is not available in
Lantai 0.6.1. Lantai is not a Zotero profile or database replacement.

## Installation

### Homebrew

```sh
brew install hanwenguo/tap/lantai
```

The tap builds Lantai from the tagged source release and provides bottles for
Apple Silicon on macOS 26, and for ARM64 and x86-64 Linux. Intel macOS is not
supported.

### Install from source

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
lantai init

lantai add --type article \
  --field 'author=Lovelace, Ada' \
  --field date=1843 \
  --field 'title=A Sketch of the Analytical Engine'

lantai list
lantai show Lov43
lantai attach Lov43 ./paper.pdf --mime application/pdf
lantai check
```

`init` asks where the bibliography should live, defaults its attachments to an
adjacent `<bibliography-stem>.files/` directory, and prints the three paths it
chose. Pass `--library` and `--attachments` to answer in advance instead, which
is also what scripts should do.

To start from an existing Zotero library, export it as **Zotero RDF** with
**Export Files** enabled, then:

```sh
lantai import "My Library.rdf" --dry-run
lantai import "My Library.rdf"
```

Nested collections become `/`-joined names such as `Projects/Engines`,
attachment files are copied into managed storage, and Zotero's citation keys
are reused.

Run the daemon when using the REST API or browser Connector:

```sh
lantai serve
```

The native API listens on `127.0.0.1:23120`. The Zotero-compatible endpoint
must own `127.0.0.1:23119`, so quit Zotero before starting Lantai. The
unmodified Chromium Manifest V3 Zotero Connector at commit `e168391` has been
acceptance-tested with translated items, PDFs, SingleFile snapshots,
standalone PDFs, and popup collection updates.

## User manual

Start with the [Lantai user manual](docs/index.md):

- [installation and first steps](docs/getting-started.md);
- [configuration](docs/configuration.md);
- [library and storage model](docs/library-model.md);
- [complete CLI reference](docs/cli-reference.md);
- [native REST API reference](docs/rest-api.md);
- [Zotero Connector setup](docs/zotero-connector.md);
- [backups, recovery, security, and troubleshooting](docs/operations.md);
- [searching, the interactive picker, and extension workflows](docs/cli-workflows.md);
- [custom post-save hooks](docs/post-save-hooks.md);
- [the agent skill](skills/README.md).

Developer references are kept separately in the [implementation
plan](docs/PLAN.md) and [Zotero Connector protocol analysis](docs/zotero-connector-protocol.md).

## Development verification

```sh
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

GitHub Actions runs the same three checks on Linux and macOS for every push to
`main` and every pull request, and separately type-checks the crate against
the minimum supported Rust version declared in `Cargo.toml`. Pushing a `vX.Y.Z`
tag whose version matches `Cargo.toml` repeats those checks, then builds
`x86_64` and `aarch64` binaries for Linux and an `aarch64` binary for macOS 26.
It publishes them, with SHA-256 checksums, as a GitHub release.

The repository-wide contribution and release rules are in `AGENTS.md`.
