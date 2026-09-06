---
name: jujuleaf
description: Safely interact with Overleaf and self-hosted Overleaf projects through the `jujuleaf` CLI, including authentication profiles, project and file discovery, exact-text and UTF-16 OT editing, tracked changes, comments, compilation and PDF download, and local-first Jujutsu clone, pull, push, sync, undo, and redo workflows. Use when an agent needs to read, edit, review, compile, synchronize, or manage an Overleaf-hosted LaTeX paper, thesis, manuscript, bibliography, or project with JujuLeaf.
---

# JujuLeaf

Use the `jujuleaf` executable to work with Overleaf while preserving editor
semantics, synchronization checkpoints, and recoverable local history.

## Follow the safety rules

- Inspect the project and target document before mutating remote state.
- Prefer exact-text edits with an expected source string. Avoid full-document
  replacement for ordinary edits.
- Reject ambiguity. If text appears more than once, add context or select an
  inspected occurrence explicitly; never guess.
- Treat positions as CodeMirror UTF-16 code units, not UTF-8 bytes or Unicode
  scalar counts. Obtain positions with `locate` whenever possible.
- Use `--dry-run` before sensitive range edits or multi-change transactions.
- Re-read the affected document after a mutation. Compile when the task requires
  a valid document or when the edit can affect LaTeX syntax.
- Use tracked changes when the user asks for suggestions, reviewable changes, or
  co-author approval.
- Obtain explicit user intent before permanent delete commands or `--all`.
- Never expose session cookies. Do not ask the user to paste a cookie into chat;
  prefer interactive browser login.
- Do not use `apply-ops` unless the user explicitly needs low-level wire OT and
  the current document snapshot and OT format have been inspected.

## Check availability and authentication

Check the installed CLI without changing remote state:

```bash
jujuleaf --version
jujuleaf profile list --raw
```

If `jujuleaf` is unavailable, tell the user to install it from the JujuLeaf
GitHub Releases page or from source. Do not silently install software.

Attempt the requested read-only command before starting login. If authentication
is missing, have the user complete the appropriate browser flow:

```bash
jujuleaf login --profile official --preset overleaf
jujuleaf login --profile cstcloud --preset cstcloud
jujuleaf login --profile company --base-url https://overleaf.example.org
```

Use a named profile when multiple accounts or endpoints exist. Outside a clone,
select it explicitly with `--profile NAME`. Inside a clone, keep the profile
bound at clone time; do not override it with a different profile.

## Choose output for the consumer

Use `--raw` for compact JSON that the agent will parse. Use `--pretty` for
JSON shown to a human. Never combine them. Use `read --content-only` when only
document text is required.

Run `jujuleaf --help` or `jujuleaf COMMAND --help` before using an unfamiliar
or destructive command. Do not parse human-readable tables when JSON is
available.

## Discover projects and documents

Resolve names to IDs and paths before acting:

```bash
jujuleaf --profile NAME projects --raw
jujuleaf --profile NAME files PROJECT_ID --raw
jujuleaf --profile NAME read PROJECT_ID main.tex --content-only
jujuleaf --profile NAME read PROJECT_ID main.tex --meta --raw
```

Use the project ID returned by `projects`. Use paths returned by `files`;
do not infer nested paths from display names.

## Make a precise remote edit

Read the current content, locate the exact source, preview the operation, apply
it, then read the document again:

```bash
jujuleaf read PROJECT_ID main.tex --content-only
jujuleaf locate PROJECT_ID main.tex --text 'exact source text' --raw
jujuleaf replace PROJECT_ID main.tex \
  --old 'exact source text' --new 'replacement text' --dry-run --pretty
jujuleaf replace PROJECT_ID main.tex \
  --old 'exact source text' --new 'replacement text' --raw
jujuleaf read PROJECT_ID main.tex --content-only
```

When `locate` returns multiple matches, either enlarge `--old` with stable
surrounding context or deliberately use one selector:

```bash
jujuleaf replace PROJECT_ID main.tex \
  --old 'repeated term' --new 'replacement' --occurrence 2 --raw
```

Use `--position` only with an inspected UTF-16 position. For range replacement,
include `--old` as a compare-before-write precondition:

```bash
jujuleaf replace PROJECT_ID main.tex \
  --from 120 --to 126 --old 'source' --new 'target' --raw
```

For one atomic multi-edit transaction, pass sorted, non-overlapping changes.
Interpret every range against the same original snapshot:

```bash
jujuleaf apply-changes PROJECT_ID main.tex --changes \
  '[{"from":10,"to":16,"insert":"new","expect":"oldest"}]' --raw
```

Prefer a local clone for broad rewrites or coordinated multi-file changes.

## Submit reviewable changes and comments

Create tracked changes instead of direct edits when review is expected:

```bash
jujuleaf suggest PROJECT_ID main.tex \
  --old 'original sentence' --new 'suggested sentence' --raw
```

Anchor a comment to exact text, placing selector options before the trailing
comment text:

```bash
jujuleaf add-comment --raw PROJECT_ID main.tex \
  --at-text 'text to discuss' 'Please verify this claim.'
jujuleaf threads PROJECT_ID --raw
```

Inspect thread and document IDs before replying, resolving, reopening, editing,
or deleting comments. Do not resolve or delete collaborators' threads unless the
user requests it.

## Use the local-first workflow

Use a clone for repeated work, large edits, multiple files, binary assets, or
recoverable local history:

```bash
jujuleaf --profile NAME clone PROJECT_ID ./paper --raw
cd ./paper
jujuleaf status --raw
# Edit normal project files with the available workspace tools.
jujuleaf checkpoint -m 'Describe the coherent change' --raw
jujuleaf sync --raw
jujuleaf status --raw
```

Treat `sync` as a serialized `pull` then `push`. Let JujuLeaf perform the
base/version/hash checks; do not bypass a conflict with a full remote overwrite.

Use these commands deliberately:

- Run `pull` to accept remote text, metadata, and binary updates without
  overwriting concurrent local changes.
- Run `push` to publish local changes only after inspecting status.
- Run `checkpoint` at coherent task boundaries.
- Run `undo` and `redo` for JujuLeaf-managed local history; do not manipulate
  the internal `.jj` repository directly.
- Run `sync --watch` only when the user wants a foreground long-running sync
  loop. Expect it to stop on a conflict or Ctrl+C.

Do not hand-edit `.jj/jujuleaf/` state or
`.jujuleaf/remote-metadata.json`. Do not upload those private/audit paths.
Publish local deletions and structural changes only through the explicit
`delete-*`, `create-*`, `rename`, and `move` commands. Local file deletion
alone is not an instruction to delete remote data.

## Compile and retrieve outputs

Compile and inspect the structured success flag and diagnostics:

```bash
jujuleaf compile PROJECT_ID --raw
jujuleaf compile PROJECT_ID --show-log --raw
jujuleaf pdf PROJECT_ID --output output.pdf --raw
```

Do not claim compilation succeeded merely because the process exited. Inspect
the returned result and diagnostics. Use `--log-output PATH` when a persistent
log is useful for debugging.

## Handle conflicts and uncertain writes

On a sync conflict:

1. Stop further pushes.
2. Run `jujuleaf status --raw`.
3. Inspect the preserved remote copies under
   `.jj/jujuleaf/incoming/` or `.jj/jujuleaf/incoming-assets/`.
4. Merge intentionally into the working copy.
5. Checkpoint the resolution, pull again, and only then sync or push.

After a connection loss or uncertain write, do not manually repeat the raw edit.
Run `pull`, `push`, or `sync` so JujuLeaf can reconcile its SQLite receipt
against the expected remote content hash.

Report conflicts, authentication requirements, compile failures, and uncertain
outcomes explicitly. Never describe an unconfirmed remote mutation as complete.
