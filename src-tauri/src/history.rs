mod json_store;
mod sqlite_store;

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::config;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallRecord {
    pub id: String,
    pub name: String,
    pub status: String,
    pub risk: String,
    pub input: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// 原始参数 JSON 文本。话题恢复重放历史时要用它重建 tool_calls
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    /// 这一发声明时本轮正文已流出的 UTF-16 码元数。前端按它把工具行插回原文流
    /// （流式内联）。None = 加这一格之前落的历史，按堆叠布局显示
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_chars: Option<u32>,
}

/// 流式内联的步骤章：前端按 content_chars 把思考段/工具行插回原文流。
/// `from` 是思考段在**拼接后** reasoning 里的 UTF-16 起点（多条回答的思考
/// 同样用空行缝拼接）。空 = 加这一格之前落的历史
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepRecord {
    pub kind: String,
    pub id: String,
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_chars: Option<u32>,
}

/// 用户消息上的附件元数据（截图/文件）。只存引用（name/kind/path/bytes）不存内容——
/// 缩略图 data URL 是几百 KB 的膨胀源，界面要用时走 asset 协议从 path 读
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AttachmentRecord {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MessageRecord {
    pub id: String,
    pub role: String,
    pub content: String,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 这一发**实际发出去**的模型名。池子与路由表会在请求前换人，所以顶层配置里那个
    /// 名字回答不了"这句是谁答的"——界面那行归属读数抄的是这里。
    /// None = 这一格加进来之前落的历史，当时没人记过它（不是"没用模型"）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 这条用户消息带的附件（截图/文件）。预览图（data URL）在 TS 侧落盘前已剥掉
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentRecord>,
    /// 分支树上的父节点。**由后端铸造、前端抄录**（见 `chat::conversation_tree`），
    /// 前端不自造父子关系——否则又是一份"界面说一条路、日志说另一条"的双轨真相。
    /// None = 这一支的根（老库里回填成线性链，所以老话题的根只有第一条）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<StepRecord>,
    /// 这条消息对应的日志条目 id。前端收 Done 时贴、后端投影补账时也写——
    /// 它是台账与日志之间的**去重键**：广播轮（目标轮）没有前端现场写台账，
    /// 后端按它分清"哪段已经在账上"，追加式补账才不会写出两份
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entry_ids: Vec<String>,
    /// 目标模式的第几轮。投影按**轮**归并 assistant 行（一轮"说话→调工具→再说话"
    /// 在日志里是多行），这一格给合并出的气泡盖上"目标 · 第 N 轮"——
    /// 重载后的界面与直播气泡说同一句话。None = 非目标轮，不冒充
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_round: Option<u32>,
    /// 视频画布节点：这条消息属于哪个节点。只有视频会话写它；None = 老消息/
    /// 非视频会话——前端把没标的行归到第一个节点名下（老视频会话的归属规则）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// 生成会话的产物标记（media 管线的气泡）：image/video/audio/music/text/transcribe。
    /// 界面靠它把这类气泡认成"产物"（操作排不挂复制/重生成那一排）。
    /// None = 对话轮消息；老存档没这一格，读回来按普通消息画
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageRecord {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub duration_ms: u64,
}

fn default_conversation_kind() -> String {
    "chat".to_string()
}

/// 视频会话画布上的一个节点（即梦式"每个节点一段独立对话"）。
/// 节点只是登记（id/名/建时），内容长在消息上——消息行的 `node_id` 指回这里
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VideoNodeRecord {
    pub id: String,
    pub label: String,
    pub created_at: i64,
}

/// 节点之间的连接（有向：from → to）。语义：下游生成时自动把上游的
/// 最新产物当参照素材（图片→参考图/全能参照、视频→编辑素材、音频→转写素材）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VideoEdgeRecord {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Conversation {
    pub id: String,
    /// 归属的工作目录；工作目录被删时保留字符串，便于事后确认
    pub project_id: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// 用户置顶。旧档案里没有这个键：serde 容器级 default 让它落成 false
    pub pinned: bool,
    /// 会话的能力档："chat" | "image" | "video"。旧档案没有这个键：default
    /// 直接落 "chat"——空串会让前端的档位比较全部错位
    #[serde(default = "default_conversation_kind")]
    pub kind: String,
    /// 视频会话的画布节点登记。只在 kind = "video" 时有内容；旧档案没有这个键
    /// 落空——画布从消息行里还能认领回节点（见前端的派生规则）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video_nodes: Vec<VideoNodeRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video_edges: Vec<VideoEdgeRecord>,
    pub messages: Vec<MessageRecord>,
    pub usage: Option<UsageRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConversationMeta {
    pub id: String,
    pub title: String,
    pub project_id: String,
    pub updated_at: i64,
    pub message_count: usize,
    pub preview: String,
    pub pinned: bool,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageInfo {
    /// 当前生效的后端："json" | "sqlite"
    pub backend: String,
    pub json_dir: String,
    pub json_count: usize,
    pub sqlite_file: String,
    pub sqlite_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageSwitch {
    pub config: config::AppConfig,
    pub info: StorageInfo,
    /// 从旧后端拷过来的话题条数
    pub moved: usize,
}

/// 一处本地存储。两种后端实现同样的四个动作，切换与测试都能走同一套代码。
#[derive(Debug, Clone)]
pub enum Location {
    /// 目录，每条话题一个 <id>.json
    Dir(PathBuf),
    /// 单个 SQLite 文件
    Database(PathBuf),
}

impl Location {
    pub fn backend(&self) -> &'static str {
        match self {
            Location::Dir(_) => "json",
            Location::Database(_) => "sqlite",
        }
    }

    pub fn path(&self) -> &PathBuf {
        match self {
            Location::Dir(path) | Location::Database(path) => path,
        }
    }

    pub fn exists(&self) -> bool {
        self.path().exists()
    }

    fn list(&self) -> Result<Vec<ConversationMeta>, String> {
        match self {
            Location::Dir(dir) => json_store::list(dir),
            Location::Database(file) => sqlite_store::with(file, sqlite_store::list),
        }
    }

    /// 只取一条话题的项目归属（两个后端各自的轻量读取）。
    /// 回合开始时判定文件工具落在哪个项目要用它，整份 load 不值这份钱
    fn load_project_id(&self, id: &str) -> Result<String, String> {
        match self {
            Location::Dir(dir) => json_store::load_project_id(dir, id),
            Location::Database(file) => {
                sqlite_store::with(file, |conn| sqlite_store::load_project_id(conn, id))
            }
        }
    }

    fn load_all(&self) -> Result<Vec<Conversation>, String> {
        match self {
            Location::Dir(dir) => json_store::load_all(dir),
            Location::Database(file) => sqlite_store::with(file, sqlite_store::load_all),
        }
    }

    pub(crate) fn load(&self, id: &str) -> Result<Conversation, String> {
        match self {
            Location::Dir(dir) => json_store::load(dir, id),
            Location::Database(file) => {
                sqlite_store::with(file, |conn| sqlite_store::load(conn, id))
            }
        }
    }

    fn save(&self, conversation: Conversation) -> Result<ConversationMeta, String> {
        let conversation = sanitized(conversation);
        match self {
            Location::Dir(dir) => json_store::save(dir, &conversation),
            Location::Database(file) => {
                sqlite_store::with(file, |conn| sqlite_store::save(conn, &conversation))
            }
        }
    }

    fn remove(&self, id: &str) -> Result<(), String> {
        match self {
            Location::Dir(dir) => json_store::remove(dir, id),
            Location::Database(file) => {
                if !file.exists() {
                    return Ok(());
                }
                sqlite_store::with(file, |conn| sqlite_store::remove(conn, id))
            }
        }
    }

    /// 只做目录列举 / COUNT(*)，不解析消息正文
    fn count(&self) -> Result<usize, String> {
        if !self.exists() {
            return Ok(0);
        }
        match self {
            Location::Dir(dir) => json_store::count(dir),
            Location::Database(file) => sqlite_store::with(file, sqlite_store::count),
        }
    }
}

/// 流式中间态不该落盘：恢复时会留下一个永远不会结束的"正在生成"
fn sanitized(conversation: Conversation) -> Conversation {
    let messages = conversation
        .messages
        .into_iter()
        .map(|mut message| {
            message
                .tool_calls
                .retain(|call| call.status != "pending" && call.status != "running");
            message
        })
        .collect();

    Conversation {
        messages,
        ..conversation
    }
}

pub fn meta_of(conversation: &Conversation) -> ConversationMeta {
    let preview = conversation
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "assistant" && !message.content.is_empty())
        .or_else(|| {
            conversation
                .messages
                .iter()
                .rev()
                .find(|message| !message.content.is_empty())
        })
        .map(|message| message.content.chars().take(80).collect())
        .unwrap_or_default();

    ConversationMeta {
        id: conversation.id.clone(),
        title: conversation.title.clone(),
        project_id: conversation.project_id.clone(),
        updated_at: conversation.updated_at,
        message_count: conversation.messages.len(),
        preview,
        pinned: conversation.pinned,
        // 旧档案/旧库落成空串：侧栏一律按空=chat 兜底
        kind: if conversation.kind.is_empty() {
            "chat".to_string()
        } else {
            conversation.kind.clone()
        },
    }
}

fn data_root(app: &AppHandle) -> Result<PathBuf, String> {
    let root = app.path().app_data_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    Ok(root)
}

/// "sqlite" 之外的任何值都按 json 处理：这个字段来自配置文件，坏值不该让应用起不来
pub fn location(app: &AppHandle, backend: &str) -> Result<Location, String> {
    location_in(&data_root(app)?, backend)
}

/// worker 进程的变体（M2 切片 2）：目录由 Main 经 CLI 传来，不问 AppHandle
pub fn location_in(data_root: &std::path::Path, backend: &str) -> Result<Location, String> {
    if backend == "sqlite" {
        return Ok(Location::Database(data_root.join("conversations.db")));
    }
    let dir = data_root.join("conversations");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(Location::Dir(dir))
}

/// 当前生效的存档位置。回合开始时要读台账（只为"日志不在就迁一次"这一件事），
/// 所以这里对 crate 内开放；界面侧一律走下面的 history_* 命令
pub(crate) fn current(app: &AppHandle) -> Result<Location, String> {
    current_in(
        &app.path().app_config_dir().map_err(|e| e.to_string())?,
        &app.path().app_data_dir().map_err(|e| e.to_string())?,
        &config::load(app).conversation_store,
    )
}

/// worker 进程的变体（M2 切片 2）：config 与 data 目录都由 Main 传来
pub(crate) fn current_in(
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
    backend: &str,
) -> Result<Location, String> {
    location_in(data_dir, backend).map(|location| {
        let _ = config_dir; // json 台账只认 data 目录；config_dir 留给未来要读它的变体
        location
    })
}

/// 调度线程也要往当前后端存话题，所以把这条路径暴露出来而不是让它自己拼
pub fn save_conversation(
    app: &AppHandle,
    conversation: Conversation,
) -> Result<ConversationMeta, String> {
    if conversation.id.is_empty() {
        return Err("话题 id 为空。".into());
    }
    current(app)?.save(conversation)
}

/// 导入时判断 id 是否已存在。不把整条话题读回来——判断存在性只需要一个 Ok/Err
pub fn conversation_exists(app: &AppHandle, id: &str) -> bool {
    current(app)
        .map(|store| store.load(id).is_ok())
        .unwrap_or(false)
}

/// 话题台账里绑的项目 id。没绑过（老档案/空串）或读不到（话题还没落过档）= None。
/// 回合开始时文件工具的根目录要从这里取归属：侧栏按话题归属分组、输入框旁的
/// 选择器显示话题归属，后端必须说同一句话
pub(crate) fn conversation_project_id(app: &AppHandle, id: &str) -> Option<String> {
    let project_id = current(app).ok()?.load_project_id(id).ok()?;
    let trimmed = project_id.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// 把 from 里目标端没有（或目标端更旧）的话题拷过去。只拷不删，源数据保持原样。
fn migrate(from: &Location, to: &Location) -> Result<usize, String> {
    let mut moved = 0;
    for conversation in from.load_all()? {
        let fresher = match to.load(&conversation.id) {
            Ok(existing) => conversation.updated_at > existing.updated_at,
            Err(_) => true,
        };
        if fresher {
            to.save(conversation)?;
            moved += 1;
        }
    }
    Ok(moved)
}

fn info(app: &AppHandle, backend: &str) -> Result<StorageInfo, String> {
    let json = location(app, "json")?;
    let sqlite = location(app, "sqlite")?;
    let active = if backend == "sqlite" {
        sqlite.clone()
    } else {
        json.clone()
    };

    Ok(StorageInfo {
        backend: active.backend().to_string(),
        json_dir: json.path().to_string_lossy().into_owned(),
        json_count: json.count().unwrap_or(0),
        sqlite_file: sqlite.path().to_string_lossy().into_owned(),
        sqlite_count: sqlite.count().unwrap_or(0),
    })
}

/// 重 IO 的统一壳：把同步实现丢进阻塞线程池。Tauri 的同步命令在主线程上跑，
/// 侧栏/保存/迁移一慢整个窗口跟着冻；命令体一律走这里，IO 不再占主线程
pub(crate) async fn run_blocking<T, F>(task: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(task)
        .await
        .map_err(|e| format!("后台任务执行失败：{e}"))?
}

/// 命令的同步实现：后台线程（目标扫描、调度落话题）不走命令也要同一份列表
pub(crate) fn list_current(app: &AppHandle) -> Result<Vec<ConversationMeta>, String> {
    current(app)?.list()
}

/// 命令的同步实现：恢复话题上下文的内部调用要与"打开话题"拿到同一份投影
pub(crate) fn load_current(app: &AppHandle, id: &str) -> Result<Conversation, String> {
    let mut conversation = current(app)?.load(id)?;
    // 正文以**日志投影**为准（覆盖台账里那份）：广播轮（目标轮）的正文只进日志，
    // 台账靠前端现场整份覆盖写——两边各存一份同一段对话，必然各缺一角。
    // 读侧统一投影，"打开话题"才拿得到完整的执行过程。元信息（标题/用量/归属）
    // 仍是台账的：日志不存第二份钱，标题的编辑权在界面。
    // 投影失败（日志读不到/损坏）就退回台账原样——台账总是可读的底线
    //
    // **例外：生成会话（image/video）跳过投影**。它们的产物附件只住在台账里
    // （生成管线不写日志），日志投影的消息不带附件——拿投影覆盖等于把图片
    // 挤掉，下次落盘连存档都洗成空（真机踩过：切会话回来图就没了）
    let is_chat = conversation.kind.is_empty() || conversation.kind == "chat";
    if is_chat {
        match crate::chat::open_session(&app, &id) {
            Ok(session) => match projected_messages(&session.log) {
                Ok(mut messages) if !messages.is_empty() => {
                    // 投影的 assistant 行不知道"这一发用的是谁"（日志条目不带模型，
                    // 实发模型名是 done 事件贴回台账的那一格）。整份替换前按条目 id
                    // 把它从台账认领回来，否则切换对话后每条脚注都成了"—"
                    restore_models_from_ledger(&mut messages, &conversation.messages);
                    conversation.messages = messages;
                }
                // 空投影 = 日志里还没有能显示的对话（刚迁移/只有记账行）：留着台账的
                Ok(_) => {}
                Err(error) => eprintln!("日志投影没成功，界面先用台账那份：{error}"),
            },
            Err(error) => eprintln!("话题日志打不开，界面先用台账那份：{error}"),
        }
    }
    Ok(conversation)
}

#[tauri::command]
pub async fn history_list(app: AppHandle) -> Result<Vec<ConversationMeta>, String> {
    run_blocking(move || list_current(&app)).await
}

#[tauri::command]
pub async fn history_load(app: AppHandle, id: String) -> Result<Conversation, String> {
    run_blocking(move || load_current(&app, &id)).await
}

/// 模型名只活在台账（done 事件贴回的实发名），投影重建的行没有这一格。
/// 按条目 id 认领：投影行的 entry_ids 与台账行的 entry_ids 有交集就抄过来。
/// 只抄模型名这一格，正文/思考/调用格一概以投影为准（那才是完整的执行过程）；
/// 已有值的行不覆盖——投影将来自己带上模型时，这里的认领自动变成纯兜底
fn restore_models_from_ledger(messages: &mut [MessageRecord], ledger: &[MessageRecord]) {
    let models: std::collections::HashMap<&str, &str> = ledger
        .iter()
        .filter_map(|record| record.model.as_deref().map(|model| (record, model)))
        .flat_map(|(record, model)| record.entry_ids.iter().map(move |id| (id.as_str(), model)))
        .collect();
    for record in messages.iter_mut() {
        if record.model.is_some() {
            continue;
        }
        let claimed = record
            .entry_ids
            .iter()
            .find_map(|id| models.get(id.as_str()));
        if let Some(model) = claimed {
            record.model = Some((*model).to_string());
        }
    }
}

/// 把话题日志投影成界面的消息（看得见的那条分支 + 切走的兄弟分支，前端按 tip 再分）。
///
/// 复用发送视图的投影管线（`context::project`）：压缩边界、段摘要、条目编辑撤回，
/// 在"模型看到什么"与"界面显示什么"之间保持同一套语义——被压缩的旧消息不回来，
/// 摘要站在它原来的位置上。改写类条目（压缩/段摘要）渲染成一条摘要消息，
/// 而不是它们展开给模型的那几行 wire（system 快照 + 摘要行是发请求的形状）。
///
/// 工具结果并进前面最近的助手消息的调用格——界面上结果长在调用上，日志里它们是相邻的两条。
/// 一轮的连续 assistant 行攒成同一条消息的撮合器。
///
/// 一轮目标在日志里是"说话 → 调工具 → 再说话"——**多行 assistant**。按行投影，
/// 收尾刷新后同一轮就散成一段一段（用户看到的就是这个）。直播气泡的形状是
/// 一轮一格：这里把同轮的行并回去——正文拼接、推理拼接、调用格并进来、
/// 条目 id 全保留（回溯与补账要的锚一格不少）。轮的边界：用户发言、续跑行、
/// 压缩/段摘要——这些一到手就先落定手头那条。
struct RoundMerger {
    held: Option<MessageRecord>,
}

impl RoundMerger {
    fn flush(&mut self, messages: &mut Vec<MessageRecord>) {
        if let Some(record) = self.held.take() {
            messages.push(record);
        }
    }

    /// 这一行的章盖在"它要并进的那条记录"的哪里：同一轮就接在手头正文/思考的
    /// 末尾（拼接缝 "\n\n" 占 2 个 UTF-16 码元），另起一格就从 0 盖。
    /// **必须是记录内坐标**：界面的切片读的是合并后这一格自己的
    /// content/reasoning，混进全对话累计游标就必然越界成空切片
    /// （「思考内容不显示」根因：第二条回复的 from 越过本行自己的 reasoning 长度）。
    /// 缝的口径三处逐字同源：push_assistant 拼 "\n\n"、这里记 2、chat.rs 盖工具章的
    /// seam=2——任何一处单改，切片就错位一格
    fn stamp_bases(
        &self,
        settled: &crate::session::entry::SettledAssistant,
        goal_round: Option<u32>,
    ) -> (usize, usize) {
        match &self.held {
            Some(held) if held.goal_round == goal_round => {
                let content_base = held.content.encode_utf16().count()
                    + 2 * usize::from(!held.content.is_empty() && !settled.content.is_empty());
                let reasoning_base = held
                    .reasoning
                    .as_ref()
                    .map_or(0, |r| r.encode_utf16().count())
                    + 2 * usize::from(held.reasoning.is_some() && settled.reasoning.is_some());
                (content_base, reasoning_base)
            }
            _ => (0, 0),
        }
    }

    fn push_user(&mut self, messages: &mut Vec<MessageRecord>, record: MessageRecord) {
        self.flush(messages);
        messages.push(record);
    }

    fn push_assistant(&mut self, messages: &mut Vec<MessageRecord>, record: MessageRecord) {
        match &mut self.held {
            // 同一轮：并进手头那条。轮次标记不同的（续跑行换过轮）另起一格
            Some(held) if held.goal_round == record.goal_round => {
                if !record.content.is_empty() {
                    if !held.content.is_empty() {
                        held.content.push_str("\n\n");
                    }
                    held.content.push_str(&record.content);
                }
                match (&mut held.reasoning, &record.reasoning) {
                    (_, None) => {}
                    (None, Some(reasoning)) => held.reasoning = Some(reasoning.clone()),
                    (Some(seam), Some(reasoning)) => {
                        seam.push_str("\n\n");
                        seam.push_str(reasoning);
                    }
                }
                held.tool_calls.extend(record.tool_calls);
                // 章是逐行盖的：并格后每一行的思考/工具章都得留下，
                // 丢了后行的章，重载后那一行的思考与工具就插不回原文流
                held.steps.extend(record.steps);
                held.entry_ids.extend(record.entry_ids);
                // 同一轮共享同一个实发模型：手头没有就用后行的补上
                if held.model.is_none() {
                    held.model = record.model;
                }
                if record.error.is_some() {
                    held.error = record.error;
                }
            }
            _ => {
                self.flush(messages);
                self.held = Some(record);
            }
        }
    }

    /// 结果行跟自己那行 assistant 走：手头还攒着的优先，落定过的从尾往前找
    fn merge_tool_result(
        &mut self,
        messages: &mut Vec<MessageRecord>,
        tool_call_id: &str,
        content: &str,
    ) {
        let held = self.held.as_mut().into_iter();
        let settled = messages
            .iter_mut()
            .rev()
            .filter(|record| record.role == "assistant");
        for record in held.chain(settled) {
            if let Some(call) = record
                .tool_calls
                .iter_mut()
                .find(|call| call.id == tool_call_id)
            {
                call.output = Some(content.to_string());
                call.status = "done".into();
                return;
            }
        }
    }
}

fn user_record(
    entry_id: &str,
    timestamp: i64,
    parent_id: Option<String>,
    content: &str,
    images: &[crate::session::entry::ImageRef],
) -> MessageRecord {
    MessageRecord {
        id: entry_id.into(),
        role: "user".into(),
        content: content.to_string(),
        created_at: timestamp,
        attachments: images
            .iter()
            .map(|image| AttachmentRecord {
                name: std::path::Path::new(&image.path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| image.path.clone()),
                kind: if image.mime.starts_with("image/") {
                    "image".into()
                } else {
                    "text".into()
                },
                path: image.path.clone(),
                bytes: image.bytes,
            })
            .collect(),
        entry_ids: vec![entry_id.into()],
        parent_id,
        ..MessageRecord::default()
    }
}

fn assistant_record(
    entry_id: &str,
    timestamp: i64,
    parent_id: Option<String>,
    settled: &crate::session::entry::SettledAssistant,
    goal_round: Option<u32>,
    // 本行在**要并进的那条记录**里的起点（UTF-16 码元）：同轮合并时接在前文之后，
    // 另起一格就是 0。思考章按它取记录内坐标——界面的切片读的是合并后这一格
    // 自己的 reasoning，混进全对话累计游标就必然越界成空。
    // 工具章不走这里：盖章器给的就是合并口径，原样透传（见下面的 steps.extend）
    content_base: usize,
    reasoning_base: usize,
    // 条目上随账的实发模型名（assistant 行落账时带的）。None = 老日志，
    // 靠 restore_models_from_ledger 从台账认领
    model: Option<&str>,
) -> MessageRecord {
    let mut steps: Vec<StepRecord> = Vec::new();
    if settled.reasoning.as_ref().map_or(false, |r| !r.is_empty()) {
        steps.push(StepRecord {
            kind: "thinking".into(),
            id: format!("{entry_id}#think"),
            at: timestamp,
            from: Some(reasoning_base as u32),
            call_id: None,
            content_chars: Some(content_base as u32),
        });
    }
    steps.extend(settled.tool_calls.iter().map(|call| StepRecord {
        kind: "tool".into(),
        id: format!("{entry_id}#{}", call.id),
        at: timestamp,
        from: None,
        call_id: Some(call.id.clone()),
        // 工具章原样透传、不加基点：chat.rs 盖章时已经按"合并后这一格"的口径
        // 累计（content_chars_so_far 跨轮累进，轮与轮之间加 2 个缝码元），
        // 恰好就是投影合并出的这条记录的坐标系；再加本行基点反而重复计前文
        content_chars: call.content_chars,
    }));
    MessageRecord {
        id: entry_id.into(),
        role: "assistant".into(),
        content: settled.content.clone(),
        created_at: timestamp,
        reasoning: settled.reasoning.clone(),
        error: settled.error.clone(),
        tool_calls: settled
            .tool_calls
            .iter()
            .map(|call| ToolCallRecord {
                id: call.id.clone(),
                name: call.name.clone(),
                // 没等到结果行的调用按"已完成"记账：截断与拒绝也会落结果行，
                // 走到没有结果的只剩异常路径，宁可显示完成也不留一个假"生成中"
                status: "done".into(),
                risk: String::new(),
                input: String::new(),
                output: None,
                arguments: Some(call.arguments.clone()),
                content_chars: call.content_chars,
            })
            .collect(),
        steps,
        entry_ids: vec![entry_id.into()],
        parent_id,
        goal_round,
        model: model.map(str::to_string),
        ..MessageRecord::default()
    }
}

fn projected_messages(log: &crate::session::SessionLog) -> Result<Vec<MessageRecord>, String> {
    use crate::session::context;
    use crate::session::entry::{EntryPayload, Message};
    use crate::session::mode::CONTINUATION_TYPE;

    let projection = context::project(log).map_err(|error| error.to_string())?;
    let mut messages: Vec<MessageRecord> = Vec::new();
    let mut merger = RoundMerger { held: None };
    let mut round: u32 = 0;

    // 看得见的那条分支：生效序列（压缩/段摘要/编辑撤回已由管线应用）
    for (entry_id, rows) in &projection.entries {
        let Some(entry) = log.entry(entry_id) else { continue };
        // 续跑行是轮的边界与图章：它自己不显示，下一行 assistant 是新一轮的开始，
        // 合并出的气泡从此盖"目标 · 第 N 轮"
        if let EntryPayload::CustomMessage { custom_type, .. } = entry.payload() {
            if custom_type == CONTINUATION_TYPE {
                merger.flush(&mut messages);
                round += 1;
                continue;
            }
        }
        match entry.payload() {
            EntryPayload::Compaction { summary, .. }
            | EntryPayload::BranchSummary { summary, .. } => {
                merger.flush(&mut messages);
                messages.push(MessageRecord {
                    id: entry_id.clone(),
                    role: "assistant".into(),
                    content: format!(
                        "【上下文压缩完成】更早的对话已压缩成摘要，任务上下文已衔接，继续处理中。\n\n{summary}"
                    ),
                    created_at: entry.timestamp,
                    entry_ids: vec![entry_id.clone()],
                    parent_id: entry.parent_id.clone(),
                    ..MessageRecord::default()
                });
            }
            _ => {
                for message in rows {
                    match message {
                        Message::User { content, images, .. } => merger.push_user(
                            &mut messages,
                            user_record(
                                entry_id,
                                entry.timestamp,
                                entry.parent_id.clone(),
                                content,
                                images,
                            ),
                        ),
                        Message::Assistant(settled) => {
                            // 章盖在"并进那格记录之后"的记录内位置上：缝的判据在
                            // stamp_bases 与 push_assistant 里保持逐字同源
                            let goal = (round > 0).then_some(round);
                            let (content_base, reasoning_base) =
                                merger.stamp_bases(settled, goal);
                            let record = assistant_record(
                                entry_id,
                                entry.timestamp,
                                entry.parent_id.clone(),
                                settled,
                                goal,
                                content_base,
                                reasoning_base,
                                entry.model(),
                            );
                            merger.push_assistant(&mut messages, record);
                        }
                        Message::Tool {
                            tool_call_id,
                            content,
                        } => merger.merge_tool_result(&mut messages, tool_call_id, content),
                        Message::System { .. } => {}
                    }
                }
            }
        }
    }
    merger.flush(&mut messages);
    // 切走的兄弟分支：日志里有、又不在叶链生效序列上的对话行，按日志顺序投影——
    // 分支切换器靠它们长出"第几支"；父链挂在分叉点（那条在叶链上），跨集合也连得上。
    // 排除集除了生效序列还要带上**省略账**：被压缩/被段摘要顶替的旧消息仍在叶链上
    // （日志只追加，压缩是投影语义不是改写），漏了它们就会以"兄弟分支"的形状还魂。
    // 归并与轮次计数照主线同一套：兄弟支上的目标轮同样一格一轮
    let mut accounted: std::collections::HashSet<&str> = projection
        .entries
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    for omission in &projection.omissions {
        accounted.insert(omission.entry_id.as_str());
    }
    for entry in log.entries() {
        if accounted.contains(entry.id.as_str()) {
            continue;
        }
        match entry.payload() {
            EntryPayload::CustomMessage { custom_type, .. }
                if custom_type == CONTINUATION_TYPE =>
            {
                merger.flush(&mut messages);
                round += 1;
            }
            EntryPayload::Message { message } => match message {
                Message::User { content, images, .. } => merger.push_user(
                    &mut messages,
                    user_record(
                        &entry.id,
                        entry.timestamp,
                        entry.parent_id.clone(),
                        content,
                        images,
                    ),
                ),
                Message::Assistant(settled) => {
                    let goal = (round > 0).then_some(round);
                    let (content_base, reasoning_base) = merger.stamp_bases(settled, goal);
                    let record = assistant_record(
                        &entry.id,
                        entry.timestamp,
                        entry.parent_id.clone(),
                        settled,
                        goal,
                        content_base,
                        reasoning_base,
                        entry.model(),
                    );
                    merger.push_assistant(&mut messages, record);
                }
                Message::Tool {
                    tool_call_id,
                    content,
                } => merger.merge_tool_result(&mut messages, tool_call_id, content),
                Message::System { .. } => {}
            },
            _ => {}
        }
    }
    merger.flush(&mut messages);
    Ok(messages)
}

/// 保存并同步全文搜索索引：命令与内部调用（调度线程、定时任务落话题）共用这一条路，
/// 保证任何落盘口索引都不会漏
pub(crate) fn save_and_index(
    app: &AppHandle,
    conversation: Conversation,
) -> Result<ConversationMeta, String> {
    let meta = save_conversation(app, conversation.clone())?;
    // 全文搜索索引同步：保存这一个口两种后端都过，索引跟在这里不会漏
    crate::search::on_saved(app, &conversation);
    Ok(meta)
}

#[tauri::command]
pub async fn history_save(
    app: AppHandle,
    conversation: Conversation,
) -> Result<ConversationMeta, String> {
    run_blocking(move || save_and_index(&app, conversation)).await
}

/// 全量扫描给搜索索引用：两个后端各扫一份，由 search::rebuild 去重
pub(crate) fn load_all_for_search(app: &AppHandle) -> Result<Vec<Conversation>, String> {
    let json = location(app, "json")?;
    let sqlite = location(app, "sqlite")?;
    let mut seen: std::collections::BTreeMap<String, Conversation> = Default::default();
    for store in [&json, &sqlite] {
        if let Ok(conversations) = store.load_all() {
            for conversation in conversations {
                match seen.get(&conversation.id) {
                    Some(existing) if existing.updated_at >= conversation.updated_at => {}
                    _ => {
                        seen.insert(conversation.id.clone(), conversation);
                    }
                }
            }
        }
    }
    Ok(seen.into_values().collect())
}

/// 删除要两边都删：只删当前后端的话，那条话题会在下次切回去时被迁移逻辑"补"回来。
fn remove_everywhere(stores: &[&Location], id: &str) -> Result<(), String> {
    for store in stores {
        store.remove(id)?;
    }
    Ok(())
}

#[tauri::command]
pub async fn history_remove(app: AppHandle, id: String) -> Result<(), String> {
    run_blocking(move || {
        remove_everywhere(&[&location(&app, "json")?, &location(&app, "sqlite")?], &id)?;
        crate::search::on_removed(&app, &id);
        // 删掉一段对话也要把它那一侧的作用域一起忘掉：技能白名单与更严的那张权限表
        // 是按话题 id 存的，而这个 id 可能被补回来（后端切回时的迁移、从别的 app 导入）。
        // 那时它该是一段新对话，不该继承上一次留下的限制
        crate::tool_runtime::forget_session(&id);
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn storage_info(app: AppHandle) -> Result<StorageInfo, String> {
    run_blocking(move || info(&app, &config::load(&app).conversation_store)).await
}

/// 切换后端：先把旧存储拷进新存储，再把开关写回 config.json。
/// 话题不会被删除，所以切回去还能拿回原来的那批文件。
#[tauri::command]
pub async fn storage_switch(app: AppHandle, backend: String) -> Result<StorageSwitch, String> {
    run_blocking(move || {
        let target = if backend == "sqlite" {
            "sqlite"
        } else {
            "json"
        };
        let from = current(&app)?;
        let to = location(&app, target)?;

        let moved = if from.backend() == target {
            0
        } else {
            migrate(&from, &to)?
        };

        let mut config = config::load(&app);
        config.conversation_store = target.to_string();
        config::save(&app, &config)?;

        Ok(StorageSwitch {
            info: info(&app, target)?,
            moved,
            config,
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{remove_tree, temp_dir};

    /// 删除一条话题时，它那一侧的按话题作用域（技能白名单 + 更严的那张权限表）要一起忘掉。
    /// 命令体要 `AppHandle`，没有行为测试入口，所以钉调用点存在——它证明的是"这条线接上了"，
    /// 不是"它活着"；两半真的被清掉那一判据在 `tool_runtime::tests::forgetting_a_session_clears_both_halves_of_its_scope`
    #[test]
    fn removing_a_session_also_forgets_its_scoped_side() {
        let source = include_str!("history.rs");
        let body = source
            .split("pub async fn history_remove")
            .nth(1)
            .expect("删除那条命令")
            .split("\n#[tauri::command]")
            .next()
            .expect("到下一条命令为止");
        assert!(
            body.contains("forget_session(&id)"),
            "删除时没清按话题的作用域：同一个 id 被补回来时会继承上一次的 restriction"
        );
    }

    /// 老库（还没有 `parent_id` 那一列）打开时要补列，并且**只在做列的那一次**按 seq
    /// 把前一条回填成父——老话题本来就是一条链。之后再打开绝不能重跑：那时 NULL 的意思
    /// 已经是"这真是某一支的根"，重跑一遍就把兄弟链回直线，把分支抹了
    #[test]
    fn an_older_database_gains_a_parent_column_and_a_one_time_linear_backfill() {
        let root = temp_dir("history-parent-migration");
        let file = root.join("conversations.db");
        {
            let conn = rusqlite::Connection::open(&file).expect("开得了一个空库");
            conn.execute_batch(
                "CREATE TABLE conversations (
                     id TEXT PRIMARY KEY, project_id TEXT NOT NULL DEFAULT '',
                     title TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL DEFAULT 0,
                     updated_at INTEGER NOT NULL DEFAULT 0, usage_input_tokens INTEGER,
                     usage_output_tokens INTEGER, usage_duration_ms INTEGER);
                 CREATE TABLE messages (
                     conversation_id TEXT NOT NULL, seq INTEGER NOT NULL, id TEXT NOT NULL,
                     role TEXT NOT NULL, content TEXT NOT NULL, reasoning TEXT, error TEXT,
                     tool_calls TEXT NOT NULL DEFAULT '[]', created_at INTEGER NOT NULL,
                     PRIMARY KEY (conversation_id, seq));
                 INSERT INTO conversations (id, updated_at) VALUES ('c1', 5);
                 INSERT INTO messages (conversation_id, seq, id, role, content, created_at)
                     VALUES ('c1',0,'m0','user','问',1),
                            ('c1',1,'m1','assistant','答',2),
                            ('c1',2,'m2','user','再问',3);",
            )
            .expect("造一个迁移前的老库");
        }
        let chain = || {
            sqlite_store::with(&file, |conn| {
                let mut stmt = conn
                    .prepare("SELECT id, parent_id FROM messages ORDER BY seq")
                    .map_err(|error| error.to_string())?;
                let rows = stmt
                    .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)))
                    .map_err(|error| error.to_string())?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| error.to_string())?;
                Ok(rows)
            })
        };
        assert_eq!(
            chain().expect("打开该跑迁移"),
            vec![
                ("m0".to_string(), None),
                ("m1".to_string(), Some("m0".to_string())),
                ("m2".to_string(), Some("m1".to_string())),
            ],
            "补列之后老数据要是一条链",
        );

        // 把 m1 改成"另一支的根"，再开一次：回填不能第二次跑把它链回 m0
        sqlite_store::with(&file, |conn| {
            conn.execute("UPDATE messages SET parent_id = NULL WHERE id = 'm1'", [])
                .map_err(|error| error.to_string())?;
            Ok(())
        })
        .expect("改得动");
        assert_eq!(
            chain().expect("第二次打开"),
            vec![
                ("m0".to_string(), None),
                ("m1".to_string(), None),
                ("m2".to_string(), Some("m1".to_string())),
            ],
            "第二次打开不该重跑回填——那会抹掉分支",
        );
    }

    fn temp_location(label: &str) -> (Location, Location, PathBuf) {
        let root = temp_dir(&format!("history-{label}"));
        let json = Location::Dir(root.join("conversations"));
        // 应用里 location() 会建好这个目录，测试要照同样的前置条件来
        fs::create_dir_all(json.path()).unwrap();
        let sqlite = Location::Database(root.join("conversations.db"));
        (json, sqlite, root)
    }

    fn conversation(updated_at: i64) -> Conversation {
        Conversation {
            id: "conv_1".into(),
            project_id: "proj_1".into(),
            title: "标题".into(),
            created_at: 10,
            updated_at,
            pinned: false,
            kind: "chat".to_string(),
            video_nodes: Vec::new(),
            video_edges: Vec::new(),
            messages: vec![
                MessageRecord {
                    id: "msg_u".into(),
                    role: "user".into(),
                    content: "问题".into(),
                    created_at: 11,
                    attachments: vec![AttachmentRecord {
                        name: "paste-20260929.png".into(),
                        kind: "image".into(),
                        path: "C:\\Users\\x\\AppData\\Local\\Temp\\aglab\\paste\\paste-20260929.png".into(),
                        bytes: 204_800,
                    }],
                    ..Default::default()
                },
                MessageRecord {
                    id: "msg_a".into(),
                    role: "assistant".into(),
                    content: "回答".into(),
                    created_at: 12,
                    reasoning: Some("先想了想".into()),
                    steps: vec![StepRecord {
                        kind: "thinking".into(),
                        id: "msg_a#think".into(),
                        at: 12,
                        from: Some(0),
                        call_id: None,
                        content_chars: Some(0),
                    }],
                    tool_calls: vec![ToolCallRecord {
                        id: "call_1".into(),
                        name: "write_file".into(),
                        status: "done".into(),
                        risk: "elevated".into(),
                        input: "{\"path\":\"a.txt\"}".into(),
                        output: Some("已写入".into()),
                        arguments: Some("{\"path\":\"a.txt\"}".into()),
                        content_chars: None,
                    }],
                    error: None,
                    attachments: Vec::new(),
                    parent_id: Some("msg_u".into()),
                    model: Some("deepseek-chat".into()),
                    entry_ids: Vec::new(),
                    goal_round: None,
                    node_id: None,
                    media: Some("music".into()),
                },
            ],
            usage: Some(UsageRecord {
                input_tokens: 21,
                output_tokens: 7,
                duration_ms: 900,
            }),
        }
    }

    fn assert_round_trip(store: &Location) {
        store.save(conversation(100)).unwrap();

        let metas = store.list().unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].title, "标题");
        assert_eq!(metas[0].message_count, 2);
        assert_eq!(metas[0].preview, "回答");
        assert_eq!(store.count().unwrap(), 1);

        let loaded = store.load("conv_1").unwrap();
        assert_eq!(loaded.project_id, "proj_1");
        assert_eq!(loaded.created_at, 10);
        assert_eq!(loaded.usage.unwrap().output_tokens, 7);
        assert_eq!(loaded.messages[1].reasoning.as_deref(), Some("先想了想"));
        assert_eq!(loaded.messages[1].tool_calls[0].name, "write_file");
        assert_eq!(
            loaded.messages[1].tool_calls[0].output.as_deref(),
            Some("已写入")
        );
        assert!(loaded.messages[0].tool_calls.is_empty());
        // 附件元数据往返：两个库都要能带着它回来（界面重开话题时靠它恢复图片显示）
        assert_eq!(loaded.messages[0].attachments.len(), 1);
        assert_eq!(loaded.messages[0].attachments[0].name, "paste-20260929.png");
        assert_eq!(loaded.messages[0].attachments[0].kind, "image");
        // 实发模型同理：答那句用了谁，是这条消息自己的读数，两个库都得带得回来
        assert_eq!(
            loaded.messages[1].model.as_deref(),
            Some("deepseek-chat")
        );
        // 没记过的那条读回来仍是 None。它和"记成了空名字"是两件事——界面那一格
        // 靠这个区别决定是留空还是照抄现在的配置
        assert_eq!(loaded.messages[0].model, None);
        // 生成会话的产物标记同理：两个库都要带得回来——重开后界面靠它认出
        // "这是产物气泡"（操作排不挂复制/重生成那排），而不是普通带附件的消息
        assert_eq!(loaded.messages[1].media.as_deref(), Some("music"));
        assert_eq!(loaded.messages[0].media, None);

        // pending 的工具调用是流式中间态，不能进存储
        let mut dirty = conversation(101);
        dirty.messages[0].tool_calls.push(ToolCallRecord {
            id: "call_2".into(),
            name: "read_text".into(),
            status: "pending".into(),
            risk: "safe".into(),
            input: "{}".into(),
            output: None,
            arguments: Some("{}".into()),
            content_chars: None,
        });
        store.save(dirty).unwrap();
        assert!(store.load("conv_1").unwrap().messages[0]
            .tool_calls
            .is_empty());

        store.remove("conv_1").unwrap();
        assert_eq!(store.count().unwrap(), 0);
        assert!(store.list().unwrap().is_empty());
        // 消息必须跟着消失，否则残留行会在同名话题回来时冒出来
        assert!(store.load("conv_1").is_err());
    }

    #[test]
    fn json_backend_round_trips() {
        let (json, _, root) = temp_location("json");
        assert_round_trip(&json);
        remove_tree(&root);
    }

    #[test]
    fn sqlite_backend_round_trips() {
        let (_, sqlite, root) = temp_location("sqlite");
        assert_round_trip(&sqlite);
        remove_tree(&root);
    }

    /// 回合开始时判定文件工具落在哪个项目，读的就是 load_project_id 这一条：
    /// 两个后端都要答得出"这条话题绑的是哪个项目"，没有这条话题就是读不到
    /// （空串的过滤在 conversation_project_id 那一层）
    #[test]
    fn the_bound_project_id_is_readable_without_the_transcript() {
        let (json, sqlite, root) = temp_location("proj-id");
        for (label, location) in [("json", json), ("sqlite", sqlite)] {
            location.save(conversation(100)).unwrap();
            assert_eq!(
                location.load_project_id("conv_1").unwrap(),
                "proj_1",
                "{label}: 绑定要原样读回来"
            );
            assert!(
                location.load_project_id("conv_missing").is_err(),
                "{label}: 没有这条话题就是读不到"
            );
        }
        remove_tree(&root);
    }

    /// 模型名只活在台账（done 事件贴回的实发名），投影重建的行没有这一格。
    /// 认领按条目 id 对上号；已经有值的行不许被覆盖
    #[test]
    fn model_names_are_claimed_back_from_the_ledger() {
        let mut projected = vec![
            MessageRecord {
                id: "log_u".into(),
                role: "user".into(),
                entry_ids: vec!["e_u".into()],
                ..Default::default()
            },
            MessageRecord {
                id: "log_a".into(),
                role: "assistant".into(),
                entry_ids: vec!["e_a1".into(), "e_a2".into()],
                ..Default::default()
            },
        ];
        let ledger = vec![
            MessageRecord {
                id: "old_u".into(),
                role: "user".into(),
                entry_ids: vec!["e_u".into()],
                ..Default::default()
            },
            MessageRecord {
                id: "old_a".into(),
                role: "assistant".into(),
                model: Some("GLM-5.3-Flash".into()),
                entry_ids: vec!["e_a1".into()],
                ..Default::default()
            },
        ];
        restore_models_from_ledger(&mut projected, &ledger);
        assert_eq!(projected[0].model, None, "用户行没有模型名，认领不着");
        assert_eq!(
            projected[1].model.as_deref(),
            Some("GLM-5.3-Flash"),
            "按条目 id 从台账认领回来"
        );

        projected[1].model = Some("already-there".into());
        restore_models_from_ledger(&mut projected, &ledger);
        assert_eq!(
            projected[1].model.as_deref(),
            Some("already-there"),
            "已有值的行不许被台账盖掉"
        );
    }

    /// 模型名随条目落账后的主路：assistant 条目自带 sent_model，投影直接带上——
    /// 台账在中途重启/崩溃时停在旧一拍（真机踩过：重开话题脚注全成"—"），
    /// 认领救不了不存在的台账行，日志这一格才是恢复的源头
    #[test]
    fn projected_assistant_rows_carry_the_model_logged_on_the_entry() {
        use crate::session::entry::{EntryPayload, Message, NewEntry, PendingAssistant};
        let mut log = crate::session::SessionLog::default();
        log.append(NewEntry::new(EntryPayload::Message { message: Message::User {
                            content: "hi".into(),
                            images: Vec::new(),
                            audios: Vec::new(),
                            videos: Vec::new(),
                        } }), 10)
            .unwrap();
        let settled = PendingAssistant { content: "答".into(), tool_calls: Vec::new() }.settle(crate::session::entry::StopReason::Stop);
        log.append(
            NewEntry::new(EntryPayload::Message { message: Message::Assistant(settled) })
                .with_model("GLM-5.3-Flash"),
            20,
        )
        .unwrap();
        // 同轮第二条 assistant 行也带模型：并格后不丢
        let settled2 = PendingAssistant { content: "续".into(), tool_calls: Vec::new() }.settle(crate::session::entry::StopReason::Stop);
        log.append(
            NewEntry::new(EntryPayload::Message { message: Message::Assistant(settled2) })
                .with_model("GLM-5.3-Flash"),
            30,
        )
        .unwrap();

        let rows = projected_messages(&log).unwrap();
        let merged = rows.iter().find(|row| row.role == "assistant").unwrap();
        assert_eq!(
            merged.model.as_deref(),
            Some("GLM-5.3-Flash"),
            "投影行直接读条目上的实发模型，不再依赖台账的新鲜度"
        );
        // 信封键名避开 payload 的 model 字段：整条日志要能原样回读
        let json = serde_json::to_value(log.path().unwrap().last().unwrap()).unwrap();
        assert!(json.get("sent_model").is_some(), "落盘键是 sent_model");
        assert!(json.get("model").is_none(), "外层不许出现与 Usage 载荷撞名的 model 键");
        let round_tripped: crate::session::Entry =
            serde_json::from_value(json).expect("带 sent_model 的条目要能读回来");
        assert_eq!(round_tripped.model(), Some("GLM-5.3-Flash"));
    }

    #[test]
    fn sqlite_keeps_conversation_when_resaving() {
        let (_, sqlite, root) = temp_location("sqlite-retry");
        sqlite.save(conversation(100)).unwrap();
        sqlite.save(conversation(120)).unwrap();
        assert_eq!(sqlite.load("conv_1").unwrap().messages.len(), 2);
        remove_tree(&root);
    }

    /// 视频画布的节点登记与消息归属，两个后端都要原样回读：
    /// 画布和节点对话靠这两格重建，丢了节点就是丢了一路创作
    #[test]
    fn video_canvas_nodes_round_trip() {
        let (json, sqlite, root) = temp_location("video-nodes");
        for (label, location) in [("json", json), ("sqlite", sqlite)] {
            let mut conversation = conversation(100);
            conversation.kind = "video".into();
            conversation.video_nodes = vec![VideoNodeRecord {
                id: "node_1".into(),
                label: "节点 1".into(),
                created_at: 5,
            }];
            if let Some(first) = conversation.messages.first_mut() {
                first.node_id = Some("node_1".into());
            }
            location.save(conversation).unwrap();
            let loaded = location.load("conv_1").unwrap();
            assert_eq!(loaded.video_nodes.len(), 1, "{label}: 节点登记要回来");
            assert_eq!(loaded.video_nodes[0].label, "节点 1", "{label}: 节点名要回来");
            assert_eq!(
                loaded.messages[0].node_id.as_deref(),
                Some("node_1"),
                "{label}: 消息的节点标要回来"
            );
        }
        remove_tree(&root);
    }

    #[test]
    fn ipc_payload_keys_match_the_frontend_types() {
        let info = serde_json::to_value(StorageInfo {
            backend: "sqlite".into(),
            json_dir: "dir".into(),
            json_count: 1,
            sqlite_file: "file".into(),
            sqlite_count: 2,
        })
        .unwrap();
        for key in [
            "backend",
            "jsonDir",
            "jsonCount",
            "sqliteFile",
            "sqliteCount",
        ] {
            assert!(info.get(key).is_some(), "StorageInfo 缺少字段 {key}");
        }

        let meta = serde_json::to_value(meta_of(&conversation(7))).unwrap();
        for key in [
            "id",
            "title",
            "projectId",
            "updatedAt",
            "messageCount",
            "preview",
        ] {
            assert!(meta.get(key).is_some(), "ConversationMeta 缺少字段 {key}");
        }
        assert_eq!(meta["messageCount"], 2);
    }

    #[test]
    fn deleting_removes_from_both_backends() {
        let (json, sqlite, root) = temp_location("delete");
        json.save(conversation(100)).unwrap();
        sqlite.save(conversation(100)).unwrap();

        remove_everywhere(&[&json, &sqlite], "conv_1").unwrap();
        assert_eq!(json.count().unwrap(), 0);
        assert_eq!(sqlite.count().unwrap(), 0);

        // 删过的话题不能在切换时被迁移复活；重复删除也不报错
        assert_eq!(migrate(&sqlite, &json).unwrap(), 0);
        assert_eq!(json.count().unwrap(), 0);
        remove_everywhere(&[&json, &sqlite], "conv_1").unwrap();

        remove_tree(&root);
    }

    #[test]
    fn migration_copies_both_ways_and_keeps_source() {
        let (json, sqlite, root) = temp_location("migrate");

        json.save(conversation(100)).unwrap();
        assert_eq!(migrate(&json, &sqlite).unwrap(), 1);
        assert_eq!(sqlite.count().unwrap(), 1);
        assert_eq!(json.count().unwrap(), 1, "迁移不能删源数据");

        // 切过去之后各自又动了：SQLite 侧多一条，JSON 侧把原来那条改得更晚
        sqlite
            .save(Conversation {
                id: "conv_2".into(),
                project_id: String::new(),
                title: "只在 SQLite".into(),
                created_at: 1,
                updated_at: 500,
                pinned: false,
                kind: "chat".to_string(),
                messages: Vec::new(),
                usage: None,
                video_nodes: Vec::new(),
            video_edges: Vec::new(),
            })
            .unwrap();
        json.save(conversation(900)).unwrap();

        assert_eq!(
            migrate(&sqlite, &json).unwrap(),
            1,
            "只补 SQLite 独有的那条，更旧的 conv_1 不能盖掉 JSON 里更新的一版"
        );
        assert_eq!(json.count().unwrap(), 2);
        assert_eq!(json.load("conv_2").unwrap().title, "只在 SQLite");
        assert_eq!(json.load("conv_1").unwrap().updated_at, 900);

        // 反方向：conv_1 在 JSON 侧更晚，所以它要覆盖 SQLite 的那一版
        assert_eq!(migrate(&json, &sqlite).unwrap(), 1);
        assert_eq!(sqlite.load("conv_1").unwrap().updated_at, 900);

        remove_tree(&root);
    }

    // ── history_load 的正文投影（读侧覆盖）────────────────────────────────

    use crate::session::entry::{EntryPayload, Message, NewEntry, SettledAssistant, StopReason, ToolCall};
    use crate::session::SessionLog;

    fn user_row(content: &str) -> EntryPayload {
        EntryPayload::Message {
            message: Message::User {
                content: content.into(),
                images: Vec::new(),
                audios: Vec::new(),
                videos: Vec::new(),
            },
        }
    }

    fn assistant_row(content: &str, calls: Vec<ToolCall>) -> EntryPayload {
        EntryPayload::Message {
            message: Message::Assistant(SettledAssistant {
                content: content.into(),
                tool_calls: calls,
                stop: StopReason::Stop,
                reasoning: None,
                error: None,
                thinking_signature: None,
                reasoning_items_json: None,
            }),
        }
    }

    fn assistant_row_with_reasoning(
        content: &str,
        reasoning: &str,
        calls: Vec<ToolCall>,
    ) -> EntryPayload {
        EntryPayload::Message {
            message: Message::Assistant(SettledAssistant {
                content: content.into(),
                tool_calls: calls,
                stop: StopReason::Stop,
                reasoning: Some(reasoning.into()),
                error: None,
                thinking_signature: None,
                reasoning_items_json: None,
            }),
        }
    }

    fn tool_row(call_id: &str, output: &str) -> EntryPayload {
        EntryPayload::Message {
            message: Message::Tool {
                tool_call_id: call_id.into(),
                content: output.into(),
            },
        }
    }

    /// 读侧覆盖的形状：工具结果并进调用格、记账行（续跑/压缩的 Custom）不出现、
    /// 消息 id 与去重键都是日志条目 id——"界面上那条"和"模型见过那条"同源
    #[test]
    fn the_thread_projection_merges_tool_results_and_skips_bookkeeping() {
        let mut log = SessionLog::new();
        log.append(NewEntry::new(user_row("帮我写个 todo 网页")), 10)
            .unwrap();
        log.append(
            NewEntry::new(assistant_row(
                "写好了。",
                vec![ToolCall {
                    id: "call_1".into(),
                    name: "write_file".into(),
                    arguments: "{}".into(),
                    content_chars: None,
                }],
            )),
            20,
        )
        .unwrap();
        log.append(NewEntry::new(tool_row("call_1", "已写入 todo.html")), 30)
            .unwrap();
        log.append(
            NewEntry::new(EntryPayload::CustomMessage {
                custom_type: "goal_continue".into(),
                content: "（目标模式自动续跑…）".into(),
                display: false,
            }),
            40,
        )
        .unwrap();
        log.append(NewEntry::new(assistant_row("目标已完成。", Vec::new())), 50)
            .unwrap();

        let rows = projected_messages(&log).expect("投影该成功");
        // 五个载荷：user + assistant(带调用) + tool(并入调用格) + 续跑记账行(跳过) + assistant
        assert_eq!(rows.len(), 3, "记账行不进投影、结果并入调用格：{rows:?}");
        assert_eq!(rows[1].tool_calls.len(), 1);
        assert_eq!(rows[1].tool_calls[0].output.as_deref(), Some("已写入 todo.html"));
        assert_eq!(rows[1].tool_calls[0].status, "done");
        assert_eq!(rows[2].content, "目标已完成。");
        // 消息 id 与 entryIds 同源（条目 id）：分支对账与补账去重共用这一格
        assert_eq!(rows[0].entry_ids, vec![rows[0].id.clone()]);
        assert_eq!(rows[0].parent_id, None, "首条就是根");
        assert_eq!(
            rows[1].parent_id.as_deref(),
            Some(rows[0].id.as_str()),
            "父链沿着日志走，界面的链与日志的链是同一条"
        );
    }

    /// 同一轮的连续 assistant 行并进**同一条**消息：一轮"说话→调工具→再说话"
    /// 在日志里是多行 assistant，按行投影的话收尾刷新后同一轮就散成一段一段
    /// （用户看到的就是这个）。续跑行既是轮的边界也是图章——合并出的气泡
    /// 盖上「目标 · 第 N 轮」，重载后的界面与直播气泡说同一句话
    #[test]
    fn same_round_assistant_rows_merge_and_goal_rounds_get_stamped() {
        let call = |id: &str, name: &str| ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
            content_chars: None,
        };
        let mut log = SessionLog::new();
        log.append(NewEntry::new(user_row("把可乐官网写出来")), 10).unwrap();
        // 同一轮：说话 → 调工具 → 再说话 → 再调工具 → 收口
        log.append(
            NewEntry::new(assistant_row("先看一眼", vec![call("call_1", "list_files")])),
            20,
        )
        .unwrap();
        log.append(NewEntry::new(tool_row("call_1", "index.html")), 30).unwrap();
        log.append(
            NewEntry::new(assistant_row("看完再写", vec![call("call_2", "write_file")])),
            40,
        )
        .unwrap();
        log.append(NewEntry::new(tool_row("call_2", "已写入")), 50).unwrap();
        log.append(NewEntry::new(assistant_row("写好了。", Vec::new())), 60).unwrap();
        // 续跑行：新一轮。它自己不显示，但下一行归它那轮
        log.append(
            NewEntry::new(EntryPayload::CustomMessage {
                custom_type: "goal_continue".into(),
                content: "（目标模式自动续跑…）".into(),
                display: false,
            }),
            70,
        )
        .unwrap();
        log.append(NewEntry::new(assistant_row("继续推进。", Vec::new())), 80)
            .unwrap();

        let rows = projected_messages(&log).expect("投影该成功");
        assert_eq!(rows.len(), 3, "同轮三行并一条 + 用户行 + 下一轮：{rows:?}");
        let merged = &rows[1];
        assert_eq!(merged.content, "先看一眼\n\n看完再写\n\n写好了。");
        assert_eq!(merged.tool_calls.len(), 2, "两行的调用格并进来");
        assert_eq!(merged.tool_calls[0].output.as_deref(), Some("index.html"));
        assert_eq!(merged.tool_calls[1].output.as_deref(), Some("已写入"));
        assert_eq!(
            merged.entry_ids.len(),
            3,
            "三行的条目 id 全保留：回溯与补账要的锚一格不少"
        );
        assert_eq!(merged.goal_round, None, "第一条续跑行之前还不是目标轮");
        assert_eq!(rows[2].goal_round, Some(1), "续跑行之后的气泡盖第 1 轮");
    }

    /// 思考章必须是记录内坐标，工具章是合并口径的原样透传：
    /// 自成一格的行章从 0 起（全对话累计游标会在这里越界成空切片——
    /// 「思考内容不显示」的根因）；同轮并格的行章接在前文 + 2 个缝码元之后，
    /// 切片正好取到自己的那段；工具章盖章器（chat.rs）已按合并口径累计，
    /// 投影再加基点就会重复计前文
    #[test]
    fn thinking_and_tool_stamps_are_record_local_not_conversation_cumulative() {
        let call = |id: &str, chars: u32| ToolCall {
            id: id.into(),
            name: "list_files".into(),
            arguments: "{}".into(),
            content_chars: Some(chars),
        };

        // 两次独立的问答：第二条记录的章必须从 0 起，不得带上第一条的坐标
        let mut log = SessionLog::new();
        log.append(NewEntry::new(user_row("第一问")), 10).unwrap();
        log.append(
            NewEntry::new(assistant_row_with_reasoning("答复一", "想想第一问", Vec::new())),
            20,
        )
        .unwrap();
        log.append(NewEntry::new(user_row("第二问")), 30).unwrap();
        log.append(
            NewEntry::new(assistant_row_with_reasoning(
                "先看一眼",
                "想想第二问",
                vec![call("call_2", 4)],
            )),
            40,
        )
        .unwrap();

        let rows = projected_messages(&log).expect("投影该成功");
        let second = &rows[3];
        assert_eq!(second.reasoning.as_deref(), Some("想想第二问"));
        let think = second
            .steps
            .iter()
            .find(|step| step.kind == "thinking")
            .expect("带思考的行要盖思考章");
        assert_eq!(
            (think.from, think.content_chars),
            (Some(0), Some(0)),
            "自成一格的行章从 0 起：累计游标在这里越界，重载后思考全空"
        );
        let tool = second
            .steps
            .iter()
            .find(|step| step.kind == "tool")
            .expect("带调用的行要盖工具章");
        assert_eq!(tool.content_chars, Some(4), "工具章透传盖章器的合并口径");

        // 同一轮的两行并进同一格：章按记录内坐标接缝排布（缝占 2 个码元）。
        // call_2 的 8 = 第一行正文 2 + 缝 2 + 本行正文 4，是盖章器给的原样数字
        let mut log = SessionLog::new();
        log.append(NewEntry::new(user_row("开工")), 10).unwrap();
        log.append(
            NewEntry::new(assistant_row_with_reasoning(
                "先想",
                "想一想",
                vec![call("call_1", 2)],
            )),
            20,
        )
        .unwrap();
        log.append(
            NewEntry::new(assistant_row_with_reasoning(
                "再想再说",
                "再想一想",
                vec![call("call_2", 8)],
            )),
            30,
        )
        .unwrap();

        let rows = projected_messages(&log).expect("投影该成功");
        let merged = &rows[1];
        assert_eq!(merged.content, "先想\n\n再想再说");
        assert_eq!(merged.reasoning.as_deref(), Some("想一想\n\n再想一想"));
        let think_stamps: Vec<(Option<u32>, Option<u32>)> = merged
            .steps
            .iter()
            .filter(|step| step.kind == "thinking")
            .map(|step| (step.from, step.content_chars))
            .collect();
        assert_eq!(
            think_stamps,
            vec![(Some(0), Some(0)), (Some(5), Some(4))],
            "第二行的章 = 前文长度 + 2 个缝码元：reasoning.slice(5) 正好取到「再想一想」"
        );
        let tool_stamps: Vec<Option<u32>> = merged
            .steps
            .iter()
            .filter(|step| step.kind == "tool")
            .map(|step| step.content_chars)
            .collect();
        assert_eq!(
            tool_stamps,
            vec![Some(2), Some(8)],
            "工具章不加基点：盖章器的数字本来就是合并口径，加了就重复计前文"
        );
    }

    /// 压缩边界在投影里同样生效：被压缩的旧消息不回来，摘要站在第一格，
    /// 保留窗与之后的原样跟着——界面显示的对话与模型收到的那份同一条链
    #[test]
    fn the_projection_honors_the_compaction_boundary() {
        let mut log = SessionLog::new();
        log.append(NewEntry::new(user_row("第一问")), 10).unwrap();
        log.append(NewEntry::new(assistant_row("答复一", Vec::new())), 20)
            .unwrap();
        let second = log
            .append(NewEntry::new(user_row("第二问")), 30)
            .unwrap()
            .id
            .clone();
        log.append(NewEntry::new(assistant_row("写好了。", Vec::new())), 40)
            .unwrap();
        log.append(
            NewEntry::new(EntryPayload::Compaction {
                summary: "前情：第一问答复完毕。".into(),
                first_kept_entry_id: second.clone(),
                tokens_before: 999,
                usage: None,
                system_message: None,
            }),
            60,
        )
        .unwrap();
        log.append(NewEntry::new(assistant_row("继续做完。", Vec::new())), 70)
            .unwrap();

        let rows = projected_messages(&log).expect("投影该成功");
        // 生效序列：摘要 + 保留窗(第二问、写好了。) + 之后的(继续做完。)。
        // "写好了。"与"继续做完。"之间隔着压缩条目，但压缩是**轮内事件**不是轮边界——
        // 两行是同一轮的连续 assistant，按同轮归并进同一格
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert!(
            rows[0].content.contains("前情：第一问答复完毕。"),
            "摘要在第一格：{:?}",
            rows[0].content
        );
        assert_eq!(rows[1].content, "第二问");
        assert_eq!(rows[2].content, "写好了。\n\n继续做完。");
        assert_eq!(rows[2].entry_ids.len(), 2, "两行的条目 id 全保留");
        // 被压缩的那两条不回来
        assert!(!rows.iter().any(|row| row.content == "第一问"));
        assert!(!rows.iter().any(|row| row.content == "答复一"));
        // 摘要消息认领的是压缩条目自己的 id
        assert_eq!(rows[1].id, second, "保留窗从 first_kept 起");
    }

    /// 切走的兄弟分支也投影出来（挂在分叉点上）：分支切换器靠它们长出"第几支"，
    /// offPath 的语义与前端现场写台账的时代保持一致
    #[test]
    fn sibling_branches_project_as_off_path_rows() {
        let mut log = SessionLog::new();
        let first = log
            .append(NewEntry::new(user_row("第一问")), 10)
            .unwrap()
            .id
            .clone();
        log.append(NewEntry::new(assistant_row("答复一", Vec::new())), 20)
            .unwrap();
        // 回到第一问另起一支：答复一成了切走的兄弟
        log.navigate(Some(&first)).expect("回溯该成功");
        log.append(NewEntry::new(user_row("换个问法")), 30).unwrap();

        let rows = projected_messages(&log).expect("投影该成功");
        assert_eq!(rows.len(), 3, "两支的消息都在：{rows:?}");
        // 兄弟首条挂分叉点（那条在叶链上），跨集合也连得上
        let sibling = rows
            .iter()
            .find(|row| row.content == "答复一")
            .expect("兄弟支的消息要投影出来");
        assert_eq!(sibling.parent_id.as_deref(), Some(first.as_str()));
        let regrown = rows
            .iter()
            .find(|row| row.content == "换个问法")
            .expect("叶链上的新支要投影出来");
        assert_eq!(regrown.parent_id.as_deref(), Some(first.as_str()));
    }
}
