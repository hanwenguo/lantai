---
name: lantai
description: Query and operate on the user's personal academic reference library through the `lantai` CLI — a headless BibLaTeX reference manager holding their papers, abstracts, PDFs, and topic collections. Use this skill whenever a request touches the user's own literature: finding papers they've saved, gathering related work or prior-art context for a research question they're thinking through, checking whether they already have a paper on some topic, inserting \cite{} or @key citations while drafting a paper, reading or locating a PDF they've filed, filing items into collections, adding new entries, or auditing the library for missing DOIs and other problems. Trigger it even when the user never says "lantai", "bibliography", or "library" — phrases like "what do I have on X", "papers I've read about X", "cite the AGT paper", "who else has done this", "pull up that paper by so-and-so", or a research question asked in a repo where they write papers all mean this library should be consulted first. Prefer this over a web search when the question is about literature the user has already collected.
---

# Lantai: the user's reference library

Lantai is a headless reference manager whose entire database is one BibLaTeX
file plus a directory of managed attachments. Everything is done through the
`lantai` CLI; there is no GUI and no separate index. Read operations are cheap
and safe, so explore freely.

The library is a working researcher's collection: real abstracts, real PDFs,
and a hand-maintained topic tree. That makes it a far better source for "what
do I know about X" than a web search — the user has already curated it, and
their citation keys are what ends up in their papers.

## Orient yourself first

Two commands, once per session, before anything else:

```sh
lantai check                        # library path, attachment dir, entry count, health
lantai collection list --format human   # the topic tree
```

`check` also tells you the bibliography's directory, which is what attachment
paths resolve against. `collection list --format human` shows the tree
indented; `--format json` gives the flat `/`-joined names that filters expect.

If `lantai` isn't on PATH or `check` reports a problem, say so and stop rather
than guessing at the library's contents — a confident answer from a library
you couldn't read is worse than no answer. (This skill is about *using* the
library. Working on Lantai's own Rust source is ordinary code work; the repo's
`AGENTS.md` governs that.)

**Collection filters need the full path.** Names are flat strings that merely
*spell* nesting with `/`, and `collection:` matches a prefix, not a substring.
On a library with `ResearchTopics/GradualTyping`, `collection:GradualTyping`
matches nothing at all and exits 0 — it looks exactly like "no such papers".
Always take the name from `collection list` and pass it whole:
`collection:ResearchTopics/GradualTyping`.

## Searching

`lantai list TERM...` takes query terms; an item must match all of them.
Matching is case-insensitive substring matching, not semantic search.

| Term | Matches |
| --- | --- |
| `WORD` | citation key or **any** field value contains WORD |
| `title:WORD`, `abstract:WORD`, `author:WORD`, `keywords:WORD` | that field contains WORD |
| `key:WORD` | citation key contains WORD |
| `type:TYPE` | exact entry type (`article`, `inproceedings`, `book`, `thesis`, `online`, …) |
| `collection:NAME` | that collection and everything nested under it |
| `year:2020`, `year:2019..2024`, `year:..2010` | publication year or range |
| `doi:` | the field is present at all |
| `-TERM` | negation — must come after a `--` separator |
| `any:WORD` | literal WORD, for values containing a colon such as a URL |

One shell argument is one term, so a phrase is just quoted:
`lantai list title:"semantic subtyping"`.

`lantai list --help` prints this same grammar with `--sort` keys and examples,
and it comes from the binary that's actually installed. If a term ever behaves
differently from the table above, the installed version is right and this table
is stale — check `--help` and trust it.

**Scope your terms.** A bare word searches every field including the `file`
field, which holds PDF paths and media types — on a typical library
`lantai list pdf` matches nearly every entry. For anything topical, search
`title:`, `abstract:`, and `keywords:` rather than bare words.

**Never dump raw `lantai list` JSON into context.** It emits every field twice
(expanded `value` plus `raw` source) for every match; a few dozen hits is tens
of thousands of tokens. Use the bundled helper instead:

```sh
scripts/lsearch [--abstracts] [--paths] [--sort=-year] [--] TERM...
```

Paths below are written relative to this skill's own directory — invoke the
script by that full path, and set a shell variable to it once if you'll use it
repeatedly. It writes one tab-separated row per match — key, year, short authors, title —
optionally followed by the abstract, or with the first attachment's resolved
absolute path appended. Terms and `--sort` are passed through unchanged. When
you need a field it doesn't project, write your own `jq` over `lantai list`
rather than reading the whole thing.

`lantai show KEY` returns one complete item and is fine to read in full.

## Answering a research question from the library

This is the highest-value use of the library, and it needs more than one
search. The query language is literal, so the user's phrasing almost never
matches the vocabulary of the papers. Work in four passes:

**1. Map.** Read `collection list` and find the collections that plausibly
cover the question. The tree encodes how the user already thinks about their
field; a topic collection is usually a better recall net than any keyword.

**2. Expand.** Turn the question into several concrete search families before
running anything: the technical term and its synonyms, the standard technique
names, the systems or calculi involved, and the two or three authors known for
the area. A question about "how do people handle unsound casts at runtime"
becomes searches for `blame`, `cast`, `sound`, `runtime check`, `contract`,
not one search for the sentence.

**3. Search wide, over abstracts.** Run the families in parallel and union the
results. Abstracts are where a paper's actual contribution is stated, so
`abstract:` recall beats `title:` recall by a lot:

```sh
scripts/lsearch --abstracts abstract:blame
scripts/lsearch abstract:cast collection:ResearchTopics/GradualTyping
```

Deduplicate by citation key, then judge relevance from the abstracts you have
rather than from titles alone.

**4. Read deep where it matters.** For the handful of papers the answer turns
on, read the PDF: `scripts/lsearch --paths key:GCT16` gives its absolute path,
which goes straight to whatever file-reading tool you have. If yours cannot
open a PDF, extract the text first (`pdftotext FILE -`) rather than answering
from the abstract alone. Read the paper before making a claim about what it
proves.

Then answer the question as a researcher would: what the library says about
it, which papers say it, where they disagree, and what the user seems *not* to
have — a gap is a real finding, and that's the moment to offer a web search to
fill it. Do not pad the answer with papers that merely matched a string.

## Presenting results

Lead with a compact markdown table, citation key first, because the key is the
handle for everything the user does next:

| Key | Year | Authors | Title |
| --- | --- | --- | --- |
| `GCT16` | 2016 | Garcia et al. | Abstracting Gradual Typing |

Below the table, add prose only where it earns its place — how the papers
relate, which to read first, what's missing. Long result sets should be cut to
the relevant ones with a note of how many matched in total, not truncated
silently. Show raw JSON only when asked for it.

## Citations while drafting a paper

When the user is writing and wants a citation, resolve the paper first (search,
confirm it's the right one) and then emit the reference in their document's
format:

```sh
lantai dwim --all --action latex  key:GCT16    # \cite{GCT16}
lantai dwim --all --action typst  key:GCT16    # @GCT16
lantai dwim --all --action bibtex key:GCT16    # the BibLaTeX entry
lantai dwim --all --action keys   collection:Projects/IfT
```

`--action latex` over several matches produces one combined
`\cite{key1,key2}`. If the project keeps its own `.bib` file, export into it
rather than hand-copying: `lantai export KEY... >> refs.bib`.

## The interactive commands are a trap for agents

`lantai pick`, `lantai open`, and bare `lantai dwim` open an `fzf` picker.
Without a terminal they produce **no output and exit 0** — silently
indistinguishable from "nothing matched", which is how a wrong answer gets
confidently reported. Only ever run their non-interactive forms:

- `lantai dwim --all --action ACTION ...` (never bare `dwim`)
- `lantai batch-collection --all [--remove] COLLECTION -- TERM...`
- `lantai open --print --stdin` (prints paths instead of launching anything)

If a task genuinely wants the user to choose from a list, print the candidates
and let them run the picker themselves.

## Changing the library

The bibliography is the user's canonical data and is often under git. Prefer
UUIDs over citation keys for mutations — keys are editable and can be
ambiguous, UUIDs are stable (`lantai show KEY | jq -r .uuid`).

Ordinary edits — `add`, `set`, `set-raw`, `unset`, `attach`,
`collection add`/`remove`, `batch-collection` — go ahead once the intent is
clear. Report what changed.

**Confirm before running these, with a preview of exactly what they'd hit:**

- `lantai remove` — deletes an entry and trashes its attachments
- `lantai detach` — trashes an attachment file
- `lantai trash purge` — irreversible
- `lantai set --key` — renames a citation key, breaking every `\cite` already
  written in the user's papers
- `lantai format` — rewrites the whole file's formatting; check `git status`
  on the library directory first

Show the affected items as a table and ask. For a bulk change, run the
matching `lantai list` first and show what it selected — a query that matched
more than expected is the failure mode worth catching.

Mutations are individually locked but a batch is not atomic: `batch-collection`
and `dwim` stop at the first failure, leaving earlier changes applied. Say so
if one fails partway.

## Library hygiene

```sh
lantai check                              # syntax, identities, missing attachment files
scripts/lsearch -- type:article -doi:     # articles with no DOI
scripts/lsearch -- -collection:           # filed nowhere
scripts/lsearch -- -abstract:             # no abstract to search on
lantai list | jq -r '.[] | select(.attachments|length==0) | .citation_key'
```

Duplicate detection isn't built in; compare normalized titles with `jq` and
report candidates rather than acting on them — near-duplicates are frequently
deliberate (preprint plus proceedings version).

## Working on a different library

Selection order is `--library PATH`, then `$LANTAI_LIBRARY`, then the
configured default. The global flag must precede the subcommand:
`lantai --library ./project.bib list type:article`.

## Looking things up

This skill deliberately bundles no copy of the manual, because a copy goes
stale against the Lantai the user actually has installed. Two authoritative
sources, in this order:

**`lantai --help` and `lantai COMMAND --help` come from the installed binary**,
so they cannot disagree with it. `lantai --help` lists every command grouped by
purpose, including any custom extensions on `PATH`; each command's own help
gives its flags, and `lantai list --help` prints the full query grammar. This
is the fast path — use it for any command not shown above, and to settle any
question about spelling or flags.

**The online manual covers what `--help` cannot**: the item JSON schema, the
JSON each command emits, attachment storage layout and path resolution,
collection semantics, configuration, and the storage guarantees. Fetch the raw
Markdown with whatever retrieval tool you have, or with `curl` if you have
none. Pin the URL to the installed version so the document matches the binary
— `lantai --version` prints `lantai X.Y.Z`, and the tag is `vX.Y.Z`:

```sh
curl -fsSL https://raw.githubusercontent.com/hanwenguo/lantai/vX.Y.Z/docs/index.md
```

Fall back to `.../main/docs/...` if that tag 404s (an unreleased build). The
pages are `index.md`, `getting-started.md`, `configuration.md`,
`library-model.md`, `cli-reference.md`, `cli-workflows.md`, `operations.md`,
`rest-api.md`, `zotero-connector.md`, `post-save-hooks.md`, plus
`extension/README.md`. Start from `library-model.md` for the item JSON schema
and storage model, `cli-reference.md` for a command's JSON output.

If the user has the Lantai repository checked out, its `docs/` directory is the
same content without the network round-trip.

Two facts worth stating here because they are easy to get wrong from output
alone: an attachment `path` is relative to the **bibliography's own directory**
(`lantai check --json | jq -r .library`) unless it is absolute, and a `null`
`uuid` means an entry some external editor added that Lantai has not adopted
yet — it cannot be used as a mutation identifier until `lantai format` runs.
