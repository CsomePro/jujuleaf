<p align="center">
  <img src="assets/jujuleaf-logo.png" alt="JujuLeaf 标志" width="176">
</p>

<h1 align="center">JujuLeaf</h1>

<p align="center">
  在本地编辑 Overleaf 项目，并获得安全同步与原生 Jujutsu 历史。
</p>

<p align="center">
  <a href="README.md">English</a>
  · <a href="https://github.com/CsomePro/jujuleaf/releases">下载</a>
  · <a href="docs/architecture.zh-CN.md">架构</a>
  · <a href="skill/SKILL.md">Agent Skill</a>
</p>

<p align="center">
  <a href="https://github.com/CsomePro/jujuleaf/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/CsomePro/jujuleaf/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/CsomePro/jujuleaf/releases"><img alt="GitHub Release" src="https://img.shields.io/github/v/release/CsomePro/jujuleaf"></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/github/license/CsomePro/jujuleaf"></a>
</p>

JujuLeaf 把 Overleaf 项目带到真正的本地工作目录中。你可以使用任何编辑器或
Coding Agent，通过内嵌的 Jujutsu 引擎保留可恢复的本地历史，再有意识地同步
改动。同步时，JujuLeaf 会把文本差异转换为 Overleaf 协作事件，而不是粗暴地
替换整份文档。

> [!IMPORTANT]
> JujuLeaf 仍处于 1.0 之前的早期阶段，并依赖 Overleaf 私有 API。请先在非关键
> 项目中试用，或提前保留备份。v0.1.1 暂时只提供 Linux AMD64 预编译版本。

## 为什么使用 JujuLeaf？

- **继续使用熟悉的工具。** 在 Neovim、VS Code、Zed、IDE 或 Coding Agent
  中工作，其他协作者仍然可以留在 Overleaf。
- **同步更安全。** 本地和远端改动都会与已确认的基线比较；遇到冲突时停止，
  不会静默覆盖任何一方。
- **内置本地历史。** 通过内嵌的 Jujutsu 仓库创建 checkpoint、撤销和重做，
  无需额外安装 jj 命令。
- **保留协作能力。** 精确编辑、修订模式、评论、编译、PDF、项目历史和实时
  事件都通过 Overleaf 的协作能力完成。
- **人和 Agent 都友好。** 默认输出清晰且带颜色；脚本和 Agent 可选择紧凑或
  格式化 JSON。

## 安装

### Linux AMD64 预编译版本

从 [最新 Release](https://github.com/CsomePro/jujuleaf/releases/latest)
下载压缩包和校验文件，或者直接安装 v0.1.1：

~~~bash
curl -LO https://github.com/CsomePro/jujuleaf/releases/download/v0.1.1/jujuleaf-v0.1.1-x86_64-unknown-linux-gnu.tar.gz
curl -LO https://github.com/CsomePro/jujuleaf/releases/download/v0.1.1/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf jujuleaf-v0.1.1-x86_64-unknown-linux-gnu.tar.gz
install -Dm755 jujuleaf-v0.1.1-x86_64-unknown-linux-gnu/jujuleaf ~/.local/bin/jujuleaf
jujuleaf --version
~~~

请确认 ~/.local/bin 已加入 PATH。

### 从源码安装

JujuLeaf 需要 Rust 1.92 或更高版本：

~~~bash
cargo install --git https://github.com/CsomePro/jujuleaf --tag v0.1.1
~~~

## 快速开始

先登录、找到项目 ID，再克隆到一个新目录或空目录：

~~~bash
jujuleaf login --preset overleaf
jujuleaf projects
jujuleaf clone PROJECT_ID paper
cd paper
jujuleaf status
~~~

登录命令会打开 Chrome 或 Chromium，并在本机保存登录会话。克隆完成后，
JujuLeaf 会在工作区中记录项目 ID 和所用配置。从这个目录或任意子目录执行
命令时，会自动使用当前项目：

~~~bash
jujuleaf files
jujuleaf read main.tex --content-only
jujuleaf compile
~~~

使用熟悉的工具修改文件，创建一个本地 checkpoint，然后同步：

~~~bash
jujuleaf checkpoint -m "重写引言"
jujuleaf sync
~~~

## 选择工作流

### 直接同步

如果改动应当作为普通编辑直接出现在 Overleaf 中，使用这套流程：

~~~bash
jujuleaf sync
# 在本地编辑文件。
jujuleaf status
jujuleaf checkpoint -m "更新实验结果"
jujuleaf sync
~~~

单次 sync 会先拉取远端改动，再推送确认安全的本地改动。运行
jujuleaf sync --watch 可以在前台持续同步。

### 通过修订模式审阅

如果希望协作者在 Overleaf 中接受或拒绝你的改动：

~~~bash
jujuleaf sync
jujuleaf begin -m "重写引言"
# 在本地编辑文件。
jujuleaf review diff
jujuleaf review submit
jujuleaf review status
~~~

协作者在 Overleaf 中处理完修订后：

~~~bash
jujuleaf review finish
~~~

JujuLeaf 会让审阅始终绑定到同步时的基线，并在结束前报告外来改动或只处理了
一部分的修订。

## 常用命令

| 目的 | 命令 |
| --- | --- |
| 列出项目 | <code>jujuleaf projects</code> |
| 查看项目文件 | <code>jujuleaf files</code> |
| 读取文档 | <code>jujuleaf read main.tex --content-only</code> |
| 搜索项目快照 | <code>jujuleaf search "bibliography"</code> |
| 精确替换文本 | <code>jujuleaf replace main.tex --old "草稿" --new "终稿"</code> |
| 提交一处修订 | <code>jujuleaf suggest main.tex --old "草稿" --new "终稿"</code> |
| 查看评论线程 | <code>jujuleaf threads</code> |
| 编译并查看诊断 | <code>jujuleaf compile</code> |
| 下载编译后的 PDF | <code>jujuleaf pdf -o paper.pdf</code> |
| 比较本地与远端状态 | <code>jujuleaf status</code> |
| 拉取、推送或同步 | <code>jujuleaf pull</code>、<code>jujuleaf push</code>、<code>jujuleaf sync</code> |
| 恢复本地历史 | <code>jujuleaf undo</code>、<code>jujuleaf redo</code> |

运行 jujuleaf --help 或 jujuleaf COMMAND --help 可以查看完整命令和示例。

## 更安全的定点编辑

精确文本命令默认拒绝有歧义的匹配。可以先定位文本，预览将要提交的协作操作，
确认后再正式执行：

~~~bash
jujuleaf locate main.tex --text "相关工作"
jujuleaf replace main.tex \
  --old "相关工作" \
  --new "背景与相关工作" \
  --dry-run
jujuleaf replace main.tex \
  --old "相关工作" \
  --new "背景与相关工作"
~~~

如果文本重复出现，可以用 --occurrence 或 --position 指定位置，也可以明确使用
--all 修改全部匹配项。位置统一采用与 Overleaf 相同的 CodeMirror UTF-16 单位。

## 多账号与自托管实例

命名 profile 可以把不同账号和服务地址隔离开：

~~~bash
jujuleaf login --profile official --preset overleaf
jujuleaf login --profile lab --base-url https://overleaf.example.org
jujuleaf login --profile cstcloud --preset cstcloud
jujuleaf profile list
jujuleaf profile use official
jujuleaf --profile lab projects
~~~

克隆会记住所用 profile，所以日常命令不必反复传入 --profile。登录信息仅保存
在用户配置目录中，不会写入项目。

## 面向人类和 Agent 的输出

默认使用人类可读格式，并且只在交互式终端中启用颜色：

~~~bash
jujuleaf status
jujuleaf status --no-color
NO_COLOR=1 jujuleaf status
jujuleaf projects --raw
jujuleaf projects --pretty
~~~

--raw 和 --pretty 输出的 JSON 天然不包含 ANSI 颜色，因此不需要再传
--no-color。Agent 可以读取仓库内的 [SKILL.md](skill/SKILL.md) 作为精简命令指南。

## 同步如何保护数据

JujuLeaf 会针对每份文档或上传文件，把本地副本和 Overleaf 当前副本分别与上次
确认的 checkpoint 比较：

| 本地状态 | 远端状态 | 结果 |
| --- | --- | --- |
| 未修改 | 已修改 | 拉取远端版本 |
| 已修改 | 未修改 | 推送本地版本 |
| 已修改 | 已修改 | 停止并报告冲突 |
| 未修改 | 未修改 | 不执行操作 |

发送更新前和收到确认后，JujuLeaf 都会再次核对远端版本与内容哈希。新增文件和
破坏性操作会被显式处理；发生冲突时，远端内容会保留下来供手动恢复。

## 当前限制

- Release 暂时只提供 Linux AMD64 预编译版本，其他平台可以从源码构建。
- 浏览器登录需要 Chrome 或 Chromium；也可以直接提供已有的会话 Cookie。
- 同步只在前台运行，目前没有后台服务。
- Overleaf 私有 API 可能随时变化，自托管实例尤其如此。

## 开发与文档

~~~bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --locked
~~~

协议、存储、冲突和审阅流程的细节请参阅
[架构文档](docs/architecture.zh-CN.md)。

## 致谢

JujuLeaf 受到
[overleaf-cli](https://github.com/dylantmoore/overleaf-cli) 的启发，也受益于
Overleaf 命令行工具社区积累的思路。

JujuLeaf 是独立的非官方项目，与 Overleaf 或 Jujutsu 项目不存在隶属或背书关系。

## 许可证

[MIT](LICENSE)
