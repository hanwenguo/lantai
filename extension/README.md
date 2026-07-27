# Lantai extensions

This is the canonical installation and authoring reference for extensions. For
the broader user-manual navigation and worked workflows, see the [Lantai user
manual](../docs/index.md) and [CLI workflow guide](../docs/cli-workflows.md).

Lantai supports Git-style custom subcommands. When `NAME` is not a built-in
command, `lantai NAME ARGS...` searches `PATH` for an executable named
`lantai-NAME` and runs it with the remaining arguments.

Built-in commands always take precedence. Lantai never searches the current
directory implicitly, so the directory containing an extension must be on
`PATH`. Whatever is installed there is listed under "Custom commands" in
`lantai --help`; that section is shown only when at least one extension is
installed.

## Install the official extensions

For development, prepend this repository's extension directory:

```sh
export PATH="$PWD/extension:$PATH"
```

For a user installation, copy the scripts into a directory already on
`PATH`:

```sh
install -d "$HOME/.local/bin"
install -m 755 extension/lantai-* "$HOME/.local/bin/"
```

The scripts can also be called directly, such as
`extension/lantai-pick collection:Reviewed`.

## Official commands

| Command | Purpose | Dependencies |
| --- | --- | --- |
| `lantai pick [--id-only] [--attachments] [--] [TERM...]` | Interactively pick items or attachments | `jq`, `fzf` 0.56 or newer |
| `lantai open [--print] [--stdin] [--] [TERM...]` | Pick an attachment and open it | `lantai pick`, `jq`, `open` or `xdg-open` |
| `lantai batch-collection [--remove] [--all] COLLECTION [--] [TERM...]` | Add or remove many items from a collection | `jq`; `lantai pick` unless `--all` |
| `lantai dwim [--action ACTION] [--all] [--] [TERM...]` | Pick items, then choose what to do with them | `jq`, `fzf`; `lantai pick` and `lantai open` |

Run any command with `--help` for its complete interface. `TERM...` is the
[query language](../docs/cli-reference.md#list) the built-in `list` takes, so
filters are written `collection:Reviewed` rather than `--collection Reviewed`.
A `--` is only needed when a term starts with `-`, such as a negation.

`open` reaches the picker through `lantai pick`, and `dwim` reaches both
through `lantai pick` and `lantai open`, so a `lantai-pick` or `lantai-open`
earlier on `PATH` replaces the one they use.

## Extension process contract

Global Lantai options must precede the custom command:

```sh
lantai --library /path/to/references.bib pick type:article
```

The extension inherits the caller's working directory and standard streams.
Lantai also sets:

- `LANTAI` to the current Lantai executable;
- `LANTAI_LIBRARY` when `--library` was supplied;
- `LANTAI_CONFIG` when the hidden configuration override was supplied.

Official extensions use `LANTAI` for nested built-in commands and forward
`LANTAI_CONFIG` when present. A directly executed extension falls back to
finding `lantai` on `PATH`.

The custom process owns its exit status and output. A normal child exit code is
returned unchanged; launch failures are reported by Lantai. Extension names
containing `/` or `\` are rejected rather than treated as paths.

## Write a custom extension

Create an executable named `lantai-NAME` on `PATH`. This minimal
example composes the JSON-first built-ins:

```bash
#!/usr/bin/env bash
set -euo pipefail

# lantai-about: Summarize items as compact JSON

lantai_bin=${LANTAI:-lantai}
args=()
if [[ -n ${LANTAI_CONFIG:-} ]]; then
  args+=(--config "$LANTAI_CONFIG")
fi

"$lantai_bin" ${args[@]+"${args[@]}"} list -- "$@" |
  jq 'map({uuid, citation_key, title})'
```

Keep data on standard output, diagnostics on standard error, quote every shell
expansion, and avoid `eval`. Prefer UUIDs whenever the extension invokes a
mutation.

If an extension drives `fzf` over tab-separated columns, note that `--nth`
counts fields of the string `--with-nth` produces, not of the input line. A
column left out of `--with-nth` cannot be searched, and an index past the end
of the transformed string silently matches nothing.

macOS ships Bash 3.2, where expanding an *empty* array under `set -u` is a fatal
"unbound variable" error — and the array above is empty on every run that did
not pass a configuration override, which is nearly all of them. Write
`${args[@]+"${args[@]}"}`, not `"${args[@]}"`; the official extensions all do.

### Describe the command

`lantai --help` shows the one-line description an extension declares for
itself:

```text
# lantai-about: Summarize items as compact JSON
```

The line must appear within the first 40 lines. Lantai *reads* the file to find
it and never runs the executable, because rendering help must not execute
whatever happens to be named `lantai-*` on `PATH`. Leading whitespace and the
spacing after `#` do not matter; a description longer than 60 characters is
truncated in the listing. Omitting the marker only costs the description — the
command is still listed and still runs.
