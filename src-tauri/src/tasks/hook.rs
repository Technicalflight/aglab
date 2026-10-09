//! Outbound webhook：一次运行有结论之后，往用户自己填的地址 POST 一发。
//!
//! 三条底线，每条都对应一个真实的坏法：
//! 1. **投递失败不改运行结论。** 通知是通知，不是授权也不是事实——跑成什么样在收尾那一行里已经定了。
//! 2. **每发都带 HMAC 签名。** 拿不到签名密钥就**不发**，而不是发一发没签名的：
//!    收端一旦分不清来路，这条通道就成了"任何人打个 POST 就能让本机以为是你派来的"。
//! 3. **幂等键就是 `run_id`。** 重试与收端重推都不会把一次事件变成两次。
//!
//! 明文 http 只放行回环地址：签名头跑到公网等于把密钥送出去。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::policy::{Capability, NetScope, Policy};
use crate::tasks::escalate;

/// 一共投几次。只重试网络层，对端给 4xx/5xx 是它的判断，再试只是在替它多收一遍
pub const MAX_ATTEMPTS: u8 = 3;
/// 单次投递的墙钟上限。调度线程不该被一个不在听的收端挂住
const TIMEOUT_MS: u64 = 5_000;

/// 一次投递的结果。它进账本，因为"这条通知到底发出去了没"是一次运行的事实
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Delivery {
    pub sent: bool,
    /// 对端状态码。没发出去时是 0
    pub status: u16,
    /// 实际试了几次
    pub attempts: u8,
    /// 为什么没发、或者为什么没成。这句是要给人看的，不能只在日志里
    pub note: String,
}

/// 这个任务配了通知地址吗。空串 = 不投，也不报错
pub fn wanted(url: &str) -> bool {
    !url.trim().is_empty()
}

/// 收信方是不是这台机器自己。判据与出口名单读的是同一个"哪一家"（`egress::host_of`），
/// 不另写一份 URL 解析——两处解析就会对同一个地址给出两个答案
pub fn loopback(url: &str) -> bool {
    let host = crate::egress::strip_port(&crate::egress::host_of(url));
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]")
}

/// 地址能不能投。https 全放，http 只放回环——其余一律拒
pub fn allowed(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("https://") {
        return !rest.is_empty();
    }
    lower.starts_with("http://") && loopback(url)
}

/// 过表的结论。`Held` 那句是要写进投递记录给人看的
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clearance {
    Go,
    Held { reason: String },
}

/// 往外发东西之前的两道闸。顺序是有意义的：**名单在前，表在后**——
/// 一份可以被点通过的名单不是名单，所以它根本不进队列（design-security-permission.md §16）。
///
/// 表那一行 `net.configured` 的三种答案处理不同：
/// - **放行**（默认就是这条：这个地址是用户在任务里自己点过名的）
/// - **表上禁止**：不发，也**不挂待批**——把一条被禁止的动作挂进队列，等于给用户一个
///   把它点通过的入口（与 [`escalate::settle`] 处理 `Deny` 时同一条规矩）
/// - **要问**：不发，但在队列里留下一条可处理的现场。人批了之后走的是"同一发下次放行"
///   那条路，而那条路现在可撤回（design-security-permission.md §14）
pub fn preflight(
    policy: &Policy,
    allow: &[String],
    queue: &mut escalate::Queue,
    run_id: &str,
    task_id: &str,
    conversation_id: &str,
    url: &str,
    now: i64,
) -> Clearance {
    if let Err(reason) = crate::egress::guard(allow, url) {
        return Clearance::Held { reason };
    }
    let host = crate::egress::host_of(url);
    // 回环与非回环在表上是两行不同的判定。此前两条都去问 `net.configured`，
    // 于是 `net.localhost` 那一行没人 resolve：收紧它的人以为自己拦住了什么，其实没有
    let scope = if loopback(url) {
        NetScope::Localhost
    } else {
        NetScope::Configured
    };
    let cap = Capability::Net { scope };
    let key = cap.key();
    let decision = policy.check(
        &cap,
        &host,
        &crate::policy::fingerprint(&[key.as_str(), &host]),
    );
    match escalate::settle(
        queue,
        &escalate::Request {
            run_id: run_id.to_string(),
            task_id: task_id.to_string(),
            conversation_id: conversation_id.to_string(),
            capability: key,
            target: host,
        },
        &decision,
        now,
    ) {
        escalate::Gate::Execute => Clearance::Go,
        escalate::Gate::Refused { reason } => Clearance::Held { reason },
        escalate::Gate::Parked(item) => Clearance::Held {
            reason: item.reason,
        },
    }
}

/// 发出去的那一份正文。`idempotencyKey` 让收端能自己去重
pub fn body(
    run_id: &str,
    conversation_id: &str,
    task_id: &str,
    task_name: &str,
    status: &str,
    error: &str,
) -> String {
    serde_json::json!({
        "idempotencyKey": run_id,
        "runId": run_id,
        "conversationId": conversation_id,
        "taskId": task_id,
        "taskName": task_name,
        "status": status,
        "error": error,
    })
    .to_string()
}

/// HMAC-SHA256（RFC 2104），小写 hex。依赖里没有 `hmac` crate，而 HMAC 是二十行的事，
/// 加一个依赖换二十行不划算——何况这一份能用已知答案直接测
pub fn signature(secret: &str, message: &str) -> String {
    const BLOCK: usize = 64;
    let mut key = secret.as_bytes().to_vec();
    if key.len() > BLOCK {
        key = Sha256::digest(&key).to_vec();
    }
    key.resize(BLOCK, 0);
    let inner: Vec<u8> = key.iter().map(|byte| byte ^ 0x36).collect();
    let outer: Vec<u8> = key.iter().map(|byte| byte ^ 0x5c).collect();

    let mut first = Sha256::new();
    first.update(&inner);
    first.update(message.as_bytes());
    let folded = first.finalize();

    let mut second = Sha256::new();
    second.update(&outer);
    second.update(folded);
    second
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 真投递。返回的 `Delivery` 永远不为"失败"而 panic，也不返回 Err：调用方只关心要不要记账。
/// `proxy` 是全局绑定解析出的那条路（None = 直连），由调用方解析好递进来
pub fn deliver(
    url: &str,
    body: &str,
    proof: &str,
    idempotency: &str,
    proxy: Option<&str>,
) -> Delivery {
    if !allowed(url) {
        return Delivery {
            sent: false,
            status: 0,
            attempts: 0,
            note: "只投 https（本机回环允许 http）。这条地址被拒了。".into(),
        };
    }
    let parsed_proxy = match proxy.map(str::trim).filter(|url| !url.is_empty()) {
        Some(url) => match ureq::Proxy::new(url) {
            Ok(parsed) => Some(parsed),
            Err(error) => {
                return Delivery {
                    sent: false,
                    status: 0,
                    attempts: 0,
                    note: format!("代理地址「{url}」不合法，这条没发出去：{error}"),
                };
            }
        },
        None => None,
    };
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_millis(TIMEOUT_MS)))
        .proxy(parsed_proxy)
        .build()
        .new_agent();
    let mut attempts = 0u8;
    let mut last = String::new();
    while attempts < MAX_ATTEMPTS {
        attempts += 1;
        match agent
            .post(url)
            .header("content-type", "application/json")
            .header("x-aglab-signature", proof)
            .header("idempotency-key", idempotency)
            .send(body)
        {
            Ok(response) => {
                let status = response.status().as_u16();
                let okay = (200..300).contains(&status);
                return Delivery {
                    sent: true,
                    status,
                    attempts,
                    note: if okay {
                        String::new()
                    } else {
                        format!("对端给了 {status}，没有重试：那是它的判断，不是链路断了。")
                    },
                };
            }
            Err(error) => last = format!("{error}"),
        }
    }
    Delivery {
        sent: false,
        status: 0,
        attempts,
        note: format!("投了 {attempts} 次都没送达：{last}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4231 的 case 2。拿已知答案测自己写的 HMAC，而不是"看起来对"：
    /// 签名错一位，收端会把它当成伪造请求丢掉，而这边只看到"发过了"
    #[test]
    fn hmac_matches_the_published_test_vector() {
        assert_eq!(
            signature("Jefe", "what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn an_overlong_key_is_hashed_down_first() {
        // 只测"它是确定的"：同一个长密钥两次算出同一个值，且不等于空串
        let long = "k".repeat(200);
        assert_eq!(signature(&long, "body"), signature(&long, "body"));
        assert_eq!(signature(&long, "body").len(), 64);
        assert_ne!(signature(&long, "body"), signature(&long, "other"));
    }

    /// 明文公网等于把签名头送出去——那是密钥，不是标签
    #[test]
    fn only_https_and_the_loopback_are_worth_posting_to() {
        assert!(allowed("https://hooks.example.test/a"));
        assert!(allowed("http://127.0.0.1:8080/hook"));
        assert!(allowed("http://localhost/hook"));
        assert!(!allowed("http://hooks.example.test/a"));
        assert!(!allowed("file:///C:/windows/temp.txt"));
        assert!(!allowed("https://"));
        assert!(!allowed(""));
    }

    #[test]
    fn a_refused_address_never_reaches_the_network() {
        let shot = deliver("http://evil.example.test/", "{}", "sig", "run-1", None);
        assert!(!shot.sent);
        assert_eq!(shot.attempts, 0, "地址不合规时一次都不该试");
        assert!(shot.note.contains("https"), "要说清为什么拒：{}", shot.note);
    }

    #[test]
    fn the_body_carries_the_run_id_as_the_idempotency_key() {
        let text = body("run-9", "conv-9", "t1", "摘要", "ok", "");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("正文该是合法 JSON");
        assert_eq!(parsed["idempotencyKey"], "run-9");
        assert_eq!(
            parsed["conversationId"], "conv-9",
            "话题 id 不该被 run id 顶掉"
        );
        assert_eq!(parsed["status"], "ok");
    }

    /// 默认这一行放行（地址是用户在任务里自己填的），但它**能被收紧**且收紧之后真的生效；
    /// 而"禁止"那一档不许挂成待批——那等于给用户一个把禁止点成通过的入口
    #[test]
    fn the_net_row_passes_by_default_and_bites_once_tightened() {
        use crate::policy::{Level, Mode};
        let url = "https://hooks.example.test/run";
        let mut queue = escalate::Queue::default();
        assert_eq!(
            preflight(
                &Policy::new(Mode::Ask),
                &[],
                &mut queue,
                "run-1",
                "t1",
                "conv-1",
                url,
                1_000
            ),
            Clearance::Go,
            "ask 档下这一行默认放行：这地址是用户点过名的"
        );
        assert!(queue.items.is_empty(), "放行的一发不该留下任何东西");

        let asking = Policy {
            mode: Mode::Ask,
            overrides: vec![("net.configured".into(), Level::Ask)],
            phase: crate::policy::Phase::Chat,
            delete_batch_ask: 50,
            file_rules: Vec::new(),
            command_blocklist: Vec::new(),
            command_rules: Vec::new(),
            network_rules: Vec::new(),
            net_http_remote: crate::file_rules::RuleAction::Ask,
            net_http_local: crate::file_rules::RuleAction::Allow,
        };
        let held = preflight(
            &asking,
            &[],
            &mut queue,
            "run-1",
            "t1",
            "conv-1",
            url,
            2_000,
        );
        assert!(
            matches!(held, Clearance::Held { .. }),
            "收紧成问人之后这一发不该发出去：{held:?}"
        );
        assert_eq!(
            queue.waiting().len(),
            1,
            "要问的那一发得留下一条可处理的现场"
        );
        if let Clearance::Held { reason } = held {
            assert!(!reason.contains("/run"), "那句理由只该说到这家：{reason}");
        }

        let id = queue.waiting()[0].id.clone();
        queue.decide(&id, true, 3_000).expect("表台");
        assert!(queue.waiting().is_empty(), "批过的不该再算待办");
        assert_eq!(
            preflight(
                &asking,
                &[],
                &mut queue,
                "run-2",
                "t1",
                "conv-1",
                url,
                4_000
            ),
            Clearance::Go
        );
        assert_eq!(queue.items.len(), 1, "放行之后不该再挂一条新的");

        assert!(
            matches!(
                preflight(
                    &asking,
                    &[],
                    &mut queue,
                    "run-3",
                    "t1",
                    "conv-1",
                    "https://other.test/run",
                    5_000
                ),
                Clearance::Held { .. }
            ),
            "一次批准只覆盖确认过的那一家"
        );
        assert_eq!(queue.items.len(), 2);

        let denied = Policy {
            mode: Mode::Ask,
            overrides: vec![("net.configured".into(), Level::Deny)],
            phase: crate::policy::Phase::Chat,
            delete_batch_ask: 50,
            file_rules: Vec::new(),
            command_blocklist: Vec::new(),
            command_rules: Vec::new(),
            network_rules: Vec::new(),
            net_http_remote: crate::file_rules::RuleAction::Ask,
            net_http_local: crate::file_rules::RuleAction::Allow,
        };
        assert!(matches!(
            preflight(
                &denied,
                &[],
                &mut queue,
                "run-4",
                "t1",
                "conv-1",
                url,
                6_000
            ),
            Clearance::Held { .. }
        ));
        assert_eq!(
            queue.items.len(),
            2,
            "被表禁止的那一发不许挂成待批：那是个把禁止点成通过的入口（与 settle 同一条规矩）"
        );
    }

    /// 回环与非回环在表上是两行不同的判定。此前两条都去问 `net.configured`，
    /// 于是 `net.localhost` 那一行没人 resolve：划红线的人以为本机那一条也断了，其实一次也没断
    #[test]
    fn a_loopback_target_is_judged_by_its_own_row_of_the_table() {
        use crate::policy::{Level, Mode};
        let here = "http://127.0.0.1:8787/hook";
        let there = "https://hooks.example.test/run";
        let mut queue = escalate::Queue::default();

        // 默认档：本机那一行是 Allow（今天真实发生的事就是不问就投），所以两头发得出去
        let open = Policy::new(Mode::Ask);
        assert_eq!(
            preflight(&open, &[], &mut queue, "run-1", "t1", "c1", here, 1_000),
            Clearance::Go,
            "默认档不该拦住一发本机钩子"
        );
        assert_eq!(
            preflight(&open, &[], &mut queue, "run-2", "t1", "c1", there, 2_000),
            Clearance::Go
        );

        // 只收紧本机那一行：外面的照旧，回环的停下来
        let only_local = Policy {
            mode: Mode::Ask,
            overrides: vec![("net.localhost".into(), Level::Deny)],
            phase: crate::policy::Phase::Chat,
            delete_batch_ask: 50,
            file_rules: Vec::new(),
            command_blocklist: Vec::new(),
            command_rules: Vec::new(),
            network_rules: Vec::new(),
            net_http_remote: crate::file_rules::RuleAction::Ask,
            net_http_local: crate::file_rules::RuleAction::Allow,
        };
        assert!(
            matches!(
                preflight(
                    &only_local,
                    &[],
                    &mut queue,
                    "run-3",
                    "t1",
                    "c1",
                    here,
                    3_000
                ),
                Clearance::Held { .. }
            ),
            "划了红线还在往本机端口投"
        );
        assert_eq!(
            preflight(
                &only_local,
                &[],
                &mut queue,
                "run-4",
                "t1",
                "c1",
                there,
                4_000
            ),
            Clearance::Go,
            "那一行只管回环，别把外网的也一起断了"
        );
        assert!(queue.items.is_empty(), "表上禁止的那一发不许挂成待批");
    }

    /// IPv6 的回环写法带着方括号。旧的那份内联解析在第一个 `:` 上切开，于是 `[::1]`
    /// 永远对不上，`allowed` 会把一台本机服务当成公网拒掉。现在两件事读的是同一个 host
    #[test]
    fn the_ipv6_loopback_is_still_the_machine_itself() {
        assert!(loopback("http://[::1]:8787/hook"));
        assert!(allowed("http://[::1]:8787/hook"));
        assert!(
            !loopback("http://2001:db8::1/hook"),
            "一个公网 v6 地址不是这台机器"
        );
        assert!(!allowed("http://[::2]:8787/hook"), "::2 不是回环");
    }

    /// 两道闸都得过，而且是**名单在前**：一份可以被点通过的名单不是名单，
    /// 所以被它拦下的那一发连待批都不该留下（design-security-permission.md §16）
    #[test]
    fn the_egress_list_vetoes_a_shot_the_table_would_have_let_go() {
        use crate::policy::Mode;
        let url = "https://hooks.example.test/run";
        let mut queue = escalate::Queue::default();
        let open = Policy::new(Mode::Ask);
        let outside = vec!["other.test".to_string()];

        assert!(matches!(
            preflight(&open, &outside, &mut queue, "run-1", "t1", "conv-1", url, 1_000),
            Clearance::Held { .. }
        ));
        assert!(
            queue.items.is_empty(),
            "被名单拦下的那一发挂成待批，等于给用户一个把它点通过的入口"
        );

        let inside = vec!["example.test".to_string()];
        assert_eq!(
            preflight(&open, &inside, &mut queue, "run-2", "t1", "conv-1", url, 2_000),
            Clearance::Go,
            "名单覆盖到这家、表也放行——那就照旧发出去"
        );
        assert!(
            matches!(
                preflight(
                    &open,
                    &inside,
                    &mut queue,
                    "run-3",
                    "t1",
                    "conv-1",
                    "https://x.other.test/run",
                    3_000
                ),
                Clearance::Held { .. }
            ),
            "名单管得到的那一家之外，一家都不许多发"
        );
    }
}
