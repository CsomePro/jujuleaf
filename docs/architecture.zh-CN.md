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
CLI / JSON
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

## 9. pull/push 冲突规则

每个文档记录远端基线 hash。pull 和 push 都会读取实时快照：

- local == base、remote != base：快进本地；
- local != base、remote == base：保留本地，可安全 push；
- local == remote：已经收敛；
- local、remote 都偏离 base 且互不相同：冲突。

冲突时工作副本不被覆盖，远端副本保存在
`.jj/jujuleaf/incoming/<doc-id>`。用户完成合并并 checkpoint 后再同步。

## 10. undo/redo 与分组

CLI 不按每个键盘事件建历史。一个显式 checkpoint、pull 或 push 边界构成一个
有意义的版本组。`undo` 会先捕获尚未 checkpoint 的工作副本，再通过 jj-lib
恢复前一 operation 的 tree；`redo` 恢复 undo 前保存的 operation。整个过程不
调用外部 `jj` 命令。
