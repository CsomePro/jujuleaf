<p align="center">
  <img src="https://raw.githubusercontent.com/CsomePro/jujuleaf/v0.1.2/assets/jujuleaf-logo.png" width="140" alt="JujuLeaf logo">
</p>

<h1 align="center">JujuLeaf v0.1.2</h1>

<p align="center"><strong>Safer Overleaf synchronization, recoverable Jujutsu history, and a stable foundation for Agent integrations.</strong></p>

> [!IMPORTANT]
> JujuLeaf is pre-1.0 software built on private Overleaf APIs. Keep a backup and
> start with a non-critical project. Prebuilt binaries currently target Linux
> AMD64; other platforms can install from source with Cargo.

## Install or upgrade

The recommended installer downloads the prebuilt binary from this release:

```bash
cargo binstall --strategies crate-meta-data jujuleaf
```

Install from source instead:

```bash
cargo install --locked jujuleaf
```

Verify the downloaded archive against the attached `SHA256SUMS` file before a
manual installation.

## Highlights

### 🛡️ Recoverable synchronization

- Preserves remote content when local and Overleaf changes conflict.
- Adds `conflict list`, `conflict show`, and explicit ours/theirs/merged
  resolution workflows.
- Handles text documents, binary assets, remote deletions, uncertain network
  results, and concurrent-process locking without silently overwriting work.
- Adds `.jujuleafignore` for generated and local-only files.

### 🌿 Local Jujutsu history and Git backup

- Adds `local log`, `local show`, `local diff`, and recoverable `local restore`.
- Running `jujuleaf` inside a clone now shows a compact jj-style commit graph.
- Human output uses short Jujutsu IDs while JSON retains complete identifiers.
- Adds managed Git remotes, fetch, and lease-protected push through
  `jujuleaf git`, without creating a second working tree.

### 🔌 Stable Bridge protocol v1

- Adds versioned, ANSI-free JSON/NDJSON commands under `jujuleaf bridge`.
- Normalizes comments, messages, multi-range and detached anchors, UTF-16
  coordinates, document paths, snapshots, and stable errors.
- `bridge comments watch` starts with an authoritative snapshot, emits
  normalized wake-up events, reconciles periodically, and reconnects safely.

See the [Bridge protocol documentation](https://github.com/CsomePro/jujuleaf/blob/v0.1.2/docs/bridge-protocol.md).

### 🤖 Managed Agent Skill

JujuLeaf now embeds its own versioned `SKILL.md` and can install or update it
for Codex, Claude Code, Kimi Code CLI, Pi, Gemini CLI, GitHub Copilot, Cursor,
OpenCode, and portable Agent Skills clients:

```bash
jujuleaf skill install
jujuleaf skill status
jujuleaf skill update
jujuleaf skill uninstall
```

The installer detects local Agents, supports project/user/custom scopes, shows
the exact target paths, and provides full keyboard interaction. Managed updates
verify file digests and back up locally modified files before a forced update.

### 🩺 Better diagnostics

`jujuleaf doctor` checks authentication, endpoints, browser availability, and
local workspace health without exposing credentials.

## Compatibility notes

- The prebuilt release contains one Linux AMD64 executable.
- A separate `jj` executable is not required; JujuLeaf embeds `jj-lib`.
- Git network operations use the system Git credentials and SSH configuration.
- Codex user Skills follow the current `~/.agents/skills` convention.
- Bridge protocol v1 is read-only and currently focuses on comment integration.

## Documentation

- [README](https://github.com/CsomePro/jujuleaf/blob/v0.1.2/README.md)
- [中文 README](https://github.com/CsomePro/jujuleaf/blob/v0.1.2/README.zh.md)
- [Full changes since v0.1.1](https://github.com/CsomePro/jujuleaf/compare/v0.1.1...v0.1.2)

Thanks to [overleaf-cli](https://github.com/dylantmoore/overleaf-cli) for prior
work that helped inform JujuLeaf's Overleaf integration.
