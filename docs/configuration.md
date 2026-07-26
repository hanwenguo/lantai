# Configuration

[Back to the user manual](index.md).

`lantai init` writes the configuration file; see [installation and first
steps](getting-started.md). This page documents what that file holds and how a
command decides which library to open.

## Where the file lives

On macOS and XDG-based Unix systems the default configuration path is
`${XDG_CONFIG_HOME:-$HOME/.config}/lantai/config.toml`. Windows uses its
platform configuration directory. `init` prints the path it used; prefer that
over hard-coding one in a script.

On Unix, Lantai creates the file with user-only permissions because it contains
the API token.

## Select a library

Library selection uses this precedence:

1. global `--library PATH`;
2. `LANTAI_LIBRARY`;
3. `library` in the configuration file.

Examples:

```sh
lantai list
LANTAI_LIBRARY=./project.bib lantai list
lantai --library ./project.bib list
```

The first form is the ordinary one. The other two override the configuration
for a single command, which is useful for a second library that does not
deserve its own configuration.

Relative paths are resolved against the current working directory. When the
selected library differs from the configured library, Lantai uses the default
adjacent attachment directory and does not contact the configured daemon.

## Settings

A generated configuration has this shape:

```toml
library = "/home/me/Documents/references.bib"
api_address = "127.0.0.1:23120"
api_token = "a-random-64-character-token"
attachment_limit_bytes = 536870912

# Present only when a separate attachment directory was chosen:
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
empty and its timeout must be greater than zero.

Protect the file as a secret: anyone who obtains `api_token` can mutate the
library while the daemon is running.

## Edit it by hand

The file is ordinary TOML, so changing a setting means editing it and
restarting any running daemon. `lantai init --force` rewrites it from scratch,
including a new API token, and never truncates the bibliography.

Verify a change with:

```sh
lantai health
```

which reports the paths actually in use.

