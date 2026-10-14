<p align="center">
  <img src="https://raw.githubusercontent.com/CsomePro/jujuleaf/v0.4.3-rc.1/assets/jujuleaf-logo.png" width="140" alt="JujuLeaf logo">
</p>

<h1 align="center">JujuLeaf v0.4.3-rc.1</h1>

<p align="center"><strong>Compile any project entry point without changing the saved Main document</strong></p>

> This is a cross-platform prerelease for validating one-off root document
> selection. Keep a backup when testing it with an important project.

## Choose the root document per command

~~~bash
# Use the Main document saved by Overleaf
jujuleaf compile
jujuleaf pdf -o paper.pdf

# Compile another project document for this invocation only
jujuleaf compile --root appendix.tex
jujuleaf pdf --root chapters/appendix.tex -o appendix.pdf
~~~

- `compile` and `pdf` now accept the concise `--root TEX_PATH` option.
- Paths are relative to the Overleaf project root and are validated against
  the remote project tree before compilation.
- JujuLeaf sends both the resolved document ID and resource path for official
  Overleaf and self-hosted compatibility.
- Omitting `--root` keeps the existing behavior and uses the project's saved
  Main document.
- A root override applies only to that command and never changes the saved
  Overleaf project setting.
- Inside a JujuLeaf clone, the project ID and profile continue to be inferred
  automatically.

The new flow was exercised against official Overleaf for both structured
compile diagnostics and PDF download.

## Install or upgrade

~~~bash
cargo binstall --strategies crate-meta-data --force jujuleaf@0.4.3-rc.1
jujuleaf --version
~~~

The reported version should be `jujuleaf 0.4.3-rc.1`.

## Release targets

- Linux AMD64 and ARM64: fully static musl binaries
- macOS Apple Silicon and Intel: single binaries using Apple system libraries
- Windows AMD64: single executable with the static MSVC runtime

- [README](https://github.com/CsomePro/jujuleaf/blob/v0.4.3-rc.1/README.md)
- [中文 README](https://github.com/CsomePro/jujuleaf/blob/v0.4.3-rc.1/README.zh.md)
- [Changes since v0.4.2](https://github.com/CsomePro/jujuleaf/compare/v0.4.2...v0.4.3-rc.1)
