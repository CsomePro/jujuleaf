# JujuLeaf

Local-first Overleaf collaboration, powered by Jujutsu.

JujuLeaf is a Rust CLI that translates precise editor changes into Overleaf's
real-time OT protocol and stores local project versions in a native Jujutsu
workspace. It is designed for both humans and coding agents: commands are
predictable, mutations are checked against their expected source text, and
normal output is JSON.

> JujuLeaf uses private Overleaf web and real-time APIs. Those APIs may change.
> Keep a project backup while this project is pre-1.0.

## Why JujuLeaf?

- Safe exact-text editing: ambiguous source text is rejected by default.
- CodeMirror-compatible UTF-16 positions, including documents containing emoji.
- Both current Overleaf OT formats: `sharejs-text-ot` and `history-ot`.
- Tracked-change suggestions and anchored comments.
- Actual `otUpdateApplied` confirmation instead of treating an RPC queue ack as
  a successful edit.
- Retry deduplication with Overleaf's `publicId`/`dupIfSource` mechanism.
- Local `.jj` history through `jj-lib`; no external `jj` executable is required.
- SQLite receipts for remote versions, hashes, inflight writes, and uncertain
  network outcomes.
- Conflict-safe pull/push: if both the local file and Overleaf changed from the
  recorded base, JujuLeaf preserves both and refuses to overwrite either side.

## Install

Rust 1.92 or newer is required.

```sh
cargo install --path .
```

During development:

```sh
cargo run -- --help
cargo test --all-targets
```

## Login

Interactive login launches an isolated Chrome/Chromium profile and captures only
the Overleaf session and load-balancer cookies:

```sh
jujuleaf login
```

You can also provide an existing cookie:

```sh
jujuleaf login --cookie 'overleaf_session2=...'
```

Self-hosted Overleaf is supported:

```sh
jujuleaf login --base-url https://overleaf.example.org
```

The session is stored with mode `0600` under the platform configuration
directory. If present, the old `overleaf-cli` session is read as a migration
fallback.

## Precise remote editing

Read and locate text:

```sh
jujuleaf read PROJECT_ID main.tex --raw
jujuleaf locate PROJECT_ID main.tex --text 'exact source'
```

An exact replacement must be unique:

```sh
jujuleaf replace PROJECT_ID main.tex \
  --old 'one unique paragraph' \
  --new 'the replacement paragraph'
```

If there are several matches, choose deliberately:

```sh
jujuleaf replace PROJECT_ID main.tex --old 'term' --new 'word' --occurrence 2
jujuleaf replace PROJECT_ID main.tex --old 'term' --new 'word' --position 120
jujuleaf replace PROJECT_ID main.tex --old 'term' --new 'word' --all
```

Positions are UTF-16 offsets, exactly like CodeMirror and browser JavaScript.
You can preview the generated wire operations:

```sh
jujuleaf replace PROJECT_ID main.tex \
  --old 'before' --new 'after' --dry-run --pretty
```

Apply one CodeMirror-style transaction:

```sh
jujuleaf apply-changes PROJECT_ID main.tex --changes \
  '[{"from":10,"to":16,"insert":"new","expect":"oldest"}]'
```

Submit a suggestion instead of a direct edit:

```sh
jujuleaf suggest PROJECT_ID main.tex \
  --old 'original sentence' --new 'suggested sentence'
```

## Local-first workflow

Clone creates normal project files plus a hidden `.jj` repository:

```sh
jujuleaf clone PROJECT_ID paper
cd paper
jujuleaf status
```

Edit files with any editor, then checkpoint and synchronize:

```sh
jujuleaf checkpoint -m 'rewrite introduction'
jujuleaf sync
```

The commands also have familiar aliases:

```text
clone = init
pull  = fetch
push  = publish
```

Pull behavior for each document:

| Local vs base | Remote vs base | Result |
|---|---|---|
| unchanged | changed | update local file |
| changed | unchanged | keep local file |
| changed | same as local | mark converged |
| changed | changed differently | report conflict; save remote copy privately |

Push performs the same check again immediately before sending an OT operation.
If the connection closes after submission, JujuLeaf reconnects, reads the
document, and compares its hash to the operation's expected result. Uncertain
receipts remain in SQLite and are reconciled on the next pull or push.

Local undo and redo are native Jujutsu operations:

```sh
jujuleaf undo
jujuleaf redo
```

## Other commands

Run `jujuleaf --help` for the full list. The Rust CLI includes project
creation/rename, file and folder management, upload/download, compile/PDF/ZIP,
comments and threads, history/diff/search, word count, real-time watch,
low-level OT validation, and tracked-change acceptance.

## Architecture

See [docs/architecture.zh-CN.md](docs/architecture.zh-CN.md) for the protocol,
Jujutsu model, synchronization state machine, and concurrency design.

## Current boundaries

- Local-first pull/push treats editable Overleaf documents as first-class sync
  units. Binary assets are included at clone time and can be managed with the
  explicit `upload` command, but automatic binary delta synchronization is not
  implemented yet.
- A true simultaneous three-way text merge is deliberately not automatic.
  Conflicting remote content is kept under `.jj/jujuleaf/incoming/`; resolve it
  in the working copy, checkpoint, pull again, then push.
- Overleaf may evict old operations. `joinDoc(fromVersion)` is implemented, but
  callers must fall back to a full snapshot when the server reports missing
  operations.

## License

MIT. Jujutsu (`jj-lib`) is used under Apache-2.0.
