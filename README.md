# JujuLeaf

Local-first Overleaf collaboration, powered by Jujutsu.

JujuLeaf is a Rust CLI that translates precise editor changes into Overleaf's
real-time OT protocol and stores local project versions in a native Jujutsu
workspace. It is designed for both humans and coding agents: commands are
predictable, mutations are checked against their expected source text, and
normal output is designed for humans. Use `--raw` for compact JSON or `--pretty`
for indented JSON when scripting.

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
- Character-level local diffs: `push` emits only changed UTF-16 spans instead
  of replacing the whole document, so unrelated comment/range anchors survive.
- Versioned remote metadata snapshots plus hash-protected binary synchronization.
- Compile-log download and file/line diagnostic parsing.

## Install

Rust 1.92 or newer is required.

```sh
cargo install --path .
```

Tagged releases also provide a prebuilt Linux AMD64 archive on the
[GitHub Releases page](https://github.com/CsomePro/jujuleaf/releases). Download
the `x86_64-unknown-linux-gnu` archive and verify it with the accompanying
`SHA256SUMS` file before placing `jujuleaf` on your `PATH`.

Maintainers publish a release by making the Cargo package version and tag
match, then pushing the tag:

```sh
git tag -a v0.1.0 -m 'JujuLeaf v0.1.0'
git push origin v0.1.0
```

The tag workflow runs formatting, lint, and tests before it builds the binary
and creates the GitHub Release. A tag such as `v0.1.0` is rejected unless
`Cargo.toml` also contains `version = "0.1.0"`.

During development:

```sh
cargo run -- --help
cargo test --all-targets
```

## Output formats

Commands are human-readable by default. Add `--raw` for compact JSON suitable
for `jq` and automation, or `--pretty` for indented JSON:

```sh
jujuleaf projects
jujuleaf projects --raw | jq '.projects[].name'
jujuleaf projects --pretty
```

`read --content-only` prints just the source text, without a table, label, or
JSON wrapper:

```sh
jujuleaf read PROJECT_ID main.tex --content-only
```

## Login

Interactive login launches an isolated Chrome/Chromium profile and captures only
the Overleaf session and load-balancer cookies:

```sh
jujuleaf login --profile official --preset overleaf
```

The default `auto` preset detects `overleaf_session2`, `overleaf.sid`, and
legacy `sharelatex.sid` session cookies. It retains `GCLB` or `latex-session`
when the target instance uses one for load-balancer affinity.

CSTCloud SSO has a dedicated preset:

```sh
jujuleaf login --profile cstcloud --preset cstcloud
```

It opens `https://latex.cstcloud.cn/oidc/login`, lets the browser complete the
CSTCloud AAI redirect, and captures `overleaf.sid` plus `latex-session`. An
anonymous CSTCloud page already has an `overleaf.sid`, so JujuLeaf does not
treat cookie presence as success: it waits until Chrome returns to the target
`/project` page and both `ol-user_id` and `ol-csrfToken` are present. Cookies
from `aai.cstcloud.net` or any other domain are never saved.

You can also provide an existing cookie:

```sh
jujuleaf login --cookie 'overleaf_session2=...'
jujuleaf login --profile cstcloud --preset cstcloud --cookie 'overleaf.sid=...'
```

Self-hosted Overleaf is supported:

```sh
jujuleaf login --profile company --base-url https://overleaf.example.org
```

Available presets are `auto`, `overleaf`, and `cstcloud`. `--instance` is an
alias for `--preset`; an explicit `--base-url` overrides the preset endpoint.

## Multiple profiles

A profile combines one Overleaf account cookie with one endpoint. The profile
option is global and can appear before or after the command:

```sh
jujuleaf login --profile official --preset overleaf
jujuleaf login --profile company --base-url https://overleaf.example.org
jujuleaf login --profile cstcloud --preset cstcloud
jujuleaf profile list
jujuleaf profile show company
jujuleaf profile use company
jujuleaf --profile official projects
jujuleaf profile delete company
```

Login creates or replaces the selected profile and makes it active. `profile
list` and `profile show` never print cookies. Outside a local clone, remote
commands use an explicit `--profile` or fall back to the active profile.

Profiles are stored under the platform configuration directory:

```text
~/.config/jujuleaf/
├── active-profile
└── profiles/
    ├── default.json
    ├── official.json
    └── company.json
```

Directories use mode `0700` and files use mode `0600` on Unix. An existing
JujuLeaf `session.json`, or an old `overleaf-cli` session, is migrated once to
the `default` profile.

## Precise remote editing

Read and locate text:

```sh
jujuleaf read PROJECT_ID main.tex --content-only
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
jujuleaf --profile company clone PROJECT_ID paper
cd paper
jujuleaf status
```

The selected profile name and endpoint are recorded in
`.jj/jujuleaf/project.json`. Later `pull`, `push`, and `sync` commands use that
bound profile automatically, even if the globally active profile changes. An
explicit, different `--profile` is rejected before synchronization.

Remote comment threads, document ranges, and history-OT metadata (including
tracked changes) are captured in `.jujuleaf/remote-metadata.json`. This file is
part of the Jujutsu working copy, so metadata changes can be inspected and
restored alongside source changes. Operational baselines and receipts remain
private under `.jj/jujuleaf/sync.sqlite3`.

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
It calculates a character-level diff and emits separate CodeMirror UTF-16
changes, leaving unchanged spans out of the OT operation. A direct `push`
refuses a local text write if remote ranges or tracked-change metadata changed
since the last accepted checkpoint; run `pull`/`sync` first to accept that
metadata state.

Uploaded binary files use the same local/base/remote hash comparison. Pull can
add, update, and remove unchanged local binaries. Push uploads new binaries in
existing remote folders and safely replaces changed binaries by retaining a
temporary remote backup until the new upload succeeds. Local deletion is not
published implicitly; use `delete-file` for an intentional remote deletion.

If the connection closes after submission, JujuLeaf reconnects, reads the
document, and compares its hash to the operation's expected result. Uncertain
receipts remain in SQLite and are reconciled on the next pull or push.

Local undo and redo are native Jujutsu operations:

```sh
jujuleaf undo
jujuleaf redo
```

Keep synchronizing in the foreground while an editor is open:

```sh
jujuleaf sync --watch
jujuleaf sync --watch --interval 750
```

This process polls both local and remote state, performs one pull→push cycle at
a time, and stops on Ctrl+C or the first conflict. It is not a background
daemon; background service management is intentionally deferred.

Compile waits for Overleaf's compile response, downloads `output.log`, and
prints parsed diagnostics:

```sh
jujuleaf compile PROJECT_ID
jujuleaf compile PROJECT_ID --show-log
jujuleaf compile PROJECT_ID --log-output build.log --timeout 720
```

## Other commands

Run `jujuleaf --help` for the full list. The Rust CLI includes project
creation/rename, file and folder management, upload/download/delete, compile/PDF/ZIP,
comments and threads, history/diff/search, word count, real-time watch,
low-level OT validation, and tracked-change acceptance.

## Architecture

See [docs/architecture.zh-CN.md](docs/architecture.zh-CN.md) for the protocol,
Jujutsu model, synchronization state machine, and concurrency design.

## Current boundaries

- New binary files are uploaded automatically only when their parent folder
  already exists remotely. Creating/renaming/moving folders and documents from
  arbitrary local filesystem changes still requires the explicit entity
  commands.
- A true simultaneous three-way text merge is deliberately not automatic.
  Conflicting remote content is kept under `.jj/jujuleaf/incoming/`; resolve it
  in the working copy, checkpoint, pull again, then push.
- Overleaf may evict old operations. `joinDoc(fromVersion)` is implemented, but
  callers must fall back to a full snapshot when the server reports missing
  operations.
- `sync --watch` is a foreground polling loop, not an installed background
  daemon. Daemon/service support is planned after the recovery semantics above
  are hardened.

## License

MIT. Jujutsu (`jj-lib`) is used under Apache-2.0.
