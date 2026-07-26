# Post-save hooks

[Back to the user manual](index.md).

Lantai can run one trusted custom program after a successful change to the
bibliography. The same hook covers direct CLI writes, daemon-backed CLI writes,
authenticated REST mutations, and Zotero Connector saves.

Hooks are useful for updating an index, committing a library, notifying another
local service, or replacing Lantai's generated citation keys with a personal
format.

## Configure a hook

Add a table to the configuration file printed by `lantai init`:

```toml
[post_save_hook]
command = "/home/me/bin/lantai-after-save"
args = ["--quiet"]
timeout_seconds = 30
```

`command` is passed directly to the operating system. Lantai does not invoke a
shell and does not interpret quoting, pipes, redirections, variables, or
wildcards. Put each fixed argument in `args`. A bare command name is searched
on `PATH`; a relative command containing a separator, such as
`./hooks/reindex`, is resolved relative to the configuration directory.

`args` defaults to an empty array. `timeout_seconds` defaults to 30 and must be
greater than zero. Restart `lantai serve` after changing daemon configuration.
Direct CLI commands load current configuration on each invocation.

The program receives one JSON event on stdin. It runs in the bibliography
directory. Its stdout is discarded so it cannot corrupt Lantai's JSON output;
write diagnostics to stderr. The process inherits the user's permissions and
environment, so configure only trusted executables.

## Event schema

```json
{
  "schema_version": 1,
  "event": "post-save",
  "operation": "item.update",
  "origin": "cli",
  "library": "/home/me/references.bib",
  "revision": "71d0...",
  "items": [
    {
      "uuid": "02ca5bd8-c86a-40e1-859e-f85cba6264a8",
      "citation_key": "lovelace1843sketch",
      "entry_type": "article",
      "title": "A Sketch of the Analytical Engine",
      "fields": [],
      "collections": [],
      "attachments": []
    }
  ],
  "removed_items": []
}
```

| Property | Meaning |
| --- | --- |
| `schema_version` | Event schema version, currently `1` |
| `event` | Always `post-save` |
| `operation` | Kind of committed mutation |
| `origin` | `cli`, `rest`, or `connector` |
| `library` | Selected absolute bibliography path |
| `revision` | BLAKE3 revision after the mutation |
| `items` | Complete post-save views of affected items |
| `removed_items` | UUID/citation-key tombstones for removed items |

Items use the shared [public item JSON schema](library-model.md#public-item-json).
Formatting includes all current items. Removal has no current item and uses
`removed_items`; its UUID can be `null` for an externally authored item.

### Operations

| Operation | Trigger | Item selection |
| --- | --- | --- |
| `item.create` | CLI/REST create; Connector item, webpage, or standalone save | Created items |
| `item.import` | CLI/REST BibLaTeX import; CLI Zotero RDF import | All imported items |
| `item.update` | Field, key, collection, or Connector popup collection change | Updated items |
| `item.delete` | Item removal | Tombstone in `removed_items` |
| `attachment.create` | CLI/REST attach or Connector child/snapshot upload | Parent item after attachment |
| `attachment.delete` | Managed attachment detach | Parent item after detach |
| `library.format` | Byte-changing canonical format | All current items |

Byte-identical successful mutations do not run the hook. Initialization,
reads, checks, exports, and trash purging do not run it.

### Connector granularity

The Connector provides no unambiguous “capture complete” request. One visible
capture can therefore produce an `item.create` event, one or more
`attachment.create` events, and a later `item.update` for the popup's chosen
collection. Lantai
does not delay or heuristically coalesce these requests. Filter by `operation`
when work should happen only after initial metadata creation.

### Environment

| Variable | Meaning |
| --- | --- |
| `LANTAI` | Absolute path to the running Lantai executable |
| `LANTAI_LIBRARY` | Selected bibliography path |
| `LANTAI_CONFIG` | Selected configuration path |
| `LANTAI_POST_SAVE` | `1` while the hook is running |
| `LANTAI_OPERATION` | Event operation |
| `LANTAI_ORIGIN` | Event origin |
| `LANTAI_REVISION` | Post-save revision |

The JSON event is authoritative; the scalar variables are conveniences for
launching tools.

## Delivery, ordering, and failure

The hook starts only after the library change has been atomically and durably
committed. Lantai waits synchronously before returning the originating CLI,
REST, or Connector response. Hook processes are serialized within one Lantai
process and never overlap.

The hook is not part of the storage transaction. A missing executable, stdin
failure, nonzero exit, or timeout produces a warning on Lantai's stderr, but the
original save remains successful. Killing Lantai between commit and execution
can lose the notification; hooks are not a durable job queue.

A hook may call `"$LANTAI"` for another mutation. The nested change is saved,
but its hook is suppressed through `LANTAI_POST_SAVE` and, in daemon mode, an
authenticated internal request marker. This prevents accidental recursion. A
hook that calls the REST API directly must implement its own recursion guard.

## Complete citation-key hook in Python

This example changes keys for newly created or imported items to:

```text
<first-author-family><four-digit-year><first-title-word>
```

Text is converted to lowercase ASCII alphanumerics. Fallbacks are `anon`, `nd`,
and `item`. Existing keys are reserved, and collisions receive `a`, `b`, ...,
`z`, `aa`, and later suffixes. The script uses UUIDs and handles event batches.

Save as `~/bin/lantai-citation-key`, make it executable, and adjust
`SKIP_TITLE_WORDS` or `make_base_key` for the preferred policy:

```python
#!/usr/bin/env python3
import json
import os
import re
import subprocess
import sys
import unicodedata

SKIP_TITLE_WORDS = {"a", "an", "and", "of", "the"}


def ascii_alnum(text):
    normalized = unicodedata.normalize("NFKD", text)
    ascii_text = normalized.encode("ascii", "ignore").decode("ascii")
    return "".join(character for character in ascii_text.lower()
                   if character.isalnum())


def fields_object(item):
    # A duplicated field name is a defect; take the last and move on.
    return {field["name"].lower(): field["value"]
            for field in item.get("fields", [])}


def first_family(author):
    first = re.split(r"\s+and\s+", author, maxsplit=1,
                     flags=re.IGNORECASE)[0].strip()
    if "," in first:
        return first.split(",", 1)[0].strip()
    words = first.split()
    return words[-1] if words else ""


def first_title_word(title):
    words = re.findall(r"[^\W_]+", title, flags=re.UNICODE)
    for word in words:
        if word.casefold() not in SKIP_TITLE_WORDS:
            return word
    return words[0] if words else ""


def make_base_key(item):
    fields = fields_object(item)
    author = ascii_alnum(first_family(fields.get("author", ""))) or "anon"
    date = fields.get("date", fields.get("year", ""))
    match = re.search(r"(?<!\d)(\d{4})(?!\d)", date)
    year = match.group(1) if match else "nd"
    title = ascii_alnum(first_title_word(fields.get("title", ""))) or "item"
    return author + year + title


def suffix(index):
    # 0 -> "", 1 -> "a", 26 -> "z", 27 -> "aa"
    if index == 0:
        return ""
    output = []
    while index:
        index -= 1
        output.append(chr(ord("a") + index % 26))
        index //= 26
    return "".join(reversed(output))


def main():
    event = json.load(sys.stdin)
    if event.get("schema_version") != 1:
        raise RuntimeError("unsupported post-save schema")
    if event.get("operation") not in {"item.create", "item.import"}:
        return

    lantai = os.environ["LANTAI"]
    global_args = [
        "--config", os.environ["LANTAI_CONFIG"],
        "--library", os.environ["LANTAI_LIBRARY"],
    ]
    listed = subprocess.run(
        [lantai, *global_args, "list"],
        check=True, text=True, capture_output=True,
    )
    occupied = {item["citation_key"] for item in json.loads(listed.stdout)}

    for item in event.get("items", []):
        uuid = item.get("uuid")
        if not uuid:
            print("citation-key hook: item has no UUID; skipping", file=sys.stderr)
            continue

        current = item["citation_key"]
        occupied.discard(current)
        base = make_base_key(item)
        index = 0
        candidate = base
        while candidate in occupied:
            index += 1
            candidate = base + suffix(index)
        occupied.add(candidate)

        if candidate == current:
            continue
        print(f"citation-key hook: {current} -> {candidate}", file=sys.stderr)
        subprocess.run(
            [lantai, *global_args, "set", uuid, "--key", candidate],
            check=True,
            stdout=subprocess.DEVNULL,
        )


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"citation-key hook: {error}", file=sys.stderr)
        raise SystemExit(1)
```

```sh
chmod 755 "$HOME/bin/lantai-citation-key"
```

Configure it without a shell:

```toml
[post_save_hook]
command = "/home/me/bin/lantai-citation-key"
timeout_seconds = 30
```

The rename is a second atomic mutation after the initial save. If generation,
collision handling, or `lantai set` fails, Lantai retains the original generated
key and reports a hook warning. A concurrent writer can still create a race;
Lantai's uniqueness checks reject rather than overwrite it.

## Short Bash/jq citation-key hook

This smaller version requires Bash, jq, and basic Unix text tools. It uses only
the first comma-delimited author family, first four date characters, and first
title word. It does not transliterate Unicode or resolve collisions, so a
conflict makes the hook fail and leaves the original key.

```bash
#!/usr/bin/env bash
set -euo pipefail

event=$(mktemp)
trap 'rm -f "$event"' EXIT
cat >"$event"

operation=$(jq -r '.operation' "$event")
case $operation in
  item.create|item.import) ;;
  *) exit 0 ;;
esac

jq -c '.items[] | select(.uuid != null)' "$event" |
while IFS= read -r item; do
  uuid=$(jq -r '.uuid' <<<"$item")
  key=$(
    jq -r '
      (.fields | map({key: (.name | ascii_downcase), value}) | from_entries) as $f
      | (($f.author // "anon") | split(",")[0])
        + (($f.date // $f.year // "nd")[0:4])
        + (($f.title // "item") | split(" ")[0])
      | ascii_downcase
      | gsub("[^a-z0-9]"; "")
    ' <<<"$item"
  )
  "$LANTAI" --config "$LANTAI_CONFIG" --library "$LANTAI_LIBRARY" \
    set "$uuid" --key "$key" >/dev/null
done
```

Avoid `eval`, quote every expansion, and prefer the Python version when names,
Unicode, batches, or collisions matter.

## Test and troubleshoot a hook

Before enabling a hook, save a representative event to a file and run:

```sh
LANTAI=$(command -v lantai) \
LANTAI_LIBRARY=/absolute/path/references.bib \
LANTAI_CONFIG=/absolute/path/config.toml \
LANTAI_POST_SAVE=1 \
LANTAI_OPERATION=item.create \
LANTAI_ORIGIN=cli \
LANTAI_REVISION=test \
/home/me/bin/lantai-after-save < event.json
```

Then enable it and perform a harmless test creation against a backup library.
If it fails, inspect stderr, check executable permission and `PATH`, verify the
timeout, and remember that stdout is intentionally discarded.
