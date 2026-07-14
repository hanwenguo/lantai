# Compose Lantai with official extensions

`lantai list` and `lantai show` write JSON by default. Lantai also ships
Git-style extension commands for common `jq`, `fzf`, and shell
workflows. Add the repository's `extension/` directory to `PATH` or
install the scripts as described in the
[extension guide](../extension/README.md):

```sh
export PATH="$PWD/extension:$PATH"
```

Use `--format human` when reading the legacy built-in displays directly:

```sh
lantai list --format human
lantai show vaswani2023attention --format human
```

`--json` is intentionally not accepted by `list` or `show`; use
`--format json` when an explicit spelling is useful. Other built-ins expose
`--json` when their default output is human-readable.

## Item JSON

`lantai list` returns an array in bibliography source order. `lantai show`
returns one object with the same item shape:

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
  "tags": ["machine-learning"],
  "attachments": [
    {
      "uuid": "5025cd5a-ead6-47c0-bb9e-b5399556af98",
      "title": "Preprint PDF",
      "path": "library.files/f450ca71-aa2a-49a1-91d3-2818f42f0903/5025cd5a-ead6-47c0-bb9e-b5399556af98-paper.pdf",
      "media_type": "application/pdf"
    }
  ]
}
```

`fields` retains every BibLaTeX field in source order. `value` is the
expanded text used for searching; `raw` is included when an exact source
expression is available. `title`, `tags`, and `attachments` are
convenient projections. Externally added entries can have a `null` UUID
until mutated or formatted. External attachments likewise have a `null`
UUID, and an attachment without a display title has a `null` title.

## Render a table

`table` turns complete list records into a compact key/type/title view:

```sh
lantai table
lantai table attention --type online --tag machine-learning
```

All arguments are forwarded to the built-in `list`. Lantai's query is a
case-insensitive substring match over citation keys and expanded fields;
`--type` and `--tag` are case-insensitive exact matches. Supplied
filters are combined with AND.

## Query with rich conditions

`query` accepts a jq predicate evaluated once per item. The complete item
is `.` and `$fields` is a temporary object made from the ordered field
array using lowercase names and expanded values:

```sh
lantai query '
  (($fields.author // "") | test("lovelace"; "i"))
  and (($fields.date // "") | startswith("1843"))
  and (($fields.doi // "") != "")
  and any(.tags[]?; ascii_downcase == "history")
' -- --type article
```

The result remains an array of complete item objects. Use built-in filters after
`--` as an inexpensive first pass, then use the predicate for regular
expressions, ranges, optional fields, and compound Boolean logic. If duplicate
field names occur, the last source occurrence wins in `$fields`; the
original `fields` array is never changed.

## Fuzzy-select an item

`pick` shows key and title in `fzf` with a colored full-record preview.
It emits the selected JSON object:

```sh
lantai pick -- --tag machine-learning | jq
```

For a query-then-mutate interface, request only the stable identifier:

```sh
item_id=$(lantai pick --id-only -- attention --type online)
if [ -n "$item_id" ]; then
  lantai tag add "$item_id" reviewed
fi
```

Cancellation succeeds without output. UUIDs are preferred; an item without one
falls back to its citation key. Run `lantai format` first when every
externally added entry must have a stable mutation identifier.

## Select and open an attachment

`open` fuzzy-selects an attachment, resolves managed relative paths
against the bibliography directory, and uses the platform opener:

```sh
lantai open -- --type article
```

To compose with a different application without launching anything:

```sh
attachment_path=$(lantai open --print -- --tag needs-review)
if [ -n "$attachment_path" ]; then
  printf '%s\n' "$attachment_path"
fi
```

Cancellation succeeds without output. Paths are always passed as a single
quoted argument; managed filenames are also sanitized by Lantai.

## Preview and apply a batch tag

`batch-tag` snapshots the matching records and prints their UUID, key, and
title. Without `--apply` it never mutates the library:

```sh
lantai batch-tag reviewed '
  .entry_type == "article"
  and any(.tags[]?; ascii_downcase == "needs-review")
'
```

After reviewing the same command, add `--apply`:

```sh
lantai batch-tag --apply reviewed '
  .entry_type == "article"
  and any(.tags[]?; ascii_downcase == "needs-review")
'
```

Application is refused before any mutation if a selected record lacks a UUID.
Otherwise items are tagged sequentially by UUID and processing stops at the
first failure. These are separate locked Lantai mutations, not one atomic batch.

## Query the REST API

`api-list` requires the bearer token in the environment, URL-encodes
filters with `curl`, and returns the REST `items`/`revision`
envelope:

```sh
export LANTAI_TOKEN='read-from-a-secure-credential-source'

lantai api-list attention --type online |
  jq '.items[] | {uuid, citation_key, title, attachments}'
```

Set `LANTAI_API_URL` to override the default
`http://127.0.0.1:23120` endpoint. The token is never accepted as a
command-line option. The response revision also appears as a quoted `ETag`
header; mutation clients should send it in `If-Match`.

## Adapt the workflows

The official commands are ordinary, executable Bash scripts in
`extension/`. Copy one under a new `lantai-NAME` filename to build a
custom operation interface. Keep machine data on stdout and diagnostics on
stderr, prefer UUIDs for mutations, use NUL delimiters for arbitrary batches,
quote every expansion, and do not use `eval`.
