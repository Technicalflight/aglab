# 设计蓝图：Agent 独立子进程架构 与 会话/配置同步

> 状态：设计稿（M0）。本文回答"要变成什么形状、分几步走、每一步怎么验收"，
> 不含实现。两件大事拆开看：**A. 进程架构**（Agent 独立子进程 + Main 无业务状态
> + 每窗口 Local Host + owner/lease），**B. 数据分层与同步**（storage 分层 +
> 远程同步 + 会话分享 + settings-sync）。它们共享同一套身份与协议地基。

## 0. 现状与动机

现状：单 Rust 进程内联全部业务（chat.rs 1.2 万行），前端直连后端命令；
会话数据落本地（json/sqlite 双后端），无远端。

三个硬伤驱动重构：
1. **崩溃域**：一次工具 panic/内存失控拖死整个 UI 进程；
2. **多窗口**：进程级单例状态（runs、goal 面板、MCP hub）在第二扇窗前全是错的；
3. **同步**：数据格式与进程生命周期耦合，没有可复制的权威日志。

## A. Agent 独立子进程

### A1. 进程模型

```
┌─ aglab.exe (Main / UI 宿主) ─────────────┐
│  窗口×N ─ 每窗口一个 LocalHost           │
│  Main 只做：窗口管理、路由表、租约登记、  │
│  设置读写、进程监督（不碰任何业务状态）   │
└──────┬───────────────────┬───────────────┘
   stdio│ (JSON Lines)      │ stdio
┌─ agent-host A (话题 1..n) ─┐ ┌─ agent-host B ─┐
│ 会话日志、回合循环、工具执行│ │ …             │
│ MCP hub、钩子、审批闸      │ │               │
└────────────────────────────┘ └───────────────┘
```

- **agent-host**：一个可独立启动的子进程（`aglab-agent.exe`，同一二进制
  `--agent` 子命令），拥有一个或多个话题的**全部业务状态**。
- **LocalHost**：每窗口一个，是窗口与 agent-host 之间的路由与代理层；
  持有它名下话题的连接句柄，把 UI 事件流转发给窗口。
- **Main**：无业务状态。它只认三张表：窗口↔LocalHost、话题↔agent-host、
  租约登记（谁以什么 fencing token 持有哪个话题）。

### A2. stdio JSON 协议（严格类型 + 运行时校验）

传输：JSON Lines（每行一个信封，UTF-8，无跨行帧——天然增量，无整体 split）。
信封是唯一通道形状，双方各持一份 schema：Rust serde（编译期）+ 前端 zod（运行时）。
所有信封带 `v: 1`（协议版本）与 `id: u64`（请求关联）。

```
// 请求（Main/Host → Agent）
{ "v":1, "id":7, "kind":"req", "method":"turn.start",
  "params":{ "conversationId":"…", "input":"…", "policy":{…}, "model":{…} } }

// 响应与事件的共用回程（Agent → Main/Host）
{ "v":1, "id":7,  "kind":"resp", "result":{…} }                  // 对 req 的终答
{ "v":1, "id":7,  "kind":"ev",   "event":"delta", "data":{…} }   // 流内事件
{ "v":1, "id":7,  "kind":"err",  "error":{ "code":"denied", "message":"…" } }
```

方法面（M1 只做六种）：`turn.start / turn.stop / tool.decide / steer /
conversation.read / ping`。事件面直接复用现有 `ChatEvent`（serde 已是
tag+camelCase，透传即可）。

**运行时校验**：agent 侧入口对每个信封过 serde；未知字段保留（向前兼容），
未知 kind/method 回 `err: unknown_method`。前端 LocalHost 对每个事件过 zod，
不合法的信封丢弃并计数（坏帧率进诊断）。**校验失败永不 panic**——回错误信封。

### A3. owner / lease 路由 + stale run 防护

- **owner**：话题的当前持有者 = agent-host 进程。路由表（Main 持有）：
  `conversationId → {hostId, fence}`。
- **lease**：agent-host 每 5s 向 Main 续租（`lease.renew{conversationIds, fence}`）。
  TTL = 20s（4 次错过即失租）。失租 → Main 标记话题 orphan，窗口按 UI 决定
  接管（重启 host 重放日志）或只读。
- **fencing token**：Main 单调发号。每次接管 fence+1；旧 host 带
  `fence < current` 的任何写回（`turn.result` / 日志追加请求）一律拒收——
  这就是 stale run 防护：被顶掉的旧进程即便活着、即便跑完了，
  它的字节也进不了台账。
- **stale 检测的另一半**：agent-host 侧把"这一轮"的每条日志追加都带 fence
  发回 Main 落账；本地只留热缓冲。进程死了最多丢热缓冲，
  台账永远等于"被 fencing 承认的那些追加"。

### A4. 迁移里程碑

| 里程碑 | 内容 | 验收 |
|---|---|---|
| M1 ✅ | agent 子命令 + 信封 + ping/echo；Main 能拉起并监督一个 host | 杀 host 进程，Main 5s 内置 orphan 并可重启；信封往返 1000 帧 0 错 |
| M2 ✅（定位链） | WorkerContext（fence + config_dir + data_dir）贯通；config/sessions/usage/audit/plugins/hooks 六模块 `_in` 变体全备 | 各模块单测 + 真子进程集成测试（config.read / session.peek / storage.probe / plugins.count） |
| M2.5 ✅（事件面） | `stream.demo` + ev 通道 + 监督者 EventSink 路由（ev 不终结请求） | 集成测试钉顺序；agent_stream_check 诊断命令 |
| M3 ✅（hub 地基） | WorkerRuntime 四件套（approval/steering/warm/mcp）在 worker 自建 | hubs.check 验收 |

### A5. run_turn 的 app 依赖清单（M3 主体施工图，2026-10-08 实测）

run_turn + turn_body（chat.rs 4712–6900）共 **72 处** `app` 引用，按搬迁档位：

**第 1 档：`_in` 变体已备，改调用即成（~20 处）**
- `hooks::runnable(app) ×8` → `runnable_in(config, data_dir)` ✅
- `audit_tool(app) ×4` → 内部 `audit::record(root)` 原生路径制，包装层加 `_in`
- `usage::calibration_for / last_prompt_tokens_for / record_turn（×7）` → 包 `open_in(config_dir)` 一层
- `open_session(app) / config::load(app) / mcp::all_servers(app)` → `_in` 变体 ✅

**第 2 档：机械参数化（~15 处，各 10-30 分钟）**
- `conversation_project ×2`、`worktree::root_for / view_for ×3`、`memory::inject_for_turn / reinforce_injection ×2`、`skills::prompt / declared_tools ×2`、`summarize_history`、`edits::commit_deleted / committed_snapshots ×2`、`ink::new`——大多只读 config + data dir，照切片 1-4 同模式

**第 3 档：交互/重活类（M3 主体核心）**
- `state::<ApprovalHub/McpHub/Warm/SteeringHub> ×4` → **WorkerRuntime 已备** ✅
- `close_turn(app) ×2`、`park_unattended`、`auto_review_verdict`——审批交互：worker 只发 `ev`（approval_request），Main 收到后弹 UI，决定经 `tool.decide` 请求送回（协议方法面已预留）
- `toast::approval_needed` / 系统通知 → ev → Main 代发
- `spawn::run_from_chat`、`agent_control_exec`、`browser::handle_tool`——子进程/浏览器操控的 AppHandle 依赖需逐个评估，可后置到 M3.5

**第 4 档：Main 专属（worker 不实现）**
- `app.opener`（打开目录/日志）、窗口操作——worker 发 ev，Main 消费

**M3 主体施工顺序建议**：第 1 档改调用（1 次提交）→ 第 2 档逐模块（2-3 次提交）→ 第 3 档审批闭环（`turn.start` + `tool.decide` + ev 审批事件，1-2 次提交）→ 真机一问一答验收（vitest 全绿不改一行）。

### A4. 迁移里程碑（原表）

| 里程碑 | 内容 | 验收 |
|---|---|---|
| M1 骨架 | agent 子命令 + 信封 + ping/echo；Main 能拉起并监督一个 host | 杀 host 进程，Main 5s 内置 orphan 并可重启；信封往返 1000 帧 0 错 |
| M2 单话题搬迁 | `turn.start/stop` 搬进 host（复用 run_turn），事件流经协议透传到 UI；UI 零改动（LocalHost 伪装成现有 invoke/Channel） | 现有 vitest 全绿不改一行；真机一问一答 |
| M3 审批/钩子/MCP 进 host | ApprovalHub、hooks、MCP hub 随迁；`tool.decide` 跨进程 | 审批弹窗跨进程可用；钩子卡片照常 |
| M4 多窗口 | 第二窗口开自己的 LocalHost；同一话题两窗并发 → 后者接管（fence+1），前者进入只读 | 两窗并发写同一话题，台账无重复无丢失 |
| M5 Main 瘦身 | chat.rs 拆空：业务只住 agent crate；Main 只剩窗口/路由/监督 | chat.rs 不再包含回合循环 |

## B. 存储分层与同步

### B1. 四层

```
L0 热：agent-host 内存（当前回合的字节）
L1 日志：sessions/*.jsonl（追加式，权威）——已存在
L2 台账：usage.db / conversations（索引与派生读数）——已存在
L3 远端：同步目标（可选，见 B2）
```

原则：**L1 是唯一真相**。L2、L3 都是 L1 的投影，任何一层坏了都可由 L1 重建。
现状已满足分层（session/ 六件套），M-B 的全部工作在 L3 与投影规则。

### B2. 远程同步（append-only 复制）

- 单位：**条目**（entry），不是整份话题。话题 id + entry.seq 是全局序键。
- 方向：单向推（本地 → 远端）+ 拉齐游标。本地记 `synced_seq`；
  远端只接受 `seq = 已有最大+1` 的追加（乱序/回退一律 409）。
- 冲突：**没有冲突**——fencing 已保证同一话题单写者；两台设备各自产生分叉时
  按 parent_id 链各自成支，UI 里本来就是分支树。
- 传输：HTTPS 批量推（每 30s 或每 20 条）；断点续传靠 synced_seq。
- 服务端选型（按落地成本升序）：
  1. **自建端点**（官网后端加两张表：devices、entries）——首选，与分享共用；
  2. 对象存储（S3/R2：`entries/<conversation>/<seq>.json`）——零服务端逻辑，
     但拉齐要列目录，慢；
  3. Supabase——省运维，引入账号体系依赖。

### B3. 会话分享（conversation-share）

- 形状：**只读快照**。分享 = 把某话题当前分支投影成一份自包含 HTML/JSON
  （标题、消息、模型读数；**不含**系统提示、凭据、userConfig、路径），上传得到
  `share/<token>` 短链，过期时间默认 7 天可调。
- 隐私闸：导出前显式列出将包含的内容清单，用户勾选确认；敏感段（打码过的
  输入保持打码）。
- 复用：`export.rs` 已有 Markdown/JSON 投影，快照 = 同源投影 + 上传器。

### B4. settings-sync

- 范围：profiles、模型池、路由表、工具契约、**不含**本机路径类字段
  （base_url 可同步，workspace 不行）与凭据（keyring 永不出本机）。
- 机制：设置版本化（`settings.rev` 单调递增），推送整份带 rev，
  远端 LWW（整份覆盖，字段级合并的复杂度不值得）；
  本地检测到远端 rev 更高 → 提示"拉取远端设置"，不静默覆盖。

### B5. 里程碑

| 里程碑 | 内容 | 验收 |
|---|---|---|
| M-B1 投影收口 | L1→L2 的投影规则全部收进 session/（消灭散落的直读） | 删掉 L2，应用可从 L1 全量重建 |
| M-B2 分享 | 只读快照上传 + 短链 + 过期 | 手机打开分享链可读整段对话 |
| M-B3 单向同步 | 推/拉齐游标 + 409 防御 | 两设备轮流写，各自补齐对方条目 |
| M-B4 settings-sync | 版本化推送 + 拉取提示 | 两设备设置一致；凭据永不出本机 |

## 风险

| 风险 | 缓解 |
|---|---|
| M2"UI 零改动"过于乐观：现有 invoke 直连业务命令 | LocalHost 先做**命令透传层**（同名命令转发给 host），迁移一批收窄一批 |
| 子进程 stdout 巨帧（快照事件几百 KB） | 信封层不做大小假设；>1MB 的载荷改走共享内存文件 + 信封带引用 |
| 同步把隐私数据推出本机 | L3 默认**关**；开它要显式选服务端 + 登录；分享永远走显式快照 |
| 双进程调试复杂度 | 协议信封全量进诊断日志（现有 audit 模式复制）；`--agent --stdio` 可独立手跑 |

### A6. M3 主体施工细节（2026-10-08 深夜定稿——下个会话的开工单）

**已就位**：WorkerContext（fence + config_dir + data_dir）、WorkerRuntime 四件套、定位链六模块 `_in`、ev 通道、`turn.once` 最小真回合、`tool.decide` / `steer.push` 协议面、监督者孤儿重启 + fencing。

**核心机制：worker 的并发分发**。当前 run_stdio_loop 是"读一行分发一行"的同步循环——turn.start 执行期间主循环被占住，Main 发来的 tool.decide 只能躺在管道里，审批会死锁。改法（与监督者读线程同款模式）：

1. **读线程 + 命令队列**：stdin 逐行读 → `Envelope` 入 `mpsc::channel`；EOF 发 Eof。
2. **主循环**收信封分发。turn.start 执行中，审批闸（ApprovalHub.wait 已是 100ms 轮询）每轮顺带 `rx.try_recv()`：
   - 收到 `tool.decide` 信封 → `runtime.approvals.resolve(requestId, approved)`，等待者醒来；
   - 收到 `steer.push` → `runtime.steering.push(...)`；
   - 其余入队等 turn 收尾后处理。
3. **审批事件**：审批闸弹框改为发 `ev("approval_request", {requestId, tool, risk, input})`，Main 收到后弹真审批 UI，用户决定经 `tool.decide` 送回。`park_unattended` / `auto_review_verdict` / `toast` 全部走这条 ev 通道由 Main 代发。
4. **close_turn / summarize_history / ink::new**：open_session_in / load_from_dir 已备，把 app 参数换成 WorkerContext 传下来的目录即可（各 10 分钟）。
5. **spawn::run_from_chat / browser::handle_tool / agent_control_exec**：子助理递归与浏览器操控留 M3.5——先让主对话轮在 worker 里跑通。

**施工顺序**：① 读线程化 run_stdio_loop（保持现有方法全绿）→ ② approval_request ev + 闸轮询 tool.decide → ③ LocalHost 伪装 invoke（Main 侧 facade：takeover + 事件转发）→ ④ 真机一问一答验收（vitest 全绿不改一行）→ ⑤ 重活类 M3.5。

### A7. 下个会话开工单（2026-10-08 晚定稿——剩余三处：④ 接 UI → ⑤ 真机验收 → ⑥ M3.5 留尾）

> **✅ 已完成（2026-10-08 深夜，同会话执行完毕）**：④ 五刀全落（参数腿/事件腿/审批 ev 闭环/
> abort 腿/边界）、TurnHost 抽象落地、Supervisor pending 表并发化、config 开关
> `agentWorkerTurns`（默认关，设置页可开）。验收：Rust 1260 passed + 1 预存环境失败、
> tsc 0 错、vitest 44 文件/423 用例全绿（UI 零改动达成）。新增坑与钉子见 2026-10-09 日志：
> 测试二进制的 comctl32 v6 清单问题（muda → TaskDialogIndirect → 0xc0000139）与
> chat.rs 守卫的"中段不得插 #[cfg(test)]"铁律。⑤ 真机一问一答待真机开开关验收；
> ⑥ spawn/browser/agent_control 在 worker 侧已诚实拒绝（chat_heavy_tools.rs）。

**①②③ 已完成**（db0b817 / 26ae7e0 / 4672ab4）：读线程化 + 三线程架构、turn.start 异步回合
（started 立即回执 → 回合线程 → ev("chat") 透传 → turn.done 收尾）、tool.decide/steer.push
回合中并发直达 hub、Supervisor `set_event_sink`（ev 不终结请求）、集成测试第 5 条钉死
异步生命周期三段形状。

**开工前事实（本会话已侦察，直接引用）**：
- 前端唯一 chat_send 调用点：`src/lib/chat-transport.ts:105 sendChat`——`Channel<ChatEvent>`
  语义（`channel.onmessage = onEvent`），UI 零改动 = 这条通道的字节形状不许变。
- Main 分流点：`chat.rs:3929 chat_send`（六 hub + 参数 + `on_event: Channel` →
  `spawn_send_turn` 内联线程）。分流开关立在这条命令入口。
- 监督者：`agent_supervisor.rs:354` SUPERVISOR OnceLock；`request(methods::TURN_START, params)`
  可发异步回合。worker 方法面**没有 turn.stop**——abort 腿缺失（见第 4 刀）。
- turn.start params 现在只有 `{prompt}`（turn.once 执行体）；真参数还没进协议。

**④ Main 侧 LocalHost 伪装 invoke 接 UI（最大件，五刀）**：
1. **参数腿**：turn.start params 扩成真参数（conversationId/input/attachments/rewindTo/
   skipMemory/poolPick，camelCase 对齐 chat_send 的 serde 入参）；worker 端把 turn.once
   执行体换成 **run_turn 全量**——目录锚点三行换成 WorkerContext 的 config_dir/data_dir，
   `summarize_history` / `ink::new` 随行（§A5 第 3 档最后的内部件）。
2. **事件腿**：Main 侧把 `Channel<ChatEvent>` 的 send 包成 EventSink 挂 `set_event_sink`；
   ev("chat") 的 data 反序列化回 ChatEvent 直接 send 进 Channel（serde tag=camelCase，透传）。
   **sink 是监督者级单出口**：多话题并发要按 conversationId 过滤（LocalHost 只转发名下话题）；
   第一版先单话题真机验收，多话题路由留 M4。
3. **审批 ev 闭环**：worker 发 `approval_request` ev → Main 弹真审批 UI → 决定经
   `tool.decide` 送回。`close_turn / park_unattended / auto_review_verdict /
   toast::approval_needed` 全走 ev 由 Main 代发（第 4 档同理）。
4. **abort 腿（工单新增，§A6 原稿没写）**：worker 加 `turn.stop` 方法（→ worker 侧
   StopHub），Main 的 `chat_abort` 分流后改打 worker。否则分流后"停止"按钮失灵。
   tool.decide/steer.push 的并发直达路由就是现成模板。
5. **边界**：第一版只分流 `chat_send` 主入口；`chat_follow_up`（goal 轮）与
   `session_goal_pause` 等留 Main 内联。两路并存时同话题互斥：Main 侧登记要能看见
   worker 回合（`stop_hub.is_running` 的竞态注释在这里同样适用）。

**⑤ 真机一问一答验收**：agent 集成测试 5 条全绿（真机桌面）→ 真机发消息、流式回复、
工具卡照常、审批弹窗跨进程可用、停止按钮生效 → **vitest 全绿不改一行**（M2 里程碑验收条款）。

**⑥ 重活类 M3.5（留尾）**：`spawn::run_from_chat` / `agent_control_exec` /
`browser::handle_tool`——真机验收期在 worker 侧先"诚实拒绝"（err 信封），别 panic；
主对话轮跑通后逐个评估搬迁（各需独立评估 AppHandle 依赖）。
