use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tauri::{AppHandle, State};

/// 工作线程在这里挂起等前端决策；前端只能按 id 投一次票，读不到别人的请求。
/// 三个集合都用 Arc 包一层，才能把句柄 clone 进 'static 的工作线程。
///
/// `remembered` 是**话题内规则**：用户对"到底要跑哪一下"点过一次头之后，同一份动作不再重复问。
/// 键是 `capability 键集合 + 指纹` 的哈希（`tool_runtime::Ruling::remember_key`），
/// 所以批准过一次 `git push origin main` 不会顺手放行 `git push origin dev`。
/// 它只活在这个进程的内存里——落盘等于把一次点头变成永久授权，那是另一件事（权限覆盖项）。
///
/// `staged` 只是"这条待批的 id 对应哪一份动作"的一张对照表：界面上的
/// "本话题内允许"按钮只知道 id，键由后端算，前端造不出一条它没见过的规则。
#[derive(Clone)]
pub struct ApprovalHub {
    waiting: Arc<Mutex<HashMap<String, Sender<bool>>>>,
    /// ask_user 的等待表：答案是字符串（点选的选项，或自由输入的那句话）。
    /// 与审批共用一个壳但不是一张表——审批投的是"允不允许"，这里投的是"答案本身"
    askings: Arc<Mutex<HashMap<String, Sender<String>>>>,
    /// `键 → 当时给用户看的那句话`。只存哈希的话，"你允许过什么"就报不出内容、
    /// 逐条撤销无从点起（design-security-permission.md §11）
    remembered: Arc<Mutex<HashMap<String, String>>>,
    staged: Arc<Mutex<HashMap<String, (String, String)>>>,
}

impl Default for ApprovalHub {
    fn default() -> Self {
        Self {
            waiting: Arc::new(Mutex::new(HashMap::new())),
            askings: Arc::new(Mutex::new(HashMap::new())),
            remembered: Arc::new(Mutex::new(HashMap::new())),
            staged: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl ApprovalHub {
    /// 等待审批。stop 是"停止生成"开关：等审批时按停止要能立刻醒过来，
    /// 而不是干等满 10 分钟超时——所以阻塞收信换成 100ms 轮询。
    /// 超时和被打断都按**不批准**处理：拿不准的时候不动用户的磁盘
    pub fn wait(&self, id: &str, timeout: Duration, stop: &AtomicBool) -> bool {
        let (tx, rx) = mpsc::channel();
        self.lock().insert(id.to_string(), tx);

        let deadline = Instant::now() + timeout;
        let approved = loop {
            if stop.load(Ordering::Relaxed) {
                break false;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(decision) => break decision,
                Err(RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline {
                        break false;
                    }
                }
                Err(_) => break false,
            }
        };
        self.lock().remove(id);
        // 这一次询问结束了，对照表里那一条就该跟着走：留着它，
        // 下一次同一个 id 的询问会继承上一份规则
        self.staged().remove(id);
        approved
    }

    pub fn resolve(&self, id: &str, approved: bool) {
        if let Some(tx) = self.lock().remove(id) {
            let _ = tx.send(approved);
        }
    }

    /// 登记"这条待批动作是哪一份、当时给用户看的是哪句话"。只在真的要弹确认框时调用
    pub fn stage(&self, id: &str, key: &str, label: &str) {
        self.staged()
            .insert(id.to_string(), (key.to_string(), label.to_string()));
    }

    pub fn is_remembered(&self, key: &str) -> bool {
        self.remembered().contains_key(key)
    }

    /// 界面上的"本话题内允许"：按这一次询问的 id 找到那份动作的键，记住它，再投赞成票。
    /// 返回 false = 这一条已经不在待批表里了（超时、被停止、或本来就没问过）
    pub fn allow_session(&self, id: &str) -> bool {
        let (key, label) = match self.staged().remove(id) {
            Some(rule) => rule,
            None => return false,
        };
        // 同一份动作被记两次时后一次的标签胜出：它们是同一条规则，标签只是那句话的最新版本
        self.remembered().insert(key, label);
        self.resolve(id, true);
        true
    }

    /// 撤销一条放行。返回 false = 这条键根本不在表里（界面上点了个已经不存在的按钮）
    pub fn forget(&self, key: &str) -> bool {
        self.remembered().remove(key).is_some()
    }

    /// 「以后都允许」：批准这一次，并把那份动作**持久化**（活过重启）。
    /// 返回 (key, label) 让调用方落盘——hub 只管内存里这一份，配置是调用方的事。
    /// 返回 None = 这条待批已经不在等了（超时、被停止、没问过）
    pub fn allow_always(&self, id: &str) -> Option<(String, String)> {
        let (key, label) = self.staged().remove(id)?;
        self.remembered().insert(key.clone(), label.clone());
        self.resolve(id, true);
        Some((key, label))
    }

    /// 启动时把配置里持久化的放行回灌进来：判定面从此与话题内规则完全同一条，
    /// `is_remembered` / `remembered_list` / `forget` / `clear` 都不必知道第二种来源
    pub fn restore(&self, rules: &[(String, String)]) {
        let mut remembered = self.remembered();
        for (key, label) in rules {
            remembered.entry(key.clone()).or_insert_with(|| label.clone());
        }
    }

    /// 话题内已记住的条数：设置页要说得出"现在有几条动作不用再问"，
    /// 不能让它成为看不见但会改变模型收到的东西的状态
    pub fn remembered_count(&self) -> usize {
        self.remembered().len()
    }

    /// 逐条撤销要用它。排序是为了让界面每次渲染的顺序一样——
    /// 一个 HashMap 的迭代顺序不该变成用户看得见的"上次和这次为什么不同"
    pub fn remembered_list(&self) -> Vec<(String, String)> {
        let mut rules: Vec<(String, String)> =
            self.remembered().iter().map(|(key, label)| (label.clone(), key.clone())).collect();
        rules.sort();
        rules.into_iter().map(|(label, key)| (key, label)).collect()
    }

    pub fn clear(&self) {
        self.remembered().clear();
    }

    /// 等一个 ask_user 的答案。与 `wait` 同款轮询 stop；返回 None = 停止生成。
    /// **故意不设超时**：这是用户拍板的分岔口，超时等于替用户做决定。
    /// 无人值守的话题根本不该走到这里（调用侧先拦）
    pub fn wait_answer(&self, id: &str, stop: &AtomicBool) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        self.askings().insert(id.to_string(), tx);
        let answer = loop {
            if stop.load(Ordering::Relaxed) {
                break None;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(answer) => break Some(answer),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(_) => break None,
            }
        };
        self.askings().remove(id);
        answer
    }

    /// 投一个答案。返回 false = 这条提问已经不在等了（超时不存在，只会是没问过或已停）
    pub fn resolve_answer(&self, id: &str, answer: String) -> bool {
        match self.askings().remove(id) {
            Some(tx) => {
                let _ = tx.send(answer);
                true
            }
            None => false,
        }
    }

    fn askings(&self) -> std::sync::MutexGuard<'_, HashMap<String, Sender<String>>> {
        self.askings.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Sender<bool>>> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn remembered(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.remembered.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn staged(&self) -> std::sync::MutexGuard<'_, HashMap<String, (String, String)>> {
        self.staged.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[tauri::command]
pub fn tool_decision(app: AppHandle, hub: State<'_, ApprovalHub>, id: String, approved: bool) {
    hub.resolve(&id, approved);
    // worker 回合的审批卡在子进程的 ApprovalHub 里等票：把人的决定送过去。
    // 未知 id 是幂等投递（两边各 resolve 各的），没有 worker 回合时不打扰子进程
    if crate::chat::any_worker_turn() {
        let _ = crate::agent_supervisor::global(&app).request(
            crate::agent_protocol::methods::TOOL_DECIDE,
            serde_json::json!({ "requestId": id, "approved": approved }),
        );
    }
}

/// ask_user 的回答通道：前端只能按它看到的那条提问 id 投一次，
/// 答案原文进工具结果（也就是进模型下一轮的上下文）。
#[tauri::command]
pub fn ask_user_respond(hub: State<'_, ApprovalHub>, id: String, answer: String) -> bool {
    hub.resolve_answer(&id, answer)
}

/// 批准这一次，并且同一份动作在本话题内不再问。键由后端按 id 查，前端只能传它看到的那条 id
#[tauri::command]
pub fn tool_allow_session(hub: State<'_, ApprovalHub>, id: String) -> bool {
    hub.allow_session(&id)
}

/// 「以后都允许」：批准这一次，并把那份动作写进配置——重启之后同一份动作不再问。
/// 落盘失败时这次点头仍然生效（hub 已记住），只是重启后会重新问：如实说，不装作存上了
#[tauri::command]
pub fn tool_allow_always(app: tauri::AppHandle, hub: State<'_, ApprovalHub>, id: String) -> bool {
    use tauri::Manager;
    let Some((key, label)) = hub.allow_always(&id) else {
        return false;
    };
    let mut config = crate::config::load(&app);
    if config.allow_rules.iter().any(|rule| rule.key == key) {
        return true;
    }
    config.allow_rules.push(crate::config::AllowRule {
        key: key.clone(),
        label: label.clone(),
    });
    if let Err(error) = crate::config::save(&app, &config) {
        eprintln!("「以后都允许」没写进配置（本话题内仍然生效）：{error}");
        return true;
    }
    if let Ok(root) = app.path().app_data_dir() {
        let _ = crate::audit::record(
            &root,
            crate::audit::Actor::User,
            "approval:always",
            &key,
            crate::audit::Outcome::Ok,
        );
    }
    true
}

/// 一条放行的可撤销句柄。`label` 就是用户当时在确认框里看到的那句话——
/// 没有它，"看看你允许过什么"只能报一串哈希，那不是给用户看的
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RememberedRule {
    pub key: String,
    pub label: String,
}

#[tauri::command]
pub fn tool_rules(hub: State<'_, ApprovalHub>) -> Vec<RememberedRule> {
    hub.remembered_list()
        .into_iter()
        .map(|(key, label)| RememberedRule { key, label })
        .collect()
}

/// 撤销一条放行。键由后端算，前端只能把它列出来的那条撤掉——造不出一条它没见过的规则。
/// 那条若是「以后都允许」落过盘的，配置里也一起摘掉：撤销必须活过重启，否则是假撤销
#[tauri::command]
pub fn tool_rule_forget(app: tauri::AppHandle, hub: State<'_, ApprovalHub>, key: String) -> bool {
    let removed = hub.forget(&key);
    if removed {
        let mut config = crate::config::load(&app);
        if config.allow_rules.iter().any(|rule| rule.key == key) {
            config.allow_rules.retain(|rule| rule.key != key);
            if let Err(error) = crate::config::save(&app, &config) {
                eprintln!("撤销的持久放行没从配置里摘掉（本话题内已生效）：{error}");
            }
        }
    }
    removed
}

/// 这一刻生效的权限表。列的是**判定面**而不是"谁被改过"：一张只列出被改过那几行的表，
/// 回答不了"这一行今天到底是几档"，而那正是"按项目收紧了没有"唯一能核对的地方
#[tauri::command]
pub fn permission_table(app: tauri::AppHandle) -> Vec<crate::policy::PermissionRow> {
    let config = crate::config::load(&app);
    let project = config.active_project();
    crate::policy::table(
        &config.policy(project),
        &config.permission_overrides,
        project
            .map(|item| item.permission_overrides.as_slice())
            .unwrap_or_default(),
    )
}

/// 撤销全部放行（话题内 + 「以后都允许」落过盘的都在列）。这条动作自己落一行审计：
/// 用户撤销过一次，和"从来没有过那批放行"，在账上必须是两件事
#[tauri::command]
pub fn tool_rules_clear(app: tauri::AppHandle, hub: State<'_, ApprovalHub>) -> Result<usize, String> {
    use tauri::Manager;
    let cleared = hub.remembered_count();
    hub.clear();
    // 持久的那批一起摘：列表里它们和话题规则长得一样，"清空"却只清一半就是假撤销
    let mut config = crate::config::load(&app);
    if !config.allow_rules.is_empty() {
        config.allow_rules.clear();
        if let Err(error) = crate::config::save(&app, &config) {
            eprintln!("清空的持久放行没从配置里摘掉（本话题内已生效）：{error}");
        }
    }
    crate::audit::record(
        &app.path().app_data_dir().map_err(|e| e.to_string())?,
        crate::audit::Actor::User,
        "tool_rules:clear",
        &format!("{cleared} 条"),
        crate::audit::Outcome::Ok,
    )?;
    Ok(cleared)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 逐条撤销要能认出"哪一条"：标签从待批那一刻一路带进列表，
    /// 而且撤销一条不该顺手把另一条也清掉
    #[test]
    fn a_remembered_rule_keeps_its_label_and_forgetting_one_leaves_the_other() {
        let hub = ApprovalHub::default();
        hub.stage("call-a", "key-a", "git push origin main");
        hub.stage("call-b", "key-b", "写 C:/work/notes.md");
        assert!(hub.allow_session("call-a"));
        assert!(hub.allow_session("call-b"));

        let rules = hub.remembered_list();
        assert_eq!(rules.len(), 2, "两条都该在列表里");
        assert_eq!(
            rules[0].1, "git push origin main",
            "标签得在：用户认不出自己放过哪一下的列表，不算可控：{rules:?}"
        );
        assert_eq!(rules[1].1, "写 C:/work/notes.md");
        assert!(hub.forget("key-a"), "按那条自己的键撤销它");
        assert!(!hub.forget("key-a"), "同一条不该能撤销两次");
        assert!(hub.is_remembered("key-b"), "撤销一条不该顺手把另一条也清掉");
        assert_eq!(hub.remembered_count(), 1);
    }

    fn wait_in_thread(hub: &ApprovalHub, id: &str, timeout: Duration) -> std::thread::JoinHandle<bool> {
        let hub = hub.clone();
        let id = id.to_string();
        std::thread::spawn(move || hub.wait(&id, timeout, &AtomicBool::new(false)))
    }

    /// 前端只可能在 sender 已经登记之后投票；等到它出现再投，测试才不是在测运气
    fn await_pending(hub: &ApprovalHub, id: &str) {
        for _ in 0..200 {
            if hub.lock().contains_key(id) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("等线程登记 {id} 超时");
    }

    #[test]
    fn a_remembered_rule_covers_that_action_and_nothing_else() {
        let hub = ApprovalHub::default();
        hub.remembered().insert("push-main".to_string(), "git push origin main".into());
        assert!(hub.is_remembered("push-main"));
        assert!(
            !hub.is_remembered("push-dev"),
            "换个分支就是另一个动作，不能共用一次点头"
        );
        assert_eq!(hub.remembered_count(), 1);
        assert!(hub.forget("push-main"));
        assert!(!hub.is_remembered("push-main"));
        hub.remembered().insert("push-dev".to_string(), "git push origin dev".into());
        hub.clear();
        assert_eq!(hub.remembered_count(), 0, "一键清空要真的清空，不然它只是看起来清空了");
    }

    #[test]
    fn the_first_vote_wins_and_a_stale_vote_is_forgiven() {
        let hub = ApprovalHub::default();
        let waiter = wait_in_thread(&hub, "call-1", Duration::from_secs(5));
        await_pending(&hub, "call-1");
        hub.resolve("call-1", true);
        assert!(waiter.join().unwrap(), "批准要传回 true");
        // 已经没人等了：这一票必须被忘掉，而不是把某个后来的请求顶掉
        hub.resolve("call-1", false);
        assert!(hub.lock().is_empty());
    }

    /// 「以后都允许」与启动回灌：allow_always 批准并交出 (key,label) 供调用方落盘；
    /// restore 把落过盘的规则灌回同一张表——判定面从此只有一份，撤销口也只有一个
    #[test]
    fn allow_always_hands_out_the_key_and_restore_fills_the_same_table() {
        let hub = ApprovalHub::default();
        hub.stage("call-x", "key-x", "npm run build");
        let (key, label) = hub.allow_always("call-x").expect("待批的那条要能被允许");
        assert_eq!(key, "key-x");
        assert_eq!(label, "npm run build");
        assert!(hub.is_remembered("key-x"));

        // 新进程：落盘的规则灌回来，判定与列表跟话题内规则长在同一张表上
        let fresh = ApprovalHub::default();
        fresh.restore(&[(key.clone(), label.clone())]);
        assert!(fresh.is_remembered("key-x"));
        assert_eq!(
            fresh.remembered_list(),
            vec![("key-x".to_string(), "npm run build".to_string())]
        );

        // 没问过的 id 投不出去，也不留下规则
        let fresh = ApprovalHub::default();
        assert!(fresh.allow_always("never-asked").is_none());
        assert_eq!(fresh.remembered_count(), 0);
    }

    #[test]
    fn pressing_stop_wakes_an_waiting_approval_without_paying_the_timeout() {        let hub = ApprovalHub::default();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let id = "call-2".to_string();
        let waiter = {
            let hub = hub.clone();
            std::thread::spawn(move || hub.wait(&id, Duration::from_secs(10), &flag))
        };
        await_pending(&hub, "call-2");
        let started = Instant::now();
        stop.store(true, Ordering::Relaxed);
        assert!(!waiter.join().unwrap(), "停止生成时按不批准处理");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "按了停止还要干等满 10 分钟超时，就是这条注释要防的那件事"
        );
    }

    #[test]
    fn allowing_a_session_needs_the_pending_id_not_a_forged_key() {
        let hub = ApprovalHub::default();
        // 没问过任何东西的 id：这一票投不出去，也不该凭空生出一条规则
        assert!(!hub.allow_session("never-asked"), "凭一个假 id 造不出永久放行");
        assert_eq!(hub.remembered_count(), 0);

        let waiter = wait_in_thread(&hub, "call-3", Duration::from_secs(5));
        await_pending(&hub, "call-3");
        hub.stage("call-3", "key-of-call-3", "rm -rf ./target");
        assert!(hub.allow_session("call-3"), "在待批的那一条上点批准，要真的把票投出去");
        assert!(waiter.join().unwrap(), "记住并批准必须是同一次动作，不能还要再点一次");
        assert_eq!(hub.remembered_count(), 1, "记住的必须是那份动作的键");
        assert!(hub.is_remembered("key-of-call-3"));

        // 第二次同样动作：判定侧读 is_remembered 就放行了，这里核对的是对照表已经清了
        assert!(!hub.allow_session("call-3"), "同一次询问不能被投两次票，对照表也不能留着");
    }

    #[test]
    fn a_resolved_question_does_not_leave_its_key_behind() {
        let hub = ApprovalHub::default();
        let waiter = wait_in_thread(&hub, "call-4", Duration::from_secs(5));
        await_pending(&hub, "call-4");
        hub.stage("call-4", "key-of-call-4", "写 C:/work/notes.md");
        hub.resolve("call-4", false);
        assert!(!waiter.join().unwrap());
        assert!(!hub.is_remembered("key-of-call-4"), "摇头不该留下任何规则");
        // 对照表随这次询问一起清掉：下一次同 id 的询问必须重新登记自己的键
        assert!(!hub.allow_session("call-4"), "已经结束的询问不该被补一票");
    }

    /// ask_user 的等待面：答案原文按 id 送达；停止生成能立刻唤醒；已结束的提问收不到补投
    #[test]
    fn an_asked_question_delivers_its_answer_text_and_survives_until_then() {
        let hub = ApprovalHub::default();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let waiter = {
            let hub = hub.clone();
            std::thread::spawn(move || hub.wait_answer("ask-1", &flag))
        };
        for _ in 0..200 {
            if !hub.askings().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            hub.resolve_answer("ask-1", "用方案 A".into()),
            "没问过的 id 投不进答案"
        );
        assert_eq!(
            waiter.join().unwrap(),
            Some("用方案 A".into()),
            "答案原文要原样送达，审批那张表投的是布尔，这里投的是话"
        );
        assert!(!hub.resolve_answer("ask-1", "补投".into()), "同一次提问不能被回答两次");
    }

    #[test]
    fn pressing_stop_wakes_a_waiting_question_without_an_answer() {
        let hub = ApprovalHub::default();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let waiter = {
            let hub = hub.clone();
            std::thread::spawn(move || hub.wait_answer("ask-2", &flag))
        };
        for _ in 0..200 {
            if !hub.askings().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, Ordering::Relaxed);
        assert_eq!(waiter.join().unwrap(), None, "停止生成时问题按没人回答收场");
        assert!(hub.askings().is_empty(), "醒过来之后等待表里不该留着那条");
    }
}
