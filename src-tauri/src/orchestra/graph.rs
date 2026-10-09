//! DAG 任务模型。四种边（完成-开始 / 条件 / map-reduce / 循环）与七种并行模式都是这张图的
//! **构造器**，不是七套机制——这是它能被测试、也它能被账本重放的原因。
//!
//! 纯数据 + 纯函数：这里没有任何线程、没有任何 IO。调度看它，账本重放看它，
//! 前端的 DAG 视图也只看它派生出来的那一份状态。

use serde::{Deserialize, Serialize};

use crate::orchestra::judge::Check;
use crate::quota::Priority;

/// 依赖的成立条件。
///
/// `Conditional` 读的是黑板上的一个结论键：它让"要不要跑下游"成为一个**看得见的事实**，
/// 而不是藏在某个节点提示词里的一句"如果……就跳过"
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Edge {
    FinishToStart,
    Conditional {
        key: String,
        equals: String,
    },
    /// map-reduce 的那一格：展开在**派发之前**完成，集合是那一次请求里带进来的那批东西。
    /// 展开后的整张图写进账本，所以崩溃恢复时不必重算（集合可能已经变了，重算等于让
    /// "上次跑到哪"有两个答案）
    ///
    /// 这一格**不带键名**。以前它带过一个 `items_from: "items"`，而除了显示文案没有任何地方读它——
    /// 更要紧的是 `expand_map` 把这一格换成一批子格之后，图上**已经没有**这一格了，
    /// 于是那句读数永远不会出现在界面上，而它写着"黑板里有一格集合"，那格没人写过（§5.35）。
    /// 真要让集合住在黑板上（运行时展开），那是另一件事：要新的黑板事实、要新的成本天花板，
    /// 而不是给这个字段补一个读者
    MapReduce,
    /// 自循环：这个节点反复重跑，直到黑板上 `until_key` 变成 `until_value`，
    /// 或者到达 `max_iters`。**上限是必填的**——没有上限的循环是一场会自己长大的成本事故
    Loop {
        until_key: String,
        until_value: String,
        max_iters: u8,
    },
}

impl Edge {
    /// 这一格**凭什么被放行**，一句人话。它是从图现读的派生读数：
    /// 面板上选了之后要看得见，不然那个选择就是一个没有读数的旋钮。
    /// 条件与循环这里故意把**真实的键名**打出来（而不是反推成"等某一格"）：
    /// 图上可能出现任何一种键（老账本里读得回来的也算），显示成猜出来的样子就是第二份真相
    pub fn gate_text(&self) -> String {
        match self {
            Edge::FinishToStart => "跑完就放行".to_string(),
            Edge::Conditional { key, equals } => format!("等「{key}」＝{equals}"),
            Edge::MapReduce => "装配时按那次请求里的集合展开成一批子格".to_string(),
            Edge::Loop {
                until_key,
                until_value,
                max_iters,
            } => format!("反复跑直到「{until_key}」＝{until_value}，最多 {max_iters} 轮"),
        }
    }
}

/// 界面上**能选**的那几种边。判据不是"哪种写起来省事"，而是"哪一种有写者"：
/// `Conditional` 与 `Loop` 读的都是黑板上的派生键，而生产代码只往黑板上写两类东西——
/// 每一格的结论（键是它的 id）与它的校验结论（[`orchestrator::verdict_key`]）。
/// 自由填一个键名等于造一条永不成立的边，所以这一份里根本没有"键"那一格，只有"等哪一格的结论"
///
/// `MapReduce` 也不在这里：它改的不是"这一格等谁"，而是"这张图有几格"（展开成 N 个节点），
/// 那是 planner 装配时的活
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum EdgeKind {
    /// 上游跑完就放行（默认，也是今天所有形状的样子）
    FinishToStart,
    /// 等某一格的校验结论等于 `pass`。等于 false 就是"它没过我才跑"
    WaitForVerdict { node: String, pass: bool },
    /// 自己反复跑，直到**自己的**校验结论变成 pass，或跑满这个轮数
    IterateUntilPass { max_iters: u8 },
}

/// 一次循环最多转几圈。一轮至少一次请求，所以这个数是**这条命令**给的成本天花板：
/// 界面上能填的数字没有上限，而"跑到裁判满意为止"没有上限就是半夜烧光预算的那种形状
pub const MAX_ITERATIONS: u8 = 8;

/// 每一格的**校验结论**住在黑板上的哪一格。写它的一方（`run_node` 落定那一刻）与
/// 读它的一方（[`Plan::with_edge_kind`] 造出来的那两种边）共用这个名字。
/// 键的名字只住在一个函数里，是 `#iters` 那一次"两个人各数各的"留下的形状
pub fn verdict_key(node_id: &str) -> String {
    format!("{node_id}#verdict")
}

/// 结论的两种取值。它是**账本里那句 detail 的形状**（没通过时 detail 写着为什么），
/// 不是模型产出里的任何一段话
pub fn verdict_value(pass: bool) -> &'static str {
    if pass {
        "pass"
    } else {
        "fail"
    }
}

/// 一次节点运行的预算闸。超了是**停止派发**，不是掐掉正在跑的请求：
/// 后者在服务商侧照样计费，还会留下一段半截话题
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Budget {
    pub max_nodes: usize,
    pub max_tokens: u64,
    pub max_cost_micros: i64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_nodes: 64,
            max_tokens: 2_000_000,
            max_cost_micros: 5_000_000,
        }
    }
}

impl Budget {
    /// 预算那一格用的是微元（1e-6 美元），而花费从用量台账来、记的是 1e-8。
    /// 换算只住在这一个函数里——两处各乘一次，就会有一个方向的预算差一百倍
    pub fn max_cost_e8(&self) -> i64 {
        self.max_cost_micros * 100
    }

    /// 两笔比较各自只住在一个函数里：整份 plan 的判定与每一格的判定共用它们，
    /// 否则"改一个方向的比较"只会红掉其中一侧
    fn tokens_spent(&self, spent_tokens: u64) -> bool {
        spent_tokens >= self.max_tokens
    }

    fn cost_spent(&self, spent_cost_e8: i64) -> bool {
        spent_cost_e8 >= self.max_cost_e8()
    }

    /// 花完了没有。返回"卡住它的那一项"，界面上要说得出是哪一条预算顶住了。
    /// `spent_cost_e8` 的单位是 1e-8 美元（台账那一份），不是配置那格的微元
    pub fn exhausted(
        &self,
        spent_tokens: u64,
        spent_cost_e8: i64,
        nodes_run: usize,
    ) -> Option<&'static str> {
        if nodes_run >= self.max_nodes {
            return Some("节点数");
        }
        if self.tokens_spent(spent_tokens) {
            return Some("token");
        }
        if self.cost_spent(spent_cost_e8) {
            return Some("花费");
        }
        None
    }

    /// **每一格自己**的上限顶住了没有。与 [`Budget::exhausted`] 有两处不同，各有理由：
    ///
    /// - 不问"节点数"那一维：一格就是一格，`max_nodes` 对它的唯一正确读法是"不存在"。
    /// - **`0` = 这一项不设上限**。整份 plan 的预算里 0 是"一分钱都不许花"，
    ///   而档案里那份 `Budget` 是按 plan 的量级定的；如果每格也照这个读法，
    ///   一个没被特别设过的节点会在第一发之前就被自己的空预算掐死（§5.16 那句"要么永远
    ///   顶不住、要么第一格就把整份计划掐死"讲的就是这件事）。
    ///
    /// 顶住之后停的是**下一次尝试**，不动已经在跑的那一发——与并发位、暂停、熔断同一条规矩
    pub fn exhausted_per_task(
        &self,
        spent_tokens: u64,
        spent_cost_e8: i64,
    ) -> Option<&'static str> {
        if self.max_tokens > 0 && self.tokens_spent(spent_tokens) {
            return Some("token");
        }
        if self.max_cost_micros > 0 && self.cost_spent(spent_cost_e8) {
            return Some("花费");
        }
        None
    }
}

/// 能派活、能补做、能收回报的那个角色的档案名。写成常量是因为"谁算监督者"这个判断
/// 散在三个地方（补做的门槛、回报路由、profile 选择），拼错一处就是静默地不当它是监督者
pub const SUPERVISOR: &str = "supervisor";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub id: String,
    /// 这一步要达成什么。它会成为那次 run 的用户消息正文
    pub goal: String,
    /// 用哪个 AgentProfile 跑（角色 / 模型 / 工具白名单 / 权限 / 记忆作用域 / 预算）
    pub profile: String,
    pub depends_on: Vec<String>,
    pub edge: Edge,
    /// 失败后允许几次尝试。与 `Edge::Loop` 是两件事：一次是"这一步没跑成"，一次是"这一步要反复逼近"
    pub max_attempts: u8,
}

/// 环。报错必须点出环上的节点，只说"有环"等于让人拿图自己找
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycles(pub Vec<Vec<String>>);

impl std::fmt::Display for Cycles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let list = self
            .0
            .iter()
            .map(|cycle| cycle.join(" → "))
            .collect::<Vec<_>>()
            .join("；");
        write!(f, "任务图里有环：{list}")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub id: String,
    pub goal: String,
    pub nodes: Vec<Node>,
    pub budget: Budget,
    /// 同时最多跑几个节点。默认 1：并行在这里是一个要花真金白银的显式决定，
    /// 不是我们替用户做的默认
    pub max_parallel: usize,
    /// 跨 plan 抢位时的那一档。`#[serde(default)]` 是为**旧账本**留的：`replan` 行里存的
    /// 是整张图，加这个字段之前落盘的那些行必须还读得回来（读不回来就等于恢复时丢图）
    #[serde(default)]
    pub priority: Priority,
    /// **每一格**自己的花费上限（微元，1e-6 美元）。`0` = 不设。
    ///
    /// 它住在 plan 上而不是只住在请求里，是因为恢复那条路必须拿得到同一个数：
    /// 一份跑到一半崩掉的计划，重启之后每格上限若悄悄变宽（或变窄），那就是
    /// "崩溃前后两个钱袋"——与 §5.15 里"恢复出来的计划按默认 6 跑"同一类洞。
    /// `#[serde(default)]` 同样是给旧账本留的
    #[serde(default)]
    pub node_cost_micros: i64,
    /// 每一格的产出要过什么样的**形状检查**。以前它是 `run_node` 里写死的一句
    /// `Check { min_chars: 1, ..Default::default() }`——于是 `must_contain` 与 `forbid`
    /// 这两格永远为空，`forbid` 那句"用来拦住结论里贴了 token，而不是装饰"就成了
    /// 一句没人执行的注释。放在 plan 上是因为形状由 planner 决定，今天没有任何地方
    /// 逐格写规格；话由用户在发起那一屏说。
    /// 默认 `min_chars: 1` 是**照抄今天的行为**，这一步不改任何既有计划的判定
    #[serde(default = "Check::plan_default")]
    pub check: Check,
}

impl Plan {
    pub fn new(id: &str, goal: &str, nodes: Vec<Node>) -> Self {
        Self {
            id: id.to_string(),
            goal: goal.to_string(),
            nodes,
            budget: Budget::default(),
            max_parallel: 1,
            priority: Priority::default(),
            node_cost_micros: 0,
            check: Check::plan_default(),
        }
    }

    pub fn find(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// 改一条依赖边：`add` 为真是"让 `to` 多等一个 `from`"，为假是从 `to` 的依赖里去掉它。
    ///
    /// 交回一张**新图**而不是改自己：调用方得先把新图写进账本，写成了才换掉共享的那一份——
    /// 改失败的运行中图比不改更糟（design-multi-agent.md §5.13）
    pub fn with_dependency(&self, from: &str, to: &str, add: bool) -> Result<Self, String> {
        if from == to {
            return Err(format!("节点不能依赖自己：「{from}」"));
        }
        if self.find(from).is_none() {
            return Err(format!("这份计划里没有节点「{from}」"));
        }
        if self.find(to).is_none() {
            return Err(format!("这份计划里没有节点「{to}」"));
        }
        let held = self
            .find(to)
            .expect("上面已经查过「to」存在")
            .depends_on
            .iter()
            .any(|held| held == from);
        if add && held {
            return Err(format!("「{to}」已经依赖「{from}」：这条边没加上"));
        }
        if !add && !held {
            return Err(format!("「{to}」并不依赖「{from}」：没有这条边可删"));
        }

        let mut updated = self.clone();
        if let Some(node) = updated.nodes.iter_mut().find(|node| node.id == to) {
            if add {
                node.depends_on.push(from.to_string());
            } else {
                node.depends_on.retain(|held| held != from);
            }
        }
        // 判据不重写：`topo` 报的那个 `Cycles` 本来就点得出环上的节点
        updated.topo().map_err(|cycles| cycles.to_string())?;
        Ok(updated)
    }

    /// 把某一格的边换成界面上能选的那一种。四种拒绝各对应一个真实的坏法：
    /// 格子不存在、等自己、等一个图里没有的格子（那条条件永远等不到）、轮数上限为 0
    ///
    /// 依赖那一边不动：`ready_set` 里条件与循环是**加在依赖之上的第二道闸**，
    /// 不是依赖的替身——换边换的是"上游落定之后我还要不要再等一个结论"
    pub fn with_edge_kind(&self, id: &str, kind: EdgeKind) -> Result<Self, String> {
        if self.find(id).is_none() {
            return Err(format!("这份计划里没有节点「{id}」"));
        }
        let edge = match kind {
            EdgeKind::FinishToStart => Edge::FinishToStart,
            EdgeKind::WaitForVerdict { node, pass } => {
                if node.trim().is_empty() {
                    return Err("条件边得说出它在等哪一格的结论。".to_string());
                }
                if node == id {
                    return Err(format!("「{id}」不能等自己的结论：那一格要等的是别人落定"));
                }
                if self.find(&node).is_none() {
                    return Err(format!("这份计划里没有节点「{node}」，那条条件永远等不到"));
                }
                Edge::Conditional {
                    key: verdict_key(&node),
                    equals: verdict_value(pass).into(),
                }
            }
            EdgeKind::IterateUntilPass { max_iters } => {
                if max_iters == 0 {
                    return Err(
                        "循环的轮数上限不能是 0：那一格一轮都不跑，界面上却挂着一条边。"
                            .to_string(),
                    );
                }
                if max_iters > MAX_ITERATIONS {
                    return Err(format!(
                        "循环最多 {MAX_ITERATIONS} 轮：一轮至少一次请求，这个数是这条命令给的成本天花板"
                    ));
                }
                Edge::Loop {
                    until_key: verdict_key(id),
                    until_value: verdict_value(true).to_string(),
                    max_iters,
                }
            }
        };
        let mut updated = self.clone();
        if let Some(node) = updated.nodes.iter_mut().find(|node| node.id == id) {
            node.edge = edge;
        }
        Ok(updated)
    }

    /// 这一格向谁回报。图里"谁是谁的监督者"不是一句推测，就是那条依赖边：
    /// 工作者 `depends_on` 监督者，所以这一问有唯一答案（多个的话取声明序里第一个，
    /// 并在账上如实写成"它同时挂在两个监督者下"由调用方决定要不要报）
    pub fn supervisor_of(&self, id: &str) -> Option<&str> {
        let node = self.find(id)?;
        node.depends_on
            .iter()
            .find(|held| {
                self.find(held)
                    .map(|boss| boss.profile == SUPERVISOR)
                    .unwrap_or(false)
            })
            .map(String::as_str)
    }

    /// 会向某个监督者回报的那些格子：barrier 的"该到几份"就是它的长度。这个数必须由图
    /// 现算，不能由第一个投报的人带来：图会被 `grow_plan` 追加，用旧数字判断"齐了"就是
    /// 拿一份过期快照当真相。
    ///
    /// 数的是"谁要报"，不是"谁不是监督者"：层级模式里一个中间层既向它的父亲回报、自己也
    /// 带一批工作者。按档案名把中间层筛掉，该到的份数就比来报的人数少，面板上那句"还差 N
    /// 份"会当场说谎（0 减去 1 只能是 0）
    pub fn workers_of(&self, boss: &str) -> Vec<&str> {
        self.nodes
            .iter()
            .filter(|node| self.supervisor_of(&node.id) == Some(boss))
            .map(|node| node.id.as_str())
            .collect()
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    fn incoming(&self, id: &str) -> Vec<&str> {
        match self.find(id) {
            Some(node) => node.depends_on.iter().map(String::as_str).collect(),
            None => Vec::new(),
        }
    }

    /// Kahn 拓扑序。同一层（就绪集）内按 id 排，所以**同样的图一定给出同样的顺序**——
    /// 汇合结果要可复现，靠的就是这一条不依赖完成时刻的定序
    pub fn topo(&self) -> Result<Vec<String>, Cycles> {
        let mut indegree: Vec<(String, usize)> = self
            .nodes
            .iter()
            .map(|node| (node.id.clone(), self.incoming(&node.id).len()))
            .collect();
        let mut ready: Vec<String> = indegree
            .iter()
            .filter(|(_, degree)| *degree == 0)
            .map(|(id, _)| id.clone())
            .collect();
        ready.sort();

        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some(current) = ready.pop() {
            order.push(current.clone());
            let mut freed: Vec<String> = Vec::new();
            for (id, degree) in indegree.iter_mut() {
                if self.incoming(id).contains(&current.as_str()) && *degree > 0 {
                    *degree -= 1;
                    if *degree == 0 {
                        freed.push(id.clone());
                    }
                }
            }
            ready.extend(freed);
            ready.sort();
        }

        if order.len() != self.nodes.len() {
            let stuck: Vec<String> = indegree
                .iter()
                .filter(|(id, degree)| *degree > 0 && !order.contains(id))
                .map(|(id, _)| id.clone())
                .collect();
            return Err(Cycles(vec![stuck]));
        }
        Ok(order)
    }

    /// 每个节点到链尾的最长距离。`topo()` 只在无环时有意义，
    /// 有环时退化成 id 顺序：让上层去报环，而不是在这里 panic 或把图丢掉
    fn depths(&self) -> Vec<(String, usize)> {
        let order = self.topo().unwrap_or_else(|_| {
            let mut ids: Vec<String> = self.nodes.iter().map(|node| node.id.clone()).collect();
            ids.sort();
            ids
        });
        let mut depths: Vec<(String, usize)> = Vec::new();
        for id in order {
            let depth = self
                .incoming(&id)
                .iter()
                .filter_map(|upstream| {
                    depths
                        .iter()
                        .find(|(held, _)| held == upstream)
                        .map(|(_, d)| *d)
                })
                .max()
                .map(|deepest| deepest + 1)
                .unwrap_or(0);
            depths.push((id.clone(), depth));
        }
        depths
    }

    /// 关键路径 = 图上最长链（节点权重一律 1）。调度拿它当优先级：
    /// 剩余链最长的那一支先占一个并发位，plan 的总时长才不会被拖到最后才发现
    pub fn critical_path(&self) -> Vec<String> {
        let depths = self.depths();
        // 起点取"最深的那一格"，同深度并列时取 id 小的：两次调用给出同一条路径
        let Some(start) = depths
            .iter()
            .max_by_key(|(id, depth)| (*depth, std::cmp::Reverse(id.clone())))
            .map(|(id, _)| id.clone())
        else {
            return Vec::new();
        };
        // 从链尾往回走，每一步只认"深度少一层"的上游
        let mut path: Vec<String> = vec![start.clone()];
        let mut current = start;
        loop {
            let depth = depths
                .iter()
                .find(|(held, _)| *held == current)
                .map(|(_, held)| *held)
                .unwrap_or(0);
            if depth == 0 {
                break;
            }
            let mut candidates: Vec<String> = self
                .incoming(&current)
                .iter()
                .filter(|upstream| {
                    depths
                        .iter()
                        .any(|(held, d)| held == *upstream && *d + 1 == depth)
                })
                .map(|upstream| upstream.to_string())
                .collect();
            candidates.sort();
            match candidates.first() {
                Some(next) => {
                    current = next.clone();
                    path.push(current.clone());
                }
                None => break,
            }
        }
        path.reverse();
        path
    }

    pub fn depth_of(&self, id: &str) -> usize {
        self.depths()
            .iter()
            .find(|(held, _)| held == id)
            .map(|(_, depth)| *depth)
            .unwrap_or(0)
    }

    // ---- 七种并行模式：都只是构造器 ----

    /// 扇出-汇聚：一个 split 结论 → N 个 worker → 一个 gather
    pub fn fanout(id: &str, goal: &str, profiles: &[&str]) -> Self {
        let mut nodes = vec![Node {
            id: "split".into(),
            goal: format!("把目标拆成可并行的分支，逐条列出：{goal}"),
            profile: "planner".into(),
            depends_on: vec![],
            edge: Edge::FinishToStart,
            max_attempts: 1,
        }];
        for profile in profiles {
            nodes.push(Node {
                id: format!("worker-{profile}"),
                goal: format!("只回答其中一个分支：{goal}"),
                profile: (*profile).to_string(),
                depends_on: vec!["split".into()],
                edge: Edge::FinishToStart,
                max_attempts: 2,
            });
        }
        nodes.push(Node {
            id: "gather".into(),
            goal: "把各分支的结论汇成一个可执行的答案".into(),
            profile: "integrator".into(),
            depends_on: profiles
                .iter()
                .map(|profile| format!("worker-{profile}"))
                .collect(),
            edge: Edge::FinishToStart,
            max_attempts: 1,
        });
        let mut plan = Self::new(id, goal, nodes);
        plan.max_parallel = profiles.len().max(1);
        plan
    }

    /// 流水线：线性链，段与段各自一个 profile
    pub fn pipeline(id: &str, goal: &str, stages: &[(&str, &str)]) -> Self {
        let nodes = stages
            .iter()
            .enumerate()
            .map(|(index, (name, profile))| Node {
                id: name.to_string(),
                goal: if index == 0 {
                    goal.to_string()
                } else {
                    format!("接着上一步的产出做：{name}")
                },
                profile: profile.to_string(),
                depends_on: if index == 0 {
                    vec![]
                } else {
                    vec![stages[index - 1].0.to_string()]
                },
                edge: Edge::FinishToStart,
                max_attempts: 1,
            })
            .collect();
        Self::new(id, goal, nodes)
    }

    /// 竞争 Best-of-N：同一输入、同一 profile 跑 N 份，由校验器挑一份
    pub fn best_of_n(id: &str, goal: &str, profile: &str, n: usize) -> Self {
        let mut nodes: Vec<Node> = (0..n)
            .map(|index| Node {
                id: format!("cand-{index}"),
                goal: goal.to_string(),
                profile: profile.to_string(),
                depends_on: vec![],
                edge: Edge::FinishToStart,
                max_attempts: 1,
            })
            .collect();
        nodes.push(Node {
            id: "pick".into(),
            goal: "从这几份候选里挑出最好的一份并说明为什么".into(),
            profile: "verifier".into(),
            depends_on: (0..n).map(|index| format!("cand-{index}")).collect(),
            edge: Edge::FinishToStart,
            max_attempts: 1,
        });
        let mut plan = Self::new(id, goal, nodes);
        // N 份前缀各付一次钱，这里不假装便宜：并发就按 N 开，成本面板会把它摊出来
        plan.max_parallel = n.max(1);
        plan
    }

    /// 辩论：两方交替 `rounds` 轮 + 一个裁判。轮数在这里被写成**有限的循环边**
    /// 辩论：正反方**交替**若干轮，最后裁判收口。
    ///
    /// 交替是摊成依赖链表达的（`pro-1 → con-1 → pro-2 → … → judge`），不是两边各挂一条自循环
    /// 再加一个"这一轮该谁说话"的门：轮数在装配那一刻就是已知的（它是请求里那一格），
    /// 链本身就是顺序，而多一道门就是多一处真相（§5.12）。
    /// 上限是**成本天花板**，不是审美：一轮两次请求
    pub fn debate(id: &str, goal: &str, rounds: usize) -> Self {
        let rounds = rounds.clamp(1, 4);
        let mut nodes: Vec<Node> = Vec::with_capacity(rounds * 2 + 1);
        let mut previous: Option<String> = None;
        for round in 1..=rounds {
            for (side, profile, brief) in [
                ("pro", "advocate", "正方立论"),
                ("con", "skeptic", "反方反驳"),
            ] {
                let node_id = format!("{side}-{round}");
                let step_goal = if round == 1 {
                    format!("{brief}：{goal}")
                } else {
                    format!("{brief}（第 {round}/{rounds} 轮，先回应对方上一轮再说）：{goal}")
                };
                nodes.push(Node {
                    id: node_id.clone(),
                    goal: step_goal,
                    profile: profile.into(),
                    depends_on: previous.clone().into_iter().collect(),
                    edge: Edge::FinishToStart,
                    max_attempts: 1,
                });
                previous = Some(node_id);
            }
        }
        nodes.push(Node {
            id: "judge".into(),
            goal: format!("判给哪一方，并说清依据（共 {rounds} 轮交替）"),
            profile: "verifier".into(),
            depends_on: previous.into_iter().collect(),
            edge: Edge::FinishToStart,
            max_attempts: 1,
        });
        Self::new(id, goal, nodes)
    }

    /// 层级：监督者派发，工作者回报，监督者再决定第二批（replanning 提供"第二批"）
    pub fn hierarchical(id: &str, goal: &str, workers: &[&str]) -> Self {
        let mut nodes = vec![Node {
            id: "supervisor".into(),
            goal: format!("规划怎么把这件事拆给{}个工作者：{goal}", workers.len()),
            profile: "supervisor".into(),
            depends_on: vec![],
            edge: Edge::FinishToStart,
            max_attempts: 1,
        }];
        nodes.extend(workers.iter().map(|worker| Node {
            id: worker.to_string(),
            goal: "按监督者分给的那一份做".into(),
            profile: (*worker).to_string(),
            depends_on: vec!["supervisor".into()],
            edge: Edge::FinishToStart,
            max_attempts: 1,
        }));
        nodes.push(Node {
            id: "collect".into(),
            // 契约写在提示词里：`followups` 只认这一种标记，所以"能不能补第二轮"
            // 是模型看得懂的一句话，而不靠我们猜它的措辞
            goal: format!(
                "汇总并决定是否还需要补一轮。确实还缺时，每个缺失项单独一行、行首写\
                 「{FOLLOWUP_MARKER}」，最多三行；不缺就一个字也别多写"
            ),
            profile: "supervisor".into(),
            depends_on: workers.iter().map(|worker| worker.to_string()).collect(),
            edge: Edge::FinishToStart,
            max_attempts: 1,
        });
        let mut plan = Self::new(id, goal, nodes);
        plan.max_parallel = workers.len().max(1);
        plan
    }
}

/// map-reduce 的展开：把带 `MapReduce` 边的节点换成"每个集合元素一个子节点"，
/// 原本等它的那些节点改成等这一批子节点。
///
/// 它是纯函数，展开后的图会整份进账本——恢复时读到的就是当时真的派发过的那张图，
/// 不必重算集合（集合可能已经变了，重算等于让"上次跑到哪"有两个答案）
pub fn expand_map(plan: &Plan, items: &[String], map_id: &str) -> Result<Plan, String> {
    let source = plan
        .find(map_id)
        .ok_or_else(|| format!("图里没有叫「{map_id}」的节点"))?;
    if source.edge != Edge::MapReduce {
        return Err(format!("节点「{map_id}」的边不是 map-reduce，不能展开"));
    }

    // 上限先问预算：10 万条集合展开成 10 万个节点，是把成本事故写进图里
    let produced = plan.node_count() - 1 + items.len();
    if produced > plan.budget.max_nodes {
        return Err(format!(
            "展开会产出 {produced} 个节点，超过预算里的节点数上限 {}",
            plan.budget.max_nodes
        ));
    }

    let children: Vec<Node> = items
        .iter()
        .enumerate()
        .map(|(index, item)| Node {
            id: format!("{map_id}#{index}"),
            goal: format!("{}：{item}", source.goal),
            profile: source.profile.clone(),
            depends_on: source.depends_on.clone(),
            edge: Edge::FinishToStart,
            max_attempts: source.max_attempts,
        })
        .collect();
    let child_ids: Vec<String> = children.iter().map(|child| child.id.clone()).collect();

    let mut nodes: Vec<Node> = Vec::new();
    for node in plan.nodes.iter().filter(|node| node.id != map_id) {
        let mut rewritten = node.clone();
        if rewritten.depends_on.iter().any(|held| held == map_id) {
            rewritten.depends_on.retain(|held| held != map_id);
            rewritten.depends_on.extend(child_ids.clone());
        }
        nodes.push(rewritten);
    }
    nodes.extend(children);

    // 三处"图长了一份"的构造都用 `..plan.clone()`：id / 目标 / 预算 / 并发上限 / 档位
    // 一律跟着原图走。逐字段抄写这件事已经坑过两次——加一个新字段而漏掉一处，
    // 表现是"追加过节点的那张图悄悄换了一份设置"
    let expanded = Plan {
        nodes,
        // 这一批子节点本来就是彼此独立的工作，但并发位仍归编排器的上限管，
        // 这里只是允许它们同时排队
        max_parallel: plan.max_parallel.max(items.len()).min(8),
        ..plan.clone()
    };
    expanded.topo().map_err(|cycles| cycles.to_string())?;
    Ok(expanded)
}

/// 重做节点的后缀。它同时是"这个节点已经是重规划的产物"的标记，
/// 所以一条规矩不需要在图外面再存一份状态
const RETRY_SUFFIX: &str = "#retry";
/// 补一轮的节点后缀，以及监督者要用的标记：只认这一种，别拿整行自由文本当任务
const NEXT_SUFFIX: &str = "#next";
pub const FOLLOWUP_MARKER: &str = "补做：";
/// 一次最多补几路。并行度是用户在面板上点过的数，不该由模型的措辞决定
pub const FOLLOWUP_CAP: usize = 3;
/// 喂回规划器的证据上限。证据是自由文本，而它会进下一次请求的前缀
const EVIDENCE_CAP: usize = 2_000;

/// 一次重新规划：把"这一步没交付"变成图上的新工作，而不是把同一个请求再发一遍。
///
/// 三条规矩，每条都在挡一种把成本说成进度的写法：
/// - **只追加**。已有节点的定义一个字都不改，已经 Done 的那几个更不动——它们的产出
///   已经进过黑板与账本，改定义等于让同一份账本对应两张图。
/// - **重做的那一步不等它重做的那一步**。新节点继承失败节点的前置，
///   否则它永远不 ready；而原本等失败节点的下游改成等新节点，否则失败一扩散就是整图 Skip。
/// - **一个失败只换一次重做**。`{id}#retry` 自己不再被重规划，第二次失败就是这条路走不通，
///   该由人来看——再叠一层只会把钱烧在同一个误解上。
///
/// 返回整份新图（与 [`expand_map`] 同形）：账本里落的就是当时真的派发过的那张图，
/// 恢复时不必重算失败证据（证据来自模型，重算等于让"上次跑到哪"有两个答案）。
pub fn replan(plan: &Plan, failed_id: &str, evidence: &str) -> Result<Plan, String> {
    let source = plan
        .find(failed_id)
        .ok_or_else(|| format!("图里没有叫「{failed_id}」的节点，没什么可重规划的"))?;
    if failed_id.ends_with(RETRY_SUFFIX) {
        return Err(format!(
            "「{failed_id}」已经是重规划的产物了，同一个误解不该再花一次钱"
        ));
    }
    let retry_id = format!("{failed_id}{RETRY_SUFFIX}");
    if plan.find(&retry_id).is_some() {
        return Err(format!(
            "「{failed_id}」已经重规划过一次了，这一次失败请人来判断"
        ));
    }
    if plan.node_count() + 1 > plan.budget.max_nodes {
        return Err(format!(
            "重规划要多出 1 个节点，总数会到 {}，超过预算里的节点数上限 {}",
            plan.node_count() + 1,
            plan.budget.max_nodes
        ));
    }

    // 证据是模型产出的自由文本：截断，别把它整份塞进下一次请求的前缀里
    let taken: String = evidence.chars().take(EVIDENCE_CAP).collect();
    let evidence = if evidence.chars().count() > EVIDENCE_CAP {
        format!("{}\n…（证据已截断）", taken.trim_end())
    } else {
        taken
    };

    let retry = Node {
        id: retry_id.clone(),
        goal: format!(
            "{}\n\n上一步「{failed_id}」没有交付它该交付的东西。它留下的证据：\n{}\n\n\
             这一次换一条路：只做上面还缺的那部分，别重复已经有结论的地方，\
             也不要以「上一步」的口吻复述它。",
            source.goal, evidence
        ),
        profile: source.profile.clone(),
        // 继承失败节点的前置，而不是等它自己
        depends_on: source.depends_on.clone(),
        edge: Edge::FinishToStart,
        max_attempts: source.max_attempts,
    };

    let nodes: Vec<Node> = plan
        .nodes
        .iter()
        .map(|node| {
            if !node.depends_on.iter().any(|held| held == failed_id) {
                return node.clone();
            }
            let mut rewritten = node.clone();
            rewritten.depends_on.retain(|held| held != failed_id);
            rewritten.depends_on.push(retry_id.clone());
            rewritten
        })
        .chain(std::iter::once(retry))
        .collect();

    let replanned = Plan {
        nodes,
        ..plan.clone()
    };
    replanned.topo().map_err(|cycles| cycles.to_string())?;
    Ok(replanned)
}

/// 监督者说"还要补一轮"时的第二批工作。
///
/// 只认 `补做：` 这一个标记、最多 [`FOLLOWUP_CAP`] 条：**不把自由文本整行当任务**。
/// 一段解释性输出就能凭空多出十个节点，那是把成本事故写进图里——而并行度是用户
/// 在面板上点过的那个数，不该由模型的措辞决定。
///
/// 与 [`replan`] 的分工：那一个回答"这一支失败了，换什么"，这一个回答"这一支成功了，
/// 还缺什么"。两条都只追加，都不动已 Done 的节点，都受同一份 `budget.max_nodes` 管
pub fn followups(plan: &Plan, from_id: &str, output: &str) -> Result<Plan, String> {
    let source = plan
        .find(from_id)
        .ok_or_else(|| format!("图里没有叫「{from_id}」的节点，没什么可补的"))?;
    // 谁能再派一批：只有监督者。工作者觉得自己"还缺一步"就往下长节点，
    // 那是把自我扩散的权力发给每一个角色（§8 风险 5 那条在这里同样成立）
    if source.profile != SUPERVISOR {
        return Err(format!(
            "「{from_id}」的档案是「{}」，不是监督者：它不能自己加活",
            source.profile
        ));
    }
    if plan.find(&format!("{from_id}{NEXT_SUFFIX}-1")).is_some() {
        return Err(format!(
            "「{from_id}」已经补过一轮了，再加一轮就是无限自我扩散"
        ));
    }

    let tasks: Vec<String> = output
        .lines()
        .filter_map(|line| line.split_once(FOLLOWUP_MARKER))
        .map(|(_, rest)| rest.trim().trim_end_matches(['。', '.', '；', ';']).trim())
        .filter(|task| !task.is_empty())
        .take(FOLLOWUP_CAP)
        .map(str::to_string)
        .collect();
    if tasks.is_empty() {
        return Err(format!(
            "「{from_id}」的产出里没有「{FOLLOWUP_MARKER}」这一行：它认为不需要补第二轮"
        ));
    }
    let produced = plan.node_count() + tasks.len();
    if produced > plan.budget.max_nodes {
        return Err(format!(
            "补一轮要多出 {} 个节点，总数会到 {produced}，超过预算里的节点数上限 {}",
            tasks.len(),
            plan.budget.max_nodes
        ));
    }

    let children = tasks
        .iter()
        .enumerate()
        .map(|(index, task)| Node {
            id: format!("{from_id}{NEXT_SUFFIX}-{}", index + 1),
            goal: task.clone(),
            profile: "worker".into(),
            depends_on: vec![from_id.to_string()],
            edge: Edge::FinishToStart,
            max_attempts: 1,
        })
        .collect::<Vec<Node>>();

    let grew = Plan {
        nodes: plan.nodes.iter().cloned().chain(children).collect(),
        ..plan.clone()
    };
    grew.topo().map_err(|cycles| cycles.to_string())?;
    Ok(grew)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, deps: &[&str]) -> Node {
        Node {
            id: id.into(),
            goal: format!("{id} 要做的事"),
            profile: "worker".into(),
            depends_on: deps.iter().map(|dep| dep.to_string()).collect(),
            edge: Edge::FinishToStart,
            max_attempts: 1,
        }
    }

    /// 加一条边、删一条边，各自都真的改到了图上；其余四种情形都**报错**而不是静默不做事。
    /// 一个返回 `Ok` 的"没什么变化"会让人以为改成了（§5.13 的第四个不许）
    #[test]
    fn an_edge_can_be_added_and_removed_and_every_refusal_names_its_reason() {
        let plan = Plan::new(
            "p",
            "g",
            vec![node("a", &[]), node("b", &[]), node("c", &["b"])],
        );

        let added = plan
            .with_dependency("a", "b", true)
            .expect("加一条合法的边该成功");
        assert_eq!(
            added.find("b").expect("b 在").depends_on,
            vec!["a".to_string()]
        );
        assert_eq!(
            plan.find("b").expect("b 在").depends_on,
            Vec::<String>::new(),
            "改的是新图，不是自己"
        );

        let removed = added
            .with_dependency("a", "b", false)
            .expect("删回去也该成功");
        assert_eq!(removed, plan, "一加一删要回到原样");

        for (from, to, add, want) in [
            ("b", "b", true, "自己"),
            ("nope", "b", true, "nope"),
            ("b", "nope", true, "nope"),
            ("a", "a", false, "自己"),
        ] {
            let error = removed
                .with_dependency(from, to, add)
                .expect_err("这一种该被拒");
            assert!(error.contains(want), "那句拒要说得出「{want}」：{error}");
        }

        // 重复加与删不存在的：两种都是"没发生的事"，不能报 Ok
        let error = removed
            .with_dependency("b", "c", true)
            .expect_err("c 本来就依赖 b，同一条边不能加两次");
        assert!(error.contains("已经依赖"), "{error}");
        let error = removed
            .with_dependency("a", "c", false)
            .expect_err("没有这条边可删");
        assert!(error.contains("并不依赖"), "{error}");

        // 正对照：一个节点多等一格是合法形状（不是"一条依赖只能挂一个"）
        let second = removed
            .with_dependency("a", "c", true)
            .expect("c 可以再等一个 a");
        assert_eq!(
            second.find("c").expect("c 在").depends_on,
            vec!["b".to_string(), "a".to_string()]
        );
    }

    /// 改完不成环——判据就是 `topo` 那一份，不在这里再写一次"能不能到"
    #[test]
    fn an_edit_that_would_close_a_cycle_is_refused_by_name() {
        let plan = Plan::new(
            "p",
            "g",
            vec![node("a", &[]), node("b", &["a"]), node("c", &["b"])],
        );
        let error = plan
            .with_dependency("c", "a", true)
            .expect_err("让 a 去等它自己的下游，是一个环");
        assert!(error.contains("环"), "{error}");
        assert!(
            error.contains("a") && error.contains("c"),
            "那句错要点了环上的格子：{error}"
        );
    }

    #[test]
    fn shuffled_node_definitions_still_yield_one_deterministic_order() {
        let a = Plan::new(
            "p",
            "g",
            vec![node("c", &["a", "b"]), node("a", &[]), node("b", &["a"])],
        );
        let b = Plan::new(
            "p",
            "g",
            vec![node("a", &[]), node("b", &["a"]), node("c", &["b", "a"])],
        );
        assert_eq!(
            a.topo().unwrap(),
            b.topo().unwrap(),
            "同一张图两个写法排出两个顺序，汇合结果就不可复现"
        );
        assert_eq!(
            a.topo().unwrap(),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn a_cycle_is_refused_and_names_the_nodes_it_includes() {
        let plan = Plan::new(
            "p",
            "g",
            vec![node("a", &["c"]), node("b", &["a"]), node("c", &["b"])],
        );
        let error = plan.topo().expect_err("环必须被拒");
        let text = error.to_string();
        assert!(
            text.contains('a') && text.contains('b') && text.contains('c'),
            "报错要认得出环上都有谁：{text}"
        );
        // 变异防线：如果哪天 topo 忘了数环，critical_path 也不能 panic
        assert!(
            !plan.critical_path().is_empty(),
            "有环时关键路径退化成顺序列表，而不是把图丢掉"
        );
    }

    #[test]
    fn the_critical_path_is_the_longest_chain_not_the_last_defined() {
        // a → b → c 与 a → d：d 与 b/c 同级，但关键路径只走最长那条
        let plan = Plan::new(
            "p",
            "g",
            vec![
                node("a", &[]),
                node("b", &["a"]),
                node("c", &["b"]),
                node("d", &["a"]),
            ],
        );
        assert_eq!(
            plan.critical_path(),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!(plan.depth_of("a"), 0);
        assert_eq!(plan.depth_of("c"), 2);
    }

    #[test]
    fn a_goal_splits_into_at_least_three_nodes_that_can_run_in_parallel() {
        // 验收第 1 条的形状：拆得出来，且拆出来的那一层确实同时在就绪集里
        let plan = Plan::fanout(
            "p",
            "找出这个仓库里所有会改动磁盘的地方",
            &["reader", "grep", "history"],
        );
        assert!(plan.node_count() >= 3, "拆出来不到三个子任务，谈不上并行");
        assert_eq!(plan.max_parallel, 3);
        let ready: Vec<String> = plan
            .nodes
            .iter()
            .filter(|node| node.depends_on.iter().any(|dep| dep == "split"))
            .map(|node| node.id.clone())
            .collect();
        assert_eq!(ready.len(), 3, "扇出的三支要互不依赖，才能真的并行");
        assert_eq!(
            plan.find("gather").unwrap().depends_on.len(),
            3,
            "汇聚要等齐三支，不能只等最后写完的那支"
        );
    }

    /// 辩论要**交替**：每一句吃上一句，正方也要能回嘴。顺序由依赖链表达，
    /// 不由"这一轮该谁说话"的门表达（§5.12）
    #[test]
    fn debate_alternates_by_construction() {
        let plan = Plan::debate("p", "这个改造值不值得做", 3);
        let ids: Vec<String> = plan.nodes.iter().map(|node| node.id.clone()).collect();
        assert_eq!(
            ids,
            vec!["pro-1", "con-1", "pro-2", "con-2", "pro-3", "con-3", "judge"],
            "交替的形状就是这条链"
        );
        for (index, node) in plan.nodes.iter().enumerate() {
            let expected: Vec<String> = if index == 0 {
                Vec::new()
            } else {
                vec![ids[index - 1].clone()]
            };
            assert_eq!(node.depends_on, expected, "{} 不该等别人", node.id);
        }
        assert!(plan.topo().is_ok(), "交替链不该被认成环");
        assert!(
            plan.find("pro-2").unwrap().goal.contains("第 2/3 轮"),
            "每一轮要说得清是第几轮：{}",
            plan.find("pro-2").unwrap().goal
        );
        // 上限是成本天花板（一轮两次请求），下限是"至少辩一个来回"
        assert_eq!(Plan::debate("p", "g", 99).node_count(), 4 * 2 + 1);
        assert_eq!(Plan::debate("p", "g", 0).node_count(), 2 + 1);
    }

    #[test]
    fn map_expansion_happens_before_dispatch_and_respects_the_node_budget() {
        let base = Plan::new(
            "p",
            "g",
            vec![
                node("read", &[]),
                Node {
                    id: "each".into(),
                    goal: "逐条检查".into(),
                    profile: "worker".into(),
                    depends_on: vec!["read".into()],
                    edge: Edge::MapReduce,
                    max_attempts: 1,
                },
                node("reduce", &["each"]),
            ],
        );
        let items = vec!["a.rs".to_string(), "b.rs".to_string(), "c.rs".to_string()];
        let expanded = expand_map(&base, &items, "each").expect("展开应成功");
        assert!(expanded.find("each#0").is_some() && expanded.find("each#2").is_some());
        assert!(
            expanded.find("each").is_none(),
            "展开后原来的那一个节点不该冒充批次"
        );
        assert!(
            expanded
                .find("reduce")
                .unwrap()
                .depends_on
                .iter()
                .all(|dep| dep.starts_with("each#")),
            "reduce 要等的是展开出来的那三支"
        );
        // 预算顶住的那一条：不是"少跑几个"，是明确拒绝展开
        let tiny = Plan {
            budget: Budget {
                max_nodes: 3,
                ..Budget::default()
            },
            ..base.clone()
        };
        let error = expand_map(&tiny, &items, "each").expect_err("超预算的展开必须被拒");
        assert!(
            error.contains("节点数"),
            "要说清是哪一条预算顶住了：{error}"
        );
        expanded.topo().expect("展开后的图不许有环");
    }

    /// map-reduce 那一格上的键名从来没有人读，而它那副"等某一格"的写法把一件不存在的事
    /// 摆进了界面上唯一会读它的地方。两条一起钉：**展开后这张图里没有这一格了**（那句读数
    /// 因此没有一个可能的读者），以及这一格上不再带任何键名
    #[test]
    fn the_map_reduce_edge_carries_no_key_and_leaves_no_gate_behind() {
        let base = Plan::new(
            "p",
            "逐条来",
            vec![
                node("read", &[]),
                Node {
                    id: "each".into(),
                    goal: "逐条检查".into(),
                    profile: "worker".into(),
                    depends_on: vec!["read".into()],
                    edge: Edge::MapReduce,
                    max_attempts: 1,
                },
                node("reduce", &["each"]),
            ],
        );
        let items = vec!["一".to_string(), "二".to_string()];
        let expanded = expand_map(&base, &items, "each").expect("展开应成功");
        assert!(
            expanded
                .nodes
                .iter()
                .all(|item| item.edge != Edge::MapReduce),
            "展开后还有一格带 map-reduce：那句读数就又有资格骗人了"
        );
        assert!(
            !Edge::MapReduce.gate_text().contains("「"),
            "还在引用一个键名：{}",
            Edge::MapReduce.gate_text()
        );
        assert!(
            Edge::MapReduce.gate_text().contains("请求"),
            "得说清集合从哪来：{}",
            Edge::MapReduce.gate_text()
        );

        // 生产代码里不该再有任何一处读这个键名（它在源码里只活过一次，作为那段变体注释的题材）。
        // 针脚用 concat! 拼：这条测试自己就在被搜的文件里，写成整串会数到自己
        let needle = concat!("items", "_from");
        for file in [include_str!("graph.rs"), include_str!("orchestrator.rs")] {
            let code = file.replace('\r', "");
            let mentions = code.matches(needle).count();
            assert!(
                mentions <= 1,
                "{needle} 又长出读者了（{} 处）：那要先回答它从哪一格被写",
                mentions
            );
        }
    }

    #[test]
    fn budgets_stop_dispatch_rather_than_killing_a_running_request() {
        let budget = Budget::default();
        assert_eq!(budget.exhausted(0, 0, 1), None);
        assert_eq!(budget.exhausted(budget.max_tokens, 0, 0), Some("token"));
        // 花费那一格比的是台账那个单位（1e-8 美元），不是配置那一格的微元。
        // 差一百倍这件事必须在这一条里红起来，而不是等某一次真跑到 $5 才发现闸是空的
        assert_eq!(
            budget.exhausted(0, budget.max_cost_e8() - 1, 0),
            None,
            "差一分就该还能派"
        );
        assert_eq!(budget.exhausted(0, budget.max_cost_e8(), 0), Some("花费"));
        assert_eq!(budget.exhausted(0, 0, budget.max_nodes), Some("节点数"));
    }

    /// 同一份 `Budget` 按"一整份 plan"读与按"其中一格"读，差别只有两条，而且两条都得钉住：
    /// 节点数那一维对一格没有意义，`0` 在一格这里读作"不设"而不是"立刻花完"。
    /// 后面那条是这次改动唯一的默认值差异——`0` 读错的话，每一个没被特别设过的节点
    /// 都会在自己的第一发之前被掐死，而那正是 §5.16 里说"每格预算没有可以对齐的数字"的原因
    #[test]
    fn the_per_task_cap_ignores_the_node_dimension_and_reads_zero_as_unset() {
        let plan_shaped = Budget::default();
        assert_eq!(
            plan_shaped.exhausted_per_task(0, 0),
            None,
            "刚起跑的一格不该被档案里那份按 plan 量级定的预算顶住"
        );
        let one_node_left = Budget {
            max_nodes: 1,
            ..plan_shaped.clone()
        };
        assert_eq!(
            one_node_left.exhausted_per_task(0, 0),
            None,
            "「最多几格」这一维问的是一格，就是问错了对象"
        );
        assert_eq!(
            one_node_left.exhausted(0, 0, 1),
            Some("节点数"),
            "同一份预算按整份 plan 读时照旧认节点数——两读法共用的那两笔比较没被改坏"
        );
        assert_eq!(
            plan_shaped.exhausted_per_task(plan_shaped.max_tokens, 0),
            Some("token")
        );
        assert_eq!(
            plan_shaped.exhausted_per_task(0, plan_shaped.max_cost_e8() - 1),
            None
        );
        assert_eq!(
            plan_shaped.exhausted_per_task(0, plan_shaped.max_cost_e8()),
            Some("花费")
        );

        let off = Budget {
            max_nodes: 8,
            max_tokens: 0,
            max_cost_micros: 0,
        };
        assert_eq!(
            off.exhausted_per_task(u64::MAX, i64::MAX),
            None,
            "0 是\"不设这一项\"，不是\"一分都不许花\""
        );
    }

    #[test]
    fn pipeline_stages_each_own_one_slot_and_depend_on_the_previous() {
        let plan = Plan::pipeline(
            "p",
            "先读再改再验",
            &[
                ("read", "reader"),
                ("write", "editor"),
                ("check", "verifier"),
            ],
        );
        assert_eq!(
            plan.topo().unwrap(),
            vec!["read".to_string(), "write".to_string(), "check".to_string()]
        );
        assert_eq!(
            plan.max_parallel, 1,
            "流水线默认一支在跑：段与段之间是同一件事的连续"
        );
        assert_eq!(
            plan.find("check").unwrap().depends_on,
            vec!["write".to_string()]
        );
    }

    /// read → write → check：一条最小链条，够看出"谁改等谁"
    fn chain() -> Plan {
        Plan::new(
            "p",
            "读完再改再验",
            vec![
                node("read", &[]),
                node("write", &["read"]),
                node("check", &["write"]),
            ],
        )
    }

    /// 这一条钉的是审计出来的那个洞的形状：**能被选出来的边，读的键必须有人写**。
    /// 判据（`ready_set` / `loop_open`）从第一天起就在读黑板上的结论键，而生产代码从来
    /// 不往那个键写东西——于是"条件"与"循环"这两种边在链路上永不成立，只有测试会写那个键
    #[test]
    fn an_authorable_edge_only_reads_a_key_that_production_code_actually_writes() {
        let gated = chain()
            .with_edge_kind(
                "check",
                EdgeKind::WaitForVerdict {
                    node: "write".into(),
                    pass: false,
                },
            )
            .expect("等某一格的校验结论，是这几种里最直白的一种");
        assert_eq!(
            gated.find("check").expect("check 在图里").edge,
            Edge::Conditional {
                key: verdict_key("write"),
                equals: "fail".into()
            },
            "条件边读的那个键必须由 verdict_key 拼出来，而不是让人手打一个"
        );

        let looped = chain()
            .with_edge_kind("write", EdgeKind::IterateUntilPass { max_iters: 3 })
            .expect("转三圈");
        assert_eq!(
            looped.find("write").expect("write 在图里").edge,
            Edge::Loop {
                until_key: verdict_key("write"),
                until_value: "pass".into(),
                max_iters: 3,
            },
            "循环等的是**自己**那一格的结论：等别人的话这一格永远停不下来"
        );
        assert_eq!(
            looped
                .with_edge_kind("write", EdgeKind::FinishToStart)
                .expect("换回普通")
                .find("write")
                .unwrap()
                .edge,
            Edge::FinishToStart,
            "换回来也要有路：不然一次选择就回不去了"
        );

        // 这份图会被写成账本里的一行、重启后再读回来，所以两种新边都得过得了编码这一关
        let json = serde_json::to_value(&gated).expect("Plan 总能序列化");
        assert_eq!(
            serde_json::from_value::<Plan>(json.clone()).expect("读得回来"),
            gated,
            "边换了形状而账本装不下它，恢复之后就变成另一张图"
        );
        assert_eq!(
            json["nodes"][2]["edge"]["key"]
                .as_str()
                .expect("条件边序列化出来带 key 那一格"),
            "write#verdict",
            "结论键的形状只住在一个函数里，别处不许再拼一次"
        );
    }

    /// 每一种拒绝都对应一个"会静默变成永远不跑"的形状
    #[test]
    fn a_shape_that_could_never_fire_is_refused_where_it_is_chosen() {
        let err = chain()
            .with_edge_kind("没有这一格", EdgeKind::FinishToStart)
            .unwrap_err();
        assert!(err.contains("没有这一格"), "格子不存在就要点名它：{err}");

        let err = chain()
            .with_edge_kind(
                "write",
                EdgeKind::WaitForVerdict {
                    node: "write".into(),
                    pass: true,
                },
            )
            .unwrap_err();
        assert!(
            err.contains("自己"),
            "等自己的结论，这一格永远等不到：{err}"
        );

        let err = chain()
            .with_edge_kind(
                "check",
                EdgeKind::WaitForVerdict {
                    node: "别处的计划".into(),
                    pass: true,
                },
            )
            .unwrap_err();
        assert!(
            err.contains("永远等不到"),
            "等一张图里没有的格子＝一条永不成立的边：{err}"
        );

        let err = chain()
            .with_edge_kind(
                "check",
                EdgeKind::WaitForVerdict {
                    node: "   ".into(),
                    pass: true,
                },
            )
            .unwrap_err();
        assert!(err.contains("哪一格"), "空格子名也要说清缺的是什么：{err}");

        let err = chain()
            .with_edge_kind("write", EdgeKind::IterateUntilPass { max_iters: 0 })
            .expect_err("0 轮不是\"跑一次\"，是\"一轮都不该跑\"");
        assert!(err.contains("不能是 0"), "{err}");

        let err = chain()
            .with_edge_kind(
                "write",
                EdgeKind::IterateUntilPass {
                    max_iters: MAX_ITERATIONS + 1,
                },
            )
            .expect_err("轮数上限由这条命令给，不由界面随手填");
        assert!(
            err.contains("成本天花板"),
            "超上限要说的是为什么有这个数：{err}"
        );
    }

    #[test]
    fn a_failed_step_becomes_new_work_that_its_downstream_waits_for() {
        let plan = chain();
        let after = replan(
            &plan,
            "write",
            "它只写了前半段，最后报了一句 self-corrected 就停了",
        )
        .expect("这一步失败了，就该有重做的路");
        let retry = after.find("write#retry").expect("重做的那一步要在图里");
        assert_eq!(
            retry.depends_on,
            vec!["read".to_string()],
            "重做等的是失败那一步的前置，不是它自己——等它就永远不 ready"
        );
        assert_eq!(
            after.find("check").unwrap().depends_on,
            vec!["write#retry".to_string()],
            "下游要改等重做的那一步，否则失败一扩散就是整图 Skip"
        );
        assert_eq!(
            after.find("write"),
            plan.find("write"),
            "已有节点的定义一个字都不改"
        );
        assert_eq!(after.node_count(), 4, "只多出一个重做节点");
        assert!(
            retry.goal.contains("self-corrected"),
            "证据得真的进了请求：{}",
            retry.goal
        );
        assert_eq!(retry.profile, "worker", "沿用失败节点自己的档案");
    }

    #[test]
    fn a_step_only_gets_one_retry_and_a_retry_never_gets_one() {
        let plan = chain();
        let once = replan(&plan, "write", "缺后半段").expect("第一次失败该换条路");
        let twice = replan(&once, "write", "还是缺");
        assert!(twice.is_err(), "同一个误解不该烧第二次：{twice:?}");
        let nested = replan(&once, "write#retry", "重做也没交付");
        assert!(nested.is_err(), "重规划的产物不再重规划：{nested:?}");
        assert_eq!(once.node_count(), 4, "被拒的两次都不该动过这张图");
    }

    #[test]
    fn replanning_respects_the_node_budget() {
        let plan = Plan {
            budget: Budget {
                max_nodes: 3,
                ..Default::default()
            },
            ..chain()
        };
        let error = replan(&plan, "write", "缺半段").expect_err("预算顶住时不能偷偷多派一个节点");
        assert!(error.contains("预算"), "报错要说清是哪一条闸拦的：{error}");
        assert_eq!(plan.node_count(), 3, "纯函数：被拒的那次没把原图改坏");
    }

    #[test]
    fn evidence_is_truncated_rather_than_streamed_into_the_next_request() {
        let long = "证据".repeat(EVIDENCE_CAP);
        let after = replan(&chain(), "write", &long).expect("再长的证据也该能重规划");
        let goal = &after.find("write#retry").expect("节点在").goal;
        assert!(goal.contains("证据已截断"), "截了要说：{goal}");
        assert!(
            goal.chars().count() < EVIDENCE_CAP * 2 + 400,
            "提示词被证据撑大就是下一笔钱：{} 字符",
            goal.chars().count()
        );
    }

    /// 成功的那一支也能长出第二批：按标记拆、按上限截、而且只有监督者有这个权力
    #[test]
    fn a_supervisor_can_dispatch_a_second_batch_but_a_worker_cannot() {
        let plan = Plan::hierarchical("p", "查三份资料再汇总", &["w0", "w1", "w2"]);
        let output = "汇总完了。\n补做：核对第 2 份里的日期\n补做：把结论翻译成英文\n\
                      补做：查一下第三个来源\n补做：这一条超出上限";
        let grew = followups(&plan, "collect", output).expect("它说了还缺，就该长出第二批");
        assert_eq!(grew.node_count(), plan.node_count() + FOLLOWUP_CAP);
        let next = grew.find("collect#next-1").expect("补做的那一批要在图里");
        assert_eq!(
            next.depends_on,
            vec!["collect".to_string()],
            "补的那一批等的是监督者自己"
        );
        assert_eq!(
            next.goal, "核对第 2 份里的日期",
            "标记后面那句话就是它的目标，别加自己的话"
        );
        assert_eq!(
            next.profile, "worker",
            "补做的一律是工作者：它们不能再往下派"
        );
        assert!(
            grew.find("collect#next-4").is_none(),
            "第四行标记该被上限截掉"
        );
        assert!(
            followups(&grew, "collect", "补做：再来一轮").is_err(),
            "补一轮只补一次，否则就是无人叫停的自我扩散"
        );
        assert!(
            followups(&plan, "w0", "补做：我自己加活").is_err(),
            "工作者不能自己加活：能派第二批的是监督者"
        );
        assert!(
            followups(&plan, "没这个节点", "补做：x").is_err(),
            "图里没有的 id 不该产出工作"
        );
    }

    /// 没有标记 = 它认为不需要补。这不是一次失败，是正常收工：报错要说得清是谁没说话
    #[test]
    fn no_marker_means_no_second_batch() {
        let plan = Plan::hierarchical("p", "g", &["w0"]);
        let error =
            followups(&plan, "collect", "都齐了，不用补。").expect_err("没标记就该没有第二批");
        assert!(
            error.contains("不需要补第二轮"),
            "要说清是没标记，不是解析坏了：{error}"
        );
    }

    #[test]
    fn an_unknown_step_cannot_be_replanned() {
        let plan = chain();
        let error = replan(&plan, "没这个节点", "证据").expect_err("图里没有的 id 不该产出工作");
        assert!(error.contains("没这个节点"), "要点名是哪个 id：{error}");
    }

    /// 收发台要问的两个问题（这一格向谁报、这一代该到几份）在图里没有第二个答案：
    /// 就是那条依赖边。所以它俩必须能被纯算出来，不靠谁在运行时登记
    #[test]
    fn who_reports_to_whom_is_read_straight_off_the_dependency_edges() {
        let plan = Plan::hierarchical("p", "g", &["w0", "w1", "w2"]);
        assert_eq!(plan.supervisor_of("w0"), Some("supervisor"));
        assert_eq!(plan.supervisor_of("supervisor"), None, "最上层没人收它的报");
        assert_eq!(
            plan.supervisor_of("collect"),
            None,
            "collect 等的是三个工作者，不是监督者"
        );
        assert_eq!(
            plan.workers_of("supervisor"),
            vec!["w0", "w1", "w2"],
            "该到几份按图现算，不能由第一个投报的人带来"
        );
        assert!(plan.workers_of("w0").is_empty(), "工作者下面没有格子");
        // 补做的那一批挂在监督者下面：它们确实是第二代的报者
        let grew = followups(&plan, "supervisor", "补做：再查一个来源").expect("监督者说了还缺");
        assert_eq!(
            grew.workers_of("supervisor"),
            vec!["w0", "w1", "w2", "supervisor#next-1"]
        );
    }

    /// 中间层既向它的父亲回报、自己也带一批工作者。按档案名把它筛掉，"该到几份"就会比
    /// "来报的人"少一个——那句"还差 N 份"就当场上谎
    #[test]
    fn a_middle_layer_reports_up_and_still_has_workers_of_its_own() {
        let node = |id: &str, profile: &str, depends_on: &[&str]| Node {
            id: id.into(),
            goal: format!("{id} 那一份"),
            profile: profile.into(),
            depends_on: depends_on.iter().map(|held| held.to_string()).collect(),
            edge: Edge::FinishToStart,
            max_attempts: 1,
        };
        let plan = Plan::new(
            "p",
            "三层",
            vec![
                node("boss", SUPERVISOR, &[]),
                node("mid", SUPERVISOR, &["boss"]),
                node("leaf", "worker", &["mid"]),
            ],
        );
        assert_eq!(
            plan.supervisor_of("mid"),
            Some("boss"),
            "中间层也要向它的父亲回报"
        );
        assert_eq!(plan.supervisor_of("leaf"), Some("mid"));
        assert_eq!(
            plan.workers_of("boss"),
            vec!["mid"],
            "boss 那一代该到的就是 mid 这一份"
        );
        assert_eq!(
            plan.workers_of("mid"),
            vec!["leaf"],
            "它自己那一层的人归它收"
        );
        assert_eq!(plan.workers_of("leaf"), Vec::<&str>::new(), "工作者不带人");
    }
}
