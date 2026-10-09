//! 网络出口的目标域名单（design-security-permission.md §16）。
//!
//! 一句话：一份"这台机器允许把字节发到哪几家"的名单。空 = 不收紧，那正是它落地前的行为。
//!
//! 它**不是**权限表的另一行：表上那一行（`net.configured`）回答"这一发要不要问用户"，
//! 这一份回答"这一发有没有可能问都不该问"。所以它拦下来的东西不进待批队列——
//! 一份可以被点通过的名单不是名单。
//!
//! 只覆盖**目标主机已知**的出口（模型请求、模型清单、outbound webhook、模型的
//! web_fetch/browser/open_path）。MCP 服务器是 stdio 子进程，它要连哪儿是那个
//! 进程自己的事，域名名单与规则表都管不到它——这条边界不装做不存在。
//!
//! 网络安全规则（design-security-center.md D5）住在同一份解析上：一张有序的
//! 「域后缀 → 动作」表，先于现行判定；未命中一字不改落回现行档。规则可以问、
//! 可以放、可以拒——但**最外圈的名单与私网拒绝照旧在它之外**：规则放行的
//! 一次出站，仍然要过名单与 SSRF 那两道物理闸。

use serde::{Deserialize, Serialize};

use crate::file_rules::RuleAction;

/// 一条网络规则：域后缀 + 命中后的动作。条目可以粘整条 URL（同一套 host 解析拆它）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NetworkRule {
    /// 域名后缀。`example.com` 覆盖 `api.example.com`、**不**覆盖 `notexample.com`；
    /// IP 走精确匹配（后缀必须整段对齐）
    pub pattern: String,
    pub action: RuleAction,
}

/// 首条命中即停（按表序）。匹配语义与 [`permitted`] 完全一致——同一份 host 解析、
/// 同一套后缀口径，两张表不各说各话
pub fn rule_hit(rules: &[NetworkRule], url: &str) -> Option<RuleAction> {
    let host = host_of(url);
    if host.is_empty() {
        return None;
    }
    let target = strip_port(&host);
    if target.is_empty() {
        return None;
    }
    for rule in rules {
        let entry = strip_port(&host_of(&rule.pattern));
        if entry.is_empty() {
            continue;
        }
        if target == entry || target.ends_with(&format!(".{entry}")) {
            return Some(rule.action);
        }
    }
    None
}

/// 入库校验：空条目永远命不中任何一次判定，存一条装饰不如当场拒掉
pub fn validate_rules(rules: &[NetworkRule]) -> Result<(), String> {
    for (index, rule) in rules.iter().enumerate() {
        if host_of(&rule.pattern).trim().is_empty() {
            return Err(format!(
                "第 {} 条网络规则的域名是空的（{}）",
                index + 1,
                rule.pattern
            ));
        }
    }
    Ok(())
}

/// 用户配置的可信端点（搜索实例等）的最低限度校验：只收 http/https、host 非空。
/// 私网/回环**不在此拒**——那类地址能不能用由端点的信任级决定（模型 baseUrl 的
/// 同一条先例：localhost 的 Ollama/中转天天在用），这里只挡"压根不是 URL"的配置
pub fn require_http_url(url: &str, what: &str) -> Result<(), String> {
    let trimmed = url.trim();
    let lowered = trimmed.to_ascii_lowercase();
    if lowered.starts_with("http://") || lowered.starts_with("https://") {
        if host_of(trimmed).is_empty() {
            return Err(format!("{what}缺 host：{url}"));
        }
        return Ok(());
    }
    Err(format!("{what}只认 http/https：{url}"))
}

/// 从 URL 里取出"这一发要发到**哪一家**"：host + 端口，小写，不带 scheme、路径、查询，
/// 也不带 `user:token@`。
///
/// 它是这台机器上唯一的一份 URL→主机 的解析：凭据常常写在钩子地址的 path/query 里，
/// 而这一串要进表、进审计 detail、进待批文案——拿整条 URL 当 target，
/// 等于替用户把凭据贴到界面上和账本里
pub fn host_of(url: &str) -> String {
    let trimmed = url.trim();
    let after_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    host.to_ascii_lowercase()
}

/// 名单里有没有哪一条覆盖这个主机。规则只有一条：**按域后缀**匹配，忽略大小写与端口。
///
/// `example.com` 覆盖 `api.example.com`，**不**覆盖 `notexample.com`（差一个点就是另一家）。
/// 不提供 `*` 通配：想"整条都能出去"的正确写法是把名单清空，而不是留一个谁都能匹配的字符串。
/// IP 走的是同一条规则，而它落出来的就是精确匹配——后缀必须整段对齐，所以 `10.0.0.1` 覆盖不到 `210.0.0.1`。
///
/// 空白条目不算数，所以"一个名字都没写"与"什么都没填"是同一种状态：不收紧。
/// 界面上那个文本框清空之后交下来的就是全空白——那种时候把用户锁在门外是最坏的错法
pub fn permitted(allow: &[String], host: &str) -> bool {
    let named: Vec<String> = allow
        .iter()
        .map(|entry| strip_port(&host_of(entry)))
        .filter(|entry| !entry.is_empty())
        .collect();
    if named.is_empty() {
        return true;
    }
    let target = strip_port(&host.to_ascii_lowercase());
    named
        .into_iter()
        .any(|entry| target == entry || target.ends_with(&format!(".{entry}")))
}

/// 这一发能不能出去。不能就给一句说得出主机的话——被自己的名单锁在门外时，
/// 最需要看得见的就是"是哪一家不在名单里"
pub fn guard(allow: &[String], url: &str) -> Result<(), String> {
    let host = host_of(url);
    if permitted(allow, &host) {
        return Ok(());
    }
    Err(format!(
        "出口被拦下：{host} 不在网络出口的域名名单里。要放行它，去设置 → 权限 → 出口域名名单；名单清空 = 不收紧。"
    ))
}

/// web_fetch 的 SSRF 闸（design-tool-capability-gaps.md §3.3）：出口名单回答
/// 「这一发该不该出去」，这道闸回答「内网地址根本不许成为目标」。GET 的
/// query string 是出站数据，`http://localhost/...?secret=…` 是一条真实的
/// 外泄通道，也是一次内网探测。域名会真解析一遍——解析到私网段的域名
/// 与私网字面量一并拒（v1 不防 DNS rebinding：那要把解析结果钉死到连接上）
pub fn refuse_private_target(url: &str) -> Result<(), String> {
    use std::net::{IpAddr, ToSocketAddrs};
    let host_with_port = host_of(url);
    let host = strip_port(&host_with_port);
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let port: u16 = if url.trim().to_ascii_lowercase().starts_with("https") {
        443
    } else {
        80
    };

    let mut resolved: Vec<IpAddr> = Vec::new();
    match bare.parse::<IpAddr>() {
        // IP 字面量直接判，不用解析
        Ok(ip) => resolved.push(ip),
        Err(_) => {
            let lower = bare.to_ascii_lowercase();
            if lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local") {
                return Err(format!("「{bare}」是本机/内网地址，web_fetch 到不了那里。"));
            }
            resolved = (bare, port)
                .to_socket_addrs()
                .map_err(|e| format!("解析 {bare} 失败：{e}"))?
                .map(|addr| addr.ip())
                .collect();
        }
    }
    if resolved.is_empty() {
        return Err(format!("解析 {bare} 没得到任何地址，没有发起连接。"));
    }
    for ip in &resolved {
        if is_private_ip(*ip) {
            return Err(format!("{bare} 指向内网地址 {ip}，web_fetch 到不了那里。"));
        }
    }
    Ok(())
}

/// 回环、私网、链路本地、未指定地址都算"内网"。IPv6 的 unique local（fc00::/7）
/// 与 link-local（fe80::/10）std 还没给稳定判据，按位段手判
fn is_private_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr::{V4, V6};
    match ip {
        V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified(),
        V6(v6) => {
            let seg = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || (seg[0] & 0xffc0) == 0xfe80
                || (seg[0] & 0xfe00) == 0xfc00
        }
    }
}

/// 去掉端口。IPv6 写作 `[::1]:8080`，方括号是地址的一部分，只剥端口
pub(crate) fn strip_port(host: &str) -> String {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        return format!("[{}]", rest.split(']').next().unwrap_or_default());
    }
    host.split(':').next().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| entry.to_string()).collect()
    }

    /// 域后缀：`example.com` 覆盖自己与子域，但不覆盖 `notexample.com`。
    /// 那一条差一个点的规则是这份名单唯一的安全边界，写成 `contains` 就全泄了
    #[test]
    fn a_domain_covers_its_subdomains_and_not_lookalikes() {
        let allow = list(&["example.com"]);
        assert!(permitted(&allow, "example.com"));
        assert!(permitted(&allow, "api.example.com"));
        assert!(permitted(&allow, "A.B.Example.COM"));
        assert!(
            !permitted(&allow, "notexample.com"),
            "差一个点就是另一家：它不该被覆盖"
        );
        assert!(
            !permitted(&allow, "example.com.evil.test"),
            "后缀反过来拼也不是这家"
        );
    }

    /// 主机名带端口是常态（钩子地址尤其），匹配只看那一串域
    #[test]
    fn ports_are_not_part_of_the_domain() {
        let allow = list(&["example.com"]);
        assert!(permitted(&allow, "api.example.com:8443"));
        assert!(permitted(&allow, "example.com:443"));
        assert!(!permitted(&allow, "evil.test:443"));
    }

    /// web_fetch 的 SSRF 闸：回环/私网/链路本地一律拒（含解析后的域名），
    /// 公网字面量放行。测试只用字面量与本地名，不碰真 DNS
    #[test]
    fn the_private_target_gate_refuses_intranet_and_allows_public() {
        assert!(refuse_private_target("http://localhost/x").is_err());
        assert!(refuse_private_target("http://api.localhost/x").is_err());
        assert!(refuse_private_target("http://127.0.0.1:8080/x").is_err());
        assert!(refuse_private_target("http://10.1.2.3/x").is_err());
        assert!(refuse_private_target("http://192.168.1.1/x").is_err());
        assert!(refuse_private_target("http://172.16.0.1/x").is_err());
        assert!(refuse_private_target("http://169.254.1.1/x").is_err());
        assert!(refuse_private_target("http://0.0.0.0/x").is_err());
        assert!(refuse_private_target("http://[::1]/x").is_err());
        assert!(refuse_private_target("http://[fe80::1]/x").is_err());
        assert!(
            refuse_private_target("http://8.8.8.8/x").is_ok(),
            "公网 IP 字面量不该被拦"
        );
        // 主机名带端口：剥端口之后判地址
        assert!(refuse_private_target("http://10.0.0.9:8123/x").is_err());
    }

    /// `strip_port` 自己说过"方括号是地址的一部分"，那它交出去的就得是一个闭合的地址。
    /// 少掉右括号时它谁都不像：IPv6 的回环判断就是这么判错的（`tasks::hook::loopback` 是消费者之一）
    #[test]
    fn an_ipv6_host_keeps_its_brackets_when_the_port_comes_off() {
        assert_eq!(strip_port("[::1]:8080"), "[::1]");
        assert_eq!(strip_port("[::1]"), "[::1]");
        assert_eq!(strip_port("127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(strip_port("example.com"), "example.com");
    }

    /// 空名单 = 不收紧，也就是这一格落地前的行为。它必须是"什么都不拦"，
    /// 而不是"谁都拦"——加一栏配置顺手改一遍默认行为是最坏的做法
    #[test]
    fn an_empty_list_tightens_nothing() {
        assert!(permitted(&[], "anything.at.all"));
        assert!(permitted(&list(&["", "   "]), "anything.at.all"));
        assert!(guard(&[], "https://api.deepseek.com/v1").is_ok());
    }

    /// IP 不需要另一套判据：后缀必须整段对齐，于是它落出来天然就是精确匹配。
    /// 这一条钉的是那个结果（曾经有一支 `parse::<IpAddr>()` 的特判在这儿，写变异测试时
    /// 发现它对任何输入都给同一个答案，就删了）
    #[test]
    fn addresses_match_exactly_instead_of_by_suffix() {
        let allow = list(&["10.0.0.1", "[::1]:8080"]);
        assert!(permitted(&allow, "10.0.0.1"));
        assert!(permitted(&allow, "10.0.0.1:443"));
        assert!(!permitted(&allow, "210.0.0.1"));
        assert!(permitted(&allow, "[::1]"));
        assert!(permitted(&allow, "[::1]:9999"));
        assert!(!permitted(&allow, "[fe80::1]"));
    }

    /// 名单里可以直接粘一整条 URL：用户从浏览器地址栏复制来的就是那个形状。
    /// 取主机的只有一处（`host_of`），所以路径、query 与 `user:token@` 都不会被当成域
    #[test]
    fn list_entries_may_be_whole_urls() {
        let allow = list(&["https://tokener@api.example.com/v1?key=secret"]);
        assert!(permitted(&allow, "api.example.com"));
        assert!(!permitted(&allow, "v1"));
    }

    /// §15 的那条底线搬到这里仍然成立：进表的那一串不能带凭据（测试名字沿用，
    /// 它是 `hook::preflight` 与这份名单共同的取法）
    #[test]
    fn a_destination_keeps_the_host_and_leaves_the_token_behind() {
        assert_eq!(
            host_of("https://user:tok@example.com:8443/hook?a=1"),
            "example.com:8443"
        );
        assert_eq!(host_of("http://localhost:3000/x"), "localhost:3000");
        assert_eq!(host_of("  https://API.Example.com  "), "api.example.com");
        assert_eq!(host_of("not a url"), "not a url");
    }

    /// 拦下来的那句话要说得出是哪一家：被自己的名单锁在门外时，这是唯一能看懂的线索
    #[test]
    fn a_blocked_shot_names_the_host_it_blocked() {
        let reason = guard(&list(&["example.com"]), "https://api.other.test/hook").unwrap_err();
        assert!(reason.contains("api.other.test"), "{reason}");
        assert!(reason.contains("不在"), "{reason}");
    }

    /// **出口只有一个**这件事是这一格的全部结构前提：三条线协议共用 `read_events`，
    /// 闸坐在它里面，所以新增第四条协议不会天然漏掉。这条测试钉的是那个"共用"——
    /// 哪天有人绕过它再开一个 POST 出口，这里当场红，而不是多一条悄悄不过闸的出路。
    /// 代理的裁决也坐在这两个出口上（`agent_for`），绕开它就等于绕开代理绑定
    #[test]
    fn the_model_and_models_exits_are_each_a_single_call_site() {
        // 出站代码搬进 wire/（O1-2）之后，出口落在两个文件里——加总才算总数
        let source = include_str!("chat.rs").to_string()
            + include_str!("chat/wire/read.rs")
            + include_str!("chat/wire/payload.rs");
        assert_eq!(
            source.matches("agent_for(").count(),
            2,
            "POST 与 GET 两个出口都得经 proxy::agent_for 拿 Agent——\
             代理解析坐在那一份里，谁绕开它谁就漏网"
        );
        assert_eq!(
            source.matches(".post(url)").count(),
            2,
            "模型请求（read_events）与联网搜索（web_search_for_model）是仅有的两个 POST 出口，\
             两者发之前都要过 egress::guard 与 SSRF 闸；再多一个 `.post(url)` 就是那份流量在绕闸"
        );
        assert_eq!(
            source.matches(".get(&endpoint)").count(),
            1,
            "模型清单之外又开了一处 GET：它同样得问名单"
        );
    }

    // ---- 网络安全规则（design-security-center.md D5）----

    #[test]
    fn net_rules_match_by_suffix_and_stop_at_the_first_hit() {
        let rules = vec![
            NetworkRule {
                pattern: "https://api.evil.example/path".into(),
                action: RuleAction::Deny,
            },
            NetworkRule {
                pattern: "evil.example".into(),
                action: RuleAction::Ask,
            },
        ];
        // 条目粘整条 URL 也行（同一套 host 解析），子域命中更具体的头一条
        assert_eq!(
            rule_hit(&rules, "https://api.evil.example/x?y=1"),
            Some(RuleAction::Deny)
        );
        assert_eq!(
            rule_hit(&rules, "https://other.evil.example/"),
            Some(RuleAction::Ask)
        );
        // 相似名不吞：差一个点就是另一家（口径与 permitted 共用）
        assert_eq!(rule_hit(&rules, "https://evilexample.com/"), None);
        // IP 精确：后缀必须整段对齐
        let ip = vec![NetworkRule {
            pattern: "10.0.0.1".into(),
            action: RuleAction::Deny,
        }];
        assert_eq!(
            rule_hit(&ip, "http://10.0.0.1:8080/"),
            Some(RuleAction::Deny)
        );
        assert_eq!(
            rule_hit(&ip, "http://210.0.0.1/"),
            None,
            "210.0.0.1 不能被 10.0.0.1 吞掉"
        );
        // 空表 = 不收紧
        assert_eq!(rule_hit(&[], "https://anything.example/"), None);
    }

    #[test]
    fn net_rule_validation_rejects_empty_patterns() {
        assert!(validate_rules(&[NetworkRule {
            pattern: "".into(),
            action: RuleAction::Deny
        }])
        .is_err());
        assert!(validate_rules(&[NetworkRule {
            pattern: "https://ok.example".into(),
            action: RuleAction::Ask
        }])
        .is_ok());
    }
}
