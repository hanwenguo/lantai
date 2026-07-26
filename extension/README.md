# Lantai extensions

This is the canonical installation and authoring reference for extensions. For
the broader user-manual navigation and worked workflows, see the [Lantai user
manual](../docs/index.md) and [CLI workflow guide](../docs/cli-workflows.md).

Lantai supports Git-style custom subcommands. When `NAME` is not a built-in
command, `lantai NAME ARGS...` searches `PATH` for an executable named
`lantai-NAME` and runs it with the remaining arguments.

Built-in commands always take precedence. Lantai never searches the current
directory implicitly, so the directory containing an extension must be on
`PATH`.

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
`extension/lantai-table --collection Reviewed`.

## Official commands

| Command | Purpose | Dependencies |
| --- | --- | --- |
| `lantai table [LIST_ARGS...]` | Render a key/type/title table | `jq`; optional `column` |
| `lantai query FILTER [-- LIST_ARGS...]` | Select items with a jq predicate | `jq` |
| `lantai pick [--id-only] [-- LIST_ARGS...]` | Fuzzy-select an item | `jq`, `fzf` |
| `lantai open [--print] [-- LIST_ARGS...]` | Fuzzy-select and open an attachment | `jq`, `fzf`, `open` or `xdg-open` |
| `lantai batch-collection [--apply] COLLECTION FILTER [-- LIST_ARGS...]` | Preview or apply a batch membership change | `jq`; optional `column` |
| `lantai api-list [QUERY] [--collection COLLECTION]` | Query the native REST API | `curl`, `jq` |

Run any command with `--help` for its complete interface. Arguments after
`--` in `query`, `pick`, `open`, and `batch-collection` are passed to
the built-in `list` command.

## Extension process contract

Global Lantai options must precede the custom command:

```sh
lantai --library /path/to/references.bib query '.entry_type == "article"'
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

lantai_bin=${LANTAI:-lantai}
args=()
if [[ -n ${LANTAI_CONFIG:-} ]]; then
  args+=(--config "$LANTAI_CONFIG")
fi

"$lantai_bin" "${args[@]}" list "$@" |
  jq 'map({uuid, citation_key, title})'
```

Keep data on standard output, diagnostics on standard error, quote every shell
expansion, and avoid `eval`. Prefer UUIDs whenever the extension invokes a
mutation.
