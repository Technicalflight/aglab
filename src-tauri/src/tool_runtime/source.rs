//! 三个工具来源收成一处。
//!
//! 三条路都是真的：内置注册表、扩展（MCP）进程、技能正文。它们过去散在 `chat.rs` 的那个
//! `if via_mcp { … } else if name == "load_skill" { … } else { … }` 里，而"这一路能不能
//! 自动重试、能不能进缓存"这些问题就得在原地各答一遍——两遍就是两份真相。
//!
//! 现在每个来源只回答四件事：**归不归我管、我是不是幂等、我拿不拿得出内容指纹、怎么调**。
//! 缓存与重试的规则写在 [`run_with`] 一处，测试用一个假来源就能把它们全钉住。

use std::path::Path;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use serde_json::Value;

use super::cache::{EntryKey, ReadCache};
use crate::config::{AppConfig, McpServer};
use crate::mcp::{self, Hub};
use crate::tools;

/// 一次调用属于哪一路。它同时是给用户与模型看的那句"这是谁产出的"——
/// 过去这句话由 `annotate` 从名字和 `via_mcp` 里猜，现在它由路由给
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Builtin,
    Mcp,
    Skill,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Builtin => "本地工具",
            Kind::Mcp => "扩展",
            Kind::Skill => "技能正文",
        }
    }
}

/// 技能正文那一路认领哪个工具。写成自由函数是为了让路由次序可测：`SkillSource` 要拿着
/// `AppHandle` 才能构造，测试里没有它可造的东西
pub fn is_skill_tool(name: &str) -> bool {
    name == "load_skill"
}

/// 三路的认领次序。扩展先认（它的名字带服务器前缀，撞不上内置），再技能，最后内置。
/// 内置兜底不是为了假装什么都能跑：`tools::execute` 会老实报"没有名为 X 的工具"，
/// 那比在这里编一个第三种错误要好
pub fn route(mcp_owns: bool, name: &str) -> Kind {
    if mcp_owns {
        Kind::Mcp
    } else if is_skill_tool(name) {
        Kind::Skill
    } else {
        Kind::Builtin
    }
}

/// 一次失败属于哪一类。分类唯一的用途是回答"能不能自动再来一次"，不是给界面贴标签
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    /// 请求没送到执行体：进程起不来、连不上。这一类**一个副作用都没发生**，重试是安全的
    Transport,
    /// 执行体答复了，但那是个失败：目录不存在、参数不对、扩展自己报的错
    Content,
    /// 这个名字背后根本没人：路由不认识、被关掉了、不在服务器当前清单里
    Absent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolError {
    pub kind: Failure,
    pub text: String,
}

impl ToolError {
    fn of(kind: Failure, text: impl Into<String>) -> Self {
        Self { kind, text: text.into() }
    }
    pub fn transport(text: impl Into<String>) -> Self {
        Self::of(Failure::Transport, text)
    }
    pub fn content(text: impl Into<String>) -> Self {
        Self::of(Failure::Content, text)
    }
    pub fn absent(text: impl Into<String>) -> Self {
        Self::of(Failure::Absent, text)
    }

    /// 能不能自动再来一次。只有"没送到"那一类可以——送到了而失败，重试就是再给一次副作用的机会
    pub fn retryable(&self) -> bool {
        self.kind == Failure::Transport
    }
}

/// 正文照旧：调用方（与模型看到的）那句话的文本没因为分类而变样
impl std::fmt::Display for ToolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.text)
    }
}

/// 一个来源要回答的那几件事
pub trait ToolSource {
    fn kind(&self) -> Kind;
    fn owns(&self, name: &str) -> bool;
    /// 幂等门闩。缓存只看它；默认 `false`——不确定就不许自动重来
    fn idempotent(&self, _name: &str) -> bool {
        false
    }
    /// 内容指纹。`None` 的意思是"我判断不出它变过没有"，于是这一路干脆不进缓存：
    /// 没有指纹的缓存会在内容改过之后继续交旧的那一版，而模型看不出来它读的是哪一版
    fn stamp(&self, _name: &str, _args: &Value) -> Option<String> {
        None
    }
    fn call(&self, name: &str, args: &Value) -> Result<String, ToolError>;
}

/// 内置注册表那一路
pub struct BuiltinSource<'a> {
    pub root: Option<&'a Path>,
    /// 发起调用的话题 id。它只往一处走：后台命令句柄认主人（run_command 的
    /// background 分支）——面板按话题清点"后台指令"靠这一格
    pub owner: Option<&'a str>,
}

impl ToolSource for BuiltinSource<'_> {
    fn kind(&self) -> Kind {
        Kind::Builtin
    }

    /// `load_skill` 的**声明**在内置注册表里（模型从首轮就看得见它），**执行**却归技能那一路。
    /// 把声明与执行混成一个 source 就会漏掉它——所以这里明确把它让出去
    fn owns(&self, name: &str) -> bool {
        tools::is_registered(name) && !is_skill_tool(name)
    }

    fn idempotent(&self, name: &str) -> bool {
        tools::is_idempotent(name)
    }

    fn stamp(&self, name: &str, args: &Value) -> Option<String> {
        tools::content_stamp(name, args, self.root)
    }

    fn call(&self, name: &str, args: &Value) -> Result<String, ToolError> {
        tools::execute_for(name, args, self.root, self.owner).map_err(ToolError::content)
    }
}

/// 扩展（MCP）那一路
pub struct McpSource<'a> {
    pub servers: &'a [McpServer],
    pub config: &'a AppConfig,
    pub hub: &'a Hub,
}

impl ToolSource for McpSource<'_> {
    fn kind(&self) -> Kind {
        Kind::Mcp
    }

    fn owns(&self, name: &str) -> bool {
        mcp::owns(self.servers, name)
    }

    /// 扩展工具没声明过自己只读（协议里那个 `readOnlyHint` 我们没接），所以它不进缓存：
    /// 拿一个不知道有没有副作用的东西去做自动重放，那不是优化
    fn call(&self, name: &str, args: &Value) -> Result<String, ToolError> {
        match mcp::call(self.servers, self.config, self.hub, name, args.clone()) {
            None => Err(ToolError::absent("MCP 工具路由失败。")),
            Some(Ok(text)) => Ok(text),
            Some(Err(text)) => Err(mcp_failure(text)),
        }
    }
}

/// 那两句记号各自属于哪一类失败。记号由 `mcp.rs` 用同一个常量拼出来，所以这里不是在猜文本，
/// 是在认它自己埋的记号。剥成自由函数是因为这三格的分类**没有别的地方能测**：
/// `McpSource::call` 要拿着服务器列表与 Hub 才走得动
pub fn mcp_failure(text: String) -> ToolError {
    if text.starts_with(mcp::TRANSPORT_MARK) {
        // 请求没送到执行体：一个副作用都没发生，自动再来一次是安全的
        ToolError::transport(text)
    } else if text.starts_with(mcp::AMBIGUOUS_MARK) {
        // 撞车时没有任何程序收到这一发：它是"没人接"，不是"接了并答了失败"
        ToolError::absent(text)
    } else {
        // 执行体答了，答的是失败——重放就是再给它一次动手的机会
        ToolError::content(text)
    }
}

/// 技能正文那一路
pub struct SkillSource<'a> {
    pub app: &'a tauri::AppHandle,
}

impl ToolSource for SkillSource<'_> {
    fn kind(&self) -> Kind {
        Kind::Skill
    }

    fn owns(&self, name: &str) -> bool {
        is_skill_tool(name)
    }

    /// 读正文不改任何东西——把名单收窄的那一步在调用方（`note_skill`），不在这条路里，
    /// 所以重试它不会重复任何副作用
    fn idempotent(&self, _name: &str) -> bool {
        true
    }

    /// 正文来自每次现扫磁盘，没有一个便宜的指纹可拿：于是它只可重试，不进缓存。
    /// 这条不是偷懒——缓存它就得回答"技能文件改了我怎么知道"，而现在答不上
    fn call(&self, _name: &str, args: &Value) -> Result<String, ToolError> {
        crate::skills::load_body(self.app, args["name"].as_str().unwrap_or_default())
            .map_err(ToolError::content)
    }
}

/// 重试的手感。`calls` 是总共允许几次调用（一次原样 + 两次重试），间隔按次数拉长
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Retry {
    pub calls: u8,
    pub backoff_ms: u64,
}

impl Default for Retry {
    fn default() -> Self {
        Self { calls: 3, backoff_ms: 200 }
    }
}

/// 一次执行的经过。`attempts` 为 0 表示根本没调用过（命中缓存）
#[derive(Debug)]
pub struct Executed {
    pub output: Result<String, ToolError>,
    pub source: Kind,
    pub cached: bool,
    pub attempts: u8,
}

impl Executed {
    /// 给用户与模型看的那句补充。只在真的发生过的时候才说：
    /// "缓存命中"与"这是第三次才连上"都是要能看见的事实，不是内部指标
    pub fn note(&self) -> Option<String> {
        if self.cached {
            return Some("命中话题内缓存，没有重新读盘。".to_string());
        }
        if self.attempts > 1 {
            return Some(format!("第 {} 次才送达，前面传输失败过。", self.attempts));
        }
        None
    }
}

/// 缓存 → 调用 → （只在传输性失败上）重试 → 回填。规则只在这里写一遍
pub fn run_with(
    source: &dyn ToolSource,
    cache: &ReadCache,
    retry: Retry,
    name: &str,
    args: &Value,
) -> Executed {
    let kind = source.kind();
    let key = EntryKey::of(
        name,
        &args.to_string(),
        source.idempotent(name),
        source.stamp(name, args),
    );
    if let Some(held) = &key {
        if let Some(text) = cache.get(held) {
            return Executed { output: Ok(text), source: kind, cached: true, attempts: 0 };
        }
    }

    let mut attempts = 1u8;
    loop {
        match source.call(name, args) {
            Ok(text) => {
                if let Some(held) = key {
                    cache.put(held, &text);
                }
                break Executed { output: Ok(text), source: kind, cached: false, attempts };
            }
            Err(error) => {
                if error.retryable() && attempts < retry.calls {
                    attempts += 1;
                    // 重试这件事要留下总数：界面上那句"第几次才送达"是当次的，
                    // Inspector 上要回答的是"这一整轮里它发生过几次"
                    cache.record_retry();
                    if retry.backoff_ms > 0 {
                        thread::sleep(Duration::from_millis(
                            retry.backoff_ms * u64::from(attempts - 1),
                        ));
                    }
                    continue;
                }
                break Executed { output: Err(error), source: kind, cached: false, attempts };
            }
        }
    }
}

/// 三条路摆在一张表上，`chat.rs` 只跟它打交道
pub struct Registry<'a> {
    pub builtin: BuiltinSource<'a>,
    pub mcp: McpSource<'a>,
    pub skill: SkillSource<'a>,
    pub retry: Retry,
}

impl<'a> Registry<'a> {
    pub fn new(
        root: Option<&'a Path>,
        servers: &'a [McpServer],
        config: &'a AppConfig,
        hub: &'a Hub,
        app: &'a tauri::AppHandle,
        conversation_id: &'a str,
    ) -> Self {
        Self {
            builtin: BuiltinSource { root, owner: Some(conversation_id) },
            mcp: McpSource { servers, config, hub },
            skill: SkillSource { app },
            retry: Retry::default(),
        }
    }

    pub fn kind_of(&self, name: &str) -> Kind {
        route(self.mcp.owns(name), name)
    }

    fn source(&self, name: &str) -> &dyn ToolSource {
        match self.kind_of(name) {
            Kind::Mcp => &self.mcp,
            Kind::Skill => &self.skill,
            Kind::Builtin => &self.builtin,
        }
    }

    pub fn run(&self, cache: &ReadCache, name: &str, args: &Value) -> Executed {
        run_with(self.source(name), cache, self.retry, name, args)
    }
}

/// 进程内那一份缓存。它不跨进程：§2.3 明写不做跨进程的"永久允许"，缓存同理
pub fn shared_cache() -> &'static ReadCache {
    static CACHE: OnceLock<ReadCache> = OnceLock::new();
    CACHE.get_or_init(ReadCache::new)
}

/// 这一轮**候选**的工具声明，分两本账交给界面：`tools` 是本地那批（含技能入口），
/// `mcp` 是扩展那批。真正发出去的是 [`Declarations::ordered`]
#[derive(Debug, Clone, Default)]
pub struct Declarations {
    pub tools: Vec<Value>,
    pub mcp: Vec<Value>,
}

impl Declarations {
    /// 发给模型的那一份：顺序就是字节的顺序，而服务商的 prompt cache 认的是前缀。
    /// 所以这条拼接次序是承重的，改动它等于让整段声明重付
    pub fn ordered(&self) -> Vec<Value> {
        self.tools
            .iter()
            .cloned()
            .chain(self.mcp.iter().cloned())
            .collect()
    }

    /// 声明里的工具名，按发出去的次序。给测试与 Inspector 用，不参与发送。
    /// 生产侧 today 没有"按名字查声明"的调用点（筛选走 `narrow_to_allowlist`）
    #[cfg(test)]
    pub fn names(&self) -> Vec<String> {
        self.ordered()
            .iter()
            .filter_map(|item| item["function"]["name"].as_str().map(str::to_string))
            .collect()
    }

    /// 按这份话题的白名单原地收窄。**判据不另写一份**：用的就是闸门那一条
    /// [`crate::tool_runtime::allowlist_violation`]，所以"声明里出现了"与"调得动"永远不会分家。
    ///
    /// 一份看得见却调不动的工具表不是收窄，是诱着模型去撞一次拒绝：编排节点里那个
    /// `tools: []` 的监督者，此前每一发都付着全部工具的字节
    fn keep_visible(&mut self, allowed: Option<&[String]>) {
        let visible = |item: &Value| match item["function"]["name"].as_str() {
            // 读不出名字的条目不判：它不会被按名字调到，留着比丢掉安全
            Some(name) => crate::tool_runtime::allowlist_violation(name, allowed).is_none(),
            None => true,
        };
        self.tools.retain(visible);
        self.mcp.retain(visible);
    }
}

/// 三路声明的唯一装配点。**为什么是自由函数而不是 `ToolSource` 的一个方法**：
/// 声明侧不需要 `AppHandle`（技能那一条是静态声明），把它挂在来源上就等于把一条
/// 本来可测的规则做成"要起着应用才测得了"。
///
/// 它存在的理由是另一件事：Inspector 的字节账与实际发出去的那份数组过去在 `chat.rs`
/// 各拼一遍，分两处写就是两份真相——"上下文里那些工具占多少字节"可以量的是另一份数组
pub fn declarations(
    project_bound: bool,
    disabled: &[String],
    has_skills: bool,
    browser_enabled: bool,
    web_search_enabled: bool,
    mcp: Vec<Value>,
    spawnable: &[(String, String)],
    allowed: Option<&[String]>,
) -> Declarations {
    // 工具只有在有活动项目时才声明：文件路径以项目根为基准，"选择项目"因此是承重控件而不是标签
    let mut local = match project_bound {
        true => match tools::schemas_for(disabled) {
            Value::Array(items) => items,
            _ => Vec::new(),
        },
        // 没有路径基准，文件与命令那几条不声明。但 Computer Use 那三条不碰路径，
        // 它们跟着技能入口一起留着——没绑工作目录不等于不能看窗口
        false => tools::computer_schemas_for(disabled),
    };
    // 没装技能就不给 `load_skill` 入口：声明一个必然失败的工具，模型第一次调用就撞上"没有技能"
    if has_skills {
        local.push(tools::skill_schema());
    }
    // 读网页与工作目录无关（web 没有路径基准），跟技能入口一样单独声明：没绑项目也能查资料。
    // 执行在 chat 循环路由外（它要配置里的出口名单与代理），关掉就不声明
    if !tools::is_disabled(disabled, "web_fetch") {
        local.push(tools::web_fetch_schema());
    }
    // 联网搜索同款：与工作目录无关，但配置说了算（供应商 + key 没配就整条不声明），
    // 执行在 chat 循环路由外（它要配置里的 key、出口名单与代理）
    if web_search_enabled && !tools::is_disabled(disabled, "web_search") {
        local.push(tools::web_search_schema());
    }
    // 内置浏览器与工作目录无关，但扩展开关说了算：关着整条不声明——
    // 声明一个必然失败的工具，等于诱着模型去撞一次拒绝（spawn 同款）
    if browser_enabled && !tools::is_disabled(disabled, "browser") {
        local.push(tools::browser_schema());
    }
    // 资料库检索与工作目录无关（读的是资料库自己的存储），单独声明。
    // 空库也声明：空库返回的是"先去资料库页添加文档"的可操作回答，
    // 不是 load_skill-无技能那种怎么调都失败——不满足整条不声明的判据
    if !tools::is_disabled(disabled, "knowledge_search") {
        local.push(tools::knowledge_schema());
    }
    // 观察召回同款：不依赖工作目录，也不依赖任何扩展开关——句柄只在话题内存里，
    // 恒声明不花钱，模型用不用取决于有没有遇到被归档的结果
    if !tools::is_disabled(disabled, "obs_recall") {
        local.push(tools::obs_recall_schema());
    }
    // 目标上报同款：不依赖工作目录，也不是一条能力。**恒声明**，不按"现在是不是目标模式"加减
    // ——声明数组在首轮就定形（§6.2），中途换数组等于把整段前缀换掉，后面每一行都失配
    local.push(tools::goal_report_schema());
    // 计划更新与向用户提问同款：控制信号，不是能力。**恒声明**——同一份声明数组
    // 首轮定形，模型用不用它们不取决于有没有这个入口
    local.push(tools::plan_schema());
    local.push(tools::ask_user_schema());
    // 派单那条：schemas() 里是形状底稿，真名单在这里按「主模型可调」写进描述与 enum。
    // 目录为空就整条不声明——声明一个必然失败的工具，等于诱着模型去撞一次拒绝
    if let Some(at) = local
        .iter()
        .position(|item| item["function"]["name"].as_str() == Some("spawn_subagent"))
    {
        if spawnable.is_empty() {
            local.remove(at);
        } else {
            let names: Vec<&str> = spawnable.iter().map(|(name, _)| name.as_str()).collect();
            let catalog = spawnable
                .iter()
                .map(|(name, description)| format!("- {name}：{description}"))
                .collect::<Vec<_>>()
                .join("\n");
            local[at]["function"]["description"] = serde_json::json!(format!(
                "把一项独立的小任务交给一个子助理，等它跑完并交回结论。可派的子助理：\n{catalog}"
            ));
            local[at]["function"]["parameters"]["properties"]["name"]["enum"] =
                serde_json::json!(names);
        }
    }
    let mut declared = Declarations { tools: local, mcp };
    declared.keep_visible(allowed);
    declared
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 一个假来源：想让它失败几次就失败几次，调用次数由它自己数
    struct Fake {
        kind: Kind,
        idempotent: bool,
        stamp: Option<String>,
        failures: usize,
        calls: AtomicUsize,
    }

    impl Fake {
        fn new(kind: Kind, idempotent: bool, stamp: Option<String>, failures: usize) -> Self {
            Self { kind, idempotent, stamp, failures, calls: AtomicUsize::new(0) }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::Acquire)
        }
    }

    impl ToolSource for Fake {
        fn kind(&self) -> Kind {
            self.kind
        }
        fn owns(&self, _name: &str) -> bool {
            true
        }
        fn idempotent(&self, _name: &str) -> bool {
            self.idempotent
        }
        fn stamp(&self, _name: &str, _args: &Value) -> Option<String> {
            self.stamp.clone()
        }
        fn call(&self, _name: &str, _args: &Value) -> Result<String, ToolError> {
            let at = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
            if at <= self.failures {
                Err(ToolError::transport("连不上"))
            } else {
                Ok(format!("第 {at} 次调用的正文"))
            }
        }
    }

    fn immediate() -> Retry {
        Retry { calls: 3, backoff_ms: 0 }
    }

    #[test]
    fn the_route_is_extension_then_skill_then_builtin() {
        assert_eq!(route(true, "load_skill"), Kind::Mcp, "带服务器前缀的先归扩展");
        assert_eq!(route(false, "load_skill"), Kind::Skill);
        assert_eq!(route(false, "read_file"), Kind::Builtin);
        // 名字谁都不认时不要在这里编错误：让内置去报"没有名为 X 的工具"
        assert_eq!(route(false, "不存在的工具"), Kind::Builtin);
    }

    #[test]
    fn load_skill_is_declared_by_builtin_but_not_executed_by_it() {
        // T01 的变异对照：谁要是把 load_skill 划回内置那一路执行，这两条立刻红
        let builtin = BuiltinSource { root: None, owner: None };
        assert!(tools::is_registered("load_skill"), "它得在注册表里，否则模型看不见它");
        assert!(!builtin.owns("load_skill"), "声明归内置，执行归技能");
        let error = builtin.call("load_skill", &json!({ "name": "x" }));
        assert!(error.is_err(), "内置执行器不认识它——划错路就是把这个错误送进上下文");
    }

    #[test]
    fn only_a_transport_failure_is_retried_and_the_cap_is_two_retries() {
        let cache = ReadCache::new();
        let source = Fake::new(Kind::Builtin, true, None, 2);
        let held = run_with(&source, &cache, immediate(), "read_file", &json!({ "path": "a" }));
        assert!(held.output.is_ok());
        assert_eq!(held.attempts, 3, "一次原样加两次重试就到顶（T06：重试上限 2）");
        assert_eq!(source.calls(), 3);
        assert_eq!(cache.stats().retries, 2, "两次重试各记一次，不是一句'重试过'就算");
        assert_eq!(held.note().as_deref(), Some("第 3 次才送达，前面传输失败过。"));
    }

    #[test]
    fn retrying_stops_at_the_cap_and_hands_back_the_original_error() {
        let cache = ReadCache::new();
        let source = Fake::new(Kind::Mcp, false, None, 5);
        let held = run_with(&source, &cache, immediate(), "mcp__x__do", &json!({}));
        assert!(matches!(&held.output, Err(error) if error.retryable()));
        assert_eq!(held.attempts, 3, "越不过那三次，多一次都不许");
        assert_eq!(source.calls(), 3, "到顶就停手：重试不是退避循环");
    }

    #[test]
    fn an_idempotent_read_with_a_stamp_is_served_from_cache_the_second_time() {
        let cache = ReadCache::new();
        let source = Fake::new(Kind::Builtin, true, Some("a@1".into()), 0);
        let first = run_with(&source, &cache, immediate(), "read_file", &json!({ "path": "a" }));
        assert!(!first.cached);
        let second = run_with(&source, &cache, immediate(), "read_file", &json!({ "path": "a" }));
        assert!(second.cached, "同一份指纹第二次不该再读盘");
        assert_eq!(second.output.as_deref().unwrap(), "第 1 次调用的正文");
        assert_eq!(second.attempts, 0, "命中缓存就是没调用过");
        assert_eq!(source.calls(), 1, "一共只调了一次");
        assert_eq!(cache.stats().hits, 1, "命中数要能报得出来，Inspector 读的就是它");
    }

    #[test]
    fn a_write_tool_and_an_unstamped_read_never_enter_the_cache() {
        let cache = ReadCache::new();
        // 写类：连键都不该被构造出来
        let write = Fake::new(Kind::Builtin, false, Some("a@1".into()), 0);
        run_with(&write, &cache, immediate(), "write_file", &json!({ "path": "a" }));
        assert_eq!(cache.stats().entries, 0, "写类工具永不出现在缓存里");

        // 幂等但没指纹：同样不进
        let bare = Fake::new(Kind::Skill, true, None, 0);
        let registry_cache = ReadCache::new();
        run_with(&bare, &registry_cache, immediate(), "load_skill", &json!({ "name": "x" }));
        assert_eq!(registry_cache.stats().entries, 0, "没有指纹就不该赌它没变过");
        let again = run_with(&bare, &registry_cache, immediate(), "load_skill", &json!({ "name": "x" }));
        assert!(!again.cached);
        assert_eq!(bare.calls(), 2, "它每次都老老实实重读，这是对的");
    }

    #[test]
    fn a_note_is_silent_when_neither_cache_nor_retry_happened() {
        let cache = ReadCache::new();
        let source = Fake::new(Kind::Builtin, false, None, 0);
        let held = run_with(&source, &cache, immediate(), "run_command", &json!({}));
        assert_eq!(held.note(), None, "什么事都没发生就别在结果里加一句自我说明");
    }

    fn mcp_entry(name: &str) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": "扩展给的一条",
                "parameters": { "type": "object", "properties": {} }
            }
        })
    }

    /// T01 的判据：三路拼出来的那份数组，内容与次序都要与"三路各自声明的那一份"逐字节相同。
    /// 次序是承重的——它决定服务商能不能命中前缀缓存，所以这条测的不只是集合相等。
    /// 目录给了一个人（spawn_subagent），它排在 computer_act 之后、技能入口之前
    #[test]
    fn the_three_sources_compose_into_one_declaration_array_in_a_fixed_order() {
        let catalog = vec![("审查员".to_string(), "对照复核".to_string())];
        let held = declarations(true, &[], true, false, false, vec![mcp_entry("mcp__a__do")], &catalog, None);
        let names = held.names();
        assert_eq!(
            names,
            vec![
                "list_files",
                "read_file",
                "search_text",
                "write_file",
                "edit_file",
                "delete_file",
                "run_command",
                "open_path",
                "command_output",
                "command_stop",
                "list_windows",
                "inspect_window",
                "computer_act",
                "spawn_subagent",
                "present_files",
                "run_program",
                "agent_control",
                "ssh_run",
                "lsp_query",
                "load_skill",
                "web_fetch",
                "knowledge_search",
                "obs_recall",
                // 目标上报恒声明：声明数组在首轮定形（§6.2），按"现在是不是目标模式"
                // 加减它就是中途换数组，后面每一行都失配
                "goal_report",
                // 计划更新与向用户提问同款：控制信号，恒声明
                "update_plan",
                "ask_user",
                "mcp__a__do"
            ],
            "内置在前、技能入口居中、web 随后、资料库再后、扩展在尾：{names:?}"
        );

        let own: Vec<Value> = match tools::schemas_for(&[]) {
            Value::Array(items) => items,
            _ => Vec::new(),
        };
        // spawn 的描述与 enum 被目录改写（设计）；present_files/run_program 排在其后；
        // agent_control/ssh_run/lsp_query 再其后——字节同一性只对"装配不该动"的前段断言，
        // 后面这几条单独认名
        assert_eq!(
            held.tools[..own.len() - 6],
            own[..own.len() - 6],
            "装配这一步不许改动任何一条声明的字节（spawn 那条除外，它由目录写形）"
        );
        assert_eq!(
            held.tools[own.len() - 6]["function"]["name"].as_str(),
            Some("spawn_subagent")
        );
        assert_eq!(
            held.tools[own.len() - 5]["function"]["name"].as_str(),
            Some("present_files")
        );
        assert_eq!(
            held.tools[own.len() - 4]["function"]["name"].as_str(),
            Some("run_program")
        );
        assert_eq!(
            held.tools[own.len() - 3]["function"]["name"].as_str(),
            Some("agent_control")
        );
        assert_eq!(
            held.tools[own.len() - 2]["function"]["name"].as_str(),
            Some("ssh_run")
        );
        assert_eq!(
            held.tools[own.len() - 1]["function"]["name"].as_str(),
            Some("lsp_query")
        );
        assert_eq!(held.tools[own.len()], tools::skill_schema(), "技能那一条就是注册表里的那个形状");
        assert_eq!(
            held.mcp,
            vec![mcp_entry("mcp__a__do")],
            "扩展那本账要单独报得出来：Inspector 问的就是它占多少字节"
        );
    }

    /// 派单那条声明由目录写形：名单进 enum 与描述，目录空就整条不声明——
    /// 声明一个必然失败的工具，等于诱着模型去撞一次拒绝
    #[test]
    fn the_spawn_declaration_is_shaped_by_the_catalog() {
        let empty = declarations(true, &[], false, false, false, vec![], &[], None);
        assert!(
            !empty.names().iter().any(|name| name == "spawn_subagent"),
            "目录空就不该有派单入口：{:?}",
            empty.names()
        );

        let catalog = vec![("审查员".to_string(), "对照复核".to_string())];
        let held = declarations(true, &[], false, false, false, vec![], &catalog, None);
        let entry = held
            .tools
            .iter()
            .find(|item| item["function"]["name"].as_str() == Some("spawn_subagent"))
            .expect("有目录就该有派单那条");
        assert_eq!(
            entry["function"]["parameters"]["properties"]["name"]["enum"],
            serde_json::json!(["审查员"]),
            "enum 就是名单：模型点名只能点这几个"
        );
        let description = entry["function"]["description"].as_str().expect("描述是字符串");
        assert!(
            description.contains("审查员") && description.contains("对照复核"),
            "描述里要带着判案用的那句：{description}"
        );
    }

    /// 话题白名单也收窄**声明**这一侧。以前它只收窄闸门：一份看得见却调不动的工具表不是收窄，
    /// 是诱着模型去撞一次拒绝，而编排节点里那个 `tools: []` 的监督者每一发都付着全部工具的字节。
    /// `None` 那一头是正对照——普通话题的声明一个字节都不该变
    #[test]
    fn a_session_allowlist_narrows_the_declarations_as_well_as_the_gate() {
        let allowed = vec!["read_file".to_string(), "mcp__a__peek".to_string()];
        let all = declarations(true, &[], true, false, false, vec![mcp_entry("mcp__a__do")], &[], None);
        let narrow = declarations(
            true,
            &[],
            true,
            false,
            false,
            vec![mcp_entry("mcp__a__do"), mcp_entry("mcp__a__peek")],
            &[],
            Some(&allowed),
        );
        assert!(all.names().contains(&"write_file".to_string()), "不收窄时写文件本来是在的");
        assert_eq!(
            narrow.names(),
            vec!["read_file".to_string(), "mcp__a__peek".to_string()],
            "白名单外的（含 `load_skill` 与另一台扩展工具）都不该出现在声明里：{:?}",
            narrow.names()
        );
        // 同一条判据：凡是这条说"不能调"的，声明里就不该有它。两侧分家就是两份真相
        for name in all.names() {
            let refused = crate::tool_runtime::allowlist_violation(&name, Some(&allowed)).is_some();
            assert_eq!(
                refused,
                !narrow.names().contains(&name),
                "声明与闸门对「{name}」给出了两个答案"
            );
        }
    }

    /// 两个门槛各管一件事：没绑项目 = 没有路径基准，没装技能 = 没有可取的正文
    #[test]
    fn an_unbound_project_and_an_empty_skill_dir_each_drop_their_own_entries() {
        let no_project = declarations(false, &[], true, false, false, vec![], &[], None);
        assert_eq!(
            no_project.names(),
            vec![
                "list_windows",
                "inspect_window",
                "computer_act",
                "agent_control",
                "load_skill",
                "web_fetch",
                "knowledge_search",
                "obs_recall",
                "goal_report",
                "update_plan",
                "ask_user"
            ],
            "没绑项目就没有路径基准，文件工具一条都不该声明；但 Computer Use 不碰路径、web 与资料库没有路径基准、观察召回与目标上报也不是一项能力，它们和技能一样留着"
        );
        // 关掉其中一条，没项目时也一样不再声明
        assert!(
            !declarations(false, &["computer_act".to_string()], true, false, false, vec![], &[], None)
                .names()
                .iter()
                .any(|name| name == "computer_act"),
            "「关掉就不再声明」这条不能只在有项目时成立"
        );
        let no_skills = declarations(true, &[], false, false, false, vec![], &[], None);
        // 十八条工作目录声明 - spawn（可派名单空被摘）+ web_fetch + knowledge_search + 观察召回 + goal_report + 计划更新 + 向用户提问 + 删除 = 24
        assert_eq!(
            no_skills.tools.len(),
            24,
            "内置与技能无关，web_fetch、资料库检索与目标上报也不依赖技能，它们该留着"
        );
        assert!(
            !no_skills.names().iter().any(|name| name == "load_skill"),
            "没装技能就不该给一个必然失败的工具入口"
        );
    }

    /// browser 的声明只认扩展开关：开着就有（不依赖工作目录），关着或逐工具
    /// 停用就整条消失——声明一个必然失败的工具，等于诱着模型去撞拒绝
    #[test]
    fn the_browser_tool_is_declared_only_when_the_switch_is_on() {
        let with = declarations(false, &[], false, true, false, vec![], &[], None);
        assert!(
            with.names().iter().any(|name| name == "browser"),
            "扩展开着：没绑项目也该声明 browser"
        );
        let without = declarations(false, &[], false, false, false, vec![], &[], None);
        assert!(
            !without.names().iter().any(|name| name == "browser"),
            "扩展关着：browser 整条不声明"
        );
        let per_tool = declarations(
            false,
            &["browser".to_string()],
            false,
            true,
            false,
            vec![],
            &[],
            None,
        );
        assert!(
            !per_tool.names().iter().any(|name| name == "browser"),
            "扩展开着但逐工具停用：照旧不声明，两道开关叠加"
        );
    }

    #[test]
    fn a_disabled_builtin_is_absent_from_the_declaration_array() {
        // 关掉的能力不再声明：模型看不见它，也就不会去调它
        let held = declarations(true, &["run_command".to_string()], false, false, false, vec![], &[], None);
        assert!(!held.names().iter().any(|name| name == "run_command"));
        // 十八条 - run_command - spawn（名单空）+ web_fetch + knowledge_search + 观察召回 + goal_report + 计划更新 + 向用户提问 + 删除 = 23
        assert_eq!(held.tools.len(), 23);
    }

    /// 扩展报回来的三种记号各归哪一类。撞车那一格是新近才有的：它**没有任何执行体
    /// 收到这一发**，所以是"没人接"而不是"接了并答了失败"。两类都不许自动重放，
    /// 但报给模型与界面的口径不一样，分错了就是替它编一个"程序答过"的假故事
    #[test]
    fn the_three_marks_from_the_extension_side_split_into_their_own_kinds() {
        for (mark, want) in [
            (mcp::TRANSPORT_MARK, Failure::Transport),
            (mcp::AMBIGUOUS_MARK, Failure::Absent),
            ("这个工具不在服务器当前声明的清单里。", Failure::Content),
        ] {
            let held = mcp_failure(format!("{mark}细节"));
            assert_eq!(held.kind, want, "记号「{mark}」分错了类");
            assert_eq!(
                held.retryable(),
                want == Failure::Transport,
                "只有没送出去的那一类可以自动再来一次"
            );
        }
    }
}
