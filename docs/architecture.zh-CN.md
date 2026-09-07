# JujuLeaf 架构与同步语义

## 1. 目标

JujuLeaf 不是把浏览器 UI 原样搬到终端。它保留会改变文章语义或同步正确性的
CodeMirror/Overleaf 行为：

- UTF-16 文本坐标；
- 单次 transaction 中有序且不重叠的 changes；
- legacy OT 与 history-OT；
- tracked changes 与评论 range；
- pending/inflight、服务端版本、确认和不确定结果；
- 断线重连和 `joinDoc(fromVersion)` 增量追赶入口；
- 内容 hash 与 out-of-sync 检查；
- 可恢复的本地版本和 undo/redo。

selection、光标、focus、viewport、scroll、lint、autocomplete、协作者光标等 UI
状态不会进入 CLI 的同步模型。

## 2. 分层

```text
CLI / 人类可读输出 / JSON
    │
    ├── 精确文本定位、UTF-16 changes、dry-run
    │
    ▼
OT 适配层
    ├── sharejs-text-ot: { d|i|c, p }
    └── history-ot: { textOperation: [...] }
    │
    ▼
可靠投递层
    ├── Socket.IO 0.9 handshake / heartbeat / RPC
    ├── joinProject / joinDoc / joinDoc(fromVersion)
    ├── applyOtUpdate
    └── 等待 otUpdateApplied，超时使用 dupIfSource 重试
    │
    ├── SQLite：远端 checkpoint 与操作回执
    └── jj-lib：内容版本、操作历史、undo/redo
```

## 3. 为什么 `--old` 不会误改多个位置

`--old` 的默认语义不是“搜到就全改”，而是：

1. 没找到：失败；
2. 只找到一次：生成一个 change；
3. 找到多次：失败，并要求增加上下文或显式选择。

显式选择有三种，且互斥：

- `--position N`：必须在 N 处逐字匹配；
- `--occurrence N`：选择第 N 个匹配；
- `--all`：选择全部互不重叠匹配。

在构造 operation 之前，`expect` 还会再次与快照中的 range 比较。因此定位和
提交之间没有“模糊匹配”。

## 4. UTF-16 坐标

CodeMirror 和浏览器 JavaScript 的字符串位置以 UTF-16 code unit 计数，Rust
字符串则使用 UTF-8 字节。JujuLeaf 的所有外部位置都使用 UTF-16，并在切片时
转换为 UTF-8 byte index。

如果位置落在 emoji 等非 BMP 字符的一对 surrogate 中间，会直接拒绝；不会把
Rust 字符串切坏。例如：

```text
"😀\ncat" 中 cat 的位置是 3，而不是 Rust char 数 2 或 UTF-8 byte 数 5。
```

## 5. 两种 OT

### sharejs-text-ot

替换 `cat → kitten` 会生成：

```json
[
  {"d":"cat","p":4},
  {"i":"kitten","p":4}
]
```

批量 changes 的位置属于同一个 CodeMirror transaction。生成后续 operation
时会累计前面插入/删除造成的位移。

### history-ot

history-OT 使用扫描操作：正整数 retain、负整数 delete、字符串 insert。每个
textOperation 必须完整消费 source snapshot。tracked delete 在 source 中仍然
存在但在编辑器可见文本中隐藏，所以 JujuLeaf 保存 visible→source 边界映射。

建议模式将插入和删除编码为带 tracking 元数据的对象。与 Overleaf 当前
editor-core 一致，history-OT 插入非 BMP 字符会被拒绝。

## 6. 为什么 RPC ack 不等于成功

`applyOtUpdate` 的 RPC callback 只说明 real-time 服务接受了排队请求。真正的
编辑确认是发送方收到不含 `op` 的：

```json
{"doc":"DOC_ID","v":NEXT_VERSION}
```

也就是 `otUpdateApplied`。协作者广播包含 `op`，不能拿来确认自己的 inflight
operation。

超时时，JujuLeaf 在同一连接上带 `dupIfSource: [publicId]` 重发。服务端可以据此
丢弃重复提交。

## 7. 断线、增量追赶和竞争

官方首次加入参数是：

```text
joinDoc(docId, options)
```

已有快照重连则是：

```text
joinDoc(docId, fromVersion, options)
```

返回的 `ops` 是从 fromVersion 到当前 version 的缺失操作。如果 Redis 中旧操作
已经被清除，服务端会返回 missing-ops 错误，此时必须完整读取快照。

JujuLeaf 的本地同步采用更保守的恢复策略：

1. 发送前记录 base version、完整 wire op、期望内容 hash；
2. 标记 inflight；
3. 连接中断则标记 unknown，而不是假定失败；
4. 重新连接并完整读取远端；
5. 远端 hash 等于期望 hash，则补记 confirmed；
6. 否则保留 unknown，下一次 pull/push 再核对。

这避免了“服务器已经写成功，客户端因为没看见确认又写一遍”的经典问题。

## 8. Jujutsu 与 SQLite 的职责

Jujutsu 保存内容版本：

- clone 后的远端基线；
- pull 前的本地工作副本；
- pull 后的远端更新；
- push 前准备发布的内容；
- 手工 checkpoint；
- undo/redo 恢复产生的新 operation。

SQLite 不保存全部文章历史，只保存同步事实：

```text
documents:
  project_id, doc_id, path, remote_version, remote_hash, jj_operation_id

sync_operations:
  receipt_id, base_version, operation_json, expected_hash,
  source_ids, status, error, timestamps

document_metadata:
  doc_id, remote_version, ranges_json, snapshot_metadata_json, metadata_hash

assets:
  file_id, path, parent_folder_id, remote_hash, size, jj_operation_id
```

状态机：

```text
prepared → inflight → confirmed
               └────→ unknown → inflight
                         └────→ confirmed（hash 核对）
prepared/inflight/unknown → failed
```

SQLite 解决“我和哪个远端版本同步过、某次网络写入到底确认没有”；Jujutsu 解决
“内容是什么、如何回退、如何保留分支和演化历史”。两者互补。

此外，`.jujuleaf/remote-metadata.json` 保存可被 Jujutsu 版本化的远端元数据快照：
评论 threads、每个文档的 ranges，以及去掉正文后的 history-OT snapshot（其中包含
tracked changes）。SQLite 是同步判断用的权威 checkpoint；JSON 文件是可审计、可
回退的历史副本，不会作为普通文件上传回 Overleaf。

### Profile 与项目绑定

一个 Profile 绑定一个 Overleaf 端点和一份账号 Cookie：

```text
~/.config/jujuleaf/
├── active-profile
└── profiles/
    ├── default.json
    ├── official.json
    └── company.json
```

直接远程命令按“显式 `--profile` → 当前 active profile”选择身份。clone 时会把
Profile 名和 base URL 一起写入：

```text
<project>/.jj/jujuleaf/project.json
```

项目中的 pull/push/sync 按“显式 `--profile` → 项目绑定 Profile”选择身份，并且
同时核对 Profile 名和 base URL。因此即使两个账号使用同一个 Overleaf 域名，也
不会仅凭 URL 相同就把内容推给错误账号。

旧的单 Session 配置只迁移一次到 `default`；Profile 列表和详情输出不会包含
Cookie。

### 登录预制与 CSTCloud SSO

`login --preset` 提供三种策略：

- `auto`：识别 `overleaf_session2`、`overleaf.sid`、`sharelatex.sid`，并保留
  `GCLB`/`latex-session` 路由 Cookie；
- `overleaf`：预置 `https://www.overleaf.com/login`，要求
  `overleaf_session2`，保留 `GCLB`；
- `cstcloud`：预置 `https://latex.cstcloud.cn/oidc/login`，要求
  `overleaf.sid`，保留 `latex-session`。

CSTCloud 匿名登录页也会设置 `overleaf.sid`，因此 Cookie 出现不代表认证成功。
浏览器登录必须同时满足：

1. 当前 URL 已回到配置端点的同源 `/project`；
2. 页面存在非空的 `ol-user_id`；
3. 页面存在非空的 `ol-csrfToken`；
4. 目标域 Cookie 中存在预制要求的认证 Cookie。

CDP 返回的 Cookie 还会按 base URL 主机过滤；AAI 身份提供方域名的 Cookie 不会
进入 Profile。HTTP 响应和 Socket.IO 握手对上述认证/路由 Cookie 的轮换使用同一
合并规则，并立即更新当前 API 客户端和所选 Profile。这样同一命令里的后续请求、
`sync` 的 pull→push 阶段以及下次启动都会使用最新 Cookie。

## 9. pull/push 冲突规则

每个文档记录远端基线 hash。pull 和 push 都会读取实时快照：

- local == base、remote != base：快进本地；
- local != base、remote == base：保留本地，可安全 push；
- local == remote：已经收敛；
- local、remote 都偏离 base 且互不相同：冲突。

冲突时工作副本不被覆盖，远端副本保存在
`.jj/jujuleaf/incoming/<doc-id>`。用户完成合并并 checkpoint 后再同步。

本地全文与远端可见正文比较时会生成字符级 diff，再转换成多个有序、不重叠的
UTF-16 change。因此两个相距很远的小改动不会把中间正文编码成“删除后重插”，
评论和修订 range 由 Overleaf 的 OT 只围绕实际变更转换。若直接 push 时发现
ranges/tracked-change 元数据已经偏离本地 checkpoint，会先拒绝写入；pull 接受
最新元数据后才可继续。

二进制文件也采用 local/base/remote 三方 hash。远端覆盖前会再次直接下载目标
文件核对 hash。由于 Overleaf 的上传接口拒绝同名文件，安全覆盖流程为：

```text
旧文件改成临时备份名 → 上传原名新文件 → 删除旧文件
                    └─ 上传失败：把旧文件改回原名
```

pull 发现双方都改了二进制时，把远端版本放到
`.jj/jujuleaf/incoming-assets/<file-id>`，不覆盖工作副本。


## 10. Review 修改状态机

本地可审阅工作采用“同步父版本 → 描述后的子 change → Overleaf tracked
changes → 审阅后远端结果”的单向状态机：

```text
sync → begin → draft → review submit → submitting → submitted → finishing → done
                  └ review abort（无远端尝试）       │             │
                                      部分提交恢复 ──┘             │
                                      review finish 可重试 ────────┘
```

`begin` 先要求正文、二进制、操作回执和已知远端元数据均为干净状态，再创建
与同步父版本内容相同、但带 description 的 Jujutsu 子 change，并把基线写入
`.jj/jujuleaf/review.json`。该文件包含每个文档的远端 version、可见内容 hash
和 metadata hash；它属于私有运行状态，不进入 Overleaf 项目。

`review diff` 和首次 `review submit` 都重新读取实时文档，并严格核对 version、
正文 hash 和 comments/tracked-change metadata hash。任何并发变化都会在发送
OT 前停止。提交时，本地各文件的提案 hash 被冻结，文本差异使用 tracked OT
发送；显式 change ID 不存在时，以完整 tracked-change JSON 的稳定 hash 作为
本地身份。操作仍经过 SQLite receipt 的 prepared/inflight/unknown/confirmed
状态。

`review status` 检查项目中的全部同步文档，而不只检查本次改过的文件。只有本次
change 的修订和其他修订均已消失、提交后的本地文件未再修改、且没有不确定回执
时，`readyToFinish` 才为 true。状态结果同时列出未提交文件和不确定回执数量。
`review finish` 以审阅后的远端正文为最终结果：接受、拒绝及审阅者后续编辑都会
被拉回当前 change，然后保留原 description 并清除 review 状态。提交时的提案
仍可从 Jujutsu operation 历史恢复。

多文档提交若在至少一次远端尝试后中止，状态保持为 `submitting`。用户先在
Overleaf 处理已出现的全部 tracked changes；之后 `review finish` 会报告未提交
文件、放弃这些文件的本地提案，并以当前远端正文收敛。若尚无任何远端尝试，
`review abort` 仍可安全恢复同步父版本。进入本地写入前，`review finish` 先持久化
`finishing` 阶段和每个文档的最终 hash；文件写入、pull 或 description 更新失败
后可直接重试，不会把已拉回的审阅结果误判为新的本地修改。不确定回执在确认远端
已无 tracked changes 后也会被收敛为终态。

review 活动期间，普通 `pull/push/sync` 以及 `undo/redo` 被拒绝，避免同一
批次绕过 tracked changes 直接发布。所有同步、review 和 Jujutsu 写操作还共享
`.jj/jujuleaf/workspace.lock` 的非阻塞进程锁；`sync --watch` 在整个监控周期持锁，
所以另一个进程不能在两个周期之间插入 `begin`。新增文件和二进制变更不进入此
状态机，必须在 `begin` 前通过显式结构操作或普通同步处理。


## 11. undo/redo 与分组

CLI 不按每个键盘事件建历史。一个显式 checkpoint、pull 或 push 边界构成一个
有意义的版本组。`undo` 会先捕获尚未 checkpoint 的工作副本，再通过 jj-lib
恢复前一 operation 的 tree；`redo` 恢复 undo 前保存的 operation。整个过程不
调用外部 `jj` 命令。

## 12. 编译与前台监控

`compile` 调用 Overleaf 的同步编译 HTTP 接口；这个请求在服务端等待 CLSI 返回，
JujuLeaf 额外设置客户端超时，超时后调用 `/compile/stop`。完成后下载
`output.log`，识别 `file:line:`、TeX `!` 错误和常见 warning，输出结构化
diagnostics；完整日志可显示或保存到文件。

`sync --watch` 是前台轮询器：每个周期严格串行执行 pull→push，周期之间等待，
Ctrl+C 正常退出，遇到任何正文、元数据或二进制冲突立即停止。当前不派生后台
进程、不安装 systemd 服务；后台 daemon 属于后续阶段。
