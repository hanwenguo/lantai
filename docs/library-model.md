# Library and storage model

[Back to the user manual](index.md).

## The bibliography is the database

Lantai's canonical library is one UTF-8 BibLaTeX file. There is no SQLite
database or persistent search index. Entry order is source order, and newly
created entries are appended. Comments, `@string`, and `@preamble` blocks are
retained by source-aware mutations.

The daemon keeps a rebuildable in-memory view for reads. If the bibliography
changes externally, the daemon reparses it and canonicalizes valid content. A
malformed edit leaves the last valid read snapshot available, marks health as
degraded, and blocks mutations until the file parses again.

Every Lantai write obtains process and advisory locks, rereads and validates the
source, checks for racing changes, writes a same-directory temporary file,
synchronizes it, and atomically replaces the bibliography. Racing external
edits are retried a bounded number of times rather than silently overwritten.

## UUIDs and citation keys

Each managed entry stores a stable UUID in the BibLaTeX `lantaiid` field:

```bibtex
@article{lovelace1843sketch,
  title = {A Sketch of the Analytical Engine},
  lantaiid = {02ca5bd8-c86a-40e1-859e-f85cba6264a8}
}
```

The UUID is the safest identifier for scripts and mutations. The citation key
is the editable name after the entry type (`lovelace1843sketch` above). A key
can identify an item only when it is unambiguous.

New items receive an ASCII `AuthorYearTitle` key. Missing components use
`anon`, `nd`, and `item`; collisions receive `a`, `b`, and later suffixes.
Lantai never automatically regenerates an existing key. Rename one explicitly:

```sh
lantai set 02ca5bd8-c86a-40e1-859e-f85cba6264a8 --key ada1843
```

Entries added by an external editor may initially have no UUID. Reads expose
their UUID as `null`; the next Lantai mutation or `lantai format` adopts them.
Duplicate keys introduced externally are reported by `check` and cannot be
used as mutation identifiers; use UUIDs.

## Public item JSON

CLI list/show output, REST item responses, and post-save events share this item
shape:

```json
{
  "uuid": "f450ca71-aa2a-49a1-91d3-2818f42f0903",
  "citation_key": "vaswani2023attention",
  "entry_type": "online",
  "title": "Attention Is All You Need",
  "fields": [
    {
      "name": "author",
      "value": "Vaswani, Ashish and Shazeer, Noam",
      "raw": "{Vaswani, Ashish and Shazeer, Noam}"
    }
  ],
  "collections": ["Machine Learning"],
  "attachments": [
    {
      "uuid": "5025cd5a-ead6-47c0-bb9e-b5399556af98",
      "title": "Preprint PDF",
      "path": "references.files/f450ca71-aa2a-49a1-91d3-2818f42f0903/5025cd5a-ead6-47c0-bb9e-b5399556af98-paper.pdf",
      "media_type": "application/pdf"
    }
  ]
}
```

`fields` contains every BibLaTeX field in source order. `value` is expanded
text; optional `raw` is the exact source expression when available. `title` is
a convenient case-insensitive projection. Collections and attachments are also
projected from managed fields. An unmanaged item, external attachment, or
attachment title may be `null` where shown by the schema.

## Literal and raw fields

`lantai set ID name=value` stores literal text and lets Lantai normalize its
BibLaTeX representation. `lantai set-raw ID name=EXPRESSION` stores an exact
valid BibTeX expression such as a quoted value concatenated with `#`.

Lantai preserves exact expressions for `abstract`, `annotation`, `note`, and
unknown/custom fields when unrelated fields are changed. Managed identity and
attachment fields cannot be supplied as ordinary user fields.

`lantai format` assigns missing UUIDs and canonicalizes managed syntax,
creators, dates, identifiers, collections, and attachment references. It is
idempotent, but it is still a write operation when bytes change. Commit or back
up hand-edited files before formatting if exact presentation matters.

## Collections

A collection is a name an item carries, stored in BibLaTeX `keywords`. There is
no separate collection object: a collection exists exactly as long as some item
belongs to it, and membership is the item's own list. `lantai collection list`
shows the whole set; `lantai collection add` and `remove` change one item's
membership.

Lantai trims collection names, removes duplicates case-insensitively keeping
the spelling already in use, and sorts case-insensitively. Because `keywords`
separates values with commas, a collection name cannot contain a comma.

Filtering with `--collection` matches the named collection and everything
nested under it. Adding and removing a membership are exact-name operations, so
removing `Projects` leaves an item in `Projects/IfT` alone.

Nesting is spelling. A name containing `/` is read as a child of its prefix, so
`ResearchTopics/Subtyping/SemanticSubtyping` displays under `Subtyping` under
`ResearchTopics`. The stored value is still one flat name; `/` only affects how
`collection list` and the Connector picker group it, and an ancestor is shown
even when no item belongs to it directly.

This is what `lantai import` maps a Zotero collection tree onto, and what the
[Connector's save popup](zotero-connector.md#collections-in-the-save-popup)
reads back as its picker.

What Lantai does not provide is the rest of Zotero's collection model: no
per-collection metadata, no collection that outlives its members, and no
distinction between a collection and a tag — there is one namespace.

## Attachments

A default managed attachment is stored under:

```text
references.files/<item-uuid>/<attachment-uuid>-<sanitized-name>
```

The BibLaTeX `file` field contains Zotero-compatible
`title:path:media-type` entries. Relative paths resolve against the
bibliography directory. A separately configured attachment root may require
safe absolute paths.

`lantai attach` copies a source file into managed storage; it does not move the
source. Managed detach and item removal move files into a timestamped `.trash`
tree before updating the bibliography. `trash purge` is the explicit,
irreversible deletion step.

Externally authored `file` references can point outside the managed root.
Lantai reports them in item JSON but never moves or deletes those external
files. External attachments normally have a `null` attachment UUID and cannot
be detached through the managed attachment interface.

## What to back up

The library consists of the `.bib` file and its configured managed attachment
root. The configuration is separate and contains the REST token. See
[Backups and recovery](operations.md#back-up-and-restore) for a consistent
procedure.
