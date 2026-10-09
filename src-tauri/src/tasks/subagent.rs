//! 子助理：一次带**窄白名单**和**小预算**的 run，认一个父亲。
//!
//! 它不是第二个执行引擎——走的就是 `chat::run_background_turn`，所以审批闸、上下文压缩、
//! 话题日志、用量归因对这些子 run 天然可见。"子助理绕过审批"那种写法在这里根本没有入口。
//!
//! 三件事由这个模块把关，每一件都对应一个真实的坏法：
//! 1. 工具名单必须是父集的子集（提权的门就在这一条）；
//! 2. 链深不许超过 [`MAX_DEPTH`]（子助理自我扩散）；
//! 3. 花超了预算就不再开一发。深度与花费都从账本读，所以进程重启之后答案一样。

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::chat;
use crate::config::ScheduledTask;

use super::escalate;
use super::graph::Node;
use super::runs::{self, Line, RunStatus, StartedBy};
use super::{handoff_of, node_brief, open_session, upstream_answers, verdict, Driving};

/// 子助理链的天花板（含父自己那一层）。设计里那条"默认 2 层"就是它
pub const MAX_DEPTH: usize = 2;

/// 这一发子 run 的规格：**只带能力面，不管"做什么"**。
/// 做什么住在 `Node::prompt`——两处都能写问句的话，就没人答得出子助理到底照哪一份干活。
/// 这里也没有 `capabilities` / `level`：它们还没有认它们的执行点，声明了就是写了没人读
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct Spec {
    /// 子助理能用的工具。空名单等于"这一发什么都干不了"，那是配错了而不是意图
    pub tools: Vec<String>,
    /// 轮数天花板。`0` = 不设这一条，沿用设置里那个全局上限（与 `budget_usd_e8` 同口径）；
    /// 非 0 只收窄，不放宽
    pub max_rounds: u32,
    /// 预算，与用量台账同样的 1e-8 整数口径。0 = 不设这条
    pub budget_usd_e8: i64,
}

/// 收窄名单。三种拒绝各有各的理由，因为它们坏的是不同的事
pub fn narrowed(parent: &[String], asked: &[String]) -> Result<Vec<String>, String> {
    if asked.is_empty() {
        return Err("子助理没带工具名单：那一发开起来也干不了事。".into());
    }
    if parent.is_empty() {
        // 父亲那一格没收窄 = 它有全集，子集判据只能落在"非空"上
        return Ok(asked.to_vec());
    }
    let mut out = Vec::with_capacity(asked.len());
    for tool in asked {
        if !parent.iter().any(|held| held == tool) {
            return Err(format!(
                "子助理要了「{tool}」，可它父亲手上没有这一项：子助理不能比父亲大。"
            ));
        }
        out.push(tool.clone());
    }
    Ok(out)
}

/// 这一发还能不能开。先深度后预算：深度是结构问题，预算是钱的问题
pub fn may_spawn(depth: usize, spent_usd_e8: i64, spec: &Spec) -> Result<(), String> {
    if depth >= MAX_DEPTH {
        return Err(format!(
            "子助理链已经有 {depth} 层，到顶了（上限 {MAX_DEPTH}）：再开一发就是自我扩散。"
        ));
    }
    if spec.budget_usd_e8 > 0 && spent_usd_e8 > spec.budget_usd_e8 {
        return Err(format!(
            "这一发已经花掉 {:.4}，超出子助理预算 {:.4}，不再开。",
            spent_usd_e8 as f64 / 1e8,
            spec.budget_usd_e8 as f64 / 1e8
        ));
    }
    Ok(())
}

/// 开一发子 run：这一格的 prompt 交给一个能力面更小的代理去执行。
/// 它返回的是**给父亲那一格写检查点用的**东西：状态、失败原因、
/// 以及子 run 自己的话题 id（成本归因与"去看它说了什么"都靠这个 id）
pub fn run(
    app: &AppHandle,
    task: &ScheduledTask,
    parent: &Line,
    node: &Node,
    spec: &Spec,
    started_by: StartedBy,
) -> Driving {
    let root = match runs::data_root(app) {
        Ok(root) => root,
        Err(error) => {
            return Driving {
                status: RunStatus::Failed,
                error: Some(error),
                conversation_id: String::new(),
            }
        }
    };
    let spent = runs::total_cost(&root, &parent.run_id)
        .map(|cost| cost.cost_usd_e8)
        .unwrap_or(0);
    // 两道闸一次过：名单是"会不会提权"的问题，深度与预算是"还能不能再开一发"的问题。
    // 拒了就返回原因，这一格记成失败——不开那一发，也就没有钱与文件被动过
    let tools = match narrowed(node.allowed_tools.as_slice(), &spec.tools).and_then(|tools| {
        may_spawn(runs::chain_len(&root, &parent.run_id), spent, spec).map(|()| tools)
    }) {
        Ok(tools) => tools,
        Err(reason) => {
            return Driving {
                status: RunStatus::Failed,
                error: Some(reason),
                conversation_id: String::new(),
            }
        }
    };

    let conversation_id = format!("conv-{}", runs::token());
    if let Err(error) = open_session(
        app,
        &conversation_id,
        &format!("{} · 第 {} 格 · 子助理", task.name, node.id),
    ) {
        return Driving {
            status: RunStatus::Failed,
            error: Some(error),
            conversation_id,
        };
    }
    let Ok(child) = runs::begin_child(app, &task.id, &conversation_id, started_by, &parent.run_id)
    else {
        return Driving {
            status: RunStatus::Failed,
            error: Some("子助理的起跑行没记上，这一发没起跑。".into()),
            conversation_id,
        };
    };

    // 与父同一套无人值守规则：这一发里要点头的动作照样挂成待审批，不超时放行
    let _unattended = escalate::watch_run(&conversation_id, &child.run_id, &task.id, started_by);
    let outcome = chat::run_background_turn(
        app,
        &conversation_id,
        // 交出去的那一格也要吃到它前置的话：判据与父那一发同一条（账本 + 话题日志现读）
        &node_brief(
            task,
            node,
            &handoff_of(node, &upstream_answers(app, &parent.run_id, node)),
        ),
        Some(tools.as_slice()),
        (spec.max_rounds > 0).then_some(spec.max_rounds),
        // 子助理不另挑模型/服务商：`Spec` 里那两格没决定过用谁，替它挑就是凭空多一次花钱的变量
        None,
        None,
    );
    drop(_unattended);

    let cost = runs::cost_for(app, &conversation_id);
    let parked = escalate::parked_in(&root, &conversation_id);
    let (status, error) = verdict(outcome, parked.as_ref());
    if let Err(ledger) = runs::finish(&root, &child, status, error.clone(), cost) {
        eprintln!("子助理跑完了却没收尾，它的花费不会归到父下发：{ledger}");
    }
    Driving {
        status,
        error,
        conversation_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(tools: &[&str]) -> Spec {
        Spec {
            tools: tools.iter().map(|item| item.to_string()).collect(),
            max_rounds: 2,
            budget_usd_e8: 0,
        }
    }

    /// 判据："子助理越权调工具被拒"。要拒得说出是哪一项
    #[test]
    fn a_subagent_cannot_ask_for_a_tool_its_parent_does_not_have() {
        let parent = vec!["read_file".to_string(), "list_files".to_string()];
        assert_eq!(
            narrowed(&parent, &["read_file".to_string()]).unwrap(),
            vec!["read_file".to_string()]
        );
        let error = narrowed(
            &parent,
            &["read_file".to_string(), "run_command".to_string()],
        )
        .expect_err("跑命令不在父名单里");
        assert!(error.contains("run_command"), "要点出是哪一项越权：{error}");
        assert!(error.contains("不能比父亲大"), "要说清为什么拒：{error}");
    }

    #[test]
    fn an_empty_tool_list_is_refused_rather_than_run_as_a_no_op() {
        let parent = vec!["read_file".to_string()];
        assert!(
            narrowed(&parent, &[]).is_err(),
            "空名单不是「只读」，是配错了"
        );
        // 父亲那格没收窄 = 它有全集，此时只能要求子助理至少得能干活
        assert!(narrowed(&[], &["write_file".to_string()]).is_ok());
    }

    #[test]
    fn the_chain_stops_at_two_layers_and_says_so() {
        let fresh = may_spawn(1, 0, &spec(&["read_file"]));
        assert!(fresh.is_ok(), "父在第一层，子助理还能开：{fresh:?}");
        let deep = may_spawn(MAX_DEPTH, 0, &spec(&["read_file"])).expect_err("到顶了");
        assert!(deep.contains("自我扩散"), "要说出这条闸在防什么：{deep}");
    }

    /// 预算那条线只在设了的时候起作用：`0` 是"这条不判"，不是"一分钱都不许花"
    #[test]
    fn a_spent_budget_blocks_the_next_spawn_only_when_one_was_set() {
        let mut bounded = spec(&["read_file"]);
        bounded.budget_usd_e8 = 100;
        assert!(may_spawn(1, 99, &bounded).is_ok());
        let over = may_spawn(1, 101, &bounded).expect_err("超预算了");
        assert!(over.contains("预算"), "{over}");
        assert!(
            may_spawn(1, 101, &spec(&["read_file"])).is_ok(),
            "没设预算就不该拦"
        );
    }

    /// 判据："子助理谱系在账本上看得境"的另一半——规格本身是人在 config.json 里手写的。
    /// 键名一旦对不上，`default` 会静默兜住，那一发就悄悄变成"没轮数、没预算"的另一件事
    #[test]
    fn a_hand_written_node_keeps_its_subagent_numbers() {
        let raw = r#"{
            "id": "抓取",
            "prompt": "只做这一件",
            "dependsOn": [],
            "allowedTools": ["read_file", "list_files"],
            "subagent": {
                "tools": ["read_file"],
                "maxRounds": 2,
                "budgetUsdE8": 150
            }
        }"#;
        let node: Node = serde_json::from_str(raw).expect("配置里手写的那一格该读得回来");
        let spec = node
            .subagent
            .expect("subagent 那一格该读出来，不是当成没写");
        assert_eq!(spec.tools, vec!["read_file".to_string()]);
        assert_eq!(spec.max_rounds, 2, "轮数天花板读丢了：{spec:?}");
        assert_eq!(
            spec.budget_usd_e8, 150,
            "预算读丢了就等于不设预算：{spec:?}"
        );
        // 没写 subagent 的老图仍然是"这一格自己开一发"
        let plain: Node = serde_json::from_str(r#"{"id":"a","prompt":"p"}"#).expect("老形状");
        assert_eq!(plain.subagent, None);
    }
}
