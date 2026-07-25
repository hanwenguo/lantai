# Zotero desktop/Connector protocol

This document describes the local protocol between the Zotero browser Connector and the Zotero desktop client. It is intended to guide Lantai's implementation of a Connector-compatible, headless endpoint, not to specify Zotero's cloud API or its word-processor integration.

This is a developer protocol reference. Users configuring browser capture
should start with [Zotero Connector setup](zotero-connector.md).

The description is based on these reference snapshots:

- Zotero desktop `582333397dc5ec308c011ef76bba2084749d0326` (2026-07-12)
- Zotero Connector `e1683916585cd8ab36327087801766a941d56e6d` (2026-07-08)

The most important source files are:

- Zotero's HTTP server: [`server.js`](https://github.com/zotero/zotero/blob/582333397dc5ec308c011ef76bba2084749d0326/chrome/content/zotero/xpcom/server/server.js)
- Zotero's Connector endpoints: [`server_connector.js`](https://github.com/zotero/zotero/blob/582333397dc5ec308c011ef76bba2084749d0326/chrome/content/zotero/xpcom/server/server_connector.js)
- Zotero's save-session state: [`saveSession.js`](https://github.com/zotero/zotero/blob/582333397dc5ec308c011ef76bba2084749d0326/chrome/content/zotero/xpcom/server/saveSession.js)
- The Connector's RPC client: [`connector.js`](https://github.com/zotero/zotero-connectors/blob/e1683916585cd8ab36327087801766a941d56e6d/src/common/connector.js)
- The Connector's translated-item save flow: [`itemSaver.js`](https://github.com/zotero/zotero-connectors/blob/e1683916585cd8ab36327087801766a941d56e6d/src/common/itemSaver.js)
- The Connector's binary attachment uploader: [`itemSaver_background.js`](https://github.com/zotero/zotero-connectors/blob/e1683916585cd8ab36327087801766a941d56e6d/src/common/itemSaver_background.js)
- The Connector's webpage/standalone-file save flow: [`pageSaving.js`](https://github.com/zotero/zotero-connectors/blob/e1683916585cd8ab36327087801766a941d56e6d/src/common/inject/pageSaving.js)

## Architecture

The protocol is ordinary HTTP initiated entirely by the browser Connector:

1. Code injected into the page detects and runs a Zotero web translator in the browser.
2. The injected code passes translated items to the extension's background process using the browser's internal extension-messaging API.
3. The background process sends HTTP requests to the desktop client.
4. If the desktop client cannot be reached, the Connector may fall back to the zotero.org API. That cloud path is separate from the protocol documented here.

The desktop client never opens a connection to the extension and cannot spontaneously send an event to it. Progress and post-save changes are implemented by further Connector requests associated with a save-session ID.

## Transport and request envelope

The default base URL is:

```text
http://127.0.0.1:23119/connector/
```

`23119` is the default of both `extensions.zotero.httpServer.port` in Zotero and `connector.url` in the Connector. The URL can be changed in Connector preferences. Only one process can own that port, so Zotero and Lantai cannot both provide the default endpoint simultaneously.

The Connector's `callMethod()` maps a method name such as `saveItems` to `/connector/saveItems`. If the JavaScript data argument is `null`, it sends `GET`; for every other value, including `{}`, it sends `POST`.

Every normal Connector request includes:

```http
X-Zotero-Version: <connector version>
X-Zotero-Connector-API-Version: 3
```

JSON calls additionally use `Content-Type: application/json` and a JSON-encoded body. Binary attachment calls use the attachment MIME type as `Content-Type` and put the metadata in an `X-Metadata` JSON header. HTTP clients provide `Content-Length`, which Zotero requires on every `POST`.

The default Connector timeout is 15 seconds. Standalone attachment upload raises it to 60 seconds. Integration endpoints deliberately have no timeout, but they are outside Lantai's scope.

Every Zotero response includes:

```http
X-Zotero-Version: <desktop version>
X-Zotero-Connector-API-Version: 3
```

The Connector records the desktop version from `X-Zotero-Version`. It parses a response as JSON only when the response `Content-Type` contains `application/json`; otherwise it returns the response as text or raw XHR response.

### Loopback security

Zotero binds the server to `127.0.0.1`. It also rejects a request unless the `Host` header is `127.0.0.1`, `[::1]`, or `localhost`, with an optional port. This is its DNS-rebinding defense.

For a browser-originated request, Zotero normally requires either `X-Zotero-Connector-API-Version` or `Zotero-Allowed-Request`. If neither is present, it closes the connection without sending an HTTP response, deliberately making Zotero-running and no-server cases indistinguishable to arbitrary web content. A direct browser navigation to `GET /connector/ping` is the exception.

CORS headers are returned only to the configured bookmarklet origin, currently `https://www.zotero.org`. There is no user token or cryptographic authentication in this local protocol. Lantai should preserve the loopback bind, `Host` validation, browser-request filtering, and restrictive CORS behavior.

## Capability discovery

The Connector uses `POST /connector/ping` both as liveness detection and capability negotiation.

Example request:

```json
{}
```

The optional `activeURL` member reports the active page for Zotero's site-specific Quick Copy behavior:

```json
{"activeURL":"https://example.com/article"}
```

Zotero returns an object shaped like:

```json
{
  "prefs": {
    "automaticSnapshots": true,
    "downloadAssociatedFiles": true,
    "supportsAttachmentUpload": true,
    "supportsTagsAutocomplete": true,
    "canUserAddNote": true,
    "googleDocsAddAnnotationEnabled": true,
    "googleDocsCitationExplorerEnabled": false,
    "translatorsHash": "...",
    "sortedTranslatorHash": "...",
    "reportActiveURL": true
  }
}
```

Some members are omitted conditionally. The current Connector coerces its known boolean preference fields to booleans, so an omitted capability becomes `false`.

The key capability for the modern protocol is `supportsAttachmentUpload: true`. It tells the Connector to:

- create bibliographic parent items first with `saveItems`;
- fetch binary attachments in the browser background process and upload their bytes with `saveAttachment`;
- generate HTML snapshots in the browser with SingleFile and upload them with `saveSingleFile`;
- save a directly viewed PDF, EPUB, or other non-HTML resource with `saveStandaloneAttachment`.

If `translatorsHash` is present and differs from the Connector's cache, the Connector asks the local client for translator metadata and code. A Lantai implementation that relies on the translators bundled with the Connector can omit both hash fields and need not initially implement translator distribution.

`GET /connector/ping` is a human-readable probe. It returns an HTML page containing `Zotero is running`; it does not return capabilities.

## Save sessions

All multi-request saves are correlated by a Connector-generated random `sessionID`. A session owns a mapping from transient Connector item IDs to the database items created by the desktop. That mapping is what makes a later binary upload's `parentItemID` meaningful.

The Connector also gives every translated parent and attachment a random `id` if the translator did not provide one. These are opaque correlation keys, not Zotero database IDs.

A new session is created by one of:

- `saveItems`
- `saveSnapshot`
- `saveStandaloneAttachment`
- `import`

Reusing an existing `sessionID` to create another session returns `409` with `{"error":"SESSION_EXISTS"}`. Follow-up calls with an unknown session generally return `400` with `{"error":"SESSION_NOT_FOUND"}`.

Zotero intends to garbage-collect sessions after ten minutes, or after one minute when at least ten sessions exist. The Connector treats a session as short-lived UI state, so Lantai should not make it durable or promise a specific lifetime.

## Modern save workflows

### Saving translated bibliographic items

The main flow is:

```text
Connector                         desktop
    | POST /ping                     |
    |<-- prefs/capabilities ----------|
    |                                 |
    | POST /saveItems                 | create parent items and session mapping
    |<-- 201 --------------------------|
    |                                 |
    | POST /saveAttachment (0..n)     | attach raw PDF/EPUB/etc. bytes
    |<-- 201 --------------------------|
    |                                 |
    | POST /saveSingleFile (0..1)     | attach generated HTML snapshot
    |<-- 201 --------------------------|
    |                                 |
    | POST /getSelectedCollection     | populate save popup
    |<-- target tree/tags -------------|
    | POST /updateSession (0..n)      | retarget and apply tags/note
    |<-- 200 {} -----------------------|
```

`POST /connector/saveItems` uses JSON:

```json
{
  "sessionID": "opaque-random-string",
  "uri": "https://example.com/article",
  "proxy": {"scheme":"https://%h.proxy.example/%p"},
  "items": [
    {
      "id": "connector-item-key",
      "itemType": "journalArticle",
      "title": "Example",
      "creators": [
        {"firstName":"Ada","lastName":"Lovelace","creatorType":"author"}
      ],
      "date": "1843",
      "url": "https://example.com/article",
      "tags": [],
      "attachments": []
    }
  ]
}
```

`items` are Zotero translator item JSON, not Web API v3 item JSON. Field availability depends on `itemType`. The global Zotero [`schema.json`](https://github.com/zotero/zotero-schema/blob/7be7b94b77bbb369c010faeff18735a760bbcb9a/schema.json) is the authoritative type/field mapping.

The Connector's cookie-aware wrapper also adds:

- `uri`, the tab URL;
- `detailedCookies`, a newline-separated rendering of the browser cookie jar, when cookies are available; or legacy `cookie` data on Safari.

The current modern desktop save path uses `uri` as a referrer and ignores the supplied cookies because the Connector itself fetches attachments. They are compatibility fields, not a reason for Lantai to become a general HTTP fetcher.

The successful response is `201 application/json` with an empty body. It does not return database IDs. Lantai must retain the submitted `items[*].id` mapping in the session for attachment follow-ups.

### Uploading child attachments

The extension background process fetches the attachment with the browser's cookies, validates its MIME type, and uploads the resulting bytes:

```http
POST /connector/saveAttachment?sessionID=<session> HTTP/1.1
Content-Type: application/pdf
X-Metadata: {"id":"attachment-key","url":"https://example.com/paper.pdf","contentType":"application/pdf","parentItemID":"connector-item-key","title":"Full Text PDF"}
Content-Length: <byte count>

<raw bytes>
```

`sessionID` may instead be inside `X-Metadata`; Zotero accepts either. `parentItemID` is the transient ID supplied in the earlier `saveItems` item, not a database ID. The server imports the stream as a child of that mapped item and returns `201` with no body.

Non-ASCII header values may be RFC 2047 Q-encoded by the Connector. Zotero decodes RFC 2047 words in all request headers before parsing `X-Metadata`. Lantai should do the same or avoid depending on non-ASCII metadata being present directly in an HTTP header.

If the selected library permits metadata edits but not file edits, Zotero returns `200 text/plain` with `Library files are not editable.` This unusual success status is part of the current behavior.

If a primary attachment download fails, the Connector can call `hasAttachmentResolvers` and then `saveAttachmentFromResolver`, asking the desktop to try its Open Access/custom resolver machinery. This can be omitted in an initial Lantai implementation by returning JSON `false` from `hasAttachmentResolvers`.

### Saving a webpage and SingleFile snapshot

When no translator is used, the Connector first creates a top-level webpage item:

```http
POST /connector/saveSnapshot
Content-Type: application/json

{
  "sessionID": "opaque-random-string",
  "url": "https://example.com/page",
  "title": "Page title",
  "referrer": "https://example.com/",
  "cookie": "legacy document.cookie value"
}
```

Despite the endpoint name, the modern `saveSnapshot` call creates only the parent `webpage` record. It returns `201` with an empty body.

If a snapshot was requested, the Connector then runs SingleFile in the page and sends the self-contained HTML document:

```http
POST /connector/saveSingleFile
Content-Type: application/json

{
  "sessionID": "opaque-random-string",
  "url": "https://example.com/page",
  "title": "Page title",
  "snapshotContent": "<!DOCTYPE html>..."
}
```

For a translator save, this payload also contains `items`; Zotero uses `items[0].id` to locate the parent. For a `saveSnapshot` session, it uses `url`, which was the key assigned to the parent item.

The Connector currently sends JSON. The endpoint also declares `multipart/form-data` support, but that is not used by the active save paths. Chromium extension-internal messaging chunks very large `snapshotContent` values to bypass the browser's per-message limit, then reconstructs the string before making this HTTP request. The HTTP request itself is not chunked at the application-protocol level.

### Saving a directly viewed PDF, EPUB, or other file

For a non-HTML browser document, the modern Connector uploads it as a standalone attachment rather than wrapping it in a webpage item:

```http
POST /connector/saveStandaloneAttachment?sessionID=<session>
Content-Type: application/pdf
X-Metadata: {"url":"https://example.com/paper.pdf","contentType":"application/pdf","title":"Paper"}
Content-Length: <byte count>

<raw bytes>
```

On success Zotero returns:

```http
HTTP/1.0 201 Created
Content-Type: application/json

{"canRecognize":true}
```

`canRecognize` tells the Connector whether Zotero started automatic PDF/EPUB metadata recognition. If true, the Connector calls:

```http
POST /connector/getRecognizedItem
Content-Type: application/json

{"sessionID":"opaque-random-string"}
```

That request waits for recognition to finish. It returns `200` with `{"title":"...","itemType":"journalArticle"}` when the attachment acquired a recognized parent, or `204` if it did not. A headless Lantai without document recognition can always return `{"canRecognize":false}` and omit `getRecognizedItem` initially.

### Choosing a destination and editing session metadata

`POST /connector/getSelectedCollection` with `{}` returns the current target and the target/tag data used by the Connector's save popup:

```json
{
  "libraryID": 1,
  "libraryName": "My Library",
  "libraryEditable": true,
  "filesEditable": true,
  "editable": true,
  "id": null,
  "name": "My Library",
  "targets": [
    {"id":"L1","name":"My Library","filesEditable":true,"level":0},
    {"id":"C23","name":"Papers","filesEditable":true,"level":1,"recent":true}
  ],
  "tags": {
    "L1": [{"tag":"history","type":0}]
  }
}
```

Target IDs are desktop UI identifiers: `L<number>` for a library and `C<number>` for a collection. A headless implementation can expose one `L1` root plus its configured collections.

Four details of this response matter to any implementation that returns more than one target. They were confirmed against the Connector sources at commit `e168391`:

- `targets` is a flat, depth-first array in which depth is carried only by `level`, and each `name` is a leaf name rather than a path. The popup finds a row's parent by scanning *backwards* for the first row at `level - 1` (`getParent`, `ui/ProgressWindow.jsx`), so every intermediate ancestor must be present and a parent must immediately precede its children. A missing ancestor silently corrupts the tree.
- Every target needs `filesEditable`. The popup drops targets without it whenever the current library is files-editable (`inject/progressWindow_inject.js`). The top-level `filesEditable` separately gates whether the Connector uploads attachments at all (`common/itemSaver.js`).
- `tags` only needs the library key. `onTargetChange` walks a target up to its level-0 ancestor and looks up the tag list under that ID, so per-collection entries are never read.
- The Connector classifies a target purely by `id.startsWith('L')`; only the desktop's own `updateSession` parses the numeric remainder. An implementation serving its own endpoint may therefore use any collection ID that does not begin with `L`, though matching `C<number>` stays closest to upstream.

`saveItems` carries no target. The desktop derives one from the collection its window has selected, falling back to the `lastViewedFolder` preference when no window is open (`getSaveTarget`, `server_connector.js`), and `saveItems` then calls `session.update(targetID)` itself. The popup only issues `updateSession` when the user actually changes something, so a headless implementation that applies the target solely on `updateSession` will silently drop it for every capture the user does not touch. Remembering the last chosen target and applying it at save time is the faithful equivalent.

The progress popup applies later changes with:

```http
POST /connector/updateSession
Content-Type: application/json

{
  "sessionID": "opaque-random-string",
  "target": "C23",
  "tags": ["history", "computing"],
  "note": "optional child note"
}
```

Older Connectors may send `tags` as one comma-separated string. Zotero accepts both forms. It moves all items belonging to the session to the target, replaces the user-selected tags while retaining automatic tags, and creates/updates/removes a child note. It returns `200 application/json` with `{}`.

Lantai explicitly does not support notes, so its `ping` response should set `canUserAddNote: false`. It should still accept an absent or empty `note` field in `updateSession`.

## Endpoint inventory

The table below covers `server_connector.js`. The word-processor/Google Docs endpoints in `server_connectorIntegration.js` are a separate long-polling command protocol and are intentionally out of scope.

| Endpoint | Method and body | Success response | Role |
| --- | --- | --- | --- |
| `/connector/ping` | `GET`, or `POST` JSON/text | `200`; HTML for GET, capabilities JSON for POST | Core discovery |
| `/connector/saveItems` | `POST` JSON | `201`, empty | Core translated-item save |
| `/connector/saveAttachment` | `POST` raw bytes plus `X-Metadata` | `201`, empty | Core child-file upload |
| `/connector/saveStandaloneAttachment` | `POST` raw bytes plus `X-Metadata` | `201 {"canRecognize":bool}` | Core direct-file save |
| `/connector/getRecognizedItem` | `POST` JSON | `200` item summary or `204` | Optional recognition follow-up |
| `/connector/saveSnapshot` | `POST` JSON | `201`, empty | Core webpage parent creation |
| `/connector/saveSingleFile` | `POST` JSON, declared multipart support | `201`, empty | Core HTML snapshot upload |
| `/connector/getSelectedCollection` | `POST` JSON | `200` target tree and tags | Core save-popup support |
| `/connector/updateSession` | `POST` JSON | `200 {}` | Core retarget/tag/note update |
| `/connector/hasAttachmentResolvers` | `POST` JSON | `200` JSON boolean | Optional OA/custom fallback |
| `/connector/saveAttachmentFromResolver` | `POST` JSON | `201` attachment title text | Optional OA/custom fallback |
| `/connector/getTranslators` | `POST` JSON; optional `url` | `200` translator metadata array | Optional local translator distribution |
| `/connector/getTranslatorCode` | `POST {"translatorID":"..."}` | `200` JavaScript source | Optional local translator distribution |
| `/connector/detect` | `POST {"uri":"...","html":"..."}` | `200` translator array | Server-supported legacy/bookmarklet detection; modern extension detects in-page |
| `/connector/import?session=<id>` | `POST` raw BibTeX/RIS/etc. | `201` imported item array | Intercepted bibliography-file import |
| `/connector/installStyle?origin=<url>` | `POST` raw CSL | `201 {"name":"..."}` | Intercepted CSL installation; out of Lantai's aim |
| `/connector/delaySync` | `POST` | `204` | Zotero cloud-sync scheduling; out of scope |
| `/connector/getClientHostnames` | Declared `POST` JSON | `200` hostname array | Proxy support; not needed for capture |
| `/connector/proxies` | `POST` JSON | `200` proxy array | Retained desktop endpoint, no longer called by this Connector snapshot |

There is stale Connector code for `/connector/sessionProgress`, used only when `supportsAttachmentUpload` is false. The current desktop advertises `true` and no longer registers that endpoint. Lantai should implement the modern upload flow instead of copying this dead compatibility path.

## Errors and fallback behavior

The common statuses used by the Connector endpoints are:

- `200`, `201`, and `204`: success;
- `400`: malformed input, missing/unknown session, unsupported method/content type, or an outdated Connector on a save endpoint;
- `409`: attempted creation of a duplicate session;
- `500`: internal save/import/resolver failure;
- `503`: integration transaction already active, only for the out-of-scope document integration protocol.

The Connector treats every status of `400` or greater as a `CommunicationError`. It only falls back from local item saving to zotero.org when the local request fails with status `0`, meaning the local server is unreachable. A valid HTTP error such as `404` or `500` does not trigger cloud fallback. Timeouts are also treated cautiously because the desktop may still have committed the save.

The Connector has client-side handling for a historical `412 Precondition Failed` version mismatch, but this desktop snapshot does not generate `412`. Its current outdated-Connector check returns `400 {"error":"CONNECTOR_VERSION_OUTDATED"}` for `saveItems` and `saveSnapshot` when the API-version header is below 3.

## Recommended initial Lantai compatibility surface

For Lantai's stated goal, the smallest useful implementation is:

1. Bind only to loopback on port `23119` and implement the request filtering described above.
2. Implement `POST` and navigational `GET /connector/ping`. Return API version 3, `supportsAttachmentUpload: true`, `canUserAddNote: false`, and Lantai's attachment/snapshot preferences. Omit translator hashes initially.
3. Implement session-backed `saveItems`, `saveSnapshot`, `saveAttachment`, `saveSingleFile`, and `saveStandaloneAttachment`.
4. Implement `getSelectedCollection` and `updateSession` with a simple headless target model.
5. Return `false` from `hasAttachmentResolvers`; add resolver behavior only if it becomes useful.
6. Return `canRecognize: false` from standalone uploads until PDF/EPUB recognition exists.
7. Add `import` for BibTeX/RIS if browser interception is desired. CSL installation, cloud sync, notes, and document integration are not needed for the project's aim.

The crucial implementation boundary is that web translation and authenticated attachment fetching already happen in the Connector. Lantai does not need Zotero's GUI, embedded browser, cloud account, or full translator runtime to accept the modern save protocol.
