//! 工具调用的**判定入口**。一次调用要动什么，先在这里被翻译成 capability，
//! 再由 [`crate::policy::Policy`] 决定放行 / 询问 / 拒绝。`chat.rs` 里那句
//! `needs_approval(permission, risk)` 是这套判定的旧影子，同一件事有两份真相
//! 就必须判断该信哪份，所以它被删掉，而不是留着兜底。
//!
//! 这里刻意**不做执行**：执行仍留在各条源里（内置工具、技能、扩展）。
//! 闸门只回答一个问题：这一次能不能跑、要不要问、为什么被拦。
//! 拦下的原因要能原样复述给用户，并且进审计（`crate::audit`）。

use std::path::Path;

use serde_json::Value;

use crate::file_rules::RuleAction;
use crate::policy::Level;
#[cfg(test)]
use crate::policy::Mode;
use crate::policy::{
    self, Capability, Decision, ExecScope, FileMode, InputScope, NetScope, PathScope, Phase, Policy,
};
use crate::{secrets, tools};

pub mod background;
pub mod cache;
pub mod constrain;
pub mod job;
pub mod ptc;
pub mod sandbox;
pub mod schema;
pub mod source;

/// 入参校验。读的就是发给模型的那一份声明，所以"模型以为能传什么"与
/// "我们检查什么"不可能分家——这是 `design-tool-runtime.md` §1.1-2 那句
/// "没有校验的 schema 就是装饰"的落点
pub fn check_arguments(call: &Call) -> Result<(), String> {
    // 扩展的入参**不在这里校**，而且不是"等 P1 接上"那种欠账：这个校验器只覆盖自家声明用到的
    // 那几个关键字（§1.1-2），服务器回来的 `inputSchema` 是别人写的任意 JSON Schema。
    // 拿子集去校全量只有两种坏法——把服务器其实接受的调用判死，或静默忽略不认识的关键字
    // （后者就是"没有校验的 schema 是装饰"本身）。那一头的契约由服务器自己判，它的错误原样回给模型。
    if call.via_mcp {
        return Ok(());
    }
    match tools::parameter_schema(call.name) {
        Some(declared) => schema::validate(&declared, call.args),
        // 查不到声明的名字就是名单外那一种，闸门已经按"来路不明"处理了
        None => Ok(()),
    }
}

/// 工具结果的来源标注。用户在话题里看到一段原文时，要能分清"这是模型写的"
/// 还是"这是从磁盘 / 子进程读回来的"。
///
/// 这句话过去由名字和 `via_mcp` 猜（`load_skill` 靠一个特判分支撑着），现在它来自路由：
/// **谁执行的就是谁产出的**，两处不可能各说一套
pub fn annotate(source: source::Kind, name: &str, output: String) -> String {
    format!("〔{} {name} 的输出〕\n{output}", source.label())
}

/// 一次待判定的调用。`via_mcp` 表示"这个名字背后是别人的程序"
pub struct Call<'a> {
    pub name: &'a str,
    pub args: &'a Value,
    pub root: Option<&'a Path>,
    pub via_mcp: bool,
}

impl<'a> Call<'a> {
    pub fn new(name: &'a str, args: &'a Value, root: Option<&'a Path>, via_mcp: bool) -> Self {
        Self {
            name,
            args,
            root,
            via_mcp,
        }
    }
}

/// 命令落在哪个执行范围里。只做前缀分类，不解析参数——
/// 解析一条 shell 命令是另一个工程，而且它一旦不准，分类就成了假安全感。
/// 认不出来的那一条就是 `Arbitrary`：**这里不留下"是哪条命令"那一格**，
/// 因为判定与审批键读到的永远是 `exec.arbitrary` 那一段，留一个没人读的字段
/// 只会让人以为 `exec.notion` 那样的键配得上（它配不上，见 `policy::is_known_key`）
pub fn exec_scope(command: &str) -> ExecScope {
    let normalized = policy::normalize_command(command);
    let head = normalized
        .split(' ')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .replace(".exe", "");
    let trimmed = head.trim_matches(|c| c == '"' || c == '\'');
    match trimmed {
        "git" | "gh" => ExecScope::Git,
        "cargo" | "npm" | "pnpm" | "yarn" | "bun" | "make" | "dotnet" | "python" | "py" | "pip"
        | "mvn" | "gradle" | "tsc" | "vite" => ExecScope::Build,
        _ => ExecScope::Arbitrary,
    }
}

/// 这次点名是不是在动沙箱边界。`default`/缺省 = 跟全局，不算；
/// 全局没开要 "on"（要给项目文件打完整性标签）与全局开着要 "off"（要脱壳）都算
fn sandbox_boundary_move(requested: &str) -> bool {
    sandbox_boundary_move_with(requested, sandbox::enabled())
}

/// 纯函数那一半：全局档由调用方给。测试不翻进程级原子，两边各测各的
fn sandbox_boundary_move_with(requested: &str, global_on: bool) -> bool {
    match if requested.is_empty() {
        "default"
    } else {
        requested
    } {
        "on" => !global_on,
        "off" => global_on,
        _ => false,
    }
}

fn arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `paths` 数组参数的字符串列表（`delete_file` 用）。缺参/空数组都算没给
fn arg_paths(args: &Value) -> Vec<&str> {
    args.get("paths")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// 路径落在哪一圈。判定必须与 `tools::classify` 用同一套 resolve + inside_root，
/// 否则"面板以为在项目内、闸门以为在项目外"就又是两份真相
pub fn path_scope(raw: &str, root: Option<&Path>) -> PathScope {
    if raw.is_empty() {
        // 没有 path 参数：不知道要动哪，只能按最越界的那一圈处理
        return PathScope::Any;
    }
    let resolved = tools::resolve(raw, root);
    if tools::inside_root(&resolved, root) {
        PathScope::ProjectRoot
    } else if inside_workspace(&resolved) {
        PathScope::Workspace
    } else {
        PathScope::Any
    }
}

/// 客户端自己的工作目录（临时目录）——往里写缓存不该和往用户文档里写同等对待
fn inside_workspace(path: &Path) -> bool {
    let temp = std::env::temp_dir();
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    real(path).starts_with(real(&temp))
}

/// 把一次调用翻译成它需要的 capability **集合**。判定取最严的那一条
pub fn capabilities_for(call: &Call) -> Vec<Capability> {
    if call.via_mcp {
        // 扩展跑的是别人的程序，客户端看不到它会做什么：既是一次具体工具调用，
        // 也是一个无法约束的执行体，还会把参数交给那个进程。三条都要过表
        return vec![
            Capability::Tool {
                name: call.name.to_string(),
            },
            Capability::Exec {
                scope: ExecScope::Arbitrary,
            },
            Capability::Net {
                scope: NetScope::Configured,
            },
        ];
    }
    match call.name {
        "read_file" | "list_files" => vec![Capability::File {
            scope: path_scope(arg(call.args, "path"), call.root),
            mode: FileMode::Read,
        }],
        "write_file" => vec![Capability::File {
            scope: path_scope(arg(call.args, "path"), call.root),
            mode: FileMode::Write,
        }],
        // 删除一等操作（design-security-center.md D1）。paths 是数组：一个调用可能
        // 同时跨着几圈范围，逐圈各给一行，最严的那圈说了算（根外删除在表上是硬错）
        "delete_file" => {
            let scopes: Vec<PathScope> = arg_paths(call.args)
                .iter()
                .map(|raw| path_scope(raw, call.root))
                .collect();
            if scopes.is_empty() {
                vec![Capability::File {
                    scope: PathScope::Any,
                    mode: FileMode::Delete,
                }]
            } else {
                scopes
                    .into_iter()
                    .map(|scope| Capability::File {
                        scope,
                        mode: FileMode::Delete,
                    })
                    .collect()
            }
        }
        "run_command" => {
            let mut caps = vec![Capability::Exec {
                scope: exec_scope(arg(call.args, "command")),
            }];
            // 逐调用沙箱策略（对齐 deepseek 的 per-call sandbox policy）：点名档位与
            // 当前生效档不同 = 动边界，加一行独立的闸。同档的点名不加重——
            // 全局开着再要 "on"、全局没开要 "off"，都只是把现状说清楚
            if sandbox_boundary_move(arg(call.args, "sandbox")) {
                caps.push(Capability::Exec {
                    scope: ExecScope::SandboxOverride,
                });
            }
            caps
        }
        "load_skill" => vec![Capability::Tool {
            name: "load_skill".into(),
        }],
        // 目标上报不是一项能力，是这一支自己的收尾信号：它不动文件也不动本机，
        // 所以与取技能同档。不接这一条的话它会掉进兜底去问 `exec.arbitrary`，
        // 等于为一句"我做完了"弹一次确认框
        "goal_report" => vec![Capability::Tool {
            name: "goal_report".into(),
        }],
        // 计划更新与向用户提问是同款控制信号：不动文件也不动本机，与目标上报同档
        "update_plan" | "ask_user" => vec![Capability::Tool {
            name: call.name.to_string(),
        }],
        // 派单与取技能同档：名字是可读键（审计与权限表都认得是哪一位被派了出去），
        // 子助理自己的每一发再按它的白名单与权限表各过各的闸
        "spawn_subagent" => vec![Capability::Tool {
            name: "spawn_subagent".into(),
        }],
        // Computer Use 的两个维度。不接这一条的话它会掉进下面的兜底去问 `exec.arbitrary`，
        // 表上那一行「操作别的程序」就成了一条没人读的装饰
        "list_windows" | "inspect_window" => vec![Capability::Input {
            scope: InputScope::Observe,
        }],
        "computer_act" => vec![Capability::Input {
            scope: InputScope::Act,
        }],
        // 模型编出来的名字：它要动什么无从得知，只能按"要执行一个来路不明的东西"问人
        _ => vec![Capability::Exec {
            scope: ExecScope::Arbitrary,
        }],
    }
}

/// 指纹的实体：命令（规范化后）、路径、或参数正文的哈希。
/// 正文**只进哈希**，不进可读键——审计和权限表都不该存着口令
fn material(call: &Call) -> String {
    match call.name {
        "run_command" if !call.via_mcp => policy::normalize_command(arg(call.args, "command")),
        "write_file" if !call.via_mcp => {
            let path = arg(call.args, "path").replace('\\', "/");
            format!(
                "{path}:{}",
                policy::fingerprint(&[arg(call.args, "content")])
            )
        }
        "read_file" | "list_files" if !call.via_mcp => arg(call.args, "path").replace('\\', "/"),
        // 删除的实体是"删了哪几个"：路径不是口令，可读地进键（design-security-center.md D1）
        "delete_file" if !call.via_mcp => arg_paths(call.args).join(", ").replace('\\', "/"),
        "load_skill" => arg(call.args, "name").to_string(),
        // 派单的实体是"派了谁"：名字不是口令，可读地进键比哈希有用得多。
        // 任务正文（args.task）不进键——它可能与敏感内容同源
        "spawn_subagent" => arg(call.args, "name").to_string(),
        // 扩展与来路不明的名字：参数整体进哈希，一次都不落明文
        _ => policy::fingerprint(&[&call.args.to_string()]),
    }
}

/// 技能声明的工具白名单。空名单 = 该技能不作限制
pub fn allowlist_violation(name: &str, allowed: Option<&[String]>) -> Option<String> {
    // `None` = 没作限制。空表是另一种意思：**一个都不许**（"全都许"的写法是 `*`，不是空表）。
    // 这一句改在这里，所以闸门与声明侧读到的是同一条判据，不会一处收紧一处放松
    let allowed = allowed?;
    let wanted = name.to_ascii_lowercase();
    let hit = allowed.iter().any(|entry| {
        // 技能清单里常见 `Read` / `mcp__server__tool`，也常见 `Bash(git *)` 这种带约束的写法。
        // 带括号的先剥掉括号：括号里的约束要 P1 的参数校验才谈得上强制
        let entry = entry
            .split('(')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        !entry.is_empty() && (entry == wanted || entry == "*")
    });
    if hit {
        return None;
    }
    Some(format!(
        "当前技能只允许用这些工具：{}。`{name}` 不在名单里，所以没有执行。",
        allowed.join(" · ")
    ))
}

/// 一次判定的完整结果
#[derive(Debug)]
pub struct Ruling {
    pub decision: Decision,
    /// 这一条动作涉及的 capability 键，审计与"记住这条"都用它
    pub key: String,
    pub fingerprint: String,
}

impl Ruling {
    /// 只有测试在用（生产侧对 Decision 的分支直接 match，不走谓词）
    #[cfg(test)]
    pub fn is_allow(&self) -> bool {
        matches!(self.decision, Decision::Allow)
    }

    /// 拦下或问起时那句原因。测试读它核对"说的是不是这道闸"，
    /// 生产侧照旧直接 match，不从这儿取文案
    #[cfg(test)]
    pub fn reason(&self) -> String {
        match &self.decision {
            Decision::Deny { reason } | Decision::Ask { reason, .. } => reason.clone(),
            Decision::Allow => String::new(),
        }
    }

    /// 记住一条动作的键：`capability 键集合 + 这一次到底动什么` 的哈希。
    /// 同一个 `git push origin main` 换成分支就是另一条，不会顺手被放行
    pub fn remember_key(&self) -> String {
        policy::fingerprint(&[&self.key, &self.fingerprint])
    }
}

/// 规划阶段拦下时要说的那一句。它只有一个出处：模型读到的工具结果、界面上那张卡片、
/// 审计里那一行，三处得是同一句话，否则同一道闸长出三种说法
///
/// 判据的顺序是有意的：破坏性命令黑名单先于这一条——`rm -rf /` 那种事在任何阶段都不该跑，
/// 那句理由比"现在是规划模式"更具体，也更该先被模型读到
const PLAN_LOCK: &str = "规划模式只读，这一条会动东西，所以没执行。把它写进方案里，\
等用户批准或把模式切回「对话」；换权限档位解不开这一条。";

/// 文件安全规则命中时的两句文案（design-security-center.md D2）。规则可以在设置里改，
/// 文案得告诉人去哪改；模型读到的工具结果、界面上那张卡片、审计那一行，
/// 三处同源（与 PLAN_LOCK 同一手法）
const FILE_RULE_DENY: &str =
    "文件安全规则把这一批路径划成了「拒绝」，没有执行。规则在设置 → 文件安全里。";
const FILE_RULE_ASK: &str = "文件安全规则命中了这次操作的路径，需要你确认（设置 → 文件安全）。";

/// 命令规则的两句文案（design-security-center.md D4）。黑名单与拒绝规则共用拒绝句；
/// 命令规则在设置 → 命令安全里
const COMMAND_RULE_DENY: &str =
    "命令安全规则拦下了这条命令（程序黑名单或拒绝项）。规则在设置 → 命令安全里。";
const COMMAND_RULE_ASK: &str = "命令安全规则要求这条命令先确认（命中了前缀规则）。";

/// 网络规则与 HTTP 明文分档的文案（design-security-center.md D5）
const NET_RULE_DENY: &str =
    "网络安全规则把这一发划成了「拒绝」，没有执行。规则在设置 → 网络安全里。";
const NET_RULE_ASK: &str = "网络安全规则命中了这次访问的域名，需要你确认（设置 → 网络安全）。";
const NET_HTTP_ASK: &str =
    "这是一次 HTTP 明文请求：内容在网络上不加密，可能被窃听或篡改。要继续吗？（分档在设置 → 网络安全）";

/// 判定。顺序是有意的：白名单先于权限表（技能没给的能力，档位再高也不该拿去问用户），
/// 出口敏感过滤先于询问（把口令印在确认框里等用户点头，等于让用户替泄漏背书）
pub fn rule(
    policy: &Policy,
    call: &Call,
    display: &str,
    allowed_tools: Option<&[String]>,
) -> Ruling {
    let caps = capabilities_for(call);
    let mut keys: Vec<String> = caps.iter().map(|cap| cap.key()).collect();
    keys.push(format!("tool.{}", call.name));
    let key = keys.join("|");
    let fingerprint = policy::fingerprint(&[&key, &material(call)]);

    if let Some(reason) = allowlist_violation(call.name, allowed_tools) {
        return Ruling {
            decision: Decision::Deny { reason },
            key,
            fingerprint,
        };
    }

    // 参数要交给本机之外的程序时，先把明文口令拦下来。
    // 只对外发这条生效：`export TOKEN=…` 这类本地命令用户每天都在用，
    // 把它一并拒掉不是加护栏，是把护栏变成了路障
    if call.via_mcp {
        if let Some(kind) = secrets::leaks_sensitive(&call.args.to_string()) {
            return Ruling {
                decision: Decision::Deny {
                    reason: format!("参数里{kind}。要把这份内容交给外部程序，先把它从参数里拿掉。"),
                },
                key,
                fingerprint,
            };
        }
    }

    // 自我毁灭形的命令不是一档权限能放开的：full 档也不问，直接拒
    if let Some(why) = constrain::is_catastrophic(arg(call.args, "command")) {
        return Ruling {
            decision: Decision::Deny {
                reason: format!(
                    "这条命令是{why}，客户端不执行它。要做这件事请在应用外的终端里自己动手。"
                ),
            },
            key,
            fingerprint,
        };
    }

    // 规划阶段的红线：这一支只看不改。判据取 `tools::classify`——"这一条到底动不动东西"
    // 那张表本来就是唯一的出处（面板上的风险标签、审批默认档都读它），在这里另列一份
    // 工具名单就是第二个真相，而第二个真相迟早会和第一份漂移成两句不同的话。
    // 它和档位无关：`full` 开不动它，正如它开不动上面那条——覆盖项也撤不掉，
    // 所以这一步走在 `policy.check` 之前，压根不进那张表
    if policy.phase == Phase::Plan
        && tools::classify(call.name, call.args, call.root) != tools::Risk::Safe
    {
        return Ruling {
            decision: Decision::Deny {
                reason: PLAN_LOCK.to_string(),
            },
            key,
            fingerprint,
        };
    }

    // 命令安全规则（design-security-center.md D4）：黑名单 → 前缀规则 → 现行 ExecScope。
    // 黑名单命中即拒，`full` 与规则放行都翻不动（红线的红线）；未命中一字不改落回现行档。
    // ssh_run 的目标在别的机器上，本机的黑名单管不到它也不装管得到
    if call.name == "run_command" && !call.via_mcp {
        if let Some(program) = crate::command_rules::blocklist_hit(
            &policy.command_blocklist,
            arg(call.args, "command"),
        ) {
            return Ruling {
                decision: Decision::Deny {
                    reason: format!("{COMMAND_RULE_DENY}（命中的程序：{program}）"),
                },
                key,
                fingerprint,
            };
        }
        match crate::command_rules::prefix_hit(&policy.command_rules, arg(call.args, "command")) {
            Some(RuleAction::Deny) => {
                return Ruling {
                    decision: Decision::Deny {
                        reason: COMMAND_RULE_DENY.to_string(),
                    },
                    key,
                    fingerprint,
                };
            }
            Some(RuleAction::Ask) => {
                return Ruling {
                    decision: Decision::Ask {
                        reason: COMMAND_RULE_ASK.to_string(),
                        fingerprint: fingerprint.clone(),
                    },
                    key,
                    fingerprint,
                };
            }
            // 放行也要等黑名单与灾难清单都过完才兑现——它们在上面已经先走了
            Some(RuleAction::Allow) => {
                return Ruling {
                    decision: Decision::Allow,
                    key,
                    fingerprint,
                };
            }
            None => {}
        }
    }

    // 网络安全规则（design-security-center.md D5）+ HTTP 明文分档：先于现行判定。
    // 未命中一字不改落回现行档；规则之外，最外圈的出口名单与私网拒绝照旧在执行侧
    if let Some(url) = net_url(call) {
        match crate::egress::rule_hit(&policy.network_rules, url) {
            Some(RuleAction::Deny) => {
                return Ruling {
                    decision: Decision::Deny {
                        reason: NET_RULE_DENY.to_string(),
                    },
                    key,
                    fingerprint,
                };
            }
            Some(RuleAction::Ask) => {
                return Ruling {
                    decision: Decision::Ask {
                        reason: NET_RULE_ASK.to_string(),
                        fingerprint: fingerprint.clone(),
                    },
                    key,
                    fingerprint,
                };
            }
            Some(RuleAction::Allow) => {
                return Ruling {
                    decision: Decision::Allow,
                    key,
                    fingerprint,
                };
            }
            None => {
                // HTTP 明文分档：远程默认问、回环默认放（两个旋钮都在配置里）。
                // https 不动——它没有这一档的问题
                let lowered = url.trim().to_ascii_lowercase();
                if lowered.starts_with("http://") {
                    let host = crate::egress::host_of(url);
                    let local = host == "localhost"
                        || host == "::1"
                        || host == "[::1]"
                        || host.starts_with("127.")
                        || host.ends_with(".localhost");
                    let action = if local {
                        policy.net_http_local
                    } else {
                        policy.net_http_remote
                    };
                    match action {
                        RuleAction::Deny => {
                            return Ruling {
                                decision: Decision::Deny {
                                    reason: NET_HTTP_ASK.to_string(),
                                },
                                key,
                                fingerprint,
                            };
                        }
                        RuleAction::Ask => {
                            return Ruling {
                                decision: Decision::Ask {
                                    reason: NET_HTTP_ASK.to_string(),
                                    fingerprint: fingerprint.clone(),
                                },
                                key,
                                fingerprint,
                            };
                        }
                        RuleAction::Allow => {}
                    }
                }
            }
        }
    }

    // 文件安全规则表（design-security-center.md D2）：先于能力表。命中规则的路径用
    // 规则档，没命中的路径保持现行档，合并取最严——规则回答"这个具体目标放不放"，
    // 表回答"这一档放不放"，同一次调用两边都该作数。全表未命中 → None，
    // 落回现行判定，一字不改
    let rule_level = file_rule_level(policy, call);
    if let Some(level) = rule_level {
        match level {
            Level::Deny => {
                return Ruling {
                    decision: Decision::Deny {
                        reason: FILE_RULE_DENY.to_string(),
                    },
                    key,
                    fingerprint,
                };
            }
            Level::Ask => {
                return Ruling {
                    decision: Decision::Ask {
                        reason: FILE_RULE_ASK.to_string(),
                        fingerprint: fingerprint.clone(),
                    },
                    key,
                    fingerprint,
                };
            }
            // 放行落到底下走：批量删除阈值在它之后还有一票
            Level::Allow | Level::Scoped => {}
        }
    }

    let strictest = policy.strictest(&caps).expect("一次调用至少有一项能力");

    // 批量删除审批阈值（design-security-center.md D1）：一次删 ≥N 个文件就问人，
    // 档位与覆盖项都压不住——阈值是用户显式配的闸，`full` 也不能替他改主意。
    // 它压得住的只有会话内"以后都允许"：remember_key 含全部路径的指纹，
    // 同一批路径再删一次仍算记得，换个名单就是新的一条
    if call.name == "delete_file" && !call.via_mcp && policy.delete_batch_ask > 0 {
        let count = arg_paths(call.args).len();
        if count >= policy.delete_batch_ask {
            return Ruling {
                decision: Decision::Ask {
                    reason: format!(
                        "一次删除 {count} 个文件，达到批量删除审批阈值（{}）。确认前不动手。",
                        policy.delete_batch_ask
                    ),
                    fingerprint: fingerprint.clone(),
                },
                key,
                fingerprint,
            };
        }
    }

    // 规则放行在这里兑现：这发是用户拿自己的手笔放走的，不再过权限表
    if matches!(rule_level, Some(Level::Allow) | Some(Level::Scoped)) {
        return Ruling {
            decision: Decision::Allow,
            key,
            fingerprint,
        };
    }

    Ruling {
        decision: policy.check(strictest, display, &fingerprint),
        key,
        fingerprint,
    }
}

/// 文件规则层的一次裁决：`None` = 没有任何规则命中（整个调用落回能力表）。
/// 命中规则的路径用规则档；没命中的路径落回现行档，合并取最严——
/// 一次 `delete_file` 同时删规则内和规则外的文件时，两边都作数
fn file_rule_level(policy: &Policy, call: &Call) -> Option<Level> {
    let targets = file_targets(call);
    if targets.is_empty() || policy.file_rules.is_empty() {
        return None;
    }
    let mut any_hit = false;
    let mut worst: Option<Level> = None;
    for (mode, raw) in targets {
        let resolved = tools::resolve(&raw, call.root);
        let level = match crate::file_rules::hit(&policy.file_rules, mode, &resolved) {
            Some(level) => {
                any_hit = true;
                level
            }
            None => policy.resolve(&Capability::File {
                scope: path_scope(&raw, call.root),
                mode,
            }),
        };
        worst = Some(match worst {
            None => level,
            Some(held) if policy::strictness(level) > policy::strictness(held) => level,
            Some(held) => held,
        });
    }
    any_hit.then(|| worst.expect("有命中就至少有一个目标"))
}

/// 这次调用要动的（操作, 路径）清单。只有直接落在本地文件系统上的四个工具进
/// 规则层：ssh 与扩展那几路的目标在别处，路径表管不到它们，也不该装管得到
/// 这次调用要访问的 http/https URL（`web_fetch`/`browser`/`open_path` 三处）。
/// 不是这三路、或参数不是 http/https（open_path 打开本地文件），就没有网络那一维
fn net_url<'a>(call: &Call<'a>) -> Option<&'a str> {
    if call.via_mcp {
        return None;
    }
    let url = match call.name {
        "web_fetch" | "browser" => arg(call.args, "url"),
        "open_path" => arg(call.args, "path"),
        _ => return None,
    };
    let trimmed = url.trim();
    let lowered = trimmed.to_ascii_lowercase();
    if lowered.starts_with("http://") || lowered.starts_with("https://") {
        Some(trimmed)
    } else {
        None
    }
}

fn file_targets(call: &Call) -> Vec<(FileMode, String)> {
    if call.via_mcp {
        return Vec::new();
    }
    match call.name {
        "read_file" | "list_files" => vec![(FileMode::Read, arg(call.args, "path").to_string())],
        "write_file" | "edit_file" => vec![(FileMode::Write, arg(call.args, "path").to_string())],
        "delete_file" => arg_paths(call.args)
            .into_iter()
            .map(|raw| (FileMode::Delete, raw.to_string()))
            .collect(),
        _ => Vec::new(),
    }
}

/// 审计里那一行该写什么：动作 + 标识，**永远不写正文**
pub fn audit_target(call: &Call) -> String {
    let head = if call.via_mcp {
        format!("mcp:{}", call.name)
    } else {
        call.name.to_string()
    };
    let raw = match call.name {
        "run_command" | "write_file" | "read_file" | "list_files" if !call.via_mcp => {
            let value = if call.name == "run_command" {
                arg(call.args, "command")
            } else {
                arg(call.args, "path")
            };
            value.to_string()
        }
        // 删除的账要记全删了哪几个——这是"误删了什么"唯一的线索来源
        "delete_file" if !call.via_mcp => arg_paths(call.args).join(", "),
        _ => call.args.to_string(),
    };
    match secrets::redact_for_audit(&raw) {
        // 命中敏感规则：留下动作与够定位的前两个词，正文一个字符都不留
        Some(kind) => format!(
            "{head} {} ‹{kind}，正文不入审计›",
            raw.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
        ),
        None => format!("{head} {}", raw.chars().take(80).collect::<String>()),
    }
}

/// 话题当前的工具白名单：技能声明的并集，按话题 id 存。
/// 它改变的是"模型能调什么"，所以取用技能时必须在界面上说一句（`chat.rs` 的 Notice），
/// 不能让它成为看不见但会决定动作能不能跑的状态
static ACTIVE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, Vec<String>>>,
> = std::sync::OnceLock::new();

const ACTIVE_CAP: usize = 256;

fn active() -> &'static std::sync::Mutex<std::collections::HashMap<String, Vec<String>>> {
    ACTIVE.get_or_init(Default::default)
}

/// 登记这一发的工具面。`None` = 来路没给名单（技能 frontmatter 里没写 `allowed_tools`）：
/// 那只是"没有新东西要并进来"，已有的一切不动。`Some(list)` = 这一发允许的就是这一份，
/// **空表也是有效答案**——一个纯推理的监督者节点该是什么都不给。
///
/// 这里以前写的是"空表 = 不限制"，于是 `AgentProfile { tools: [] }` 那句注释
/// （"空数组 = 一个都不给"）与实际行为正好相反：那一发拿的是全部工具。
/// 返回合并后的名单，调用方用它决定要不要提示用户。并集而不是覆盖：
/// 同一话题里加载两个技能，第二个不该把第一个的限制解掉，也不该把第一个的能力收回
pub fn note_tools(conversation_id: &str, tools: Option<&[String]>) -> Vec<String> {
    let Some(tools) = tools else {
        return allowlist(conversation_id).unwrap_or_default();
    };
    let mut map = active()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // 话题是会被一直新建的：这里封顶，超了就整张重来。
    // 白名单是便利而不是一道墙，被清掉的最坏结果是"下一次调用重新问一遍"
    if map.len() >= ACTIVE_CAP && !map.contains_key(conversation_id) {
        map.clear();
    }
    let entry = map.entry(conversation_id.to_string()).or_default();
    for tool in tools {
        if !entry.iter().any(|held| held == tool) {
            entry.push(tool.clone());
        }
    }
    entry.clone()
}

/// `None` = 这个话题没作过限制；`Some(空表)` = 限制了，一个都不给。
/// 这两种状态以前在这张表里长得一样，而它们的意思正好相反
pub fn allowlist(conversation_id: &str) -> Option<Vec<String>> {
    let map = active()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.get(conversation_id).cloned()
}

pub fn clear_session(conversation_id: &str) {
    let mut map = active()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.remove(conversation_id);
}

/// 话题没了：它那一侧的两张表要**一起**忘掉。
///
/// 为什么需要一个函数做两件事：这两张表说的是同一句"这一支的权限从哪来"，
/// 只清一张就会留下一个没人认领的作用域。删掉的话题并不会消失干净——
/// `history.rs` 的 `remove_everywhere` 自己写着"只删当前后端的话，那条话题会在下次切回另一种后端时被迁移逻辑补回来"，
/// 而导入别的 app 也可能带同一个 id 回来。那时候它该是新的一段对话，
/// 而不是继承上一次留下的技能白名单或更严的那张权限表
pub fn forget_session(conversation_id: &str) {
    clear_session(conversation_id);
    clear_policies(conversation_id);
}

/// 话题作用域的权限表。档案（`orchestra::AgentProfile`）或任务设置可以为一次话题
/// 换一张**更严**的表；它和工具白名单住在同一份按话题切的作用域里，
/// 所以"这一支的权限从哪来"只有一个答案，不用翻两处
static POLICIES: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Policy>>> =
    std::sync::OnceLock::new();

pub fn set_policy(conversation_id: &str, policy: Policy) {
    let mut map = POLICIES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if map.len() >= ACTIVE_CAP && !map.contains_key(conversation_id) {
        map.clear();
    }
    map.insert(conversation_id.to_string(), policy);
}

/// 取这张表。没有登记过就用调用方给的那一份（也就是全局配置那张）
pub fn policy_for(conversation_id: &str, fallback: &Policy) -> Policy {
    POLICIES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(conversation_id)
        .cloned()
        .unwrap_or_else(|| fallback.clone())
}

pub fn clear_policies(conversation_id: &str) {
    let mut map = POLICIES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.remove(conversation_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn root_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aglab-gate-{tag}-{}",
            std::time::UNIX_EPOCH
                .elapsed()
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rule_in(mode: Mode, name: &str, args: &Value, root: &Path, via_mcp: bool) -> Ruling {
        let call = Call::new(name, args, Some(root), via_mcp);
        rule(&Policy::new(mode), &call, name, None)
    }

    #[test]
    fn reading_inside_the_project_needs_no_prompt_in_any_mode() {
        let root = root_dir("read");
        fs::write(root.join("a.rs"), "fn a() {}").unwrap();
        for mode in [Mode::Ask, Mode::Auto, Mode::Full] {
            let ruling = rule_in(mode, "read_file", &json!({ "path": "a.rs" }), &root, false);
            assert!(
                ruling.is_allow(),
                "{mode:?} 档下读项目内的文件今天就不弹窗，权限表不能把它变成弹窗：{ruling:?}"
            );
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn control_signals_are_not_capabilities_and_never_hit_the_arbitrary_exec_fallback() {
        // update_plan / ask_user 不动文件也不动本机：漏掉显式臂的话它们会掉进
        // 兜底按 exec.arbitrary 判，等于为一句"计划更新了"或"问个问题"弹一次确认框
        let root = root_dir("control-signals");
        for name in ["update_plan", "ask_user"] {
            let caps = capabilities_for(&Call::new(
                name,
                &json!({ "steps": [], "question": "?", "options": [] }),
                Some(&root),
                false,
            ));
            assert!(
                caps.iter()
                    .all(|cap| matches!(cap, Capability::Tool { .. })),
                "{name} 该映射成具名工具能力，实际是 {caps:?}"
            );
            let ruling = rule_in(Mode::Ask, name, &json!({}), &root, false);
            assert!(ruling.is_allow(), "控制信号在 ask 档也不该弹窗：{ruling:?}");
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_per_call_sandbox_request_that_moves_the_boundary_gets_its_own_gate() {
        // 逐调用沙箱策略：点名与生效档不同 = 动边界，多一行闸，auto 档也问一句。
        // 进程内全局沙箱默认关——这个前提下 "on" 是动边界（要给项目文件打标签），
        // "default"/"off"/缺省都不是
        let root = root_dir("sandbox-override");
        let caps = capabilities_for(&Call::new(
            "run_command",
            &json!({ "command": "echo hi", "sandbox": "on" }),
            Some(&root),
            false,
        ));
        assert!(
            caps.iter().any(|cap| matches!(
                cap,
                Capability::Exec {
                    scope: ExecScope::SandboxOverride
                }
            )),
            "全局没开要 on = 动边界，该多一行闸：{caps:?}"
        );
        let ruling = rule_in(
            Mode::Auto,
            "run_command",
            &json!({ "command": "echo hi", "sandbox": "on" }),
            &root,
            false,
        );
        assert!(
            matches!(ruling.decision, Decision::Ask { .. }),
            "动边界在 auto 档也要问一句：{ruling:?}"
        );

        for requested in ["default", "off"] {
            let caps = capabilities_for(&Call::new(
                "run_command",
                &json!({ "command": "echo hi", "sandbox": requested }),
                Some(&root),
                false,
            ));
            assert!(
                caps.iter().all(|cap| !matches!(
                    cap,
                    Capability::Exec {
                        scope: ExecScope::SandboxOverride
                    }
                )),
                "全局关着要「{requested}」不该被当成动边界：{caps:?}"
            );
        }

        // 另一半在纯函数上补齐：全局开着的分支不翻进程级原子，免得与沙箱测试互踩
        assert!(
            sandbox_boundary_move_with("off", true),
            "全局开着要 off = 脱壳"
        );
        assert!(
            !sandbox_boundary_move_with("on", true),
            "全局开着要 on 只是复述现状"
        );
        assert!(!sandbox_boundary_move_with("off", false));
        assert!(
            !sandbox_boundary_move_with("", true),
            "缺省跟全局，不是边界动作"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn writing_outside_the_project_is_denied_not_asked() {
        let root = root_dir("outside");
        let outside = std::env::temp_dir().join("aglab-gate-outside-target.txt");
        for mode in [Mode::Ask, Mode::Auto, Mode::Full] {
            let ruling = rule_in(
                mode,
                "write_file",
                &json!({ "path": outside.to_string_lossy(), "content": "x" }),
                &root,
                false,
            );
            assert!(
                matches!(ruling.decision, Decision::Deny { .. }),
                "根外写入在 {mode:?} 下也必须是被拒，弹窗等于把责任推给用户：{ruling:?}"
            );
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn commands_are_asked_until_the_mode_is_full() {
        let root = root_dir("cmd");
        for mode in [Mode::Ask, Mode::Auto] {
            let ruling = rule_in(
                mode,
                "run_command",
                &json!({ "command": "git push" }),
                &root,
                false,
            );
            assert!(
                matches!(ruling.decision, Decision::Ask { .. }),
                "{mode:?} 档下命令必须问：{ruling:?}"
            );
        }
        let full = rule_in(
            Mode::Full,
            "run_command",
            &json!({ "command": "git push" }),
            &root,
            false,
        );
        assert!(full.is_allow(), "full 档的语义就是不弹：{full:?}");
        // 而项目内写入在 ask 下问、auto 下过——这正是旧的 risk=Elevated 三档行为
        let write = json!({ "path": "a.rs", "content": "x" });
        assert!(matches!(
            rule_in(Mode::Ask, "write_file", &write, &root, false).decision,
            Decision::Ask { .. }
        ));
        assert!(rule_in(Mode::Auto, "write_file", &write, &root, false).is_allow());
        fs::remove_dir_all(&root).ok();
    }

    /// 规划阶段是红线不是问句：三档全拦，且覆盖项把它指向 Allow 也解不开
    #[test]
    fn the_plan_phase_locks_writes_at_every_tier() {
        let root = root_dir("plan-lock");
        fs::write(root.join("a.rs"), "fn a() {}").unwrap();
        let write = json!({ "path": "a.rs", "content": "x" });
        for mode in [Mode::Ask, Mode::Auto, Mode::Full] {
            let ruling = {
                let call = Call::new("write_file", &write, Some(root.as_path()), false);
                rule(
                    &Policy::new(mode).with_phase(Phase::Plan),
                    &call,
                    "写入",
                    None,
                )
            };
            assert!(
                matches!(ruling.decision, Decision::Deny { .. }),
                "{mode:?} 档下规划模式也必须拒掉写入，弹窗等于把解闸的责任推给用户：{ruling:?}"
            );
            assert!(
                ruling.reason().contains("规划模式"),
                "拒绝要说出是哪道闸拦的：{}",
                ruling.reason()
            );
            // 同一张表退回对话阶段，这条写入就照档位走——证明拦它的是阶段不是档位
            let back = {
                let call = Call::new("write_file", &write, Some(root.as_path()), false);
                rule(&Policy::new(mode), &call, "写入", None)
            };
            assert!(
                !matches!(back.decision, Decision::Deny { .. }),
                "退回对话阶段后 {mode:?} 不该被拒（那是档位的事，不是红线）：{back:?}"
            );
        }
        // 覆盖项松不开红线：file.write 写成 Allow 也照样拦
        let loosened = Policy {
            mode: Mode::Auto,
            overrides: vec![("file.write".into(), Level::Allow)],
            phase: Phase::Plan,
            delete_batch_ask: 50,
            file_rules: Vec::new(),
            command_blocklist: Vec::new(),
            command_rules: Vec::new(),
            network_rules: Vec::new(),
            net_http_remote: crate::file_rules::RuleAction::Ask,
            net_http_local: crate::file_rules::RuleAction::Allow,
        };
        let call = Call::new("write_file", &write, Some(root.as_path()), false);
        assert!(
            matches!(
                rule(&loosened, &call, "写入", None).decision,
                Decision::Deny { .. }
            ),
            "覆盖项只能往严了改，用它解规划模式的红线等于给静默松闸开门"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// 只读的那几条不受阶段影响：规划模式还是要能看代码、查资料，否则连方案都写不出来。
    /// 这条不钉"放行"，钉的是"与对话阶段判得一模一样"——问不问由档位管，阶段不参与
    #[test]
    fn the_plan_phase_does_not_tighten_the_read_only_paths() {
        let root = root_dir("plan-read");
        fs::write(root.join("a.rs"), "fn a() {}").unwrap();
        for (name, args) in [
            ("read_file", json!({ "path": "a.rs" })),
            ("list_files", json!({ "path": "." })),
            ("search_text", json!({ "query": "fn", "path": "." })),
            ("web_fetch", json!({ "url": "https://example.com" })),
            ("load_skill", json!({ "name": "pdf" })),
            ("knowledge_search", json!({ "query": "fn" })),
            ("obs_recall", json!({ "handle": "#x" })),
        ] {
            let call = Call::new(name, &args, Some(root.as_path()), false);
            let chat = rule(&Policy::new(Mode::Ask), &call, name, None).decision;
            let plan = rule(
                &Policy::new(Mode::Ask).with_phase(Phase::Plan),
                &call,
                name,
                None,
            )
            .decision;
            assert_eq!(chat, plan, "{name} 是只读的那一条，规划模式不该改它的判定");
        }
        fs::remove_dir_all(&root).ok();
    }

    /// 阶段收紧的是"会动东西"那一侧：同一条动作在对话阶段问一句，在规划阶段直接拒
    #[test]
    fn the_plan_phase_tightens_the_mutating_paths() {
        let root = root_dir("plan-tighten");
        fs::write(root.join("a.rs"), "fn a() {}").unwrap();
        let write = json!({ "path": "a.rs", "content": "x" });
        let call = Call::new("write_file", &write, Some(root.as_path()), false);
        assert!(
            matches!(
                rule(&Policy::new(Mode::Ask), &call, "写入", None).decision,
                Decision::Ask { .. }
            ),
            "对话阶段里 ask 档写项目内要问一句——这是今天的行为，不能被阶段改掉"
        );
        let plan = Policy::new(Mode::Ask).with_phase(Phase::Plan);
        assert!(
            matches!(
                rule(&plan, &call, "写入", None).decision,
                Decision::Deny { .. }
            ),
            "同一条动作在规划阶段得被拒，弹窗等于让人一路点同意把规划模式过掉"
        );

        // 根外的读同理：对话阶段它不在档位上被拦（临时目录还是应用自己的那一圈），
        // 规划阶段一律拒
        let outside = std::env::temp_dir().join("aglab-gate-plan-outside.txt");
        fs::write(&outside, "x").unwrap();
        let args = json!({ "path": outside.to_string_lossy() });
        let call = Call::new("read_file", &args, Some(root.as_path()), false);
        let chat = rule(&Policy::new(Mode::Ask), &call, "读取", None).decision;
        assert!(
            !matches!(chat, Decision::Deny { .. }),
            "对话阶段里这条读不该被拒，否则这条针就是在测一件今天不发生的事：{chat:?}"
        );
        assert!(
            matches!(
                rule(&plan, &call, "读取", None).decision,
                Decision::Deny { .. }
            ),
            "根外的读在规划模式里也该拒"
        );
        fs::remove_dir_all(&root).ok();
        fs::remove_file(&outside).ok();
    }

    #[test]
    fn a_hallucinated_tool_name_is_asked_not_silently_run() {
        let root = root_dir("hallu");
        let ruling = rule_in(Mode::Auto, "delete_the_world", &json!({}), &root, false);
        assert!(
            matches!(ruling.decision, Decision::Ask { .. }),
            "名单外的名字要动什么无从得知，静默执行是这条闸门最不能接受的失败方式：{ruling:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_mcp_call_carrying_a_token_is_denied_and_the_reason_keeps_no_body() {
        let root = root_dir("mcp");
        let ruling = rule_in(
            Mode::Full,
            "mcp__notion__create_page",
            &json!({ "title": "纪要", "token": "sk-abcdefghijklmnopqrstuvwxyz012345" }),
            &root,
            true,
        );
        let Decision::Deny { reason } = ruling.decision else {
            panic!("full 档下把口令交给外部程序也要拦：{ruling:?}");
        };
        assert!(
            !reason.contains("sk-abcdefghijklmnop"),
            "拒绝原因里不能把口令再抄一遍：{reason}"
        );
        // 同样这个扩展调用不带口令时，full 档按今天的行为放行
        let clean = rule_in(
            Mode::Full,
            "mcp__notion__create_page",
            &json!({ "title": "纪要" }),
            &root,
            true,
        );
        assert!(
            clean.is_allow(),
            "扩展调用在 full 档下按现在的行为自动放行：{clean:?}"
        );
        // 而 ask 档下它要问——扩展跑的是别人的程序，这条不能因为换了实现就变松
        let asked = rule_in(
            Mode::Ask,
            "mcp__notion__create_page",
            &json!({ "title": "纪要" }),
            &root,
            true,
        );
        assert!(matches!(asked.decision, Decision::Ask { .. }), "{asked:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_tool_the_active_skill_does_not_allow_is_denied() {
        let root = root_dir("skill");
        fs::write(root.join("a.rs"), "fn a() {}").unwrap();
        let allowed = vec!["read_file".to_string(), "Bash(git *)".to_string()];
        let read_args = json!({ "path": "a.rs" });
        let write_args = json!({ "path": "a.rs", "content": "x" });
        let read = Call::new("read_file", &read_args, Some(&root), false);
        assert!(
            rule(&Policy::new(Mode::Full), &read, "读取", Some(&allowed)).is_allow(),
            "名单内的工具不该被拦"
        );
        let write = Call::new("write_file", &write_args, Some(&root), false);
        let ruling = rule(&Policy::new(Mode::Full), &write, "写入", Some(&allowed));
        assert!(
            matches!(ruling.decision, Decision::Deny { .. }),
            "技能白名单必须在权限表之前生效，否则它又是一句印进提示词的愿望：{ruling:?}"
        );
        // 给了空表与没给名单是两件事。这一条今天被拆开过：以前两者都读成"不限制"，
        // 于是 `AgentProfile { tools: [] }` 那句"空数组 = 一个都不给"实际发的是全套
        let empty_list = vec![];
        let unrestricted_by_absence = Call::new("write_file", &write_args, Some(&root), false);
        assert!(
            matches!(
                rule(
                    &Policy::new(Mode::Full),
                    &unrestricted_by_absence,
                    "写入",
                    Some(&empty_list)
                )
                .decision,
                Decision::Deny { .. }
            ),
            "给了空表就是什么都不许"
        );
        assert!(
            rule(
                &Policy::new(Mode::Full),
                &unrestricted_by_absence,
                "写入",
                None
            )
            .is_allow(),
            "没给名单 = 不收窄，那是技能没写白名单时的既有语义"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_fingerprint_identifies_the_action_not_its_spacing() {
        let root = root_dir("fp");
        let a = rule_in(
            Mode::Ask,
            "run_command",
            &json!({ "command": "git  push   origin main" }),
            &root,
            false,
        );
        let b = rule_in(
            Mode::Ask,
            "run_command",
            &json!({ "command": "git push origin main" }),
            &root,
            false,
        );
        let c = rule_in(
            Mode::Ask,
            "run_command",
            &json!({ "command": "git push origin dev" }),
            &root,
            false,
        );
        assert_eq!(
            a.fingerprint, b.fingerprint,
            "同一句命令换个空格就是另一次审批，是假精细"
        );
        assert_ne!(
            a.fingerprint, c.fingerprint,
            "换分支不是同一个动作，不能共用一次点头"
        );
        // 记住的键随内容变：写同一个文件、正文不同，是两次不同的授权
        let w1 = rule_in(
            Mode::Ask,
            "write_file",
            &json!({ "path": "a.rs", "content": "one" }),
            &root,
            false,
        );
        let w2 = rule_in(
            Mode::Ask,
            "write_file",
            &json!({ "path": "a.rs", "content": "two" }),
            &root,
            false,
        );
        assert_ne!(w1.remember_key(), w2.remember_key());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn audit_lines_never_carry_the_command_body() {
        let root = root_dir("audit");
        let args = json!({
            "command": "aws s3 cp ./out.txt s3://bucket --secret-access-key AKIAIOSFODNN7EXAMPLE"
        });
        let call = Call::new("run_command", &args, Some(&root), false);
        let line = audit_target(&call);
        assert!(
            !line.contains("AKIAIOSFODNN7EXAMPLE"),
            "审计里只留动作与标识：{line}"
        );
        assert!(
            line.contains("run_command"),
            "但也不能少到认不出是哪一次：{line}"
        );
        assert!(
            line.contains("aws s3"),
            "前两个词还能定位到是哪条命令：{line}"
        );
        // 不含敏感信息的普通命令照常留头
        let plain_args = json!({ "command": "git status --short" });
        let plain = Call::new("run_command", &plain_args, Some(&root), false);
        assert_eq!(audit_target(&plain), "run_command git status --short");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_loaded_skill_restricts_the_rest_of_the_conversation_and_nothing_else() {
        let root = root_dir("scope");
        fs::write(root.join("a.rs"), "fn a() {}").unwrap();
        let conversation = "conv-1";
        clear_session(conversation);
        assert_eq!(allowlist(conversation), None, "没取用过技能 = 不限制");

        let merged = note_tools(conversation, Some(&["read_file".into()]));
        assert_eq!(merged, vec!["read_file".to_string()]);
        // 第二个技能是并集：它既不该解掉第一个的限制，也不该把第一个的能力收回
        let merged = note_tools(
            conversation,
            Some(&["list_files".into(), "read_file".into()]),
        );
        assert_eq!(
            merged,
            vec!["read_file".to_string(), "list_files".to_string()]
        );
        assert_eq!(allowlist(conversation).as_deref(), Some(merged.as_slice()));

        // 没给名单（技能 frontmatter 里没写）与给了空表是两件事：前者不动这张表，后者是"一个都不许"
        assert_eq!(note_tools("conv-empty", Some(&[])), Vec::<String>::new());
        assert_eq!(
            allowlist("conv-empty"),
            Some(Vec::new()),
            "空表要留得下来：它就是 `AgentProfile {{ tools: [] }} 那一句的意思"
        );
        assert!(
            allowlist_violation("read_file", allowlist("conv-empty").as_deref()).is_some(),
            "限制了空表之后任何一个名字都该被拒"
        );
        note_tools("conv-none", None);
        assert_eq!(
            allowlist("conv-none"),
            None,
            "没给名单 = 不限制，也不留下一个空的条目"
        );

        // 别的话题不受影响：作用域按话题切，这是"权限隔离"最小可用的一刀
        assert_eq!(allowlist("conv-2"), None);

        let write_args = json!({ "path": "a.rs", "content": "x" });
        let write = Call::new("write_file", &write_args, Some(&root), false);
        assert!(
            matches!(
                rule(
                    &Policy::new(Mode::Auto),
                    &write,
                    "写入",
                    allowlist(conversation).as_deref()
                )
                .decision,
                Decision::Deny { .. }
            ),
            "名单外的写入在 auto 档下也要被拒，不然这份名单只对着 ask 档生效"
        );
        clear_session(conversation);
        assert_eq!(allowlist(conversation), None, "清空后要回到不限制");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_session_policy_can_tighten_a_conversation_but_not_loosen_the_mode() {
        use crate::policy::Level;
        let conversation = "conv-policy";
        let global = Policy::new(Mode::Ask);
        assert_eq!(
            policy_for(conversation, &global).mode,
            global.mode,
            "没登记过的话题用的就是全局那一张，不该有一张隐形的默认表"
        );
        clear_policies(conversation);

        set_policy(
            conversation,
            Policy {
                mode: Mode::Ask,
                overrides: vec![("tool.read_file".into(), Level::Deny)],
                phase: crate::policy::Phase::Chat,
                delete_batch_ask: 50,
                file_rules: Vec::new(),
                command_blocklist: Vec::new(),
                command_rules: Vec::new(),
                network_rules: Vec::new(),
                net_http_remote: crate::file_rules::RuleAction::Ask,
                net_http_local: crate::file_rules::RuleAction::Allow,
            },
        );
        assert_eq!(
            policy_for(conversation, &global).resolve(&Capability::Tool {
                name: "read_file".into()
            }),
            Level::Deny,
            "话题上挂着的更严那一张必须被判定读到，否则每任务独立权限是句空话"
        );

        // 想借一份档案放松全局档：不行
        set_policy(
            conversation,
            Policy {
                mode: Mode::Ask,
                overrides: vec![("exec".into(), Level::Allow)],
                phase: crate::policy::Phase::Chat,
                delete_batch_ask: 50,
                file_rules: Vec::new(),
                command_blocklist: Vec::new(),
                command_rules: Vec::new(),
                network_rules: Vec::new(),
                net_http_remote: crate::file_rules::RuleAction::Ask,
                net_http_local: crate::file_rules::RuleAction::Allow,
            },
        );
        assert_eq!(
            policy_for(conversation, &global).resolve(&Capability::Exec {
                scope: ExecScope::Git
            }),
            Level::Ask,
            "ask 档不会因为一份档案写了 Allow 就不问"
        );

        clear_policies(conversation);
        assert_eq!(
            policy_for(conversation, &global).resolve(&Capability::Tool {
                name: "read_file".into()
            }),
            Level::Scoped,
            "清掉之后要回到全局那一张，不留一张没人认领的表"
        );
    }

    /// 两张按话题切的作用域要**一起**忘掉。只清一张时，一个被补回来的同 id 话题
    /// 会继承上一次留下的那一半——而这两半合起来才是"这一支的权限从哪来"
    #[test]
    fn forgetting_a_session_clears_both_halves_of_its_scope() {
        use crate::policy::Level;
        let conversation = "conv-forgotten";
        let global = Policy::new(Mode::Ask);
        note_tools(conversation, Some(&["read_file".to_string()]));
        set_policy(
            conversation,
            Policy {
                mode: Mode::Ask,
                overrides: vec![("tool.read_file".into(), Level::Deny)],
                phase: crate::policy::Phase::Chat,
                delete_batch_ask: 50,
                file_rules: Vec::new(),
                command_blocklist: Vec::new(),
                command_rules: Vec::new(),
                network_rules: Vec::new(),
                net_http_remote: crate::file_rules::RuleAction::Ask,
                net_http_local: crate::file_rules::RuleAction::Allow,
            },
        );
        assert_eq!(
            allowlist(conversation),
            Some(vec!["read_file".to_string()]),
            "前提没立住：这一格该带着白名单"
        );
        assert_eq!(
            policy_for(conversation, &global).resolve(&Capability::Tool {
                name: "read_file".into()
            }),
            Level::Deny,
            "前提没立住：这一格该带着更严的那张表"
        );

        forget_session(conversation);

        assert_eq!(allowlist(conversation), None, "白名单那一半没跟着清");
        assert_eq!(
            policy_for(conversation, &global).resolve(&Capability::Tool {
                name: "read_file".into()
            }),
            Level::Scoped,
            "权限表那一半没跟着清"
        );
    }

    #[test]
    fn the_command_head_that_goes_into_the_table_is_classified_by_its_first_word() {
        let key = |scope: ExecScope| Capability::Exec { scope }.key();
        assert_eq!(key(exec_scope("git push origin main")), "exec.git");
        assert_eq!(key(exec_scope("CARGO  test")), "exec.build");
        assert_eq!(key(exec_scope("net user")), "exec.arbitrary");
        assert_eq!(key(exec_scope("")), "exec.arbitrary");
        assert_eq!(
            key(exec_scope("\"git\" status")),
            "exec.git",
            "带引号的可执行名也是 git"
        );
        assert_eq!(
            key(exec_scope("cargo.exe build")),
            "exec.build",
            "Windows 的 .exe 后缀不算另一个命令"
        );
    }

    /// 内置工具的名字只许有一处在册。这一文件以前自己养着 `const KNOWN: [&str; 5]`
    /// 加一个 `is_known`——那是 `tools::is_registered`（读的是真注册表）的第二份答案：
    /// 注册表加了第六条而那份名单忘了改时，两个函数就会对同一个名字给出两个回答。
    /// 生产代码零消费者不等于无害，它只是还没被用到
    #[test]
    fn the_builtin_names_are_listed_in_exactly_one_place() {
        // 切成"生产段"时不能拿 `#[cfg(test)]` 当刀：这一文件第 17 行就有一条
        // test-only 的 `use`，那样切出来的前 16 行里当然一个 KNOWN 也没有——
        // 针会绿，而它谁都没看。要切在测试模块那一行上，并且自证读到了正文
        let production = include_str!("mod.rs")
            .replace('\r', "")
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .unwrap_or_default()
            .to_string();
        assert!(
            production.contains("pub fn rule("),
            "这一段没读到生产代码，下面那条计数因此是空转的"
        );
        assert_eq!(
            production.matches("KNOWN").count(),
            0,
            "这一文件里不该再有第二份内置名单"
        );
        // 唯一的那份答案住在 tools.rs，而它真的有读者
        let registry = include_str!("../tools.rs").replace('\r', "");
        assert!(
            registry.contains("pub fn is_registered"),
            "唯一的名单谓词得在那里"
        );
        let claimed = include_str!("source.rs").replace('\r', "");
        assert!(
            claimed.contains("tools::is_registered"),
            "而且路由要读它，不然它只是换一个地方当装饰"
        );
    }

    /// `ExecScope::Arbitrary` 不带"是哪条命令"那一格：判定与审批键都只读
    /// `exec.arbitrary` 那一段，留一个没人读的字段会让人以为 `exec.notion` 配得上
    #[test]
    fn an_unclassified_command_is_just_arbitrary_and_nothing_more() {
        for command in [
            "notion run --all",
            "rm -rf /",
            "",
            "\"C:\\Program Files\\x.exe\"",
        ] {
            assert_eq!(
                exec_scope(command),
                ExecScope::Arbitrary,
                "认不出的都只到 arbitrary：{command}"
            );
            let args = json!({ "command": command });
            let call = Call::new("run_command", &args, None, false);
            let caps = capabilities_for(&call);
            assert_eq!(caps.len(), 1, "一条命令只问一个维度：{command}");
            assert_eq!(
                caps[0].key(),
                "exec.arbitrary",
                "键里不许藏命令名：{command}"
            );
        }
        // 正对照：认得出的两类各归各的键
        assert_eq!(exec_scope("git push origin main"), ExecScope::Git);
        assert_eq!(exec_scope("cargo test --lib"), ExecScope::Build);
    }

    // ---- delete_file（design-security-center.md D1）----

    #[test]
    fn deleting_asks_by_default_and_the_outside_is_a_hard_no() {
        let root = root_dir("delete-single");
        // 任何档：项目外是硬错（与根外写入同一行 Deny）。路径不需要存在——
        // 判定是词法范围的账，不在文件系统上赌
        let outside = "C:/Program Files/aglab-must-not-delete.txt";
        for mode in [Mode::Ask, Mode::Auto, Mode::Full] {
            let ruling = rule_in(
                mode,
                "delete_file",
                &json!({ "paths": [outside] }),
                &root,
                false,
            );
            assert!(
                matches!(ruling.decision, Decision::Deny { .. }),
                "{mode:?} 档下项目外删除也不该放行：{ruling:?}"
            );
        }
        // ask 档：项目内删除要问（删除没有 Scoped，执行面比写严一档）
        let ruling = rule_in(
            Mode::Ask,
            "delete_file",
            &json!({ "paths": ["a.txt"] }),
            &root,
            false,
        );
        assert!(
            matches!(ruling.decision, Decision::Ask { .. }),
            "{ruling:?}"
        );
        // full 档：项目内放行
        let ruling = rule_in(
            Mode::Full,
            "delete_file",
            &json!({ "paths": ["a.txt"] }),
            &root,
            false,
        );
        assert!(ruling.is_allow(), "{ruling:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_batch_over_the_threshold_asks_even_in_full_mode() {
        let root = root_dir("delete-batch");
        let args = json!({ "paths": (0..60).map(|i| format!("f{i}.txt")).collect::<Vec<_>>() });
        let call = Call::new("delete_file", &args, Some(&root), false);
        // full 档 + 阈值 50：60 个路径强制问——阈值是用户显式配的闸，full 也改不了主意
        match rule(&Policy::new(Mode::Full), &call, "delete_file", None).decision {
            Decision::Ask { reason, .. } => {
                assert!(reason.contains("60"), "文案要点出这一次删了多少：{reason}")
            }
            other => panic!("批量删除要问人：{other:?}"),
        }
        // 阈值之下一切照旧
        let small = json!({ "paths": ["a.txt", "b.txt"] });
        assert!(rule(
            &Policy::new(Mode::Full),
            &Call::new("delete_file", &small, Some(&root), false),
            "delete_file",
            None
        )
        .is_allow());
        assert!(matches!(
            rule(
                &Policy::new(Mode::Ask),
                &Call::new("delete_file", &small, Some(&root), false),
                "delete_file",
                None
            )
            .decision,
            Decision::Ask { .. }
        ));
        // 阈值关掉（0）：批量闸不存在
        let mut off = Policy::new(Mode::Ask);
        off.delete_batch_ask = 0;
        assert!(
            matches!(
                rule(&off, &call, "delete_file", None).decision,
                Decision::Ask { .. }
            ),
            "ask 档的询问来自档位而不是阈值"
        );
        let mut full_off = Policy::new(Mode::Full);
        full_off.delete_batch_ask = 0;
        assert!(rule(&full_off, &call, "delete_file", None).is_allow());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_different_path_set_is_a_different_remember_key() {
        let root = root_dir("delete-remember");
        let a = rule_in(
            Mode::Full,
            "delete_file",
            &json!({ "paths": ["a.txt"] }),
            &root,
            false,
        );
        let b = rule_in(
            Mode::Full,
            "delete_file",
            &json!({ "paths": ["b.txt"] }),
            &root,
            false,
        );
        let again = rule_in(
            Mode::Full,
            "delete_file",
            &json!({ "paths": ["a.txt"] }),
            &root,
            false,
        );
        assert_ne!(a.remember_key(), b.remember_key(), "换个名单就是新的一条");
        assert_eq!(
            a.remember_key(),
            again.remember_key(),
            "同一批路径才共用一条会话内规则"
        );
        fs::remove_dir_all(&root).ok();
    }

    // ---- 文件安全规则表（design-security-center.md D2）----

    #[test]
    fn a_file_rule_overrides_the_table_for_its_paths_and_lets_the_rest_fall_through() {
        use crate::file_rules::{FileRule, RuleAction};
        let root = root_dir("file-rules");
        let mut policy = Policy::new(Mode::Full);
        policy.file_rules = vec![FileRule {
            pattern: root.join(".env").to_string_lossy().to_string(),
            read: RuleAction::Deny,
            write: RuleAction::Deny,
            delete: RuleAction::Deny,
        }];
        // full 档压不住规则拒绝：读与写 .env 都被拦
        for (name, args) in [
            ("write_file", json!({ "path": ".env", "content": "A=1" })),
            ("read_file", json!({ "path": ".env" })),
        ] {
            let ruling = rule(
                &policy,
                &Call::new(name, &args, Some(&root), false),
                name,
                None,
            );
            assert!(
                matches!(ruling.decision, Decision::Deny { .. }),
                "{name} {ruling:?}"
            );
        }
        // 没命中的路径落回现行表：full 档写别的文件照常放行，一字不改
        let ruling = rule(
            &policy,
            &Call::new(
                "write_file",
                &json!({ "path": "src.rs", "content": "fn a(){}" }),
                Some(&root),
                false,
            ),
            "write_file",
            None,
        );
        assert!(ruling.is_allow(), "{ruling:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_rule_allow_lets_an_outside_path_through_but_the_threshold_still_counts() {
        use crate::file_rules::{FileRule, RuleAction};
        let root = root_dir("file-rules-allow");
        let outside = root_dir("file-rules-outside");
        // 项目外路径在表上是硬错（file.delete.any = Deny）；规则显式放行它——
        // 放行是用户的显式手笔（与「以后都允许」同责），表上的硬错让位
        let mut policy = Policy::new(Mode::Full);
        policy.delete_batch_ask = 0;
        policy.file_rules = vec![FileRule {
            pattern: outside.to_string_lossy().to_string(),
            read: RuleAction::Allow,
            write: RuleAction::Allow,
            delete: RuleAction::Allow,
        }];
        let target = outside
            .join("cache.bin")
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR.to_string().as_str(), "/");
        let ruling = rule(
            &policy,
            &Call::new(
                "delete_file",
                &json!({ "paths": [target] }),
                Some(&root),
                false,
            ),
            "delete_file",
            None,
        );
        assert!(ruling.is_allow(), "规则放行要兑现：{ruling:?}");
        // 但批量删除阈值压得住规则放行：≥N 个文件照样问人
        policy.delete_batch_ask = 2;
        let two = json!({ "paths": [format!("{target}1"), format!("{target}2")] });
        let ruling = rule(
            &policy,
            &Call::new("delete_file", &two, Some(&root), false),
            "delete_file",
            None,
        );
        assert!(
            matches!(ruling.decision, Decision::Ask { .. }),
            "阈值是用户配的闸：{ruling:?}"
        );
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&outside).ok();
    }

    // ---- 命令安全规则（design-security-center.md D4）----

    #[test]
    fn the_blocklist_denies_whatever_the_mode_says() {
        let root = root_dir("cmd-blocklist");
        let mut policy = Policy::new(Mode::Full);
        policy.command_blocklist = vec!["reg.exe".into()];
        for command in ["reg export HKLM", "cargo build && reg export HKLM"] {
            let ruling = rule(
                &policy,
                &Call::new(
                    "run_command",
                    &json!({ "command": command }),
                    Some(&root),
                    false,
                ),
                "run_command",
                None,
            );
            assert!(
                matches!(ruling.decision, Decision::Deny { .. }),
                "{command} {ruling:?}"
            );
        }
        // 不在名单里：full 档照常走现行档（exec.build → Allow）
        let ruling = rule(
            &policy,
            &Call::new(
                "run_command",
                &json!({ "command": "cargo build" }),
                Some(&root),
                false,
            ),
            "run_command",
            None,
        );
        assert!(ruling.is_allow(), "{ruling:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_prefix_rule_ask_beats_full_and_allow_beats_auto() {
        let root = root_dir("cmd-prefix");
        let rules = vec![
            crate::command_rules::CommandRule {
                prefix: "git push".into(),
                action: RuleAction::Ask,
            },
            crate::command_rules::CommandRule {
                prefix: "cargo test".into(),
                action: RuleAction::Allow,
            },
        ];
        let mut full = Policy::new(Mode::Full);
        full.command_rules = rules.clone();
        let ruling = rule(
            &full,
            &Call::new(
                "run_command",
                &json!({ "command": "git push origin main" }),
                Some(&root),
                false,
            ),
            "run_command",
            None,
        );
        assert!(
            matches!(ruling.decision, Decision::Ask { .. }),
            "ask 规则压住 full 档：{ruling:?}"
        );
        let ruling = rule(
            &full,
            &Call::new(
                "run_command",
                &json!({ "command": "cargo test --lib" }),
                Some(&root),
                false,
            ),
            "run_command",
            None,
        );
        assert!(ruling.is_allow(), "allow 规则要兑现：{ruling:?}");
        // 未命中规则一字不改落回现行档：auto 档下未分类命令要问
        let mut auto = Policy::new(Mode::Auto);
        auto.command_rules = rules;
        let ruling = rule(
            &auto,
            &Call::new(
                "run_command",
                &json!({ "command": "some-unknown-tool --do-things" }),
                Some(&root),
                false,
            ),
            "run_command",
            None,
        );
        assert!(
            matches!(ruling.decision, Decision::Ask { .. }),
            "未命中规则落回 exec.arbitrary：{ruling:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_catastrophic_check_still_wins_over_a_rule_allow() {
        let root = root_dir("cmd-catastrophic");
        let mut policy = Policy::new(Mode::Full);
        policy.command_rules = vec![crate::command_rules::CommandRule {
            prefix: "format".into(),
            action: RuleAction::Allow,
        }];
        let ruling = rule(
            &policy,
            &Call::new(
                "run_command",
                &json!({ "command": "format c: /q" }),
                Some(&root),
                false,
            ),
            "run_command",
            None,
        );
        assert!(
            matches!(ruling.decision, Decision::Deny { .. }),
            "灾难清单在规则层之前：规则放行救不回一条格式化命令：{ruling:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    // ---- 网络安全规则与 HTTP 明文分档（design-security-center.md D5）----

    #[test]
    fn a_net_rule_denies_asks_and_allows_by_domain_order() {
        let root = root_dir("net-rules");
        let mut policy = Policy::new(Mode::Full);
        policy.network_rules = vec![
            crate::egress::NetworkRule {
                pattern: "evil.example".into(),
                action: RuleAction::Deny,
            },
            crate::egress::NetworkRule {
                pattern: "example.com".into(),
                action: RuleAction::Ask,
            },
        ];
        // 精确与子域都命中
        for url in ["https://evil.example/x", "https://api.evil.example/x"] {
            let ruling = rule(
                &policy,
                &Call::new("web_fetch", &json!({ "url": url }), Some(&root), false),
                "web_fetch",
                None,
            );
            assert!(
                matches!(ruling.decision, Decision::Deny { .. }),
                "{url} {ruling:?}"
            );
        }
        let ruling = rule(
            &policy,
            &Call::new(
                "web_fetch",
                &json!({ "url": "https://example.com/page" }),
                Some(&root),
                false,
            ),
            "web_fetch",
            None,
        );
        assert!(
            matches!(ruling.decision, Decision::Ask { .. }),
            "{ruling:?}"
        );
        // 后缀不吞相似名：notexample.com 不被 example.com 命中，落回现行判定（full 档下 web_fetch 本来就不问）
        let ruling = rule(
            &policy,
            &Call::new(
                "web_fetch",
                &json!({ "url": "https://notexample.com/" }),
                Some(&root),
                false,
            ),
            "web_fetch",
            None,
        );
        assert!(ruling.is_allow(), "{ruling:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn http_plaintext_asks_remotely_and_passes_on_loopback() {
        let root = root_dir("net-http");
        let policy = Policy::new(Mode::Full);
        // 远程 http：默认问
        let ruling = rule(
            &policy,
            &Call::new(
                "web_fetch",
                &json!({ "url": "http://example.com/a" }),
                Some(&root),
                false,
            ),
            "web_fetch",
            None,
        );
        assert!(
            matches!(ruling.decision, Decision::Ask { .. }),
            "{ruling:?}"
        );
        // 回环 http：默认放
        let ruling = rule(
            &policy,
            &Call::new(
                "open_path",
                &json!({ "path": "http://127.0.0.1:8080/health" }),
                Some(&root),
                false,
            ),
            "open_path",
            None,
        );
        assert!(ruling.is_allow(), "{ruling:?}");
        // https 不吃这一档
        let ruling = rule(
            &policy,
            &Call::new(
                "web_fetch",
                &json!({ "url": "https://example.com/a" }),
                Some(&root),
                false,
            ),
            "web_fetch",
            None,
        );
        assert!(ruling.is_allow(), "{ruling:?}");
        // 旋钮拧到 deny：远程 http 直接拒
        let mut deny = Policy::new(Mode::Full);
        deny.net_http_remote = RuleAction::Deny;
        let ruling = rule(
            &deny,
            &Call::new(
                "web_fetch",
                &json!({ "url": "http://example.com/a" }),
                Some(&root),
                false,
            ),
            "web_fetch",
            None,
        );
        assert!(
            matches!(ruling.decision, Decision::Deny { .. }),
            "{ruling:?}"
        );
        fs::remove_dir_all(&root).ok();
    }
}
