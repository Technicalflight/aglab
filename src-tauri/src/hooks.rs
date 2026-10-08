//! 插件钩子：在回合的几个固定节点上插入一段插件自带的脚本。
//!
//! 结构照搬参照实现的三级形状（事件 → 匹配组 → 处理器），插件作者手里的
//! hooks.json 就是这一份，换个读法它就读不到了。
//!
//! 但门槛比参照实现严。钩子是别人写的、拥有本进程全部文件与网络权限的可执行代码，
//! 而 aglab 是装在用户自己机器上的客户端，所以一条钩子要同时满足
//! 「所属插件启用 + 事件在本客户端有落点 + 用户确认过当前这份定义的指纹 + 没被单独关掉」
//! 才会被执行。指纹差一个字节，确认就作废。

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as OsCommand, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};

use crate::config::{AppConfig, TrustedHook};
use crate::plugins::{self, Plugin};

/// 本客户端真正会触发的事件。其余事件照样列出来并标成"没有落点"，
/// 而不是悄悄跳过：用户以为护栏挂上了、其实一直没有，比明确不支持危险得多。
/// 三个话题级事件（SessionStart/SessionEnd/PreCompact）在应用启动/退出与压缩前各有一个落点。
/// PermissionRequest 站在审批闸前投票（deny/allow/ask）；PostToolUseFailure 在
/// 工具执行失败后转达检查意见——副作用已经发生，它的deny与PostToolUse同义，只能转达
pub const SUPPORTED_EVENTS: [&str; 9] = [
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PostToolUseFailure",
    "Stop",
    "SessionStart",
    "SessionEnd",
    "PreCompact",
];

/// 钩子的传输类型：stdio 是别人写的本机脚本，http 是一拳一个 JSON 的远端，
/// sse 是"通知一声就走"的单向事件出口——它天生 fire-and-forget，写 async 没有意义
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookTransport {
    Stdio,
    Http,
    Sse,
}

impl HookTransport {
    pub fn as_str(&self) -> &'static str {
        match self {
            HookTransport::Stdio => "stdio",
            HookTransport::Http => "http",
            HookTransport::Sse => "sse",
        }
    }
}

const DEFAULT_TIMEOUT: u64 = 15;
/// 界面就在前台等着，不能照抄参照实现那个 600 秒默认值
const MAX_TIMEOUT: u64 = 60;
const MAX_HOOK_OUTPUT: u64 = 8 * 1024;
const READ_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct Hook {
    /// 位置标识：插件目录名 + 哪份文件 + 事件 + 文件内序号。
    /// 内容改了它不变，所以它只能用来定位，不能用来判断"还是原来那条钩子"
    pub id: String,
    pub event: String,
    pub matcher: Option<String>,
    pub command: String,
    /// 传输类型（hooks.json 的 handler.type：command→stdio，http→http，sse→sse）
    pub transport: HookTransport,
    /// http/sse 的目标地址。stdio 型不读这一格
    pub url: Option<String>,
    pub timeout: u64,
    /// 发射后不管：异步钩子不占回合的等待时间，它的输出与退出码都被丢弃。
    /// 拦截类事件（PreToolUse/Stop）写 async 等于自己把护栏拆了——解析侧照收，
    /// 语义侧它永远拦不住，因为没人在等它。sse 型天生异步，这一格对它没有意义
    pub async_flag: bool,
    pub status_message: String,
    pub plugin_id: String,
    pub plugin_dir: PathBuf,
    pub file: PathBuf,
    /// 定义指纹：确认信任时记下的就是它
    pub hash: String,
}

impl Hook {
    pub fn supported(&self) -> bool {
        SUPPORTED_EVENTS.contains(&self.event.as_str())
    }

    /// 界面上给这次钩子调用起的名字。它走的是现成的工具卡片，
    /// 因为"跑了一段别人写的脚本"本来就该和别的工具调用排在同一条时间线上
    pub fn card_name(&self) -> &str {
        match self.event.as_str() {
            "UserPromptSubmit" => "钩子·提交前",
            "PreToolUse" => "钩子·执行前",
            "PermissionRequest" => "钩子·权限问询",
            "PostToolUse" => "钩子·执行后",
            "PostToolUseFailure" => "钩子·执行失败",
            "Stop" => "钩子·收尾前",
            "SessionStart" => "钩子·话题开始",
            "SessionEnd" => "钩子·话题结束",
            "PreCompact" => "钩子·压缩前",
            other => other,
        }
    }

    /// 卡片上那行说明：哪个插件带的、跑的是哪条命令 / 打的是哪个地址
    pub fn card_input(&self) -> String {
        let command: String = self.command.chars().take(140).collect();
        let command = if self.command.chars().count() > 140 {
            format!("{command}…")
        } else {
            command
        };
        let target = match self.transport {
            HookTransport::Stdio => command,
            HookTransport::Http | HookTransport::Sse => {
                let url = self.url.clone().unwrap_or_default();
                let url: String = url.chars().take(140).collect();
                format!("[{}] {}", self.transport.as_str(), url)
            }
        };
        let label = if self.status_message.is_empty() {
            self.plugin_id.clone()
        } else {
            format!("{} · {}", self.plugin_id, self.status_message)
        };
        format!("{label}\n{target}")
    }

    /// 匹配组的正则只筛工具名
    fn matches_tool(&self, tool_name: &str) -> bool {
        let Some(pattern) = self.matcher.as_deref().map(str::trim) else {
            return true;
        };
        if pattern.is_empty() || pattern == "*" {
            return true;
        }
        match Regex::new(pattern) {
            // 正则写错时宁可漏跑一次，也不能退化成"全匹配"，那等于凭空放大护栏范围
            Ok(re) => re.is_match(tool_name),
            Err(_) => false,
        }
    }
}

/// 钩子说出来的话，只有五种，没有"部分同意"
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 退出码 0 且没吐决策：这个钩子对本次操作没意见
    Silent,
    /// 别做这件事 / 别收这轮尾，把原因转给模型
    Block(String),
    /// 执行前钩子的"问一句"：本次调用照常走审批，弹框让用户拍板。
    /// PostToolUse / Stop 上没有"待执行的动作"可问，落到 AddContext
    Ask(String),
    /// PermissionRequest 钩子的"放行"：这一次审批闸直接过，不再问人。
    /// 只有可信任的钩子才会走到这里（runnable 已按指纹过滤），
    /// 凭据写进审批卡的 pass_reason——悄悄放行要答得出是谁放的
    Approve(String),
    /// 往上下文里补一句。工具执行后钩子的"拒绝"也落到这里：副作用已经发生，撤不掉，只能转达
    AddContext(String),
    /// 钩子自己坏了。脚本崩溃既不是安全决策也不是放行，必须单独报出来
    Broken(String),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookView {
    pub id: String,
    pub event: String,
    pub matcher: Option<String>,
    pub command: String,
    pub timeout: u64,
    /// 发射后不管：输出与退出码都被丢弃，不占回合时间
    #[serde(rename = "async")]
    pub is_async: bool,
    pub status_message: String,
    pub file: String,
    pub hash: String,
    /// 这个事件在本客户端有落点吗
    pub supported: bool,
    /// 记下的指纹是否还和当前定义一致。trusted 为真而它为假，就是"确认过后内容被改过"
    pub current: bool,
    pub trusted: bool,
    pub enabled: bool,
    /// 上面几条合起来才是"这次真的会跑"
    pub runs: bool,
}

fn hooks_files(plugin: &Plugin) -> Vec<(PathBuf, &'static str)> {
    // 两种放法都常见：插件根下的 hooks.json，和 hooks/ 目录里那一份。
    // 标签会进 id、进配置文件，所以用 ASCII，别把中文塞进用户要手改的键里
    vec![
        (plugin.path.join("hooks.json"), "root"),
        (plugin.path.join("hooks").join("hooks.json"), "dir"),
    ]
}

fn digest(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        // 分隔符：免得 ("ab","c") 和 ("a","bc") 撞成同一个指纹
        hasher.update([0u8]);
    }
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Windows 上优先取 commandWindows，其他平台取 command；没有专用写法就仍用通用命令
fn command_for_platform(handler: &Value) -> String {
    let preferred = if cfg!(windows) {
        handler
            .get("commandWindows")
            .or_else(|| handler.get("command_windows"))
            .and_then(Value::as_str)
    } else {
        None
    };

    preferred
        .or_else(|| handler.get("command").and_then(Value::as_str))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// 解析一份 hooks.json。第二个返回值是"这条为什么没被收进来"——
/// 只打到 stderr 的话用户永远看不到自己写的护栏被丢掉了，所以要交给界面说清楚。
fn parse_hooks_at(plugin: &Plugin, file: &Path, tag: &str) -> (Vec<Hook>, Vec<String>) {
    let mut notes = Vec::new();

    let Ok(text) = fs::read_to_string(file) else {
        return (Vec::new(), notes);
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        notes.push(format!(
            "{} 不是合法 JSON，里面的钩子一条都没读出来。",
            file.display()
        ));
        return (Vec::new(), notes);
    };

    let description = value
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();

    let Some(events) = value.get("hooks").and_then(Value::as_object) else {
        notes.push(format!(
            "{} 里没有 hooks 对象。",
            file.file_name().unwrap_or_default().to_string_lossy()
        ));
        return (Vec::new(), notes);
    };

    let mut hooks = Vec::new();

    for (event, groups) in events {
        let Some(groups) = groups.as_array() else {
            continue;
        };

        for group in groups {
            let matcher = group
                .get("matcher")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty() && *value != "*")
                .map(str::to_string);

            let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
                notes.push(format!(
                    "{event} 里有一个匹配组没写 hooks 数组，那一条被跳过。"
                ));
                continue;
            };

            for handler in handlers {
                // 处理器类型：command（默认，stdio 本机脚本）之外认 http 与 sse
                // 两种远端形状——它们不落本机盘，凭据管理与沙箱约束都不适用，
                // 但指纹信任照旧：URL 或类型改一个字节，确认就作废
                let kind = handler
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("command");
                let (transport, url) = match kind {
                    "command" => (HookTransport::Stdio, None),
                    "http" | "sse" => {
                        let url = handler
                            .get("url")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(str::to_string);
                        let Some(url) = url else {
                            notes.push(format!(
                                "{event} 里有一条 {kind} 处理器没写 url，无法投递。"
                            ));
                            continue;
                        };
                        if !url.starts_with("http://") && !url.starts_with("https://") {
                            notes.push(format!(
                                "{event} 里有一条 {kind} 处理器的 url 不是 http(s)，拒收。"
                            ));
                            continue;
                        }
                        let transport = if kind == "http" {
                            HookTransport::Http
                        } else {
                            HookTransport::Sse
                        };
                        (transport, Some(url))
                    }
                    other => {
                        notes.push(format!(
                            "{event} 里有一个 {other} 处理器。aglab 只认 command/http/sse，这一条没有执行它。"
                        ));
                        continue;
                    }
                };

                let command = command_for_platform(handler);
                if transport == HookTransport::Stdio && command.is_empty() {
                    notes.push(format!("{event} 里有一条钩子没写 command，无法执行。"));
                    continue;
                }

                let timeout = handler["timeout"]
                    .as_u64()
                    .unwrap_or(DEFAULT_TIMEOUT)
                    .clamp(1, MAX_TIMEOUT);

                let async_flag = handler["async"].as_bool().unwrap_or(false);

                let status_message = handler["statusMessage"]
                    .as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or(&description)
                    .to_string();

                let index = hooks.len();
                let hash = digest(&[
                    event,
                    matcher.as_deref().unwrap_or_default(),
                    &command,
                    transport.as_str(),
                    url.as_deref().unwrap_or_default(),
                ]);

                hooks.push(Hook {
                    id: format!("{}::{tag}::{event}::{index}", plugin.id),
                    event: event.clone(),
                    // 一个文件里多条钩子会共用同一个匹配组，所以这里只能借用
                    matcher: matcher.clone(),
                    command,
                    transport,
                    url,
                    timeout,
                    async_flag,
                    status_message,
                    plugin_id: plugin.id.clone(),
                    plugin_dir: plugin.path.clone(),
                    file: file.to_path_buf(),
                    hash,
                });
            }
        }
    }

    (hooks, notes)
}

/// 界面上按这些节点在回合里发生的先后排，而不是按事件名的字母序
fn event_rank(event: &str) -> u8 {
    match event {
        "SessionStart" => 0,
        "UserPromptSubmit" => 1,
        "PermissionRequest" => 2,
        "PreToolUse" => 3,
        "PostToolUse" => 4,
        "PostToolUseFailure" => 5,
        "PreCompact" => 6,
        "Stop" => 7,
        "SessionEnd" => 8,
        _ => 9,
    }
}

/// 一个插件带来的所有钩子，以及没收进来的原因
pub fn for_plugin(plugin: &Plugin) -> (Vec<Hook>, Vec<String>) {
    let mut hooks = Vec::new();
    let mut notes = Vec::new();

    for (file, tag) in hooks_files(plugin) {
        let (found, skipped) = parse_hooks_at(plugin, &file, tag);
        hooks.extend(found);
        notes.extend(skipped);
    }

    hooks.sort_by_key(|hook| (event_rank(&hook.event), hook.id.clone()));
    (hooks, notes)
}

fn trusted_entry<'a>(trusted: &'a [TrustedHook], id: &str) -> Option<&'a TrustedHook> {
    trusted.iter().find(|entry| entry.id == id)
}

pub fn view_of(hook: &Hook, config: &AppConfig) -> HookView {
    let entry = trusted_entry(&config.trusted_hooks, &hook.id);
    let trusted = entry.is_some();
    // 没确认过时无所谓"新旧"；确认过但指纹对不上就是内容被改过，那份确认不再作数
    let current = entry.map_or(true, |entry| entry.hash == hook.hash);
    let enabled = !config.disabled_hooks.iter().any(|id| id == &hook.id);
    let supported = hook.supported();

    HookView {
        id: hook.id.clone(),
        event: hook.event.clone(),
        matcher: hook.matcher.clone(),
        command: hook.command.clone(),
        timeout: hook.timeout,
        is_async: hook.async_flag,
        status_message: hook.status_message.clone(),
        file: hook.file.display().to_string(),
        hash: hook.hash.clone(),
        supported,
        current,
        trusted,
        enabled,
        runs: supported && trusted && current && enabled,
    }
}

/// 这一轮该跑的钩子：四个条件任何一个不成立都不会出现在这里
pub fn runnable(app: &AppHandle, config: &AppConfig) -> Vec<Hook> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string()).unwrap_or_default();
    runnable_in(config, &data_dir)
}

/// worker 进程的变体（M2 切片 4）：数据目录由调用方传入，
/// 插件名册与 ${aglab_*} 展开都从它派生。全程不碰 AppHandle——
/// 工作区钩子读的是 config.active_project()，不是应用状态
pub fn runnable_in(config: &AppConfig, data_dir: &std::path::Path) -> Vec<Hook> {
    let mut hooks = runnable_for(&plugins::enabled_in(config, data_dir), config);

    // 工作区钩子（信任链第三条腿）：定义与插件同格式，但内容出自**当前工作目录**
    // ——第三方仓库带来了什么，用户工作到一半才知道。三道边界一起上：
    // ① 定义文件路径上任何组件是符号链接就整份不收（skills 同款判据）；
    // ② 信任记录锚定工作目录路径——同名的两个项目互不顶替；
    // ③ 指纹即安全修订：本函数在每轮开始与每次事件发射前都会重跑（调用点见
    //    chat.rs 的各 fire 位），文件一改指纹就失配，撤销即时生效
    if let Some(project) = config.active_project() {
        let root = std::path::PathBuf::from(&project.path);
        let (found, _notes) = workspace_hooks(&root);
        for hook in found {
            let trusted = trusted_entry(&config.trusted_hooks, &hook.id).is_some_and(|entry| {
                entry.hash == hook.hash && entry.root.as_deref() == Some(project.path.as_str())
            });
            let disabled = config.disabled_hooks.iter().any(|id| id == &hook.id);
            if trusted && !disabled {
                hooks.push(hook);
            }
        }
    }
    // ${aglab_*} 变量在"真的要跑"那一刻展开：命令串里写的占位符换成真实
    // 路径与当前用户配置值。指纹信任的是**未展开**的定义——变量值一变，
    // 指纹不变、无需重新确认，但跑出去的就是新值
    for hook in &mut hooks {
        if hook.transport == HookTransport::Stdio {
            hook.command = plugins::expand_with(
                &plugins::expansion_context_in(config, data_dir, &hook.plugin_id),
                &hook.command,
            );
        }
    }
    hooks
}

/// 工作目录里的钩子定义：hooks.json 放工作区根上或 hooks/ 里，格式与插件完全一致。
/// 用伪插件（id 带工作区路径指纹）走同一条解析路——解析写两份就是第二份真相。
/// 定义文件本身是符号链接的整份不收：内容是仓库带来的，链接是路径逃逸的跳板
pub fn workspace_hooks(root: &std::path::Path) -> (Vec<Hook>, Vec<String>) {
    let root_digest = digest(&[root.to_string_lossy().as_ref()]);
    let pseudo = crate::plugins::Plugin {
        user_config: Vec::new(),
        id: format!("ws-{root_digest}"),
        name: "工作区".into(),
        description: String::new(),
        version: String::new(),
        author: String::new(),
        category: String::new(),
        path: root.to_path_buf(),
    };
    let mut hooks = Vec::new();
    let mut notes = Vec::new();
    for (file, tag) in hooks_files(&pseudo) {
        if crate::skills::path_contains_symlink(&file) {
            notes.push(format!(
                "{} 是符号链接，这份钩子定义没有读。工作区内容不替你信任链接。",
                file.display()
            ));
            continue;
        }
        let (found, skipped) = parse_hooks_at(&pseudo, &file, tag);
        hooks.extend(found);
        notes.extend(skipped);
    }
    hooks.sort_by_key(|hook| (event_rank(&hook.event), hook.id.clone()));
    (hooks, notes)
}

/// 应用级事件（SessionStart / SessionEnd）的落点：没有话题线程与工具上下文，
/// 工作目录取当前项目（没有就落回插件目录）。同步跑——启动时它挡的是启动那一瞬，
/// 退出时挡的是退出，作者给这类钩子写的应当是快脚本；慢活用 `async: true`
pub fn fire_app_event(app: &AppHandle, event: &str) {
    let config = crate::config::load(app);
    let hooks = runnable(app, &config);
    if hooks.iter().all(|hook| hook.event != event) {
        return;
    }
    let root = config
        .active_project()
        .map(|project| std::path::PathBuf::from(&project.path));
    let _ = fire(&hooks, event, root.as_deref(), |hook, cwd| {
        json!({
            "hook_event_name": hook.event,
            "cwd": cwd.display().to_string(),
        })
    });
}

fn runnable_for(plugins_enabled: &[Plugin], config: &AppConfig) -> Vec<Hook> {
    plugins_enabled
        .iter()
        .flat_map(|plugin| for_plugin(plugin).0)
        .filter(|hook| hook.supported())
        .filter(|hook| {
            trusted_entry(&config.trusted_hooks, &hook.id)
                .is_some_and(|entry| entry.hash == hook.hash)
        })
        .filter(|hook| !config.disabled_hooks.iter().any(|id| id == &hook.id))
        .collect()
}

/// 钩子载荷落临时文件：stdin 管道建不出时的兜底通道。
/// WorkBuddy 调用链下带 stdin 管道的 spawn 会确定性撞 os error 231
/// （ERROR_PIPE_BUSY，"所有的管道范例都在使用中"——独立探针 10/10 复现，
/// 只管道 stdout/stderr 则 10/10 通过，且串行无缓解），生产重试救不了它。
/// 返回路径：命令行里由 cmd 按路径重定向（见 build_command），文件必须
/// 活到孩子退出，所以这里只写不删、也不开读句柄
fn payload_to_temp_file(payload: &str) -> std::io::Result<PathBuf> {
    let path = std::env::temp_dir().join(format!(
        "aglab-hook-stdin-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
    file.write_all(payload.as_bytes())?;
    file.flush()?;
    drop(file); // 写句柄先关，文件完整落盘；读取方由 cmd 按路径重开
    Ok(path)
}

/// 跑一条钩子：事件 JSON 写进 stdin，读回 stdout / stderr / 退出码，按该事件的语义解释。
/// `root` 是用户当前选中的工作目录；没选工作目录时脚本就在自己插件的目录里跑，
/// 同时给它 `AGLAB_PLUGIN_DIR`，这样项目脚本不必猜自己被装在哪儿。
/// 构造钩子的外壳命令。`stdin_payload` 决定 stdin 的来路：
/// - None（默认途径）：父进程把事件 JSON 写进管道。模板带 chcp 65001——中文
///   Windows 上默认 cp936，钩子吐出的中文理由会被转码转坏、JSON 解析不出来，
///   一条护栏就这样被当成"没意见"悄悄失效。
/// - Some(载荷文件)（兜底途径，spawn 撞 os error 231 时的第二通道）：模板 =
///   `chcp 65001 >nul & {command} < "{payload}"`。chcp 吃的是**继承句柄** stdin
///   的读取位置（推到 EOF、孩子读到空；管道没有位置概念、不受影响，独立探针
///   11 组合钉死），但 `< path` 是 cmd 执行到该命令时重新 CreateFile 打开的
///   **新句柄**（位置 0），chcp 动不到它；顺带脚本按 UTF-8 解释、输出也是 UTF-8。
///   旧方案「文件句柄当 stdin + 裸命令」死于 cmd 按 cp936 解释 UTF-8 脚本：
///   中文是奇数个字节时，最后一个字节会和后面的结构字符（如收尾引号）配成假
///   汉字，JSON 结构损坏、解析失败，护栏静默失效——独立探针实锤。
/// 非 windows：命令行不带重定向（unix 没有 chcp/231 问题），Some 时 stdin 句柄
/// 由 run() 侧打开。编码的双保险不受影响：PYTHONUTF8/PYTHONIOENCODING 照常带上
fn build_command(hook: &Hook, cwd: &Path, stdin_payload: Option<&Path>) -> OsCommand {
    let mut command =
        crate::childproc::hide(OsCommand::new(if cfg!(windows) { "cmd" } else { "sh" }));
    // 钩子是一整行 shell，跑的是插件带的脚本：它比模型点的那条命令更容易把环境打出来，
    // 所以过的是同一份约束（凭据形状的不进子进程）。下面那几个 AGLAB_* / PYTHON* 是
    // 在这之后再显式加回去的，不受筛选影响
    crate::tool_runtime::constrain::constrained(&mut command);

    if cfg!(windows) {
        // 钩子命令是一整行 shell 语法，不能按参数拆开。外面这层引号 + /S 是
        // 唯一能让「带空格的路径」和「带参数的命令行」都认的写法。
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            match stdin_payload {
                None => command.raw_arg(format!(
                    "/S /C \"chcp 65001 >/dev/null & {}\"",
                    hook.command
                )),
                // 兜底途径：chcp 前置 + 按路径重定向，两头问题一起绕开（见函数注释）。
                // 父侧 stdin 给 null——cmd 会用重定向句柄覆盖它
                Some(payload) => command.raw_arg(format!(
                    "/S /C \"chcp 65001 >nul & {} < \"{}\"\"",
                    hook.command,
                    payload.display()
                )),
            };
        }
    } else {
        let _ = stdin_payload; // unix 兜底不走命令行重定向，句柄由 run() 侧打开
        command.args(["-c", hook.command.as_str()]);
    }

    command
        .current_dir(cwd)
        .env("AGLAB_PLUGIN_DIR", &hook.plugin_dir)
        .env("AGLAB_HOOK_EVENT", &hook.event)
        // stdin 是管道，不是控制台：chcp 改不了子进程按 ANSI 代码页解码管道的默认行为。
        // 中文 Windows 上 Python 会用 cp936 去读我们写进去的 UTF-8，载荷里只要有中文
        // 就解成乱码、JSON 直接崩。这两个变量是 Python 官方的强制 UTF-8 开关。
        .env("PYTHONUTF8", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

pub fn run(hook: &Hook, input: Value, root: Option<&Path>) -> Outcome {
    // 传输分派：http 是一拳一个 JSON 的远端问询（等答复，答复就是裁决）；
    // sse 天生 fire-and-forget（发完即走，不占回合）；stdio 走本机脚本的老路。
    // sse 即便写成同步形状也不许等：单向出口没有"答复"可等
    if hook.transport == HookTransport::Sse || (hook.transport == HookTransport::Http && hook.async_flag)
    {
        post_remote(hook, input, false);
        return Outcome::Silent;
    }
    if hook.transport == HookTransport::Http {
        return post_remote(hook, input, true);
    }

    let cwd = root.unwrap_or(hook.plugin_dir.as_path());
    let mut command = build_command(hook, cwd, None);
    command.stdin(Stdio::piped());

    // 起子进程：stdin 先走管道。本调用链下带 stdin 管道的 spawn 可能确定性撞
    // os error 231（管道实例耗尽的长相），前 3 次按瞬时抖动重试；3 次全败后
    // 切兜底通道——载荷落临时文件、命令行里由 cmd 按路径重定向 < 载荷再试
    // 2 次（孩子读到的还是同一份 JSON 字节、文件尾作 EOF，stdin 合同不变）。
    // 兜底也建不出才认 Broken 收容（拍板：约束建立失败 = 拒绝执行）。钩子是
    // 别人写的脚本，收不进去就杀掉孩子、按"没跑成"报出来——Broken 会让调用方
    // 出声，绝不降级成明跑
    let payload = input.to_string();
    let (mut child, _job_guard, mut stdin_file) = {
        let mut attempt = 0;
        let mut stdin_file: Option<PathBuf> = None;
        loop {
            attempt += 1;
            match command.spawn() {
                Ok(child) => match crate::tool_runtime::job::Guard::contain(&child) {
                    Ok(guard) => break (child, guard, stdin_file.take()),
                    Err(problem) => {
                        if let Some(path) = stdin_file.take() {
                            let _ = fs::remove_file(path);
                        }
                        let mut failed = child;
                        crate::tool_runtime::constrain::reap_tree(&mut failed);
                        return Outcome::Broken(format!(
                            "执行收容约束建立失败，钩子没有执行（fail-closed）：{problem}"
                        ));
                    }
                },
                Err(error) => {
                    if attempt == 3 && stdin_file.is_none() {
                        #[cfg(windows)]
                        if let Ok(path) = payload_to_temp_file(&payload) {
                            // 文件-stdin 兜底 v2（chcp 前置 + 按路径重定向，见
                            // build_command）：旧方案「句柄当 stdin + 裸命令」死于
                            // cmd 按 cp936 解释 UTF-8 脚本，奇数中文字节吞掉收尾
                            // 引号、JSON 结构损坏；命令行 chcp 又会吃掉继承句柄的
                            // 读取位置。新句柄是 cmd 重开的，位置 0，两头绕开。
                            command = build_command(hook, cwd, Some(&path));
                            command.stdin(Stdio::null());
                            stdin_file = Some(path);
                        }
                    }
                    if attempt >= 5 {
                        if let Some(path) = stdin_file.take() {
                            let _ = fs::remove_file(path);
                        }
                        return Outcome::Broken(format!(
                            "启动「{}」失败：{error}",
                            hook.plugin_id
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(40 * attempt));
                }
            }
        }
    };
    // 兜底载荷文件此刻不能删：孩子是 cmd 按**路径**重开它的（不再是继承句柄），
    // 进程没退时 Windows 上会因句柄占用删不掉。清理挪到下面的退出分支

    if let Some(mut stdin) = child.stdin.take() {
        // 钩子可以选择不读 stdin，此时写不进去不是它的错
        let _ = stdin.write_all(input.to_string().as_bytes());
        let _ = stdin.flush();
    }

    let pipes = match (child.stdout.take(), child.stderr.take()) {
        (Some(stdout), Some(stderr)) => (one_shot(stdout), one_shot(stderr)),
        (Some(stdout), None) => (one_shot(stdout), never()),
        (None, Some(stderr)) => (never(), one_shot(stderr)),
        (None, None) => return Outcome::Broken("这个钩子没有接上标准输出".into()),
    };

    let deadline = Instant::now() + Duration::from_secs(hook.timeout);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = pipes.0.recv_timeout(READ_GRACE).ok().unwrap_or_default();
                let stderr = pipes.1.recv_timeout(READ_GRACE).ok().unwrap_or_default();
                // 孩子已退，兜底载荷文件清掉（删不掉也只是 %TEMP% 里多一个小文件）
                if let Some(path) = stdin_file.take() {
                    let _ = fs::remove_file(path);
                }
                // UTF-8 优先，解不开降级系统 ANSI（中文 Windows 上是 cp936）：
                // 文件-stdin 兜底途径没有 chcp 段（它吃文件读取位置，见
                // build_command），cmd 脚本钩子的中文理由按 cp936 出来——
                // 之前这里硬判 Broken，等于"护栏挂在编码上"：拦截意见整个丢了。
                // 个别字形误解的代价比整条护栏失效小
                let stdout = crate::tool_runtime::constrain::decode_output(&stdout);
                let stderr = crate::tool_runtime::constrain::decode_output(&stderr);
                return interpret(status.code(), stdout, stderr, &hook.event);
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                crate::tool_runtime::constrain::reap_tree(&mut child);
                if let Some(path) = stdin_file.take() {
                    let _ = fs::remove_file(path);
                }
                return Outcome::Broken(format!("超过 {}s 没收尾，已经终止", hook.timeout));
            }
            Err(error) => {
                crate::tool_runtime::constrain::reap_tree(&mut child);
                if let Some(path) = stdin_file.take() {
                    let _ = fs::remove_file(path);
                }
                return Outcome::Broken(format!("等它收尾时出错：{error}"));
            }
        }
    }
}

/// 边跑边读。等进程退完再收管道，输出多的钩子会把管道写满然后和我们对峙。
/// 收的是字节：编码对不对要留给调用方判，不能先 lossy 掉再假装读懂了
fn one_shot(pipe: impl Read + Send + 'static) -> Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = pipe.take(MAX_HOOK_OUTPUT).read_to_end(&mut buffer);
        let _ = sender.send(buffer);
    });
    receiver
}

/// http/sse 的投递。`wait = false`（sse / async http）时在后台线程里发，
/// 发完丢弃一切；`wait = true`（同步 http）把 HTTP 200 的响应体当钩子输出，
/// 按与 stdio stdout 同一套规则解释成裁决——http 型钩子的 deny/allow 与
/// 本机脚本钩子说同一种话
fn post_remote(hook: &Hook, input: Value, wait: bool) -> Outcome {
    let Some(url) = hook.url.clone() else {
        return Outcome::Broken(format!("{} 型钩子没有写 url", hook.transport.as_str()));
    };
    if !wait {
        thread::spawn(move || {
            let _ = crate::net::with_timeouts(
                ureq::post(&url).header("content-type", "application/json"),
                Duration::from_secs(10),
            )
            .send_json(&input);
        });
        return Outcome::Silent;
    }

    let response = crate::net::with_timeouts(
        ureq::post(&url).header("content-type", "application/json"),
        Duration::from_secs(hook.timeout.max(1)),
    )
    .send_json(&input);

    match response {
        Ok(response) => {
            if !response.status().is_success() {
                return Outcome::Broken(format!("远端钩子回了 HTTP {}", response.status()));
            }
            let body = response
                .into_body()
                .read_to_string()
                .unwrap_or_default();
            interpret(Some(0), body, String::new(), &hook.event)
        }
        Err(ureq::Error::StatusCode(code)) => {
            Outcome::Broken(format!("远端钩子回了 HTTP {code}"))
        }
        Err(error) => Outcome::Broken(format!("远端钩子投递失败：{error}")),
    }
}

fn never() -> Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    drop(sender);
    receiver
}


/// 退出码 2 的含义按事件分头解释，不存在通用的 deny；其他非零退出只是脚本出错
fn interpret(code: Option<i32>, stdout: String, stderr: String, event: &str) -> Outcome {
    let stdout = stdout.trim().to_string();
    let stderr = stderr.trim().to_string();

    if code == Some(2) {
        let reason = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            "钩子以退出码 2 拒绝了这次操作，但没写原因。".to_string()
        };

        return match event {
            // 收尾钩子的"拒绝"意思是"别停，继续干活"，不是把已经做完的事撤掉
            "PreToolUse" | "Stop" | "PermissionRequest" => Outcome::Block(reason),
            // 副作用已经发生，撤不掉了，只能把话转给模型
            _ => Outcome::AddContext(reason),
        };
    }

    if code != Some(0) {
        return Outcome::Broken(if stderr.is_empty() {
            format!("退出码 {code:?}")
        } else {
            stderr
        });
    }

    if stdout.is_empty() {
        return Outcome::Silent;
    }

    // stdout 优先按结构化输出读；读不懂就不算意见。
    // 例外是提交前钩子：按参照实现，它的纯文本 stdout 就是要补进去的上下文。
    let Ok(value) = serde_json::from_str::<Value>(&stdout) else {
        return if event == "UserPromptSubmit" {
            Outcome::AddContext(stdout)
        } else {
            Outcome::Silent
        };
    };

    let specific = &value["hookSpecificOutput"];
    let reason_of = |fallback: &str| -> String {
        specific["permissionDecisionReason"]
            .as_str()
            .or_else(|| value["reason"].as_str())
            .unwrap_or(fallback)
            .to_string()
    };

    match specific["permissionDecision"]
        .as_str()
        .or_else(|| value["decision"].as_str())
    {
        // 收尾钩子的 block 和退出码 2 同义：别收这轮尾。只有工具执行前的 block 才是"别做"
        Some("deny") | Some("block") => {
            if matches!(event, "PreToolUse" | "Stop" | "PermissionRequest") {
                Outcome::Block(reason_of("钩子对本次操作提出了异议。"))
            } else {
                Outcome::AddContext(reason_of("钩子对本次执行提出了异议。"))
            }
        }
        // ask 只有执行前有意义：把这次调用升级成"该问而问"——弹审批让用户拍板。
        // 其余事件没有待执行的动作，问了也白问，按转达处理
        Some("ask") => {
            if event == "PreToolUse" || event == "PermissionRequest" {
                Outcome::Ask(reason_of("钩子想让你先过目这一次调用。"))
            } else {
                Outcome::AddContext(reason_of("钩子有一句话要转达。"))
            }
        }
        // "放行"只在权限问询上是一句真裁决：审批闸直接过，不再问人。
        // 其余事件的 allow 与沉默同义
        Some("allow") if event == "PermissionRequest" => {
            Outcome::Approve(reason_of("按权限钩子放行。"))
        }
        Some("allow") => Outcome::Silent,
        _ => match specific["additionalContext"].as_str() {
            Some(context) if !context.trim().is_empty() => {
                Outcome::AddContext(context.trim().to_string())
            }
            _ => Outcome::Silent,
        },
    }
}

/// 一个事件上所有匹配钩子的答复
#[derive(Debug, Default)]
pub struct Report {
    pub notes: Vec<(Hook, Outcome)>,
}

impl Report {
    /// 有没有人要求停下。多条钩子意见不一致时，任何一条 deny 都作数：
    /// 多数"允许"盖不住单数"拒绝"
    pub fn blocked(&self) -> Option<String> {
        let reasons: Vec<&str> = self
            .notes
            .iter()
            .filter_map(|(_, outcome)| match outcome {
                Outcome::Block(reason) => Some(reason.as_str()),
                _ => None,
            })
            .collect();

        (!reasons.is_empty()).then(|| reasons.join("\n"))
    }

    /// 有没有钩子要求"这一次先问人"。调用方把它并进审批判定：
    /// 哪怕权限表本来就放行，钩子的 ask 也要把这一次拉回审批
    pub fn asks(&self) -> Option<String> {
        let reasons: Vec<&str> = self
            .notes
            .iter()
            .filter_map(|(_, outcome)| match outcome {
                Outcome::Ask(reason) => Some(reason.as_str()),
                _ => None,
            })
            .collect();

        (!reasons.is_empty()).then(|| reasons.join("\n"))
    }

    /// 有没有权限钩子替用户点了头。优先级低于 block/ask：任何一条 deny 都作数，
    /// 任何一条 ask 都把人拉回来——单数的"放行"盖不过它们
    pub fn approves(&self) -> Option<String> {
        if self.blocked().is_some() || self.asks().is_some() {
            return None;
        }
        let reasons: Vec<&str> = self
            .notes
            .iter()
            .filter_map(|(_, outcome)| match outcome {
                Outcome::Approve(reason) => Some(reason.as_str()),
                _ => None,
            })
            .collect();

        (!reasons.is_empty()).then(|| reasons.join("\n"))
    }

    /// 钩子要补给模型的话。坏掉的钩子也在这里出声，不然用户根本不知道护栏没生效
    pub fn context(&self) -> Option<String> {
        let lines: Vec<String> = self
            .notes
            .iter()
            .filter_map(|(hook, outcome)| match outcome {
                Outcome::AddContext(text) => Some(format!("（{}）{text}", hook.event)),
                Outcome::Broken(detail) => Some(format!("（{}）钩子没跑成：{detail}", hook.event)),
                _ => None,
            })
            .collect();

        (!lines.is_empty()).then(|| lines.join("\n"))
    }
}

/// 在某个节点上依次跑该事件的钩子。`build` 负责按事件拼 stdin 的载荷，
/// 拼一次就够了：筛选要看里面的工具名，而它得知道脚本真正的工作目录。
/// `async: true` 的钩子发射后不管：起一个线程跑，输出与结论全部丢弃，
/// 本函数不等它——它拦不住任何事，也拖不慢任何事
pub fn fire(
    hooks: &[Hook],
    event: &str,
    root: Option<&Path>,
    mut build: impl FnMut(&Hook, &Path) -> Value,
) -> Report {
    let mut report = Report::default();

    for hook in hooks.iter().filter(|hook| hook.event == event) {
        let cwd = root.unwrap_or(hook.plugin_dir.as_path());
        let payload = build(hook, cwd);

        // 话题级节点（话题起止、压缩前）与提交/收尾一样没有可匹配的对象，
        // matcher 按定义不参与筛选
        let applies = matches!(
            event,
            "UserPromptSubmit" | "Stop" | "SessionStart" | "SessionEnd" | "PreCompact"
        ) || hook.matches_tool(payload["tool_name"].as_str().unwrap_or_default());

        if !applies {
            continue;
        }
        if hook.async_flag {
            let hook = hook.clone();
            let payload = payload.clone();
            let root = root.map(Path::to_path_buf);
            let spawned = thread::Builder::new()
                .name(format!("hook-async-{}", hook.id))
                .spawn(move || {
                    let _ = run(&hook, payload, root.as_deref());
                });
            // 起不来线程也只是这条异步钩子没跑：它本来就不影响判定，不报错
            if spawned.is_err() {
                continue;
            }
            continue;
        }
        report.notes.push((hook.clone(), run(hook, payload, root)));
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    /// chcp 改的是整台控制台的代码页，同进程里并行跑的两条钩子测试会互相踩到，
    /// 所以真起进程的测试串行跑。生产上每个钩子进程有自己那份控制台，不存在这个问题
    static CONSOLE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn console_lock() -> std::sync::MutexGuard<'static, ()> {
        CONSOLE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn plugin_at(base: &Path) -> Plugin {
        Plugin {
            user_config: Vec::new(),
            id: "guard".into(),
            name: "guard".into(),
            description: String::new(),
            version: String::new(),
            author: String::new(),
            category: String::new(),
            path: base.to_path_buf(),
        }
    }

    fn parse(base: &Path, text: &str) -> Vec<Hook> {
        let plugin = plugin_at(base);
        let file = base.join("hooks").join("hooks.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, text).unwrap();
        for_plugin(&plugin).0
    }

    use serde_json::json;

    fn hook(id: &str, event: &str) -> Hook {
        Hook {
            id: id.into(),
            event: event.into(),
            matcher: None,
            command: "a.sh".into(),
            transport: HookTransport::Stdio,
            url: None,
            timeout: 5,
            async_flag: false,
            status_message: String::new(),
            plugin_id: "p".into(),
            plugin_dir: PathBuf::from("."),
            file: PathBuf::from("."),
            hash: "hash".into(),
        }
    }

    #[test]
    fn reads_the_three_level_shape_and_fingerprints_each_handler() {
        let base = crate::test_support::temp_dir("hooks-parse");
        let hooks = parse(
            &base,
            r#"{
  "description": "别删数据集",
  "hooks": {
    "PreToolUse": [
      { "matcher": "^run_command$",
        "hooks": [
          { "type": "command", "command": "py -3 check.py", "timeout": 5, "statusMessage": "查一遍" },
          { "type": "prompt", "command": "不该被收进来的那个" }
        ] }
    ],
    "Stop": [{ "hooks": [{ "type": "command", "command": "check_done.sh" }] }]
  }
}"#,
        );

        assert_eq!(hooks.len(), 2, "prompt 处理器不该被执行");
        let guard = &hooks[0];
        assert_eq!(guard.event, "PreToolUse");
        assert_eq!(guard.matcher.as_deref(), Some("^run_command$"));
        assert_eq!(guard.timeout, 5);
        assert_eq!(guard.status_message, "查一遍");
        assert_eq!(guard.hash.len(), 16, "指纹要短到能显示在界面上");
        assert!(guard.id.starts_with("guard::dir::PreToolUse::"));

        // 没写 statusMessage 的那条退回用文件级 description
        assert_eq!(hooks[1].status_message, "别删数据集");
        assert_eq!(hooks[1].timeout, DEFAULT_TIMEOUT);
        assert_ne!(hooks[1].hash, guard.hash);

        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn the_fingerprint_follows_the_definition_not_the_position() {
        let base = crate::test_support::temp_dir("hooks-hash");
        let first = parse(
            &base,
            r#"{"hooks":{"PreToolUse":[{"matcher":"^a$","hooks":[{"type":"command","command":"one.sh"}]}]}}"#,
        );
        let same = parse(
            &base,
            r#"{"hooks":{"PreToolUse":[{"matcher":"^a$","hooks":[{"type":"command","command":"one.sh"}]}]}}"#,
        );
        let edited = parse(
            &base,
            r#"{"hooks":{"PreToolUse":[{"matcher":"^a$","hooks":[{"type":"command","command":"two.sh"}]}]}}"#,
        );

        assert_eq!(first[0].id, edited[0].id, "位置没动，id 就该一样");
        assert_eq!(first[0].hash, same[0].hash);
        assert_ne!(
            first[0].hash, edited[0].hash,
            "命令一改指纹必须跟着改，否则信任就跟着跑了"
        );

        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn a_missing_matcher_means_every_tool() {
        let base = crate::test_support::temp_dir("hooks-match");
        let hooks = parse(
            &base,
            r#"{"hooks":{"PreToolUse":[
                {"matcher":"*","hooks":[{"type":"command","command":"a.sh"}]},
                {"hooks":[{"type":"command","command":"b.sh"}]},
                {"matcher":"^write_file$","hooks":[{"type":"command","command":"c.sh"}]}
            ]}}"#,
        );

        assert_eq!(hooks.len(), 3);
        assert!(hooks[0].matches_tool("run_command"));
        assert!(hooks[1].matches_tool("read_file"));
        assert!(hooks[2].matches_tool("write_file"));
        assert!(!hooks[2].matches_tool("run_command"));
        // 扩展工具的名字是 mcp__服务__工具，锚定的正则要能挑得出来
        assert!(hooks[0].matches_tool("mcp__db__query"));

        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn a_broken_regex_matches_nothing_instead_of_everything() {
        let base = crate::test_support::temp_dir("hooks-badregex");
        let hooks = parse(
            &base,
            r#"{"hooks":{"PreToolUse":[{"matcher":"^run(","hooks":[{"type":"command","command":"a.sh"}]}]}}"#,
        );

        assert_eq!(hooks.len(), 1);
        assert!(!hooks[0].matches_tool("run_command"));

        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn events_without_a_hook_point_are_listed_but_never_scheduled() {
        let base = crate::test_support::temp_dir("hooks-events");
        let hooks = parse(
            &base,
            r#"{"hooks":{
                "SubagentStop":[{"hooks":[{"type":"command","command":"a.sh"}]}],
                "SessionStart":[{"hooks":[{"type":"command","command":"b.sh"}]}],
                "PreToolUse":[{"hooks":[{"type":"command","command":"c.sh"}]}]
            }}"#,
        );

        assert_eq!(hooks.len(), 3);
        let subagent = hooks
            .iter()
            .find(|hook| hook.event == "SubagentStop")
            .expect("没落点的事件也要列出来，用户写了他就该看见");
        let session_start = hooks
            .iter()
            .find(|hook| hook.event == "SessionStart")
            .expect("话题开始在本客户端有落点");
        let pre = hooks
            .iter()
            .find(|hook| hook.event == "PreToolUse")
            .expect("PreToolUse 要有");
        assert!(!subagent.supported(), "SubagentStop 在本客户端没有落点");
        assert!(session_start.supported());
        assert!(pre.supported());

        let config = AppConfig::default();
        // 信任的两条会跑；SubagentStop 没落点，信任了也不该跑
        let mut trusted_config = AppConfig::default();
        for hook in hooks.iter().filter(|hook| hook.supported()) {
            trusted_config.trusted_hooks.push(TrustedHook {
                id: hook.id.clone(),
                hash: hook.hash.clone(),
                root: None,
            });
        }
        let runnable = runnable_for(&[plugin_at(&base)], &trusted_config);
        assert_eq!(runnable.len(), 2, "SubagentStop 永远不跑，另外两条照常");
        assert!(runnable.iter().all(|hook| hook.event != "SubagentStop"));
        assert!(runnable_for(&[plugin_at(&base)], &config).is_empty(), "没信任过的照旧一条不跑");

        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn nothing_runs_until_the_definition_has_been_confirmed() {
        let base = crate::test_support::temp_dir("hooks-trust");
        let hooks = parse(
            &base,
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"a.sh"}]}]}}"#,
        );
        let plugin = plugin_at(&base);

        let mut config = AppConfig::default();
        assert!(
            runnable_for(&[plugin.clone()], &config).is_empty(),
            "没确认过就不该跑"
        );

        config.trusted_hooks.push(TrustedHook {
            id: hooks[0].id.clone(),
            hash: hooks[0].hash.clone(),
            root: None,
        });
        assert_eq!(runnable_for(&[plugin.clone()], &config).len(), 1);

        // 确认过后把命令改掉：指纹变了，那条钩子立刻停下来
        let edited = parse(
            &base,
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"a.sh --i-changed"}]}]}}"#,
        );
        let view = view_of(&edited[0], &config);
        assert!(view.trusted);
        assert!(!view.current, "界面上要能单独说出：确认过后内容被改过");
        assert!(!view.runs);
        assert!(runnable_for(&[plugin.clone()], &config).is_empty());

        // 单独关掉同样停下来，但不必把那份确认抹掉
        fs::write(
            base.join("hooks").join("hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"a.sh"}]}]}}"#,
        )
        .unwrap();
        config.disabled_hooks.push(hooks[0].id.clone());
        assert!(runnable_for(&[plugin], &config).is_empty());
        assert!(view_of(&hooks[0], &config).trusted);

        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn exit_code_two_blocks_only_before_the_tool_runs() {
        let block = interpret(
            Some(2),
            String::new(),
            "只读目录，别写".into(),
            "PreToolUse",
        );
        assert_eq!(block, Outcome::Block("只读目录，别写".into()));

        // 工具已经跑完了，这时候说"拒绝"只能转达，不能假装撤销
        assert_eq!(
            interpret(Some(2), String::new(), "结果不对".into(), "PostToolUse"),
            Outcome::AddContext("结果不对".into())
        );

        // 收尾钩子的拒绝意思是"继续干活"，退出码 2 和 JSON 写法都得算
        assert_eq!(
            interpret(Some(2), String::new(), "还差验证".into(), "Stop"),
            Outcome::Block("还差验证".into())
        );
        assert_eq!(
            interpret(
                Some(0),
                json!({"decision": "block", "reason": "还差验证"}).to_string(),
                String::new(),
                "Stop"
            ),
            Outcome::Block("还差验证".into()),
            "参照实现里 Stop 就是靠 decision:block 要求继续的"
        );

        assert_eq!(
            interpret(Some(2), String::new(), String::new(), "PreToolUse"),
            Outcome::Block("钩子以退出码 2 拒绝了这次操作，但没写原因。".into()),
            "一个字都没写也得给用户一句能懂的话"
        );
    }

    #[test]
    fn a_crashing_hook_is_reported_not_treated_as_permission() {
        for event in ["UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop"] {
            assert_eq!(
                interpret(Some(1), String::new(), "Traceback: boom".into(), event),
                Outcome::Broken("Traceback: boom".into()),
                "{event} 上脚本崩溃不算意见"
            );
        }

        let report = Report {
            notes: vec![(hook("h", "PreToolUse"), Outcome::Broken("崩了".into()))],
        };
        assert!(report.blocked().is_none(), "崩溃不能被当成拒绝");
        assert!(
            report.context().is_some(),
            "崩溃要出声，不然用户以为护栏在生效"
        );
    }

    #[test]
    fn structured_stdout_is_read_per_event() {
        let deny = json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": "这条命令要递归删受保护目录"
            }
        });
        assert_eq!(
            interpret(Some(0), deny.to_string(), String::new(), "PreToolUse"),
            Outcome::Block("这条命令要递归删受保护目录".into())
        );
        assert_eq!(
            interpret(Some(0), deny.to_string(), String::new(), "PostToolUse"),
            Outcome::AddContext("这条命令要递归删受保护目录".into())
        );

        // 兼容老写法
        assert_eq!(
            interpret(
                Some(0),
                json!({"decision":"block","reason":"先跑测试"}).to_string(),
                String::new(),
                "PreToolUse"
            ),
            Outcome::Block("先跑测试".into())
        );

        assert_eq!(
            interpret(
                Some(0),
                json!({"hookSpecificOutput":{"permissionDecision":"allow"}}).to_string(),
                String::new(),
                "PreToolUse"
            ),
            Outcome::Silent
        );
        // ask 现在是"执行前升级成审批"：不再是报错，其余事件没有待执行的动作、按转达处理
        assert!(matches!(
            interpret(
                Some(0),
                json!({"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"先问一下用户"}}).to_string(),
                String::new(),
                "PreToolUse"
            ),
            Outcome::Ask(_)
        ));
        assert!(matches!(
            interpret(
                Some(0),
                json!({"hookSpecificOutput":{"permissionDecision":"ask"}}).to_string(),
                String::new(),
                "PostToolUse"
            ),
            Outcome::AddContext(_)
        ));

        assert_eq!(
            interpret(
                Some(0),
                json!({"hookSpecificOutput":{"additionalContext":"本项目用 COCO 类别映射"}})
                    .to_string(),
                String::new(),
                "PostToolUse"
            ),
            Outcome::AddContext("本项目用 COCO 类别映射".into())
        );

        // 提交前钩子的纯文本 stdout 就是要补的上下文；其他事件读不懂的 stdout 不算意见
        assert_eq!(
            interpret(
                Some(0),
                "先看 data.yaml".into(),
                String::new(),
                "UserPromptSubmit"
            ),
            Outcome::AddContext("先看 data.yaml".into())
        );
        assert_eq!(
            interpret(Some(0), "just some log".into(), String::new(), "PreToolUse"),
            Outcome::Silent
        );
    }

    /// 走一遍 fire()：真实起进程，验证"载荷里的工具名 → matcher 筛选 → 拦截结论"这条链
    #[test]
    fn a_fired_hook_only_stops_the_tool_its_matcher_points_at() {
        let _serial = console_lock();
        let base = crate::test_support::temp_dir("hooks-fire");
        fs::create_dir_all(&base).unwrap();
        let script = base.join("deny-write.cmd");
        fs::write(
            &script,
            "@echo off\r\necho {\"decision\":\"block\",\"reason\":\"写入要先看一眼\"}\r\n",
        )
        .unwrap();

        let deny = Hook {
            id: "p::root::PreToolUse::0".into(),
            event: "PreToolUse".into(),
            matcher: Some("^write_file$".into()),
            command: format!("\"{}\"", script.display()),
            transport: HookTransport::Stdio,
            url: None,
            timeout: 20,
            async_flag: false,
            status_message: "查一遍".into(),
            plugin_id: "p".into(),
            plugin_dir: base.clone(),
            file: script.clone(),
            hash: "hash".into(),
        };
        let hooks = [deny];

        let build = |hook: &Hook, cwd: &Path| {
            json!({
                "hook_event_name": hook.event,
                "cwd": cwd.display().to_string(),
                "tool_name": "write_file",
            })
        };
        let report = fire(&hooks, "PreToolUse", Some(&base), build);
        assert_eq!(report.blocked().as_deref(), Some("写入要先看一眼"));
        assert_eq!(report.notes.len(), 1, "匹配上了就该跑一次");

        // 同一个钩子，换个工具名就不该跑——护栏不能顺手把读也拦了
        let report = fire(&hooks, "PreToolUse", Some(&base), |hook, cwd| {
            json!({
                "hook_event_name": hook.event,
                "cwd": cwd.display().to_string(),
                "tool_name": "read_file",
            })
        });
        assert!(report.notes.is_empty() && report.blocked().is_none());

        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn the_hook_view_carries_every_field_the_frontend_reads() {
        let view = view_of(
            &hook("p::root::PreToolUse::0", "PreToolUse"),
            &AppConfig::default(),
        );
        crate::test_support::assert_matches_ts(&serde_json::to_value(view).unwrap(), "HookView");
    }

    /// 【环境拦截态探测】用与 run() 兜底**完全同款**的途径（文件句柄当 stdin）
    /// 喂一段已知内容给孩子读回：读不回 = 环境在吞孩子的句柄 I/O（行为沙箱对
    /// 长进程链的漂移式接管，全量跑 278s 末尾实测出现过：python 读 stdin 得空、
    /// json.load 在 char 0 炸）。探测与钩子脚本无关，任何环境只依赖 System32 自带
    /// 的 findstr。返回 true 时依赖 stdin 的钩子测试应跳过——被吞的句柄里
    /// 什么断言都立不住
    #[cfg(windows)]
    fn environment_swallows_child_stdin() -> bool {
        use std::os::windows::process::CommandExt;

        let probe = std::env::temp_dir().join(format!(
            "aglab-stdin-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::write(&probe, "PROBE-MARKER").expect("写 stdin 探测文件");
        // 与 run() 兜底同款途径：cmd 按路径重定向 < probe（生产链里 chcp 前置，
        // 这里只关心「环境吞不吞孩子的 stdin I/O」，不必带）
        let output = OsCommand::new("cmd")
            .raw_arg(format!(
                "/S /C \"findstr \"PROBE\" < \"{}\"\"",
                probe.display()
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output();
        let _ = fs::remove_file(&probe);
        match output {
            Ok(out) => !String::from_utf8_lossy(&out.stdout).contains("PROBE-MARKER"),
            Err(_) => true, // 连 spawn 都起不来 = 拦截态
        }
    }

    #[cfg(not(windows))]
    fn environment_swallows_child_stdin() -> bool {
        false
    }

    /// 回归：子进程的 stdin 是管道，中文 Windows 上 Python 默认按 cp936 解，
    /// 载荷里带中文就会解成乱码、JSON 崩掉——这条测试没有 PYTHONUTF8 就会红。
    /// 机器上没有 py 启动器时跳过并说明，不假装通过。
    #[test]
    fn a_hook_reading_chinese_from_stdin_still_gets_valid_json() {
        let probe = OsCommand::new("py")
            .args(["-3", "--version"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if probe.is_err() {
            eprintln!("这台机器上没有 py -3，跳过 stdin 编码回归");
            return;
        }
        if environment_swallows_child_stdin() {
            println!("【环境劫持】孩子进程的 stdin 句柄 I/O 被安全软件吞掉（与 run() 兜底同款途径的对照探测读不回喂入内容），钩子端到端断言跳过——231 兜底路径本身由其余三条不依赖 stdin 的钩子测试覆盖");
            return;
        }

        let base = crate::test_support::temp_dir("hooks-utf8");
        fs::create_dir_all(&base).unwrap();
        let script = base.join("echo_note.py");
        // 把载荷里的 note 原样当成拒绝理由：解错编码就拼不出合法 JSON
        fs::write(
            &script,
            "import json, sys
event = json.load(sys.stdin)
print(json.dumps({\"decision\": \"block\", \"reason\": sys.stdin.encoding}, ensure_ascii=False))
",
        )
        .unwrap();

        let hook = Hook {
            id: "utf8::root::Stop::0".into(),
            event: "Stop".into(),
            matcher: None,
            command: format!(r#"py -3 "{}""#, script.display()),
            transport: HookTransport::Stdio,
            url: None,
            timeout: 30,
            async_flag: false,
            status_message: String::new(),
            plugin_id: "utf8".into(),
            plugin_dir: base.clone(),
            file: script.clone(),
            hash: "hash".into(),
        };

        let outcome = run(
            &hook,
            json!({ "hook_event_name": "Stop", "note": "收尾前那句中文得原样回来" }),
            Some(&base),
        );
        // 没设 PYTHONUTF8 时这里是 gbk：载荷里的中文会被按 cp936 解，
        // 长一点的中文就能把 JSON 的引号吃掉（真机上就是这么崩的）
        assert_eq!(
            outcome,
            Outcome::Block("utf-8".into()),
            "钩子的 stdin 必须是 UTF-8，否则中文载荷解不出合法 JSON"
        );

        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn one_deny_outvotes_any_number_of_allows() {
        let report = Report {
            notes: vec![
                (hook("a", "PreToolUse"), Outcome::Silent),
                (hook("b", "PreToolUse"), Outcome::Block("不行".into())),
                (hook("c", "PreToolUse"), Outcome::Block("也别这么写".into())),
            ],
        };

        assert_eq!(report.blocked().as_deref(), Some("不行\n也别这么写"));
    }

    /// ask 的升级不覆盖 deny，deny 也不吃掉 ask：blocked 与 asks 各自独立出声，
    /// 调用方先看 blocked（拦下就完），拦不下再看 asks（要不要弹审批）
    #[test]
    fn ask_and_block_report_through_their_own_channels() {
        let report = Report {
            notes: vec![
                (hook("a", "PreToolUse"), Outcome::Ask("这个先问人".into())),
                (hook("b", "PreToolUse"), Outcome::Silent),
            ],
        };
        assert!(report.blocked().is_none());
        assert_eq!(report.asks().as_deref(), Some("这个先问人"));

        let report = Report {
            notes: vec![
                (hook("a", "PreToolUse"), Outcome::Ask("先问".into())),
                (hook("b", "PreToolUse"), Outcome::Block("拦下".into())),
            ],
        };
        assert_eq!(report.blocked().as_deref(), Some("拦下"));
        assert_eq!(report.asks().as_deref(), Some("先问"), "拦下也把 ask 带出来：两条钩子的话都得有人听见");
    }

    /// async 钩子发射后不管：fire 立刻返回、不出现在报告里——它拦不住任何事，
    /// 也拖不慢任何事。慢脚本留给它的 60 秒不该变成回合的 60 秒
    #[test]
    fn an_async_hook_fires_without_waiting_for_its_verdict() {
        let _serial = console_lock();
        let base = crate::test_support::temp_dir("hooks-async");
        fs::create_dir_all(&base).unwrap();
        // 慢脚本 + 落文件：fire 返回时它多半还没跑完，但最终文件必须出现
        #[cfg(windows)]
        let command = "ping -n 3 127.0.0.1 > nul & echo done > done.txt";
        #[cfg(not(windows))]
        let command = "sleep 2 && echo done > done.txt";

        let mut hook = hook("p::root::PostToolUse::0", "PostToolUse");
        hook.async_flag = true;
        hook.command = command.to_string();
        hook.plugin_dir = base.clone();
        hook.timeout = MAX_TIMEOUT;

        let started = Instant::now();
        let report = fire(&[hook], "PostToolUse", Some(&base), |hook, cwd| {
            json!({ "hook_event_name": hook.event, "cwd": cwd.display().to_string() })
        });
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "async 钩子不该让 fire 等它跑完"
        );
        assert!(report.notes.is_empty(), "异步钩子没有结论可报");

        // 给慢脚本留出跑完的余量，验证它真的被执行了（发射后不管 ≠ 没发射）
        let done = base.join("done.txt");
        for _ in 0..80 {
            if done.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        assert!(done.exists(), "异步钩子终究要被执行一次");

        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn timeout_is_clamped_into_the_range_a_front_desk_turn_can_afford() {
        let base = crate::test_support::temp_dir("hooks-timeout");
        let hooks = parse(
            &base,
            r#"{"hooks":{"PreToolUse":[{"hooks":[
                {"type":"command","command":"a.sh","timeout":600},
                {"type":"command","command":"b.sh","timeout":0}
            ]}]}}"#,
        );

        assert_eq!(hooks[0].timeout, MAX_TIMEOUT);
        assert_eq!(hooks[1].timeout, 1);

        crate::test_support::remove_tree(&base);
    }

    /// 真的起进程跑一遍：验证 stdin 能读进去、退出码和 stdout 都按约定解释。
    /// 平台分支只覆盖当前 CI，非 Windows 上走 sh，两边都断言同一份语义。
    #[test]
    fn a_real_hook_receives_its_event_json_and_its_verdict_is_honoured() {
        let _serial = console_lock();
        // 目录名里带空格：钩子命令必须能扛住，不然真实用户目录下就跑不起来
        let base = crate::test_support::temp_dir("hooks run");
        fs::create_dir_all(&base).unwrap();

        #[cfg(windows)]
        let script = {
            // 读 stdin 再决定：退出码 2 + stderr 走阻断这条路
            let file = base.join("deny.cmd");
            fs::write(
                &file,
                "@echo off\r\necho {\"hookSpecificOutput\":{\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"脚本说的原因\"}}\r\n",
            )
            .unwrap();
            file
        };
        #[cfg(not(windows))]
        let script = {
            let file = base.join("deny.sh");
            fs::write(
                &file,
                "printf '%s' '{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"脚本说的原因\"}}'\n",
            )
            .unwrap();
            file
        };

        let hook = Hook {
            id: "run::目录::PreToolUse::0".into(),
            event: "PreToolUse".into(),
            matcher: None,
            // 带空格的路径要自己加引号，这是 shell 的规矩，aglab 不替它兜
            command: format!("\"{}\"", script.display()),
            transport: HookTransport::Stdio,
            url: None,
            timeout: 20,
            async_flag: false,
            status_message: String::new(),
            plugin_id: "run".into(),
            plugin_dir: base.clone(),
            file: script.clone(),
            hash: "hash".into(),
        };

        let outcome = run(
            &hook,
            json!({"hook_event_name": "PreToolUse", "tool_name": "run_command"}),
            Some(&base),
        );
        assert_eq!(outcome, Outcome::Block("脚本说的原因".into()));

        // 超时那条路：不能把"没跑完"当成"没意见"，也不能留下活着的子进程
        #[cfg(windows)]
        let slow = "ping -n 30 127.0.0.1 > nul";
        #[cfg(not(windows))]
        let slow = "sleep 30";
        let hook = Hook {
            timeout: 1,
            command: slow.into(),
            ..hook
        };
        assert!(matches!(
            run(&hook, json!({}), Some(&base)),
            Outcome::Broken(_)
        ));

        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn http_and_sse_handlers_parse_with_urls_and_fingerprint_the_url() {
        let base = crate::test_support::temp_dir("hooks-remote-parse");
        let hooks = parse(
            &base,
            r#"{
                "hooks": {
                    "PreToolUse": [
                        {
                            "hooks": [
                                {"type": "http", "url": "https://gate.example.com/check"},
                                {"type": "sse", "url": "http://127.0.0.1:9/feed"},
                                {"type": "http"},
                                {"type": "sse", "url": "ftp://nope"},
                                {"type": "prompt", "prompt": "nope"}
                            ]
                        }
                    ]
                }
            }"#,
        );
        assert_eq!(hooks.len(), 2, "没 url 与非 http(s) 的两条要被拒收");
        assert_eq!(hooks[0].transport, HookTransport::Http);
        assert_eq!(hooks[0].url.as_deref(), Some("https://gate.example.com/check"));
        assert_eq!(hooks[1].transport, HookTransport::Sse);
        assert!(
            hooks[0].hash != hooks[1].hash,
            "传输类型与 URL 都进指纹：改一个字节，确认就作废"
        );
        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn permission_request_verdicts_map_to_approve_block_and_ask() {
        // allow 是一句真裁决：审批闸直接过
        let allow = interpret(
            Some(0),
            r#"{"hookSpecificOutput":{"permissionDecision":"allow","permissionDecisionReason":"白名单命中"}}"#.into(),
            String::new(),
            "PermissionRequest",
        );
        assert_eq!(allow, Outcome::Approve("白名单命中".into()));

        // deny / 退出码 2 都是拦
        let deny = interpret(
            Some(0),
            r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"禁写区"}}"#.into(),
            String::new(),
            "PermissionRequest",
        );
        assert_eq!(deny, Outcome::Block("禁写区".into()));
        let exit_two = interpret(Some(2), String::new(), "不许".to_string(), "PermissionRequest");
        assert_eq!(exit_two, Outcome::Block("不许".into()));

        // ask 拉回审批
        let ask = interpret(
            Some(0),
            r#"{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"先过目"}}"#.into(),
            String::new(),
            "PermissionRequest",
        );
        assert_eq!(ask, Outcome::Ask("先过目".into()));
    }

    #[test]
    fn an_approval_yields_to_block_and_ask() {
        let mut report = Report::default();
        report.notes.push((
            hook("a", "PermissionRequest"),
            Outcome::Approve("放行".into()),
        ));
        assert_eq!(report.approves().as_deref(), Some("放行"));

        // 单数的放行盖不过任何一条 ask
        report.notes.push((hook("b", "PermissionRequest"), Outcome::Ask("问一句".into())));
        assert!(report.approves().is_none(), "ask 在场时放行不作数");
        assert_eq!(report.asks().as_deref(), Some("问一句"));

        // deny 在场时放行更不作数；blocked 独立汇总所有 deny
        report.notes.push((hook("c", "PermissionRequest"), Outcome::Block("不许".into())));
        assert!(report.approves().is_none());
        assert!(report.asks().is_some(), "deny 与 ask 各自独立汇总，互不吸收");
        assert_eq!(report.blocked().as_deref(), Some("不许"));
    }

    #[test]
    fn an_http_hook_reads_its_verdict_from_the_response_body() {
        // 本地 TCP 监听器扮一次远端钩子：收 POST，回 200 + allow 裁决
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("起本地监听");
        let port = listener.local_addr().expect("拿到端口").port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("等到一条连接");
            let mut stream = stream;
            let mut buffer = [0u8; 2048];
            let _ = std::io::Read::read(&mut stream, &mut buffer);
            let body = r#"{"decision":"deny","reason":"远端说了不许"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
        });

        let mut hook = hook("remote", "PermissionRequest");
        hook.transport = HookTransport::Http;
        hook.url = Some(format!("http://127.0.0.1:{port}/check"));
        hook.timeout = 5;
        let outcome = run(&hook, json!({"tool_name": "write_file"}), None);
        server.join().expect("服务线程收尾");
        assert_eq!(
            outcome,
            Outcome::Block("远端说了不许".into()),
            "http 型钩子的响应体按与 stdio 同一套规则解释成裁决"
        );
    }

    #[test]
    fn an_sse_hook_never_waits_and_reports_silent() {
        // sse 的契约是"发完即走"：远端不应答也不能拖慢回合——
        // 用一个永远不回应的监听器证明 run() 立刻返回 Silent
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("起本地监听");
        let port = listener.local_addr().expect("拿到端口").port();
        let started = Instant::now();
        let mut hook = hook("feed", "PostToolUse");
        hook.transport = HookTransport::Sse;
        hook.url = Some(format!("http://127.0.0.1:{port}/feed"));
        hook.timeout = 30;
        let outcome = run(&hook, json!({"tool_name": "write_file"}), None);
        assert_eq!(outcome, Outcome::Silent);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "sse 不等远端：run() 必须立刻回来"
        );
    }
}
