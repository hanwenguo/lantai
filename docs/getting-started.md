# Installation and configuration

[Back to the user manual](index.md).

## Requirements

Building Lantai requires Rust 1.88 or newer and Cargo. The core binary is Rust
and has no runtime shell dependency. The optional official extensions require
Bash and additional tools listed in the [extension guide](../extension/README.md).

Build and install the current checkout:

```sh
cargo build --release
cargo install --path .
lantai --version
```

`cargo install --path .` installs the `lantai` executable into Cargo's binary
directory, normally `~/.cargo/bin`. Ensure that directory is on `PATH`.

## Initialize a library

Choose the `.bib` file that will be the library:

```sh
lantai --library "$HOME/Documents/references.bib" init
```

`init` creates or adopts the bibliography, creates its managed attachment
directory, and writes a configuration file containing a random REST bearer
token. It never truncates an existing bibliography. It refuses to replace an
existing configuration unless `--force` is supplied.

By default, `references.bib` uses `references.files/` beside it. To put managed
attachments elsewhere:

```sh
lantai --library "$HOME/Documents/references.bib" init \
  --attachments "$HOME/Documents/reference-files"
```

The human output prints all three selected paths. `init --json` returns:

```json
{
  "status": "initialized",
  "library": "/home/me/Documents/references.bib",
  "attachments": "/home/me/Documents/references.files",
  "config": "/home/me/.config/lantai/config.toml"
}
```

The exact default configuration directory is platform-specific. Use the path
printed by `init` rather than assuming a location. On Unix, Lantai creates the
configuration with user-only permissions because it contains the API token.

## Select a library

Library selection uses this precedence:

1. global `--library PATH`;
2. `LANTAI_LIBRARY`;
3. `library` in the configuration file.

Examples:

```sh
lantai --library ./project.bib list
LANTAI_LIBRARY=./project.bib lantai list
lantai list
```

Relative paths are resolved against the current working directory. When the
selected library differs from the configured library, Lantai uses the default
adjacent attachment directory and does not contact the configured daemon.

## Configuration reference

A generated configuration has this shape:

```toml
library = "/home/me/Documents/references.bib"
api_address = "127.0.0.1:23120"
api_token = "a-random-64-character-token"
attachment_limit_bytes = 536870912

# Present only when init --attachments was used:
attachment_root = "/home/me/Documents/reference-files"

# Optional; see post-save-hooks.md:
[post_save_hook]
command = "/home/me/bin/after-lantai-save"
args = ["--quiet"]
timeout_seconds = 30
```

| Setting | Meaning |
| --- | --- |
| `library` | Configured bibliography path |
| `attachment_root` | Optional managed attachment root; otherwise adjacent to the bibliography |
| `api_address` | Native REST listener; it must resolve to a loopback address |
| `api_token` | Bearer token required by every native REST request |
| `attachment_limit_bytes` | Maximum uploaded/copied attachment size; default 512 MiB |
| `post_save_hook` | Optional command run after actual library changes |

Unknown configuration keys are rejected. `post_save_hook.command` cannot be
empty and its timeout must be greater than zero. Protect the file as a secret:
anyone who obtains `api_token` can mutate the library while the daemon is
running.

## Direct and daemon-backed CLI operation

Ordinary commands do not require a daemon. When a command selects the same
library as the configuration, Lantai briefly probes the configured REST API:

- if the authenticated daemon is reachable, the command uses REST;
- if the loopback connection is refused, it safely accesses the `.bib` file
  through the same locked storage service;
- authentication failures and other daemon errors are reported instead of
  silently falling back.

The two modes produce the same public CLI results. Direct mode is convenient
for occasional commands; daemon mode is required for REST clients and browser
capture and provides external-edit watching.

## Start the daemon

```sh
lantai serve
```

This starts two loopback listeners:

| Address | Purpose |
| --- | --- |
| `127.0.0.1:23120` by default | Authenticated native REST API |
| `127.0.0.1:23119` | Zotero Connector compatibility endpoint |

Keep the process running in a terminal or supervise it with your preferred
user service manager. Stop it with the normal process interrupt, usually
Control-C. Zotero and Lantai cannot simultaneously own port 23119; quit Zotero
before starting Lantai. See [Connector setup](zotero-connector.md).

Verify the selected paths and parsed library:

```sh
lantai health
lantai health --json
lantai check
```

`health` confirms that the bibliography and attachment directory are
accessible. `check` performs the detailed integrity diagnostics described in
[Operations and troubleshooting](operations.md).

## First records

```sh
lantai add --type article \
  --field 'author=Lovelace, Ada' \
  --field date=1843 \
  --field 'title=A Sketch of the Analytical Engine'

lantai list
lantai show lovelace1843sketch
lantai tag add lovelace1843sketch history computing
lantai attach lovelace1843sketch ./paper.pdf --mime application/pdf
lantai export lovelace1843sketch --output selected.bib
```

Continue with the [library model](library-model.md) and [CLI reference](cli-reference.md).
