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
  <a href="https://crates.io/crates/jujuleaf"><img alt="crates.io" src="https://img.shields.io/crates/v/jujuleaf.svg"></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/github/license/CsomePro/jujuleaf"></a>
</p>

JujuLeaf 把 Overleaf 项目带到真正的本地工作目录中。你可以使用任何编辑器或
Coding Agent，通过内嵌的 Jujutsu 引擎保留可恢复的本地历史，再有意识地同步
改动。同步时，JujuLeaf 会把文本差异转换为 Overleaf 协作事件，而不是粗暴地
替换整份文档。

> [!IMPORTANT]
> JujuLeaf 仍处于 1.0 之前的早期阶段，并依赖 Overleaf 私有 API。请先在非关键
> 项目中试用，或提前保留备份。v0.1.2 暂时只提供 Linux AMD64 预编译版本。

## 为什么使用 JujuLeaf？

- **继续使用熟悉的工具。** 在 Neovim、VS Code、Zed、IDE 或 Coding Agent
  中工作，其他协作者仍然可以留在 Overleaf。
- **同步更安全。** 本地和远端改动都会与已确认的基线比较；遇到冲突时停止，
  不会静默覆盖任何一方。
- **内置本地历史。** 通过内嵌的 Jujutsu 仓库创建 checkpoint、撤销和重做，
  无需额外安装 jj 命令。
- **保留协作能力。** 精确编辑、修订模式、评论、编译、PDF、项目历史和实时
  事件都通过 Overleaf 的协作能力完成。
- **人和 Agent 都友好。** 默认输出清晰且带颜色；普通脚本可以使用紧凑 JSON，
  长期运行的集成则可以通过 `jujuleaf bridge` 使用稳定、带版本的进程协议。

## 安装

### cargo-binstall（推荐）

安装 [cargo-binstall](https://github.com/cargo-bins/cargo-binstall) 后，可以
直接从 GitHub Releases 下载官方预编译版本：

~~~bash
cargo binstall --strategies crate-meta-data jujuleaf
~~~

在无人值守环境中可增加 --no-confirm。预编译版本目前支持 Linux AMD64。

### 手动下载 Linux AMD64 版本

从 [最新 Release](https://github.com/CsomePro/jujuleaf/releases/latest)
下载压缩包和校验文件，或者直接安装 v0.1.2：

~~~bash
curl -LO https://github.com/CsomePro/jujuleaf/releases/download/v0.1.2/jujuleaf-v0.1.2-x86_64-unknown-linux-gnu.tar.gz
curl -LO https://github.com/CsomePro/jujuleaf/releases/download/v0.1.2/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf jujuleaf-v0.1.2-x86_64-unknown-linux-gnu.tar.gz
install -Dm755 jujuleaf-v0.1.2-x86_64-unknown-linux-gnu/jujuleaf ~/.local/bin/jujuleaf
jujuleaf --version
~~~

请确认 ~/.local/bin 已加入 PATH。

### 从源码安装

JujuLeaf 需要 Rust 1.92 或更高版本：

~~~bash
cargo install --locked jujuleaf
~~~

## 安装 Agent Skill

JujuLeaf 可执行文件内嵌了一份带版本、自描述的 Agent Skill。运行下面的命令会
进入终端引导界面：它会检测本机已有的 Agent 命令与配置目录，先选择安装到
项目根目录、当前目录、用户全局目录或自定义目录，再多选 Agent 目标：

~~~bash
jujuleaf skill install
~~~

使用 ↑/↓ 移动、Space 切换选择、Enter 确认、Esc 取消。最终确认时使用
←/→ 选择、Enter 确认，也可以直接按 `y` 或 `n`。

目前支持 Codex、Claude Code、Kimi Code CLI、Pi、Gemini CLI、GitHub
Copilot、Cursor、OpenCode，以及通用的 `.agents/skills` 约定。安装器会写入
各 Agent 的原生目录，但所有目标共用一份标准 `SKILL.md`；Codex 会额外得到
可选的 `agents/openai.yaml` 界面配置。

交互界面的所有选择都有对应参数，方便脚本或无人值守环境使用：

~~~bash
# 在最近的项目根目录中为两个 Agent 安装。
jujuleaf skill install --agent codex,claude-code --scope project --yes

# 写入当前目录下各 Agent 的原生配置目录。
jujuleaf skill install --agent kimi-code --scope current --yes

# 把另一个目录作为共享的 skills 根目录。
jujuleaf skill install --agent portable --to ./agent-skills --yes
~~~

可以查看检测结果，并管理 JujuLeaf 记录的全部安装位置：

~~~bash
jujuleaf skill detect
jujuleaf skill status
jujuleaf skill update
jujuleaf skill uninstall
~~~

更新前会比较文件摘要；如果用户改过已安装的 Skill，JujuLeaf 默认拒绝覆盖。
显式增加 `--force` 时会先创建带时间戳的备份。自动化调用可结合 `--raw`、
完整目标参数、`--yes`，以及可选的 `--dry-run`。

## 快速开始

先登录、找到项目 ID，再克隆到一个新目录或空目录：

~~~bash
jujuleaf login --preset overleaf
jujuleaf projects
jujuleaf clone PROJECT_ID paper
cd paper
jujuleaf status
jujuleaf
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
| 接入外部 worker | <code>jujuleaf bridge describe</code>、<code>jujuleaf bridge comments watch</code> |
| 安装或更新 Agent Skill | <code>jujuleaf skill install</code>、<code>jujuleaf skill update</code> |
| 编译并查看诊断 | <code>jujuleaf compile</code> |
| 下载编译后的 PDF | <code>jujuleaf pdf -o paper.pdf</code> |
| 比较本地与远端状态 | <code>jujuleaf status</code> |
| 拉取、推送或同步 | <code>jujuleaf pull</code>、<code>jujuleaf push</code>、<code>jujuleaf sync</code> |
| 查看并解决冲突 | <code>jujuleaf conflict list</code>、<code>jujuleaf conflict show PATH</code>、<code>jujuleaf conflict resolve PATH</code> |
| 查看本地历史 | <code>jujuleaf local log</code>、<code>jujuleaf local show REVISION</code>、<code>jujuleaf local diff</code> |
| 显示当前提交图 | <code>jujuleaf</code> |
| 通过 Git 分享历史 | <code>jujuleaf git remote add origin URL</code>、<code>jujuleaf git fetch</code>、<code>jujuleaf git push</code> |
| 检查环境与登录状态 | <code>jujuleaf doctor</code>、<code>jujuleaf auth status</code> |
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

### 稳定的进程级集成

`--raw` 跟随 JujuLeaf 各命令的内部输出演进；需要长期兼容的外部 worker 应使用
专门的 bridge：

~~~bash
jujuleaf bridge describe
jujuleaf bridge comments list --protocol 1
jujuleaf bridge comments get THREAD_ID --protocol 1
jujuleaf bridge comments watch --protocol 1
~~~

Bridge 命令固定输出紧凑、无 ANSI 的 JSON。`comments.watch` 使用 NDJSON，先输出
一份权威评论快照，再进入实时事件流；它还会定期重新对账并在断线后自动重连。
事件只用于快速唤醒：worker 收到 `comments.snapshot` 时替换已知状态，收到
`comment.event` 后再读取 `comments.get`。

Bridge 会在 Rust 端统一关联线程消息、多范围或 detached 锚点、文档路径、UTF-16
原始/可见坐标和内容哈希，不会泄漏 Overleaf 私有事件名或 history-OT payload。
每条记录都包含协议名称和版本；失败时输出稳定错误码并以非零状态退出。完整字段
及事件流生命周期见 [Bridge 协议说明](docs/bridge-protocol.md)。

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

### 解决同步冲突

pull 和 push 会记录未解决的冲突，但不会覆盖工作副本。先查看两侧内容，必要时
合并，再明确选择解决方案：

~~~bash
jujuleaf conflict list
jujuleaf conflict show main.tex
jujuleaf conflict resolve main.tex --ours
jujuleaf conflict resolve main.tex --theirs
jujuleaf conflict resolve main.tex --merged /tmp/main.tex
~~~

`--ours` 保留本地文件，`--theirs` 接受已保存的远端副本，`--merged` 使用你准备
好的合并文件。解决冲突时会创建可恢复的 Jujutsu checkpoint，并推进已观察到的
远端基线；下一次 push 仍会在写入前重新核对实时远端状态。

### 查看与恢复本地历史

JujuLeaf 可以直接查看内嵌的 Jujutsu operation 历史：

~~~bash
jujuleaf local log
jujuleaf local show OPERATION_ID
jujuleaf local diff
jujuleaf local restore OPERATION_ID
~~~

`local show` 接受 operation 或 commit ID 前缀，也可以用 `@` 表示当前 operation。
`local restore` 会把目标 tree 恢复成一个新的 operation，因此恢复本身仍可撤销，
后续历史也不会被抹掉。JujuLeaf 内嵌 jj-lib，无需额外安装或调用 `jj` 可执行文件。

在 clone 内不带子命令运行 `jujuleaf`，会先 snapshot 尚未记录的工作区改动，然后
显示类似 `jj` 默认视图的紧凑彩色 first-parent 提交图。默认最多显示 10 条提交；
operation 历史请用 `local log`，完整结构化提交数据请用 `jujuleaf --raw`。

紧凑提交图使用与 jj 相似的 8 位显示 ID；其他人类可读输出会把 Jujutsu
operation、commit 和 change ID 缩写为 12 位前缀。这个前缀可以直接传给
`local show` 和 `local restore`；如果极少数情况下发生歧义，JujuLeaf 会要求
提供更长的前缀。`--raw` 和 `--pretty` JSON 始终保留完整 ID。

### 通过 Git 分享同一份历史

每个 JujuLeaf clone 都已经由 `.jj` 内部的 bare Git 仓库提供存储。请通过
JujuLeaf 配置和使用它，不要另建一套 `.git` working tree：

~~~bash
jujuleaf git root
jujuleaf git remote add origin git@github.com:OWNER/REPOSITORY.git
jujuleaf git push --branch main
jujuleaf git fetch --remote origin
jujuleaf git remote list
~~~

`git push` 会先记录尚未 checkpoint 的文件，再把当前 Jujutsu working-copy commit
发布到指定分支，默认分支为 `main`。它把最后一次 fetch 到的远端位置作为 lease；
如果远端出现预期外的新提交，push 会拒绝覆盖。更新已有分支前应先运行
`git fetch`。Fetch 会把远端分支和 tag 导入 Jujutsu 历史，但不会替换当前工作副本。

Remote 配置还支持 `remote remove`、`remote rename` 和 `remote set-url`。在 clone
外运行时用 `-R PATH` 指定工作区。工作区仍由 `jujuleaf clone` 创建，不使用
`jujuleaf git clone` 或 `git init`。不需要单独安装 `jj`；网络 fetch 和 push 会使用
系统 Git 以及它已有的 SSH 或 credential helper 配置。

### 忽略仅供本地使用的文件

在 clone 根目录创建 `.jujuleafignore`，可以让匹配的生成文件或私密文件不被发现
为上传候选，也不被 Jujutsu 首次跟踪。语法与 gitignore 一致：

~~~gitignore
/build/
*.aux
*.log
.env
~~~

规则不会取消跟踪已经进入 checkpoint 的文件或已经同步的 Overleaf 实体。
`.jj`、`.git`、`.jujuleaf` 和
`.jujuleafignore` 始终视为私有内容，永远不会作为待上传文件。

## 诊断与退出登录

~~~bash
jujuleaf auth status
jujuleaf doctor
jujuleaf doctor --offline
jujuleaf auth logout
~~~

`doctor` 会检查所选 profile、凭据权限、浏览器、服务端连接、项目绑定、Jujutsu
工作区和待处理同步状态；离线模式会跳过网络检查。`auth status` 永远不会打印
Cookie；`auth logout` 会删除所选本地 profile 及其凭据。

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
