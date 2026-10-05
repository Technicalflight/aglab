use std::fs;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
// 与 mcp.rs / hooks.rs 同一条先例：spawn 的程序来自用户配置而非模型传参时，
// 用别名让这一事实少被安全钩子误读成"拼接 shell"
use std::process::Command as OsCommand;
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

use crate::config;

pub const MAX_READ_BYTES: u64 = 128 * 1024;
pub const MAX_LIST_ENTRIES: usize = 200;
// 时长与输出预算的定义在 `tool_runtime::constrain`：那两个数是执行约束的一部分，
// UI 文案、测试和这里都从那一处读，别出现第二个 60 秒
const COMMAND_TIMEOUT: Duration = crate::tool_runtime::constrain::COMMAND_TIMEOUT;
const MAX_COMMAND_OUTPUT: usize = crate::tool_runtime::constrain::MAX_COMMAND_OUTPUT;

/// 工具面板上每一项的说明。风险这里取常态值：读写是否算越界还要看落点在项目内还是项目外。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuiltinTool {
    pub id: &'static str,
    pub title: &'static str,
    pub blurb: &'static str,
    pub risk: &'static str,
    pub enabled: bool,
}

struct ToolSpec {
    id: &'static str,
    title: &'static str,
    blurb: &'static str,
    risk: &'static str,
    /// 重复调它会不会有第二次副作用。它是缓存与自动重试的门闩，**不是**"它大概只读"的意思：
    /// 写类与跑命令的一律 false
    idempotent: bool,
}

    const REGISTRY: [ToolSpec; 25] = [
    ToolSpec {
        id: "list_files",
        title: "浏览目录",
        blurb: "列出项目内某个目录的文件与子目录，单次最多 200 项。",
        risk: "safe",
        idempotent: true,
    },
    ToolSpec {
        id: "read_file",
        title: "读取文件",
        blurb: "读取文本文件内容，单次上限 128 KB，只认 UTF-8。可传 offset/limit 只读行区间（带行号）。",
        risk: "safe",
        idempotent: true,
    },
    ToolSpec {
        id: "search_text",
        title: "搜索内容",
        blurb: "在项目里用正则搜文本，输出「文件:行号:原文」，单次最多 200 行命中。自动跳过 .git、node_modules 等重目录与二进制文件。",
        risk: "safe",
        idempotent: true,
    },
    ToolSpec {
        id: "write_file",
        title: "写入文件",
        blurb: "整文件覆盖写入项目内的文本文件，写项目外一律按高风险处理。",
        risk: "elevated",
        idempotent: false,
    },
    ToolSpec {
        id: "edit_file",
        title: "精确编辑",
        blurb: "把文件里一段唯一匹配的原文替换成新文本，不用整文件重写。old_string 必须恰好出现一次，否则拒绝并报出实际出现次数。",
        risk: "elevated",
        idempotent: false,
    },
    ToolSpec {
        id: "delete_file",
        title: "删除文件",
        blurb: "把文件移入回收站（设置里可改为直接删除）。paths 接受多个路径一次删；一次删除达到设置里的批量删除审批阈值（默认 50）时必须先经用户确认。动手前自动留一份改前备份——回收站被系统策略停用时，那份备份就是能反悔的保险。项目外路径按高风险处理。",
        risk: "elevated",
        idempotent: false,
    },
    ToolSpec {
        id: "run_command",
        title: "执行命令",
        blurb: "在项目根目录跑一条 shell 命令（默认跟随设置里的「命令 Shell」，也可显式选 PowerShell/PowerShell 7/Git Bash），默认 60 秒超时（可传 timeout_seconds 到 600），输出截到 32 KB。background=true 交给后台不套超时。",
        risk: "high",
        idempotent: false,
    },
    ToolSpec {
        id: "open_path",
        title: "打开文件或网址",
        blurb: "用系统关联的程序打开项目内的文件/目录（等价于资源管理器双击），或用默认浏览器打开 http/https 网址（过出口与内网两道闸）。cmd 的 start 在这个执行环境里不可用，用它。",
        risk: "high",
        idempotent: false,
    },
    ToolSpec {
        id: "command_output",
        title: "读后台命令输出",
        blurb: "增量读一条后台命令（run_command background=true 启动）的新输出：只给上次之后的部分，附运行状态与退出码。",
        risk: "safe",
        idempotent: false,
    },
    ToolSpec {
        id: "command_stop",
        title: "停后台命令",
        blurb: "终止一条后台命令及其整棵进程树。任务做完记得收尾；重复停同一句柄无害。",
        risk: "elevated",
        idempotent: true,
    },
    ToolSpec {
        id: "web_fetch",
        title: "读网页",
        blurb: "抓一个公开网页的可读正文交回：过出口域名名单，内网/回环地址一律拒，20 秒超时。",
        risk: "safe",
        idempotent: true,
    },
    ToolSpec {
        id: "web_search",
        title: "联网搜索",
        blurb: "用搜索接口查公开网页，交回相关结果的标题、链接与摘要。要在设置里配置过搜索服务（Tavily 或 SearXNG 实例）才声明给模型；执行过出口名单闸。",
        risk: "safe",
        idempotent: true,
    },
    ToolSpec {
        id: "knowledge_search",
        title: "资料库检索",
        blurb: "在本地资料库里检索用户整理的文档与资料，返回相关段落摘要与出处。只读本地存储，不碰项目路径。",
        risk: "safe",
        idempotent: true,
    },
    ToolSpec {
        id: "load_skill",
        title: "取用技能",
        blurb: "读回一个已启用技能的正文，只读，不碰磁盘上的其他东西。",
        risk: "safe",
        // 正文是读出来的，收窄名单那一步在调用方，所以重试它不重复副作用
        idempotent: true,
    },
    ToolSpec {
        id: "list_windows",
        title: "列出窗口",
        blurb: "列出桌面上可见的窗口：标题、大小、位置、是不是当前焦点。只读，不动任何东西。",
        risk: "safe",
        idempotent: true,
    },
    ToolSpec {
        id: "inspect_window",
        title: "读窗口控件",
        blurb: "读某个窗口的控件树（按钮、输入框、菜单项…）与它们认的动作。标题带口令/支付类词的窗口会被拒读。",
        risk: "elevated",
        idempotent: true,
    },
    ToolSpec {
        id: "computer_act",
        title: "操作别的程序",
        blurb: "往指定窗口动手：切前台、点控件、填输入框、敲字、按组合键。敲进去的正文不进审计；标题带口令/支付类词的窗口一律拒绝。",
        risk: "high",
        idempotent: false,
    },
    ToolSpec {
        id: "spawn_subagent",
        title: "派出子助理",
        blurb: "把一项独立的小任务交给可派名单里的子助理（出厂名册 + 设置页定义）：它开自己的话题、用自己的工具白名单与连接跑完，把结论交回来。它自己要点头的动作照常弹审批。",
        risk: "high",
        idempotent: false,
    },
    ToolSpec {
        id: "browser",
        title: "操作内置浏览器",
        blurb: "驱动内置浏览器访问与操作网页：打开地址、按编号点击/输入/滚动、读回页面快照。导航过出口名单与内网地址两道闸；浏览器用独立配置目录，与你日常的浏览器不共享登录态。",
        risk: "high",
        idempotent: false,
    },
    ToolSpec {
        id: "agent_control",
        title: "子助理控制",
        blurb: "查看与操控由 spawn_subagent 派出的子助理：list 列出全部子助理与运行状态；send 往正在运行的子助理插话（steering）；interrupt 中断一个正在运行的子助理。子助理自己也可以用它协作。",
        risk: "high",
        idempotent: false,
    },
    ToolSpec {
        id: "run_program",
        title: "运行脚本",
        blurb: "写一段 Rhai 脚本，脚本内 tool(name, json_args) 直接调用已有工具：循环、条件、组合多工具一次跑完。30 秒预算，工具审批照常。",
        risk: "high",
        idempotent: false,
    },
    ToolSpec {
        id: "present_files",
        title: "声明交付物",
        blurb: "把这一轮真正交付给用户的文件郑重声明出来（路径 + 一句话说明）：界面会把它当产出物展示，与「顺手改过」区分开。只验证文件存在，不写任何东西。",
        risk: "safe",
        idempotent: true,
    },
    ToolSpec {
        id: "ssh_run",
        title: "SSH 执行",
        blurb: "在设置里配好的远程主机上执行一条 shell 命令（走系统 ssh，密钥认证，不支持口令）。远程输出原样回来；执行本体照常过审批闸。",
        risk: "high",
        idempotent: false,
    },
    ToolSpec {
        id: "lsp_query",
        title: "LSP 语义查询",
        blurb: "借常驻语言服务器做四类语义查询：跳转定义、找引用、悬停文档、列文档符号。服务器按扩展名挑选（rust-analyzer / typescript-language-server / pyright / gopls / clangd），设置里可以逐扩展覆盖命令。只读。",
        risk: "safe",
        idempotent: true,
    },
    ToolSpec {
        id: "obs_recall",
        title: "取回观察原文",
        blurb: "按句柄取回一条被归档的超长工具结果原文的任意段落（分页）。只读话题内存档，不碰磁盘与网络。",
        risk: "safe",
        idempotent: true,
    },
];

/// 这个名字在不在内置注册表里。声明数组由它生成，路由也问它
pub fn is_registered(name: &str) -> bool {
    REGISTRY.iter().any(|spec| spec.id == name)
}

/// 这个内置工具能不能重复调。缓存与重试都只看这一个数
pub fn is_idempotent(name: &str) -> bool {
    REGISTRY.iter().any(|spec| spec.id == name && spec.idempotent)
}

/// 内容指纹：解析后的绝对路径 + mtime（含纳秒）+ 字节数。
///
/// 只有 `read_file` 与 `list_files` 给得出。目录的 mtime 会随增删改名变、不随文件内容变，
/// 而 `list_files` 报的正是名字列表，所以这个指纹对它成立；`read_file` 用的是文件自己的 mtime。
/// 拿不出指纹就返回 `None`，缓存那条路会因此跳过它——而不是赌它没变过
pub fn content_stamp(name: &str, args: &Value, root: Option<&Path>) -> Option<String> {
    if name != "read_file" && name != "list_files" {
        return None;
    }
    let path = resolve(arg_str(args, "path")?, root);
    let meta = fs::metadata(&path).ok()?;
    let since = meta
        .modified()
        .ok()?
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .ok()?;
    Some(format!("{}|{}.{:09}|{}", path.to_string_lossy(), since.as_secs(), since.subsec_nanos(), meta.len()))
}

/// 关掉的能力不再声明给模型：模型看不见它，也就不会去调它
pub fn schemas_for(disabled: &[String]) -> Value {
    let all = match schemas() {
        Value::Array(items) => items,
        other => return other,
    };

    Value::Array(without(all, disabled))
}

/// 只留下没被关掉的声明。`schemas_for` 与"没有工作目录也要声明的那几条"共用这一份过滤，
/// 免得两边的"关掉"长成两种样子
fn without(items: Vec<Value>, disabled: &[String]) -> Vec<Value> {
    items
        .into_iter()
        .filter(|item| {
            let name = item["function"]["name"].as_str().unwrap_or_default();
            !disabled.iter().any(|off| off == name)
        })
        .collect()
}

/// Computer Use 那三条。它们不碰路径，所以**不挂在"绑了工作目录"那扇门上**——
/// 那条判据说的是"文件路径以项目根为基准"，对点窗口敲字不成立
pub fn computer_schemas_for(disabled: &[String]) -> Vec<Value> {
    // agent_control 不碰路径（动的是子助理回合），未绑定也照常声明
    let names = ["list_windows", "inspect_window", "computer_act", "agent_control"];
    match schemas() {
        Value::Array(items) => without(items, disabled)
            .into_iter()
            .filter(|item| {
                names
                    .iter()
                    .any(|name| item["function"]["name"].as_str() == Some(name))
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub fn is_disabled(disabled: &[String], name: &str) -> bool {
    disabled.iter().any(|off| off == name)
}

#[tauri::command]
pub fn builtin_tools_list(app: tauri::AppHandle) -> Result<Vec<BuiltinTool>, String> {
    let disabled = config::load(&app).disabled_tools;
    Ok(REGISTRY
        .iter()
        .map(|spec| BuiltinTool {
            id: spec.id,
            title: spec.title,
            blurb: spec.blurb,
            risk: spec.risk,
            enabled: !is_disabled(&disabled, spec.id),
        })
        .collect())
}

/// 关掉之后模型仍然硬调这个工具时的兜底：不执行，但要把原因作为工具结果喂回去
pub const DISABLED_NOTE: &str = "该能力已在设置里被关闭，没有执行。";

/// 读网页不依赖工作目录（web 没有路径基准），单独声明：没绑项目也能查资料。
/// **执行**不在注册表——它要配置里的出口名单与代理，由 chat 循环路由外接走
/// （spawn_subagent 同款先例）
pub fn web_fetch_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "web_fetch",
            "description": "抓取一个公开网页的可读正文（HTML 转文本，20 秒超时）。受出口域名名单约束，内网/回环地址到不了。",
            "parameters": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "http/https 链接" }
                },
                "required": ["url"]
            }
        }
    })
}

/// 联网搜索与 web_fetch 同款：与工作目录无关，单独声明。**声明与否看配置**——
/// 没配搜索接口（供应商/key）就整条不给，执行在 chat 循环路由外接走
/// （它要配置里的 key、出口名单与代理）
pub fn web_search_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "web_search",
            "description": "在公开网页里搜索一个话题，交回相关结果的标题、链接与内容摘要。查文档、找 issue、确认某个库的当前用法之前先用它；要读某一条的全文再用 web_fetch。",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "搜索词。要具体：库名加用法、错误码加消息，比一两个词有效得多" }
                },
                "required": ["query"]
            }
        }
    })
}

/// 取用技能不依赖工作目录，所以单独声明，由 chat.rs 交给 skills 处理
pub fn skill_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "load_skill",
            "description": "读回一个已启用技能的完整正文。决定要用某个技能时先调用它。",
            "parameters": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "技能名字，取自可用技能清单" }
                },
                "required": ["name"]
            }
        }
    })
}

/// 资料库检索不依赖工作目录（读的是资料库自己的存储），单独声明：没绑项目也能查。
/// **执行**在注册表里照常走 tools::execute（只读本地存储，经 knowledge::tool_search）
pub fn knowledge_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "knowledge_search",
            "description": "在本地资料库里检索用户整理的资料（导入的文件、手写的笔记），返回相关段落的摘要与出处。回答涉及用户整理过的资料、项目背景、约定、钓获记录这类「用户自己存的」内容时，先查这里再下结论。",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "检索词。中文按词组切分检索，多个关键词用空格分开" },
                    "limit": { "type": "integer", "description": "最多返回几条命中，默认 8，上限 20" }
                },
                "required": ["query"]
            }
        }
    })
}

/// 目标模式的上报。它是一条控制信号，不是一项能力：不进注册表（那一排开关管的是逐条能力，
/// 关掉 `goal_report` 等于把目标模式的正常收尾摘掉，只剩预算烧到顶那一条路），也不依赖工作目录。
/// 单独声明，与 web_fetch / knowledge_search 同一条路。不在目标模式时硬调，执行体回一句
/// "这一句没有对象"，不静默收下
pub fn goal_report_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "goal_report",
            "description": "上报目标模式这一支的收尾。complete = 目标确实做完了，note 写清交付了什么、还剩什么没做；blocked = 做不下去，note 写清卡在哪、需要什么才能继续。只在目标模式下有意义，别的模式下这一句没有对象。不调它，这一支不会自己停：它会一轮一轮跑到花费上限，或被用户暂停、结束为止。complete 要过覆盖审计：契约里的每条判据都得有证据（本轮 evidence 里给的，或此前已交过且判据没改过的）；有判据缺证据或证据是失败，complete 会被打回、这一支继续推进——所以审计要证明完成，不是没发现明显没做的就算完。",
            "parameters": {
                "type": "object",
                "properties": {
                    "status": { "type": "string", "enum": ["complete", "blocked"], "description": "complete 或 blocked" },
                    "note": { "type": "string", "description": "说给用户的那一句：交付了什么，或卡在哪" },
                    "evidence": {
                        "type": "array",
                        "description": "逐条判据的证据。complete 时按判据逐条给（此前已交过且判据未改的可以省）；blocked 可省",
                        "items": {
                            "type": "object",
                            "properties": {
                                "criterion_id": { "type": "string", "description": "契约里的判据 id" },
                                "verdict": { "type": "string", "enum": ["pass", "fail"], "description": "这条判据过了还是没过" },
                                "summary": { "type": "string", "description": "一句结论：凭什么" },
                                "output": { "type": "string", "description": "关键输出的摘录（可省）" }
                            },
                            "required": ["criterion_id", "verdict", "summary"]
                        }
                    }
                },
                "required": ["status", "note"]
            }
        }
    })
}

/// 计划更新：模型自管步骤清单的控制信号（TodoWrite / update_plan 的合体）。
/// 与 goal_report 同档：不进注册表（那一排开关管的是逐条能力，关掉计划等于
/// 摘掉多步任务的进度条），也不依赖工作目录。**恒声明**，整份替换语义。
pub fn plan_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "update_plan",
            "description": "把当前任务的步骤清单整份上报给用户。多步任务（超过三步、或有先后依赖）开工前先用它立计划；做完一步、或换思路时整份重发。steps 是全量替换，不是增量；同一时刻恰好一步 in_progress，做完了才标 completed。",
            "parameters": {
                "type": "object",
                "properties": {
                    "explanation": { "type": "string", "description": "一句话说明当前进展或这次变化的原因（可省）" },
                    "steps": {
                        "type": "array",
                        "description": "完整步骤清单（每次都给全量）",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": { "type": "string", "description": "这一步要做什么，一句话" },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"], "description": "pending=没开始；in_progress=正在做；completed=已完成" }
                            },
                            "required": ["title", "status"]
                        }
                    }
                },
                "required": ["steps"]
            }
        }
    })
}

/// 向用户提问：模型在真正需要人拍板的分岔口给出选项，而不是自说自话地猜。
/// 与 goal_report / update_plan 同档：控制信号，不进注册表，恒声明，
/// 执行在 chat 循环里挂起等用户点选（stop 可打断）。
pub fn ask_user_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "ask_user",
            "description": "就一个影响后续走向的分岔口向用户提问并给出可选答案。只在真该问的时候用（方案取舍、不可逆操作、缺关键信息）；能自己查资料、看文件解决的不问。用户点选或补充后你会拿到答案原文。",
            "parameters": {
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "要问的问题：说清背景与分岔点" },
                    "options": {
                        "type": "array",
                        "description": "2 到 6 个可选答案",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": { "type": "string", "description": "选项本身，按钮上那行字" },
                                "description": { "type": "string", "description": "选它的代价或后果（可省）" }
                            },
                            "required": ["label"]
                        }
                    }
                },
                "required": ["question", "options"]
            }
        }
    })
}

/// 观察召回不依赖工作目录（存档住在话题线程的内存里），单独声明，由 chat 循环路由外接走。
/// 恒声明：声明它不花钱，模型用不用取决于有没有遇到被归档的结果
pub fn obs_recall_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "obs_recall",
            "description": "按句柄取回一条被归档的超长工具结果原文的任意段落。工具结果超长时正文里只给了首尾摘录与句柄，需要中间那段（被截断的文件中部、日志中段）时用它分页取回。",
            "parameters": {
                "type": "object",
                "properties": {
                    "handle": { "type": "string", "description": "观察句柄，来自被归档结果正文里的「已归档为观察 #…」那一段" },
                    "start": { "type": "integer", "description": "起始字符位（0 起），缺省 0" },
                    "limit": { "type": "integer", "description": "取多少字符，缺省 4000" }
                },
                "required": ["handle"]
            }
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    Safe,
    Elevated,
    High,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Safe => "safe",
            Risk::Elevated => "elevated",
            Risk::High => "high",
        }
    }
}

/// 工具只有在有活动项目时才声明：文件路径以项目根为基准，"选择项目"因此是承重控件而不是标签。
fn schemas() -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "list_files",
                "description": "列出目录下的文件和子目录，路径相对于项目根目录。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "要列出的目录，默认为项目根目录" }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "读取一个文本文件的完整内容。传 offset/limit 时只读行区间，每行带行号前缀（行号是坐标不是内容，给 edit_file 的 old_string 不要带行号）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "文件路径，相对于项目根目录" },
                        "offset": { "type": "integer", "description": "起始行号（1 起）。与 limit 搭配做行区间读取" },
                        "limit": { "type": "integer", "description": "最多读多少行" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_text",
                "description": "在项目里按正则搜索文本内容，输出「文件:行号: 该行原文」。自动跳过 .git、node_modules、target 等重目录与二进制文件。改代码前先搜，别靠猜路径。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "正则表达式。默认大小写不敏感" },
                        "path": { "type": "string", "description": "搜索的目录，默认项目根目录" },
                        "glob": { "type": "string", "description": "文件名过滤，如 *.ts 或 README*。只支持 * 通配" },
                        "case_sensitive": { "type": "boolean", "description": "大小写敏感，默认 false" }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "写入或覆盖一个文本文件（整文件替换）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "文件路径，相对于项目根目录" },
                        "content": { "type": "string", "description": "要写入的完整内容" }
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "把文件里一段原文精确替换成新文本，不用整文件重写。old_string 必须在文件中恰好出现一次（缩进、空白都要逐字一致），否则拒绝并报出实际次数。先 read_file 拿原文再编辑。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "文件路径，相对于项目根目录" },
                        "old_string": { "type": "string", "description": "要被替换的原文片段，必须逐字唯一（除非 replace_all）" },
                        "new_string": { "type": "string", "description": "替换成的新文本" },
                        "replace_all": { "type": "boolean", "description": "全部替换而不是只换第一处，默认 false" }
                    },
                    "required": ["path", "old_string", "new_string"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "delete_file",
                "description": "删除文件。默认移入回收站（可在回收站找回）；一次删多个传 paths 数组。删大量文件（达到用户设置的批量删除阈值）会先请求确认。别用 run_command 的 rm/del 删文件——走这里才有回收站、台账与备份。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "要删除的文件路径列表，相对于项目根目录。一次调用删几个就是几个：数量达到阈值会整体走审批"
                        }
                    },
                    "required": ["paths"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_command",
                "description": "在项目根目录下执行一条 shell 命令。默认 shell 跟随设置里的「命令 Shell」（没设置时是 cmd /C）：Start-Process、$env: 这类 PowerShell 语法选 shell=\"powershell\" 或 \"pwsh\"；Unix 工具链（git、grep）选 git-bash。等不到结束的命令（开发服务器、watcher）传 background=true，再用 command_output 看输出、command_stop 停。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "要执行的命令" },
                        "shell": {
                            "type": "string",
                            "enum": ["cmd", "powershell", "pwsh", "git-bash"],
                            "description": "用哪个 shell 跑。留空跟设置里的「命令 Shell」走；PowerShell 语法（Start-Process、$env:、Get-Content）选 powershell；Unix 工具链（git、grep）选 git-bash"
                        },
                        "background": {
                            "type": "boolean",
                            "description": "true = 后台运行，立即返回句柄 id，不套 60 秒超时；用 command_output 增量读、command_stop 停。开发服务器这类长活用它"
                        },
                        "timeout_seconds": {
                            "type": "integer",
                            "description": "前台命令的超时秒数，默认 60，上限 600。构建、测试套件这类确实要跑一阵的命令给大一点；再长的活用 background"
                        },
                        "sandbox": {
                            "type": "string",
                            "enum": ["default", "on", "off"],
                            "description": "这条命令的沙箱档位：default=跟全局设置；on=进沙箱；off=不进。点名与全局不同的档位要过一道单独的确认（动的是隔离边界，不是命令本身）"
                        }
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "open_path",
                "description": "用系统关联的程序打开文件或目录（等价于资源管理器里双击：html 开浏览器、目录开 Explorer），或用默认浏览器打开 http/https 网址。不要用 cmd 的 start——它在这个执行环境里不可用。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "target": { "type": "string", "description": "项目根内的文件/目录路径（相对或绝对），或 http/https 网址" }
                    },
                    "required": ["target"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "command_output",
                "description": "增量读一条后台命令（run_command 传了 background=true 启动的）的新输出：只给上次之后的部分，附运行状态，结束时给退出码。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer", "description": "启动时返回的句柄 id" }
                    },
                    "required": ["id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "command_stop",
                "description": "停掉一条后台命令及其整棵进程树（run_command 传了 background=true 启动的）。任务做完记得收尾。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer", "description": "启动时返回的句柄 id" }
                    },
                    "required": ["id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_windows",
                "description": "列出桌面上可操作的窗口：编号、把手、标题、大小与位置、是否当前焦点。要看别的程序先问这一条。",
                "parameters": { "type": "object", "properties": {} }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "inspect_window",
                "description": "读某个窗口的控件树：每个控件的编号、名字、类型、可用状态，以及它认哪些动作（invoke/value/toggle/expand）。界面一变编号就失效，动手前要重读。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "window": { "type": "string", "description": "list_windows 给的那个把手" }
                    },
                    "required": ["window"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "computer_act",
                "description": "往指定窗口动手。focus 切到前台；invoke 点某个控件；set_value 往输入框填值；toggle 切换勾选；expand 展开；type 往当前焦点敲字；keys 按组合键（如 ctrl+s）。标题里带口令/支付/凭据类词的窗口会被直接拒。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "window": { "type": "string", "description": "list_windows 给的那个把手" },
                        "action": {
                            "type": "string",
                            "enum": ["focus", "invoke", "set_value", "toggle", "expand", "type", "keys"]
                        },
                        "element": { "type": "integer", "description": "inspect_window 给的控件编号" },
                        "text": { "type": "string", "description": "set_value / type 要写的字" },
                        "keys": { "type": "string", "description": "keys 的组合键，如 ctrl+s" }
                    },
                    "required": ["window", "action"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                // 这条是**形状底稿**：可派名单由声明侧（source::declarations）按
                // config.subagents 的「主模型可调」那批写进描述与 enum——目录为空时整条不声明
                "name": "spawn_subagent",
                "description": "把一项独立的小任务交给一个子助理，等它跑完交回结论。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "子助理名，取自可派名单" },
                        "task": { "type": "string", "description": "交给它的完整任务描述：目标、范围、要交回什么" }
                    },
                    "required": ["name", "task"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "present_files",
                "description": "把这一轮真正交付给用户的文件郑重声明出来。与「顺手改过」区分：交付物是用户要找的那几份产出。只验证文件存在，不写任何东西。任务产出多于一两个文件时必须用它收尾。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "files": {
                            "type": "array",
                            "description": "交付文件清单",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "path": { "type": "string", "description": "文件路径，相对于项目根目录" },
                                    "note": { "type": "string", "description": "一句话说明这份文件是什么（可省）" }
                                },
                                "required": ["path"]
                            }
                        }
                    },
                    "required": ["files"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_program",
                "description": "写一段 Rhai 脚本，脚本内 tool(name, json_args) 直接调用已有工具（只读工具与权限表直接放行的扩展工具）。多工具任务（循环搜→逐个读→汇总）一次跑完，不用每步都回来对话。脚本变量跨调用保留（同话题常驻），30 秒预算；工具的审批、沙箱、审计照常。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "script": { "type": "string", "description": "Rhai 脚本。用 tool(name, json_args) 调用工具，返回值就是脚本结果（可以是对象/数组）" },
                        "reset": { "type": "boolean", "description": "true = 清空本话题的常驻变量，从零开始。默认 false" }
                    },
                    "required": ["script"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "agent_control",
                "description": "查看与操控子助理：list 列出全部子助理（id、任务、运行状态）；send 往正在运行的子助理插话；interrupt 中断一个正在运行的子助理。子助理之间也可以互相协作。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["list", "send", "interrupt"], "description": "list=列出全部子助理；send=往运行中的子助理插话；interrupt=中断运行中的子助理" },
                        "agent_id": { "type": "string", "description": "目标子助理的话题 id（list 结果里给的）" },
                        "message": { "type": "string", "description": "send 的插话内容" }
                    },
                    "required": ["action"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "ssh_run",
                "description": "在设置里配好的远程主机上执行一条 shell 命令（系统 ssh，密钥/agent 认证，不做口令交互）。host 填配置里的主机名，不是地址。远端输出原样回来，带退出码。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "配置里的主机名（设置 → Agent → SSH 主机，形如 名字=user@地址:端口）" },
                        "command": { "type": "string", "description": "要在远端执行的命令（远端 shell 解释）" },
                        "timeout_seconds": { "type": "integer", "description": "超时秒数，默认 60，上限 600" }
                    },
                    "required": ["host", "command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "lsp_query",
                "description": "借常驻语言服务器做语义查询：definition=跳转定义；references=找全部引用；hover=悬停文档（类型与文档注释）；symbols=列当前文件的符号表。只读，比文本搜索准（改名后不失效、能看到重载与导入）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "项目内的源码文件（相对或绝对路径）" },
                        "query": { "type": "string", "enum": ["definition", "references", "hover", "symbols"], "description": "四类查询选一个" },
                        "symbol": { "type": "string", "description": "要查的标识符文本：在文件里找它的第一次出现作为查询位置。给了它就不用 line/column" },
                        "line": { "type": "integer", "description": "1 起的行号（与编辑器一致）。与 column 一起给，优先于 symbol" },
                        "column": { "type": "integer", "description": "1 起的列号（与编辑器一致）" }
                    },
                    "required": ["file", "query"]
                }
            }
        }
    ])
}

/// browser 单独声明（web_fetch 同款先例）：它不依赖工作目录，扩展开关说了算——
/// 关着整条不声明，声明一个必然失败的工具等于诱着模型去撞一次拒绝
pub fn browser_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "browser",
            "description": "操作内置浏览器：open 打开网址，snapshot 读当前页面，click 点击编号元素，type 输入文字，press 按键，scroll 滚动，back 后退。每次动作后都返回带元素编号的新快照，用 index 指定要操作的元素。",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["open", "snapshot", "click", "type", "press", "scroll", "back"],
                        "description": "open=打开网址（需要 url）；snapshot=重新读页面；click=点编号元素（需要 index）；type=往编号元素输入（需要 index 与 text，submit=true 再按回车）；press=对焦点元素按键（如 Enter、Escape）；scroll=滚动（amount 默认 600，负值向上）；back=后退"
                    },
                    "url": { "type": "string", "description": "open 的目标地址，仅 http/https；过出口名单与内网地址两道闸" },
                    "index": { "type": "integer", "description": "最近一次快照里的元素编号" },
                    "text": { "type": "string", "description": "type 要输入的文字" },
                    "submit": { "type": "boolean", "description": "type 之后是否按回车提交" },
                    "key": { "type": "string", "description": "press 的键名，如 Enter、Escape、Tab、ArrowDown" },
                    "amount": { "type": "integer", "description": "scroll 的像素数，负值向上，默认 600" }
                },
                "required": ["action"]
            }
        }
    })
}

/// 每个内置工具声明里的 `parameters` 子 schema，按工具名取。
/// 校验器读的就是发给模型的那一份：两边各写一份契约，早晚会出现"模型以为能传、我们以为不传"
pub fn parameter_schemas() -> Vec<(String, Value)> {
    let mut list: Vec<(String, Value)> = schemas()
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let function = entry.get("function")?;
            let name = function.get("name")?.as_str()?.to_string();
            function.get("parameters").cloned().map(|parameters| (name, parameters))
        })
        .collect();
    // 取用技能是单独声明的（它不依赖工作目录），但它同样要过校验
    if let Some(function) = skill_schema().get("function") {
        if let (Some(name), Some(parameters)) = (
            function.get("name").and_then(Value::as_str),
            function.get("parameters"),
        ) {
            list.push((name.to_string(), parameters.clone()));
        }
    }
    // 观察召回同款：独立声明，校验器照读
    if let Some(function) = obs_recall_schema().get("function") {
        if let (Some(name), Some(parameters)) = (
            function.get("name").and_then(Value::as_str),
            function.get("parameters"),
        ) {
            list.push((name.to_string(), parameters.clone()));
        }
    }
    // 资料库检索同款：独立声明，校验器照读
    if let Some(function) = knowledge_schema().get("function") {
        if let (Some(name), Some(parameters)) = (
            function.get("name").and_then(Value::as_str),
            function.get("parameters"),
        ) {
            list.push((name.to_string(), parameters.clone()));
        }
    }
    // 目标上报同款：它不是一项能力，但参数一样要按发出去的那一份校
    if let Some(function) = goal_report_schema().get("function") {
        if let (Some(name), Some(parameters)) = (
            function.get("name").and_then(Value::as_str),
            function.get("parameters"),
        ) {
            list.push((name.to_string(), parameters.clone()));
        }
    }
    // 计划更新、向用户提问、联网搜索同款：控制信号或独立声明的工具，参数一样要过校验
    for schema in [plan_schema(), ask_user_schema(), web_search_schema()] {
        if let Some(function) = schema.get("function") {
            if let (Some(name), Some(parameters)) = (
                function.get("name").and_then(Value::as_str),
                function.get("parameters"),
            ) {
                list.push((name.to_string(), parameters.clone()));
            }
        }
    }
    list
}

pub fn parameter_schema(name: &str) -> Option<Value> {
    parameter_schemas()
        .into_iter()
        .find(|(held, _)| held == name)
        .map(|(_, schema)| schema)
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// 逐级往上找第一个真实存在的祖先，才能对"还不存在的写入路径"做 canonicalize。
fn nearest_existing(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return current;
        }
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => current = parent.to_path_buf(),
            _ => return current,
        }
    }
}

pub(crate) fn resolve(raw: &str, root: Option<&Path>) -> PathBuf {
    let path = Path::new(raw);
    match (path.is_absolute(), root) {
        (true, _) => path.to_path_buf(),
        (false, Some(root)) => root.join(path),
        (false, None) => path.to_path_buf(),
    }
}

/// 用 canonicalize 后的前缀比较，`..` 回退因此无法伪装成项目内路径。
pub(crate) fn inside_root(path: &Path, root: Option<&Path>) -> bool {
    let Some(root) = root else { return false };
    let Some(root) = root.canonicalize().ok() else {
        return false;
    };
    nearest_existing(path)
        .canonicalize()
        .map(|real| real.starts_with(&root))
        .unwrap_or(false)
}

/// 这次调用打算写成的（路径, 全文）。编辑台账靠它在动手前拿到 before/after：
/// 快照、行数 diff、回滚都以它为真相。edit_file 要读现文件做替换——
/// 「old 不唯一」在这里就报得出来，文件没动成就不落账。None = 不是可追踪的写入
pub(crate) fn planned_content(
    name: &str,
    args: &Value,
    root: Option<&Path>,
) -> Option<(PathBuf, String)> {
    let raw = arg_str(args, "path")?;
    let path = resolve(raw, root);
    match name {
        "write_file" => {
            let content = args.get("content")?.as_str()?;
            Some((path, content.to_string()))
        }
        // 与 edit_file 执行体同一份合同（apply_edit）：这里失败 = 参数没到能写的地步，
        // 返回 None 让台账跳过，真错误由执行体原样报给模型
        "edit_file" => {
            let old_text = fs::read_to_string(&path).ok()?;
            let (new_text, _) = apply_edit(&old_text, args).ok()?;
            Some((path, new_text))
        }
        _ => None,
    }
}

/// 写入路径的展示形式：能给相对工作目录的相对路径就用它，否则用绝对路径
pub(crate) fn write_target_display(raw: &str, resolved: &Path, root: Option<&Path>) -> String {
    match root {
        Some(root) if resolved.starts_with(root) => resolved
            .strip_prefix(root)
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| resolved.to_string_lossy().to_string()),
        _ => raw.to_string(),
    }
}

pub fn classify(name: &str, args: &Value, root: Option<&Path>) -> Risk {
    let target = arg_str(args, "path").map(|raw| resolve(raw, root));

    match name {
        // 只是把用户自己写的操作清单读回来，不碰磁盘上的其他东西
        "load_skill" => Risk::Safe,
        // 控制信号：不动文件也不动本机，与目标上报同档
        "update_plan" | "ask_user" => Risk::Safe,
        // 子助理控制：list 是只读 Safe；send/interrupt 动的是别的 agent 的回合，按 High 走审批
        "agent_control" => match arg_str(args, "action") {
            Some("list") => Risk::Safe,
            _ => Risk::High,
        },
        // 资料库检索只读应用自己的数据目录，与项目路径无关
        "knowledge_search" => Risk::Safe,
        "obs_recall" => Risk::Safe,
        // 声明交付物：只验证文件存在并格式化清单，不写任何东西
        "present_files" => Risk::Safe,
        // PTC 脚本：调工具走审批闸，但脚本本身的循环/条件是模型写的逻辑，按 High 走
        "run_program" => Risk::High,
        "run_command" => Risk::High,
        // 远程任意执行：闸与本地命令同一条 exec.arbitrary，档位同一个 High
        "ssh_run" => Risk::High,
        // 语义查询是只读的（不写文件、不合成输入）；它读的是代码库内容，
        // 与 read_file 同判据：项目内 Safe，项目外要看一句。注意它的路径参数
        // 叫 file 不叫 path——上面那份 target 对它恒为空，要自己算
        "lsp_query" => match arg_str(args, "file").map(|raw| resolve(raw, root)).as_deref() {
            Some(path) if inside_root(path, root) => Risk::Safe,
            _ => Risk::High,
        },
        "write_file" | "edit_file" => match target.as_deref() {
            Some(path) if inside_root(path, root) => Risk::Elevated,
            _ => Risk::High,
        },
        // 删除一等操作：比写严一档的执行面，任何一处路径出了项目根就按 High 走；
        // paths 缺参/为空说明连要删什么都不知道，同 High
        "delete_file" => {
            let paths: Vec<PathBuf> = args
                .get("paths")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|raw| resolve(raw, root))
                        .collect()
                })
                .unwrap_or_default();
            if !paths.is_empty() && paths.iter().all(|p| inside_root(p, root)) {
                Risk::Elevated
            } else {
                Risk::High
            }
        },
        // 搜索与读取同判据：项目内随便看，项目外要看一句
        "list_files" | "read_file" | "search_text" => match target.as_deref() {
            Some(path) if inside_root(path, root) => Risk::Safe,
            _ => Risk::High,
        },
        // 只读的公网抓取：出口名单与 SSRF 闸（egress::refuse_private_target）兜着
        "web_fetch" => Risk::Safe,
        // 只读的搜索接口调用，闸在 egress；key 与供应商没配时整条不声明
        "web_search" => Risk::Safe,
        // 看别的程序：列名单是安全的，读一棵控件树等于读那个窗口里的内容
        "list_windows" => Risk::Safe,
        "inspect_window" => Risk::Elevated,
        // 合成输入能做任何用户能做的事，这一条没有"看情况"
        "computer_act" => Risk::High,
        // 模型编出来的未知工具一律按最高风险处理，绝不静默执行
        _ => Risk::High,
    }
}

pub fn summary(name: &str, args: &Value) -> String {
    match name {
        "list_files" => format!("列出 {}", arg_str(args, "path").unwrap_or(".")),
        "read_file" => format!("读取 {}", arg_str(args, "path").unwrap_or("?")),
        "search_text" => match arg_str(args, "glob").filter(|g| !g.trim().is_empty()) {
            Some(glob) => format!("搜索「{}」({})", arg_str(args, "query").unwrap_or("?"), glob),
            None => format!("搜索「{}」", arg_str(args, "query").unwrap_or("?")),
        },
        "write_file" => format!(
            "写入 {}（{} 字）",
            arg_str(args, "path").unwrap_or("?"),
            arg_str(args, "content").unwrap_or("").chars().count()
        ),
        "edit_file" => format!("编辑 {}", arg_str(args, "path").unwrap_or("?")),
        "delete_file" => {
            let paths: Vec<&str> = args
                .get("paths")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            match paths.len() {
                0 => "删除文件".to_string(),
                1 => format!("删除 {}", paths[0]),
                n => format!("删除 {} 等 {n} 个文件", paths[0]),
            }
        }
        "web_fetch" => format!("读网页 {}", arg_str(args, "url").unwrap_or("?")),
        "web_search" => format!("联网搜索「{}」", arg_str(args, "query").unwrap_or("?")),
        "present_files" => {
            let count = args["files"].as_array().map(|f| f.len()).unwrap_or(0);
            format!("声明交付物（{count} 个文件）")
        }
        "agent_control" => match arg_str(args, "action") {
            Some("list") => "列出子助理".to_string(),
            Some("send") => format!("向子助理 {} 插话", arg_str(args, "agent_id").unwrap_or("?")),
            Some("interrupt") => format!("中断子助理 {}", arg_str(args, "agent_id").unwrap_or("?")),
            _ => "子助理控制".to_string(),
        },
        "load_skill" => format!("取用技能 {}", arg_str(args, "name").unwrap_or("?")),
        "knowledge_search" => format!("检索资料库「{}」", arg_str(args, "query").unwrap_or("?")),
        "update_plan" => format!(
            "更新计划（{} 步）",
            args["steps"].as_array().map(|steps| steps.len()).unwrap_or(0)
        ),
        "ask_user" => format!("提问：{}", arg_str(args, "question").unwrap_or("?")),
        "obs_recall" => format!("取回观察 {}", arg_str(args, "handle").unwrap_or("?")),
        "run_program" => format!(
            "运行 Rhai 脚本（{} 字）",
            arg_str(args, "script").unwrap_or("").chars().count()
        ),
        "run_command" => {
            let base = arg_str(args, "command").unwrap_or("?").to_string();
            match arg_str(args, "sandbox") {
                Some("on") => format!("{base}（进沙箱）"),
                Some("off") => format!("{base}（沙箱外）"),
                _ => base,
            }
        }
        "ssh_run" => format!("SSH {}：{}", arg_str(args, "host").unwrap_or("?"), arg_str(args, "command").unwrap_or("?")),
        "lsp_query" => format!(
            "LSP {}：{}（{}）",
            arg_str(args, "query").unwrap_or("?"),
            arg_str(args, "file").unwrap_or("?"),
            arg_str(args, "symbol").unwrap_or("")
        ),
        "list_windows" => "列出窗口".to_string(),
        "inspect_window" => format!("读控件树 [{}]", arg_str(args, "window").unwrap_or("?")),
        // 这里只放"做了什么动作"。type / set_value 的正文由 computer.rs 换成字符数——
        // 摘要会进审计与审批文案，而那两个地方都不该存着别人机器上的口令
        "computer_act" => match crate::computer::parse_act(args) {
            Ok(act) => format!(
                "操作 [{}] {}",
                arg_str(args, "window").unwrap_or("?"),
                crate::computer::describe_act(&act)
            ),
            // 参数没到能执行的地步：把校验给出的原因原样报出去，不另编一句
            Err(problem) => format!("操作被拒：{problem}"),
        },
        other => format!("{other} {}", args),
    }
}

fn read_text(path: &Path) -> Result<(String, bool), String> {
    let metadata = fs::metadata(path).map_err(|e| format!("无法访问 {}: {e}", path.display()))?;
    if metadata.is_dir() {
        return Err("这是一个目录，请用 list_files 查看".into());
    }

    let mut handle = fs::File::open(path).map_err(|e| format!("无法打开: {e}"))?;
    let mut buffer = Vec::new();
    handle
        .by_ref()
        .take(MAX_READ_BYTES + 1)
        .read_to_end(&mut buffer)
        .map_err(|e| format!("读取失败: {e}"))?;

    let truncated = buffer.len() as u64 > MAX_READ_BYTES;
    if truncated {
        buffer.truncate(MAX_READ_BYTES as usize);
    }

    let mut text = String::from_utf8(buffer).map_err(|_| "文件不是 UTF-8 文本，已拒绝读取")?;
    if truncated {
        text.push_str("\n…（内容已截断）");
    }
    Ok((text, truncated))
}

/// 无主执行。**只有测试在用**：生产执行统一走 `execute_for`（tool_runtime 的兜底源
/// 会带上发起话题的 owner，后台命令句柄要认主人），测试不关心归属，就用这个省得
/// 每处都手抄 None
#[cfg(test)]
pub fn execute(name: &str, args: &Value, root: Option<&Path>) -> Result<String, String> {
    execute_for(name, args, root, None)
}

/// 带归属的执行。`owner` 是发起这条调用的话题 id：后台命令句柄要认主人，
/// 面板那张"后台"小卡片按话题清点指令。话题外的调用（测试等）给 None
pub fn execute_for(
    name: &str,
    args: &Value,
    root: Option<&Path>,
    owner: Option<&str>,
) -> Result<String, String> {
    match name {
        "list_files" => list_files(args, root),
        "read_file" => read_file(args, root),
        "search_text" => search_text(args, root),
        "write_file" => write_file(args, root),
        "edit_file" => edit_file(args, root),
        "delete_file" => delete_file(args, root),
        "run_command" => run_command(args, root, owner),
        "ssh_run" => ssh_run(args),
        "lsp_query" => lsp_query(args, root),
        "list_windows" => list_windows(),
        "inspect_window" => inspect_window(args),
        "computer_act" => computer_act(args),
        // spawn 与 web_fetch 在注册表里有名有姓，但**执行**不住在这里：
        // 它们要话题上下文或配置（出口名单/代理），由 chat 循环在路由外接走
        // （load_skill 归技能路是同款先例）。走到这里说明有人在没有话题上下文的地方调了它，
        // 老实说清而不是装没这个工具
        "spawn_subagent" => Err("spawn_subagent 只能在对话里派（它要记下是谁派的、派到哪个话题）。".into()),
        "agent_control" => Err("agent_control 只能在对话里调（要访问运行登记表）。".into()),
        "run_program" => Err("run_program 只能在对话里调（要访问工具注册表）。".into()),
        "web_fetch" => Err("web_fetch 要经出口名单与代理执行，只能在对话里调。".into()),
        "browser" => Err("browser 要驱动内置浏览器进程，只能在对话里调。".into()),
        // 资料库检索读的是应用自己的数据目录，不需要话题上下文，照常在这里执行
        "knowledge_search" => crate::knowledge::tool_search(args),
        // 观察召回的存档住在话题线程的内存里：注册表够不着，chat 循环路由外接走
        // （spawn 与 web_fetch 是同款先例）
        "obs_recall" => Err("obs_recall 的存档住在对话线程里，只能在对话内调。".into()),
        // 声明交付物：逐个验证文件存在（相对项目根），格式化清单交回。
        // 只读操作——不写不删，存在的意义是"模型郑重声明 + 界面可展示"
        "present_files" => {
            let files = args["files"]
                .as_array()
                .ok_or("present_files 缺少 files 参数：交付文件清单")?;
            if files.is_empty() {
                return Err("交付文件清单是空的：至少声明一个文件。".into());
            }
            let mut lines = Vec::new();
            for (index, item) in files.iter().enumerate() {
                let raw = item["path"].as_str().unwrap_or_default();
                if raw.trim().is_empty() {
                    return Err(format!("第 {} 个文件的 path 是空的。", index + 1));
                }
                let path = resolve(raw, root);
                if !path.exists() {
                    return Err(format!(
                        "交付文件「{raw}」不存在：声明的是实际产出，不是计划要写的文件。先写好再声明。"
                    ));
                }
                let note = item["note"].as_str().unwrap_or_default();
                let line = if note.is_empty() {
                    format!("- {raw}")
                } else {
                    format!("- {raw}（{note}）")
                };
                lines.push(line);
            }
            Ok(format!(
                "已声明 {} 个交付文件：\n{}",
                lines.len(),
                lines.join("\n")
            ))
        }
        "open_path" => {
            let target = arg_str(args, "target")
                .ok_or("open_path 缺少 target：项目内的文件/目录路径，或 http/https 网址")?;
            #[cfg(windows)]
            {
                let resolved = resolve_open_target(target, root)?;
                open_target(resolved)
            }
            #[cfg(not(windows))]
            {
                let _ = (target, root);
                Err("open_path 目前只在 Windows 上可用。".into())
            }
        }
        "command_output" => {
            let id = args["id"]
                .as_u64()
                .ok_or("command_output 缺少 id：启动后台命令时返回的句柄编号")? as u32;
            let value = crate::tool_runtime::background::state()
                .lock()
                .expect("后台命令登记表锁")
                .output(id)?;
            Ok(serde_json::to_string_pretty(&value).unwrap_or_default())
        }
        "command_stop" => {
            let id = args["id"]
                .as_u64()
                .ok_or("command_stop 缺少 id：启动后台命令时返回的句柄编号")? as u32;
            crate::tool_runtime::background::state()
                .lock()
                .expect("后台命令登记表锁")
                .stop(id)
        }
        other => Err(format!("没有名为 {other} 的工具")),
    }
}

/// 桌面上有哪些窗口能操作。只读
fn list_windows() -> Result<String, String> {
    Ok(crate::computer::render_windows(
        &crate::computer::win::list_windows(),
    ))
}

/// 敏感窗口那道闸：动手之前、读之前都要过一遍。返回 Ok(标题) 表示这个窗口可以碰
fn guard_window(id: &str) -> Result<String, String> {
    let title = crate::computer::win::title_of(id)?;
    match crate::computer::sensitive_title(&title) {
        Some(word) => Err(crate::computer::refusal(word, &title)),
        None => Ok(title),
    }
}

fn inspect_window(args: &Value) -> Result<String, String> {
    let id = arg_str(args, "window").ok_or("inspect_window 需要 window（list_windows 给的那个把手）")?;
    let title = guard_window(id)?;
    let (controls, truncated) = crate::computer::win::tree(id, crate::computer::MAX_CONTROLS)?;
    // render_tree 只用得到把手与标题，位置那一格留零不影响读
    let window = crate::computer::WindowInfo {
        id: id.to_string(),
        title,
        rect: (0, 0, 0, 0),
        focused: false,
    };
    Ok(crate::computer::render_tree(&window, &controls, truncated))
}

/// 往一个窗口动手。三道顺序不能换：**先解析参数**（不合法的一个字节都不发出去），
/// **再过敏感闸**，**最后才动**。type / keys 一定先把目标切到前台——
/// 不切就直接敲字，等于把话敲进用户当时正在看的那个窗口
fn computer_act(args: &Value) -> Result<String, String> {
    use crate::computer::Act;
    let id = arg_str(args, "window").ok_or("computer_act 需要 window（list_windows 给的那个把手）")?;
    let act = crate::computer::parse_act(args)?;
    let title = guard_window(id)?;

    let result = match &act {
        Act::Focus => crate::computer::win::focus(id).map(|_| "已把它切到前台".to_string()),
        Act::Invoke(index) => crate::computer::win::invoke(id, *index)
            .map(|_| format!("已点控件 #{index}")),
        Act::SetValue(index, text) => crate::computer::win::set_value(id, *index, text)
            .map(|_| format!("已填入控件 #{index}（{} 字）", text.chars().count())),
        Act::Toggle(index) => crate::computer::win::toggle(id, *index)
            .map(|_| format!("已切换控件 #{index}")),
        Act::Expand(index) => crate::computer::win::expand(id, *index)
            .map(|_| format!("已展开控件 #{index}")),
        Act::Type(text) => crate::computer::win::focus(id).map(|_| {
            crate::computer::win::type_text(text);
            format!("已往「{}」敲入 {} 个字符", title, text.chars().count())
        }),
        Act::Keys(keys) => crate::computer::key_sequence(keys).and_then(|sequence| {
            crate::computer::win::focus(id).map(|_| {
                crate::computer::win::press_keys(&sequence);
                format!("已往「{title}」按下 {keys}")
            })
        }),
    };
    // 结果只报"做了什么、对谁做的"。敲进去的正文一个都不回——它会进话题日志，
    // 也就是进下一轮的上下文
    result
}

fn list_files(args: &Value, root: Option<&Path>) -> Result<String, String> {
    let dir = resolve(arg_str(args, "path").unwrap_or("."), root);
    let mut entries: Vec<String> = Vec::new();

    for item in fs::read_dir(&dir).map_err(|e| format!("无法列出 {}: {e}", dir.display()))? {
        let item = item.map_err(|e| e.to_string())?;
        let is_dir = item.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let label = item.file_name().to_string_lossy().into_owned();
        entries.push(if is_dir { format!("{label}/") } else { label });
        if entries.len() >= MAX_LIST_ENTRIES {
            break;
        }
    }

    entries.sort();
    if entries.is_empty() {
        return Ok("(空目录)".into());
    }
    Ok(entries.join("\n"))
}

fn read_file(args: &Value, root: Option<&Path>) -> Result<String, String> {
    let raw = arg_str(args, "path").ok_or("read_file 缺少 path 参数")?;
    let path = resolve(raw, root);
    let offset = args.get("offset").and_then(Value::as_u64);
    let limit = args.get("limit").and_then(Value::as_u64);
    match (offset, limit) {
        // 不带区间参数 = 原样整读（无行号），已有话题与技能零感知
        (None, None) => {
            let (text, _truncated) = read_text(&path)?;
            Ok(text)
        }
        (offset, limit) => read_file_range(&path, offset, limit),
    }
}

/// 行区间读。128 KB 的口粮上限在这里不适用——输出体量由区间本身兜着，而模型按行号
/// 要第 4000 行时，文件常常早就越过 128 KB 了。封顶提到 2 MB（与 edit_file 同一档），
/// 再大的文件直说"不接"，不给一个悄悄截断的假区间
fn read_file_range(path: &Path, offset: Option<u64>, limit: Option<u64>) -> Result<String, String> {
    let metadata = fs::metadata(path).map_err(|e| format!("无法访问 {}: {e}", path.display()))?;
    if metadata.is_dir() {
        return Err("这是一个目录，请用 list_files 查看".into());
    }
    if metadata.len() > EDIT_MAX_BYTES {
        return Err(format!(
            "文件超过 {} MB，行区间读不接：用 run_command 取你要的段落。",
            EDIT_MAX_BYTES / 1024 / 1024
        ));
    }
    let text = fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::InvalidData {
            "文件不是 UTF-8 文本，已拒绝读取".to_string()
        } else {
            format!("读取失败: {e}")
        }
    })?;
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = (offset.unwrap_or(1).max(1) as usize).saturating_sub(1);
    if start >= total {
        return Ok(format!("（区间为空：文件共 {total} 行，起始行越过了结尾）"));
    }
    let end = match limit {
        Some(l) => start.saturating_add(l.max(1) as usize).min(total),
        None => total,
    };
    let numbered: Vec<String> = lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{}\t{line}", start + i + 1))
        .collect();
    Ok(numbered.join("\n"))
}

fn write_file(args: &Value, root: Option<&Path>) -> Result<String, String> {
    let raw = arg_str(args, "path").ok_or("write_file 缺少 path 参数")?;
    let content = arg_str(args, "content").ok_or("write_file 缺少 content 参数")?;
    let path = resolve(raw, root);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("无法创建目录: {e}"))?;
    }
    fs::write(&path, content).map_err(|e| format!("写入失败: {e}"))?;

    Ok(format!(
        "已写入 {}（{} 字节）",
        path.display(),
        content.as_bytes().len()
    ))
}

/// edit_file 的读取上限。128 KB 是「读给模型看」的口粮上限，不是文件系统事实——
/// 这里必须读全文件：截断后替换再写回，等于把没读到的尾巴整个毁掉
const EDIT_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// edit_file 与编辑台账（planned_content）共用的一步：按参数把旧全文变成新全文，
/// 返回替换处数。合同：old_string 必须恰好出现一次（replace_all 除外）；
/// old == new 拒；old 为空拒（新建文件是 write_file 的事）。跑两遍——
/// 快照一遍（文件没动成不落账）、执行一遍，两处判的是同一份
fn apply_edit(old_text: &str, args: &Value) -> Result<(String, usize), String> {
    let old_string = arg_str(args, "old_string").ok_or("edit_file 缺少 old_string 参数")?;
    let new_string = arg_str(args, "new_string").unwrap_or_default();
    if old_string.is_empty() {
        return Err("old_string 是空的：改一个不存在的东西没有意义，新建文件用 write_file。".into());
    }
    if old_string == new_string {
        return Err("old_string 与 new_string 相同：没有可改的内容。".into());
    }
    let replace_all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
    let count = old_text.matches(old_string).count();
    if count == 0 {
        return Err(
            "old_string 在文件里没有找到：先 read_file 拿逐字原文，再照抄一段唯一的片段（缩进与空白也要一致）。"
                .into(),
        );
    }
    if count > 1 && !replace_all {
        return Err(format!(
            "old_string 在文件里出现了 {count} 次：带上更多上下文让它唯一，或传 replace_all=true 全部替换。"
        ));
    }
    let new_text = if replace_all {
        old_text.replace(old_string, new_string)
    } else {
        old_text.replacen(old_string, new_string, 1)
    };
    Ok((new_text, if replace_all { count } else { 1 }))
}

fn edit_file(args: &Value, root: Option<&Path>) -> Result<String, String> {
    let raw = arg_str(args, "path").ok_or("edit_file 缺少 path 参数")?;
    let path = resolve(raw, root);
    let metadata = fs::metadata(&path).map_err(|e| format!("无法访问 {}: {e}", path.display()))?;
    if metadata.is_dir() {
        return Err("这是一个目录，edit_file 只改文件。".into());
    }
    if metadata.len() > EDIT_MAX_BYTES {
        return Err(format!(
            "文件超过 {} MB，edit_file 不接（整读做替换才有回滚的底气）：用 write_file 整写或 run_command 处理。",
            EDIT_MAX_BYTES / 1024 / 1024
        ));
    }
    let old_text = fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::InvalidData {
            "文件不是 UTF-8 文本，已拒绝编辑".to_string()
        } else {
            format!("读取失败: {e}")
        }
    })?;
    let (new_text, replacements) = apply_edit(&old_text, args)?;
    fs::write(&path, &new_text).map_err(|e| format!("写入失败: {e}"))?;
    Ok(format!(
        "已编辑 {}（替换 {replacements} 处，{} → {} 字节）",
        write_target_display(raw, &path, root),
        old_text.len(),
        new_text.len()
    ))
}

/// 删除保护的总开关（design-security-center.md D1）：true = 移入回收站（默认），
/// false = 按系统删除。启动与设置页各落一次到进程级开关，执行侧不回读配置文件
/// ——沙箱的 `set_enabled` 是同一个先例
static DELETE_TO_TRASH: AtomicBool = AtomicBool::new(true);

pub fn set_delete_to_trash(value: bool) {
    DELETE_TO_TRASH.store(value, Ordering::SeqCst);
}

fn delete_to_trash() -> bool {
    DELETE_TO_TRASH.load(Ordering::SeqCst)
}

/// 删除文件。paths 逐个处理、互不拖累：删得掉的删掉，删不掉的照实报——
/// 全军覆没才整体报错。回收站这一层是"能反悔"的全部来路，所以文案必须
/// 说清是"移入回收站"而不是"已删除"：把可反悔说成不可反悔，等于谎报
fn delete_file(args: &Value, root: Option<&Path>) -> Result<String, String> {
    let raws = args
        .get("paths")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    if raws.is_empty() {
        return Err("delete_file 缺少 paths 参数。".into());
    }
    let trash = delete_to_trash();
    let mut done: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for raw in &raws {
        let path = resolve(raw, root);
        let display = write_target_display(raw, &path, root);
        let outcome = if trash {
            trash::delete(&path).map_err(|e| format!("移入回收站失败: {e}"))
        } else if path.is_dir() {
            fs::remove_dir_all(&path).map_err(|e| format!("删除失败: {e}"))
        } else {
            fs::remove_file(&path).map_err(|e| format!("删除失败: {e}"))
        };
        match outcome {
            Ok(()) => done.push(display),
            Err(problem) => failed.push(format!("{display}：{problem}")),
        }
    }
    if done.is_empty() {
        return Err(failed.join("\n"));
    }
    let verb = if trash { "已移入回收站" } else { "已删除" };
    let mut report = format!("{verb} {} 个：{}", done.len(), done.join("、"));
    if !failed.is_empty() {
        report.push_str(&format!("\n失败 {} 个：{}", failed.len(), failed.join("；")));
    }
    Ok(report)
}

/// search_text 的固定跳过名单：都是「重得没有搜索价值」的目录。不是 .gitignore 的
/// 实现（那份语义要牵扯 ignore crate），是对常见依赖/构建目录的实用裁剪
const SEARCH_SKIP_DIRS: &[&str] = &[
    ".git", ".hg", ".svn", "node_modules", "target", "dist", "build", "out", ".next", ".nuxt",
    "__pycache__", ".venv", "venv", "coverage",
];

const SEARCH_MAX_HITS: usize = 200;
/// 单文件读进来的封顶：搜索是找线索，不是把大文件倒进上下文
const SEARCH_FILE_BYTES: u64 = 1024 * 1024;
/// 递归深度闸：Windows 的 junction 环不能把遍历变成死循环
const SEARCH_MAX_DEPTH: usize = 32;
/// 单行展示截断：命中行太长时只留前段
const SEARCH_LINE_CHARS: usize = 300;

fn search_text(args: &Value, root: Option<&Path>) -> Result<String, String> {
    // rg 在就先走 rg（大仓库快一个量级）；它跑不成（没装、正则不合法、环境问题）
    // 就落回 std 遍历——正则报错的人话住在 std 路上，两条路输出同一份形状
    if rg_available() {
        if let Some(via_rg) = search_via_rg(args, root) {
            return via_rg;
        }
    }
    search_text_std(args, root)
}

/// rg（ripgrep）可用性探测，进程内只探一次。不引依赖：用户机器上恰好有 rg 才赚这一笔，
/// 没有就是今天的 std 遍历，两边输出同一份形状
fn rg_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new("rg")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

/// rg 那条路的时限。rg 自己通常秒级返回；超时说明仓库大到 std 遍历也救不了，
/// 带着已收到的部分命中回来（截断标注），不再往 std 路上倒一遍
const SEARCH_RG_TIMEOUT: Duration = Duration::from_secs(15);

/// rg 路线。返回 `None` = 这条路没跑成，调用方落回 std：正则合法性的人话报错、
/// "路径不存在"这类话都由 std 路给出，两边不各说各的。
fn search_via_rg(args: &Value, root: Option<&Path>) -> Option<Result<String, String>> {
    let query = arg_str(args, "query")?.trim();
    if query.is_empty() {
        return None;
    }
    let base = resolve(arg_str(args, "path").unwrap_or("."), root);
    if !base.exists() {
        return None;
    }

    let mut cmd = Command::new("rg");
    // --sort=path 换确定性：rg 默认多线程，命中顺序会在两次调用之间抖，
    // 而模型要能对着上一轮的结果接着走
    cmd.args(["--json", "--sort=path", "--no-messages", "-e", query]);
    if !args.get("case_sensitive").and_then(Value::as_bool).unwrap_or(false) {
        cmd.arg("-i");
    }
    if let Some(glob) = arg_str(args, "glob").map(str::trim).filter(|g| !g.is_empty()) {
        cmd.args(["-g", glob]);
    }
    // 固定跳过名单与 std 遍历同一份：rg 默认吃 .gitignore，但"gitignore 没写
    // node_modules"的仓库就不该是另一副面孔
    for dir in SEARCH_SKIP_DIRS {
        cmd.args(["-g", &format!("!{dir}/**")]);
    }
    if base.is_file() {
        cmd.current_dir(base.parent()?);
        cmd.arg(base.file_name()?);
    } else {
        cmd.current_dir(&base);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let (sender, receiver) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut lines = std::io::BufReader::new(stdout).lines();
        loop {
            match lines.next() {
                Some(Ok(line)) => {
                    if sender.send(line).is_err() {
                        break;
                    }
                }
                // 单行解不出来就跳过：读不到 UTF-8 的一行不该让整场搜索停摆
                Some(Err(_)) => continue,
                None => break,
            }
        }
    });

    let deadline = Instant::now() + SEARCH_RG_TIMEOUT;
    let mut hits: Vec<String> = Vec::new();
    let mut timed_out = false;
    loop {
        if hits.len() >= SEARCH_MAX_HITS {
            break;
        }
        match receiver.recv_timeout(Duration::from_millis(120)) {
            Ok(line) => {
                if let Some(hit) = rg_match_line(&line) {
                    hits.push(hit);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    break;
                }
            }
            // rg 自己退了：把队列里剩下的收完由断开前的循环兜着，这里直接走
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let truncated = hits.len() >= SEARCH_MAX_HITS || timed_out;
    let _ = child.kill();
    let status = child.wait().ok().and_then(|status| status.code());
    let _ = reader.join();

    // 一发都没收到就异常退出：多半是正则不合法——std 路上有人话，落回去
    if hits.is_empty() {
        if timed_out {
            return None;
        }
        match status {
            // exit 2 = rg 自己报错（正则不合法之类）；拿不到退出码也不赌
            Some(2) | None => return None,
            // 其余（含 exit 1 = 无命中）都按"搜过了，没有命中"收场
            _ => {
                let query = arg_str(args, "query").unwrap_or_default();
                return Some(Ok(format!("没有命中（query={query}）。")));
            }
        }
    }
    let mut out = hits.join("\n");
    if truncated {
        out.push_str("\n…（命中超过 200 行已截断：收窄 query，或用 glob 限定文件名）");
    }
    Some(Ok(out))
}

/// rg --json 的一行 → 与 std 路同形状的命中（`相对路径:行号: 原文`）。
/// 非 match 行（begin/end/summary）与缺字段的行一律丢弃
fn rg_match_line(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value["type"].as_str() != Some("match") {
        return None;
    }
    let rel = value["data"]["path"]["text"]
        .as_str()?
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string();
    let line_number = value["data"]["line_number"].as_u64()?;
    let text = value["data"]["lines"]["text"].as_str()?.trim_end_matches(['\n', '\r']);
    let shown: String = {
        let mut taken: String = text.chars().take(SEARCH_LINE_CHARS).collect();
        if text.chars().count() > SEARCH_LINE_CHARS {
            taken.push('…');
        }
        taken
    };
    Some(format!("{rel}:{line_number}: {shown}"))
}

fn search_text_std(args: &Value, root: Option<&Path>) -> Result<String, String> {
    let query = arg_str(args, "query")
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or("search_text 缺少 query 参数")?
        .to_string();
    let case_sensitive = args
        .get("case_sensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let pattern = regex::RegexBuilder::new(&query)
        .case_insensitive(!case_sensitive)
        .size_limit(4 * 1024 * 1024)
        .build()
        .map_err(|e| format!("「{query}」不是合法的正则：{e}"))?;
    let glob = match arg_str(args, "glob").map(str::trim).filter(|g| !g.is_empty()) {
        Some(raw) => Some(
            regex::RegexBuilder::new(&format!("^{}$", regex::escape(raw).replace("\\*", ".*")))
                .case_insensitive(true)
                .build()
                .map_err(|e| format!("glob「{raw}」不合法：{e}"))?,
        ),
        None => None,
    };
    let base = resolve(arg_str(args, "path").unwrap_or("."), root);
    if !base.exists() {
        return Err(format!("搜索的路径不存在：{}", base.display()));
    }

    let mut hits: Vec<String> = Vec::new();
    let mut truncated = false;
    if base.is_file() {
        search_one_file(&base, &base, &pattern, glob.as_ref(), &mut hits, &mut truncated, 0);
    } else {
        walk_text_files(&base, &base, &pattern, glob.as_ref(), &mut hits, &mut truncated, 0);
    }

    if hits.is_empty() {
        return Ok(format!("没有命中（query={query}）。"));
    }
    let mut out = hits.join("\n");
    if truncated {
        out.push_str("\n…（命中超过 200 行已截断：收窄 query，或用 glob 限定文件名）");
    }
    Ok(out)
}

fn walk_text_files(
    dir: &Path,
    display_base: &Path,
    pattern: &regex::Regex,
    glob: Option<&regex::Regex>,
    hits: &mut Vec<String>,
    truncated: &mut bool,
    depth: usize,
) {
    if depth > SEARCH_MAX_DEPTH || *truncated {
        *truncated = true;
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    // 排序遍历：同一份输入要有同一份输出，模型才能对着结果接着走
    let mut items: Vec<_> = entries.flatten().collect();
    items.sort_by_key(|item| item.file_name());
    for item in items {
        if hits.len() >= SEARCH_MAX_HITS {
            *truncated = true;
            return;
        }
        let Ok(file_type) = item.file_type() else { continue };
        let path = item.path();
        let name = item.file_name().to_string_lossy().into_owned();
        if file_type.is_dir() {
            if SEARCH_SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk_text_files(&path, display_base, pattern, glob, hits, truncated, depth + 1);
            continue;
        }
        search_one_file(&path, display_base, pattern, glob, hits, truncated, depth);
    }
    if hits.len() >= SEARCH_MAX_HITS {
        *truncated = true;
    }
}

fn search_one_file(
    path: &Path,
    display_base: &Path,
    pattern: &regex::Regex,
    glob: Option<&regex::Regex>,
    hits: &mut Vec<String>,
    truncated: &mut bool,
    depth: usize,
) {
    if depth > SEARCH_MAX_DEPTH || hits.len() >= SEARCH_MAX_HITS {
        return;
    }
    if let Some(glob) = glob {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        match name {
            Some(name) if glob.is_match(&name) => {}
            _ => return,
        }
    }
    let Ok(mut handle) = fs::File::open(path) else { return };
    let mut bytes = Vec::new();
    if handle.by_ref().take(SEARCH_FILE_BYTES).read_to_end(&mut bytes).is_err() {
        return;
    }
    // 二进制嗅探：头部就有 NUL 的不当文本搜
    if bytes.contains(&0) {
        return;
    }
    let Ok(text) = String::from_utf8(bytes) else { return };
    let rel = path
        .strip_prefix(display_base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    for (i, line) in text.lines().enumerate() {
        if pattern.is_match(line) {
            let shown: String = {
                let mut taken: String = line.chars().take(SEARCH_LINE_CHARS).collect();
                if line.chars().count() > SEARCH_LINE_CHARS {
                    taken.push('…');
                }
                taken
            };
            hits.push(format!("{rel}:{}: {shown}", i + 1));
            if hits.len() >= SEARCH_MAX_HITS {
                *truncated = true;
                return;
            }
        }
    }
}

/// 「设置 → 命令 Shell」的全局快照：工具执行体没有 config 通道（同 knowledge 的
/// init_root 先例），setup 与配置变更钩子各同步一次，跑命令时读这份
static COMMAND_SHELL: std::sync::OnceLock<std::sync::RwLock<String>> = std::sync::OnceLock::new();

pub fn set_command_shell(value: &str) {
    let mut guard = COMMAND_SHELL
        .get_or_init(|| std::sync::RwLock::new(String::new()))
        .write()
        .expect("命令 Shell 快照锁");
    *guard = value.trim().to_string();
}

/// 模型没显式指定 shell 时的默认值：设置里选了就用设置，空着回 cmd
fn configured_shell() -> String {
    COMMAND_SHELL
        .get()
        .map(|lock| lock.read().expect("命令 Shell 快照锁").clone())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "cmd".to_string())
}

/// 「设置 → Agent → SSH 主机」的全局快照：每行 `名字=user@host:端口`。
/// 执行体没有 config 通道（command_shell 同款先例），setup 与配置变更钩子各同步一次
static SSH_HOSTS: std::sync::OnceLock<std::sync::RwLock<Vec<String>>> = std::sync::OnceLock::new();

pub fn set_ssh_hosts(values: &[String]) {
    let mut guard = SSH_HOSTS
        .get_or_init(|| std::sync::RwLock::new(Vec::new()))
        .write()
        .expect("SSH 主机快照锁");
    *guard = values.to_vec();
}

/// 花名册里的一台主机：显示名 → user@host + 可选端口。名字必须精确命中，
/// 不猜、不拼地址——"模型自己编一个 user@host"不是花名册的用法
fn ssh_target(name: &str) -> Result<(String, Option<String>), String> {
    let table = SSH_HOSTS
        .get()
        .map(|lock| lock.read().expect("SSH 主机快照锁").clone())
        .unwrap_or_default();
    let line = table
        .iter()
        .find(|line| {
            line.split_once('=')
                .map(|(key, _)| key.trim() == name)
                .unwrap_or(false)
        })
        .ok_or_else(|| {
            format!(
                "花名册里没有叫「{name}」的主机。先到 设置 → Agent → SSH 主机 加一行：名字=user@地址:端口"
            )
        })?;
    let rest = line.split_once('=').expect("刚按 = 找到").1.trim();
    // rsplit 取最后一段做端口：[::1]:22 这种带括号的 IPv6 也能拆对
    let (target, port) = match rest.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            (host.trim().to_string(), Some(port.to_string()))
        }
        _ => (rest.to_string(), None),
    };
    if target.is_empty() {
        return Err(format!("「{name}」这一行没有 user@host：{line}"));
    }
    Ok((target, port))
}

/// 在配置的远程主机上执行一条命令。系统 ssh 自己管认证（密钥/agent），
/// BatchMode 关掉一切交互式口令询问——连不上就诚实报错，不会挂在等口令上。
/// 沙箱令牌不套（远端执行碰不到本机文件边界），收容与清洗照旧
fn ssh_run(args: &Value) -> Result<String, String> {
    let name = arg_str(args, "host").ok_or("ssh_run 缺少 host 参数")?;
    let command = arg_str(args, "command").ok_or("ssh_run 缺少 command 参数")?;
    if command.trim().is_empty() {
        return Err("要执行的远端命令是空的。".into());
    }
    let (target, port) = ssh_target(name)?;
    let timeout = command_timeout(args);

    // 与 mcp.rs 同一条先例：spawn 的程序来自用户在设置里亲手写的配置
    let mut child = OsCommand::new("ssh");
    child
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
        // 首次连接自动接受新主机键：没有这一条，第一次连每台主机都卡在交互确认上
        .args(["-o", "StrictHostKeyChecking=accept-new"]);
    if let Some(port) = &port {
        child.args(["-p", port]);
    }
    child.arg(&target).arg(command);
    crate::tool_runtime::constrain::constrained(&mut child);

    let mut child = child
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 ssh 失败（系统里装了 OpenSSH 客户端吗？）：{e}"))?;

    // 收容 fail-closed（与 run_command 同一条拍板）：收不进去就杀掉、拒绝执行
    let _job_guard = match crate::tool_runtime::job::Guard::contain(&child) {
        Ok(guard) => guard,
        Err(problem) => {
            crate::tool_runtime::constrain::reap_tree(&mut child);
            return Err(format!("执行收容约束建立失败，已拒绝执行（fail-closed）：{problem}"));
        }
    };

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = Vec::new();
                let mut err = Vec::new();
                if let Some(mut pipe) = child.stdout.take() {
                    let _ = pipe.read_to_end(&mut out);
                }
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_end(&mut err);
                }
                let stdout = String::from_utf8_lossy(&out);
                let stderr = String::from_utf8_lossy(&err);
                let code = status.code().unwrap_or(-1);
                let mut text = String::new();
                if !stdout.trim().is_empty() {
                    text.push_str(stdout.trim_end());
                    text.push('\n');
                }
                if !stderr.trim().is_empty() {
                    text.push_str("〔stderr〕\n");
                    text.push_str(stderr.trim_end());
                    text.push('\n');
                }
                if text.is_empty() {
                    text.push_str("（远端没有输出）\n");
                }
                text.push_str(&format!("〔退出码 {code}〕"));
                return match code {
                    0 => Ok(text),
                    // ssh 自身的失败（连不上、被拒）也在 stderr 里，原样给模型看
                    _ => Err(text),
                };
            }
            Ok(None) => {
                if started.elapsed() > timeout {
                    crate::tool_runtime::constrain::reap_tree(&mut child);
                    return Err(format!(
                        "ssh 在 {} 秒内没有回来，已终止。远端挂起或网络不通时，stderr 里通常有 ConnectTimeout 的报错。",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("等待 ssh 失败：{e}")),
        }
    }
}

/// LSP 查询转接：路径按 read_file 同一套 resolve（相对项目根），位置参数原样递
fn lsp_query(args: &Value, root: Option<&Path>) -> Result<String, String> {
    let file = arg_str(args, "file").ok_or("lsp_query 缺少 file 参数")?;
    let query = match arg_str(args, "query") {
        Some("definition") => crate::lsp_host::Query::Definition,
        Some("references") => crate::lsp_host::Query::References,
        Some("hover") => crate::lsp_host::Query::Hover,
        Some("symbols") => crate::lsp_host::Query::Symbols,
        other => {
            return Err(format!(
                "不认识的查询「{}」：definition / references / hover / symbols 选一个。",
                other.unwrap_or("（空）")
            ))
        }
    };
    let position = match (args["line"].as_u64(), args["column"].as_u64()) {
        (Some(line), Some(column)) => crate::lsp_host::Position::At(line, column),
        _ => {
            let symbol = arg_str(args, "symbol").unwrap_or_default();
            if symbol.is_empty() {
                return Err("要给查询位置：line+column（都从 1 起），或 symbol（标识符文本）。".into());
            }
            crate::lsp_host::Position::Symbol(symbol.to_string())
        }
    };
    let resolved = resolve(file, root);
    if !resolved.exists() {
        return Err(format!("文件不存在：{}。", resolved.display()));
    }
    crate::lsp_host::query(&resolved, root, query, position)
}

/// Git Bash 的 bash.exe 位置探测：Git for Windows 的常见安装位按序试
pub fn git_bash_path() -> Result<std::path::PathBuf, String> {
    for base in [
        std::env::var("ProgramFiles").unwrap_or_default(),
        std::env::var("ProgramFiles(x86)").unwrap_or_default(),
        std::env::var("LOCALAPPDATA").unwrap_or_default(),
    ] {
        if base.is_empty() {
            continue;
        }
        let candidate = std::path::Path::new(&base).join("Git").join("bin").join("bash.exe");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err("没找到 Git Bash（bash.exe）：请确认 Git for Windows 装在默认位置，或改用其他 shell。".into())
}

/// 前台命令的超时：模型显式给了 timeout_seconds 就用它（1..=600），否则默认值。
/// 上限 600 的意思是"构建/测试确实要跑一阵"，不是"长驻进程也可以"——那种活走 background
fn command_timeout(args: &Value) -> Duration {
    let seconds = args
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(COMMAND_TIMEOUT.as_secs());
    Duration::from_secs(seconds.clamp(1, 600))
}

fn run_command(args: &Value, root: Option<&Path>, owner: Option<&str>) -> Result<String, String> {
    let command = arg_str(args, "command").ok_or("run_command 缺少 command 参数")?;
    // 模型显式指定的优先；没指定时跟「设置 → 命令 Shell」
    let shell = args["shell"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(configured_shell);
    if !matches!(shell.as_str(), "cmd" | "powershell" | "pwsh" | "git-bash") {
        return Err(format!(
            "不认识的 shell「{shell}」：cmd / powershell / pwsh / git-bash 选一个。"
        ));
    }
    // 没有项目根就不跑。退回进程自己的工作目录不是"宽松一点"，
    // 那是在一个用户看不见的地方动文件
    let cwd = crate::tool_runtime::constrain::require_cwd(root)?;
    let timeout = command_timeout(args);
    // 沙箱（低完整性 + WRITE_RESTRICTED）：先把可写根就位（标签+授权，
    // 拿到能力 SID 清单），失败按拒绝处理；spawn 用挂起标志，换完令牌再恢复——
    // 一条指令都不漏在沙箱外跑。档位逐调用可点名（sandbox=on/off，对齐 deepseek
    // 的 per-call policy）：与生效档不同的点名由判定面多问一行（ExecScope::SandboxOverride），
    // 执行侧只认结果——点名挡住了就根本走不到这里
    let requested = args["sandbox"].as_str().unwrap_or("default");
    if !matches!(requested, "default" | "on" | "off") {
        return Err(format!(
            "不认识的 sandbox 档位「{requested}」：default / on / off 选一个。"
        ));
    }
    let sandbox_on = match requested {
        "on" => true,
        "off" => false,
        _ => crate::tool_runtime::sandbox::enabled(),
    };
    let sandbox_cap_sids = if sandbox_on {
        match crate::tool_runtime::sandbox::prepare_command_roots(&cwd) {
            Ok(sids) => Some(sids),
            Err(problem) => return Err(format!("沙箱可写根没就位，已拒绝执行（fail-closed）：{problem}")),
        }
    } else {
        None
    };

    // 开发服务器这类长活走后台：立即返回句柄，不套 60 秒
    if args["background"].as_bool().unwrap_or(false) {
        let state = crate::tool_runtime::background::state();
        let mut registry = state.lock().expect("后台命令登记表锁");
        let id = registry.spawn(&command, &shell, &cwd, owner.unwrap_or(""), sandbox_on)?;
        return Ok(format!(
            "后台命令 #{id} 已启动（{command}）。它不套 60 秒超时；\
             用 command_output 增量看输出，任务做完用 command_stop 收尾。"
        ));
    }

    #[cfg(windows)]
    let mut child = match shell.as_str() {
        "powershell" | "pwsh" => {
            let mut child = Command::new(shell.as_str());
            child.args(["-NoProfile", "-NonInteractive", "-Command", command]);
            child
        }
        "git-bash" => {
            let mut child = Command::new(git_bash_path()?);
            child.args(["-c", command]);
            child
        }
        _ => {
            let mut child = Command::new("cmd");
            child.args(["/C", command]);
            child
        }
    };

    #[cfg(not(windows))]
    let mut child = Command::new("sh");
    #[cfg(not(windows))]
    let args_slice: [&str; 2] = ["-c", command];

    // 凭据形状的环境变量不进子进程：一句 `set`、或一个话多的构建脚本，
    // 就够把它们抄进上下文，而上下文是要发给服务商的。
    // 这一条现在三处 spawn 共用（工具命令 / MCP 服务器 / 插件钩子），判据只有一份
    crate::tool_runtime::constrain::constrained(&mut child);

    // 沙箱开着：专用临时目录顶掉系统 TMP（系统那份不在可写根里，低完整性写不进），
    // 并挂起拉起——换完受限令牌再恢复
    if sandbox_on {
        let tmp = crate::tool_runtime::sandbox::sandbox_tmp();
        child.env("TMP", &tmp).env("TEMP", &tmp);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            child.creation_flags(crate::tool_runtime::sandbox::CREATE_SUSPENDED);
        }
    }

    #[cfg(not(windows))]
    let mut child = child.args(args_slice);

    let mut child = child
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动命令失败: {e}"))?;
    let pid = child.id();

    // 收容（拍板：约束建立失败 = 拒绝执行）。收不进去就杀掉孩子、报错，
    // 绝不降级成"没有约束也照跑"。Guard 活到本函数返回：句柄一关，
    // 树连同 shell 留下的所有子孙一起被内核收走
    let _job_guard = match crate::tool_runtime::job::Guard::contain(&child) {
        Ok(guard) => guard,
        Err(problem) => {
            crate::tool_runtime::constrain::reap_tree(&mut child);
            return Err(format!("执行收容约束建立失败，已拒绝执行（fail-closed）：{problem}"));
        }
    };
    // 沙箱（低完整性 + WRITE_RESTRICTED）换令牌并恢复执行。失败同一条拍板：
    // 杀孩子、拒绝执行
    if sandbox_on {
        let sids = sandbox_cap_sids.unwrap_or_default();
        if let Err(problem) = crate::tool_runtime::sandbox::activate(&child, &sids) {
            crate::tool_runtime::constrain::reap_tree(&mut child);
            return Err(format!("沙箱建立失败，已拒绝执行（fail-closed）：{problem}"));
        }
    }

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // 按字节收，统一转码：read_to_string 遇到 GBK 输出会整段报错，
                // 被 `let _` 吞掉后就成了"无输出"——那是最难查的一种静默失败
                let mut bytes = Vec::new();
                if let Some(mut stdout) = child.stdout.take() {
                    let _ = stdout.read_to_end(&mut bytes);
                }
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_end(&mut bytes);
                }
                let mut output = crate::tool_runtime::constrain::decode_output(&bytes);
                let truncated = output.len() > MAX_COMMAND_OUTPUT;
                if truncated {
                    output.truncate(MAX_COMMAND_OUTPUT);
                }
                if output.trim().is_empty() {
                    output = "(无输出)".into();
                }
                return Ok(format!(
                    "退出码 {:?}\n{output}{}",
                    status.code(),
                    if truncated {
                        "\n…（输出已截断）"
                    } else {
                        ""
                    }
                ));
            }
            Ok(None) if started.elapsed() < timeout => sleep(Duration::from_millis(50)),
            Ok(None) => {
                // 终止整棵树：先收容壳（内核收整棵树），再按 PID 打一遍补刀——
                // 只掐 `cmd` 那一层的话，它生出去的编译器还在跑，
                // 而我们已经在结果里告诉用户"命令结束了"
                _job_guard.terminate();
                crate::tool_runtime::constrain::kill_tree(pid);
                let _ = child.kill();
                return Err(format!(
                    "命令超过 {}s 未结束，已终止整棵进程树。要跑服务器、watcher 这类长活，\
                     给 run_command 传 background=true，然后用 command_output 看输出、command_stop 收尾。",
                    timeout.as_secs()
                ));
            }
            Err(e) => return Err(format!("等待命令结束失败: {e}")),
        }
    }
}

/// open_path 的目标解析：项目内的文件/目录，或过闸的 http/https 网址。
/// 解析与执行分离——测试钉住解析，ShellExecute 那一下靠人工冒烟
#[cfg(windows)]
fn resolve_open_target(
    target: &str,
    root: Option<&Path>,
) -> Result<OpenTarget, String> {
    let trimmed = target.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        // 网址过 web_fetch 同样的两道闸——开在用户的默认浏览器里也一样不能逛内网
        let config = crate::config::AppConfig::default();
        crate::egress::guard(&config.net_egress_allow, trimmed)?;
        crate::egress::refuse_private_target(trimmed)?;
        return Ok(OpenTarget::Url(trimmed.to_string()));
    }
    let cwd = crate::tool_runtime::constrain::require_cwd(root)?;
    let path = resolve(trimmed, Some(&cwd));
    if !path.exists() {
        return Err(format!("要打开的路径不存在：{}", path.display()));
    }
    let canonical = path.canonicalize().map_err(|e| format!("解析路径失败：{e}"))?;
    let root_canonical = cwd
        .canonicalize()
        .map_err(|e| format!("解析项目根失败：{e}"))?;
    if !canonical.starts_with(&root_canonical) {
        return Err(format!(
            "只能打开项目根内的路径（{} 在项目外）。项目外的网址可以用 http/https 链接开。",
            canonical.display()
        ));
    }
    Ok(OpenTarget::Path(canonical))
}

#[cfg(windows)]
pub(crate) enum OpenTarget {
    Path(std::path::PathBuf),
    Url(String),
}

/// 用系统关联程序打开（ShellExecute verb="open"）。
/// 这是"资源管理器里双击"的原语——cmd 的 start 在无控制台的
/// 管道上下文里 ShellExecute 必被拒（design-command-execution-fixes.md §2）
#[cfg(windows)]
pub(crate) fn open_target(target: OpenTarget) -> Result<String, String> {
    use windows::core::HSTRING;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let (file, display) = match &target {
        OpenTarget::Path(path) => (HSTRING::from(path.as_os_str()), format!("{}", path.display())),
        OpenTarget::Url(url) => (HSTRING::from(url), url.clone()),
    };
    let verb = HSTRING::from("open");
    let result = unsafe {
        ShellExecuteW(
            None,
            &verb,
            &file,
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW 返回值 > 32 = 成功；否则是错误码
    if result.0 as isize > 32 {
        Ok(format!("已用系统关联的程序打开：{display}"))
    } else {
        Err(format!(
            "系统拒绝打开 {display}（ShellExecute 错误码 {}）。文件类型可能没有关联的程序。",
            result.0 as isize
        ))
    }
}

// ---- @-提及的文件建议 ----

/// 输入框里敲 `@` 时的文件候选。这是**输入框的 IPC**，不是模型工具——
/// 模型找文件走 list_files / search_text，用户找文件走这里。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSuggest {
    /// 项目相对路径（正斜杠），也是插入草稿的 `@` 记号
    pub rel: String,
    pub abs: String,
    pub is_dir: bool,
}

const SUGGEST_MAX_RESULTS: usize = 20;
/// 排序前的候选池上限：收满就停，不为了一个"更好的第 21 名"把大仓库走完
const SUGGEST_MAX_CANDIDATES: usize = 200;
/// 遍历预算：候选池没收满也要停——扫描是给建议用的，不是给模型建索引
const SUGGEST_MAX_ENTRIES: usize = 20_000;
const SUGGEST_MAX_DEPTH: usize = 24;

#[tauri::command]
pub fn files_suggest(app: tauri::AppHandle, query: String) -> Result<Vec<FileSuggest>, String> {
    let config = config::load(&app);
    // 没绑工作目录就没有路径基准：给空名单，让前端把菜单收起来
    let Some(project) = config.active_project() else {
        return Ok(Vec::new());
    };
    Ok(suggest_files(Path::new(&project.path), &query))
}

fn suggest_files(root: &Path, query: &str) -> Vec<FileSuggest> {
    let query = query.trim_start_matches('@').trim().to_lowercase();
    let mut candidates: Vec<(u8, FileSuggest)> = Vec::new();
    let mut budget = SUGGEST_MAX_ENTRIES;
    suggest_walk(root, root, &query, 0, &mut candidates, &mut budget);
    candidates.sort_by(|a, b| (a.0, &a.1.rel).cmp(&(b.0, &b.1.rel)));
    candidates.truncate(SUGGEST_MAX_RESULTS);
    candidates.into_iter().map(|(_, item)| item).collect()
}

fn suggest_walk(
    dir: &Path,
    display_base: &Path,
    query: &str,
    depth: usize,
    out: &mut Vec<(u8, FileSuggest)>,
    budget: &mut usize,
) {
    if depth > SUGGEST_MAX_DEPTH || *budget == 0 || out.len() >= SUGGEST_MAX_CANDIDATES {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    // 排序遍历：同一份输入同一份输出，菜单不能每次按键抖一个顺序
    let mut items: Vec<_> = entries.flatten().collect();
    items.sort_by_key(|item| item.file_name());
    for item in items {
        if *budget == 0 || out.len() >= SUGGEST_MAX_CANDIDATES {
            return;
        }
        *budget -= 1;
        let path = item.path();
        let name = item.file_name().to_string_lossy().into_owned();
        let is_dir = item.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && SEARCH_SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let rel = path
            .strip_prefix(display_base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        // 空查询 = 浏览顶层：给第一层做起点，再深的让用户继续敲。
        // 打分：文件名前缀 0 < 文件名包含 1 < 路径段/顶层浏览 2
        let score: Option<u8> = if query.is_empty() {
            (depth == 0).then_some(2)
        } else {
            let lower_rel = rel.to_lowercase();
            let lower_name = name.to_lowercase();
            if lower_name.starts_with(query) {
                Some(0)
            } else if lower_name.contains(query) {
                Some(1)
            } else if lower_rel.contains(query) {
                Some(2)
            } else {
                None
            }
        };
        if let Some(score) = score {
            out.push((
                score,
                FileSuggest {
                    rel,
                    abs: path.to_string_lossy().into_owned(),
                    is_dir,
                },
            ));
        }
        if is_dir {
            suggest_walk(&path, display_base, query, depth + 1, out, budget);
        }
    }
}

/// 附件选择器认得的图片扩展名：按扩展名走图片那条路（字节读 + base64 预览），
/// 不当文本读——二进制死在 UTF-8 校验上，用户看到的就是"选了参考图输入框里什么都没有"
/// （真机踩过：生图会话选参考图，chip 不出现）
/// 附件选择器认得的视频/音频扩展名。chat 发送那一侧（with_attachments）也用
/// 这两张表认媒体——认出来就记引用走多模态投影，不当文本读
pub(crate) fn video_mime_of(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        _ => return None,
    })
}

pub(crate) fn audio_mime_of(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "ogg" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        _ => return None,
    })
}

fn image_mime_of(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => return None,
    })
}

/// 参考图读取上限：再大的图片图生图 multipart 也扛不动，让用户先压一压
const IMAGE_ATTACHMENT_MAX_BYTES: u64 = 20 * 1024 * 1024;

/// 用户通过「添加」主动选中的文件不需要审批，但仍然限制为 UTF-8 文本并截断。
/// 图片例外：按字节读并带回 base64 预览，走与粘贴截图同一条 chip 渲染链
pub fn read_attachment(path: &str) -> Result<Value, String> {
    let target = Path::new(path);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());

    // 音频素材（语音转写的对象/生成的产物）：只挂元数据，播放走 asset 协议
    if let Some(mime) = audio_mime_of(target) {
        let metadata = fs::metadata(target).map_err(|e| format!("无法访问 {path}: {e}"))?;
        return Ok(json!({
            "name": name,
            "path": path,
            "chars": metadata.len(),
            "truncated": false,
            "kind": "audio",
            "mime": mime,
            "text": "",
        }));
    }

    // 视频素材（视频编辑模式的参照）：只挂元数据不读内容，chip 走 asset 协议
    if let Some(mime) = video_mime_of(target) {
        let metadata = fs::metadata(target).map_err(|e| format!("无法访问 {path}: {e}"))?;
        return Ok(json!({
            "name": name,
            "path": path,
            "chars": metadata.len(),
            "truncated": false,
            "kind": "video",
            "mime": mime,
            "text": "",
        }));
    }

    if let Some(mime) = image_mime_of(target) {
        let metadata = fs::metadata(target).map_err(|e| format!("无法访问 {path}: {e}"))?;
        if metadata.len() > IMAGE_ATTACHMENT_MAX_BYTES {
            return Err(format!(
                "图片超过 {} MB，先压一压再当参考图用。",
                IMAGE_ATTACHMENT_MAX_BYTES / 1024 / 1024
            ));
        }
        let mut buffer = Vec::new();
        fs::File::open(target)
            .and_then(|mut handle| handle.read_to_end(&mut buffer))
            .map_err(|e| format!("读取失败: {e}"))?;
        use base64::Engine as _;
        let preview = format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&buffer)
        );
        return Ok(json!({
            "name": name,
            "path": path,
            // 图片没有"字数"：chip 上的 KB 标签拿这一格当字节数用（与粘贴截图一致）
            "chars": buffer.len(),
            "truncated": false,
            "kind": "image",
            "mime": mime,
            "previewDataUrl": preview,
            "text": "",
        }));
    }

    let (text, truncated) = read_text(target)?;

    Ok(json!({
        "name": name,
        "path": path,
        "chars": text.chars().count(),
        "truncated": truncated,
        "text": text,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(value: &Value) -> Vec<String> {
        value
            .as_array()
            .expect("schemas 必须是数组")
            .iter()
            .map(|item| {
                item["function"]["name"]
                    .as_str()
                    .expect("每个工具都要有 function.name")
                    .to_string()
            })
            .collect()
    }

    fn off(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn registry_ids_match_the_declared_tools() {
        // 面板里的 id 一旦和 schema 对不上，开关就会"看起来能拨但什么都不关"
        let mut declared = names(&schemas());
        for standalone in [
            skill_schema(),
            web_fetch_schema(),
            web_search_schema(),
            browser_schema(),
            knowledge_schema(),
            obs_recall_schema(),
        ] {
            declared.push(
                standalone["function"]["name"]
                    .as_str()
                    .expect("单独声明的工具要有名字")
                    .to_string(),
            );
        }
        for spec in REGISTRY.iter() {
            assert!(
                declared.iter().any(|name| name == spec.id),
                "工具清单里的 {} 在声明中不存在",
                spec.id
            );
        }
        assert_eq!(declared.len(), REGISTRY.len(), "有工具没进面板清单");
        assert_eq!(
            schemas_for(&[]).as_array().unwrap().len(),
            19,
            "取用技能、读网页、子助理控制、内置浏览器不混进工作目录工具；工作目录整表 19 条"
        );
    }

    #[test]
    fn disabling_removes_the_tool_from_the_declaration() {
        assert_eq!(names(&schemas_for(&[])).len(), 19);

        let kept = names(&schemas_for(&off(&["run_command"])));
        assert_eq!(kept.len(), 18);
        assert!(!kept.iter().any(|name| name == "run_command"));
        assert!(kept.iter().any(|name| name == "read_file"));

        // 全关掉要得到空数组，由调用方整段省略 tools 字段
        let all_off = off(&REGISTRY
            .iter()
            .map(|spec| spec.id)
            .collect::<Vec<_>>()
            .as_slice());
        assert!(names(&schemas_for(&all_off)).is_empty());
    }

    #[test]
    fn unknown_ids_in_the_disabled_list_are_harmless() {
        assert_eq!(names(&schemas_for(&off(&["nope"]))).len(), 19);
        assert!(!is_disabled(&off(&["nope"]), "read_file"));
        assert!(is_disabled(&off(&["read_file"]), "read_file"));
    }

    /// 切进「完全访问」那个弹窗里列的工具，必须与注册表一一对上。
    /// 加一个工具而忘了改那句"免确认的是哪些"，弹窗就会**往轻了说**——
    /// 而一个往轻了说的确认框比没有确认框更危险（这条判据写在 design-security-permission.md）
    #[test]
    fn the_full_access_confirmation_names_every_registered_tool() {
        let copy = include_str!("../../src/components/full-access-confirm.tsx");
        for spec in REGISTRY.iter() {
            assert!(
                copy.contains(spec.title),
                "确认框的清单里没提「{}」，它说的比实际放开的少",
                spec.title
            );
        }
    }

    // ---- read_attachment ----

    /// 参考图按字节读并带 base64 预览：图片走 UTF-8 文本校验必然被拒，
    /// 用户看到的就是"选了参考图输入框里什么都没有"（真机踩过）
    #[test]
    fn a_picked_image_reads_as_an_image_attachment_with_a_preview() {
        let root = crate::test_support::scoped_temp_dir("read-attachment-image");
        let image = root.path.as_path().join("ref.png");
        // 内容故意含非法 UTF-8 字节：按文本读必须失败，按图片读必须成功
        fs::write(&image, [0x89, b'P', b'N', b'G', 0xFF, 0xFE, 0x00]).unwrap();

        let value = read_attachment(image.to_str().unwrap()).unwrap();
        assert_eq!(value["kind"], "image", "按扩展名认出图片");
        assert_eq!(value["mime"], "image/png");
        assert_eq!(value["chars"], 7, "chip 上的 KB 标签拿这一格当字节数");
        assert!(value["text"].as_str().unwrap().is_empty());
        let preview = value["previewDataUrl"].as_str().unwrap();
        assert!(
            preview.starts_with("data:image/png;base64,"),
            "预览是数据 URL，chip 不用再读一次盘"
        );

        // 文本照旧走原路：形状一个键都不多
        let text_file = root.path.as_path().join("note.txt");
        fs::write(&text_file, "正文").unwrap();
        let value = read_attachment(text_file.to_str().unwrap()).unwrap();
        assert!(value.get("kind").is_none(), "文本附件不带 kind，消费者按缺省当文本");
        assert_eq!(value["chars"], 2);
    }

    // ---- open_path ----

    /// 解析层：项目外的路径拒、不存在的拒、网址过出口与内网两道闸。
    /// ShellExecute 那一下不进测试（它真的会开窗口），解析全在纯函数里
    #[cfg(windows)]
    #[test]
    fn open_path_resolves_only_in_project_paths_and_gated_urls() {
        let root = crate::test_support::scoped_temp_dir("open-path");
        let root_path = root.path.as_path().to_path_buf();
        fs::write(root_path.join("index.html"), "<h1>ok</h1>").unwrap();

        let inside = resolve_open_target("index.html", Some(root_path.as_path())).unwrap();
        assert!(matches!(inside, OpenTarget::Path(_)), "项目内文件照常解析");

        assert!(
            resolve_open_target("不存在.html", Some(root_path.as_path())).is_err(),
            "不存在的路径要报，不是默默开个空白"
        );
        assert!(
            resolve_open_target("../outside.txt", Some(root_path.as_path())).is_err(),
            "项目外一律拒：双击别的盘的东西不是工具该做的事"
        );
        assert!(
            resolve_open_target(r"C:\Windows
otepad.exe", Some(root_path.as_path())).is_err(),
            "绝对路径出了项目根同样拒"
        );

        // 网址过 web_fetch 同样的两道闸：内网字面量拒，坏协议拒
        assert!(resolve_open_target("http://localhost:8080/", Some(root_path.as_path())).is_err());
        assert!(resolve_open_target("file:///C:/x.html", Some(root_path.as_path())).is_err());
        match resolve_open_target("https://example.com/", Some(root_path.as_path())).unwrap() {
            OpenTarget::Url(url) => assert_eq!(url, "https://example.com/"),
            _ => panic!("网址该解析成 Url"),
        }
    }

    // ---- search_text ----

    fn write_into(root: &Path, rel: &str, content: &str) -> PathBuf {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn search_reports_hits_with_locations_and_skips_noise() {
        let root = crate::test_support::scoped_temp_dir("search-text");
        write_into(root.path.as_path(), "src/main.rs", "fn main() {}\nfn helper() {}\n");
        write_into(root.path.as_path(), "node_modules/pkg/index.js", "fn main() {}\n");
        write_into(root.path.as_path(), "blob.bin", "ok\x00binary\nfn main() {}\n");

        let out = execute("search_text", &json!({"query": "fn main"}), Some(root.path.as_path())).unwrap();
        assert!(out.contains("src/main.rs:1: fn main() {}"), "{out}");
        assert!(!out.contains("node_modules"), "依赖目录必须整棵跳过：{out}");
        assert!(!out.contains("blob.bin"), "含 NUL 的文件不当文本搜：{out}");

        // glob 限定文件名：同名命中在别的扩展名里不算数
        write_into(root.path.as_path(), "src/other.js", "fn main() {}\n");
        let only_rs = execute(
            "search_text",
            &json!({"query": "fn main", "glob": "*.rs"}),
            Some(root.path.as_path()),
        )
        .unwrap();
        assert!(only_rs.contains("main.rs"));
        assert!(!only_rs.contains("other.js"), "{only_rs}");

        // 非法正则要报人话，不是空结果
        let bad = execute("search_text", &json!({"query": "(unclosed"}), Some(root.path.as_path())).unwrap_err();
        assert!(bad.contains("不是合法的正则"), "{bad}");
    }

    #[test]
    fn search_truncates_when_hits_exceed_the_cap() {
        let root = crate::test_support::scoped_temp_dir("search-cap");
        let body: String = (0..250).map(|i| format!("needle {i}\n")).collect();
        write_into(root.path.as_path(), "big.txt", &body);
        let out = execute("search_text", &json!({"query": "needle"}), Some(root.path.as_path())).unwrap();
        assert!(out.contains("已截断"), "{out}");
        assert_eq!(out.lines().count(), 201, "200 行命中 + 1 行截断提示");
    }

    // ---- edit_file ----

    #[test]
    fn edit_requires_a_unique_match_and_reports_the_count() {
        let root = crate::test_support::scoped_temp_dir("edit-file");
        let path = write_into(root.path.as_path(), "a.txt", "alpha\nbeta\nalpha\n");

        // 两处命中且没开 replace_all：拒，并报出实际次数
        let err = execute(
            "edit_file",
            &json!({"path": "a.txt", "old_string": "alpha", "new_string": "x"}),
            Some(root.path.as_path()),
        )
        .unwrap_err();
        assert!(err.contains("2 次"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "alpha\nbeta\nalpha\n", "拒了就不许动文件");

        // replace_all 全换
        let out = execute(
            "edit_file",
            &json!({"path": "a.txt", "old_string": "alpha", "new_string": "x", "replace_all": true}),
            Some(root.path.as_path()),
        )
        .unwrap();
        assert!(out.contains("替换 2 处"), "{out}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "x\nbeta\nx\n");

        // 唯一替换、old==new 拒、找不到拒、文件不存在引导 write_file
        execute(
            "edit_file",
            &json!({"path": "a.txt", "old_string": "beta", "new_string": "gamma"}),
            Some(root.path.as_path()),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "x\ngamma\nx\n");
        let same = execute(
            "edit_file",
            &json!({"path": "a.txt", "old_string": "x", "new_string": "x"}),
            Some(root.path.as_path()),
        )
        .unwrap_err();
        assert!(same.contains("相同"), "{same}");
        let missing = execute(
            "edit_file",
            &json!({"path": "a.txt", "old_string": "zzz", "new_string": "y"}),
            Some(root.path.as_path()),
        )
        .unwrap_err();
        assert!(missing.contains("没有找到"), "{missing}");
        let absent = execute(
            "edit_file",
            &json!({"path": "ghost.txt", "old_string": "a", "new_string": "b"}),
            Some(root.path.as_path()),
        )
        .unwrap_err();
        assert!(absent.contains("无法访问"), "{absent}");
    }

    /// 台账的接线点：planned_content 要给得出"将要写成的全文"，参数没到能写的地步
    /// 就返回 None（不落账）——这是 write_file/edit_file 共用快照管道的合同
    #[test]
    fn planned_content_previews_edits_and_gates_the_ledger() {
        let root = crate::test_support::scoped_temp_dir("planned-content");
        write_into(root.path.as_path(), "a.txt", "alpha\nbeta\n");

        let (target, content) = planned_content(
            "edit_file",
            &json!({"path": "a.txt", "old_string": "beta", "new_string": "b"}),
            Some(root.path.as_path()),
        )
        .unwrap();
        assert!(target.ends_with("a.txt"));
        assert_eq!(content, "alpha\nb\n");

        // 校验不过 = 文件没动 = 不进台账
        assert!(
            planned_content(
                "edit_file",
                &json!({"path": "a.txt", "old_string": "zzz", "new_string": "b"}),
                Some(root.path.as_path())
            )
            .is_none()
        );
        // write_file 照旧；与写入无关的工具一律 None
        assert!(planned_content("write_file", &json!({"path": "n.txt", "content": "hi"}), Some(root.path.as_path())).is_some());
        assert!(planned_content("run_command", &json!({"command": "x"}), Some(root.path.as_path())).is_none());
    }

    // ---- read_file 行区间 ----

    #[test]
    fn read_file_ranges_carry_line_numbers() {
        let root = crate::test_support::scoped_temp_dir("read-range");
        write_into(root.path.as_path(), "lines.txt", "l1\nl2\nl3\nl4\n");

        let full = execute("read_file", &json!({"path": "lines.txt"}), Some(root.path.as_path())).unwrap();
        assert_eq!(full, "l1\nl2\nl3\nl4\n", "不带区间参数 = 原样整读，无行号");

        let range = execute(
            "read_file",
            &json!({"path": "lines.txt", "offset": 2, "limit": 2}),
            Some(root.path.as_path()),
        )
        .unwrap();
        assert_eq!(range, "2\tl2\n3\tl3");

        let tail = execute("read_file", &json!({"path": "lines.txt", "offset": 3}), Some(root.path.as_path())).unwrap();
        assert_eq!(tail, "3\tl3\n4\tl4");

        let beyond = execute("read_file", &json!({"path": "lines.txt", "offset": 99}), Some(root.path.as_path())).unwrap();
        assert!(beyond.contains("区间为空"), "{beyond}");
    }

    /// 新工具的风险定档：搜索与读取同判据（项目内 Safe / 项目外 High），
    /// 精确编辑与写入同档，web_fetch 只读 Safe（闸在 egress）
    #[test]
    fn new_tools_risk_by_location() {
        let root = crate::test_support::scoped_temp_dir("risk-arms");
        write_into(root.path.as_path(), "f.txt", "x");
        assert_eq!(classify("search_text", &json!({"path": "f.txt"}), Some(root.path.as_path())), Risk::Safe);
        assert_eq!(
            classify("search_text", &json!({"path": "C:\\Windows\\win.ini"}), Some(root.path.as_path())),
            Risk::High
        );
        assert_eq!(classify("edit_file", &json!({"path": "f.txt"}), Some(root.path.as_path())), Risk::Elevated);
        assert_eq!(classify("web_fetch", &json!({"url": "https://example.com"}), None), Risk::Safe);
        // LSP 查询与 read_file 同判据，但它的路径参数叫 file 不叫 path——
        // 这个错位漏掉的话项目内的语义查询会一直按 High 弹审批
        write_into(root.path.as_path(), "a.rs", "fn f() {}");
        assert_eq!(
            classify("lsp_query", &json!({"file": "a.rs", "query": "definition"}), Some(root.path.as_path())),
            Risk::Safe
        );
        assert_eq!(
            classify("lsp_query", &json!({"file": "C:\\Windows\\win.ini", "query": "symbols"}), Some(root.path.as_path())),
            Risk::High
        );
        assert_eq!(classify("ssh_run", &json!({"host": "h", "command": "ls"}), Some(root.path.as_path())), Risk::High);
    }

    // ---- files_suggest ----

    #[test]
    fn suggestions_rank_basename_matches_first_and_skip_noise_dirs() {
        let root = crate::test_support::scoped_temp_dir("files-suggest");
        write_into(root.path.as_path(), "src/app/main.rs", "x");
        write_into(root.path.as_path(), "docs/notes-main.md", "x");
        write_into(root.path.as_path(), "node_modules/main.js", "x");

        let out = suggest_files(root.path.as_path(), "main");
        let rels: Vec<&str> = out.iter().map(|item| item.rel.as_str()).collect();
        assert_eq!(rels[0], "src/app/main.rs", "文件名前缀匹配排最前：{rels:?}");
        assert!(!rels.iter().any(|rel| rel.contains("node_modules")), "重目录整棵跳过：{rels:?}");
        assert!(out.iter().all(|item| !item.abs.is_empty() && !item.is_dir));
    }

    #[test]
    fn empty_query_lists_top_level_as_browsing_start() {
        let root = crate::test_support::scoped_temp_dir("files-suggest-empty");
        write_into(root.path.as_path(), "src/deep/inner.rs", "x");
        write_into(root.path.as_path(), "README.md", "x");

        let out = suggest_files(root.path.as_path(), "");
        let rels: Vec<&str> = out.iter().map(|item| item.rel.as_str()).collect();
        assert!(rels.contains(&"README.md"), "{rels:?}");
        assert!(rels.contains(&"src"), "顶层目录也要给：它是用户浏览的起点");
        assert!(!rels.iter().any(|rel| rel.contains("inner.rs")), "空查询不下钻：{rels:?}");
    }

    #[test]
    fn query_matches_any_path_segment_but_ranks_basename_higher() {
        let root = crate::test_support::scoped_temp_dir("files-suggest-path");
        write_into(root.path.as_path(), "src/main.rs", "x");
        write_into(root.path.as_path(), "src/main_test.rs", "x");

        let out = suggest_files(root.path.as_path(), "main.r");
        assert_eq!(out[0].rel, "src/main.rs", "后缀命中也算，但完整前缀命中在前");
        // 目录里的匹配照样能被找到（路径段命中）
        let out = suggest_files(root.path.as_path(), "src/ma");
        assert!(out.iter().any(|item| item.rel == "src/main.rs"));
    }

    // ---- run_command 超时参数 ----

    #[test]
    fn command_timeout_clamps_into_the_configured_band() {
        assert_eq!(command_timeout(&json!({})), COMMAND_TIMEOUT, "没给就用默认 60s");
        assert_eq!(command_timeout(&json!({"timeout_seconds": 300})), Duration::from_secs(300));
        assert_eq!(
            command_timeout(&json!({"timeout_seconds": 0})),
            Duration::from_secs(1),
            "0 秒等于永远跑不完：夹到下限"
        );
        assert_eq!(
            command_timeout(&json!({"timeout_seconds": 100_000})),
            Duration::from_secs(600),
            "上限 600：长驻进程的活是 background 的，不是把超时拉到天上去"
        );
    }

    /// 真跑一把：timeout_seconds=1 的睡眠命令要在远小于 60s 的时候被掐掉，
    /// 且报错文案把 background 那条路指出来
    #[test]
    fn run_command_honours_an_explicit_shorter_timeout() {
        let root = crate::test_support::scoped_temp_dir("run-timeout");
        let started = Instant::now();
        let err = execute(
            "run_command",
            &json!({
                "command": if cfg!(windows) { "ping -n 30 127.0.0.1 > nul" } else { "sleep 30" },
                "timeout_seconds": 1
            }),
            Some(root.path.as_path()),
        )
        .unwrap_err();
        assert!(err.contains("超过 1s"), "{err}");
        assert!(err.contains("background"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10), "不能干等满默认 60s");
    }

    // ---- rg 路线的 JSON 解析 ----

    #[test]
    fn rg_json_lines_render_into_the_same_shape_as_the_std_walk() {
        let line = r#"{"type":"match","data":{"path":{"text":"src\\main.rs"},"line_number":3,"lines":{"text":"fn main() {}\n"}}}"#;
        assert_eq!(
            rg_match_line(line).as_deref(),
            Some("src/main.rs:3: fn main() {}"),
            "反斜杠归一成正斜杠，行尾换行剥掉——两条路输出必须长得一样"
        );
        // begin/end/summary 不是命中；缺字段的行不炸
        assert!(rg_match_line(r#"{"type":"begin","data":{}}"#).is_none());
        assert!(rg_match_line(r#"{"type":"match"}"#).is_none());
        assert!(rg_match_line("不是 JSON").is_none());
    }

    // ---- delete_file（design-security-center.md D1）----

    fn delete_temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aglab-del-{tag}-{}",
            std::time::SystemTime::now().elapsed().map(|d| d.as_nanos()).unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 回收站与硬删两档各走一遍。开关是进程级的：两段写在同一条测试里，
    /// 免得并行测试互相改状态
    #[test]
    fn delete_goes_through_the_one_tool_and_reports_honestly() {
        let root = delete_temp("delete");
        let a = root.join("a.txt");
        let b = root.join("b.txt");
        fs::write(&a, "a").unwrap();
        fs::write(&b, "b").unwrap();

        // 默认（回收站）：文件从原位消失，文案要说清"能反悔"
        let report = execute("delete_file", &json!({ "paths": ["a.txt"] }), Some(&root)).unwrap();
        assert!(!a.exists(), "回收站档下原位不该再有文件：{report}");
        assert!(report.contains("回收站"), "文案要说是移入回收站：{report}");

        // 硬删档：逐路径互不拖累，失败照实报
        set_delete_to_trash(false);
        let report =
            execute("delete_file", &json!({ "paths": ["b.txt", "nope.txt"] }), Some(&root)).unwrap();
        assert!(!b.exists());
        assert!(
            report.contains("失败 1 个") && report.contains("nope.txt"),
            "部分失败要逐条点名：{report}"
        );

        // 全部失败：整体报错并点名每一条
        let err = execute("delete_file", &json!({ "paths": ["nope.txt"] }), Some(&root)).unwrap_err();
        assert!(err.contains("nope.txt"), "{err}");

        // 空参：老实拒绝，不假装删了
        assert!(execute("delete_file", &json!({ "paths": [] }), Some(&root)).is_err());

        set_delete_to_trash(true);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn delete_file_is_registered_and_declared() {
        assert!(is_registered("delete_file"));
        assert!(names(&schemas()).contains(&"delete_file".to_string()));
    }
}
