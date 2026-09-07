---
name: jujuleaf
description: Safely interact with Overleaf and self-hosted Overleaf projects through the `jujuleaf` CLI, including authentication profiles, exact-text and UTF-16 OT editing, tracked changes, comments, compilation, and Jujutsu-backed sync, begin, review, finish, abort, undo, and redo workflows. Use when an agent needs to read, edit, review, compile, synchronize, or manage an Overleaf-hosted LaTeX paper, thesis, manuscript, bibliography, or project with JujuLeaf.
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
jujuleaf auth status --raw
jujuleaf doctor --raw
```

If `jujuleaf` is unavailable, tell the user to install the official prebuilt
release with `cargo binstall --strategies crate-meta-data jujuleaf`. Link to
the JujuLeaf GitHub Releases page as a manual fallback. Do not silently install
software.

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

Use `doctor --offline --raw` when a network check is inappropriate. `auth status`
does not expose the cookie. Run `auth logout` only when the user explicitly asks
to remove the selected local profile and its credentials.

## Choose output for the consumer

Use `--raw` for compact, ANSI-free JSON that the agent will parse.
Use `--pretty` only when indented JSON materially helps a human.
Never combine `--raw` and `--pretty`. Use `read --content-only` when only
document text is required.

Run `jujuleaf --no-color --help` or `jujuleaf COMMAND --no-color --help`
before using an unfamiliar or destructive command. Do not parse human-readable
tables when JSON is available.

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

Inside a JujuLeaf clone or any child directory, omit `PROJECT_ID`. The CLI
discovers the nearest `.jujuleaf/project.json` or compatible clone binding and
uses its project and profile automatically:

```bash
jujuleaf files --raw
jujuleaf read main.tex --content-only
jujuleaf locate main.tex --text 'exact source text' --raw
jujuleaf compile --raw
```

The `PROJECT_ID` placeholders below are for calls outside a clone. Remove that
argument when operating on the current clone. To target a different project
from inside a clone, put `--project-id OTHER_PROJECT_ID` before the command;
use this explicit override for commands with trailing text such as `search`,
`comment`, and `add-comment`.

## Make a precise remote edit

Read the current content, locate the exact source, preview the operation, apply
it, then read the document again:

```bash
jujuleaf read PROJECT_ID main.tex --content-only
jujuleaf locate PROJECT_ID main.tex --text 'exact source text' --raw
jujuleaf replace PROJECT_ID main.tex \
  --old 'exact source text' --new 'replacement text' --dry-run --raw
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

For one isolated remote edit outside a local review batch, create a tracked
change instead of a direct edit:

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

## Review 修改范式

Use this as the default local workflow whenever collaborators should review a
coherent text change before it becomes the accepted synchronized baseline:

```bash
cd ./paper
jujuleaf sync --raw
jujuleaf begin -m 'Rewrite the introduction' --raw
# Edit existing text documents with the available workspace tools.
jujuleaf review diff --raw
jujuleaf review submit --raw
jujuleaf review status --raw
# Reviewers accept or reject every tracked change in Overleaf.
jujuleaf review finish --raw
jujuleaf begin -m 'Start the next coherent change' --raw
```

Interpret the boundary as `synchronized parent -> described child work`.
`begin` requires a clean synchronized baseline with no pending tracked
changes, then creates a new described Jujutsu child. It is the work boundary;
`checkpoint` only snapshots the current child and does not start another one.

Apply these review rules:

- Use `review diff` before submission. It verifies that every remote document
  still has the version, visible content, comments, and tracked-change metadata
  captured at `begin`.
- Use `review submit` to publish the local diff as Overleaf tracked changes.
  Do not use direct `push`, `sync`, or per-file `suggest` for that batch.
- Treat submitted local files as frozen. `review status` reports owned pending
  changes, foreign pending changes, unsubmitted files, unresolved receipts,
  post-submit local edits, and `readyToFinish`.
- Add review comments after `review submit`, then inspect or reply through the
  thread commands. Comments are reconciled by `review finish`.
- Run `review finish` only after every tracked change in the project is
  accepted or rejected. It takes the reviewed remote text as final, pulls all
  remote state, preserves the work description, and closes the review state.
- If a multi-document submission stops after at least one remote attempt,
  inspect the `unsubmittedFiles` from `review status`, resolve every tracked
  change already present in Overleaf, and run `review finish`. Finishing reports
  those files and replaces their unsubmitted local proposals with remote text.
- Run `review abort` only before any remote submission attempt. It restores the
  synchronized parent and keeps the discarded draft recoverable in Jujutsu
  operation history. A stopped submission with no remote attempt can therefore
  be aborted; after an attempt, resolve the remote review and finish it instead.
- Retry `review finish` after an interruption. Its persisted `finishing` phase
  accepts either the frozen proposal or the already-written reviewed result.
- Do not add new files or change binary files inside this review workflow.
  Perform explicit structural/binary operations before `sync` and `begin`.

An active review blocks `pull`, `push`, `sync`, `undo`, and `redo`. Mutating
workspace commands also share a process lock, including the full lifetime of
`sync --watch`. This prevents a tracked review batch from being accidentally
published as ordinary direct edits.

Use `suggest` only for an isolated remote suggestion outside an active local
review batch. It changes Overleaf directly and does not advance a clone's local
synchronization checkpoint; run `pull` or `sync` before the next `begin`.

## Use direct local synchronization deliberately

Use a clone for direct edits, binary assets, or work that does not require
Overleaf review:

```bash
jujuleaf --profile NAME clone PROJECT_ID ./paper --raw
cd ./paper
jujuleaf status --raw
# Edit normal project files with the available workspace tools.
jujuleaf checkpoint -m 'Describe the coherent direct change' --raw
jujuleaf sync --raw
jujuleaf status --raw
```

Treat `sync` as a serialized `pull` then `push`. Let JujuLeaf perform the
base/version/hash checks; do not bypass a conflict with a full remote overwrite.

Use these commands deliberately:

- Run `pull` to accept remote text, metadata, and binary updates without
  overwriting concurrent local changes.
- Run `push` only for intentionally direct publication outside a review.
- Run `checkpoint` to snapshot the current work, not to create a work boundary.
- Run `local log`, `local show REVISION`, and `local diff` to inspect embedded
  Jujutsu history. Prefer these read-only commands before a restoration.
- Run `jujuleaf` without a subcommand for the compact current first-parent
  commit graph. It snapshots pending working-copy changes; use `jujuleaf --raw`
  when structured full commit IDs are needed.
- Run `local restore REVISION` to restore a selected tree as a new recoverable
  operation; do not assume it erases later history.
- Run `undo` and `redo` for JujuLeaf-managed local history outside an active
  review; do not manipulate the internal `.jj` repository directly.
- Run `sync --watch` only for a foreground direct-sync loop. Expect it to stop
  on a conflict or Ctrl+C.

## Interoperate with Git history

A JujuLeaf clone already has a bare Git repository inside `.jj`. Use JujuLeaf's
Git commands rather than creating or manipulating a second `.git` worktree:

```bash
jujuleaf git root --raw
jujuleaf git remote list --raw
jujuleaf git remote add origin GIT_URL --raw
jujuleaf git fetch --remote origin --raw
jujuleaf git push --remote origin --branch main --raw
```

Use `jujuleaf git remote remove`, `remote rename`, or `remote set-url` for remote
configuration. Outside the clone, add `-R PATH`. Create a JujuLeaf workspace
with `jujuleaf clone`; there is no separate `jujuleaf git clone` or `git init`
flow.

`git push` checkpoints pending files and publishes the current Jujutsu
working-copy commit. It uses the last fetched remote position as a lease. If a
remote branch may already exist or has advanced, fetch it before pushing and do
not bypass a rejection. `git fetch` imports remote history but does not check it
out into the working copy. These mutating Git commands are blocked during an
active review. Let the system Git executable use its configured SSH agent or
credential helper; never expose credentials in command arguments or output.

The compact graph uses jj-style 8-character display IDs. Other human output
abbreviates Jujutsu IDs to unambiguous-looking 12-character prefixes. Agent
JSON from `--raw` retains complete IDs. Revision-taking local commands accept
an unambiguous prefix and request more characters if needed.

Put local generated or private paths in a root-level `.jujuleafignore`, using
gitignore syntax. This affects discovery and checkpointing of new files; it does
not untrack an already synchronized Overleaf entity. JujuLeaf always excludes
`.jj`, `.git`, `.jujuleaf`, and `.jujuleafignore` from upload discovery.

Do not hand-edit `.jj/jujuleaf/` state, `.jujuleaf/project.json`, or
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
2. Run `jujuleaf conflict list --raw` and
   `jujuleaf conflict show PATH --raw`.
3. Compare the local file, preserved remote state, and returned patch. Do not
   choose a side based only on the words "ours" or "theirs".
4. Resolve exactly one path with an explicit choice:
   - `jujuleaf conflict resolve PATH --ours --raw` keeps the working-copy file.
   - `jujuleaf conflict resolve PATH --theirs --raw` accepts the preserved
     remote state.
   - `jujuleaf conflict resolve PATH --merged FILE --raw` uses a reviewed merge.
5. Never choose `--ours` or `--theirs` without user intent unless the task
   already states which version is authoritative. Prefer `--merged` when both
   sides contain required work.
6. Run `jujuleaf status --raw`, then `sync --raw` only after every conflict is
   resolved. Resolution creates its own recoverable Jujutsu checkpoint.

If `review diff` detects remote drift while still in draft, run
`review abort`, synchronize, and start a new `begin`. Once submission starts,
never delete the review state or retry an edit with standalone `suggest`. Rerun
`review submit` for its receipt-aware recovery. If a later document has drifted
after an earlier document was attempted, inspect `review status`, resolve every
tracked change already on the project, and run `review finish`; its
`unsubmittedFiles` are intentionally replaced by remote text. If no remote
attempt was made, `review abort` is safe.

After a connection loss or uncertain write outside an active review, do not
manually repeat the raw edit. Run `pull`, `push`, or `sync` so JujuLeaf can
reconcile its SQLite receipt against the expected remote content hash.

Report conflicts, authentication requirements, compile failures, and uncertain
outcomes explicitly. Never describe an unconfirmed remote mutation as complete.
