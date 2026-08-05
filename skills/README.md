# Lantai agent skills

This directory holds skills that teach a coding agent to operate a Lantai
library. For the human-facing workflows they automate, see the [CLI workflow
guide](../docs/cli-workflows.md) and the [CLI reference](../docs/cli-reference.md).

A skill is a directory containing `SKILL.md` — YAML frontmatter naming the
skill and describing when it applies, followed by Markdown instructions — plus
any scripts it bundles. Agents that understand this layout keep the description
in context at all times and load the body only when a request matches it, which
is why the description is written to say *when* the skill applies and not just
what it does.

Nothing here depends on that layout, though. `SKILL.md` is ordinary Markdown
addressed to whatever agent reads it, so an agent with a different convention
can take the same instructions as a rules file, a system prompt, or context
pasted into a session. It assumes only a shell, a filesystem, and the ability
to read a PDF.

## Available skills

| Skill | Purpose |
| --- | --- |
| [`lantai`](lantai/SKILL.md) | Search the library, gather literature context for a research question, insert `\cite{}`/`@key` citations, read filed PDFs, manage collections and attachments, and audit the library |

## Install

Put `skills/lantai/` where your agent looks for skills — consult its
documentation for that directory, which is typically one for skills available
everywhere and another inside a project for skills scoped to it. Symlinking
rather than copying keeps the installed skill current, so a `git pull` updates
it:

```sh
ln -s "$PWD/skills/lantai" /path/to/agent/skills/lantai
```

Copy it instead to pin a version, at the cost of having to repeat the copy for
every update:

```sh
cp -R skills/lantai /path/to/agent/skills/lantai
```

Install it once. A skill reachable from two places at the same time — say
globally and inside a project — is ambiguous, and the agent may load either.

An agent that has no skill directory needs no installation: point it at
`skills/lantai/SKILL.md` and ask it to follow the file, or add that path to
whatever it reads for standing instructions.

To verify, ask the agent something the skill covers, such as what the library
holds on a topic. It should reach for `lantai` rather than guess, and cite
entries by their citation keys.

## Dependencies

Lantai itself, plus `jq` for the bundled `lsearch` helper. The skill deliberately
avoids the `fzf` picker extensions: `lantai pick` and the interactive forms of
`lantai open` and `lantai dwim` write nothing and exit zero when no terminal is
attached, which an agent cannot distinguish from an empty result. It uses the
non-interactive forms (`--all --action`, `batch-collection --all`,
`open --print --stdin`) instead.

## Maintenance

The skill bundles no copy of the manual. A skill installed by copying, or
symlinked from a checkout that has moved on, would otherwise describe a Lantai
its user does not have. Instead `SKILL.md` sends the agent to
`lantai COMMAND --help`, which comes from the installed binary, and to the
manual on GitHub pinned to the tag matching `lantai --version`. What the skill
states in its own voice is the part no document covers: how to search a
literal-matching library well, and which commands mislead an agent by
succeeding silently.

`lantai/scripts/lsearch` projects `lantai list` JSON down to one
tab-separated row per item, because the full JSON repeats every field in both
expanded and raw form and exhausts an agent's context on a library of any size.
Keep machine data on standard output and diagnostics on standard error, as the
[extension guide](../extension/README.md) requires of extensions.
