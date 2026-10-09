//! AgentProfile：一个节点"是谁、能碰什么、花多少"。
//!
//! 六项都在这里：角色、模型、工具白名单、权限、记忆作用域、预算。
//! **权限不在这份档案里重新定义**——它只引用 `policy.rs` 那张表，
//! 因为分级表的定义处只能有一个（`design-security-permission.md` 的边界）。

use serde::{Deserialize, Serialize};

use crate::orchestra::graph::Budget;
use crate::policy::{Capability, ExecScope, FileMode, Level, PathScope, Policy};

/// 记忆作用域。`Namespaced` = 只读这份 plan 自己的那批记忆，
/// 一个探索性节点不该把它的临时结论当成全局事实读进来。
///
/// **今天这一维没有执行者，两半都缺**，别说成"已隔离"：
/// 1. 节点那一发走 `chat::run_turn_into`，而记忆注入只挂在 `chat::run_turn`（用户那条路）上——
///    编排的节点**根本不读记忆**，所以"只读哪一批"无从生效；
/// 2. 它还与 `crate::memory::record::MemoryScope`（`Global/Project/Session/Temp`）
///    **是两个同名的词表**：记忆层从没听说过 `Namespaced`，就算接上注入也判不出这一格。
///
/// 要做成真需要两件事：给节点那一发开一条注入入口（**它会按字节抬高每一发的上下文，
/// 也就是真金白银**，属于要先点头的改动），以及在记忆层落下命名空间的键。
/// 有一条测试（`the_profiles_memory_scope_still_has_no_executor_and_says_so`）盯着这两半，
/// 任何一半先动起来都会让它红一次，逼接线的人回来改这里，而不是让字段继续当装饰
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MemoryScope {
    None,
    Shared,
    Project,
    Namespaced(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProfile {
    pub name: String,
    /// 写进那次 run 的第一句：你是谁、这一支只负责什么
    pub role: String,
    /// `None` = 跟着用户的当前配置走。指定模型时它是服务商认的那个字符串
    pub model: Option<String>,
    /// 专属服务商档案 id（`config.profiles` 里的 id）。`None` = 跟着当前连接走。
    /// 与 `model` 是两格：只点名模型 = 换模型不换服务商；点名档案 = 那张档案的
    /// 连接域（地址/凭据/参数/默认模型）整体生效，档案里没另点模型就用档案默认
    pub endpoint: Option<String>,
    /// 工具白名单。空数组 = 一个都不给（纯推理节点）
    pub tools: Vec<String>,
    /// 这一支要动的能力维度，与 `level` 配成一行权限表
    pub capability: Capability,
    pub level: Level,
    pub memory_scope: MemoryScope,
    pub budget: Budget,
}

/// 由工具白名单推导能力维度（自定义子助理专用，[`AgentProfile::custom`] 调它）。
/// 判据取名单里"最会动手"的那一件，档位一律取保守侧：要动手的档案一律 `Ask`——
/// 执行时照常弹审批，白名单与权限表才是真的闸门，这条映射只是把没写的格子填上
pub fn derive_capability(tools: &[String]) -> (Capability, Level) {
    if tools.iter().any(|tool| tool == "run_command") {
        (
            Capability::Exec {
                scope: ExecScope::Arbitrary,
            },
            Level::Ask,
        )
    } else if tools.iter().any(|tool| tool == "write_file") {
        (
            Capability::File {
                scope: PathScope::ProjectRoot,
                mode: FileMode::Write,
            },
            Level::Ask,
        )
    } else if !tools.is_empty() {
        (
            Capability::File {
                scope: PathScope::ProjectRoot,
                mode: FileMode::Read,
            },
            Level::Scoped,
        )
    } else {
        // 纯推理：不碰任何东西。Deny 是表里最严的一档，不是"没想好"
        (
            Capability::Exec {
                scope: ExecScope::Arbitrary,
            },
            Level::Deny,
        )
    }
}

impl AgentProfile {
    /// 只读侦察：读项目内文件，不给命令、不给写入
    pub fn reader(name: &str) -> Self {
        Self {
            name: name.to_string(),
            role: "只看不改：把事实读回来，不要动任何东西。".to_string(),
            model: None,
            endpoint: None,
            tools: vec!["read_file".into(), "list_files".into()],
            capability: Capability::File {
                scope: PathScope::ProjectRoot,
                mode: FileMode::Read,
            },
            level: Level::Scoped,
            memory_scope: MemoryScope::Project,
            budget: Budget::default(),
        }
    }

    /// 会动手的执行者：写文件要问，命令一律要问
    pub fn worker(name: &str) -> Self {
        Self {
            name: name.to_string(),
            role: "在指定范围内完成这一步，并把结论写成一句可核对的话。".to_string(),
            model: None,
            endpoint: None,
            tools: vec!["read_file".into(), "list_files".into(), "write_file".into()],
            capability: Capability::File {
                scope: PathScope::ProjectRoot,
                mode: FileMode::Write,
            },
            level: Level::Ask,
            memory_scope: MemoryScope::Namespaced(name.to_string()),
            budget: Budget::default(),
        }
    }

    /// 监督者：不碰磁盘，只做判断与派发
    pub fn supervisor(name: &str) -> Self {
        Self {
            name: name.to_string(),
            role: "你不亲自动手：读结论、决定下一步派给谁。".to_string(),
            model: None,
            endpoint: None,
            tools: Vec::new(),
            capability: Capability::Exec {
                scope: ExecScope::Arbitrary,
            },
            level: Level::Deny,
            memory_scope: MemoryScope::Shared,
            budget: Budget::default(),
        }
    }

    /// 自定义子助理（设置页「子助理」的定义）→ 档案。角色行、工具白名单、
    /// 模型与服务商四格由定义照搬，权限那两维由 [`derive_capability`] 从名单推导——
    /// 定义里没有、也不该有"权力"这一栏可填。
    /// 记忆作用域沿用 worker 的口径（每个名字一份私有命名空间）；
    /// 今天节点那一发根本不读记忆，这一格仍是无执行者的占位（见 `MemoryScope` 文档）
    pub fn custom(
        name: &str,
        role: &str,
        model: Option<String>,
        endpoint: Option<String>,
        tools: &[String],
    ) -> Self {
        let (capability, level) = derive_capability(tools);
        Self {
            name: name.to_string(),
            role: role.to_string(),
            model,
            endpoint,
            tools: tools.to_vec(),
            capability,
            level,
            memory_scope: MemoryScope::Namespaced(name.to_string()),
            budget: Budget::default(),
        }
    }

    /// 节点话题 id：一个节点一次尝试一个 id，所以
    /// "重跑这个节点"留下的是另一段独立上下文，而不是把上一段改写掉
    pub fn conversation_id(plan_id: &str, node: &str, attempt: u8) -> String {
        format!("plan-{plan_id}-{node}-a{attempt}")
    }

    /// 把这份档案并进全局权限表。
    ///
    /// **只能收紧**：档案写着 `Allow` 而全局档是 `ask`，结果仍然是问。
    /// 这条不是保守，是产品底线——一个 sub-agent 的档案不该成为绕过用户弹窗的后门，
    /// 否则"每任务独立权限"就变成了"每任务独立降权限"
    pub fn policy_under(&self, global: &Policy) -> Policy {
        let mut overrides = global.overrides.clone();
        overrides.push((self.capability.key(), self.level));
        // 工具白名单也进表：`tool.<name>` 的那一行由这份档案声明
        for tool in &self.tools {
            overrides.push((format!("tool.{tool}"), Level::Scoped));
        }
        Policy {
            mode: global.mode,
            overrides,
            phase: global.phase,
            delete_batch_ask: global.delete_batch_ask,
            file_rules: global.file_rules.clone(),
            command_blocklist: global.command_blocklist.clone(),
            command_rules: global.command_rules.clone(),
            network_rules: global.network_rules.clone(),
            net_http_remote: global.net_http_remote,
            net_http_local: global.net_http_local,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Mode;

    #[test]
    fn a_profile_can_tighten_the_table_but_never_loosen_it() {
        // 验收"权限隔离"的关键一条：档案不是放松通道
        let ask = Policy::new(Mode::Ask);
        let loose = AgentProfile {
            name: "pusher".into(),
            role: String::new(),
            model: None,
            endpoint: None,
            tools: vec!["run_command".into()],
            capability: Capability::Exec {
                scope: ExecScope::Git,
            },
            level: Level::Allow,
            memory_scope: MemoryScope::Shared,
            budget: Budget::default(),
        };
        let merged = loose.policy_under(&ask);
        assert_eq!(
            merged.resolve(&Capability::Exec {
                scope: ExecScope::Git
            }),
            Level::Ask,
            "ask 档下的命令执行不能因为一份档案就不问了：{merged:?}"
        );
        // 反过来，档案主动收紧是生效的：全局 full，节点自己说不许执行
        let full = Policy::new(Mode::Full);
        let strict = AgentProfile::supervisor("boss");
        let merged = strict.policy_under(&full);
        assert_eq!(
            merged.resolve(&strict.capability),
            Level::Deny,
            "full 档也不能越过档案里那一行 Deny：那等于让开关吃掉红线"
        );
    }

    #[test]
    fn a_reader_profile_cannot_touch_the_disk() {
        let reader = AgentProfile::reader("scout");
        // 白名单的判据只有闸门那一条（`tool_runtime::allowlist_violation`）。
        // 档案这边以前另有一个 `is_tool_allowed`，测试问的是它、运行时走的是另一条——
        // 同一个问题两个问法就是双轨真相的开端，所以这里直接问真的那个
        let may = |name: &str| {
            crate::tool_runtime::allowlist_violation(name, Some(&reader.tools)).is_none()
        };
        assert!(may("read_file"));
        assert!(
            !may("write_file"),
            "只读侦察的档案里出现 write_file，那份档案就不是只读的了"
        );
        assert!(!may("run_command"));
        // 白名单是真的闸门：走 tool_runtime 的同一份作用域机制，不是提示词里的一句希望
        let args = serde_json::json!({ "path": "a.rs", "content": "x" });
        let root = std::env::temp_dir();
        let call = crate::tool_runtime::Call::new("write_file", &args, Some(&root), false);
        let ruling =
            crate::tool_runtime::rule(&Policy::new(Mode::Full), &call, "写入", Some(&reader.tools));
        assert!(
            matches!(ruling.decision, crate::policy::Decision::Deny { .. }),
            "全满档下，只读档案也不能写文件：{ruling:?}"
        );
    }

    #[test]
    fn each_attempt_gets_its_own_conversation_so_a_rerun_is_not_a_rewrite() {
        let first = AgentProfile::conversation_id("p1", "worker-a", 1);
        let again = AgentProfile::conversation_id("p1", "worker-a", 2);
        assert_ne!(
            first, again,
            "重跑要留下另一段上下文，否则账本上第 1 次尝试发生过什么就查不到了"
        );
        assert!(
            first.starts_with("plan-p1-"),
            "话题 id 里要认得出是哪份 plan 的：{first}"
        );
    }

    #[test]
    fn memory_scopes_are_distinct_values_not_a_single_boolean() {
        // "关掉记忆"与"只读这份 plan 的记忆"是两件事，压成一个 bool 就没法表达后者
        assert_ne!(MemoryScope::None, MemoryScope::Shared);
        assert_ne!(
            MemoryScope::Namespaced("worker-a".into()),
            MemoryScope::Namespaced("worker-b".into()),
            "两个节点的私有作用域不能撞成同一个"
        );
    }

    /// 自定义档案的能力维度由白名单推导。这张映射本身就是一条安全决定：
    /// 多认一件工具就是多放一份能力，所以每一档都要钉死，改它必须过人眼
    #[test]
    fn the_derived_capability_follows_the_most_capable_tool_in_the_list() {
        let list = |items: &[&str]| -> (Capability, Level) {
            derive_capability(
                &items
                    .iter()
                    .map(|item| item.to_string())
                    .collect::<Vec<_>>(),
            )
        };
        // 命令是最会动手的一件：有它在，别的都白搭，档位是 Ask
        let (exec, level) = list(&["read_file", "run_command"]);
        assert!(matches!(exec, Capability::Exec { .. }));
        assert_eq!(
            level,
            Level::Ask,
            "要跑命令的档案一律问，不因为名单里还有只读工具而变松"
        );
        // 有写没有命令：写档 + Ask
        let (write, level) = list(&["read_file", "write_file"]);
        assert!(matches!(
            write,
            Capability::File {
                mode: FileMode::Write,
                ..
            }
        ));
        assert_eq!(level, Level::Ask);
        // 只有读：读档 + Scoped
        let (read, level) = list(&["read_file", "list_files"]);
        assert!(matches!(
            read,
            Capability::File {
                mode: FileMode::Read,
                ..
            }
        ));
        assert_eq!(level, Level::Scoped);
        // 空名单 = 纯推理：Deny 兜底，不是"没想好"
        let (none, level) = list(&[]);
        assert!(matches!(none, Capability::Exec { .. }));
        assert_eq!(level, Level::Deny);
    }

    /// 定义照搬三格、推导两格：角色行与模型/服务商是用户写的，权力那两栏不是
    #[test]
    fn a_custom_profile_carries_the_definition_and_derives_the_power() {
        let custom = AgentProfile::custom(
            "审查员",
            "对照要求检查结论，不动任何东西。",
            Some("deepseek-chat".into()),
            Some("prof-relay".into()),
            &["read_file".into(), "list_files".into()],
        );
        assert_eq!(custom.name, "审查员");
        assert_eq!(custom.role, "对照要求检查结论，不动任何东西。");
        assert_eq!(custom.model.as_deref(), Some("deepseek-chat"));
        assert_eq!(custom.endpoint.as_deref(), Some("prof-relay"));
        assert_eq!(
            custom.tools,
            vec!["read_file".to_string(), "list_files".to_string()]
        );
        assert!(matches!(
            custom.capability,
            Capability::File {
                mode: FileMode::Read,
                ..
            }
        ));
        assert_eq!(custom.level, Level::Scoped);
        // 白名单仍然是真的闸门：推导出的能力面不越过名单
        assert!(
            crate::tool_runtime::allowlist_violation("write_file", Some(&custom.tools)).is_some()
        );
    }

    /// 这一条是**反向的钉**：它断言的是"这一维今天还没有执行者"，并且任何一半被接起来时都要红。
    /// 一个字段最坏的长相不是没接，而是**接了一半还写在要求里**——所以两处都盯着：
    /// ① 记忆层一旦长出 `Namespaced`，两个同名词表就变成真词表，必须去接判据；
    /// ② 节点那条路一旦开始注入记忆，`memory_scope` 就必须在注入处被读，不能继续当装饰
    #[test]
    fn the_profiles_memory_scope_still_has_no_executor_and_says_so() {
        let record = include_str!("../memory/record.rs");
        assert!(
            !record.contains(concat!("Name", "spaced")),
            "记忆层已经有 Namespaced 了：profile.memory_scope 不再可能是装饰，去接执行者并改 §5.26"
        );

        let chat = include_str!("../chat.rs");
        let shared = chat
            .split("pub fn run_turn_into(")
            .nth(1)
            .expect("节点走的那条共享路")
            .split("\nfn ")
            .next()
            .unwrap_or_default();
        assert!(
            !shared.contains("inject_for_turn"),
            "节点那一发开始读记忆了：那 memory_scope 必须在这条路上被读到，否则每个节点都在读全局记忆"
        );
        // 而用户那条路确实读——这条测试不许我把它当成"两边都没接"而误删
        let user_side = chat
            .split("fn run_turn(")
            .nth(1)
            .expect("用户那条路")
            .split("\nfn ")
            .next()
            .unwrap_or_default();
        assert!(
            user_side.contains("inject_for_turn"),
            "用户那条路的记忆注入不见了：那上面那条断言就失去意义了"
        );
    }
}
