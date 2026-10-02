<p align="center">
  <img src="https://raw.githubusercontent.com/CsomePro/jujuleaf/v0.4.2-rc.1/assets/jujuleaf-logo.png" width="140" alt="JujuLeaf logo">
</p>

<h1 align="center">JujuLeaf v0.4.2-rc.1</h1>

<p align="center"><strong>Clear Jujutsu change boundaries and recoverable Git backups</strong></p>

> This is a cross-platform prerelease for validating the new local-history
> model. Keep a backup when testing it with an important project.

## Meaningful change history

- A successful `review finish` now preserves the reconciled result as a stable
  parent change and moves the same workspace to a fresh blank child.
- The next `begin` reuses that blank child instead of creating another empty
  layer or another physical workspace.
- Lightweight checkpoints remain in the recoverable Jujutsu operation journal;
  they do not become noisy semantic commits.
- Interrupted `review finish` retries recognize an already-created blank child
  and do not duplicate completed changes.

## Separate change and operation views

~~~bash
jujuleaf log
jujuleaf op log
jujuleaf op show REVISION
jujuleaf op diff
jujuleaf op restore REVISION
~~~

- `log` displays the current first-parent change graph.
- `op` commands inspect checkpoint and recovery operations.
- The previous `local` command forms remain accepted as compatibility aliases.
- Running `jujuleaf` without a subcommand remains the compact default form of
  the change graph inside a clone.

## Git backup boundaries

- `jujuleaf git push` snapshots all pending files before contacting the remote.
- The exact current state is frozen as a parent commit, the workspace moves to
  a blank child, and the frozen parent is pushed to Git.
- Later edits no longer rewrite the version that was backed up.
- A lease rejection or network failure leaves the frozen state recoverable in
  the local Jujutsu graph.

`git fetch` still imports remote history without checking it out or merging it
automatically. Remote-snapshot rebase/merge is intentionally reserved for the
next development phase.

## Install or upgrade

~~~bash
cargo binstall --strategies crate-meta-data --force jujuleaf@0.4.2-rc.1
jujuleaf --version
~~~

The reported version should be `jujuleaf 0.4.2-rc.1`.

## Release targets

- Linux AMD64 and ARM64: fully static musl binaries
- macOS Apple Silicon and Intel: single binaries using Apple system libraries
- Windows AMD64: single executable with the static MSVC runtime

- [README](https://github.com/CsomePro/jujuleaf/blob/v0.4.2-rc.1/README.md)
- [中文 README](https://github.com/CsomePro/jujuleaf/blob/v0.4.2-rc.1/README.zh.md)
- [Changes since v0.4.1](https://github.com/CsomePro/jujuleaf/compare/v0.4.1...v0.4.2-rc.1)
