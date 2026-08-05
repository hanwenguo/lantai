# Lantai user manual

This manual documents Lantai 0.6.0 as it is currently implemented. Lantai uses
one UTF-8 BibLaTeX file as its canonical library and exposes that library
through a CLI, a native local REST API, and a Zotero-compatible browser capture
endpoint.

## Start here

1. [Install Lantai and initialize a library](getting-started.md), then keep
   [configuration](configuration.md) at hand for the settings file.
2. Read the [library and storage model](library-model.md) before editing the
   `.bib` file or moving attachments.
3. Use the [CLI reference](cli-reference.md) for ordinary library management.
4. If needed, enable the [REST API](rest-api.md) or [Zotero Connector](zotero-connector.md).
5. Read [operations and troubleshooting](operations.md) before designing a
   backup or recovery procedure.

## Automation

- [Search, pick items interactively, and compose the CLI](cli-workflows.md).
- [Install or write Git-style extensions](../extension/README.md).
- [Run a custom post-save hook](post-save-hooks.md), including custom
  citation-key generation.
- [Install the agent skill](../skills/README.md), which teaches a coding agent
  to search the library, gather literature context, and cite from it.

## Scope and limitations

Lantai supports a single library with items, collections, and attachments.
Collections are one flat namespace stored in BibLaTeX `keywords` and nested by
spelling them with `/`; there is no separate collection object, no
per-collection metadata, and no second notion of a "tag". Lantai does not
provide notes, a reader, cloud sync, word-processor integration, formatted
citations, metadata recognition, or automatic deduplication. A Zotero RDF
import maps collection membership onto that namespace, and imports neither
Zotero's own tags nor its notes. The servers are local-only. Official extension
scripts target Bash-capable macOS and Linux environments, although the Rust
core is not intentionally Unix-only.

## Developer references

The [implementation plan](PLAN.md) records project goals and design decisions;
it is not a user reference. The [Connector protocol analysis](zotero-connector-protocol.md)
documents the upstream Zotero desktop/extension protocol in greater depth than
is needed to operate Lantai.
