<p align="center">
  <img src="https://raw.githubusercontent.com/CsomePro/jujuleaf/v0.4.1-rc.3/assets/jujuleaf-logo.png" width="140" alt="JujuLeaf logo">
</p>

<h1 align="center">JujuLeaf v0.4.1-rc.3</h1>

<p align="center"><strong>Reliable CSTCloud compilation and complete TeX box diagnostics</strong></p>

> This is a cross-platform prerelease. Keep a backup and start with a
> non-critical Overleaf project.

## CSTCloud compile compatibility

- Fixes `compile` and `pdf` hanging when `latex.cstcloud.cn` sends an interim
  HTTP `102 Processing` response while CLSI is compiling.
- Uses a scoped HTTP/1.0 compatibility transport only for CSTCloud compile
  requests. Official Overleaf and other self-hosted instances retain the
  default HTTP transport.
- Compile timeouts now return promptly on CSTCloud instead of leaving the CLI
  waiting indefinitely.

## Complete layout diagnostics

- Parses `Underfull` and `Overfull` horizontal and vertical box messages as
  structured warnings.
- Preserves repeated diagnostics from the log instead of silently collapsing
  distinct occurrences with identical text.
- Continues to download `output.log` and `output.pdf` from the URLs returned by
  the current compile response.

## Install or upgrade

~~~bash
cargo binstall --strategies crate-meta-data --force jujuleaf@0.4.1-rc.3
jujuleaf --version
~~~

The reported version should be `jujuleaf 0.4.1-rc.3`.

## Release targets

- Linux AMD64 and ARM64: fully static musl binaries
- macOS Apple Silicon and Intel: single binaries using Apple system libraries
- Windows AMD64: single executable with the static MSVC runtime

- [README](https://github.com/CsomePro/jujuleaf/blob/v0.4.1-rc.3/README.md)
- [中文 README](https://github.com/CsomePro/jujuleaf/blob/v0.4.1-rc.3/README.zh.md)
- [Changes since v0.4.1-rc.2](https://github.com/CsomePro/jujuleaf/compare/v0.4.1-rc.2...v0.4.1-rc.3)
