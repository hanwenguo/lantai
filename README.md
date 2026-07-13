I'm planning to create a slopware (which is fine since it's only for my own use for now) that acts as a headless Zotero only keeping the core features (connect to browser, bibliography management, file attachments, export bibtex, export bibliography formatted with CSL). No notes, no PDF/epub reader, no Word integration, no plugins, no cloud, no GUI (interact using CLI or local server endpoints).

About the name: 兰台 (lán tái, orchid platform) is the name of the Royal Library/Imperial Archives of the Han Dynasty.

## Current implementation

Lantai 0.1.0 is a working Rust implementation. It keeps the bibliography
as a human-editable BibLaTeX file while providing:

- locked, atomic item and tag CRUD with stable UUIDs and generated citation keys;
- source-aware raw-field preservation, canonical formatting, validation, search,
  and export;
- adjacent or separately configured managed attachments, crash-safe
  detach/delete-to-trash behavior, and
  attachment integrity checks;
- a bearer-token-protected native REST API with ETags and external-edit watching;
- daemon-first CLI operation with safe direct-file fallback when the configured
  daemon is not listening; and
- the modern Zotero Connector save flows for translated items, child files,
  plain webpages with SingleFile snapshots, directly viewed files, and popup tag
  updates.

The Connector implementation is source-compatible with API version 3. The
unmodified Chromium Manifest V3 extension built from Zotero Connector commit
`e168391` has been acceptance-tested against Lantai on the standard
`127.0.0.1:23119` endpoint: an arXiv preprint with PDF and SingleFile snapshot,
a plain webpage with SingleFile snapshot, a directly viewed standalone PDF, and
a progress-popup tag update all completed successfully.

## Getting started

```sh
cargo run -- --library references.bib init
cargo run -- add --type article \
  --field 'author=Lovelace, Ada' \
  --field date=1843 \
  --field 'title=A Sketch of the Analytical Engine'
cargo run -- list
cargo run -- show lovelace1843sketch
cargo run -- attach lovelace1843sketch paper.pdf --mime application/pdf
cargo run -- add --from imported.bib
cargo run -- export lovelace1843sketch --output selected.bib
cargo run -- check
cargo run -- serve
```

Use `init --attachments PATH` when the managed files should live somewhere
other than the default adjacent `<bibliography-stem>.files/` directory. `add
--from -` imports BibLaTeX from standard input, and `export` writes canonical
BibLaTeX to standard output unless `--output` is supplied.

The configured library path can be overridden with `--library` or
`LANTAI_LIBRARY`. Configuration is stored in the platform configuration
directory and includes a random native-API bearer token. The native API listens
on `127.0.0.1:23120` by default.

The unmodified Zotero Connector requires `127.0.0.1:23119`. Zotero and Lantai
cannot own that port simultaneously, so quit Zotero before running `lantai
serve` when testing Connector capture. Lantai preserves Zotero's loopback,
`Host`-header, request-header, and restrictive CORS checks on that listener.

Run `cargo run -- --help` for the complete command surface.

## Verification

```sh
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

The suite covers source-preserving mutations, generated formatting
idempotence/semantic round trips, concurrent writers and stale-source guards,
attachments and trash, watcher recovery, native API authentication/ETags,
daemon/direct parity, Connector security and response headers, translated-item
capture, binary file uploads, SingleFile snapshots, standalone files, session
conflicts, target/tag updates, and Zotero-to-BibLaTeX mapping.

The official-extension acceptance matrix above was run from Zotero Connector
commit `e168391`; it exercises the extension's real translator detection,
browser action, progress popup, resource downloads, and upload requests rather
than synthetic HTTP payloads alone.

## Development notes

- [Implementation plan](docs/PLAN.md)
- [Zotero desktop/Connector protocol](docs/zotero-connector-protocol.md)
