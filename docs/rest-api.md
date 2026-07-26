# Native REST API

[Back to the user manual](index.md).

The native API is intended for trusted local automation. `lantai serve` binds
it to the configured loopback address, `127.0.0.1:23120` by default. It is
separate from the unauthenticated-but-filtered Zotero-compatible listener on
port 23119.

## Authentication and representation

Every `/api/v1/*` request requires:

```http
Authorization: Bearer <api_token from config.toml>
```

Keep the token out of command histories and process arguments when practical:

```sh
export LANTAI_TOKEN='read this from the protected config file'
export LANTAI_API_URL='http://127.0.0.1:23120'

curl -sS \
  -H "Authorization: Bearer $LANTAI_TOKEN" \
  "$LANTAI_API_URL/api/v1/health" | jq
```

JSON item representations use the shared [public item schema](library-model.md#public-item-json).
JSON bodies use UTF-8. Bibliography export uses
`application/x-bibtex; charset=utf-8`; attachment downloads use their stored
media type and a sanitized `Content-Disposition` filename.

## Revisions, ETags, and writes

Every successful response includes the current library revision as a quoted
`ETag`. Mutating requests require that exact value in `If-Match`:

```sh
headers=$(mktemp)
body=$(mktemp)
trap 'rm -f "$headers" "$body"' EXIT

curl -sS -D "$headers" -o "$body" \
  -H "Authorization: Bearer $LANTAI_TOKEN" \
  "$LANTAI_API_URL/api/v1/health"

etag=$(
  awk 'tolower($1) == "etag:" { sub("\r$", "", $2); print $2 }' "$headers"
)

curl -sS \
  -H "Authorization: Bearer $LANTAI_TOKEN" \
  -H "If-Match: $etag" \
  -H 'Content-Type: application/json' \
  -d '{"type":"article","fields":{"title":"A New Item"}}' \
  "$LANTAI_API_URL/api/v1/items" | jq
```

Missing `If-Match` returns `428 precondition_required`. A stale revision
returns `409 revision_conflict` with `details.current_revision`. Fetch a fresh
representation/ETag, reconsider the intended mutation against current state,
and retry; do not blindly overwrite conflicts.

The `revision` property is included inside health, item, and item-list JSON.
Other JSON responses carry it only in the `ETag` header.

## Shared errors

Application errors normally use:

```json
{
  "error": {
    "code": "revision_conflict",
    "message": "the library revision has changed",
    "details": {"current_revision": "..."}
  }
}
```

`details` is omitted when unavailable. Common statuses/codes are:

| Status | Codes and meaning |
| --- | --- |
| 400 | `invalid_request`, `invalid_multipart` |
| 401 | `unauthorized` |
| 404 | `not_found`, `attachment_not_found` |
| 409 | `conflict`, `revision_conflict` |
| 413 | `attachment_too_large` |
| 428 | `precondition_required` |
| 500 | `internal_error`, `task_failed`, `upload_failed` |

Malformed extractor-level JSON or HTTP framing may be rejected by the HTTP
framework before Lantai can produce this application error shape.

## Health and items

### `GET /api/v1/health`

Returns `200` even when the library is degraded:

```json
{
  "status": "ok",
  "version": "0.4.0",
  "revision": "...",
  "entries": 42,
  "warnings": 1,
  "errors": 0
}
```

`status` is `degraded` when the cached check has errors or a disk/watcher error.
`disk_error` is included when present.

This endpoint is a cheap liveness probe answered from the cache; for the full
diagnostic report use [`GET /api/v1/check`](#get-apiv1check).

`version` is the daemon's own version. The CLI compares it on connect and
refuses to use a daemon that does not match, rather than exchanging field names
the other side may spell differently.

### `GET /api/v1/items`

Optional query parameters:

| Parameter | Meaning |
| --- | --- |
| `q` | Query terms, in the language `lantai list` takes |
| `collection` | The collection and everything nested under it, case-insensitively |
| `sort` | Comma-separated sort keys, each optionally prefixed with `-` |

`q` holds whitespace-separated terms, all of which must match: `q=type:article
author:vaswani`. A term containing whitespace is double-quoted, and inside
quotes `\"` and `\\` are literal — so the old whole-string substring search is
now spelled `q="two words"`. The grammar, and the sort keys, are documented
under [`lantai list`](cli-reference.md#list). A malformed term or sort key is
rejected with `400`.

Unknown parameters are rejected with `400`, so a client still sending the
removed `tag` or `type` filters fails loudly instead of receiving the whole
library. `q` and `collection` are ANDed. Without `sort` the response preserves
bibliography order:

```json
{"items": [], "revision": "..."}
```

```sh
curl -sS -G \
  -H "Authorization: Bearer $LANTAI_TOKEN" \
  --data-urlencode 'q=attention' \
  --data-urlencode 'collection=Reviewed' \
  "$LANTAI_API_URL/api/v1/items" | jq '.items'
```

### `POST /api/v1/items`

Requires `If-Match`. Request:

```json
{
  "type": "article",
  "citation_key": "optionalExplicitKey",
  "fields": {
    "author": "Lovelace, Ada",
    "date": "1843",
    "title": "A Sketch"
  }
}
```

`citation_key` and `fields` are optional; `type` is required. Returns `201` and
the created item with a top-level `revision` alongside the item fields.

### `GET /api/v1/items/{id}`

`id` is a UUID or unambiguous citation key. Returns `200` with the item fields
and top-level `revision`.

### `PATCH /api/v1/items/{id}`

Requires `If-Match`. All members are optional and are applied together:

```json
{
  "set": {"title": "Revised title"},
  "set_raw": {"custom": "\"prefix \" # {Suffix}"},
  "unset": ["month"],
  "collections": ["Reviewed", "History/Computing"],
  "citation_key": "newKey"
}
```

`set` stores literals, `set_raw` stores validated BibTeX expressions, `unset`
removes fields, `collections` replaces the complete membership list, and
`citation_key` renames the key. Unknown fields — including the former `tags`
spelling — are rejected with `422` rather than ignored. Duplicate/conflicting
field actions are rejected. Returns `200` with the complete updated item and
revision.

### `DELETE /api/v1/items/{id}`

Requires `If-Match`. Removes the item, moves managed attachments to trash, and
returns `204` with no body and a new ETag. External files are not deleted.

## Import and export

### `POST /api/v1/import`

Requires `If-Match`:

```json
{"source":"@book{key, title={Imported}}\n"}
```

The import is all-or-nothing and must contain at least one entry. Returns `201`
with an array of `{uuid, citation_key}` records.

### `GET /api/v1/export`

Returns canonical BibLaTeX. Omit `ids` for the complete library or provide a
comma-separated list of UUIDs/citation keys:

```sh
curl -sS -G \
  -H "Authorization: Bearer $LANTAI_TOKEN" \
  --data-urlencode 'ids=first-key,second-key' \
  "$LANTAI_API_URL/api/v1/export" > selected.bib
```

## Attachments

### `POST /api/v1/items/{id}/attachments`

Requires `If-Match` and multipart form data. Exactly one `file` part is
required; optional text parts are `title` and `media_type`:

```sh
curl -sS \
  -H "Authorization: Bearer $LANTAI_TOKEN" \
  -H "If-Match: $etag" \
  -F 'file=@paper.pdf;type=application/pdf' \
  -F 'title=Preprint PDF' \
  -F 'media_type=application/pdf' \
  "$LANTAI_API_URL/api/v1/items/$uuid/attachments" | jq
```

The upload is streamed and subject to `attachment_limit_bytes`. Returns `201`
with `item_uuid`, `attachment_uuid`, `citation_key`, `title`, `path`,
`media_type`, and `size`.

### `GET /api/v1/items/{id}/attachments/{attachment_uuid}`

Streams the managed file with `200`, its media type, filename disposition, and
ETag. It does not expose arbitrary external attachment paths.

### `DELETE /api/v1/items/{id}/attachments/{attachment_uuid}`

Requires `If-Match`. Moves the managed file to trash and returns `200` with
`item_uuid`, `attachment_uuid`, `citation_key`, and optional `trashed_to`.

## Formatting, checking, and trash

### `POST /api/v1/format`

Requires `If-Match` and may use an empty body. Returns:

```json
{"changed": true, "assigned_ids": 3}
```

### `GET /api/v1/check`

Returns status, counts, and detailed issues. It never changes the library.
`lantai check --json` returns the same report plus the `library` and
`attachments` paths, which are a property of the caller's configuration rather
than of the served library.

### `GET /api/v1/trash`

Returns an array of `{"path": ..., "size": ...}` records.

### `DELETE /api/v1/trash`

Requires `If-Match`. Permanently removes all trash and returns
`{"purged": NUMBER}`. Because purging does not change the bibliography, the
ETag normally remains unchanged.

## REST composition

`curl` needs `--data-urlencode` for anything with a space or a `+` in it:

```sh
curl --get --fail --silent \
  --header "Authorization: Bearer $LANTAI_TOKEN" \
  --data-urlencode 'q=type:article collection:Reviewed' \
  --data-urlencode 'sort=-year' \
  http://127.0.0.1:23120/api/v1/items |
  jq '.items[] | {uuid, citation_key, title}'
```

For a custom mutation client, always retain and update the ETag after each
response, use UUIDs, and treat each request as a separate atomic operation.
