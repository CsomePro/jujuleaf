<p align="center">
  <img src="https://raw.githubusercontent.com/CsomePro/jujuleaf/v0.4.1-rc.2/assets/jujuleaf-logo.png" width="140" alt="JujuLeaf logo">
</p>

<h1 align="center">JujuLeaf v0.4.1-rc.2</h1>

<p align="center"><strong>Clearer Overleaf compilation diagnostics</strong></p>

> This is a cross-platform prerelease. Keep a backup and start with a
> non-critical Overleaf project.

## Compile fixes

- Human-readable mode now explains that Overleaf compilation is a synchronous
  server request and that diagnostics become available only after it finishes.
- Compile reports distinguish a missing log from a clean log with
  `logAvailable` and `logBytes`, and report the saved `logOutput` path.
- `--show-log` no longer emits a misleading empty string when Overleaf did not
  return `output.log`.
- Multiline LaTeX class warnings, including `Class ... Warning:`, are now
  parsed into structured diagnostics.
- `--raw` remains free of human progress messages.

## Log output

`jujuleaf compile` does not create a local log by default. Save it explicitly:

~~~bash
jujuleaf compile --log-output output.log
~~~

If the Overleaf/CLSI request reaches the configured timeout, JujuLeaf stops the
compile and reports the timeout. A log cannot be downloaded before the server
finishes.

## Release targets

- Linux AMD64: fully static musl binary
- Linux ARM64: fully static musl binary
- macOS Apple Silicon and Intel: single binaries using Apple system libraries
- Windows AMD64: single executable with the static MSVC runtime

~~~bash
cargo binstall --strategies crate-meta-data --force jujuleaf@0.4.1-rc.2
jujuleaf --version
~~~

- [README](https://github.com/CsomePro/jujuleaf/blob/v0.4.1-rc.2/README.md)
- [中文 README](https://github.com/CsomePro/jujuleaf/blob/v0.4.1-rc.2/README.zh.md)
- [Changes since v0.1.4-rc.1](https://github.com/CsomePro/jujuleaf/compare/v0.1.4-rc.1...v0.4.1-rc.2)
