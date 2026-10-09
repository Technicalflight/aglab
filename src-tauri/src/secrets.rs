//! 敏感信息判定。原本长在 `memory/record.rs` 里，只有记忆写入门禁用它。
//! 现在它是共享能力：写记忆、把内容发往网络出口、写审计行、把一发要审批的动作摆到人眼前，
//! 以及**工具结果进入话题流之前的那一道打码**（`mask_for_thread`，design-security-center.md D6）。
//!
//! 判定是"分类 + 理由"。`leaks_sensitive` 不改写；`mask_secrets` 改写，但**读同一张规则表**——
//! 两条规则各写一遍，就会出现"打码后仍然命中检测"那种没人发现的洞。
//!
//! 规则库是**命名**的（`RULES`，每条有 id 与界面名）：设置里按 id 关掉某一条，
//! 判定与打码同时生效——同一张表，不出现"检测还在、打码没了"的半开关状态。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use regex::Regex;
use serde::{Deserialize, Serialize};

/// 静态提示词：先过一遍，省掉绝大多数文本的正则开销。只有"键=值"形状的规则吃这个闸
/// （token 形状的规则自带辨识度——`sk-`、`AKIA` 这些前缀本身就是提示词），
/// 但它们不吃这个闸不代表抢跑：规则命中后给出的分类与打码口径完全一致
const HINTS: [&str; 15] = [
    "password",
    "passwd",
    "secret",
    "token",
    "api key",
    "api_key",
    "apikey",
    "private key",
    "authorization",
    "bearer ",
    "密码",
    "口令",
    "密钥",
    "令牌",
    "验证码",
];

/// 敏感检测规则库。`id` 是配置里关某条的键；`needs_hint` 的规则要先被提示词捞到才跑正则
#[derive(Debug, Clone, Copy)]
pub struct SecretRule {
    pub id: &'static str,
    pub label: &'static str,
    pub pattern: &'static str,
    pub kind: &'static str,
    pub needs_hint: bool,
}

const fn rule(
    id: &'static str,
    label: &'static str,
    pattern: &'static str,
    kind: &'static str,
    needs_hint: bool,
) -> SecretRule {
    SecretRule {
        id,
        label,
        pattern,
        kind,
        needs_hint,
    }
}

pub const RULES: [SecretRule; 13] = [
    rule(
        "kv-generic",
        "通用凭据赋值（password/token=…）",
        r"(?i)(password|passwd|secret|api[_ -]?key|access[_ -]?token|auth[_ -]?token|token)\s*[:=]\s*\S+",
        "凭据",
        true,
    ),
    rule(
        "kv-generic-cn",
        "中文凭据赋值（密码：…）",
        r"(密码|口令|密钥|令牌|验证码)\s*[:：=是为]\s*\S+",
        "凭据",
        true,
    ),
    rule(
        "bearer",
        "Bearer 认证头",
        r"(?i)bearer\s+[A-Za-z0-9._\-]{16,}",
        "凭据",
        true,
    ),
    rule(
        "openai",
        "OpenAI / DeepSeek 风格密钥（sk-…）",
        r"\bsk-[A-Za-z0-9\-_]{20,}",
        "凭据",
        false,
    ),
    rule(
        "github",
        "GitHub Token（ghp_/gho_/ghu_/ghs_/ghr_）",
        r"\bgh[posur]_[A-Za-z0-9]{30,}",
        "凭据",
        false,
    ),
    rule(
        "slack",
        "Slack Token（xox…）",
        r"\bxox[abprs]-[A-Za-z0-9\-]{10,}",
        "凭据",
        false,
    ),
    rule(
        "aws",
        "AWS Access Key（AKIA…）",
        r"\bAKIA[0-9A-Z]{16}",
        "凭据",
        false,
    ),
    rule(
        "google",
        "Google API Key（AIza…）",
        r"\bAIza[0-9A-Za-z_\-]{35}",
        "凭据",
        false,
    ),
    rule(
        "stripe",
        "Stripe 密钥（sk_live_…）",
        r"\b[sr]k_(live|test)_[0-9a-zA-Z]{16,}",
        "凭据",
        false,
    ),
    rule(
        "sendgrid",
        "SendGrid API Key（SG.…）",
        r"\bSG\.[A-Za-z0-9_\-]{16,}\.[A-Za-z0-9_\-]{16,}",
        "凭据",
        false,
    ),
    rule(
        "npm",
        "npm Token（npm_…）",
        r"\bnpm_[A-Za-z0-9]{36}",
        "凭据",
        false,
    ),
    rule(
        "pypi",
        "PyPI Token（pypi-…）",
        r"\bpypi-[A-Za-z0-9_\-]{16,}",
        "凭据",
        false,
    ),
    rule(
        "jwt",
        "JWT（三段式）",
        r"\beyJ[A-Za-z0-9_\-]{20,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
        "凭据",
        false,
    ),
];

/// 私钥的 PEM 头单独成一条：大小写都要认，且不依赖任何提示词——它结构就是自证。
/// 与其把它塞进库再解释为什么免提示词，不如让它保持独立判断的身份
const PRIVATE_KEY: &str = r"(?i)-----BEGIN [A-Z ]*PRIVATE KEY-----";

// ---- 进程级开关（design-security-center.md D6）----

static THREAD_MASK_ENABLED: AtomicBool = AtomicBool::new(true);
static DISABLED_RULES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static CUSTOM_RULES: OnceLock<Mutex<Vec<CustomSecretRule>>> = OnceLock::new();
static PATTERN_EDITS: OnceLock<Mutex<Vec<SecretRulePatternEdit>>> = OnceLock::new();

/// 用户自建的检测规则（design-security-center.md D6）：名称 + 正则。
/// id 由前端生成（`custom-` 前缀）；自建规则不吃提示词闸——用户亲手写的
/// pattern 按字面生效，这跟内置规则"先提示词省正则"的省钱逻辑不是一回事
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CustomSecretRule {
    pub id: String,
    pub label: String,
    pub pattern: String,
}

/// 对内置规则正则的改写：只许改正则，不许改名——名字是"这条是什么"的契约
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SecretRulePatternEdit {
    pub id: String,
    pub pattern: String,
}

/// 敏感保护的执行侧开关：只管**工具结果进入话题流**的那一道打码
/// （记忆门禁、MCP 参数拦截、审计脱敏是各自独立的能力，不跟这个开关走）。
/// 启动与设置页各落一次，执行侧不回读配置文件
pub fn set_scan_options(
    enabled: bool,
    disabled_rules: Vec<String>,
    custom_rules: Vec<CustomSecretRule>,
    pattern_edits: Vec<SecretRulePatternEdit>,
) {
    THREAD_MASK_ENABLED.store(enabled, Ordering::SeqCst);
    *DISABLED_RULES
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = disabled_rules;
    *CUSTOM_RULES
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = custom_rules;
    *PATTERN_EDITS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = pattern_edits;
}

fn scan_enabled() -> bool {
    THREAD_MASK_ENABLED.load(Ordering::SeqCst)
}

fn disabled_rules() -> Vec<String> {
    DISABLED_RULES
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn custom_rules() -> Vec<CustomSecretRule> {
    CUSTOM_RULES
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn pattern_edits() -> Vec<SecretRulePatternEdit> {
    PATTERN_EDITS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// 工具结果进话题流前的那一道。唯一的调用入口在 chat.rs 的 `tool_result_pair`——
/// 那里是"界面事件"与"给模型的 tool 消息"共同的源头（同源纪律），打码一次两处同文。
/// 打码发生在**第一次发出之前**：实发体与存档从头到尾都是打码后的那份，
/// 缓存前缀一致性不受影响；打码后的文本也不再命中任何规则（见 mask_secrets 的合同）
pub fn mask_for_thread(text: &str) -> String {
    if !scan_enabled() {
        return text.to_string();
    }
    let masked = mask_secrets(text);
    if masked == text {
        text.to_string()
    } else {
        format!("{masked}\n〔敏感保护〕这段内容命中了敏感信息规则，已就地打码。")
    }
}

fn category(kind: &'static str) -> &'static str {
    match kind {
        "凭据" => "疑似凭据（密钥 / token）",
        "私钥" => "疑似私钥",
        "号码" => "疑似证件号 / 银行卡号",
        "手机号" => "疑似手机号",
        other => other,
    }
}

/// 命中了哪一类敏感信息。命中即整条拒写：把带密钥的内容存成"待确认"仍然是把它落到了盘上
pub fn leaks_sensitive(text: &str) -> Option<&'static str> {
    leaks_sensitive_ex(text, &disabled_rules(), &custom_rules(), &pattern_edits())
}

/// 全量版：关闭名单 + 自建规则 + 内置正则改写都由调用方给。
/// 生产入口 [`leaks_sensitive`] 从进程级快照取后三者
pub fn leaks_sensitive_ex(
    text: &str,
    disabled: &[String],
    customs: &[CustomSecretRule],
    edits: &[SecretRulePatternEdit],
) -> Option<&'static str> {
    let lowered = text.to_ascii_lowercase();
    let looks_secretish = HINTS.iter().any(|hint| lowered.contains(hint));
    // 连续数字段：regex crate 不支持环视，所以按游程判长度，别用 \d 前后断言
    let runs: Vec<&str> = text
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|run| !run.is_empty())
        .collect();

    for rule in active_rules(disabled, customs, edits) {
        if rule.needs_hint && !looks_secretish {
            continue;
        }
        if Regex::new(&rule.pattern).is_ok_and(|re| re.is_match(text)) {
            return Some(category(rule.kind));
        }
    }
    if Regex::new(PRIVATE_KEY).is_ok_and(|re| re.is_match(text)) {
        return Some(category("私钥"));
    }
    // 15/16/19 位卡号、18 位身份证号、11 位手机号：判据与打码共用 [`number_kind`]，
    // 两边各写一遍阈值就会有一串数字"打码不拦、判定拦"或反过来
    if let Some(kind) = runs.iter().find_map(|run| number_kind(run)) {
        return Some(category(kind));
    }
    None
}

/// 生效中的规则运行时形状：内置（正则可能被改写）+ 用户自建
struct ActiveRule {
    pattern: String,
    kind: &'static str,
    needs_hint: bool,
}

fn active_rules(
    disabled: &[String],
    customs: &[CustomSecretRule],
    edits: &[SecretRulePatternEdit],
) -> Vec<ActiveRule> {
    let mut out = Vec::new();
    for rule in RULES.iter() {
        if disabled.iter().any(|off| off == rule.id) {
            continue;
        }
        let edited = edits.iter().find(|edit| edit.id == rule.id);
        // 改过正则的规则不再吃提示词闸：用户亲手改的 pattern 按字面生效——
        // 他要是把关键词那截改没了，提示词闸会把这条规则静默变成摆设
        let needs_hint = rule.needs_hint && edited.is_none();
        let pattern = edited
            .map(|edit| edit.pattern.clone())
            .unwrap_or_else(|| rule.pattern.to_string());
        out.push(ActiveRule {
            pattern,
            kind: rule.kind,
            needs_hint,
        });
    }
    for custom in customs {
        if disabled.iter().any(|off| off.as_str() == custom.id) {
            continue;
        }
        out.push(ActiveRule {
            pattern: custom.pattern.clone(),
            kind: "凭据",
            needs_hint: false,
        });
    }
    out
}

/// 一整串连续数字该按哪一类挡。判定与打码**共用这一句**：两处各写一遍阈值，
/// 就会出现"打码以为够长才算号码、判定以为不够"那种两边都绿的漏网
fn number_kind(run: &str) -> Option<&'static str> {
    if run.len() >= 15 {
        return Some("号码");
    }
    if is_phone(run) {
        return Some("手机号");
    }
    None
}

fn is_phone(run: &str) -> bool {
    run.len() == 11
        && run.starts_with('1')
        && run.as_bytes()[1].is_ascii_digit()
        && run.as_bytes()[1] >= b'3'
}

/// 把敏感那一段换成一句说明，其余**原样留着**。
///
/// 为什么需要它而 `redact_for_audit` 不够：审计可以整条丢掉，**审批的文案不行**——
/// 用户得看得懂"这一发要动谁、往哪发"，才能点头或摇头；
/// 而那一串原样落进待审批队列就是把它写到了盘上（队列在盘上，跨重启）。
///
/// 被替换的是**整段命中**（连 `token=` 那截键名一起），所以打码后的文本不再触发上面任何一条规则：
/// 只遮住值、留着 `key=value` 的形状，打码结果自己就会被 `leaks_sensitive` 判成敏感，
/// 那份"已经安全了"的假设就成了假的。这条由 `masked_text_stops_looking_sensitive` 钉住
pub fn mask_secrets(text: &str) -> String {
    mask_secrets_ex(text, &disabled_rules(), &custom_rules(), &pattern_edits())
}

/// 全量版（同 [`mask_secrets_with`] 的关系见 [`leaks_sensitive_ex`]）
pub fn mask_secrets_ex(
    text: &str,
    disabled: &[String],
    customs: &[CustomSecretRule],
    edits: &[SecretRulePatternEdit],
) -> String {
    let mut spans: Vec<(usize, usize, &'static str)> = Vec::new();
    let lowered = text.to_ascii_lowercase();
    let looks_secretish = HINTS.iter().any(|hint| lowered.contains(hint));
    for rule in active_rules(disabled, customs, edits) {
        if rule.needs_hint && !looks_secretish {
            continue;
        }
        if let Ok(re) = Regex::new(&rule.pattern) {
            spans.extend(re.find_iter(text).map(|m| (m.start(), m.end(), rule.kind)));
        }
    }
    if let Ok(re) = Regex::new(PRIVATE_KEY) {
        spans.extend(re.find_iter(text).map(|m| (m.start(), m.end(), "私钥")));
    }
    // 数字游程按字节位置扫：全是 ASCII 数字，字节下标与字符下标在这里重合
    let bytes = text.as_bytes();
    let mut at = 0usize;
    while at < bytes.len() {
        if !bytes[at].is_ascii_digit() {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && bytes[at].is_ascii_digit() {
            at += 1;
        }
        let run = &text[start..at];
        // 阈值与判定共用同一句：这条如果各写一遍，就会出现"打码放过、判定拦下"的那类静默不一致
        let Some(kind) = number_kind(run) else {
            continue;
        };
        spans.push((start, at, kind));
    }

    // 重叠的命中留最长的那一段：同一段里两条规则都抓到，写两个占位串反而把上下文切碎
    spans.sort_by_key(|(start, end, _)| (*start, std::cmp::Reverse(*end)));
    let mut merged: Vec<(usize, usize, &'static str)> = Vec::new();
    for (start, end, kind) in spans {
        if merged
            .last()
            .is_some_and(|(_, last_end, _)| *last_end > start)
        {
            continue;
        }
        merged.push((start, end, kind));
    }
    let mut out = text.to_string();
    for (start, end, kind) in merged.into_iter().rev() {
        out.replace_range(start..end, &format!("‹已隐去的{kind}›"));
    }
    out
}

/// 自建规则的入库校验：名称非空、正则编译得过得去——
/// 存一条编译不过的正则等于存一条永远不会命中的装饰（is_known_key 同一条纪律）
pub fn validate_custom_rules(rules: &[CustomSecretRule]) -> Result<(), String> {
    for rule in rules {
        if rule.label.trim().is_empty() {
            return Err(format!("自定义规则「{}」的名称是空的", rule.id));
        }
        if rule.pattern.trim().is_empty() {
            return Err(format!("自定义规则「{}」的正则是空的", rule.label));
        }
        if let Err(problem) = Regex::new(&rule.pattern) {
            return Err(format!(
                "自定义规则「{}」的正则编译失败：{problem}",
                rule.label
            ));
        }
    }
    Ok(())
}

/// 内置正则改写的入库校验：目标规则得存在，新正则得编译得过去
pub fn validate_pattern_edits(edits: &[SecretRulePatternEdit]) -> Result<(), String> {
    for edit in edits {
        if !RULES.iter().any(|rule| rule.id == edit.id) {
            return Err(format!("要改正则的规则不存在：{id}", id = edit.id));
        }
        if let Err(problem) = Regex::new(&edit.pattern) {
            return Err(format!(
                "规则「{}」的新正则编译失败：{problem}",
                RULES
                    .iter()
                    .find(|rule| rule.id == edit.id)
                    .map(|rule| rule.label)
                    .unwrap_or(edit.id.as_str())
            ));
        }
    }
    Ok(())
}

/// 用户明确说过"不要记"的内容不得写入。这一条只查原文，不做语义推断
pub fn marked_do_not_store(text: &str) -> bool {
    const MARKERS: [&str; 6] = [
        "不要记",
        "别记",
        "不用记",
        "不得记录",
        "don't remember",
        "do not store",
    ];
    let lowered = text.to_ascii_lowercase();
    MARKERS.iter().any(|marker| lowered.contains(marker))
}

/// 出口过滤：要把这段文字发给网络或写进审计之前问一句。
/// 与 `leaks_sensitive` 同一个判定，区别只在语义上——调用方因此可以只写一处规则表
pub fn redact_for_audit(text: &str) -> Option<&'static str> {
    leaks_sensitive(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 打码后的文本必须**自己不再被判定成敏感**。只遮住值、留着 `key=value` 的形状，
    /// 打码结果就会被同一条规则再抓一次——而调用方以为它安全了，于是那份文本被存到了盘上
    #[test]
    fn masked_text_stops_looking_sensitive() {
        for raw in [
            r#"curl -H "Authorization: Bearer gho_1234567890abcdef" https://api.example.com/x"#,
            "curl https://api.example.com -d token=sk-abcdef0123456789",
            "API_KEY: sk-abcdef0123456789",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA",
            "把 6222021234567890123 转到 13800138000",
        ] {
            assert!(
                leaks_sensitive(raw).is_some(),
                "这条本来就没被抓到，打码也就无从谈起：{raw}"
            );
            let masked = mask_secrets(raw);
            assert_eq!(
                leaks_sensitive(&masked),
                None,
                "打码后仍然命中检测：{masked}"
            );
            assert_eq!(mask_secrets(&masked), masked, "打码该是幂等的：{masked}");
        }
    }

    /// 遮的只是那一段。审批要让人看得懂"这一发要动谁、往哪发"——命令名、目标主机、路径
    /// 都得原样留着。这是它与 `redact_for_audit`（整条丢掉）的分工：那边丢了不可惜，这边丢了就没法判
    #[test]
    fn masking_keeps_everything_that_is_not_the_secret() {
        let masked =
            mask_secrets(r#"curl -H "password: hunter2abcdefgh" https://pay.example.com/charge"#);
        assert!(
            masked.contains("curl") && masked.contains("https://pay.example.com/charge"),
            "{masked}"
        );
        assert!(masked.contains("已隐去的凭据"), "{masked}");
        assert!(!masked.contains("hunter2abcdefgh"), "密钥还在：{masked}");
        assert_eq!(
            mask_secrets("读取 src/main.rs"),
            "读取 src/main.rs",
            "无关文本一个字都不该动"
        );
        // 命令里那个端口号不是秘密：只有够长的数字游程才动手
        assert!(
            mask_secrets("curl http://127.0.0.1:8787/hook").contains("8787"),
            "把端口也遮了就没人认得出这是哪台"
        );
    }

    #[test]
    fn catches_credentials_in_both_languages() {
        assert_eq!(
            leaks_sensitive("我的密码是hunter2abcdefgh"),
            Some("疑似凭据（密钥 / token）")
        );
        assert_eq!(
            leaks_sensitive("API_KEY: sk-abcdef0123456789"),
            Some("疑似凭据（密钥 / token）")
        );
        assert_eq!(
            leaks_sensitive("token=ghp_1234567890abcdef"),
            Some("疑似凭据（密钥 / token）")
        );
        // PEM 头统一归"私钥"。以前它先被凭据那条抓到——因为同一个模式在两张地方各列过一次，
        // 而那张表恰好被 "private key" 这个提示词放行。分类是偶然的（两类都该拦）；
        // 规则表收成一份之后它只剩一个归属，而这个归属更准
        assert_eq!(
            leaks_sensitive("-----BEGIN RSA PRIVATE KEY-----"),
            Some("疑似私钥"),
            "私钥头的分类不再依赖它碰巧也被列进凭据规则"
        );
        // 大小写都要走同一条独立判断：私钥头不该依赖关键词命中
        assert_eq!(
            leaks_sensitive("-----begin openssl private key-----"),
            Some("疑似私钥"),
            "私钥头不该依赖关键词命中——独立那条就是为这个存在的"
        );
    }

    #[test]
    fn catches_long_id_card_and_phone_numbers() {
        assert_eq!(
            leaks_sensitive("身份证 11010519491231002X"),
            Some("疑似证件号 / 银行卡号")
        );
        assert_eq!(
            leaks_sensitive("卡号 6222021234567890123"),
            Some("疑似证件号 / 银行卡号")
        );
        assert_eq!(leaks_sensitive("手机 13800138000"), Some("疑似手机号"));
    }

    #[test]
    fn leaves_ordinary_numbers_and_prose_alone() {
        // 这三条是"别把正常内容也拦了"的反向证据：没有它们，前面的断言只是恒真
        assert_eq!(leaks_sensitive("用 pnpm 9.15.4 管理依赖"), None);
        assert_eq!(leaks_sensitive("提交 26ea36c 修了缓存保温"), None);
        assert_eq!(leaks_sensitive("窗口宽 1344，右栏 384"), None);
    }

    #[test]
    fn honours_explicit_do_not_store() {
        assert!(marked_do_not_store("这条不要记：我更喜欢简洁的答复"));
        assert!(marked_do_not_store("do not store this line"));
        assert!(!marked_do_not_store("记住我偏好结论先行"));
    }

    // ---- 命名规则库与关闭名单（design-security-center.md D6）----

    #[test]
    fn token_shaped_rules_fire_without_any_hint_word() {
        // 这些格式自带辨识度，不吃提示词闸：文本里没有任何 password/token 字样也要抓
        assert!(leaks_sensitive(
            " curl https://ci.example.com/upload -H \"X-Key: sk-proj-abcdefghij0123456789\""
        )
        .is_some());
        assert!(leaks_sensitive("deploy with AKIAIOSFODNN7EXAMPLE").is_some());
        assert!(leaks_sensitive(
            "id_token=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.SflKxwRJSMeKKF2QT4f"
        )
        .is_some());
        // 无关文本不误伤
        assert_eq!(leaks_sensitive("task-12345 finished"), None);
    }

    #[test]
    fn a_disabled_rule_stops_firing_for_both_detection_and_masking() {
        let raw = "deploy with AKIAIOSFODNN7EXAMPLE";
        assert!(leaks_sensitive_ex(raw, &[], &[], &[]).is_some());
        assert!(
            masked_away(&mask_secrets_ex(raw, &[], &[], &[]), raw),
            "规则开着就要打码"
        );
        // 关掉 aws 那条：判定与打码同时失灵——半开关状态写不出来
        assert_eq!(
            leaks_sensitive_ex(raw, &["aws".to_string()], &[], &[]),
            None
        );
        assert!(
            !masked_away(&mask_secrets_ex(raw, &["aws".to_string()], &[], &[]), raw),
            "关掉的规则连打码一起停"
        );
        // 关掉的是这一条，不是整张表
        assert!(leaks_sensitive_ex(
            "API_KEY: sk-abcdef0123456789",
            &["aws".to_string()],
            &[],
            &[]
        )
        .is_some());
    }

    fn masked_away(masked: &str, raw: &str) -> bool {
        masked != raw
    }

    // ---- 自建规则与内置正则改写（design-security-center.md D6）----

    #[test]
    fn a_custom_rule_fires_without_hints_and_masks_like_a_builtin() {
        let customs = vec![CustomSecretRule {
            id: "custom-test-1".into(),
            label: "内部工单密钥".into(),
            pattern: r#"GDQ-[A-Z0-9]{12}"#.into(),
        }];
        let raw = "ticket GDQ-A1B2C3D4E5F6 accepted";
        assert!(
            leaks_sensitive_ex(raw, &[], &customs, &[]).is_some(),
            "自建规则不吃提示词闸"
        );
        let masked = mask_secrets_ex(raw, &[], &customs, &[]);
        assert!(
            masked.contains("已隐去的凭据") && !masked.contains("GDQ-A1B2"),
            "{masked}"
        );
        // 关掉它（按 id）与关内置规则同一套语义
        assert!(leaks_sensitive_ex(raw, &["custom-test-1".to_string()], &customs, &[]).is_none());
    }

    #[test]
    fn a_pattern_edit_replaces_the_builtin_and_skips_the_hint_gate() {
        // 把 openai 那条的 pattern 改成完全不同的形状：改写按字面生效，
        // 且不再吃提示词闸（用户改的 pattern 里未必还留着关键词）
        let edits = vec![SecretRulePatternEdit {
            id: "openai".into(),
            pattern: r"ZZQ-[A-Z0-9]{8}".into(),
        }];
        let raw = "key ZZQ-AB12CD34 issued";
        assert!(leaks_sensitive_ex(raw, &[], &[], &edits).is_some());
        let masked = mask_secrets_ex(raw, &[], &[], &edits);
        assert!(!masked.contains("ZZQ-AB12CD34"), "{masked}");
        // 改写只动命中的那条：别的规则照旧
        assert!(leaks_sensitive_ex("token=abcdef123456", &[], &[], &edits).is_some());
    }

    #[test]
    fn custom_and_edit_validation_reject_broken_patterns() {
        assert!(validate_custom_rules(&[CustomSecretRule {
            id: "c1".into(),
            label: "坏正则".into(),
            pattern: "([".into(),
        }])
        .is_err());
        assert!(validate_custom_rules(&[CustomSecretRule {
            id: "c1".into(),
            label: "".into(),
            pattern: "ok".into(),
        }])
        .is_err());
        assert!(validate_pattern_edits(&[SecretRulePatternEdit {
            id: "no-such-rule".into(),
            pattern: "ok".into(),
        }])
        .is_err());
        assert!(validate_pattern_edits(&[SecretRulePatternEdit {
            id: "aws".into(),
            pattern: "(".into(),
        }])
        .is_err());
        assert!(validate_pattern_edits(&[SecretRulePatternEdit {
            id: "aws".into(),
            pattern: "AKIA[0-9A-Z]+".into(),
        }])
        .is_ok());
    }

    /// 进程级开关只管话题流那一道：关掉后 mask_for_thread 原样放行。
    /// 这条测试碰全局状态——用串行的方式在末尾恢复，别的测试读的是默认值
    #[test]
    fn thread_masking_honours_the_process_switch() {
        set_scan_options(false, vec![], vec![], vec![]);
        assert_eq!(
            mask_for_thread("API_KEY: sk-abcdef0123456789"),
            "API_KEY: sk-abcdef0123456789"
        );
        set_scan_options(true, vec![], vec![], vec![]);
        let masked = mask_for_thread("API_KEY: sk-abcdef0123456789");
        assert!(
            masked.contains("已隐去的凭据") && masked.contains("敏感保护"),
            "{masked}"
        );
    }
}
