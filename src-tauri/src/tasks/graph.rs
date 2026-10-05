//! 任务图：节点 + 依赖边 + 失败策略。**这里只有纯函数。**
//!
//! 图不存"跑到哪儿了"。节点状态一律从 `runs` 账本推导（`runs::checkpoints`）：
//! 一旦图自己也记一份进度，就没人答得出"这两份不一致时信谁"——那正是这一整套
//! 设计从头到尾在拆的那类双轨真相。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// 图里的一格：一段要做的事，加上它等谁的产出。
/// 没有 `capabilities` / `level` 这两格，是因为这一刻还没有执行点认它们——
/// 先声明再接线就是"写了没人读"。轮数与工具名单不一样，它们各自已有能强制它们的口子，
/// 所以那两个数住在 [`Node::subagent`] 的规格里，而不是在格子上再摆一份没人读的
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct Node {
    pub id: String,
    pub prompt: String,
    /// 前置节点的 id。空 = 起跑就能跑
    pub depends_on: Vec<String>,
    /// 空 = 沿用任务自己的工具白名单；非空 = 这一格只允许列出的那几个工具。
    /// 它交给 `chat::run_background_turn` 的 `allowed_tools`，是收紧不是放宽
    pub allowed_tools: Vec<String>,
    /// 这一格交不交给一个更小的代理去跑。`None` = 这一格自己开一发
    pub subagent: Option<crate::tasks::subagent::Spec>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnFailure {
    /// 停下（默认）：后面的节点多半在等这份产出，硬跑下去只是多烧钱、多改文件
    #[default]
    BlockRun,
    /// 只放弃这条分支：别的分支照跑，失败那格的下游永远等不到前置所以永远不会被挑中
    SkipBranch,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct TaskGraph {
    pub nodes: Vec<Node>,
    pub on_failure: OnFailure,
}

impl TaskGraph {
    /// 没图 = 这条任务还是"一句 prompt 跑一发"的老形状，执行路径一个字都不变
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 这一份图**发得出去吗**：形状由 `order()` 判（空 id、重复 id、悬空与自依赖、环），
    /// 这里补上它看不见的那一维——每一格得真有一句要说的话。
    /// 空 prompt 的一格不是"这一格跳过"：它会把一发请求问出去、花一笔钱、产出一个没人要的结论
    pub fn validate(&self) -> Result<(), String> {
        self.order()?;
        for node in &self.nodes {
            if node.prompt.trim().is_empty() {
                return Err(format!("节点「{}」没有要发出去的内容", node.id));
            }
        }
        Ok(())
    }

    /// id → 声明时的下标。重复 id 与悬空依赖都是定义写坏了：宁可现在报错，
    /// 也不要跑一半才发现"这一格到底指谁"
    fn positions(&self) -> Result<BTreeMap<&str, usize>, String> {
        let mut out: BTreeMap<&str, usize> = BTreeMap::new();
        for (index, node) in self.nodes.iter().enumerate() {
            if node.id.trim().is_empty() {
                return Err(format!("第 {} 个节点没有 id", index + 1));
            }
            if out.insert(node.id.as_str(), index).is_some() {
                return Err(format!("节点 id「{}」重复了", node.id));
            }
        }
        for node in &self.nodes {
            for held in &node.depends_on {
                if !out.contains_key(held.as_str()) {
                    return Err(format!("节点「{}」依赖了一个不存在的「{}」", node.id, held));
                }
                if held == &node.id {
                    return Err(format!("节点「{}」依赖它自己", node.id));
                }
            }
        }
        Ok(out)
    }

    /// 确定的拓扑序：每一轮取**声明序里第一个前置齐了的**，所以同一层的先后由声明决定，
    /// 而"乱序写"与"正序写"给出的是同一份计划。有环就报错并点出卡住的那几格——
    /// 只说"有环"等于让人自己去找
    pub fn order(&self) -> Result<Vec<String>, String> {
        self.positions()?;
        let mut out: Vec<String> = Vec::with_capacity(self.nodes.len());
        while out.len() < self.nodes.len() {
            let Some(node) = self.nodes.iter().find(|node| {
                !out.iter().any(|done| done == &node.id)
                    && node.depends_on.iter().all(|held| out.iter().any(|done| done == held))
            }) else {
                let stuck: Vec<&str> = self
                    .nodes
                    .iter()
                    .map(|item| item.id.as_str())
                    .filter(|id| !out.iter().any(|done| done == id))
                    .collect();
                return Err(format!(
                    "任务图里有环，这些节点排在任何次序里都不下去：{}",
                    stuck.join("、")
                ));
            };
            out.push(node.id.clone());
        }
        Ok(out)
    }

    /// 现在能起跑的那些：所有前置都已经 `done`，而它自己还没跑过。按拓扑序给
    pub fn ready(&self, done: &BTreeSet<String>) -> Result<Vec<&Node>, String> {
        let order = self.order()?;
        Ok(order
            .iter()
            .filter(|id| !done.contains(*id))
            .filter(|id| {
                self.nodes
                    .iter()
                    .find(|node| node.id == **id)
                    .map(|node| node.depends_on.iter().all(|held| done.contains(held)))
                    .unwrap_or(false)
            })
            .filter_map(|id| self.nodes.iter().find(|node| node.id == *id))
            .collect())
    }

    /// 跑到没得跑之后仍留在场外的节点。`BlockRun` 停下时、以及收尾要说"哪几格被挡住了"时读它
    pub fn stranded(&self, done: &BTreeSet<String>, failed: &BTreeSet<String>) -> Result<Vec<String>, String> {
        Ok(self
            .order()?
            .into_iter()
            .filter(|id| !done.contains(id) && !failed.contains(id))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这三份形状与前端那三个 interface 逐字对齐。`deny_unknown_fields` 只保证
    /// "生键进不来"，保证不了"两侧的键名一模一样"——那要靠拿 TS 原文当断言对象
    #[test]
    fn the_graph_shapes_are_named_the_same_on_both_sides() {
        let spec = crate::tasks::subagent::Spec {
            tools: vec!["read_file".into()],
            max_rounds: 3,
            budget_usd_e8: 1000,
        };
        let node = Node {
            id: "a".into(),
            prompt: "做点什么".into(),
            depends_on: vec!["b".into()],
            allowed_tools: vec!["read_file".into()],
            subagent: Some(spec.clone()),
        };
        let graph = TaskGraph {
            nodes: vec![node.clone()],
            on_failure: OnFailure::SkipBranch,
        };
        // 三份各查一次：harness 只对得上它拿到的那一个对象的顶层键
        crate::test_support::assert_matches_ts(&serde_json::to_value(&graph).unwrap(), "TaskGraph");
        crate::test_support::assert_matches_ts(&serde_json::to_value(&node).unwrap(), "TaskNode");
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&spec).unwrap(),
            "TaskSubagent",
        );
        // `subagent: None` 那一格也要带键：省略它，界面读到的就是 undefined 而不是 null
        let bare = serde_json::to_value(Node {
            subagent: None,
            ..node.clone()
        })
        .unwrap();
        crate::test_support::assert_matches_ts(&bare, "TaskNode");
    }

    /// 图那三个入站形状不认生键。`default` 单独用会把打错的键读成"这一格没带它"：
    /// `depends_on` 打错 = 这一格不等任何人（起跑就并发），`allowed_tools` 打错 = 这一格不限工具
    #[test]
    fn a_graph_keyed_with_a_stray_spelling_is_refused_not_read_as_a_missing_field() {
        // 正对照：界面写的那套键、账本存的那套键，照旧收
        let accepted: TaskGraph = serde_json::from_str(
            r#"{"nodes":[{"id":"a","prompt":"做点什么","dependsOn":["b"],"allowedTools":["read_file"],"subagent":{"tools":["read_file"],"maxRounds":3,"budgetUsdE8":1000}}],"onFailure":"skip_branch"}"#,
        )
        .expect("编辑器写出的那一份必须读得回来");
        assert_eq!(accepted.nodes[0].depends_on, vec!["b".to_string()]);
        assert_eq!(accepted.nodes[0].allowed_tools, vec!["read_file".to_string()]);
        assert_eq!(
            accepted.nodes[0].subagent.clone().map(|spec| spec.max_rounds),
            Some(3)
        );

        // 反面对照：同一个意思换成 snake_case 的键名，就得当场拒掉而不是"读成没带"
        for stray in [
            r#"{"nodes":[{"id":"a","prompt":"p","depends_on":["b"]}],"onFailure":"block_run"}"#,
            r#"{"nodes":[{"id":"a","prompt":"p","allowed_tools":[]}],"onFailure":"block_run"}"#,
            r#"{"nodes":[{"id":"a","prompt":"p","subagent":{"tools":[],"maxRounds":3,"budget_usd_e8":0}}],"onFailure":"block_run"}"#,
            r#"{"nodes":[],"on_failure":"skip_branch"}"#,
        ] {
            let error = serde_json::from_str::<TaskGraph>(stray)
                .err()
                .unwrap_or_else(|| panic!("生键要被拒掉，这一份却收了：{stray}"));
            assert!(
                error.to_string().contains("unknown field"),
                "拒的理由要说得出是哪一格生键：{error}"
            );
        }

        // 自己写出去的那一份也要读得回来（配置与账本里存的就是它）
        let written = serde_json::to_string(&accepted).expect("图写得出 JSON");
        let round_tripped: TaskGraph =
            serde_json::from_str(&written).expect("自己写的图必须读得回来");
        assert_eq!(round_tripped, accepted);
    }

    fn node(id: &str, depends_on: &[&str]) -> Node {
        Node {
            id: id.into(),
            prompt: format!("做 {id}"),
            depends_on: depends_on.iter().map(|item| item.to_string()).collect(),
            allowed_tools: Vec::new(),
            subagent: None,
        }
    }

    fn graph(nodes: Vec<Node>) -> TaskGraph {
        TaskGraph { nodes, on_failure: OnFailure::default() }
    }

    /// 判据："乱序节点定义能排出确定顺序"。这里"确定"指的是两件事：
    /// 任何一格都不会排到它的前置之前，且同一份声明跑两遍给出同一份计划。
    /// 同层之间的先后按声明次序——那是写任务的人唯一能直接看懂的顺序
    #[test]
    fn a_shuffled_definition_still_places_every_node_after_its_dependencies() {
        let shuffled = graph(vec![
            node("报告", &["统计", "校对"]),
            node("校对", &["抓取"]),
            node("统计", &["抓取"]),
            node("抓取", &[]),
        ]);
        let plan = shuffled.order().unwrap();
        assert_eq!(
            plan,
            vec!["抓取", "校对", "统计", "报告"],
            "校对声明在统计前面，同层就该它先跑：{plan:?}"
        );
        for item in &shuffled.nodes {
            let at = plan.iter().position(|id| id == &item.id).expect("每格都该在计划里");
            for held in &item.depends_on {
                assert!(
                    plan[..at].iter().any(|done| done == held),
                    "「{}」排到了它的前置「{}」前面",
                    item.id,
                    held
                );
            }
        }
        assert_eq!(shuffled.order(), shuffled.order(), "同一份定义不能跑两次给两份计划");
    }

    #[test]
    fn a_linear_declaration_orders_itself_unchanged() {
        let straight = graph(vec![
            node("抓取", &[]),
            node("统计", &["抓取"]),
            node("校对", &["抓取"]),
            node("报告", &["统计", "校对"]),
        ]);
        assert_eq!(
            straight.order().unwrap(),
            vec!["抓取", "统计", "校对", "报告"],
            "正着写的图不该被重排"
        );
    }

    #[test]
    fn a_cycle_is_refused_and_names_the_nodes_that_cannot_be_placed() {
        let looped = graph(vec![node("a", &["b"]), node("b", &["a"])]);
        let error = looped.order().expect_err("有环必须被拒，不能默默漏掉那两格");
        assert!(error.contains("环"), "要说清是环：{error}");
        assert!(error.contains("a") && error.contains("b"), "要点出卡住的是哪些节点：{error}");

        // 自环也在这里拦下：它同样是"排在任何次序里都不下去"
        let selfish = graph(vec![node("a", &["a"])]);
        assert!(selfish.order().is_err(), "依赖自己的节点跑不起来");
    }

    #[test]
    fn a_broken_definition_fails_loudly_instead_of_guessing() {
        let dangling = graph(vec![node("a", &["不存在的格子"])]);
        let error = dangling.order().expect_err("悬空依赖不能当成没有依赖");
        assert!(error.contains("不存在"), "{error}");

        let duplicated = graph(vec![node("a", &[]), node("a", &[])]);
        assert!(duplicated.order().unwrap_err().contains("重复"));

        let nameless = graph(vec![node("  ", &[])]);
        assert!(nameless.order().unwrap_err().contains("没有 id"));
    }

    /// `SkipBranch` 的实现其实不写代码：失败那格的下游永远等不到前置，于是永远不进 ready。
    /// 这条测的就是那个"没有代码"的性质
    #[test]
    fn only_nodes_whose_dependencies_are_all_done_are_ready() {
        let plan = graph(vec![
            node("抓取", &[]),
            node("统计", &["抓取"]),
            node("校对", &["抓取"]),
            node("报告", &["统计", "校对"]),
        ]);
        let none: BTreeSet<String> = BTreeSet::new();
        assert_eq!(
            plan.ready(&none).unwrap().iter().map(|node| node.id.as_str()).collect::<Vec<_>>(),
            vec!["抓取"]
        );

        let done: BTreeSet<String> = ["抓取".to_string()].into_iter().collect();
        assert_eq!(
            plan.ready(&done).unwrap().iter().map(|node| node.id.as_str()).collect::<Vec<_>>(),
            vec!["统计", "校对"],
            "汇合前的两条分支都该就绪，且按声明次序"
        );

        let half: BTreeSet<String> = ["抓取", "统计"]
            .iter()
            .map(|item| item.to_string())
            .collect();
        assert_eq!(
            plan.ready(&half).unwrap().iter().map(|node| node.id.as_str()).collect::<Vec<_>>(),
            vec!["校对"],
            "只跑完一条分支时汇合点不能起跑"
        );
    }

    #[test]
    fn stranded_names_what_a_failure_left_behind() {
        let plan = graph(vec![node("一", &[]), node("二", &["一"]), node("三", &["二"])]);
        let done: BTreeSet<String> = ["一".to_string()].into_iter().collect();
        let failed: BTreeSet<String> = ["二".to_string()].into_iter().collect();
        assert_eq!(plan.stranded(&done, &failed).unwrap(), vec!["三"], "三永远等不到二");
        assert_eq!(plan.stranded(&done, &done).unwrap(), vec!["二", "三"]);
    }

    /// 没有内容的那一格既不会"跳过"也不会等到运行时才炸：它会把一发请求问出去、花一笔钱。
    /// 所以这条判据要站在写它的那一刻
    #[test]
    fn a_node_with_nothing_to_say_is_refused_before_it_spends_a_request() {
        let mut silent = node("写说明", &["汇总"]);
        silent.prompt = "   ".into();
        let error = graph(vec![node("汇总", &[]), silent])
            .validate()
            .expect_err("一句要说的话都没有，这一格不该算配好了");
        assert!(error.contains("写说明"), "要点出是哪一格：{error}");

        // 正向对照：每格都有一句真话就该过，别让这条判据退化成"什么都拒"
        assert!(graph(vec![node("汇总", &[]), node("写说明", &["汇总"])])
            .validate()
            .is_ok());
        // 空图是老的单发形状：它今天能存，加了这道判据之后还得能存
        assert!(
            TaskGraph::default().validate().is_ok(),
            "没写图的任务不能被这道判据捎带上"
        );
    }

    /// 形状那几条判据只能有一份：`validate` 问的就是 `order` 问的那一句，
    /// 再加上"有没有话要说"。把 `validate` 开头那行 `self.order()?` 删掉，
    /// 这四张坏图会全部带着"能跑"的结论存进 config
    #[test]
    fn validate_also_asks_everything_the_order_check_asks() {
        for broken in [
            graph(vec![node("a", &["b"]), node("b", &["a"])]),
            graph(vec![node("a", &[]), node("a", &[])]),
            graph(vec![node("a", &["不存在的格子"])]),
            graph(vec![node("   ", &[])]),
        ] {
            let error = broken.validate().expect_err("图形状坏了就不该存下去");
            assert_eq!(
                error,
                broken.order().unwrap_err(),
                "同一件坏事在两道判据里说了两种话，就说明其中一处是抄的"
            );
        }
    }
}
