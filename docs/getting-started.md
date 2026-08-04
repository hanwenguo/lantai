# Installation and first steps

[Back to the user manual](index.md).

## Requirements

The Homebrew tap provides bottles for Apple Silicon on macOS 26, and for ARM64
and x86-64 Linux. Intel macOS is not supported:

```sh
brew install hanwenguo/tap/lantai
lantai --version
```

Building Lantai from source requires Rust 1.88 or newer and Cargo. The core
binary is Rust and has no runtime shell dependency. The optional official
extensions require Bash and additional tools listed in the [extension
guide](../extension/README.md).

Build and install the current checkout:

```sh
cargo build --release
cargo install --path .
lantai --version
```

`cargo install --path .` installs the `lantai` executable into Cargo's binary
directory, normally `~/.cargo/bin`. Ensure that directory is on `PATH`.

## Initialize a library

```sh
lantai init
```

`init` asks where the bibliography should live, offers the managed attachment
directory beside it, and writes a configuration file containing a random REST
bearer token. It creates or adopts the bibliography and never truncates an
existing one. Answer the last question with no, or press Escape, and nothing is
written.

Prompting needs a terminal on stdin, stdout, and stderr. If `LANTAI_LIBRARY` is
set, `init` offers it as the answer, because that variable outranks the
configuration for every later command.

It refuses to replace an existing configuration unless you confirm, or unless
`--force` is supplied.

To answer in advance instead of being asked — in a script, a container image,
or a dotfiles bootstrap — supply the values as flags:

```sh
lantai init --library "$HOME/Documents/references.bib" \
  --attachments "$HOME/Documents/reference-files"
```

Supplying `--library` selects the non-interactive path outright; `--attachments`
and `--force` merely pre-answer their own questions. `--json` or a redirected
stream also disables prompting, so automation behaves predictably and fails
instead of waiting for an answer nobody can give.

`init --json` returns:

```json
{
  "status": "initialized",
  "library": "/home/me/Documents/references.bib",
  "attachments": "/home/me/Documents/references.files",
  "config": "/home/me/.config/lantai/config.toml"
}
```

Once a library is configured, ordinary commands need no arguments about it. See
[configuration](configuration.md) for the settings file, its location, and how
to point a single command at a different library.

## First records

```sh
lantai add --type article \
  --field 'author=Lovelace, Ada' \
  --field date=1843 \
  --field 'title=A Sketch of the Analytical Engine'

lantai list
lantai show Lov43
lantai collection add Lov43 History/Computing
lantai collection list
lantai attach Lov43 ./paper.pdf --mime application/pdf
lantai export Lov43 --output selected.bib
```

`lantai --help` groups the commands by what they are for and lists any custom
commands installed on `PATH`.

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

Verify the selected paths and the parsed library:

```sh
lantai check
lantai check --json
```

`check` reports which bibliography and attachment directory are in use and the
detailed integrity diagnostics described in
[Operations and troubleshooting](operations.md). Against a running daemon it
also reports trouble only the daemon can see.

Continue with the [library model](library-model.md) and [CLI
reference](cli-reference.md).
