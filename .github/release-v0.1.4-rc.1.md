<p align="center">
  <img src="https://raw.githubusercontent.com/CsomePro/jujuleaf/v0.1.4-rc.1/assets/jujuleaf-logo.png" width="140" alt="JujuLeaf logo">
</p>

<h1 align="center">JujuLeaf v0.1.4-rc.1</h1>

<p align="center"><strong>Cross-platform release candidate</strong></p>

> This is a prerelease for validating JujuLeaf on mainstream Linux, macOS, and
> Windows systems. Keep a backup and start with a non-critical Overleaf project.

## Five native release targets

| Platform | Architecture | Rust target | Portability |
| --- | --- | --- | --- |
| Linux | AMD64 | `x86_64-unknown-linux-musl` | Fully static |
| Linux | ARM64 | `aarch64-unknown-linux-musl` | Fully static |
| macOS | Apple Silicon | `aarch64-apple-darwin` | Single binary; Apple system libraries |
| macOS | Intel | `x86_64-apple-darwin` | Single binary; Apple system libraries |
| Windows | AMD64 | `x86_64-pc-windows-msvc` | Single `.exe`; static MSVC CRT |

Every target is compiled and tested on its native GitHub Actions runner. Release
archives are assembled into one GitHub prerelease with a shared
`SHA256SUMS` file.

## Portability improvements

- State and review files now use atomic replacement that also works when the
  destination already exists on Windows.
- Project paths are validated before materialization. Windows-reserved names,
  unsupported characters, backslashes, and case-insensitive collisions now fail
  clearly instead of producing a partial or ambiguous checkout.
- Browser login can discover Chrome and Chromium from `PATH`, user-local macOS
  Applications, and Chrome, Chromium, or Edge installations on Windows.
- Embedded Agent Skill frontmatter accepts both LF and CRLF line endings.
- `cargo-binstall` maps GNU/Linux hosts to the matching static musl archive and
  selects the native Windows ZIP automatically.

## Install or upgrade

~~~bash
cargo binstall --strategies crate-meta-data --force jujuleaf@0.1.4-rc.1
jujuleaf --version
~~~

The reported version should be `jujuleaf 0.1.4-rc.1`.

JujuLeaf remains a single application binary: its Jujutsu engine and Agent Skill
bundle are embedded. Linux does not require a compatible host glibc; macOS and
Windows builds use only their operating system's standard libraries.

## Feedback

When reporting a platform issue, include the target name from the table,
`jujuleaf --version`, and the failing command. Do not include session cookies
or other credentials.

- [README](https://github.com/CsomePro/jujuleaf/blob/v0.1.4-rc.1/README.md)
- [中文 README](https://github.com/CsomePro/jujuleaf/blob/v0.1.4-rc.1/README.zh.md)
- [Full changes since v0.1.3](https://github.com/CsomePro/jujuleaf/compare/v0.1.3...v0.1.4-rc.1)
