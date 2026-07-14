# Compose Lantai with `jq`, `fzf`, and shell tools

`lantai list` and `lantai show` write JSON by default so their output can be
passed directly to other programs. Diagnostics go to standard error and a
failed command exits nonzero, leaving standard output available for data.

Use `--format human` when reading the legacy display directly in a terminal:

```sh
lantai list --format human
lantai show vaswani2023attention --format human
```

`--json` is intentionally not accepted by `list` or `show`; use `--format json`
when an explicit spelling is useful. Other commands retain their existing
output defaults and expose `--json` when structured output is available.

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

`fields` retains every BibLaTeX field in source order. `value` is the expanded
text used for searching; `raw` is included when an exact source expression is
available. `title`, `tags`, and `attachments` are convenient projections of
their corresponding fields. An entry added outside Lantai can have a `null`
UUID until it is mutated or formatted. External attachments likewise have a
`null` attachment UUID, and an attachment without a display title has a `null`
title.

## Project and filter with `jq`

Render a compact table without changing Lantai's output contract:

```sh
lantai list |
  jq -r '
    ["KEY", "TYPE", "TITLE"],
    (.[] | [.citation_key, .entry_type, (.title // "")])
    | @tsv
  ' |
  column -t -s $'\t'
```

Lantai's built-in query is a case-insensitive substring match over the citation
key and expanded field values. `--type` and `--tag` are case-insensitive exact
filters, and all supplied filters are combined with AND:

```sh
lantai list attention --type online --tag machine-learning
```

For richer conditions, convert the ordered field array into a temporary jq
object. This does not change or discard the field array in Lantai's output:

```sh
lantai list --type article |
  jq '
    map(
      . as $item
      | ($item.fields
          | map({key: (.name | ascii_downcase), value: .value})
          | from_entries) as $fields
      | select(
          (($fields.author // "") | test("lovelace"; "i"))
          and (($fields.date // "") | startswith("1843"))
          and (($fields.doi // "") != "")
          and any($item.tags[]?; ascii_downcase == "history")
        )
    )
  '
```

Use the built-in filters as an inexpensive first pass, then jq for conditions
such as ranges, regular expressions, optional fields, and compound Boolean
logic.

## Build a fuzzy item picker

The first column below is an item UUID when available and otherwise its citation
key. The remaining columns are the display text shown by `fzf`:

```sh
item_id=$(
  lantai list |
    jq -r '.[] | [(.uuid // .citation_key), .citation_key, (.title // "")] | @tsv' |
    fzf \
      --delimiter=$'\t' \
      --with-nth=2,3 \
      --prompt='Lantai> ' \
      --preview='lantai show {1} | jq -C' |
    cut -f1
)

if [ -n "$item_id" ]; then
  lantai show "$item_id" | jq
fi
```

Prefer UUIDs for later mutations: citation keys can be renamed and externally
edited files can contain duplicate keys. Run `lantai format` explicitly if all
externally added entries need stable UUIDs before building a batch interface.

## Select and open an attachment

Managed attachment paths are relative to the bibliography directory. External
references can be absolute, so resolve both cases before passing a path to an
opener:

```sh
library_dir=$(dirname "$(lantai health --json | jq -r '.library')")

attachment=$(
  lantai list |
    jq -r '
      .[] as $item
      | $item.attachments[]?
      | [
          ($item.uuid // $item.citation_key),
          $item.citation_key,
          (.title // ""),
          .media_type,
          .path
        ]
      | @tsv
    ' |
    fzf --delimiter=$'\t' --with-nth=2,3,4,5 --prompt='Attachment> '
)

if [ -n "$attachment" ]; then
  attachment_path=$(printf '%s\n' "$attachment" | cut -f5-)
  case "$attachment_path" in
    /*) resolved_path=$attachment_path ;;
    *)  resolved_path=$library_dir/$attachment_path ;;
  esac

  if command -v open >/dev/null 2>&1; then
    open -- "$resolved_path"          # macOS
  else
    xdg-open "$resolved_path"         # Linux desktop
  fi
fi
```

Lantai sanitizes managed filenames, but shell variables should still always be
quoted because titles and paths can contain spaces.

## Preview and apply a batch change

First inspect the exact records that will change:

```sh
lantai list |
  jq -r '
    .[]
    | select(.entry_type == "article")
    | select(any(.tags[]?; ascii_downcase == "needs-review"))
    | [.uuid, .citation_key, (.title // "")]
    | @tsv
  '
```

After reviewing the selection, emit only non-null UUIDs with NUL separators and
pass them as arguments without `eval` or shell interpolation:

```sh
lantai list |
  jq -j '
    .[]
    | select(.entry_type == "article")
    | select(any(.tags[]?; ascii_downcase == "needs-review"))
    | select(.uuid != null)
    | .uuid, "\u0000"
  ' |
  xargs -0 -I{} lantai tag add {} reviewed
```

## Query the REST API

The native REST list uses the same rich item objects inside its existing
`items`/`revision` envelope. Supply the bearer token through a secure environment
or credential mechanism rather than embedding it in a script:

```sh
curl --fail --silent --show-error \
  -H "Authorization: Bearer $LANTAI_TOKEN" \
  'http://127.0.0.1:23120/api/v1/items?q=attention&type=online' |
  jq '.items[] | {uuid, citation_key, title, attachments}'
```

The response also carries the same library revision as a quoted `ETag` header.
Mutation clients should use that value in `If-Match` as documented by the
native API contract.
