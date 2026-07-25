# Zotero Connector setup

[Back to the user manual](index.md).

Lantai implements the local desktop protocol used by the official Zotero
browser Connector. Translation and authenticated downloads happen inside the
browser extension; Lantai receives translated metadata and uploaded file
bytes. No modified Lantai-specific extension is required.

## Compatibility

Lantai reports Connector API version 3. The unmodified Chromium Manifest V3
Zotero Connector built from commit `e168391` has been acceptance-tested with:

- an arXiv translated item, PDF, and SingleFile HTML snapshot;
- a plain webpage and SingleFile snapshot;
- a directly viewed standalone PDF;
- progress-popup tag changes; and
- choosing a collection in the progress popup.

The protocol implementation is intended to work with other current official
Connector builds, but only that snapshot and matrix are a recorded acceptance
test. Lantai relies on translators bundled with the extension and does not
distribute translator updates.

## Install and start

1. Install the official Zotero Connector for the browser using Zotero's normal
   distribution channel.
2. Quit Zotero completely. Zotero and Lantai both use `127.0.0.1:23119`, and
   only one process can listen there.
3. Initialize a Lantai library if necessary.
4. Start `lantai serve` and leave it running.
5. Verify the endpoint:

   ```sh
   curl -sS http://127.0.0.1:23119/connector/ping
   ```

   The human-readable GET probe contains `Zotero is running` for compatibility.
6. Open a supported article or resource and use the Connector toolbar button.
7. Confirm the result with `lantai list`, `lantai show ID`, and `lantai check`.

The native REST token is not used on port 23119. Instead, Lantai preserves
Zotero's loopback bind, `Host` validation, browser-request filtering, and
restrictive CORS behavior. Do not proxy or expose either Lantai listener to a
network.

## What a capture does

A translated capture normally arrives in stages:

1. `saveItems` creates one or more bibliography parents and an in-memory save
   session.
2. `saveAttachment` uploads fetched PDFs or other child files.
3. `saveSingleFile` may upload a browser-generated HTML snapshot.
4. The popup queries Lantai's save targets and may send collection or tag
   changes.

A plain webpage uses a webpage parent plus SingleFile snapshot. A directly
viewed file uses the standalone-attachment flow. Sessions are short-lived and
in memory; restarting Lantai during a capture loses the session, not already
committed items.

Lantai supports tags and files but not notes, recognition, attachment
resolvers, cloud sync, or Zotero's word-processor integration. A nonempty popup
note is rejected.

### Collections in the save popup

Lantai has no collection model, so the popup's collection picker is a view over
the library's tags. Under the `Lantai` root it lists every tag, nesting any tag
that contains `/`: `Projects/IfT` appears as `IfT` inside `Projects`. Parents
are shown even when nothing carries that exact tag, which is what an imported
Zotero library usually looks like.

Choosing a collection tags the captured item with that full path, and switching
to another moves the item rather than adding a second membership. Choosing the
`Lantai` root files it under no collection. See [tags](library-model.md#tags).

The daemon remembers the last collection you chose and applies it to later
captures without further clicks, the way Zotero saves into whichever collection
its window has selected. That memory lives in the running process only, so
restarting `lantai serve` returns to the library root. Nothing else creates
collections: a target exists once some item carries the tag, so use
`lantai tag add` or an import to introduce one.

Browser capture can generate several [post-save hook](post-save-hooks.md)
events: initial items, each later file request, and a tag update are separate
durable operations.

## Troubleshooting

### The Connector says Zotero is unavailable

- Confirm `lantai serve` is still running and printed both listener messages.
- Run the ping command above.
- Check whether Zotero or another process already owns port 23119.
- Ensure the browser extension uses its standard local endpoint.
- Inspect daemon stderr for rejected headers, invalid sessions, attachment
  limits, or hook warnings.

The official Connector may offer or attempt its normal zotero.org fallback
when no local endpoint is reachable. That cloud path is outside Lantai; restore
the local listener before retrying if the item must be saved to Lantai.

### Metadata saves but a file does not

- Run `lantai show ID` to distinguish the parent from its attachments.
- Check the configured attachment size limit and free disk space.
- The browser performs the source download; authentication, cookies, content
  blocking, or a failed source response can prevent upload before Lantai sees
  it.
- Lantai returns `false` for attachment-resolver support, so it does not perform
  a second Open Access lookup.
- Run `lantai check` for missing or orphaned managed files.

### Popup tags or collections fail

The target must be the `Lantai` root or a collection derived from a tag that
still exists; a tag removed while the popup was open no longer resolves. The
save session must still exist, and notes must be empty. Restarting the daemon
between item save and tag submission invalidates the session.

### The collection picker is empty

The picker is built from the library's tags, so a library with no tags shows
only the `Lantai` root. Add a tag with `lantai tag add`, or import a Zotero
library with `lantai import`, and reopen the popup.

### Zotero starts instead of Lantai

Stop Zotero, then restart `lantai serve`. If Lantai reports that it cannot bind
`127.0.0.1:23119`, another process still owns the port.

For endpoint payloads, headers, session behavior, and upstream source links,
see the developer-oriented [Connector protocol analysis](zotero-connector-protocol.md).
