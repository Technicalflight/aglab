//! 编排账本的完整性对账（O18-1）。
//!
//! 续跑的跳过判据只有 `(plan_id, node)`——"这一格跑过"从来不问"跑的是什么"。
//! 一旦两次起跑之间节点的定义被改过（目标/档案/依赖），续跑会拿新输入配旧产出，
//! 而账本看起来一切正常。这里的职责就一件事：**让"同一步换了输入却看起来一样"
//! 变成大声失败**，而不是一次安静的续跑。

use std::collections::HashMap;

use super::graph::{Node, Plan};
use super::orchestrator::TraceRow;

/// 一个节点的**输入定义**哈希：整份节点序列化后过 SHA-256。
/// 节点的输入就是它的定义——目标、档案、依赖、环、尝试上限，缺一样"这一格要干什么"
/// 就不完整。字段顺序由 struct 声明定死（serde_json 对 struct 按声明序输出），
/// 同一份二进制内稳定。它只用于**续跑对账**，不是内容寻址：
/// 升级后 Node 结构若长出新格，哈希会变、续跑会被拦——那是"大声失败"该拦的方向，
/// 升级后恢复的图本来就该人工过目
pub(super) fn node_input_hash(node: &Node) -> String {
    use sha2::{Digest, Sha256};
    let encoded = serde_json::to_string(node).unwrap_or_default();
    format!("{:x}", Sha256::digest(encoded.as_bytes()))
}

/// 续跑对账：账本里落过输入哈希的节点，必须与这次传入的图对得上。
/// 对账的对象是**账本套回之后的那张图**（`plan_from_ledger` 的产物）——replan 只追加
/// 新节点、不改已有节点的定义（`grow_plan` 的 `added.is_empty()` 早退保证），
/// 所以套回后的图与账本行的哈希天然对得上；对不上就是真的有人换了输入。
/// 账本行没有哈希（旧账本 / 旧行）→ 跳过对账：不能让存量计划在升级后全部恢复不了
pub(super) fn verify_ledger_inputs(prior: &[TraceRow], live: &Plan) -> Result<(), String> {
    let current: HashMap<&str, &Node> = live.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    // 节点可能被反复排队（重跑），取账本里最后一次出现的哈希
    let mut ledger: HashMap<&str, &str> = HashMap::new();
    for row in prior {
        if let (Some(hash), Some(_)) = (&row.input_hash, current.get(row.node.as_str())) {
            ledger.insert(row.node.as_str(), hash);
        }
    }
    let mut mismatches: Vec<&str> = ledger
        .iter()
        .filter(|(id, hash)| {
            current
                .get(*id)
                .is_some_and(|node| node_input_hash(node) != **hash)
        })
        .map(|(id, _)| *id)
        .collect();
    if mismatches.is_empty() {
        return Ok(());
    }
    mismatches.sort_unstable();
    Err(format!(
        "这份计划的账本与传入的图对不上：{} 的输入（目标/档案/依赖）在两次起跑之间被改过。\
         续跑会拿新输入配旧产出——重新起一份计划，或换一个新的 plan id。",
        mismatches.join("、")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestra::orchestrator::NodeStatus;

    fn node(id: &str, deps: &[&str]) -> Node {
        Node {
            id: id.into(),
            goal: format!("{id} 要做的事"),
            profile: "worker".into(),
            depends_on: deps.iter().map(|dep| dep.to_string()).collect(),
            edge: crate::orchestra::graph::Edge::FinishToStart,
            max_attempts: 1,
        }
    }

    fn row(node: &str, event: &str, input_hash: Option<String>) -> TraceRow {
        TraceRow {
            ts_ms: 1,
            plan_id: "p".into(),
            node: node.into(),
            attempt: 0,
            event: event.into(),
            status: Some(NodeStatus::Pending),
            tokens: None,
            duration_ms: None,
            cost_e8: None,
            conversation_id: None,
            detail: None,
            input_hash,
        }
    }

    /// 输入定义的哈希——定义没变哈希不变，动一格就变
    #[test]
    fn the_input_hash_follows_the_node_definition() {
        let base = node("a", &[]);
        assert_eq!(
            node_input_hash(&base),
            node_input_hash(&node("a", &[])),
            "同一定义两次计算要一致"
        );
        let different_goal = Node {
            goal: "换了目标".into(),
            ..node("a", &[])
        };
        let different_profile = Node {
            profile: "reviewer".into(),
            ..node("a", &[])
        };
        assert_ne!(
            node_input_hash(&base),
            node_input_hash(&different_goal),
            "目标变了要看得出来"
        );
        assert_ne!(
            node_input_hash(&base),
            node_input_hash(&different_profile),
            "档案变了要看得出来"
        );
        assert_ne!(
            node_input_hash(&base),
            node_input_hash(&node("a", &["b"])),
            "依赖变了要看得出来"
        );
    }

    /// 续跑对账。改了输入就大声失败并点名节点；没改的照旧放行；
    /// 旧账本（行里没有哈希）跳过对账——新钉子不许把存量的计划堵死
    #[test]
    fn a_continuation_with_a_changed_node_input_is_refused_loudly() {
        let plan = Plan::pipeline("p", "目标", &[("a", "worker"), ("b", "worker")]);
        let ledger_rows: Vec<TraceRow> = plan
            .nodes
            .iter()
            .map(|n| row(&n.id.clone(), "queued", Some(node_input_hash(n))))
            .collect();

        assert!(
            verify_ledger_inputs(&ledger_rows, &plan).is_ok(),
            "定义没变：续跑照旧放行"
        );

        let mut tampered = plan.clone();
        if let Some(a) = tampered.nodes.iter_mut().find(|n| n.id == "a") {
            a.goal = "偷偷换了目标".into();
        }
        let error =
            verify_ledger_inputs(&ledger_rows, &tampered).expect_err("换过输入的续跑必须被拦");
        assert!(error.contains('a'), "报错要点名节点：{error}");
        assert!(error.contains("重新起一份"), "要说清怎么办：{error}");

        let mut rewired = plan.clone();
        if let Some(b) = rewired.nodes.iter_mut().find(|n| n.id == "b") {
            b.depends_on.clear();
        }
        assert!(
            verify_ledger_inputs(&ledger_rows, &rewired).is_err(),
            "改依赖也是换输入"
        );

        let legacy: Vec<TraceRow> = plan
            .nodes
            .iter()
            .map(|n| row(&n.id.clone(), "queued", None))
            .collect();
        assert!(
            verify_ledger_inputs(&legacy, &plan).is_ok(),
            "旧账本（行里没有哈希）不该被新钉子堵死"
        );
    }
}
