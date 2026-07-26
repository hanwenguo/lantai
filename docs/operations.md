# Operations, backups, and troubleshooting

[Back to the user manual](index.md).

## Back up and restore

A complete library backup includes:

1. the configured `.bib` file;
2. the configured managed attachment root; and
3. optionally, the protected configuration file containing the API token.

For a consistent filesystem-level snapshot, stop `lantai serve` and avoid
running mutating CLI commands while copying the bibliography and attachments.
The bibliography and each individual mutation are atomic, but an ordinary copy
of two paths is not a cross-file snapshot while writes continue.

Example after stopping writers:

```sh
backup="$HOME/backups/lantai-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$backup"
cp -p "$HOME/Documents/references.bib" "$backup/"
cp -Rp "$HOME/Documents/references.files" "$backup/"
```

Copy the configuration separately only if preserving the current REST token is
desired, and keep that backup private. If it is lost, reinitializing with
`--force` can produce new configuration without truncating the `.bib`, but
review the attachment-root selection before doing so.

To restore, stop writers, restore the `.bib` and matching attachment root to
their configured paths, then run:

```sh
lantai check
```

Do not use the managed `.trash` directory as a backup. Purging is irreversible.

## Safe external editing

The `.bib` file is intentionally editable. Prefer an editor that writes a
complete UTF-8 file atomically. While the daemon is running, valid external
edits are detected and canonicalized; malformed edits leave the last valid
read snapshot cached and block writes.

Recommended workflow for extensive edits:

1. make a version-control commit or backup;
2. stop the daemon if many structural edits are planned;
3. edit the bibliography, preserving unique `lantaiid` values and managed
   `file` references;
4. run `lantai check`;
5. inspect `lantai format` effects in version control before accepting them;
6. restart the daemon.

Do not directly reorganize managed attachment files without updating their
BibLaTeX `file` references. Use `attach`, `detach`, and `remove` when possible.

## Integrity

`lantai check` is the single, non-mutating diagnostic interface. It reports the
bibliography and attachment paths actually in use and high-level counts, then
details malformed BibLaTeX, missing/invalid/duplicate identities, duplicate
keys, malformed or unsafe attachment references, missing files, orphaned
managed files, and stale temporary files. Against a running daemon it adds what
only that process knows, such as a failed write that left it serving a stale
revision while the file on disk still parses.

Warnings do not necessarily block mutations. Errors and malformed source can
put the daemon in degraded state and cause `check` to exit nonzero.

A daemon also answers `GET /api/v1/health`, a cheap cached liveness probe for
monitoring; use `check` when you want to know what is actually wrong.

## Recover from a degraded bibliography

1. Save a copy of the broken `.bib` before editing it further.
2. Run `lantai check --json` and inspect issue line/column information.
3. Repair the syntax in an external editor. Do not run a destructive cleanup or
   replace the file with a partial parse.
4. Run `lantai check` again.
5. When it succeeds, run `lantai format` only if canonicalization and missing
   UUID assignment are wanted.

The daemon watcher will recover after a valid file appears. If filesystem
notifications were lost, restart `lantai serve`.

## Attachment recovery

- A missing attachment means the `file` reference exists but its target does
  not. Restore the file to that exact safe path or deliberately detach/update
  the reference.
- An orphan managed file exists under the attachment root without a
  bibliography reference. Inspect it before moving or deleting it; an
  interrupted or externally edited operation may explain it.
- Item removal and detach move managed files under `.trash`. Use `trash list`
  to locate them. Lantai has no automatic restore command; restoration requires
  moving the file safely and recreating the reference, or reattaching the file.
- External attachment references are never moved or deleted by Lantai and must
  be backed up independently.

## Security

- Both servers must remain loopback-only. Do not publish them through a reverse
  proxy, tunnel, container port mapping, or permissive firewall rule.
- Protect `config.toml`: its bearer token permits all native API operations.
- Pass tokens in headers and preferably environment/credential stores, not
  command-line arguments or committed scripts.
- Post-save hooks and Git-style extensions execute local programs with the
  user's permissions. Install only trusted scripts and quote data at process
  boundaries. Lantai does not invoke a shell for hook configuration.
- Attachment filenames are sanitized and managed paths checked, but external
  paths still refer to user-controlled filesystem locations.

## Common failures

### “No library configured”

Pass `--library`, set `LANTAI_LIBRARY`, or run `init`. Confirm the command uses
the expected user account and platform configuration directory.

### Daemon authentication fails

The CLI does not fall back after a 401. Confirm the running daemon and CLI use
the same configuration and token, then restart the daemon after intentional
configuration replacement.

### Revision conflict

Another writer changed the library after the client's ETag. Fetch current
state, recompute the intended patch, and retry with the new ETag. Do not
automatically replay a stale destructive request.

### Port 23119 is already in use

Quit Zotero and any other compatible local server. Port 23119 is fixed for the
unmodified Connector. The native API address is configurable but must remain
loopback-only.

### Attachment too large

Increase `attachment_limit_bytes` deliberately and restart the daemon, or use a
smaller file. The limit defaults to 512 MiB and applies to CLI copies, REST
uploads, and Connector uploads.

### A post-save hook fails

The original save remains committed and returns success. Read daemon/CLI stderr
for launch, exit, or timeout warnings; run the hook manually with a captured
event; and remember that hook stdout is discarded. See [Post-save hooks](post-save-hooks.md).

### CLI output is not valid JSON

`list` and `show` default to JSON. Other built-ins need `--json`. Human
diagnostics go to stderr; ensure wrapper scripts do not merge stderr into
stdout. Custom extensions own their output contract.

## Report useful diagnostics

When investigating locally, record:

```sh
lantai --version
lantai check --json
```

Also note whether the command used direct or daemon mode, the selected paths,
the operating system, and relevant stderr. Remove bearer tokens, private
bibliographic content, URLs, and attachment data before sharing diagnostics.
