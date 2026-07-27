# Search, pick, and compose

[Back to the user manual](index.md).

`lantai list` and `lantai show` write JSON by default, and `list` takes a small
query language that the shipped extension commands reuse. The extensions are
Git-style scripts around `jq` and `fzf`; add the repository's `extension/`
directory to `PATH` or install the scripts as described in the
[extension guide](../extension/README.md):

```sh
export PATH="$PWD/extension:$PATH"
```

Once they are on `PATH`, `lantai --help` lists them under "Custom commands",
with the one-line description each declares.

Use `--format human` when reading the legacy built-in displays directly:

```sh
lantai list --format human
lantai show VS23 --format human
lantai collection list --format human
```

`--json` is intentionally not accepted by `list`, `show`, or
`collection list`; use `--format json` when an explicit spelling is useful.
Other built-ins expose `--json` when their default output is human-readable.

## Item JSON

`lantai list` returns an array in bibliography source order. `lantai show`
returns one object with the same shape. See the canonical [public item JSON
schema](library-model.md#public-item-json) for fields, raw expressions,
collections, attachments, ordering, and nullable identities.

## Search with query terms

Every argument to `lantai list` is one term, and an item has to match all of
them. A bare word is the substring search over citation keys and expanded field
values that `list` has always had; `name:value` narrows it. The grammar is in
the [CLI reference](cli-reference.md#list), and `lantai list --help` prints a
summary.

```sh
lantai list attention transformer            # both words, anywhere
lantai list author:vaswani year:2017         # one field each
lantai list type:article collection:Reviewed # exact type, nested collection
lantai list year:2019..2024 --sort=-year     # a range, newest first
lantai list doi:                             # has a DOI at all
lantai list -- -collection:                  # filed nowhere
```

Terms starting with `-` are negations, and they have to follow `--` so that a
mistyped flag is still reported as one. `--sort` takes comma-separated keys —
`key`, `type`, `title`, `year`, or any field name — each optionally prefixed
with `-` for descending; items missing that value sort last either way.

Values containing a colon after a name-shaped prefix would read as a scope, so
`any:` says you meant the whole thing literally:

```sh
lantai list any:https://example.org/paper
```

The same language reaches the daemon and the REST API, where the terms travel
as one `q` string and whitespace inside a term is quoted: `q=type:article
"exact phrase"`.

## Pick items interactively

`pick` runs the matching items through `fzf` and writes the selected items as a
JSON array in bibliography order:

```sh
lantai pick collection:"Machine Learning" | jq '.[].title'
```

Each row shows the year, the authors, and the title; the columns follow the
terminal size. Given names appear in full where they fit, abbreviated where
they do not, and only then does the list shorten to `et al.`. The record itself
is shown underneath as a labelled list rather than as JSON, with the abstract
last and the import bookkeeping left out.

Typing matches whole words against the complete citation key, author list,
title, and collections — including the parts a row abbreviates, so an author's
given name or a phrase late in a long title still finds it. Prefix a word with
`'` for a loose fuzzy match instead. Matching is literal by default because a
fuzzy subsequence over that much text matches almost everything and ranks it by
accident.

Inside the picker, `TAB` adds a selection, `alt-a`/`alt-k`/`alt-u`/`alt-t`
switch what matching looks at (everything, citation key, author, title), and
`alt-y`/`alt-e`/`alt-l`/`alt-r` reorder by year, key, title, or bibliography
order. `alt-c` opens a second picker listing every collection the loaded items
sit in — choosing one narrows the list to it and anything nested under it, and
`(all collections)` clears it again. An item can belong to several collections,
so membership is shown in the record rather than as a column. The header shows
the current state, and narrowing and re-sorting are both local, so neither
re-reads the library.

For a query-then-mutate interface, ask for identifiers instead:

```sh
lantai pick --id-only attention | while IFS= read -r item_id; do
  lantai collection add "$item_id" Reviewed
done
```

Cancelling succeeds without output, and so does a query that matches nothing —
the picker never opens on an empty list. UUIDs are preferred; an item without
one falls back to its citation key. Run `lantai format` first when every
externally added entry must have a stable mutation identifier.

## Open an attachment

`open` is the same picker over attachments rather than items — the same
columns, keys, and matching — which then resolves managed relative paths
against the bibliography directory and hands them to the platform opener:

```sh
lantai open collection:Reviewed
```

Selecting several attachments opens all of them. To compose with a different
application without launching anything:

```sh
lantai open --print collection:needs-review | while IFS= read -r path; do
  printf '%s\n' "$path"
done
```

Paths are always passed as a single quoted argument; managed filenames are also
sanitized by Lantai.

`--stdin` skips the picker and reads a selection that has already been made,
which is how `dwim` opens what it picked. It takes what either picker writes —
an array of items opens every attachment they have, an array of attachment
selections opens exactly those — and one item on its own, as `show` writes it:

```sh
lantai pick collection:Reviewed | lantai open --stdin
lantai show VS23 | lantai open --stdin --print
```

## Change collection membership in bulk

`batch-collection` narrows the library with query terms, offers the matches in
the picker, and applies the change to what you select:

```sh
lantai batch-collection Reviewed type:article collection:needs-review
```

`--remove` takes items out of the collection instead. `--all` skips the picker
and changes every match, which is the form to use from a script:

```sh
lantai batch-collection --remove --all needs-review collection:Reviewed
```

The change is refused before any mutation if a selected record lacks a UUID.
Otherwise items are changed sequentially by UUID and processing stops at the
first failure. These are separate locked Lantai mutations, not one atomic
batch.

## Pick once, then choose what to do

`dwim` is the same picker followed by a menu, for the times when the operation
is easier to recognize than to remember:

```sh
lantai dwim attention
```

`TAB` selects as usual, and what the menu then offers applies to everything
selected: the BibLaTeX of the selection, a `\cite{key,...}` line for LaTeX,
`@key` references for Typst, the bare citation keys, opening every attachment,
putting the items into a collection or taking them out of one, and removing
them. Attaching a file is offered as well when exactly one item is selected.

The collection chooser lists the collections that already exist; typing a name
that is not among them chooses that name, which creates it. Attaching lists the
files directly inside `~/Downloads`, newest first, and lets Lantai infer the
title and the media type. Removing asks first, with the safe answer selected.

Each menu has a flag that answers it, so the same workflow scripts:

```sh
lantai dwim --all --action latex collection:Reviewed year:2024
lantai dwim --all --action collection-add --collection Reviewed key:VS23
lantai dwim --all --action attach --file ~/Downloads/paper.pdf key:VS23
lantai dwim --all --action remove --yes key:draft
```

`--all` skips the picker and acts on every match, `--action` skips the menu,
`--collection` and `--file` skip their choosers, `--from` looks for files
somewhere other than `~/Downloads`, `--yes` skips the removal confirmation, and
`--print` writes the paths the open action would have opened.
Text goes to standard output, so `\cite{...}` reaches the clipboard the usual
way — `lantai dwim --action latex | pbcopy`.

Cancelling any menu does nothing and succeeds, and a change is refused before
it starts if a selected record has no UUID. Mutations are separate locked
Lantai operations, not one atomic batch, exactly as in `batch-collection`.

## Adapt the workflows

The official commands are ordinary, executable Bash scripts in `extension/`.
Copy one under a new `lantai-NAME` filename to build a custom operation
interface. Keep machine data on stdout and diagnostics on stderr, prefer UUIDs
for mutations, use NUL delimiters for arbitrary batches, quote every expansion,
and do not use `eval`. On macOS the scripts run under Bash 3.2, where an empty
array is an error under `set -u`: expand as `${array[@]+"${array[@]}"}`.
