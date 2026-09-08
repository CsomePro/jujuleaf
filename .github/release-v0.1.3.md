<p align="center">
  <img src="https://raw.githubusercontent.com/CsomePro/jujuleaf/v0.1.3/assets/jujuleaf-logo.png" width="140" alt="JujuLeaf logo">
</p>

<h1 align="center">JujuLeaf v0.1.3</h1>

<p align="center"><strong>Linux compatibility hotfix</strong></p>

> [!IMPORTANT]
> This release replaces the Linux release build from v0.1.2. Users who saw
> `GLIBC_2.39 not found` should upgrade to v0.1.3.

## What changed

### Portable static Linux binary

- The Linux AMD64 release is now built for `x86_64-unknown-linux-musl`.
- The executable is a static PIE and no longer depends on the host glibc version.
- Release CI rejects a Linux artifact if it contains a dynamic interpreter.
- `cargo-binstall` transparently selects the static musl artifact on GNU/Linux hosts.

There are no application behavior or data-format changes in this hotfix.

## Install or upgrade

~~~bash
cargo binstall --strategies crate-meta-data --force jujuleaf
jujuleaf --version
~~~

The reported version should be `jujuleaf 0.1.3`.

### Manual Linux AMD64 install

~~~bash
curl -LO https://github.com/CsomePro/jujuleaf/releases/download/v0.1.3/jujuleaf-v0.1.3-x86_64-unknown-linux-musl.tar.gz
curl -LO https://github.com/CsomePro/jujuleaf/releases/download/v0.1.3/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf jujuleaf-v0.1.3-x86_64-unknown-linux-musl.tar.gz
install -Dm755 jujuleaf-v0.1.3-x86_64-unknown-linux-musl/jujuleaf ~/.local/bin/jujuleaf
~~~

## Links

- [README](https://github.com/CsomePro/jujuleaf/blob/v0.1.3/README.md)
- [中文 README](https://github.com/CsomePro/jujuleaf/blob/v0.1.3/README.zh.md)
- [Full changes since v0.1.2](https://github.com/CsomePro/jujuleaf/compare/v0.1.2...v0.1.3)
