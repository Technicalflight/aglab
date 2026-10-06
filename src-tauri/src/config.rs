use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    /// 这个项目额外拦哪几行。只能比全局更严：一个项目撤销不了用户在总设置里划的红线，
    /// 合并规则在 [`crate::policy::effective`] 里
    pub permission_overrides: Vec<crate::policy::PermissionOverride>,
    /// 这个项目的文件安全规则（design-security-center.md D2）。判定顺序在**全局表之前**：
    /// 首条命中即停的语义下，项目先行意味着项目能对它自己最清楚的那几个目录说话
    pub file_rules: Vec<crate::file_rules::FileRule>,
    /// 这个项目的命令前缀规则（D4）。黑名单是机器级的，不进项目
    pub command_rules: Vec<crate::command_rules::CommandRule>,
}

/// 一条定时任务。运行结果不在这里：事实只住在 `runs.jsonl` 那本账（`tasks/runs.rs`），
/// `task-state.json` 是从它算出来的缓存，删了可重建，谁也不许当真相读
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    pub prompt: String,
    /// interval = 每隔 everyMinutes 分钟；daily = 每天 atMinute 分（本地时间）
    pub kind: String,
    pub every_minutes: u32,
    pub at_minute: u32,
    /// 每周 atWeekday 星期几的本地时间 atMinute 分（0=周日…6=周六，chrono 的
    /// num_days_from_sunday 口径）。interval/daily 不读这一格
    pub at_weekday: u32,
    /// cron 型的表达式（标准 5 段：分 时 日 月 周，周日=0；也吃带秒的 6/7 段）。
    /// 空串或解析不出的表达式 = 没有触发器：写入口（`check_task_graphs`）会把
    /// 坏表达式拒在门外，手改 config 绕过去的就按"不会跑"放着
    pub cron_expr: String,
    pub enabled: bool,
    pub created_at: i64,
    /// 多步任务的图。空 = 一句 prompt 跑一发（老任务一个字都不用改）；
    /// 非空时 `prompt` 只作为"这条任务在干什么"的说明，实际发出去的是各节点的 prompt
    pub graph: crate::tasks::graph::TaskGraph,
    /// 跑完之后 POST 到哪儿。空 = 不发。签名密钥不在这里——它和 API 密钥一样走系统凭据，
    /// 拿不到密钥就不发（见 `tasks/hook.rs`）
    pub webhook_url: String,
    /// 反方向：本机的另一个进程拿这个令牌敲一下 `POST /hook/<令牌>`，就跑这一条任务。
    /// 空 = 这条任务不可被外部触发（默认，也是唯一的"没配"状态）。
    /// 它不是网络服务：只绑 127.0.0.1，见 `tasks/inbound.rs` 与 design-task-engine.md §15
    pub webhook_token: String,
}

/// 一个外部 MCP 服务器（stdio 或 streamable HTTP 上的 JSON-RPC）。它是"扩展"：带来的工具属于别人。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct McpServer {
    pub id: String,
    pub name: String,
    /// 传输方式："stdio"（默认，老配置没有这一格）| "http"
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    /// 启动时注入的环境变量。cc-switch 的 MCP 库普遍靠 env 传路径和凭据，
    /// 不接这个字段导进来就是"起了但立刻死"
    pub env: BTreeMap<String, String>,
    /// http 型的服务地址（streamable HTTP 的服务商 URL）。stdio 型不读这一格
    pub url: String,
    /// http 型随每个请求带上的请求头（Authorization 这类凭据住这里）
    pub headers: BTreeMap<String, String>,
    /// http 型走 OAuth 登录（MCP 授权规范：发现 → 动态注册 → PKCE）。
    /// 开着时传输侧自动注入 Bearer（用户手写的 Authorization 头优先）
    pub oauth: bool,
    pub enabled: bool,
}

/// 一条被用户确认过的钩子。id 定位"哪个插件的哪个钩子"，hash 是确认当时那份定义的指纹：
/// 脚本内容一改，指纹就对不上，钩子立刻停下来，得重新确认。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TrustedHook {
    pub id: String,
    pub hash: String,
}

/// 一套连接里**某个模型自己**的那一份读数。为什么要有这张表：一张档案可以挂好几个模型
/// （池成员就是"档案 id + 模型名"），而窗口、最大输出、思考档是**模型**的属性不是服务商的
/// ——不分开的话 grok 的 1.1M 会套在 deepseek 的 128K 上，压缩阈值与用量百分比全读假数。
/// 口径与 `cache_ttl_by_model` 一致：这一行是最具体的证据，压过档案级默认
/// 生图设置（design：生图设置面板）。size 为"宽x高"字符串；
/// quality 走 gpt-image 口径（auto/high/medium/low）；count 1..=10
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ImageGenSettings {
    pub size: String,
    pub quality: String,
    pub count: u32,
}

impl Default for ImageGenSettings {
    fn default() -> Self {
        Self { size: "1024x1024".into(), quality: "auto".into(), count: 1 }
    }
}

/// 视频会话画布的生成参数（design：视频参数面板，对照即梦"16:9 · 720P · 5S"）。
/// 随生成请求原样发给上游（New API 视频文档口径：ratio "16:9"、
/// resolution "480p/720p/1080p"、duration 4-15 整数秒），支不支持由上游/模型决定
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VideoGenSettings {
    /// 生成模式："omni" 全能参考（图片作风格/主体参照）｜"frames" 首尾帧
    /// （第一张首帧、第二张尾帧）｜"edit" 视频编辑（素材视频改写）
    pub mode: String,
    pub ratio: String,
    pub resolution: String,
    pub duration: u32,
}

impl Default for VideoGenSettings {
    fn default() -> Self {
        Self {
            mode: "omni".into(),
            ratio: "16:9".into(),
            resolution: "720P".into(),
            duration: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebSearchConfig {
    /// 搜索供应商。空串 = 关：工具整条不声明给模型——声明一个必然失败的工具，
    /// 等于诱着模型去撞一次拒绝。认 "tavily"（api.tavily.com）与 "searxng"
    /// （自建或公共实例的 JSON 接口，免费无需 key）
    pub provider: String,
    /// 搜索接口的 API key。按配置文件现状存明文（插件 MCP 的 headers 同款先例），
    /// 设置页要说明这一点；出口名单与 SSRF 闸在执行侧照常兜着。
    /// SearXNG 不需要 key——实例开了反代鉴权时这里填的是 Bearer 令牌（可选）
    pub api_key: String,
    /// 一次搜索最多回几条结果（1..=10）
    pub max_results: u32,
    /// SearXNG 实例地址（用户自建或选定的公共实例）。provider = "searxng" 时必填。
    /// 这是**用户配置的可信端点**，与模型 baseUrl 同一信任级——模型只能给查询词，
    /// 动不了这台机器到哪个实例（所以私网/回环的自建实例放行，SSRF 闸不适用于它）
    pub searxng_url: String,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            provider: String::new(),
            api_key: String::new(),
            max_results: 8,
            searxng_url: String::new(),
        }
    }
}

impl WebSearchConfig {
    /// 工具声明的总闸。tavily 要 key；searxng 要实例地址
    pub fn enabled(&self) -> bool {
        match self.provider.as_str() {
            "tavily" => !self.api_key.trim().is_empty(),
            "searxng" => !self.searxng_url.trim().is_empty(),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelSpec {
    /// 模型名，原样匹配（与 proxyByModel / cacheTtlByModel 同一口径）
    pub model: String,
    /// 上下文窗口（tokens）。0 = 这一格没填，用档案级默认
    pub context_tokens: u32,
    /// 最大输出。0 = 没填，用档案级默认
    pub max_tokens: u32,
    /// 思考档。None = 没填，用档案级默认；Some("") = 明确不向服务商发送该字段
    pub reasoning_effort: Option<String>,
    /// 这个模型可选的思考档。空 = 全部档位都可选（服务商各家不同，只能人告诉它）
    pub effort_levels: Vec<String>,
    /// 收不收图片。false 是保守默认：没勾过就别往它发图
    pub supports_images: bool,
    /// 收不收视频（视频理解：把视频本体随行发给它看）。false 是保守默认
    #[serde(default)]
    pub supports_video: bool,
    /// 收不收音频（音频理解：把音频本体随行发给它听）。false 是保守默认
    #[serde(default)]
    pub supports_audio: bool,
    /// AI 起的回合（子助理/编排/定时任务，且没点名服务商时）能不能被调度到这一行。
    /// false = 只有界面聊天与明确点名会用它，消费方在 pool.rs 的候选过滤
    #[serde(default = "default_true")]
    pub delegatable: bool,
    /// 能力档（"chat"/"image"/"video"）。空 = 按模型名启发式识别
    /// （seedream/cogview→生图，kling/sora/cogvideo→视频），前端同一套口径
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// 手写而不是 derive：bool 的 derive 默认是 false，一行 default 出来的模型行会静默
/// 变成"不许派工"。其余字段与 derive 逐格相同，只有 delegatable 是有意为之的 true
impl Default for ModelSpec {
    fn default() -> Self {
        Self {
            model: String::new(),
            context_tokens: 0,
            max_tokens: 0,
            reasoning_effort: None,
            effort_levels: Vec::new(),
            supports_images: false,
            supports_video: false,
            supports_audio: false,
            delegatable: true,
            capabilities: Vec::new(),
        }
    }
}

/// serde 的容器级 `default` 对 bool 是 false：老配置里没有"可派工"这一格，
/// 反序列化会把每一行模型静默变成"不许派工"。所以那一格逐字段给 true
fn default_true() -> bool {
    true
}

/// 一条服务商档案：一套可以整体切换的连接配置。
///
/// 密钥凭据对（credential_user.service）必须跟着档案走——切到另一个服务商
/// 却沿用旧凭据目标，从凭据管理器读到的就是错的那把钥匙。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EndpointProfile {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub api_format: String,
    pub reasoning_effort: String,
    pub temperature: f64,
    pub max_tokens: u32,
    pub context_tokens: u32,
    pub auto_compact: bool,
    /// 这套连接里各模型自己的读数（窗口/输出/思考档/附件/可派工）。
    /// 档案级那几格是**默认**，命中这一表的行就盖上去——见 [`ModelSpec`]
    pub models: Vec<ModelSpec>,
    pub prompt_cache_key: Option<bool>,
    pub cache_ttl_seconds: Option<u32>,
    pub cache_ttl_by_model: BTreeMap<String, u32>,
    pub credential_service: String,
    pub credential_user: String,
    /// 代理绑定（值域见 [`crate::proxy`]）："" 继承全局 / "direct" 直连 / "pool" 代理池 / 代理 id。
    /// 它是连接域的一部分：池成员 overlay 与子助理的服务商覆盖都会把这一格抄进顶层
    pub proxy: String,
    /// 服务商内按模型覆盖代理，键是模型名原样匹配（同 [`Self::cache_ttl_by_model`] 的口径）。
    /// 值域同 [`Self::proxy`]，但空键不进表——没有键就是没有覆盖
    pub proxy_by_model: BTreeMap<String, String>,
}

/// 手写而不是 derive：bool 的 derive 默认是 false，一张 default 出来的档案会静默
/// 变成"不许派工"。其余字段与 derive 逐格相同，只有 delegatable 是有意为之的 true
impl Default for EndpointProfile {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            base_url: String::new(),
            model: String::new(),
            api_format: String::new(),
            reasoning_effort: String::new(),
            temperature: 0.0,
            max_tokens: 0,
            context_tokens: 0,
            auto_compact: false,
            models: Vec::new(),
            prompt_cache_key: None,
            cache_ttl_seconds: None,
            cache_ttl_by_model: BTreeMap::new(),
            credential_service: String::new(),
            credential_user: String::new(),
            proxy: String::new(),
            proxy_by_model: BTreeMap::new(),
        }
    }
}

/// 档案覆盖的连接域字段（IPC camelCase 键名）。这份清单只用于守卫测试：
/// 钉住"档案字段集合"不被将来加字段时悄悄漏抄——抄写路径在
/// [`profile_from_config`] / [`apply_profile_to_config`]，两边必须覆盖同一组字段。
/// 生产代码不读它（抄写是手写字段、测试比对清单），所以只在测试构建里存在
#[cfg(test)]
pub const PROFILE_FIELD_KEYS: [&str; 16] = [
    "baseUrl",
    "model",
    "apiFormat",
    "reasoningEffort",
    "temperature",
    "maxTokens",
    "contextTokens",
    "autoCompact",
    "models",
    "promptCacheKey",
    "cacheTtlSeconds",
    "cacheTtlByModel",
    "credentialService",
    "credentialUser",
    "proxy",
    "proxyByModel",
];

/// 顶层配置 → 档案快照。"保存当前配置为新档案"与 cc-switch 导入共用这一条抄写路径，
/// 字段清单偏离 [`PROFILE_FIELD_KEYS`] 时编译器帮不上忙，靠测试钉住
pub(crate) fn profile_from_config(id: String, name: &str, config: &AppConfig) -> EndpointProfile {
    EndpointProfile {
        id,
        name: name.to_string(),
        base_url: config.base_url.clone(),
        model: config.model.clone(),
        api_format: config.api_format.clone(),
        reasoning_effort: config.reasoning_effort.clone(),
        temperature: config.temperature,
        max_tokens: config.max_tokens,
        context_tokens: config.context_tokens,
        auto_compact: config.auto_compact,
        models: config.models.clone(),
        prompt_cache_key: config.prompt_cache_key,
        cache_ttl_seconds: config.cache_ttl_seconds,
        cache_ttl_by_model: config.cache_ttl_by_model.clone(),
        credential_service: config.credential_service.clone(),
        credential_user: config.credential_user.clone(),
        proxy: config.proxy.clone(),
        proxy_by_model: config.proxy_by_model.clone(),
    }
}

/// 档案 → 顶层配置。"切换档案"唯一的落点：顶层字段整体被抄写成档案的样子，
/// 聊天链路读的还是顶层，所以它对整条请求路径零改动
fn apply_profile_to_config(config: &mut AppConfig, profile: &EndpointProfile) {
    apply_profile_connection(config, profile);
    apply_model_spec(config);
    config.active_profile_id = profile.id.clone();
}

/// 档案 → 顶层配置的**连接域**部分：13 个字段一个不落，但不碰 `active_profile_id`。
/// 切换档案 = 这份抄写 + 认档案为当前；模型池按成员路由 = 只有这份抄写——
/// 池是每一发请求的路由决定，不是一次配置切换，不该动"当前档案是哪张"这个持久事实
pub(crate) fn apply_profile_connection(config: &mut AppConfig, profile: &EndpointProfile) {
    config.base_url = profile.base_url.clone();
    config.model = profile.model.clone();
    config.api_format = profile.api_format.clone();
    config.reasoning_effort = profile.reasoning_effort.clone();
    config.temperature = profile.temperature;
    config.max_tokens = profile.max_tokens;
    config.context_tokens = profile.context_tokens;
    config.auto_compact = profile.auto_compact;
    config.models = profile.models.clone();
    config.prompt_cache_key = profile.prompt_cache_key;
    config.cache_ttl_seconds = profile.cache_ttl_seconds;
    normalize_context_window(config);
    config.cache_ttl_by_model = profile.cache_ttl_by_model.clone();
    config.credential_service = profile.credential_service.clone();
    config.credential_user = profile.credential_user.clone();
    // 代理绑定跟着连接走：换一个服务商，这一发该走哪条代理路径也换
    config.proxy = profile.proxy.clone();
    config.proxy_by_model = profile.proxy_by_model.clone();
}

/// 连接域抄完之后，把"这一发实际用的那个模型"的那一行盖上去。为什么单独一步：
/// `config.model` 有三个来源（档案自己、池成员、子助理点名），每一处换了模型，
/// 上下文窗口没填（0）时按这个算预算、压缩与用量。设置面板允许不填；
/// serde 的 default 只补缺字段，补不了显式存成 0 的旧配置，所以 0 必须在读数处归一
pub const DEFAULT_CONTEXT_TOKENS: u32 = 128_000;

/// 0 = 没填窗口：归一成默认值。直读（load）与档案连接应用之后都要过这一遍——
/// 窗口按 0 算的后果不只是面板 0%：预算表 limit=0 让自动压缩每轮开闸，
/// 输出钳制把每轮回复截到 1K（真机踩过）
pub(crate) fn normalize_context_window(config: &mut AppConfig) {
    if config.context_tokens == 0 {
        config.context_tokens = DEFAULT_CONTEXT_TOKENS;
    }
}

/// 窗口/最大输出/思考档就得跟着换——留在档案那一份上，grok 的 1.1M 会套在
/// deepseek 的 128K 头上，压缩阈值与用量百分比一起读假数。
/// 表里没有这一行就一个字段都不动：档案级默认就是它的读数
pub(crate) fn apply_model_spec(config: &mut AppConfig) {
    let Some(spec) = config
        .models
        .iter()
        .find(|spec| spec.model == config.model)
        .cloned()
    else {
        return;
    };
    if spec.context_tokens > 0 {
        config.context_tokens = spec.context_tokens;
    }
    if spec.max_tokens > 0 {
        config.max_tokens = spec.max_tokens;
    }
    if let Some(effort) = spec.reasoning_effort {
        config.reasoning_effort = effort;
    }
}

/// 池成员的定位键：哪张档案 + 哪个模型。档案 id 空串 = "当前连接"——
/// 顶层配置本身就是一张看不见的档案，池允许不建档案直接把现在这套连接加进来
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase", default)]
pub struct PoolKey {
    pub profile_id: String,
    pub model: String,
}

/// 模型池的一个成员。成员 = 一张服务商档案（或空 id 的"当前连接"）上的一个模型；
/// 调度时按档案现值取连接，档案改了地址池子跟着走——成员存引用不存快照，
/// 否则档案页改完 baseUrl 池子还在往老地址发
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PoolMember {
    pub profile_id: String,
    pub model: String,
    /// 权重。轮询走 nginx 平滑加权轮询，随机走加权随机；"最少并发"不看它——
    /// 那条策略的权重本来就是实时的并发数
    pub weight: u32,
    pub enabled: bool,
}

/// 模型池。它住在配置里，但调度读数（轮询位置、并发、失败冷却）只住在
/// [`crate::pool::Hub`] 的内存里：前者是"池里有哪些人"，后者是"这一刻谁闲着"。
/// 两件事混进一份文件，就会出现"重启后冷却时间还没过"这种没人能解释的状态
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelPool {
    /// off = 池子只在设置页躺着，请求照旧走顶层；auto = 调度器每一发挑一个；
    /// pinned = 固定用 `pinned` 那个成员（用户手动选用）；decision = System 1 决策层
    /// （Laya 本地 / Jev 云端，漏斗与敏感性归决策层自己的配置管）挑，
    /// 挑不动退回调度器，绝不因为选不出模型就不干活
    pub mode: String,
    /// auto 的子策略："round_robin"（平滑加权轮询）| "least_used"（当前并发最少）| "random"（加权随机）| "failover"（按列表顺序，第一个没进冷却的成员优先）
    pub strategy: String,
    pub members: Vec<PoolMember>,
    /// pinned 模式生效的那一个成员
    pub pinned: Option<PoolKey>,
    /// 缓存感知首挑（前缀聚类 + 跨 provider 缓存感知路由，2026-10）：新话题第一次
    /// 挑人时，优先落到"最近刚成功过、缓存还热"的那个成员——新话题与刚完成的
    /// 话题共享同一份 system prompt 与工具声明，热成员身上那段前缀直接命中。
    /// 只影响新话题的第一发；话题粘住之后照旧由亲和账接管
    #[serde(default = "default_true")]
    pub cache_aware_pick: bool,
}

impl ModelPool {
    pub fn enabled(&self) -> bool {
        self.mode == "auto" || self.mode == "pinned" || self.mode == "decision"
    }
}

/// 模型路由表的一条规则（design-model-routing.md §4）。生效档位在链上排第三：
/// 点名（子助理/编排/任务）> 模型池 > **路由表** > 设置直连——命中的规则把
/// 「跟着设置走」的那一发改写到指定的服务商档案与模型名上
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelRoute {
    pub id: String,
    /// 匹配的模型名：精确匹配，或 `*` 结尾的前缀匹配；单独一个 `*` 接住一切。
    /// 逐字节比较——模型名大小写敏感，`GPT-4o` 与 `gpt-4o` 是两个名字
    pub pattern: String,
    /// 命中后改去的服务商档案 id。空 = 不改服务商，连接域跟现状走。
    /// 指向已删档案的规则按不命中处理，滑到下一条或直连（界面用徽章标出）
    pub endpoint_profile_id: String,
    /// 命中后改成的模型名。空 = 不改名
    pub model: String,
    pub enabled: bool,
}

/// 一条代理。url 的形状见 [`crate::proxy::parse_proxy_url`]：
/// `http://host:port`、`http://user:pass@host:port` 或 `socks5://…`，协议前缀必带。
/// `weight` 只在轮询与随机两档参与分配，读侧钳进 1..=100（写侧不校验：0 与超界
/// 都归到服务商，钳位只住在那一处）
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ProxyEntry {
    pub id: String,
    pub name: String,
    pub url: String,
    pub enabled: bool,
    pub weight: u32,
}

/// 代理池。绑定值为 `"pool"` 的请求在**启用的**代理之间按策略均衡，
/// 失败冷却与策略的语义与 [`ModelPool`] 同源（见 `crate::proxy`）。
/// 它只管"池里有哪些人、按什么顺序挑"——谁去用池子由三级绑定决定
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProxyPool {
    /// "round_robin"（平滑加权轮询）| "random"（加权随机）| "least_used"（当前并发最少）
    /// | "adaptive"（并发×响应头耗时，没量过的先量一次）。认不出的退回轮询
    pub strategy: String,
    pub proxies: Vec<ProxyEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UiState {
    pub sidebar_collapsed: bool,
    pub panel_collapsed: bool,
    pub panel_tab: String,
    /// 侧边栏当前分区：chats / review / tasks / tools
    pub section: String,
    /// 变更请求页的差异布局："unified" 单栏 / "split" 双栏
    pub diff_layout: String,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            sidebar_collapsed: false,
            // 右侧详情面板默认收起：主界面先给对话，要看的再拉开
            panel_collapsed: true,
            // 详情/工具/上下文/记忆/思考/检查点诸格已删（最关注的几项搬去了输入框上方）：
            // 默认落在还存在的第一格上，老配置里的旧值由前端 isPanelTab 拒掉
            panel_tab: "decision".into(),
            section: "chats".into(),
            diff_layout: "unified".into(),
        }
    }
}

/// 一个自定义子助理的定义（设置页「子助理」）。它只描述"是谁、干什么、
/// 用哪条连接"，权限那两维**不进定义**——由工具白名单推导
/// （`orchestra::profile::derive_capability`），且并进全局权限表时只能收紧不能放松
/// （`AgentProfile::policy_under`）：一份定义不是绕过审批的后门。
///
/// 两条消费路径都从这一份取人：编排派工（决策桥的花名册，`orchestrationAssignable`）
/// 与聊天派单（`spawn_subagent` 工具的可调名单，`chatSpawnable`）
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct SubagentDef {
    /// 稳定名：编排节点的 profile 字符串、spawn_subagent 工具的 `name` 参数都用它。
    /// 与内置角色（reader/worker/verifier/supervisor/planner/integrator）撞名的定义
    /// 在消费点被无视——内置角色赢；这条防御与前端校验是两层，前端拦大概率，这里拦漏网
    pub name: String,
    /// 什么时候该派它。决策层派工花名册的判据、spawn 工具 schema 里的描述，都是这一句
    pub description: String,
    /// 写进那次 run 的第一句：你是谁、这一支只负责什么
    pub system_prompt: String,
    /// 工具白名单（内置注册表里的 id）。空 = 纯推理，一个工具都不给
    pub tools: Vec<String>,
    /// 专属服务商档案 id（`profiles` 里的 id）。空 = 跟着当前连接走
    pub endpoint_profile_id: String,
    /// 专属模型名。空 = 跟着（服务商档案或当前配置的）默认模型走
    pub model: String,
    /// 决策层派工可不可以派它（进 `assignAgent` 的花名册）
    pub orchestration_assignable: bool,
    /// 主聊天模型可不可以按需调用它（进 `spawn_subagent` 的可派名单）
    pub chat_spawnable: bool,
}

impl SubagentDef {
    /// 模型覆盖：空串/空白 = `None`（"继承默认"的配置写法，连接覆盖那一格认 `None` 为跟配置走）
    pub fn model_override(&self) -> Option<String> {
        non_empty(&self.model)
    }

    /// 服务商覆盖：口径同上
    pub fn endpoint_override(&self) -> Option<String> {
        non_empty(&self.endpoint_profile_id)
    }
}

/// 空白不算指定。只去首尾，不重写中间——模型名里多一个空格是服务商的事
fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// 一个**内置**子助理的用户覆盖（设置页「内置子助理」）。定义本身住代码
/// （`spawn::builtin_subagents`），配置只存偏离默认的那几格——升级能加新角色、
/// 改描述，不会被配置里的旧拷贝钉死；「出厂即可预期」由这一条保证
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct SubagentOverride {
    /// 必须命中出厂名册的名字。不命中的整条被安静无视：升级挪走了某个内置角色，
    /// 一条失效的旧覆盖不该变成每次启动弹一个红条
    pub name: String,
    /// 专属服务商档案 id（`profiles` 里的 id）。空 = 继承默认
    pub endpoint_profile_id: String,
    /// 专属模型名。空 = 继承默认
    pub model: String,
    /// 停用：从可派名单与 spawn enum 里摘掉，设置页卡片置灰
    pub disabled: bool,
}

impl SubagentOverride {
    /// 口径与 [`SubagentDef::model_override`] 完全一致：空白 = 没指定 = 继承默认
    pub fn model_override(&self) -> Option<String> {
        non_empty(&self.model)
    }

    pub fn endpoint_override(&self) -> Option<String> {
        non_empty(&self.endpoint_profile_id)
    }
}

/// 未绑定工作目录时的默认基准根：用户主目录（%USERPROFILE%）。
/// Windows-only 的客户端，环境变量取不到的剥离环境才没有
pub(crate) fn home_root() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
}

/// 「以后都允许」落盘的那批放行。键与标签和话题内规则同形，但活过重启：
/// 键 = capability 键集合 + 动作指纹的哈希（`Ruling::remember_key`），
/// 由后端在审批那一刻算好——前端造不出一条它没见过的规则。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AllowRule {
    pub key: String,
    /// 审批框上当初给用户看的那句话：逐条撤销时能认出自己放过的是哪一下
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppConfig {
    /// 「设置 → 命令 Shell」：AI 跑命令（run_command 工具）用的 shell。
    /// 空 = 默认 cmd；powershell / pwsh / git-bash 跟随设置页选择
    pub command_shell: String,
    /// OpenAI 兼容根地址，例如 https://api.deepseek.com/v1
    pub base_url: String,
    pub model: String,
    /// 线协议。"chat"=/chat/completions，"responses"=/responses。
    /// 中转站常常只开其中一条，所以这是配置而不是猜测；导入 cc-switch 时会照搬它的 apiFormat
    pub api_format: String,
    /// 思考程度。空串表示"默认"，即不向服务商发送该字段
    pub reasoning_effort: String,
    /// 用 f64：f32 会把 0.7 序列化成 0.699999988079071 发给服务商
    pub temperature: f64,
    pub max_tokens: u32,
    /// 模型上下文窗口（tokens）。只用于界面估算"上下文用量"的百分比，
    /// 不参与请求；各模型差异大，所以是配置而不是猜测
    pub context_tokens: u32,
    /// 项目约定文件（AGENTS.md / CLAUDE.md）最多注入多少字符。0 = 不设上限。
    /// 它是**夹**：那份文件是已经存在的事实，超了就只留前面那段（§14）
    pub project_rules_max_chars: usize,
    /// 一条工具结果最多留多少字符。0 = 不设上限。头 3/4 与尾 1/8 由它派生——
    /// 拆成三个旋钮就会出现 `head > max` 这种自相矛盾的配置
    pub tool_result_max_chars: usize,
    /// 本轮检索出来的记忆段超过这么多字符就**整段不发**。0 = 不设上限（默认）。
    /// 它是**让**不是夹：记忆是按本轮提法挑出来的几条记录，截一半等于伪造一条不存在的记忆（§14.1）
    pub memory_section_max_chars: usize,
    /// 自动压缩：发送前估算上下文超过窗口 90% 时，把更早的对话压成摘要再继续。
    /// 压缩要额外花一次请求，默认开——爆窗口的失败比一次摘要请求贵得多
    pub auto_compact: bool,
    /// "当前连接"这张看不见的档案里，各模型自己的读数（窗口/输出/思考档/附件/可派工）。
    /// 档案上的同名表会整体覆盖它——顶层只是"没建档案时的那份连接"
    pub models: Vec<ModelSpec>,
    /// 缓存保温：赶在服务商缓存过期前用一次 max_tokens=1 的重放把它续上。默认关——
    /// 它是「先花一笔确定的小钱、赌一笔不确定的大钱」，用户没点头之前不动他的服务商。
    /// 真要发还要过三道闸：服务商声明过存活期、价表算得出至少省 $0.05、日志末端没动
    pub cache_warming: bool,
    /// 重复循环护栏：模型解码退化时会在短周期上无限复读，流式增量过检测器，
    /// 模式一成型就拉闸断流，省下循环后半段的 token。默认开——误报有三道闸
    /// （单位须含文字、代码块内挂起、触发线放宽），正常回答碰不到它
    pub repetition_guard: bool,
    /// 自动检查更新：每 24 小时联网查一次新版本，发现后弹窗提醒。默认开——
    /// 用户也可以只用手动的「检查更新」，这一格只关自动的那部分
    pub auto_update_check: bool,
    /// 上次自动检查的时刻（epoch 毫秒）。0 = 从没查过，启动即查一次
    pub last_update_check_at: i64,
    /// 主题模式："dark" | "light" | "system"。跟随系统时由前端监听系统外观切换
    pub theme_mode: String,
    /// 强调色（--brand 的值，#RRGGBB）。空串 = 用主题默认的品牌紫
    pub accent_color: String,
    /// 是否把模型的思考过程（reasoning）显示在消息里。关掉时前端不渲染
    pub show_reasoning: bool,
    /// 服务商回 429（限流）时无限重试：指数退避（封顶 60 秒），直到成功或用户按停止。
    /// 默认关——"无限"意味着这一发可能永远不结束，用户没点头之前不这么做。
    /// 开着时每次重试都会在对话流里说一句，等待期间随时可停
    pub unlimited_retry_429: bool,
    /// 重启后自动继续挂着的目标。**默认关**：那一格管的是"要不要继续花钱"，
    /// 归人决定，不归一次程序重启替他重按播放键。
    /// 关着时启动会把 active 的目标落成 `paused` 并说一声，人在目标带上按「继续」才往下跑
    pub goal_resume_on_launch: bool,
    /// 单回合工具调用轮数上限。0 = 不设上限；非 0 到顶就停，防一个任务无限循环烧 token
    pub max_tool_rounds: u32,
    /// **全局**并发上限：这台机器上同时跑几路模型请求——编排计划的节点与定时任务的
    /// 每一发都算，两边共用同一个池子。每份计划自己的那个上限管不到这件事——三份各开
    /// 4 路就是 12 路并发请求，每一路都是一份完整上下文的钱。写 0 = 不设上限
    /// （要表达"一格都不许跑"，用的是取消，不是这个数）
    pub total_parallel: usize,
    /// 缓存身份（prompt_cache_key）能力的显式覆盖。
    /// null = 交给内置能力表判断（表里没有的服务商一律当作不支持）；写 true 是用户给我看的证据
    pub prompt_cache_key: Option<bool>,
    /// 缓存存活期（秒）的覆盖。0/未知 = 保温不跑：不知道期限就不该花真钱赌命中
    pub cache_ttl_seconds: Option<u32>,
    /// 逐模型的缓存存活期（秒）。这是最具体的证据，压过全局覆盖与内置表；
    /// 目前设置页不提供编辑入口，直接改 config.json（键是模型名原样匹配）
    pub cache_ttl_by_model: BTreeMap<String, u32>,
    /// 各能力档记住的模型（design：能力会话）。键是 "chat"/"image"/"video"——
    /// 切换会话档时把对应模型换上来；在某档里选模型时也记回这一格
    pub kind_models: BTreeMap<String, String>,
    /// 生图会话的生成参数（design：生图设置面板）。随生成请求原样发给上游，
    /// 支不支持由上游/模型决定，不支持时通常被忽略
    pub image_gen: ImageGenSettings,
    /// 视频会话画布的生成参数（比例/分辨率/时长）。随生成请求原样发给上游
    pub video_gen: VideoGenSettings,
    /// 已保存的服务商档案（设置页的卡片）。切换 = 档案字段抄进顶层；
    /// 编辑表单只动顶层、**不回写档案**——档案是保存那一刻的快照，
    /// 想更新某张卡片就在它的编辑弹窗里改，避免"临时改一下"悄悄污染存档
    pub profiles: Vec<EndpointProfile>,
    /// 当前生效的档案 id。空 = 尚未关联任何档案，顶层字段独立生效
    pub active_profile_id: String,
    /// 密钥不放界面也不放配置文件，只从 Windows 凭据管理器读
    pub credential_service: String,
    pub credential_user: String,
    pub projects: Vec<Project>,
    pub active_project_id: String,
    /// ask=每次改动都问 / auto=只在高风险时问 / full=不再询问
    pub permission: String,
    /// 全局覆盖项。每一行都只能比那一档更严（`Policy::resolve` 管这件事），
    /// 项目那份再在它上面叠一层同样的规则
    pub permission_overrides: Vec<crate::policy::PermissionOverride>,
    /// 文件安全规则表（design-security-center.md D2）：路径前缀 × 读/写/删三档动作。
    /// 判定顺序是项目表在前、全局表在后，首条命中即停；未命中的路径落回现行权限档
    pub file_rules: Vec<crate::file_rules::FileRule>,
    /// 命令黑名单（design-security-center.md D4）：只收程序名，机器级——
    /// wsl.exe 在哪个项目里都不该由模型跑，没有项目粒度
    pub command_blocklist: Vec<String>,
    /// 命令前缀规则（全局那份）。项目表在前、全局表在后，段间取最严
    pub command_rules: Vec<crate::command_rules::CommandRule>,
    /// 网络安全规则（design-security-center.md D5）：域后缀 → 动作，机器级。
    /// 未命中落回现行判定；规则之外，最外圈的出口名单与私网拒绝照旧在
    pub network_rules: Vec<crate::egress::NetworkRule>,
    /// HTTP 明文分档：远程目标（非回环）。默认问——明文凭据上线的代价问一次不算贵
    pub net_http_remote: crate::file_rules::RuleAction,
    /// HTTP 明文分档：回环目标。默认放——本机端口调用天天有，问就是路障
    pub net_http_local: crate::file_rules::RuleAction,
    /// 删除保护（design-security-center.md D1）：`delete_file` 默认移入回收站；
    /// 关掉 = 按系统删除。启动与设置页各落一次到进程级开关，执行侧不回读配置文件
    pub delete_to_trash: bool,
    /// 批量删除审批阈值：一次 `delete_file` 的路径数达到它就强制问人，
    /// 档位（含 full）与覆盖项都压不住——这是用户显式配的闸。0 = 不设阈值
    pub delete_approval_threshold: usize,
    /// 敏感保护（design-security-center.md D6）：工具结果进话题流之前就地打码。
    /// 只管这一道——记忆门禁、MCP 参数拦截、审计脱敏是各自独立的能力
    pub secret_scan_enabled: bool,
    /// 被关闭的敏感检测规则（`secrets::RULES` 的 id）。关闭对检测与打码同时生效
    pub disabled_secret_rules: Vec<String>,
/// 用户自建的敏感检测规则（design-security-center.md D6）：名称 + 正则。
/// 自建规则不吃提示词闸，按字面生效；id 由前端生成（custom- 前缀）
pub custom_secret_rules: Vec<crate::secrets::CustomSecretRule>,
    /// 对内置检测规则正则的改写：只许改正则。改过的规则不再吃提示词闸
    pub secret_rule_pattern_edits: Vec<crate::secrets::SecretRulePatternEdit>,
    /// 自定义 MCP 总开关（design-security-center.md D7）：一键停掉**用户自配**的全部
    /// MCP 服务器（config.mcp_servers）。出厂扩展与插件自带的不受它管——
    /// 它们是随应用语义走的，不是用户外接的
    pub user_mcp_enabled: bool,
    /// 自动备份（design-security-center.md D3）：写/删之前存可恢复副本。
    /// 备份是尽力而为：失败不挡原操作，只落审计
    pub backup_enabled: bool,
    /// 备份总量上限（MB），按最老先删的 LRU 清。0 = 不设上限
    pub backup_total_mb: u32,
    /// 网络出口的目标域名单（design-security-permission.md §16）。**空 = 不收紧**，
    /// 也就是这一格落地前的行为。条目按域后缀匹配主机名（`example.com` 覆盖 `api.example.com`，
    /// 不覆盖 `notexample.com`），可以粘整条 URL。它管的是目标主机已知的那三处出口
    /// （模型请求、模型清单、outbound webhook）；MCP 在这里是 stdio 子进程，
    /// 它要连哪儿是那个进程自己的事，这份名单管不到它
    pub net_egress_allow: Vec<String>,
    /// inbound webhook：本机监听那一格。**默认关**——这是一台桌面应用里唯一一处
    /// "别人能引起一次花钱的动作"的入口。绑的地址永远是 127.0.0.1，不在配置里（§15）
    pub webhook_in_enabled: bool,
    /// 只绑回环的那个端口。0 不在允许值里：让系统选一个随机端口等于让用户自己去找
    pub webhook_in_port: u16,
    /// 已在风险弹窗里逐条看过并确认过「完全访问」，之后切入该档不再弹。
    /// 撤销入口只放在设置页：留一条回得去的路，比让沉默变成默认更重要
    pub full_access_acknowledged: bool,
    /// 对话记录的存储介质："json" | "sqlite"。
    /// 这个开关本身永远写在 config.json 里——它不能存在自己所选择的存储中。
    pub conversation_store: String,
    /// 审计日志的保留天数。过期的整天分片**整片搬进** `audit/archive/`，不删除：
    /// 保留策略是归档，删除只能由用户自己点"清空"
    pub audit_keep_days: i64,
    /// 被关闭的工具 id；空=全开。关掉的工具不会声明给模型，硬调回来也不执行
    pub disabled_tools: Vec<String>,
    /// 被关闭的插件目录名。插件是容器：关掉它，它带来的技能和 MCP 服务一起消失
    pub disabled_plugins: Vec<String>,
    /// 被关闭的内置扩展 id（`crate::builtins` 的出厂名册）。关掉一条，
    /// 它带的全部技能同时从清单、取用与插件页消失——定义在代码里，这里只存偏离
    pub disabled_builtins: Vec<String>,
    /// 被关闭的技能（"来源/目录名"）。技能只是提示词，不给执行权
    pub disabled_skills: Vec<String>,
    /// 用户逐条确认过内容的钩子（id + 确认当时那份定义的指纹）。
    /// 钩子是插件带进来的可执行脚本，没确认过的坚决不跑；改一个字节就作废重来
    pub trusted_hooks: Vec<TrustedHook>,
    /// 「以后都允许」的持久放行（审批那一刻用户点过头的指纹规则）。启动时回灌审批中心，
    /// 判定面与话题内规则完全同一条；撤销时从这里一起摘掉
    pub allow_rules: Vec<AllowRule>,
    /// 单独关掉的钩子 id。确认过但不想让它跑时用这个，不必去掉信任
    pub disabled_hooks: Vec<String>,
    pub mcp_servers: Vec<McpServer>,
    /// 被关闭的扩展工具暴露名，例如 mcp__mcp-1__ping
    pub disabled_mcp_tools: Vec<String>,
    /// 联网搜索（web_search 工具）。没配就不把工具声明给模型
    pub web_search: WebSearchConfig,
    pub tasks: Vec<ScheduledTask>,
    /// 开机自启（用户自选，默认关）。这里存的是意图，OS 里那条注册在启动时按它对一遍
    /// （`autostart.rs`）：界面上说"开"就必须真的注册着，两件事不许各说各的
    pub autostart: bool,
    /// 聊天正文字号档位："small" | "medium" | "large" | "xlarge"。只管读消息那段文字，
    /// 标题与代码由 CSS 按倍数派生——拆成逐元素的旋钮就再也没人能一眼调对了
    pub chat_font_size: String,
    /// 界面缩放（webview 原生 zoom）。1.0 = 100%。改动即时生效走 `window_zoom`，
    /// 启动时按它恢复；夹在 0.5–2.0 之间，再小看不清、再大乱版
    pub ui_zoom: f64,
    /// 减少动效：压掉界面过渡与动画，只留最终状态。可读性偏好，不影响任何功能
    pub reduce_motion: bool,
    /// 开发者模式：把关「关于」里的开发者工具入口（打开 WebView2 控制台）。
    /// 只管入口显隐，不是功能开关——调试设施不该出现在默认视线里
    pub dev_mode: bool,
    /// 点关闭时的行为："ask"=每次问 / "tray"=直接收进托盘 / "quit"=直接退出。
    /// 判定在 `run()` 的 `CloseRequested` 里；"tray" 只在托盘真建起来时才硬藏
    pub close_action: String,
    /// 窗口置顶。改动即时套、启动时按它恢复
    pub always_on_top: bool,
    /// 系统通知（Windows toast）：审批等待、定时任务收尾、目标停下时弹。
    /// 默认开——无人值守的场景里，不弹通知等于让人守着窗口等。窗口在前台时不打扰
    pub notifications: bool,
    /// 全局快捷键（Ctrl+Shift+G 唤起窗口）。默认关：全局热键会占用系统按键，
    /// 用户没点头之前不抢。开关在 设置 → 应用 → 窗口，注册失败不落盘
    pub global_shortcut_enabled: bool,
    /// 命令沙箱（低完整性）：run_command 的子进程压到 Low IL——写只限项目根与
    /// 专用临时目录，读与网络不受限；收容（Job）仍然无条件兜底。默认关：
    /// 完整性标注会改项目里文件的安全描述符，用户没点头之前不动
    pub sandbox_enabled: bool,
    /// 自动审查：审批升级请求交给审查模型替人拍板（对齐 deepseek 的 Auto review）。
    /// 审查模型看工具名 + 入参 + 风险档位，回 APPROVE 或 DENY: 理由。
    /// **不改沙箱边界**——只处理升级请求，边界内的动作照旧自主执行。默认关。
    pub auto_review: bool,
    /// 自动审查的专属服务商档案 id（`profiles` 里的 id）。空 = 跟着当前连接走。
    /// 审查那一发不必烧主模型：点名一套便宜快的连接，审查才养得起常开
    pub auto_review_profile_id: String,
    /// 自动审查的专属模型名。空 = 跟着（审查档案或当前的）默认模型走。
    /// 换了模型就读那一行的读数（窗口/输出/思考档），与子助理点名同一条路
    pub auto_review_model: String,
    /// 沙箱的额外可写根（writable_roots，对齐 Codex 的同名概念）：
    /// 每个目录在 prepare 时照项目根同一套打法打 Low 标签
    pub sandbox_writable_roots: Vec<String>,
    /// SSH 主机花名册（对齐 deepseek 的 SSH backend 家族），每行
    /// `名字=user@host:端口`（端口可省，默认 22）。凭据不在这里——走系统 ssh
    /// 自己的钥匙链（默认密钥/agent），口令认证不做（BatchMode 挂不起交互）。
    /// `ssh_run` 是执行体，这串是它的花名册；执行本身照旧过审批闸
    pub ssh_hosts: Vec<String>,
    /// LSP 服务器逐扩展覆盖，每行 `ext=启动命令`（如 `rs=D:\tools\rust-analyzer.exe`）。
    /// 不认识的扩展没有默认服务器，全靠这里补
    pub lsp_servers: Vec<String>,
    /// 模型池。路由决定在 `crate::pool`：每一发请求按池的 mode/strategy 挑成员
    pub model_pool: ModelPool,
    /// 模型路由表（`crate::route`）：只在「池子没接管、也没点名」的那一档生效，
    /// 按模型名把这一发改写到规则指定的服务商档案与模型名上。空表 = 不路由
    pub model_routes: Vec<ModelRoute>,
    /// 内置浏览器控制总开关（`crate::browser`）。关 = browser 工具不声明、
    /// 被硬调也报诚实错误。默认关：它让模型能驱动一个真浏览器
    pub browser_control_enabled: bool,
    /// 内置浏览器忽略 HTTPS 证书校验。只进启动参数——改完要重启内置浏览器
    /// （关掉它的窗口或重启 aglab）才生效
    pub browser_ignore_cert_errors: bool,
    /// 代理池本体与全局默认绑定。绑定的三级解析在 `crate::proxy`：
    /// 服务商内按模型覆盖（下面的 `proxy_by_model`）→ 服务商（`proxy`）→ 这里（`proxy_default`）
    pub proxy_pool: ProxyPool,
    /// 全局默认代理绑定："" 直连 / "direct" 直连 / "pool" 代理池 / 代理 id
    pub proxy_default: String,
    /// 不走代理的主机后缀（`example.com` 覆盖 `api.example.com`）。
    /// 本机回环恒在名单内，不依赖这里
    pub proxy_bypass: Vec<String>,
    /// 当前连接的代理绑定（连接域字段，随档案切换/池 overlay 整体抄写，口径同 `model`）
    pub proxy: String,
    /// 当前连接的按模型代理覆盖（连接域字段，口径同 `cache_ttl_by_model`）
    pub proxy_by_model: BTreeMap<String, String>,
    /// 自定义子助理目录（设置页「子助理」）。编排派工与聊天 spawn 工具都从这里
    /// 取人；空表 = 两条通道都保持原样——决策层只认内置三角色，聊天还有出厂名册可派
    pub subagents: Vec<SubagentDef>,
    /// 内置子助理的覆盖项（键 = 出厂名）。定义住代码，这里只存偏离：
    /// 服务商/模型留空 = 继承默认；未知名字的条目在消费点被安静无视
    pub subagent_overrides: Vec<SubagentOverride>,
    /// 资料库语义检索的 embedding 档。base_url/model 留空 = 未启用（纯关键词检索）。
    /// 密钥沿用当前连接的 API 密钥（中转站同一把钥匙开两个端点是常态），不另设一格
    pub embedding: EmbeddingConfig,
    /// Umi-OCR 引擎档。资料库导入 PDF/图片时的文字提取走它
    pub ocr: OcrConfig,
    pub ui: UiState,
}

/// 资料库语义检索的 embedding 档。OpenAI 兼容 /embeddings 端点，
/// 中转站通常同样代理这个路径——密钥直接沿用主密钥
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EmbeddingConfig {
    /// 端点基址（如 https://relay.example.com/v1）。空 = 未启用语义检索
    pub base_url: String,
    /// embedding 模型名（如 text-embedding-3-small / bge-m3）
    pub model: String,
    /// 向量维度。0 = 首次嵌入时从响应自动探测并记下——
    /// 之后必须一致，换了模型要全量重建
    pub dimensions: u32,
    /// rerank 精排模型名（如 bge-reranker-v2-m3）。空 = 不做精排。
    /// 与 embedding 同一个端点同一把钥匙——中转站同站代理 /rerank 是常态
    pub rerank_model: String,
}

/// Umi-OCR 引擎档（资料库导入 PDF/图片用）。base_url 留空 = 用默认本机地址。
/// Umi-OCR 的 HTTP 服务默认开在 127.0.0.1:1224，仅本地环回
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OcrConfig {
    pub base_url: String,
}

/// embedding 专用凭据槽（keyring 条目 = default.aglab/embedding）。
/// 与主密钥、各档案槽完全隔离——换档案、换服务商钥匙都不牵连语义检索
pub const EMBEDDING_CREDENTIAL_SERVICE: &str = "aglab/embedding";
pub const EMBEDDING_CREDENTIAL_USER: &str = "default";

/// embedding 的钥匙：专用槽优先；没设专用密钥就沿用当前档案主密钥
/// （中转站一把钥匙开 chat 与 embeddings 两个端点是常态）
pub(crate) fn embedding_key(config: &AppConfig) -> Result<String, String> {
    match api_key_for(EMBEDDING_CREDENTIAL_SERVICE, EMBEDDING_CREDENTIAL_USER) {
        Ok(key) => Ok(key),
        Err(_) => api_key(config),
    }
}

impl AppConfig {
    pub fn active_project(&self) -> Option<&Project> {
        self.projects
            .iter()
            .find(|project| project.id == self.active_project_id)
    }

    /// 按 id 找项目。active_project 的按 id 版本：话题台账里记的是 id，
    /// 回合开始时把话题的归属读回来要靠它（active_project_id 是应用级默认，
    /// 话题自己的归属是另一回事——两者不该混用）
    pub fn project_by_id(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|project| project.id == id)
    }

    /// 本回合的基准根：绑定了工作目录就是它；**没绑也落到用户主目录**——
    /// 文件/命令工具不再以"绑定工作目录"为门槛（2026-10-03 设计）：相对路径
    /// 相对主目录解析，权限表照常把关（主目录内的写入按 ProjectRoot 档判定）。
    /// 连主目录都拿不到的剥离环境才返回 None（文件工具那时照旧不声明）
    pub fn effective_root(&self) -> Option<PathBuf> {
        self.active_project()
            .map(|project| PathBuf::from(&project.path))
            .or_else(home_root)
    }

    /// 这一刻生效的那张权限表：全局档 + 全局覆盖项 + 这个项目额外收紧的几行。
    ///
    /// 它是**唯一**的入口。原来三处各自 `Policy::new(mode_from_legacy(...))`，
    /// 那样"按项目收紧"只能变成"再抄第四份"，而抄的那份早晚和这三处不一样
    pub fn policy(&self, project: Option<&Project>) -> crate::policy::Policy {
        crate::policy::effective(
            crate::policy::mode_from_legacy(&self.permission),
            &self.permission_overrides,
            project
                .map(|item| item.permission_overrides.as_slice())
                .unwrap_or_default(),
            self.delete_approval_threshold,
        )
        .with_file_rules(match project {
            // 首条命中即停：项目表排前、全局表排后。项目对自己地盘上的目录先说话
            Some(item) => item
                .file_rules
                .iter()
                .chain(self.file_rules.iter())
                .cloned()
                .collect(),
            None => self.file_rules.clone(),
        })
        .with_command_rules(
            self.command_blocklist.clone(),
            match project {
                // 同一个口径：项目的前缀规则先说话，全局的跟在后面
                Some(item) => item
                    .command_rules
                    .iter()
                    .chain(self.command_rules.iter())
                    .cloned()
                    .collect(),
                None => self.command_rules.clone(),
            },
        )
        .with_net_rules(
            self.network_rules.clone(),
            self.net_http_remote,
            self.net_http_local,
        )
    }

    /// 当前活动项目的那一张。没绑项目 = 只有全局那两层
    pub fn active_policy(&self) -> crate::policy::Policy {
        self.policy(self.active_project())
    }

    /// 存盘前问一句：覆盖项的键认不认得。
    ///
    /// 写错的键永远不会命中任何一次判定，而界面上那一行显示着"我已经拦了 git"——
    /// 那是最坏的一种坏：静默。所以它要么当场被拒，要么就别说它生效了
    pub fn check_overrides(&self) -> Result<(), String> {
        let mut bad: Vec<String> = Vec::new();
        for item in &self.permission_overrides {
            if !crate::policy::is_known_key(&item.key) {
                bad.push(format!("全局 · {}", item.key));
            }
        }
        for project in &self.projects {
            for item in &project.permission_overrides {
                if !crate::policy::is_known_key(&item.key) {
                    bad.push(format!("项目「{}」 · {}", project.name, item.key));
                }
            }
            // 文件规则的同一条纪律（design-security-center.md D2）：展开不成绝对路径的
            // 条目永远命不中任何一次判定，界面上却显示着"我已经拦了"——存盘时就拒
            if let Err(problem) = crate::file_rules::validate(&project.file_rules) {
                bad.push(format!("项目「{}」文件规则：{}", project.name, problem));
            }
        }
        if let Err(problem) = crate::file_rules::validate(&self.file_rules) {
            bad.push(format!("全局文件规则：{problem}"));
        }
        for project in &self.projects {
            if let Err(problem) = crate::command_rules::validate(&self.command_blocklist, &project.command_rules) {
                bad.push(format!("项目「{}」命令规则：{}", project.name, problem));
            }
        }
        if let Err(problem) = crate::command_rules::validate(&self.command_blocklist, &self.command_rules) {
            bad.push(format!("全局命令规则：{problem}"));
        }
        if let Err(problem) = crate::egress::validate_rules(&self.network_rules) {
            bad.push(format!("全局网络规则：{problem}"));
        }
        if let Err(problem) = crate::secrets::validate_custom_rules(&self.custom_secret_rules) {
            bad.push(format!("自定义检测规则：{problem}"));
        }
        if let Err(problem) = crate::secrets::validate_pattern_edits(&self.secret_rule_pattern_edits) {
            bad.push(format!("检测规则改写：{problem}"));
        }
        match bad.is_empty() {
            true => Ok(()),
            false => Err(format!(
                "这些配置存不进去——它们要么命中不了任何一次判定，要么根本展开不出路径：{}",
                bad.join("；")
            )),
        }
    }

    /// 存盘前问一句：这几条任务里的图发得出去吗、cron 表达式认得出来吗。判据住在
    /// [`crate::tasks::graph::TaskGraph::validate`] 与 `trigger::cron_schedule`，
    /// 这里只把它们接到写入口上。
    ///
    /// 坏图不拦的代价是延迟发生的：任务在没有人看的时候起一发，第一格就停住，
    /// 而界面上只剩一条失败的运行记录。坏表达式同理：任务开着，却永远不会有下一次
    pub fn check_task_graphs(&self) -> Result<(), String> {
        let mut bad: Vec<String> = Vec::new();
        for task in &self.tasks {
            if let Err(error) = task.graph.validate() {
                let named = task.name.trim();
                let named = if named.is_empty() { task.id.trim() } else { named };
                bad.push(format!("任务「{named}」{error}"));
            }
            if task.kind.split('|').next() == Some("cron") {
                let named = task.name.trim();
                let named = if named.is_empty() { task.id.trim() } else { named };
                if crate::tasks::trigger::cron_schedule(&task.cron_expr).is_none() {
                    bad.push(format!(
                        "任务「{named}」的 cron 表达式认不出来（{}）",
                        if task.cron_expr.trim().is_empty() { "空" } else { task.cron_expr.trim() }
                    ));
                }
            }
        }
        match bad.is_empty() {
            true => Ok(()),
            false => Err(format!(
                "这些任务发不出去，在任务页「编辑」里改对再存：{}",
                bad.join("；")
            )),
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: String::new(),
            api_format: "chat".into(),
            reasoning_effort: "medium".into(),
            temperature: 0.7,
            max_tokens: 2048,
            context_tokens: 128_000,
            // 三个默认值复现的是加旋钮之前的行为：8 000 是原来那个私有常量，
            // 16 000 派生出原来的 12 000 / 2 000，记忆段那一格默认不管
            project_rules_max_chars: 8_000,
            tool_result_max_chars: 16_000,
            memory_section_max_chars: 0,
            net_egress_allow: Vec::new(),
            webhook_in_enabled: false,
            webhook_in_port: 8786,
            auto_compact: true,
            models: Vec::new(),
            cache_warming: false,
            repetition_guard: true,
            auto_update_check: true,
            last_update_check_at: 0,
            theme_mode: "dark".into(),
            accent_color: String::new(),
            show_reasoning: true,
            unlimited_retry_429: false,
            goal_resume_on_launch: false,
            max_tool_rounds: 6,
            total_parallel: crate::orchestra::orchestrator::DEFAULT_TOTAL_PARALLEL,
            prompt_cache_key: None,
            cache_ttl_seconds: None,
            cache_ttl_by_model: BTreeMap::new(),
            kind_models: BTreeMap::new(),
            image_gen: ImageGenSettings::default(),
            video_gen: VideoGenSettings::default(),
            profiles: Vec::new(),
            active_profile_id: String::new(),
            command_shell: String::new(),
            credential_service: "aglab/api-key".into(),
            credential_user: "default".into(),
            subagents: Vec::new(),
            subagent_overrides: Vec::new(),
            embedding: EmbeddingConfig::default(),
            ocr: OcrConfig::default(),
            proxy_pool: ProxyPool::default(),
            proxy_default: String::new(),
            proxy_bypass: Vec::new(),
            proxy: String::new(),
            proxy_by_model: BTreeMap::new(),
            projects: Vec::new(),
            active_project_id: String::new(),
            permission: "ask".into(),
            permission_overrides: Vec::new(),
            file_rules: Vec::new(),
            command_blocklist: Vec::new(),
            command_rules: Vec::new(),
            network_rules: Vec::new(),
            net_http_remote: crate::file_rules::RuleAction::Ask,
            net_http_local: crate::file_rules::RuleAction::Allow,
            delete_to_trash: true,
            delete_approval_threshold: 50,
            secret_scan_enabled: true,
            disabled_secret_rules: Vec::new(),
            custom_secret_rules: Vec::new(),
            secret_rule_pattern_edits: Vec::new(),
            user_mcp_enabled: true,
            backup_enabled: true,
            backup_total_mb: 3000,
            full_access_acknowledged: false,
            conversation_store: "json".into(),
            audit_keep_days: crate::audit::RETENTION_DAYS,
            disabled_tools: Vec::new(),
            disabled_plugins: Vec::new(),
            disabled_builtins: Vec::new(),
            disabled_skills: Vec::new(),
            trusted_hooks: Vec::new(),
            disabled_hooks: Vec::new(),
            mcp_servers: Vec::new(),
            disabled_mcp_tools: Vec::new(),
            web_search: WebSearchConfig::default(),
            tasks: Vec::new(),
            autostart: false,
            chat_font_size: "medium".into(),
            ui_zoom: 1.0,
            reduce_motion: false,
            dev_mode: false,
            close_action: "ask".into(),
            always_on_top: false,
            notifications: true,
            global_shortcut_enabled: false,
            sandbox_enabled: false,
            auto_review: false,
            auto_review_profile_id: String::new(),
            auto_review_model: String::new(),
            sandbox_writable_roots: Vec::new(),
            ssh_hosts: Vec::new(),
            lsp_servers: Vec::new(),
            allow_rules: Vec::new(),
            model_pool: ModelPool::default(),
            model_routes: Vec::new(),
            browser_control_enabled: false,
            browser_ignore_cert_errors: false,
            ui: UiState::default(),
        }
    }
}

impl AppConfig {
    pub fn chat_endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    pub fn responses_endpoint(&self) -> String {
        format!("{}/responses", self.base_url.trim_end_matches('/'))
    }

    /// 只有明确写了 responses 才走那条路；空值、错值都退回 chat，与历史配置保持一致
    pub fn uses_responses(&self) -> bool {
        self.api_format == "responses"
    }

    /// 只有明确写了 anthropic 才走 Anthropic Messages 那条线
    pub fn uses_anthropic(&self) -> bool {
        self.api_format == "anthropic"
    }

    /// 只有明确写了 gemini 才走 Gemini generateContent 那条线
    pub fn uses_gemini(&self) -> bool {
        self.api_format == "gemini"
    }

    /// Gemini generateContent（SSE）的服务商。base_url 建议带版本段
    /// （官方根 https://generativelanguage.googleapis.com/v1beta），不靠猜
    pub fn gemini_endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{base}/models/{}:streamGenerateContent?alt=sse", self.model)
    }

    /// Anthropic Messages 的服务商。官方根地址不带 /v1（https://api.anthropic.com），
    /// 中转站常把 /v1 写在 base_url 里——两种约定都接住，不靠猜
    pub fn anthropic_endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        }
    }

    /// 这条线能带哪些缓存旋钮。缺证据即关闭，不靠 base_url 里有没有某个品牌名猜
    /// 这一发用的模型收不收图片。问的是**模型表里那一行**：池成员与点名都会换
    /// `config.model`，而 `supports_images` 是模型的属性不是服务商的。表里没这一行
    /// 就是没收过这个证据——按不发处理，赌服务商会 400 不如先守住
    pub fn takes_images(&self) -> bool {
        self.models
            .iter()
            .any(|spec| spec.model == self.model && spec.supports_images)
    }

    /// 这一发用的模型收不收视频/音频。与 takes_images 同一口径：问模型表那一行，
    /// 没这一行按不发处理
    pub fn takes_video(&self) -> bool {
        self.models
            .iter()
            .any(|spec| spec.model == self.model && spec.supports_video)
    }

    pub fn takes_audio(&self) -> bool {
        self.models
            .iter()
            .any(|spec| spec.model == self.model && spec.supports_audio)
    }

    pub fn capability(&self) -> crate::provider::capability::Capability {
        crate::provider::capability::resolve(
            &self.model,
            &self.base_url,
            self.prompt_cache_key,
            self.cache_ttl_seconds,
            &self.cache_ttl_by_model,
        )
    }
}

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("config.json"))
}

pub fn load(app: &AppHandle) -> AppConfig {
    // 热缓存：config::load 在每条 history 命令、每轮发送、每个工具回合的热路径上被调，
    // 每次都读盘+整份解析不值这份钱。以（mtime, len）做指纹：不变就回缓存的克隆；
    // 变了（config::save 写回、设置页外的手改）才重新读，外改也看得见。
    // 指纹不等就直接回默认值的老语义保持不变
    let Ok(path) = config_path(app) else {
        return normalized_default();
    };
    let Ok(meta) = std::fs::metadata(&path) else {
        return normalized_default();
    };
    let fingerprint = (meta.modified().ok(), meta.len());

    {
        let cache = CONFIG_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = cache.as_ref() {
            if cached.path == path && cached.fingerprint == fingerprint {
                return cached.config.clone();
            }
        }
    }

    let mut config = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    normalize_context_window(&mut config);
    *CONFIG_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some(CachedConfig {
        path,
        fingerprint,
        config: config.clone(),
    });
    config
}

struct CachedConfig {
    path: PathBuf,
    fingerprint: (Option<std::time::SystemTime>, u64),
    config: AppConfig,
}

static CONFIG_CACHE: Mutex<Option<CachedConfig>> = Mutex::new(None);

/// 没有配置文件可读时的老语义：默认值 + 同样的归一化
fn normalized_default() -> AppConfig {
    let mut config = AppConfig::default();
    normalize_context_window(&mut config);
    config
}

pub(crate) fn save(app: &AppHandle, config: &AppConfig) -> Result<(), String> {
    let path = config_path(app)?;
    let text = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;

    // 先写同目录的临时文件再改名替换。配置是读-改-写出来的，正好在写一半时崩掉，
    // 留下的就是截断的 config.json，下次启动会被当成"没有配置"静默回到默认值。
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text).map_err(|e| format!("写入临时配置失败: {e}"))?;
    std::fs::rename(&temp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("替换配置文件失败: {e}")
    })?;
    // 写完直接作废缓存：下一次 load 重读一次，省得跟 mtime 的精度捉迷藏
    *CONFIG_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    Ok(())
}

/// 配置文件的完整路径。设置页提供"打开配置文件"入口用
#[tauri::command]
pub fn config_file_path(app: AppHandle) -> Result<String, String> {
    let path = config_path(&app)?;
    Ok(path.to_string_lossy().into_owned())
}

/// 用系统文件管理器打开当前话题绑定的工作目录。
///
/// 路径只从后端配置里取：opener:default 没有 allow-open-path，为这个按钮去开
/// 那条权限，等于给 webview 加一门"打开任意路径"的能力。前端传的是**话题 id**
/// 不是路径——id 只能在配置与台账里命中归属，造不出任意路径，安全模型不变。
/// 归属与文件工具同一条链（话题的项目 → 激活项目），按钮说的与工具做的必须是
/// 同一个目录
#[tauri::command]
pub fn workspace_open(app: AppHandle, conversation_id: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;

    let config = load(&app);
    let conversation_project = crate::history::conversation_project_id(&app, &conversation_id)
        .as_deref()
        .and_then(|project_id| config.project_by_id(project_id))
        .map(|project| project.path.clone());
    let path = conversation_project
        .or_else(|| config.active_project().map(|project| project.path.clone()))
        .ok_or_else(|| "还没有绑定工作目录：先在输入框上方选一个目录。".to_string())?;
    if !PathBuf::from(&path).is_dir() {
        return Err(format!("工作目录目录已经不在了：{path}"));
    }
    app.opener()
        .open_path(path, None::<&str>)
        .map_err(|e| format!("打开工作目录失败：{e}"))
}

/// 「关于」里的"打开日志"。本应用唯一成目录的日志 sink 是审计日志：
/// `<app_data_dir>/audit/audit-<日期>.jsonl`（工具调用与风险归因都在里面）。
/// 路径只从后端取、不给前端开门：与 workspace_open 同一条安全模型——
/// opener:default 没有 allow-open-path，不为一个按钮换"打开任意路径"的能力。
/// 目录还没建出来时先建再开：资源管理器打开不存在的路径是报错
#[tauri::command]
pub fn open_log_dir(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;

    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("应用数据目录定位失败：{e}"))?
        .join("audit");
    std::fs::create_dir_all(&dir).map_err(|e| format!("日志目录建不出来：{e}"))?;
    app.opener()
        .open_path(dir.to_string_lossy().into_owned(), None::<&str>)
        .map_err(|e| format!("打开日志目录失败：{e}"))
}

/// 服务商上这些值带空格没有意义，统一在落盘前去掉。
fn normalize(config: &mut AppConfig) {
    // 只去首尾空白。这里绝不能去掉 base_url 的结尾 '/'：输入框是受控的，
    // 保存即改写会把用户正在敲的那个斜杠吞掉，表现为"打不进 /"。
    config.base_url = config.base_url.trim().to_string();
    config.model = config.model.trim().to_string();
    config.reasoning_effort = config.reasoning_effort.trim().to_string();
    config.credential_service = config.credential_service.trim().to_string();
    config.credential_user = config.credential_user.trim().to_string();
}

/// 把补丁里出现的顶层键合进当前配置，其余字段一个都不动。
/// 单独拆出来是为了能不带 AppHandle 断言这条规则——它正是这次修复的全部内容。
fn merge_patch(current: &AppConfig, patch: &Value) -> Result<AppConfig, String> {
    let changes = patch
        .as_object()
        .ok_or_else(|| "配置补丁得是一个对象。".to_string())?;

    let mut merged = serde_json::to_value(current).map_err(|e| e.to_string())?;
    let target = merged
        .as_object_mut()
        .ok_or_else(|| "当前配置序列化出来不是对象。".to_string())?;

    for (key, value) in changes {
        if key == "conversationStore" {
            // 这个开关不能存在它所选择的存储里，改它得先迁数据，只有 storage_switch 有资格
            return Err("存储介质不能通过配置补丁改，请在设置里切换存储。".into());
        }
        if !target.contains_key(key) {
            // 键名写错如果只是被静默丢掉，界面上就表现为"开关拨了没反应"
            return Err(format!("配置里没有「{key}」这一项，补丁没有写入。"));
        }
        target.insert(key.clone(), value.clone());
    }

    let mut config: AppConfig =
        serde_json::from_value(merged).map_err(|e| format!("补丁合进去后配置读不回来了：{e}"))?;
    normalize(&mut config);
    config.check_overrides()?;
    // 代理地址的形状只在**这份补丁正在写代理池**时问（形状规则见 parse_proxy_url）；
    // 空 url 是设置页"新增代理"落下的草稿行，允许落盘，池挑选时本就跳过它
    if changes.contains_key("proxyPool") {
        for entry in &config.proxy_pool.proxies {
            if entry.url.trim().is_empty() {
                continue;
            }
            crate::proxy::parse_proxy_url(&entry.url)
                .map_err(|error| format!("代理「{}」的地址不合法：{error}", entry.name))?;
        }
    }
    // 图的形状只在**这份补丁正在写任务**时问：config.json 是用户可以手改的，
    // 一条没被碰过的坏图不该顺手把"改一下模型名"也一起挡在门外
    if changes.contains_key("tasks") {
        config.check_task_graphs()?;
    }
    Ok(config)
}

/// keyring v4 在 Windows 上把条目名拼成 "{用户名}.{服务}"，
/// 提示文案必须给这个真实目标名，否则用户照提示手建条目后仍然读不到。
/// 指定凭据目标读钥匙。档案弹窗按它自己的目标拉模型列表、
/// cc-switch 导入按来源隔离的目标写钥匙，都走这一条
pub(crate) fn api_key_for(service: &str, user: &str) -> Result<String, String> {
    let entry = keyring::Entry::new(service, user)
        .map_err(|e| format!("凭据条目初始化失败：{e}"))?;
    match entry.get_password() {
        Ok(secret) if !secret.trim().is_empty() => Ok(secret),
        Ok(_) | Err(keyring::Error::NoEntry) => Err(format!(
            "未找到 API 密钥。请在 Windows 凭据管理器新增「通用凭据」，目标 {user}.{service}。"
        )),
        Err(e) => Err(format!("读取凭据失败：{e}")),
    }
}

pub fn api_key(config: &AppConfig) -> Result<String, String> {
    api_key_for(&config.credential_service, &config.credential_user)
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    /// 窗口 0 = 没填：读数边界必须把它归一成默认值——按 0 算的后果是预算表 limit=0
    /// （自动压缩每轮开闸）、输出钳制每轮截到 1K、用量面板恒 0%（真机踩过）。
    /// 显式存成 0 的旧配置 serde 的 default 补不了，靠的就是这道归一
    #[test]
    fn a_zero_context_window_normalizes_to_the_default_at_read_boundaries() {
        // 存档里的显式 0（load 的归一在 I/O 那侧，这里钉的是同一只函数）
        let mut config = AppConfig::default();
        config.context_tokens = 0;
        normalize_context_window(&mut config);
        assert_eq!(config.context_tokens, DEFAULT_CONTEXT_TOKENS, "0 → 默认窗口");

        // 档案连接应用是 0 的另一条入口：档案本身没填窗口，顶层也不许跟着变 0
        let mut config = AppConfig::default();
        config.context_tokens = 0;
        let mut profile = profile_from_config("prof-1".into(), "某档案", &config);
        profile.context_tokens = 0;
        apply_profile_connection(&mut config, &profile);
        assert_eq!(
            config.context_tokens,
            DEFAULT_CONTEXT_TOKENS,
            "档案 0 → 顶层照旧归一，池路由读到的也是它"
        );

        // 用户真填了的窗口不许被改
        let mut config = AppConfig::default();
        config.context_tokens = 200_000;
        normalize_context_window(&mut config);
        assert_eq!(config.context_tokens, 200_000, "显式窗口原样保留");
    }

    /// 未绑定工作目录的基准根：落到用户主目录，绑定了就是项目目录。
    /// 文件/命令工具的门槛从"绑没绑"改成"有没有基准根"（几乎恒有），这条要钉住
    #[test]
    fn effective_root_falls_back_to_the_home_directory() {
        let mut config = AppConfig::default();
        let home = home_root().expect("测试机有 USERPROFILE");

        assert_eq!(config.effective_root(), Some(home.clone()), "没绑项目 → 主目录");

        config.projects.push(Project {
            id: "p1".into(),
            name: "项目".into(),
            path: "C:\\work\\demo".into(),
            permission_overrides: Vec::new(),
            file_rules: Vec::new(),
            command_rules: Vec::new(),
        });
        config.active_project_id = "p1".into();
        assert_eq!(
            config.effective_root(),
            Some(PathBuf::from("C:\\work\\demo")),
            "绑了项目 → 项目目录优先于主目录"
        );
    }

    /// 设置界面读的是这些键名，少一个就是一片 undefined，而且不会有人报错
    #[test]
    fn the_config_payload_matches_the_frontend_types() {
        let value = serde_json::to_value(AppConfig::default()).unwrap();
        crate::test_support::assert_matches_ts(&value, "AppConfig");
        crate::test_support::assert_matches_ts(&value["ui"], "UiState");
        crate::test_support::assert_matches_ts(&value["modelPool"], "ModelPool");
        crate::test_support::assert_matches_ts(&value["imageGen"], "ImageGenSettings");
        crate::test_support::assert_matches_ts(&value["videoGen"], "VideoGenSettings");
    }

    /// 代理条目的 IPC 形状：设置页整组读写 `proxies`，少一个字段
    /// 就是代理列表里静默出现一格 undefined
    #[test]
    fn the_proxy_entry_payload_matches_the_frontend_type() {
        let mut config = AppConfig::default();
        config.proxy_pool.proxies.push(ProxyEntry::default());
        let value = serde_json::to_value(&config.proxy_pool).unwrap();
        crate::test_support::assert_matches_ts(&value, "ProxyPool");
        crate::test_support::assert_matches_ts(&value["proxies"][0], "ProxyEntry");
    }

    /// 模型行的 IPC 形状：档案弹窗按行编辑 `models`，字段名对不上就是
    /// 卡片上静默出现一格 undefined——而这一行管的是窗口与思考档，读错就发错
    #[test]
    fn the_model_spec_payload_matches_the_frontend_type() {
        let mut config = AppConfig::default();
        config.models.push(ModelSpec::default());
        let value = serde_json::to_value(&config).unwrap();
        crate::test_support::assert_matches_ts(&value["models"][0], "ModelSpec");
    }

    /// 子助理定义的 IPC 形状：设置页整组读写 `subagents`，少一个字段
    /// 就是编辑弹窗里静默出现一格 undefined
    #[test]
    fn the_subagent_def_payload_matches_the_frontend_type() {
        let mut config = AppConfig::default();
        config.subagents.push(SubagentDef::default());
        let value = serde_json::to_value(&config.subagents[0]).unwrap();
        crate::test_support::assert_matches_ts(&value, "SubagentDef");
    }

    /// 内置覆盖项的 IPC 形状：设置页整组读写 `subagentOverrides`，键名对不上
    /// 就是覆盖静默丢格——「停用了怎么又派出去了」这类问题最难查
    #[test]
    fn the_subagent_override_payload_matches_the_frontend_type() {
        let mut config = AppConfig::default();
        config.subagent_overrides.push(SubagentOverride::default());
        let value = serde_json::to_value(&config.subagent_overrides[0]).unwrap();
        crate::test_support::assert_matches_ts(&value, "SubagentOverride");
    }

    /// 路由规则的 IPC 形状：设置页整组读写 `modelRoutes`，字段名对不上
    /// 就是规则静默丢行——「明明配了怎么没生效」先查这里
    #[test]
    fn the_model_route_payload_matches_the_frontend_type() {
        let mut config = AppConfig::default();
        config.model_routes.push(ModelRoute::default());
        let value = serde_json::to_value(&config.model_routes[0]).unwrap();
        crate::test_support::assert_matches_ts(&value, "ModelRoute");
    }

    /// Anthropic 线的服务商：官方根地址不带 /v1，中转站常带——两种约定都要接住
    #[test]
    fn the_anthropic_endpoint_accepts_both_base_url_conventions() {
        let mut config = AppConfig::default();
        config.api_format = "anthropic".into();

        config.base_url = "https://api.anthropic.com".into();
        assert_eq!(
            config.anthropic_endpoint(),
            "https://api.anthropic.com/v1/messages"
        );
        config.base_url = "https://relay.example.test/v1/".into();
        assert_eq!(
            config.anthropic_endpoint(),
            "https://relay.example.test/v1/messages"
        );
        assert!(config.uses_anthropic());
        assert!(!config.uses_responses());
    }

    /// 逐模型 TTL 经配置一路贯通到能力表：有条目以条目为准，没有则内置表兜底
    #[test]
    fn a_per_model_ttl_entry_reaches_the_capability_through_the_config() {
        let mut config = AppConfig::default();
        config.base_url = "https://relay.example.test/v1".into();
        config.model = "claude-sonnet-4-6".into();

        config
            .cache_ttl_by_model
            .insert("claude-sonnet-4-6".into(), 600);
        assert_eq!(config.capability().cache_ttl_seconds, 600);

        config.cache_ttl_by_model.clear();
        assert_eq!(
            config.capability().cache_ttl_seconds,
            300,
            "没有条目时内置表兜底"
        );
    }
}

#[cfg(test)]
mod profile_tests {
    use super::*;
    use serde_json::json;

    /// 一条带着完整连接域字段的配置 + 指向它自己的当前档案
    fn config_with_active_profile() -> AppConfig {
        let mut config = AppConfig::default();
        config.base_url = "https://api.deepseek.com/v1".into();
        config.model = "deepseek-chat".into();
        config.api_format = "chat".into();
        config.reasoning_effort = "medium".into();
        config.temperature = 0.7;
        config.max_tokens = 4096;
        config.context_tokens = 128_000;
        config.auto_compact = true;
        config.prompt_cache_key = Some(false);
        config.cache_ttl_seconds = Some(120);
        config
            .cache_ttl_by_model
            .insert("deepseek-chat".into(), 300);
        config.credential_service = "aglab/api-key".into();
        config.credential_user = "default".into();
        let profile = profile_from_config("prof-1".into(), "DeepSeek 常用", &config);
        config.profiles.push(profile);
        config.active_profile_id = "prof-1".into();
        config
    }

    /// 档案的序列化键必须恰好等于"身份两键 + 连接域键清单"——
    /// 往档案加字段而忘了进抄写路径（或反之），切换就会静默丢字段
    #[test]
    fn the_profile_fields_and_the_field_key_list_cover_the_same_ground() {
        let profile = profile_from_config("prof-x".into(), "名字", &AppConfig::default());
        let mut keys: Vec<String> = serde_json::to_value(&profile)
            .expect("档案该能序列化")
            .as_object()
            .expect("档案序列化出来是对象")
            .keys()
            .cloned()
            .collect();
        keys.sort();

        let mut expected: Vec<String> = PROFILE_FIELD_KEYS
            .iter()
            .map(|key| key.to_string())
            .chain(["id".to_string(), "name".to_string()])
            .collect();
        expected.sort();

        assert_eq!(keys, expected);
    }

    /// 老配置里没有模型表、模型行里没写"可派工"：两处都必须落成"没有这一行/可以派工"，
    /// 而不是 bool 与 Vec 的 derive 默认——否则升级一次，既有档案一夜之间不可派工
    #[test]
    fn model_rows_from_before_the_flags_default_to_lazy_and_delegatable() {
        let profile: EndpointProfile = serde_json::from_value(serde_json::json!({
            "id": "p",
            "name": "老档案",
            "baseUrl": "https://x.example/v1",
            "model": "m",
        }))
        .expect("老配置该能照常读进来");
        assert!(profile.models.is_empty(), "没有 models 就是没有行");

        let with_row: EndpointProfile = serde_json::from_value(serde_json::json!({
            "id": "p",
            "models": [{ "model": "m", "contextTokens": 128_000 }],
        }))
        .expect("行里缺字段也该能读");
        let row = with_row.models.first().expect("有一行");
        assert!(row.delegatable, "缺 delegatable 是可派工，不是禁止");
        assert_eq!(row.max_tokens, 0, "没填的读数用档案级默认，不猜一个数");
        assert_eq!(row.reasoning_effort, None, "None = 没填，与 Some(\"\") = 明确不发送是两件事");
    }

    /// 模型行只在**填了的那一格**盖档案默认：0 与 None 是"没填"，不是"零窗口/不发送"。
    /// 而 `Some("")` 是明确决定——它必须真的把思考档清成不发送
    #[test]
    fn a_model_row_only_overrides_the_cells_it_fills() {
        let mut config = AppConfig::default();
        config.context_tokens = 200_000;
        config.max_tokens = 4096;
        config.reasoning_effort = "high".into();
        config.models = vec![
            ModelSpec {
                model: "填过的".into(),
                context_tokens: 128_000,
                max_tokens: 0,
                reasoning_effort: Some(String::new()),
                ..Default::default()
            },
            ModelSpec {
                model: "空行".into(),
                ..Default::default()
            },
        ];

        config.model = "填过的".into();
        apply_model_spec(&mut config);
        assert_eq!(config.context_tokens, 128_000, "填了的那格盖上去");
        assert_eq!(config.max_tokens, 4096, "0 = 没填，不该把档案的 4096 抹成 0");
        assert_eq!(config.reasoning_effort, "", "Some(\"\") 是明确不发送");

        // 每一发都是从档案默认重新起步的（overlay 之前 config 是新读/新抄的那份），
        // 所以这里要把默认摆回去再问"空行会不会动它"
        config.context_tokens = 200_000;
        config.max_tokens = 4096;
        config.reasoning_effort = "high".into();
        config.model = "空行".into();
        apply_model_spec(&mut config);
        assert_eq!(config.context_tokens, 200_000, "整行都没填就一个字段都不动");
        assert_eq!(config.max_tokens, 4096);
        assert_eq!(config.reasoning_effort, "high");

        config.model = "表里没有".into();
        apply_model_spec(&mut config);
        assert_eq!(config.context_tokens, 200_000, "命不中就用档案默认，不猜");
    }

    /// 保存即快照：档案拿到当前全部连接域字段，并立即成为当前档案
    #[test]
    fn saving_a_profile_snapshots_the_connection_and_becomes_active() {
        let config = config_with_active_profile();
        let profile = config.profiles.first().expect("有档案");
        assert_eq!(profile.name, "DeepSeek 常用");
        assert_eq!(profile.base_url, "https://api.deepseek.com/v1");
        assert_eq!(profile.credential_user, "default");
        assert_eq!(
            profile.cache_ttl_by_model.get("deepseek-chat"),
            Some(&300),
            "逐模型 TTL 也要进档案"
        );
        assert_eq!(config.active_profile_id, "prof-1");
    }

    /// 编辑不污染档案：补丁只动顶层，已存档案保持保存那一刻的快照。
    /// "临时改一下试试"不该把存档悄悄改掉——要更新卡片就走编辑弹窗
    #[test]
    fn patching_a_connection_field_leaves_the_saved_profile_alone() {
        let current = config_with_active_profile();
        let original_model = current.profiles[0].model.clone();

        let patched =
            merge_patch(&current, &json!({"baseUrl": "https://new.example.test/v1", "model": "glm-5.3"}))
                .unwrap();
        assert_eq!(patched.base_url, "https://new.example.test/v1");
        assert_eq!(
            patched.profiles[0].base_url, current.profiles[0].base_url,
            "顶层编辑不该惊动已存档案"
        );
        assert_eq!(patched.profiles[0].model, original_model);
    }

    /// 保存对"使用中"档案的编辑：档案更新、顶层同步抄写——
    /// 编辑当前卡片等于改当前连接（含凭据对）
    #[test]
    fn updating_the_active_profile_rewrites_the_top_level() {
        let mut config = config_with_active_profile();

        let mut draft = config.profiles[0].clone();
        draft.name = "DeepSeek 改过名".into();
        draft.base_url = "https://api2.deepseek.com/v1".into();
        draft.credential_user = "second".into();
        let mut by_model = BTreeMap::new();
        by_model.insert("deepseek-chat".into(), 600);
        draft.cache_ttl_by_model = by_model;

        write_profile_fields(&mut config, &draft).expect("更新该成功");
        normalize(&mut config);

        assert_eq!(config.profiles[0].name, "DeepSeek 改过名");
        assert_eq!(config.base_url, "https://api2.deepseek.com/v1");
        assert_eq!(config.credential_user, "second", "凭据对跟着当前档案走");
        assert_eq!(config.cache_ttl_by_model.get("deepseek-chat"), Some(&600));
    }

    /// 保存对"非使用中"档案的编辑：只有那张卡片变，顶层一字不动——
    /// 别的卡片不跟着抖，正在跑的连接也不受影响
    #[test]
    fn updating_an_inactive_profile_leaves_the_top_level_alone() {
        let mut config = config_with_active_profile();
        let mut second = config.profiles[0].clone();
        second.id = "prof-2".into();
        second.name = "备用".into();
        second.model = "glm-5.3".into();
        config.profiles.push(second);
        let top_before = config.base_url.clone();

        let mut draft = config.profiles[1].clone();
        draft.model = "kimi-k3".into();
        write_profile_fields(&mut config, &draft).expect("更新该成功");

        assert_eq!(config.profiles[1].model, "kimi-k3");
        assert_eq!(
            config.model, "deepseek-chat",
            "当前连接不该被别的卡片的编辑带走"
        );
        assert_eq!(config.base_url, top_before);

        let missing = EndpointProfile {
            id: "prof-ghost".into(),
            ..config.profiles[0].clone()
        };
        assert!(
            write_profile_fields(&mut config, &missing).is_err(),
            "更新不存在的档案必须报错"
        );
    }

    /// 切换 = 整体抄写：凭据对与逐模型 TTL 一起换，active 指过去
    #[test]
    fn switching_copies_every_field_including_credentials_and_cache_map() {
        let deepseek = config_with_active_profile();

        // 另一张档案：Anthropic 官方 + 不同的凭据目标
        let mut other = deepseek.clone();
        other.base_url = "https://api.anthropic.com".into();
        other.model = "claude-sonnet-4-6".into();
        other.api_format = "anthropic".into();
        other.credential_service = "aglab/anthropic".into();
        other.credential_user = "main".into();
        let mut by_model = BTreeMap::new();
        by_model.insert("claude-sonnet-4-6".into(), 600);
        other.cache_ttl_by_model = by_model.clone();
        let anthropic = profile_from_config("prof-2".into(), "Claude 官方", &other);

        let mut config = deepseek;
        config.profiles.push(anthropic);

        // 像命令那样切换
        let target = config.profiles.last().cloned().expect("有档案");
        apply_profile_to_config(&mut config, &target);
        normalize(&mut config);

        assert_eq!(config.active_profile_id, "prof-2");
        assert_eq!(config.base_url, "https://api.anthropic.com");
        assert_eq!(config.model, "claude-sonnet-4-6");
        assert_eq!(config.api_format, "anthropic");
        assert_eq!(config.credential_service, "aglab/anthropic");
        assert_eq!(config.credential_user, "main");
        assert_eq!(
            config.cache_ttl_by_model.get("claude-sonnet-4-6"),
            Some(&600)
        );
        assert!(
            config.cache_ttl_by_model.get("deepseek-chat").is_none(),
            "旧服务商的逐模型表不能残留"
        );
    }

    /// 删掉当前档案：卡片消失、关联清空，但顶层连接原样生效——
    /// 用户只是不想要这张卡片，不是想换掉正在用的连接
    #[test]
    fn deleting_the_active_profile_keeps_the_connection_working() {
        let mut config = config_with_active_profile();
        assert!(config.profiles.iter().any(|profile| profile.id == "prof-1"));

        config.profiles.retain(|profile| profile.id != "prof-1");
        if config.active_profile_id == "prof-1" {
            config.active_profile_id = String::new();
        }

        assert!(config.profiles.is_empty());
        assert_eq!(config.active_profile_id, "");
        assert_eq!(
            config.base_url, "https://api.deepseek.com/v1",
            "顶层连接不受删除影响"
        );
    }
}

#[cfg(test)]
mod patch_tests {
    use super::*;
    use serde_json::json;

    fn config_with_a_trusted_hook() -> AppConfig {
        AppConfig {
            model: "deepseek-chat".into(),
            trusted_hooks: vec![TrustedHook {
                id: "hook-demo::dir::PreToolUse::0".into(),
                hash: "0123456789abcdef".into(),
            }],
            ..Default::default()
        }
    }

    /// 这次修复的全部内容：改一个字段不许碰别的字段。
    /// 整份覆盖写时，插件页手里那份过期快照一存，设置页刚改的模型就回去了。
    #[test]
    fn patching_one_field_leaves_every_other_field_alone() {
        let current = config_with_a_trusted_hook();

        let patched = merge_patch(
            &current,
            &json!({"ui": {"sidebarCollapsed": true, "panelCollapsed": false,
                           "panelTab": "decision", "section": "plugins"}}),
        )
        .unwrap();

        assert_eq!(patched.ui.section, "plugins");
        assert_eq!(patched.model, current.model);
        assert_eq!(patched.trusted_hooks, current.trusted_hooks);
    }

    #[test]
    fn an_empty_patch_rewrites_nothing() {
        let current = config_with_a_trusted_hook();
        let patched = merge_patch(&current, &json!({})).unwrap();
        assert_eq!(
            serde_json::to_value(&patched).unwrap(),
            serde_json::to_value(&current).unwrap()
        );
    }

    /// 键名写错如果只是被静默丢掉，界面上就表现为"开关拨了没反应"，谁也查不出来
    #[test]
    fn a_key_that_is_not_a_config_field_is_an_error() {
        let err = merge_patch(&AppConfig::default(), &json!({"disabledHoks": []})).unwrap_err();
        assert!(err.contains("disabledHoks"), "{err}");
    }

    /// 存储介质由 storage_switch 改，它要先迁数据；补丁绕过去就会留下两半记录
    #[test]
    fn the_store_switch_cannot_be_moved_by_a_patch() {
        let err = merge_patch(
            &AppConfig::default(),
            &json!({"conversationStore": "sqlite"}),
        )
        .unwrap_err();
        assert!(err.contains("存储介质"), "{err}");
    }

    #[test]
    fn a_patch_that_is_not_an_object_is_refused() {
        assert!(merge_patch(&AppConfig::default(), &json!(null)).is_err());
        assert!(merge_patch(&AppConfig::default(), &json!("model")).is_err());
    }

    /// 认不出的键名要被拒，不是被静默丢掉——这一支的代码注释早就写着理由
    /// （"界面上表现为开关拨了没反应"），但**从来没有测试罩着**：
    /// 把那段 `contains_key` 判断删掉，全库不会有任何一条测试变红。
    /// 混合补丁也要整体拒：不能"能认的那半写进去、认错的那半丢掉"，那是半份配置
    #[test]
    fn a_patch_naming_a_key_that_does_not_exist_is_refused() {
        // 差一个字母的那种错法：`contextTokens` 是真键（下面的正向对照就是它）
        let err = merge_patch(&AppConfig::default(), &json!({"contextTokenz": 1}))
            .expect_err("打错一个字母的键名不能算改了配置");
        assert!(err.contains("contextTokenz"), "要把认错的那个键说给他听：{err}");
        assert!(
            merge_patch(&AppConfig::default(), &json!({"model": "m", "noSuchKey": 1})).is_err(),
            "一半认得一半不认得时，整份补丁都得退回去"
        );
        // 正向对照：真正存在的键名走的是同一段代码，不能被这条测试误伤
        assert_eq!(
            merge_patch(&AppConfig::default(), &json!({"contextTokens": 123_456}))
                .expect("存在的键该写进去")
                .context_tokens,
            123_456
        );
    }

    /// 上面那几条量的是纯函数自己合得对不对。**命令有没有去问它**是另一件事：
    /// 测试本身就是 `merge_patch` 的调用方，所以把 `config_patch` 里那一行删掉，
    /// 编译器一声不响、这一族测试一条不红——"库层成立≠链路成立"
    #[test]
    fn the_patch_command_asks_the_one_merge_gate() {
        // 每一根针脚都用 concat! 拼：这条测试自己就在被搜的那份文件里，
        // 而且 config.rs 的 test 模块排在命令前面——写成一整串，`split` 拿到的第一段
        // 会是这条测试自己，`count` 也会数到自己
        let source = include_str!("config.rs");
        let command = source
            .split(concat!("pub fn config", "_patch"))
            .nth(1)
            .expect("命令得在")
            .split("\n}")
            .next()
            .unwrap_or_default();
        assert!(
            command.contains(concat!("merge_pat", "ch(&load(&app), &patch)?")),
            "设置那条命令没去问判据：{command}"
        );
        assert!(
            command.contains("save(&app, &config)?"),
            "合完不写盘，那句改好了就是假的：{command}"
        );
        assert_eq!(
            source.matches(concat!("fn merge_", "patch(")).count(),
            1,
            "合补丁的规则只许有一份"
        );
        assert_eq!(
            source.matches(concat!("这一项，补丁没有", "写入")).count(),
            1,
            "认错键名那句不许在别处再抄一份"
        );
    }

    /// 落盘前仍然去掉首尾空白，但不能吃掉用户正在敲的那个结尾斜杠
    #[test]
    fn text_fields_are_trimmed_but_a_trailing_slash_survives() {
        let patched = merge_patch(
            &AppConfig::default(),
            &json!({"baseUrl": "  https://api.deepseek.com/v1/  "}),
        )
        .unwrap();
        assert_eq!(patched.base_url, "https://api.deepseek.com/v1/");
    }

    /// 类型对不上时必须报错，不能把配置写成一半就落盘
    #[test]
    fn a_patch_with_the_wrong_type_for_a_field_is_refused() {
        let err = merge_patch(&AppConfig::default(), &json!({"maxTokens": "2048"})).unwrap_err();
        assert!(err.contains("读不回来"), "{err}");
    }

    /// 认不出来的键存不进去：存进去的那一条永远不会命中任何一次判定，
    /// 而界面上那一行显示着"我拦了 git"——静默失效是最坏的一种坏
    #[test]
    fn an_override_with_an_unknown_key_is_refused_at_the_boundary() {
        let err = merge_patch(
            &AppConfig::default(),
            &json!({"permissionOverrides": [{ "key": "exc", "level": "deny" }]}),
        )
        .expect_err("拼错的键不该被收下");
        assert!(err.contains("exc"), "要点出是哪一条：{err}");

        let good = merge_patch(
            &AppConfig::default(),
            &json!({"permissionOverrides": [{ "key": "exec", "level": "deny" }]}),
        )
        .expect("认识的键该存得进去");
        assert_eq!(good.permission_overrides.len(), 1);
        assert_eq!(
            good.active_policy().resolve(&crate::policy::Capability::Exec {
                scope: crate::policy::ExecScope::Git
            }),
            crate::policy::Level::Deny,
            "存进去的覆盖项要真的改到判定，不然它只是配置里的一段文本"
        );
    }

    /// 代理地址的形状在**写代理池的那一刻**问。proxy.rs 里那条测试只证明规则本身，
    /// 这条证明保存路径真的去调了它；空 url 是"新增代理"落下的草稿行，要放行
    #[test]
    fn a_proxy_pool_patch_with_a_malformed_url_is_refused_where_the_pool_is_written() {
        let entry = |id: &str, name: &str, url: &str| {
            json!({"id": id, "name": name, "url": url, "enabled": true})
        };

        let err = merge_patch(
            &AppConfig::default(),
            &json!({"proxyPool": {"strategy": "round_robin",
                "proxies": [entry("p1", "家里那台", "127.0.0.1:7890")]}}),
        )
        .expect_err("没有协议前缀的地址不该被收下");
        assert!(err.contains("家里那台"), "要说清是哪条代理：{err}");

        merge_patch(
            &AppConfig::default(),
            &json!({"proxyPool": {"strategy": "round_robin",
                "proxies": [entry("p1", "家里那台", "socks5://user:pass@proxy.example.test:1080")]}}),
        )
        .expect("形状对的地址该存得进去");

        let patched = merge_patch(
            &AppConfig::default(),
            &json!({"proxyPool": {"strategy": "round_robin",
                "proxies": [entry("p1", "新增草稿", "")]}}),
        )
        .expect("设置页新增代理先落一行空草稿，不能被这道判据挡住");
        assert_eq!(patched.proxy_pool.proxies.len(), 1);
    }

    fn task(id: &str, name: &str, nodes: Vec<crate::tasks::graph::Node>) -> ScheduledTask {
        ScheduledTask {
            id: id.into(),
            name: name.into(),
            graph: crate::tasks::graph::TaskGraph {
                nodes,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn filled(id: &str, prompt: &str, depends_on: &[&str]) -> crate::tasks::graph::Node {
        crate::tasks::graph::Node {
            id: id.into(),
            prompt: prompt.into(),
            depends_on: depends_on.iter().map(|item| item.to_string()).collect(),
            ..Default::default()
        }
    }

    /// 一张发不出去的图现在站在**写它的那一刻**。三个坏法各钉一条，正向对照钉住
    /// "这道判据不是什么都拒"；最后那一支是默认档零变化：没写图的任务照旧存得下去
    #[test]
    fn a_graph_that_cannot_be_ordered_is_refused_where_the_config_is_written() {
        let written = |nodes: Vec<crate::tasks::graph::Node>| {
            json!({"tasks": [task("nightly", "夜里那条", nodes)]})
        };

        let err = merge_patch(
            &AppConfig::default(),
            &written(vec![
                filled("汇总", "汇总今天", &[]),
                filled("写说明", "据此写说明", &["没这一格"]),
            ]),
        )
        .expect_err("第二格等着一个不存在的前置，这张图发不出去");
        assert!(err.contains("夜里那条"), "要说清是哪条任务：{err}");
        assert!(err.contains("没这一格"), "要说清是哪一根边断了：{err}");

        let err = merge_patch(
            &AppConfig::default(),
            &written(vec![filled("汇总", "   ", &[])]),
        )
        .expect_err("一句要说的话都没有的一格不该算配好了");
        assert!(err.contains("没有要发出去的内容"), "{err}");

        merge_patch(
            &AppConfig::default(),
            &written(vec![
                filled("汇总", "汇总今天", &[]),
                filled("写说明", "据此写说明", &["汇总"]),
            ]),
        )
        .expect("一张说得通的图不该被这道判据拦住");

        let patched = merge_patch(&AppConfig::default(), &written(Vec::new()))
            .expect("没写图 = 老的单发形状，一个字节都不该被这道判据改到");
        assert!(patched.tasks[0].graph.is_empty());
    }

    /// 这道判据只问**这份补丁正在写**的那一侧。config.json 是用户可以手改的：
    /// 一条没被碰过的坏图不该顺手把"改一下模型名"也挡在门外——那会把一道好判据
    /// 变成用户怎么都关不掉的弹窗
    #[test]
    fn a_graph_nobody_touched_does_not_hold_an_unrelated_save_hostage() {
        let mut current = AppConfig::default();
        current.tasks = vec![task("nightly", "夜里那条", vec![filled("a", "做 a", &["不在场"])])];

        merge_patch(&current, &json!({"model": "deepseek-chat"}))
            .expect("这份补丁没在写任务，坏图不该替它挨一发");
        merge_patch(
            &current,
            &json!({"tasks": serde_json::to_value(&current.tasks).unwrap()}),
        )
        .expect_err("这一份确实在写任务，那张坏图就该被拦下来");
    }

    /// 判据剥成纯函数之后，"写入口有没有去问它"要单独钉一次：把 `merge_patch`
    /// 里那三行删掉，上面两条判据测试照样全绿——它们自己就是那道判据的调用方。
    /// 这一条量的是**存在**，不是"真挡住过一次"：那要靠跑起来的应用
    #[test]
    fn the_save_path_asks_the_graph_gate_instead_of_reinventing_it() {
        // 针脚用 concat! 拼：这条测试自己就在被搜的这份文件里
        let source = include_str!("config.rs");
        let body = source
            .split(concat!("fn merge_", "patch("))
            .nth(1)
            .expect("合补丁那个函数得在")
            .split("\n}")
            .next()
            .unwrap_or_default();
        assert!(
            body.contains("config.check_task_graphs()?"),
            "写入口没去问那道判据：{body}"
        );
        assert!(
            body.contains("changes.contains_key(\"tasks\")"),
            "问话没绑在「这份补丁在写任务」上，一条没人碰过的坏图就会挡住别的保存：{body}"
        );
        assert_eq!(
            source.matches(concat!("fn check_task_", "graphs(")).count(),
            1,
            "图的判据只许有一份"
        );
        let gate = source
            .split(concat!("fn check_task_", "graphs("))
            .nth(1)
            .expect("那道判据得在")
            .split("\n    }")
            .next()
            .unwrap_or_default();
        assert!(
            gate.contains("task.graph.validate()"),
            "它没把活交给图自己那份 validate()，那就是抄了第二份规则：{gate}"
        );
    }

    /// 项目那一份只在这个项目是活动项目时生效。漏了这一条，"按项目"就等于按最后写入
    #[test]
    fn a_projects_overrides_only_apply_while_it_is_the_active_project() {        use crate::policy::{ExecScope, Level, PermissionOverride, Capability};
        let mut config = AppConfig::default();
        config.projects = vec![Project {
            id: "p1".into(),
            name: "仓库".into(),
            path: "C:/repo".into(),
            permission_overrides: vec![PermissionOverride { key: "exec".into(), level: Level::Deny }],
            file_rules: Vec::new(),
            command_rules: Vec::new(),
        }];
        config.active_project_id = "p1".into();
        assert_eq!(
            config.active_policy().resolve(&Capability::Exec { scope: ExecScope::Git }),
            Level::Deny,
            "活动项目那一份要进得了生效的表"
        );

        config.active_project_id = String::new();
        assert!(
            config.active_policy().overrides.is_empty(),
            "没绑项目时，别的项目的覆盖项不该还在生效"
        );
    }
}

#[tauri::command]
pub fn config_get(app: AppHandle) -> AppConfig {
    load(&app)
}

/// 按字段落盘：只改补丁里出现的顶层键，其余一律以磁盘上那份为准。
/// 早先是前端把整个 config 提交回来覆盖写的，于是"谁手里那份快照最新"决定了一切——
/// 插件页开着不动、在设置页改了模型，插件页一存就把模型抹回旧值。
#[tauri::command]
pub fn config_patch(app: AppHandle, patch: Value) -> Result<AppConfig, String> {
    let config = merge_patch(&load(&app), &patch)?;
    save(&app, &config)?;
    // 代理绑定变了：下一次 spawn 的子进程要立刻知道，离场地址的连接池与账也要掉
    crate::proxy::on_config_changed(&config);
    crate::tools::set_command_shell(&config.command_shell);
    crate::tools::set_ssh_hosts(&config.ssh_hosts);
    crate::lsp_host::set_server_overrides(&config.lsp_servers);
    // 语义检索的快照跟着刷新（embedding 档可能变了）
    crate::knowledge::on_config_changed(&config);
    Ok(config)
}

/// 只回答"密钥在不在"，绝不把密钥内容送回前端
#[tauri::command]
pub fn credential_probe(app: AppHandle) -> bool {
    api_key(&load(&app)).is_ok()
}

/// 前端只能写入、不能读回密钥内容，避免它出现在界面或配置文件里
#[tauri::command]
pub fn credential_set(app: AppHandle, secret: String) -> Result<(), String> {
    let config = load(&app);
    let secret = secret.trim();
    if secret.is_empty() {
        return Err("密钥为空。".into());
    }

    keyring::Entry::new(&config.credential_service, &config.credential_user)
        .map_err(|e| format!("凭据条目初始化失败：{e}"))?
        .set_password(secret)
        .map_err(|e| format!("写入凭据失败：{e}"))
}

/// embedding 专用密钥：写进独立凭据槽，与主密钥/各档案槽互不牵连
#[tauri::command]
pub fn embedding_credential_set(secret: String) -> Result<(), String> {
    let secret = secret.trim();
    if secret.is_empty() {
        return Err("密钥为空。".into());
    }
    keyring::Entry::new(EMBEDDING_CREDENTIAL_SERVICE, EMBEDDING_CREDENTIAL_USER)
        .map_err(|e| format!("凭据条目初始化失败：{e}"))?
        .set_password(secret)
        .map_err(|e| format!("写入凭据失败：{e}"))
}

/// 清掉 embedding 专用密钥：语义检索回到沿用主密钥
#[tauri::command]
pub fn embedding_credential_clear() -> Result<(), String> {
    keyring::Entry::new(EMBEDDING_CREDENTIAL_SERVICE, EMBEDDING_CREDENTIAL_USER)
        .map_err(|e| format!("凭据条目初始化失败：{e}"))?
        .delete_credential()
        .or_else(|e| match e {
            keyring::Error::NoEntry => Ok(()),
            other => Err(format!("清除凭据失败：{other}")),
        })
}

/// embedding 专用密钥在不在。只答在不在，密钥内容不回前端
#[tauri::command]
pub fn embedding_credential_probe() -> bool {
    api_key_for(EMBEDDING_CREDENTIAL_SERVICE, EMBEDDING_CREDENTIAL_USER).is_ok()
}

/// 导入（import.rs）也要建项目，所以这条 id 生成是 crate 内公共的
pub(crate) fn new_project_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("proj-{nanos:x}")
}

/// 服务商档案的 id。同一条时钟抖动策略，前缀区分开就行
fn new_profile_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("prof-{nanos:x}")
}

/// 新建档案：把草稿落成一张卡片、设为当前档案、并把字段抄进顶层——
/// "新建一套配置"就是想立刻用它。cc-switch 导入也走这里（导入即卡片且立即生效）
pub(crate) fn upsert_new_profile(config: &mut AppConfig, mut profile: EndpointProfile) {
    profile.id = new_profile_id();
    // 专属凭据槽：新建（或导入）的档案若继承的是顶层共享槽（aglab/api-key.default
    // 这类），就地改道本档案的专属槽——一把 key 伺候所有服务商必然互相踩：换一个站
    // 的 key 就顶掉上一个站的（2026-10-01 事故：OAuth 令牌顶掉共享槽里的 key，
    // 全部档案一起 401）。已有档案不动，只改新铸的这张
    if profile.credential_service == config.credential_service
        && profile.credential_user == config.credential_user
    {
        profile.credential_service = format!("aglab/profile-{}", profile.id);
        profile.credential_user = "default".to_string();
    }
    apply_profile_to_config(config, &profile);
    config.active_profile_id = profile.id.clone();
    config.profiles.push(profile);
}

/// 更新一张已有档案的全部连接域字段与名字。
/// 目标是当前档案时，顶层同步抄写——编辑"使用中"的卡片等于改当前连接；
/// 目标不是当前档案时顶层一字不动，别的卡片不跟着抖
fn write_profile_fields(config: &mut AppConfig, draft: &EndpointProfile) -> Result<(), String> {
    let Some(index) = config
        .profiles
        .iter()
        .position(|profile| profile.id == draft.id)
    else {
        return Err("档案不存在或已被删除。".into());
    };
    let was_active = config.active_profile_id == draft.id;
    let mut profile = draft.clone();
    profile.id = draft.id.clone();
    config.profiles[index] = profile.clone();
    if was_active {
        apply_profile_to_config(config, &profile);
    }
    Ok(())
}

/// 把一份可选的密钥写进指定凭据目标。密钥只在这里过手：
/// 直接进凭据管理器，不回前端、不进配置文件
fn write_profile_secret(
    draft: &EndpointProfile,
    secret: Option<String>,
) -> Result<(), String> {
    let Some(secret) = secret.filter(|secret| !secret.trim().is_empty()) else {
        return Ok(());
    };
    keyring::Entry::new(&draft.credential_service, &draft.credential_user)
        .map_err(|e| format!("凭据条目初始化失败：{e}"))?
        .set_password(secret.trim())
        .map_err(|e| format!("写入凭据失败：{e}"))
}

/// 新建一张档案并立即生效。草稿的 id 字段被忽略（后端铸造）
#[tauri::command]
pub fn profile_create(
    app: AppHandle,
    name: String,
    draft: EndpointProfile,
    secret: Option<String>,
) -> Result<AppConfig, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("档案名称不能为空。".into());
    }
    let mut config = load(&app);
    // 弹窗草稿是权威状态：名字与 id 之外的全部字段以草稿为准
    let profile = EndpointProfile {
        id: String::new(),
        name: name.clone(),
        ..draft.clone()
    };
    upsert_new_profile(&mut config, profile);
    // 密钥最后写：upsert 可能已把继承的共享槽改道成本档案的专属槽，
    // 先写就落进共享槽顶掉别的站在用的 key。刚 push 的那张就是本档案
    if let Some(stored) = config.profiles.last() {
        write_profile_secret(stored, secret)?;
    }
    normalize(&mut config);
    save(&app, &config)?;
    Ok(config)
}

/// 保存对一张已有档案的编辑。目标是当前档案时顶层跟着变（含凭据对），
/// 所以编辑"使用中"的卡片后前端要重探密钥、按可能换了的目标重拉模型
#[tauri::command]
pub fn profile_update(
    app: AppHandle,
    draft: EndpointProfile,
    secret: Option<String>,
) -> Result<AppConfig, String> {
    let mut config = load(&app);
    write_profile_fields(&mut config, &draft)?;
    write_profile_secret(&draft, secret)?;
    normalize(&mut config);
    save(&app, &config)?;
    Ok(config)
}

/// 整体切换到某张档案：档案字段抄进顶层，密钥凭据对跟着换。
/// 切换后前端要重探一次密钥可用性（凭据目标已经变了）
#[tauri::command]
pub fn profile_switch(app: AppHandle, id: String) -> Result<AppConfig, String> {
    let mut config = load(&app);
    let profile = config
        .profiles
        .iter()
        .find(|profile| profile.id == id)
        .cloned()
        .ok_or_else(|| "档案不存在或已被删除。".to_string())?;
    apply_profile_to_config(&mut config, &profile);
    normalize(&mut config);
    save(&app, &config)?;
    Ok(config)
}

/// 只改档案名，不动任何连接字段——重命名不该有副作用
#[tauri::command]
pub fn profile_rename(app: AppHandle, id: String, name: String) -> Result<AppConfig, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("档案名称不能为空。".into());
    }
    let mut config = load(&app);
    let profile = config
        .profiles
        .iter_mut()
        .find(|profile| profile.id == id)
        .ok_or_else(|| "档案不存在或已被删除。".to_string())?;
    profile.name = name;
    save(&app, &config)?;
    Ok(config)
}

/// 删除一张档案。删的是当前档案时，顶层字段原样保留生效——
/// 用户只是不想要这张卡片了，不是想换掉正在用的连接
#[tauri::command]
pub fn profile_delete(app: AppHandle, id: String) -> Result<AppConfig, String> {
    let mut config = load(&app);
    if !config.profiles.iter().any(|profile| profile.id == id) {
        return Err("档案不存在或已被删除。".into());
    }
    config.profiles.retain(|profile| profile.id != id);
    if config.active_profile_id == id {
        config.active_profile_id = String::new();
    }
    save(&app, &config)?;
    Ok(config)
}

#[tauri::command]
pub fn project_add(app: AppHandle, name: String, path: String) -> Result<AppConfig, String> {
    let mut config = load(&app);
    let name = name.trim().to_string();
    let path = path.trim().to_string();

    if name.is_empty() {
        return Err("项目名不能为空。".into());
    }
    if !std::path::Path::new(&path).is_dir() {
        return Err("还没有选择源文件夹。".into());
    }

    // 同一目录重复添加时直接复用，不生成两份
    if let Some(existing) = config.projects.iter().find(|p| p.path == path) {
        config.active_project_id = existing.id.clone();
        save(&app, &config)?;
        return Ok(config);
    }

    let project = Project {
        id: new_project_id(),
        name,
        path,
        ..Default::default()
    };
    config.active_project_id = project.id.clone();
    config.projects.push(project);
    save(&app, &config)?;

    Ok(config)
}

#[tauri::command]
pub fn project_select(app: AppHandle, id: String) -> Result<AppConfig, String> {
    let mut config = load(&app);
    if !id.is_empty() && !config.projects.iter().any(|p| p.id == id) {
        return Err("项目不存在。".into());
    }
    config.active_project_id = id;
    save(&app, &config)?;
    Ok(config)
}

#[tauri::command]
pub fn project_remove(app: AppHandle, id: String) -> Result<AppConfig, String> {
    let mut config = load(&app);
    config.projects.retain(|project| project.id != id);
    if config.active_project_id == id {
        config.active_project_id = String::new();
    }
    save(&app, &config)?;
    Ok(config)
}

#[cfg(test)]
mod retention {
    use super::AppConfig;

    /// 老 config.json 里没有这个键时该退回默认保留期。退回 0 天就是"启动即归档"，
    /// 那等于把审计的可见期变成一个没人预期的东西
    #[test]
    fn a_config_from_before_the_retention_field_keeps_the_default_window() {
        let legacy: AppConfig =
            serde_json::from_str(r#"{"baseUrl":"https://x/v1","model":"m"}"#).expect("旧配置该读得动");
        assert_eq!(legacy.audit_keep_days, crate::audit::RETENTION_DAYS);
        assert_eq!(
            serde_json::from_str::<AppConfig>(r#"{"auditKeepDays":7}"#).expect("新键该读得进来").audit_keep_days,
            7
        );
    }
}

/// 自动审查开关。与沙箱/快捷键同款：改配置就够，不需要 OS 注册
#[tauri::command]
pub fn auto_review_set(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    let mut config = load(&app);
    config.auto_review = enabled;
    save(&app, &config)
}

/// 删除保护的执行侧开关（design-security-center.md D1）：存盘 + 落进程级开关，
/// 两处一次做完——只存盘不落开关，界面上的切换对进行中的会话不生效
#[tauri::command]
pub fn delete_to_trash_set(app: tauri::AppHandle, to_trash: bool) -> Result<(), String> {
    crate::tools::set_delete_to_trash(to_trash);
    let mut config = load(&app);
    config.delete_to_trash = to_trash;
    save(&app, &config)
}

/// 敏感检测规则库的界面数据（design-security-center.md D6）：id/名称/类别，
/// 设置页按它渲染清单与开关；开关写 `disabled_secret_rules`
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretRuleView {
    pub id: &'static str,
    pub label: &'static str,
    pub kind: &'static str,
    /// 这条规则吃"关键词预筛"闸（先提示词后正则）。界面上作为检测能力标签展示
    pub hint_gated: bool,
    /// 当前生效的正则（可能被 config 里的改写覆盖）。弹窗编辑从它开始
    pub pattern: String,
}

#[tauri::command]
pub fn secret_rules_list(app: tauri::AppHandle) -> Vec<SecretRuleView> {
    let edits = load(&app).secret_rule_pattern_edits;
    crate::secrets::RULES
        .iter()
        .map(|rule| {
            let edited = edits.iter().find(|edit| edit.id == rule.id);
            SecretRuleView {
                id: rule.id,
                label: rule.label,
                kind: rule.kind,
                hint_gated: rule.needs_hint && edited.is_none(),
                pattern: edited
                    .map(|edit| edit.pattern.clone())
                    .unwrap_or_else(|| rule.pattern.to_string()),
            }
        })
        .collect()
}

/// 自定义 MCP 总开关（design-security-center.md D7）：只存盘——声明与连接都
/// 现读配置，没有执行侧进程状态要落
#[tauri::command]
pub fn user_mcp_enabled_set(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    let mut config = load(&app);
    config.user_mcp_enabled = enabled;
    save(&app, &config)
}

/// 敏感保护的执行侧开关（design-security-center.md D6）：存盘 + 落进程级开关一次做完
#[tauri::command]
pub fn secret_scan_set(
    app: tauri::AppHandle,
    enabled: bool,
    disabled_rules: Vec<String>,
    custom_rules: Vec<crate::secrets::CustomSecretRule>,
    pattern_edits: Vec<crate::secrets::SecretRulePatternEdit>,
) -> Result<(), String> {
    crate::secrets::set_scan_options(
        enabled,
        disabled_rules.clone(),
        custom_rules.clone(),
        pattern_edits.clone(),
    );
    let mut config = load(&app);
    config.secret_scan_enabled = enabled;
    config.disabled_secret_rules = disabled_rules;
    config.custom_secret_rules = custom_rules;
    config.secret_rule_pattern_edits = pattern_edits;
    save(&app, &config)
}

#[cfg(test)]
mod file_rules_order {
    use super::{AppConfig, Project};

    /// 首条命中即停的语义下，表序就是优先序：项目表必须排在全局表前面，
    /// 全局表跟在后面而不是被替换掉
    #[test]
    fn project_file_rules_are_consulted_before_the_global_ones() {
        use crate::file_rules::{FileRule, RuleAction};
        let shared = FileRule {
            pattern: "C:/shared".into(),
            read: RuleAction::Allow,
            write: RuleAction::Allow,
            delete: RuleAction::Allow,
        };
        let mut config = AppConfig::default();
        config.file_rules = vec![shared];
        config.projects = vec![Project {
            id: "p1".into(),
            name: "仓库".into(),
            path: "C:/repo".into(),
            permission_overrides: Vec::new(),
            file_rules: vec![FileRule {
                pattern: "C:/shared".into(),
                read: RuleAction::Deny,
                write: RuleAction::Deny,
                delete: RuleAction::Deny,
            }],
            command_rules: Vec::new(),
        }];
        config.active_project_id = "p1".into();
        let policy = config.active_policy();
        assert_eq!(
            policy.file_rules.first().map(|rule| rule.read),
            Some(RuleAction::Deny),
            "项目表在前：排在前面的一条说了算"
        );
        assert_eq!(policy.file_rules.len(), 2, "全局表跟在项目表后面，不是被替换掉");
    }
}
