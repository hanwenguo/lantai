# Development guidelines

These instructions apply to the entire repository.

## Working tree and scope

- Inspect `git status --short --branch` and the relevant diffs before editing and
  again before committing. Preserve user changes and do not fold unrelated work
  into a commit.
- Keep changes narrowly scoped. Update tests and user-facing documentation in
  the same change as the behavior they describe.
- Treat documentation as part of every feature. Whenever a user-visible
  feature, interface, default, limitation, or workflow is added or changed,
  update the affected user-manual pages, reference material, examples, and
  navigation in the same change. A feature is not complete while its
  documentation is missing or stale.
- `Cargo.lock` is committed because Lantai is an application. Commit useful
  `proptest-regressions/` seeds, but never commit `target/`, `output/`, local
  configuration, bearer tokens, browser profiles, or captured attachment data.
- Preserve Lantai's core safety properties: source-aware BibLaTeX retention,
  locked and atomic mutation, loopback-only listeners, Connector request
  filtering, REST authentication, bounded streaming, and managed-path checks.

## Verification

Run checks in proportion to the change:

- Documentation-only changes: `git diff --check` and review rendered Markdown
  structure and links.
- Rust changes: `cargo fmt --check`, `cargo test --all-targets`, and
  `cargo clippy --all-targets -- -D warnings`.
- Storage, concurrency, attachment, REST, or Connector changes: run the focused
  test while iterating, then the complete Rust checks above before committing.
- Release preparation: additionally run `cargo build --release`, confirm the
  intended version with `cargo metadata --no-deps`, and perform the official
  Zotero Connector acceptance matrix when Connector-visible behavior changed.

Do not claim a stronger verification level than was actually run.

GitHub Actions repeats the complete Rust checks on Linux and macOS for pushes to
`main` and for pull requests, and type-checks against the `rust-version` in
`Cargo.toml`. CI is a backstop, not a substitute for running the checks locally
before committing.

## Commits

- Create a commit when the user explicitly asks for one, when completing an
  agreed release, or when a coherent implementation milestone needs a durable
  checkpoint. Do not create speculative or automatic commits during ordinary
  exploration.
- Commit only after reviewing `git diff`, `git diff --cached`, and the final
  status. Stage paths deliberately; never use a destructive cleanup to obtain a
  clean tree.
- Each commit should be coherent and buildable. Avoid `WIP` commits on the main
  branch. Keep refactors separate from unrelated behavior changes when that
  separation makes review or rollback safer.
- Use a concise imperative subject in the form `<type>: <summary>`, normally
  `feat`, `fix`, `docs`, `test`, `refactor`, `build`, or `release`. Explain the
  reason and important compatibility or migration details in the body when the
  subject is not enough.
- Do not amend, rebase, tag, push, or force-push unless the user explicitly asks
  for that operation.

## Versions and releases

- `Cargo.toml` is the version source of truth. The binary version and Zotero's
  `X-Zotero-Version` response derive from `CARGO_PKG_VERSION`; do not introduce a
  second hard-coded version constant.
- Bump the version only when preparing a release or when the user explicitly
  requests it. Ordinary feature, fix, test, and documentation commits do not
  receive independent version bumps.
- Follow Semantic Versioning. Before 1.0, increment the patch component for
  compatible fixes and internal improvements, and the minor component for new
  user-visible capabilities or intentional compatibility breaks. Move to 1.0.0
  only when the public CLI, REST API, storage rules, and Connector contract are
  declared stable.
- Change the package version with Cargo-aware tooling when practical (for
  example, `cargo set-version X.Y.Z` if installed); otherwise edit
  `Cargo.toml`, then run Cargo so the root package entry in `Cargo.lock` is
  regenerated. Never hand-edit unrelated locked dependency versions.
- Update version-specific README or protocol wording in the same release
  change. A pure version bump must not silently alter the `.bib` format, API, or
  Connector capabilities.
- Use a final release commit subject such as `release: 0.2.0` when a commit only
  prepares a release. Create the matching annotated `v0.2.0` tag only after the
  release commit passes all checks and only when tagging was requested.
- Pushing a `vX.Y.Z` tag is a publishing action: the release workflow verifies
  that the tag matches `Cargo.toml`, re-runs the checks, and publishes Linux and
  macOS binaries as a public GitHub release. Push a tag only when the user asks
  for that release.
