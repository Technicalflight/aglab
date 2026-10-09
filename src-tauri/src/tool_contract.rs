//! 工具契约表：每个工具一份结构化声明（只读/破坏性/可并行/副作用范围/输出上限），
//! 调度器与权限都从这份表推导——过去 `classify` 里的散装 match 是第一份真相，
//! 契约表是第二份，两份迟早漂移。现在反过来：**契约是唯一的声明来源**，
//! `classify` 读它，调度器读它，新增工具先填契约再写实现。
//!
//! 口径（对齐外部设计的 zod 契约字段）：
//! - `read_only`：这一发不改变任何调用方可见的状态。它直接决定风险档
//!   （Safe）与能否并行；
//! - `destructive`：删除/覆盖不可恢复的东西。直接顶到 High；
//! - `concurrent_safe`：与同轮其他调用并行跑不会因为顺序产生不同结果。
//!   只读默认真；一切写文件/发命令/提问（要等人）默认假；
//! - `side_effect_scope`：副作用落点。权限表按它选 capability 键的语义档；
//! - `max_output_bytes`：单发输出的字节上限（0 = 用全局钳制）。

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffectScope {
    /// 不改任何调用方可见状态
    None,
    /// 只改本进程内存（台账、登记表、话题内缓存）
    Memory,
    /// 工作目录里的文件
    Filesystem,
    /// 出站网络
    Network,
    /// 本机 shell 子进程
    Shell,
    /// 窗口、输入、浏览器这类整机面
    System,
}

#[derive(Debug, Clone, Copy)]
pub struct Contract {
    pub read_only: bool,
    pub destructive: bool,
    pub concurrent_safe: bool,
    pub side_effect_scope: SideEffectScope,
    /// 0 = 不另设上限，走全局 tool_result_max_chars
    pub max_output_bytes: usize,
}

impl Contract {
    const READ: Contract = Contract {
        read_only: true,
        destructive: false,
        concurrent_safe: true,
        side_effect_scope: SideEffectScope::None,
        max_output_bytes: 0,
    };

    /// 只读但落点在别处（网络/整机面）：能并行，权限档按范围给
    const fn read_in(scope: SideEffectScope) -> Contract {
        Contract {
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: scope,
            max_output_bytes: 0,
        }
    }

    /// 动文件的写：串行栅栏（同轮写顺序就是语义）
    const FILE_WRITE: Contract = Contract {
        read_only: false,
        destructive: false,
        concurrent_safe: false,
        side_effect_scope: SideEffectScope::Filesystem,
        max_output_bytes: 0,
    };

    /// 提问/计划这类要"等人或改台账"的控制信号：绝不能并行
    const CONTROL: Contract = Contract {
        read_only: false,
        destructive: false,
        concurrent_safe: false,
        side_effect_scope: SideEffectScope::Memory,
        max_output_bytes: 0,
    };

    /// 命令面：read_only 由命令内容现判（bash 只读策略模块），这里给默认档
    const SHELL: Contract = Contract {
        read_only: false,
        destructive: false,
        concurrent_safe: false,
        side_effect_scope: SideEffectScope::Shell,
        max_output_bytes: 0,
    };
}

/// 全部已声明工具的契约。**新工具先在这里落一行**——表里没有的名字按
/// "不可并行、未知范围"的保守默认处理（见 [`contract_for`]），漏写的代价
/// 是多一道栅栏，不是少一道闸
pub fn contract_for(name: &str, args: &serde_json::Value) -> Contract {
    match name {
        // ---- 纯读：并行批次的主力 ----
        "read_file" | "list_files" | "search_text" | "load_skill" | "present_files" => {
            Contract::READ
        }
        "knowledge_search" | "obs_recall" => Contract::READ,
        "lsp_query" | "goal_report" => Contract::READ,
        "list_windows" | "inspect_window" => Contract::read_in(SideEffectScope::System),
        "web_search" | "web_fetch" => Contract::read_in(SideEffectScope::Network),

        // ---- 控制信号：要等人的绝不能并排跑 ----
        "ask_user" | "update_plan" => Contract::CONTROL,

        // ---- 文件写：顺序即语义 ----
        "write_file" | "edit_file" | "open_path" => Contract::FILE_WRITE,
        "delete_file" => Contract {
            destructive: true,
            ..Contract::FILE_WRITE
        },

        // ---- 命令面：只读与否看命令内容（bash 只读策略模块）----
        "run_command" | "ssh_run" | "node_repl" => {
            let command = args
                .get("command")
                .or_else(|| args.get("script"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if crate::command_policy::is_read_only(command) {
                Contract {
                    read_only: true,
                    concurrent_safe: true,
                    ..Contract::SHELL
                }
            } else {
                Contract::SHELL
            }
        }
        "run_program" => Contract::SHELL,
        // 读后台输出是纯读；停后台命令动的是进程树，按栅栏走
        "command_output" => Contract::READ,
        "command_stop" => Contract {
            side_effect_scope: SideEffectScope::Shell,
            ..Contract::CONTROL
        },

        // ---- 整机面与浏览器：范围大，一律栅栏 ----
        "computer_act" | "browser" => Contract {
            side_effect_scope: SideEffectScope::System,
            ..Contract::CONTROL
        },

        // ---- 子助理与登记表 ----
        // spawn 是"多一个执行体在跑"，与谁并排都可能改变时序语义；列清单是纯读
        "spawn_subagent" | "wait_agent" | "command_stop_alias" => Contract::CONTROL,
        "agent_control" => {
            let action = args
                .get("action")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if action == "list" {
                Contract::READ
            } else {
                Contract::CONTROL
            }
        }

        // ---- 计划任务（cron 族）：改配置 = 栅栏 ----
        "cron_create" | "cron_delete" | "task_run_now" => Contract::CONTROL,
        "cron_list" | "memory_search" | "search_history" => Contract::READ,

        // ---- 规划模式切换：落一条模式行，栅栏 ----
        "plan_mode" => Contract::CONTROL,

        // ---- 扩展（mcp__*）：别人的程序，一律按不可并行、网络范围 ----
        other if other.starts_with("mcp__") => Contract {
            side_effect_scope: SideEffectScope::Network,
            ..Contract::CONTROL
        },

        // 未登记的名字：最保守的默认。进不了并行批次，权限按 High 走
        _ => Contract {
            read_only: false,
            destructive: false,
            concurrent_safe: false,
            side_effect_scope: SideEffectScope::None,
            max_output_bytes: 0,
        },
    }
}

/// 契约 → 风险档（权限的入口）。`classify` 的特判（路径越界、命令内容、
/// 扩展一律 High）在它自己的 match 里先走；走不到的落这里。
/// 语义：只读 → Safe；破坏性 → High；动 shell/system → High；
/// 动文件 → Elevated；其余（纯内存/网络读）→ Safe
/// 单发输出的有效上限：契约的 max_output_bytes（0 = 不另设）与全局钳制取小者。
/// 串行主干与并行后账共用这一把尺——两条执行路各算各的，
/// "同一个工具两条路输出不一样长"就是第二份真相
pub fn effective_cap(contract_max_output_bytes: usize, global: usize) -> usize {
    if contract_max_output_bytes > 0 {
        contract_max_output_bytes.min(global)
    } else {
        global
    }
}

pub fn risk_of(contract: &Contract) -> crate::tools::Risk {
    use crate::tools::Risk;
    if contract.destructive {
        return Risk::High;
    }
    if contract.read_only {
        return Risk::Safe;
    }
    match contract.side_effect_scope {
        SideEffectScope::None | SideEffectScope::Memory | SideEffectScope::Network => Risk::Safe,
        SideEffectScope::Filesystem => Risk::Elevated,
        SideEffectScope::Shell | SideEffectScope::System => Risk::High,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn read_only_tools_are_parallel_safe_and_safe_risk() {
        for name in [
            "read_file",
            "search_text",
            "knowledge_search",
            "lsp_query",
            "web_search",
            "obs_recall",
        ] {
            let contract = contract_for(name, &json!({}));
            assert!(contract.read_only, "{name}");
            assert!(contract.concurrent_safe, "{name}");
            assert!(
                matches!(risk_of(&contract), crate::tools::Risk::Safe),
                "{name}"
            );
        }
    }

    #[test]
    fn mutating_tools_never_enter_parallel_batches() {
        for name in [
            "write_file",
            "edit_file",
            "delete_file",
            "ask_user",
            "update_plan",
            "spawn_subagent",
        ] {
            let contract = contract_for(name, &json!({}));
            assert!(!contract.concurrent_safe, "{name}");
            assert!(!contract.read_only, "{name}");
        }
        let delete = contract_for("delete_file", &json!({}));
        assert!(delete.destructive);
        assert!(matches!(risk_of(&delete), crate::tools::Risk::High));
    }

    #[test]
    fn shell_read_only_follows_the_command_content() {
        let status = contract_for("run_command", &json!({ "command": "git status" }));
        assert!(status.read_only && status.concurrent_safe);
        let push = contract_for("run_command", &json!({ "command": "git push" }));
        assert!(!push.read_only);
        assert!(matches!(risk_of(&push), crate::tools::Risk::High));
        // ssh 同一条路
        let ssh = contract_for("ssh_run", &json!({ "command": "cat /etc/hostname" }));
        assert!(ssh.read_only);
    }

    #[test]
    fn effective_cap_takes_the_tighter_of_contract_and_global() {
        let global = 8_000;
        assert_eq!(effective_cap(0, global), global, "0 = 不另设上限");
        assert_eq!(effective_cap(2_000, global), 2_000, "契约更紧取契约");
        assert_eq!(effective_cap(64_000, global), global, "全局更紧取全局");
    }

    #[test]
    fn extensions_and_unknown_names_take_the_conservative_default() {
        let mcp = contract_for("mcp__srv__do", &json!({}));
        assert!(!mcp.concurrent_safe && !mcp.read_only);
        let unknown = contract_for("totally_new_tool", &json!({}));
        assert!(!unknown.concurrent_safe);
        assert!(
            matches!(risk_of(&unknown), crate::tools::Risk::Safe),
            "未登记≠危险，只是不可并行"
        );
    }
}
