# CLI reference

[Back to the user manual](index.md).

## Conventions

```text
lantai [--library PATH] COMMAND ...
```

`--library` is global and must precede a custom extension name. Built-in item
arguments accept a stable UUID or an unambiguous citation key. Prefer UUIDs in
scripts. See the [library model](library-model.md) for identity and the shared
[item JSON schema](library-model.md#public-item-json).

Commands are grouped in `lantai --help` the way they are grouped below, and
that listing also names any custom commands installed on `PATH`.

Successful commands exit zero. Usage, configuration, validation, storage,
authentication, and transport failures write diagnostics to stderr and exit
nonzero. `check` also exits nonzero when it finds errors. Successful JSON
output contains no diagnostics.

`list`, `show`, and `collection list` default to JSON and accept
`--format json|human`. Other commands default to human output and accept
`--json` where listed. `export` writes BibLaTeX rather than JSON.

The CLI uses the configured daemon when it is reachable and authenticated,
otherwise it uses locked direct-file access. This fallback does not occur for
authentication or protocol errors.

## Setup and status

### `init`

```text
lantai init [--library PATH] [--attachments PATH] [--force] [--json]
```

Creates or adopts a bibliography, creates managed attachment storage, and
writes configuration.

With no `--library`, and when stdin, stdout, and stderr are all terminals, it
asks where the library should live and offers the adjacent attachment
directory; `--attachments` and `--force` pre-answer their questions. Supplying
`--library`, passing `--json`, or redirecting any of those streams selects the
non-interactive path, which asks nothing and fails when a required value is
missing. `--force` replaces configuration but never truncates the bibliography.

If `LANTAI_LIBRARY` is set it becomes the suggested answer, since it outranks
the configuration for every later command. See
[installation](getting-started.md) and [configuration](configuration.md).

### `serve`

```text
lantai serve
```

Runs the native REST and Zotero Connector listeners until interrupted. Both
listeners must bind successfully. See [REST API](rest-api.md) and [Connector
setup](zotero-connector.md).

### `check`

```text
lantai check [--json]
```

Reports which bibliography and attachment directory are in use, then diagnoses
syntax, identities, attachment references, managed files, and stale temporary
data without changing the library. A managed attachment that is missing is an
error; an attachment directory that does not exist yet is not, because `init`
and the first `attach` create it on demand. Against a daemon, `check` also
reports trouble the daemon alone can see, such as a failed write that left it
serving a stale revision.

JSON contains `library`, `attachments`, `status`, counts, and an `issues` array;
each issue has severity, code, message, optional citation key, and optional
line/column. Errors produce a nonzero exit status. A script that only wants the
paths can read stdout and ignore the status, but must still handle the case
where the library cannot be read at all and no report is produced.

## Search

### `list`

```text
lantai list [QUERY] [--collection COLLECTION] [--format json|human]
```

Returns a JSON array in bibliography source order, or tab-separated
key/type/title rows in human mode. `QUERY` is a case-insensitive substring over
citation keys and expanded field values. `--collection` matches the named
collection and everything nested under it, case-insensitively, so
`--collection Projects` also finds items in `Projects/IfT`. Both filters are
ANDed. An empty result is `[]`.

```sh
lantai list attention --collection Reviewed
lantai list --collection Projects
lantai list --format human
```

For entry-type filters, regular expressions, and compound conditions, use the
official `query` extension described in [CLI workflows](cli-workflows.md).

### `show`

```text
lantai show ID [--format json|human]
```

Returns one complete item object. Human mode prints the entry type/key, UUID,
and expanded fields.

### `collection list`

```text
lantai collection list [--format json|human]
```

Lists every collection in the library. JSON is a flat array of complete names,
which is what `--collection` expects back; human output presents the same names
in the same order, nested on `/` and showing leaf names.

```console
$ lantai collection list
[
  "Inbox",
  "Projects",
  "Projects/IfT"
]
$ lantai collection list --format human
Inbox
Projects
  IfT
```

A collection exists only through the items in it, so an empty library lists
nothing and its JSON is `[]`. Ancestors are shown even when no item belongs to
them directly: a library holding only `Projects/IfT` still lists `Projects`.
Every listed name works as a `--collection` filter, including such an ancestor,
because the filter matches nested collections too.

## Edit

### `add`

Create one item:

```text
lantai add --type TYPE [--field NAME=VALUE ...] [--key KEY] [--json]
```

Or import one or more BibLaTeX entries:

```text
lantai add --from FILE [--json]
lantai add --from - [--json]
```

`--from -` reads standard input. Import preserves source blocks and assigns
missing UUIDs; duplicate UUIDs or citation keys reject the complete import.
JSON creation output is `{ "uuid": ..., "citation_key": ... }`; import output
is an array of those records.

### `import`

```text
lantai import FILE [--attachment-base PATH] [--dry-run] [--json]
```

Imports a Zotero RDF export. `FILE` is the exported `.rdf` document; attachment
files are read from the `files/` directory Zotero writes beside it, so export
with **Export Files** enabled. Zotero RDF is the only built-in Zotero format
that carries collections, attachment files, and citation keys together.

Each Zotero item becomes one entry, using the same item-type and field mapping
as the Connector. A nested Zotero collection becomes one `/`-joined name such as
`Projects/Engines`; a comma in a collection name becomes a space, because
`keywords` separates values with commas. `<z:citationKey>` values are reused,
falling back to a generated `AuthorYearTitle` key when the key is already taken
or Lantai rejects it. **Zotero's own tags are not imported**, and neither are
its notes. Zotero writes `dc:date` in its application locale, so dates are
converted to ISO 8601, narrowing to the year alone when the month cannot be
recognized.

Attachment files are copied into managed storage, so the export directory can
be deleted afterwards. `--attachment-base PATH` resolves linked files that
Zotero recorded against its linked attachment base directory rather than
copying into the export.

A rejected item rolls the whole import back. A file that cannot be copied is
reported instead of aborting, and can be attached later with `attach`.
`--dry-run` reports what would be imported and writes nothing.

JSON output is `{ "imported": ..., "attachments": ..., "collections": [...],
"items": [{ "uuid": ..., "citation_key": ... }], "skipped_attachments":
[{ "item": ..., "title": ..., "reason": ... }] }`. `items` is empty for
`--dry-run`.

### `set`

```text
lantai set ID [NAME=VALUE ...] [--key KEY] [--json]
```

Sets one or more literal fields and/or renames the citation key in one
mutation. At least a field or `--key` is required.

```sh
lantai set "$uuid" 'title=A Revised Title' date=1844
lantai set "$uuid" --key lovelace1844revised
```

### `set-raw`

```text
lantai set-raw ID NAME=EXPRESSION ... [--json]
```

Replaces fields with exact, validated BibTeX value expressions:

```sh
lantai set-raw "$uuid" 'custom="prefix " # {Suffix}'
```

Shell-quote expressions so spaces, braces, quotes, and `#` reach Lantai
unchanged.

### `unset`

```text
lantai unset ID FIELD ... [--json]
```

Removes fields case-insensitively. Managed identity fields cannot be changed
through ordinary field operations.

### `collection add` and `collection remove`

```text
lantai collection add ID COLLECTION ... [--json]
lantai collection remove ID COLLECTION ... [--json]
```

Adds the item to collections, ignoring ones it already belongs to, or removes
it from matching collections case-insensitively. Adding to `Projects/IfT`
creates that collection if no other item was in it.

Both are exact-name operations, unlike the `--collection` filter: removing
`Projects` leaves an item in `Projects/IfT` alone. Adding a name the item
already carries in another case or with different spacing changes nothing and
keeps the existing spelling.

The JSON output for `set`, `set-raw`, `unset`, and collection mutations
contains the item `uuid` and resulting `citation_key`.

### `remove`

```text
lantai remove ID [--json]
```

Removes the bibliography entry and moves its managed attachments to trash.
External files are untouched. JSON contains the optional UUID and citation key.

## Attachments

### `attach`

```text
lantai attach ID FILE [--title TITLE] [--mime MEDIA_TYPE] [--json]
```

Copies a regular file into managed storage. The title defaults to the source
filename and MIME type is inferred when omitted. The configured size limit is
enforced. JSON contains `item_uuid`, `attachment_uuid`, `citation_key`, `title`,
`path`, `media_type`, and `size`.

### `detach`

```text
lantai detach ID ATTACHMENT_UUID [--json]
```

Removes a managed attachment reference and moves its file to trash. JSON
contains the item UUID, attachment UUID, citation key, and optional trash path.

### `trash list`

```text
lantai trash list [--json]
```

Lists trashed managed files. Human output is size and path; JSON is an array of
`{"path": ..., "size": ...}` records.

### `trash purge`

```text
lantai trash purge [--json]
```

Permanently deletes all managed trash. JSON is `{"purged": NUMBER}`. This
operation is irreversible.

## Output

### `export`

```text
lantai export [ID ...] [-o|--output PATH]
```

Writes canonical BibLaTeX for the complete library or selected records. Output
defaults to stdout; `--output -` also means stdout. Selection retains required
support blocks such as strings and preambles.

### `format`

```text
lantai format [--json]
```

Canonicalizes the library and assigns missing UUIDs. JSON contains `changed`
and `assigned_ids`. Review or back up hand-formatted source first.

## Custom commands

An unknown built-in name is dispatched Git-style: `lantai NAME ARGS...` runs
`lantai-NAME` from `PATH`. Built-ins win, paths in names are rejected, and the
child inherits standard streams and exit status.

Whatever is installed appears under "Custom commands" in `lantai --help`, which
omits that section entirely when nothing is installed. Each command is listed
with the one-line description it declares in a `# lantai-about:` comment;
Lantai reads that line from the file and never runs the executable to obtain
it. See the [extension guide](../extension/README.md) for the environment
contract, the description convention, and the six shipped commands: `table`,
`query`, `pick`, `open`, `batch-collection`, and `api-list`.

Use `lantai COMMAND --help` as the executable source of truth for command-line
spelling.
