# Lantai 0.2.0: BibLaTeX-backed headless reference manager

> **Development reference:** This file records implementation goals and design
> decisions. For current user instructions, start with the [Lantai user
> manual](index.md).

## Goal

Lantai is a small, headless reference manager for a personal bibliography
workflow. It keeps Zotero's useful capture path—saving translated web items and
attachments from an unmodified browser Connector—while making a human-editable
BibLaTeX file the library itself. The same library can be managed through a CLI
or authenticated local HTTP API and consumed directly by ordinary BibLaTeX
tools.

Lantai is intentionally not a Zotero profile replacement: Zotero database
compatibility and the omitted features listed below are not goals.

## Summary

Build Lantai as a Rust single binary whose canonical database is one UTF-8 `.bib` file. Attachments live in an adjacent managed directory; no SQLite or other persistent index is used.

The binary provides:

- Unmodified Zotero Connector compatibility on `127.0.0.1:23119`
- A token-protected loopback REST API
- CLI bibliography CRUD, search, attachment management, validation, formatting, and export
- Safe coexistence with external `.bib` editing

One library with collections is supported. A collection is a name stored in BibLaTeX `keywords`, nested by spelling it with `/`; there is no collection object, and membership is the only record one has. There is deliberately no second notion of a "tag": one namespace, one word for it. Notes, readers, Word integration, cloud sync, formatted citations, metadata recognition, and automatic deduplication remain out of scope. Importing a Zotero RDF export maps collection membership onto that namespace and imports neither Zotero's own tags nor its notes. Browser capture matches that rule: the Zotero `tags` array a translator sends is not consulted at all. Collections reach `map_item` out of band, as an explicit field the Connector fills from the save popup's target and the RDF importer fills from collection membership, so a change to tag handling cannot silently unfile items. A name the user types in the popup arrives later through `updateSession` and does file the item.

## Implementation

### Runtime and storage

- Structure the crate around reusable catalog, formatting, attachment, Connector, REST, and CLI services.
- Use Tokio/Axum for HTTP, Clap for CLI parsing, `bibtex-parser` 0.4 for source-aware BibTeX manipulation, `notify` for file watching, `fs4` for advisory locking, and `directories` for platform-standard configuration paths. These current APIs support the required router, source-preserving document, watcher, lock, and project-directory primitives. [bibtex-parser](https://docs.rs/bibtex-parser/latest/bibtex_parser/), [Axum](https://docs.rs/axum/latest/axum/struct.Router.html), [notify](https://docs.rs/notify/latest/notify/), [fs4](https://docs.rs/fs4/latest/fs4/), [directories](https://docs.rs/directories/latest/directories/)
- Configuration precedence is `--library` CLI option, `LANTAI_LIBRARY`, then the default config file.
- `lantai init --library PATH` creates the bibliography, adjacent `<stem>.files/` directory, config, and a random bearer token. Secret-bearing config files use user-only permissions.
- The Connector listener is fixed to `127.0.0.1:23119`, as required by the
  unmodified extension. The authenticated native REST API uses a separately
  configurable loopback port; the attachment root is also configurable.

### BibLaTeX catalog

- Store a stable UUID in the managed `lantaiid` field. Citation keys are editable public identifiers.
- Assign a UUID to an unmanaged entry on its next Lantai mutation or an
  explicit import/format operation; merely reading an externally added entry
  does not rewrite the file.
- Generate new keys as normalized ASCII `AuthorYearTitle`, for example `lovelace1843sketch`; use `anon`, `nd`, and `item` fallbacks and append `a`, `b`, etc. on collision. Never regenerate a key after creation.
- Preserve source entry order and append new entries. Canonicalize entry syntax, indentation, managed field names/order, creator syntax, dates, identifiers, collections, and attachment references.
- Normalize most bibliographic fields after external edits. Preserve exact raw value expressions for `abstract`, `annotation`, `note`, and unknown/custom fields. Preserve comments, `@string`, and `@preamble` blocks in source order.
- An explicit CLI/REST update to a raw field replaces its expression; unrelated writes retain it exactly.
- Adapt Zotero’s BibLaTeX type/field mapping for every Connector item type. Preserve otherwise unmapped scalar Zotero fields as managed `zotero-*` custom fields rather than silently dropping them.
- Reuse that mapping for Zotero RDF import by translating RDF into the same Connector item shape, rather than maintaining a second field table. Read container-level identifiers, since Zotero records a conference paper’s DOI and ISBN on the proceedings rather than the item, and rewrite the locale-rendered `dc:date` as ISO 8601.
- Store collections in normalized `keywords`: trim, remove exact duplicates, preserve spelling/case, and sort case-insensitively.
- Allow duplicate bibliographic records. Enforce UUID and citation-key
  uniqueness for Lantai-created mutations, but retain duplicate keys introduced
  by external edits for `check` to report. A duplicate key cannot identify an
  item for mutation; the caller must use its UUID.

### External edits and crash safety

- Maintain a parsed in-memory search index, rebuilt entirely from the `.bib`.
- On each mutation: acquire the process mutex and advisory lock, reread the file, parse and validate, patch by `lantaiid`, verify the source hash has not changed, then write via same-directory temporary file, `fsync`, and atomic rename. Retry a racing external edit a bounded number of times before returning a conflict.
- Watch the bibliography with debounced events. Valid external changes reload the index and trigger canonical formatting; self-generated watcher events are ignored by content hash.
- If an external edit is malformed, retain the last valid snapshot for reads, mark health as degraded, and reject all writes until parsing succeeds. Never rewrite a partially parsed file.
- Make formatting idempotent: a second reload/write produces identical bytes.

### Attachments

- Store managed files as `<stem>.files/<item-uuid>/<attachment-uuid>-<sanitized-name>`.
- Encode attachments in the BibLaTeX `file` field using Zotero’s `title:relative-path:MIME` entries separated by semicolons, including compatible escaping.
- Stream uploads into temporary files instead of buffering them. Enforce a configurable upload limit, defaulting to 512 MiB.
- CLI attachment operations copy files by default. Manually supplied paths outside the managed root are treated as external references and are never moved or deleted.
- On detach or item deletion, atomically move managed files under `<stem>.files/.trash/<timestamp>/...`; provide `trash list` and explicit `trash purge`.
- Coordinate attachment and bibliography writes so a failed upload or catalog
  transaction cannot leave an entry pointing to an incomplete managed file.
- `check` reports missing references, malformed `file` entries, unreferenced managed files, invalid UUIDs, duplicate keys, and stale temporary files.

## Interfaces

### Zotero Connector

Implement the security and headers described in [the protocol document](zotero-connector-protocol.md):

- Loopback-only binding, `Host` validation, browser-request filtering, restrictive CORS
- `X-Zotero-Version: <Lantai version>`
- `X-Zotero-Connector-API-Version: 3`

Implement:

- `GET|POST /connector/ping`
- `POST /connector/saveItems`
- `POST /connector/saveAttachment`
- `POST /connector/saveStandaloneAttachment`
- `POST /connector/saveSnapshot`
- `POST /connector/saveSingleFile`
- `POST /connector/getSelectedCollection`
- `POST /connector/updateSession`
- `POST /connector/hasAttachmentResolvers`
- `POST /connector/delaySync`
- Empty compatibility responses for `getClientHostnames` and `proxies`

`ping` advertises attachment upload, snapshots, associated-file download, and name autocomplete; it advertises no notes, translator hashes, cloud features, or recognition. Standalone saves return `canRecognize: false`; attachment resolver checks return `false`; sync delay is a no-op.

Expose the library root as Connector target `L1`, named “Lantai”, and derive the remaining save targets from the library's collections. Nest on `/`, synthesizing ancestors the Connector needs to resolve a row's parent, and identify a target by hashing its path so it survives an unrelated collection appearing while the popup is open. The Connector protocol spells these `tags` on the wire; that is Zotero's name, not Lantai's, and only the wire keeps it. `updateSession` resolves a target back to its collection, folds it into the rebase so retargeting moves rather than accumulates, and rejects nonempty notes. Filtering by a collection matches everything nested under it, so the ancestors the picker synthesizes are usable filters rather than dead names. Because `saveItems` carries no target and the popup only calls `updateSession` on user edits, the daemon remembers the last chosen target for the life of the process and applies it to new captures, mirroring Zotero's own selected-collection behavior.

Keep Connector save sessions in memory for ten minutes. Sessions map transient Connector item IDs to Lantai UUIDs and support subsequent binary and SingleFile attachment uploads.

The browser Connector performs translation and authenticated resource
downloads. Lantai consumes translated item JSON and uploaded bytes; it does not
embed Zotero translators or fetch protected page resources itself.

### Native REST API

Require `Authorization: Bearer <token>` on every `/api/v1/*` route. Return structured JSON errors and global library ETags.

- `GET /api/v1/health`
- `GET|POST /api/v1/items`
- `POST /api/v1/import`
- `GET|PATCH|DELETE /api/v1/items/{uuid-or-key}`
- `POST /api/v1/items/{id}/attachments`
- `GET|DELETE /api/v1/items/{id}/attachments/{attachment-uuid}`
- `GET /api/v1/export`
- `POST /api/v1/format`
- `GET /api/v1/check`
- `GET|DELETE /api/v1/trash`

Item responses contain UUID, citation key, entry type, normalized fields, exact raw expressions for pass-through fields, collections, attachment metadata, and revision. `PATCH` supports normalized `set`, raw-field `set_raw`, `unset`, collection replacement, and citation-key rename. Mutations require `If-Match`; stale revisions return `409`.

Item listing supports basic text search and collection filtering. Errors use a
stable JSON code, a human-readable message, and optional structured details.

### CLI

Provide:

- `init`, `serve`, `check`
- `list`, `show`, `collection list`
- `add --from <file|->` or `add --type … --field name=value`
- `import <file.rdf>` for Zotero RDF exports, including files and collections
- `set`, `set-raw`, `unset`, `collection add`, `collection remove`
- `remove`, `attach`, `detach`
- `export`, `format`
- `trash list`, `trash purge`

`init` prompts when it has no `--library` and stdin, stdout, and stderr are all terminals, and asks nothing otherwise, so the flags are overrides rather than the ordinary path. `check` subsumes the earlier separate `health`: one command answers both which paths are in use and whether the library is intact. REST keeps `/health` as a cheap cached liveness probe, which is a different question.

Commands accept UUID or citation key. They use REST when the configured daemon is reachable and authenticated; otherwise they invoke the same locked catalog service directly. `list` and `show` emit rich JSON by default and accept `--format human`; other commands retain human-readable defaults with `--json` for automation. `export` emits the complete canonical file or a filtered selection to stdout/path.

Unknown command names use Git-style extension dispatch: `lantai NAME` runs a
`lantai-NAME` executable found on `PATH`. Official Bash extensions provide the
documented table, rich-query, fuzzy-selection, attachment-opening,
batch-collection, and direct REST workflows without expanding the built-in
query language.

`check` is diagnostic and never changes the library. Any future repair behavior
must be exposed as an explicit operation.

## Delivery sequence

1. **Bibliography core:** create the Rust crate, configuration and `init`;
   implement source-preserving BibLaTeX parsing, UUIDs, citation keys, locked
   atomic mutations, direct-mode CRUD, formatting, export, validation, and
   external-edit recovery.
2. **Attachments:** implement the managed layout, safe streamed ingestion,
   BibLaTeX `file` encoding, downloads, detach-to-trash, purge, and integrity
   checks.
3. **Daemon and native REST:** expose authenticated CRUD/search/attachment
   routes, health, ETag conflicts, and CLI daemon/direct parity.
4. **Zotero Connector:** implement loopback security and discovery first, then
   item saves, attachment and snapshot flows, session collection updates, compatibility
   no-ops, and acceptance tests with the unpacked official extension.

## Test Plan

- Unit-test Zotero-to-BibLaTeX mapping, citation-key generation, creator/date normalization, `file` escaping, UUID handling, and raw-field preservation.
- Property-test formatting idempotence and parse–format–parse semantic equivalence.
- Test external edits to managed and raw fields, watcher reloads, concurrent hash conflicts, malformed-file degraded mode, and atomic-write recovery.
- Test attachment streaming, filename sanitization, external references, detach/delete trash behavior, missing files, and interrupted temporary uploads.
- Add protocol contract tests for all implemented Connector endpoints, headers, session mapping, binary uploads, SingleFile snapshots, target/collection updates, and browser-request rejection.
- Run an end-to-end acceptance test with the unpacked official Connector: capture a translated article with PDF, save a plain webpage with SingleFile snapshot, save a directly viewed PDF, and change the target collection in the progress popup.
- Test CLI direct fallback and daemon-backed operation against the same temporary bibliography, ensuring identical output and conflict behavior.

## Completion criteria

Version 0.2.0 is complete when an unmodified Zotero Connector can save common web
items and attachments into Lantai; the result remains a valid, human-editable
BibLaTeX library; external edits are detected without losing unmanaged source
text; item, collection, and attachment operations work through both CLI and REST; and
the test suite demonstrates safe concurrent writes and daemon/direct parity.
