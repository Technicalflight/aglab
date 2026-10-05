//! System 1 决策层的原生出站通道（design-decision-layer.md §2.2）。
//!
//! 为什么它在 Rust 这边：Jev 是云端 API，WebView 里的 `fetch` 受 CORS 约束，
//! `api.typesafe.ai` 没有义务放行 `tauri://localhost` 这个源。原生 ureq 不看 CORS，
//! 但**必须**看 `egress` 的域名出口名单——这里不是第二个不过闸的 POST 出口，
//! 而是同一条规矩（出站先问名单）下的另一条已申报线路，与 `tasks/hook.rs` 的
//! outbound webhook 同构：各自调 `egress::guard`。目标主机默认是已知的那几家，
//! 而 Jev 这一条**允许用户自己填一个服务商**（`via = "custom"`）——填进来的那个先过
//! 形式校验（[`jev_endpoint_problem`]），再过同一份名单，所以它不是第二条不过闸的路。
//!
//! 敏感数据不在这条通道上做判断——路由器（前端）已把 private/confidential 钉死
//! 在本地 Laya，能走到这里的只有 public 请求。通道只负责忠实转发 + 报出人话错误。
//!
//! 同一模块里的另一件事：把 Laya 的 sidecar 进程拉起来（`decision_sidecar_start`）。
//! 它之所以在 Rust：WebView 里没有任何起进程的途径。它**刻意不做**的事是"开机自动起"——
//! 首次运行要从 HuggingFace 拉 1.7GB 权重、还要求机器上有 Node 20+，
//! 这两个决定都该是用户亲手按下去的（见 design-decision-layer.md §6 Phase 2）。

use std::path::PathBuf;
use std::process::{Child, Command as OsCommand, Stdio};
use std::sync::Mutex;

use serde::Deserialize;

/// Jev 的模型别名。要复现实验时在调用方锁具体版本号（jev-x.y.z），这里不写死版本
const MODEL_ID: &str = "jev-latest";

/// 超时的合法窗口，与前端 config 的钳制口径一致（config.ts jev.timeoutMs 的边界）
const TIMEOUT_MIN_MS: u64 = 100;
const TIMEOUT_MAX_MS: u64 = 120_000;

/// Jev 密钥在系统 keyring 里的固定落点。它不属于任何 provider 档案——
/// 是决策层自己的凭据；固定的名字让「写、查、删」三处与用户手动建条目都对得上
const KEY_SERVICE: &str = "aglab-decision";
const KEY_USER: &str = "jev-api-key";

/// 密钥的取用顺序：显式带 key 的请求直接用（联调/旧配置），否则回退 keyring。
/// 抽成带闭包的纯函数是为了测试不用碰真实凭据库
fn resolve_key(explicit: &str, lookup: impl FnOnce() -> Result<String, keyring::Error>) -> Result<String, String> {
    if !explicit.trim().is_empty() {
        return Ok(explicit.to_string());
    }
    match lookup() {
        Ok(secret) if !secret.trim().is_empty() => Ok(secret),
        Ok(_) | Err(keyring::Error::NoEntry) => Err("Jev apiKey 未配置（显式配置为空，keyring 里也没有条目）".to_string()),
        Err(e) => Err(format!("读取凭据失败：{e}")),
    }
}

fn lookup_keyring() -> Result<String, keyring::Error> {
    keyring::Entry::new(KEY_SERVICE, KEY_USER)?.get_password()
}

/// 内置的两家。它们的地址由这里持有，`via` 认不到第三个名字——
/// 想打别的地址只有一条路：`via = "custom"` 并带 `baseUrl`，那一发要先过
/// [`jev_endpoint_problem`] 的形式校验，再和内置地址一样过出口域名名单
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JevVia {
    Typesafe,
    Openrouter,
}

impl JevVia {
    /// 与前端 `JEV_ENDPOINTS`（providers/jev.ts）是同一对地址的两份拷贝。
    /// 改任何一边都要同步另一边——providers.test.ts 与这里的测试各钉一份
    fn endpoint(self) -> &'static str {
        match self {
            Self::Typesafe => "https://api.typesafe.ai/v1/systemone",
            Self::Openrouter => "https://openrouter.ai/api/alpha/decisions",
        }
    }
}

/// 环回那三个名字。127.0.0.0/8 整段都是本机，`localhost` 与 `::1` 是它的别名
fn is_loopback(host: &str) -> bool {
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

/// 自定义服务商的形式规则。**与前端 `jevEndpointProblem`（providers/jev.ts）是同一条**，
/// 两边由那张 `JEV_URL_CASES` 表钉着（见 tests 里那条读表逐行比对的测试）。
///
/// 它管的是"这个地址读不读得出来、是不是明文、有没有把凭据写在里面"。
/// 它**不**管"这台主机该不该收这一发"——那是出口域名名单的活，而名单为空 = 不收紧。
/// 所以这一发最终发到哪，由"用户亲手填的那格 + 那份名单"共同决定，不由这里决定
pub(crate) fn jev_endpoint_problem(raw: &str) -> Option<&'static str> {
    let url = raw.trim();
    if url.is_empty() {
        return Some("要填完整的服务商 URL");
    }
    if url.chars().any(char::is_whitespace) {
        return Some("URL 里不许有空格");
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return Some("读不出这个 URL");
    };
    let secure = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => false,
        _ => return Some("只允许 http 或 https"),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Some("缺主机名");
    }
    // 凭据常常写在 authority 里，而这一串要进日志与审计的 target：
    // 带 @ 的一律不收（`egress::host_of` 会把它剥掉，那是取 target 的口径，不是收单的口径）
    if authority.contains('@') {
        return Some("URL 里不许带凭据（user:token@）");
    }
    let host = crate::egress::strip_port(authority)
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    if host.is_empty() {
        return Some("缺主机名");
    }
    if !secure && !is_loopback(&host) {
        return Some("http 只允许本机（127.x / localhost / ::1），其余要 https");
    }
    let path = rest[authority.len()..].split(['?', '#']).next().unwrap_or_default();
    if path.is_empty() || path == "/" {
        return Some("要把服务商路径写全，例如 /v1/systemone");
    }
    None
}

/// 这一发要打到哪。`via` 在命令边界上仍是白名单：那两家的名字，或者 `custom`
/// 配一个过形式校验的地址。别的字符串一律一句话打回
fn endpoint_of(via: Option<&str>, base_url: Option<&str>) -> Result<String, String> {
    match via.unwrap_or("typesafe") {
        "typesafe" => Ok(JevVia::Typesafe.endpoint().to_string()),
        "openrouter" => Ok(JevVia::Openrouter.endpoint().to_string()),
        "custom" => {
            let Some(raw) = base_url else {
                return Err("via=custom 得同时带上 baseUrl".to_string());
            };
            if let Some(problem) = jev_endpoint_problem(raw) {
                return Err(format!("Jev 自定义服务商不合法：{problem}"));
            }
            Ok(raw.trim().to_string())
        }
        other => Err(format!(
            "via 只认 typesafe/openrouter/custom，收到的是 {other}"
        )),
    }
}

/// 前端 invoke 的参数包。camelCase 是 Tauri IPC 的前端口径，serde 这边对齐
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JevDecisionRequest {
    api_key: String,
    /// Jev 的 state 只收文本；序列化在前端做完了，这里原样转发
    state: String,
    questions: serde_json::Value,
    via: Option<String>,
    /// 只在 `via = "custom"` 时参与：地址由用户亲手填，这里再校一遍形式
    base_url: Option<String>,
    timeout_ms: Option<u64>,
}

#[tauri::command]
pub async fn decision_jev_system_one(
    app: tauri::AppHandle,
    request: JevDecisionRequest,
) -> Result<serde_json::Value, String> {
    let url = endpoint_of(request.via.as_deref(), request.base_url.as_deref())?;
    let config = crate::config::load(&app);
    // 名单是"有没有可能问都不该问"的那道闸，坐在 send 之前：被拦下的请求
    // 连排队都不进，更不该有流量先出去再补票
    crate::egress::guard(&config.net_egress_allow, &url)?;
    // 决策流量与模型同族，跟全局代理绑定走（本机回环与绕过名单照旧豁免）。
    // 这一发的成败由 send 回报，占用由 Leg 的 Drop 兜底
    let mut leg = crate::proxy::take_global(&config, &url)?;
    // ureq 是阻塞的，不能占 async 执行器的线程；5 秒级的等待丢进阻塞池
    tauri::async_runtime::spawn_blocking(move || send(&url, &request, &mut leg))
        .await
        .map_err(|error| format!("决策代理的执行线程异常：{error}"))?
}

/// 密钥存进系统凭据库。从此 config 里不再有明文，WebView 内存里也不再有——
/// 请求到达时由 [`send`] 在原生侧自取，前端只拿「有没有」这一个比特
#[tauri::command]
pub fn decision_jev_key_set(secret: String) -> Result<(), String> {
    keyring::Entry::new(KEY_SERVICE, KEY_USER)
        .map_err(|e| format!("凭据条目初始化失败：{e}"))?
        .set_password(&secret)
        .map_err(|e| format!("写入凭据失败：{e}"))
}

/// 只报有无，不报内容。装配层启动时探一次，把这一个比特喂给前端可用性判断
#[tauri::command]
pub fn decision_jev_key_state() -> Result<bool, String> {
    match lookup_keyring() {
        Ok(secret) => Ok(!secret.trim().is_empty()),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(format!("读取凭据失败：{e}")),
    }
}

/// 删条目。本来就没有也算删成——「清除」的语义是不留痕迹，不是报一笔陈账
#[tauri::command]
pub fn decision_jev_key_delete() -> Result<(), String> {
    let entry = keyring::Entry::new(KEY_SERVICE, KEY_USER)
        .map_err(|e| format!("凭据条目初始化失败：{e}"))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("删除凭据失败：{e}")),
    }
}

/// 超时的合法窗口由这里唯一裁定：前端手滑传 0 或传一天，原生这边都不照办
fn clamp_timeout(timeout_ms: Option<u64>) -> u64 {
    timeout_ms
        .unwrap_or(5000)
        .clamp(TIMEOUT_MIN_MS, TIMEOUT_MAX_MS)
}

/// 阻塞发送。薄薄一层：超时、代理、鉴权头、载荷、错误翻译，没有决策逻辑——
/// 决策（路由、阈值、敏感性）全在前端路由器，两层各说各的职责。
/// `proxy` 是全局绑定解析出的那条路（None = 直连），由命令边界解析好递进来
fn send(
    url: &str,
    request: &JevDecisionRequest,
    leg: &mut crate::proxy::Leg,
) -> Result<serde_json::Value, String> {
    let api_key = resolve_key(&request.api_key, lookup_keyring)?;
    let timeout = std::time::Duration::from_millis(clamp_timeout(request.timeout_ms));
    let mut config = ureq::Agent::config_builder().timeout_global(Some(timeout));
    if let Some(proxy_url) = leg.proxy_url() {
        // 地址不合法是我们自己的毛病：话先抄下来，再收这一步的账
        let parsed = match ureq::Proxy::new(proxy_url) {
            Ok(parsed) => parsed,
            Err(error) => {
                let message = format!("代理地址「{proxy_url}」不合法：{error}");
                leg.finish(crate::proxy::Outcome::Neutral);
                return Err(message);
            }
        };
        config = config.proxy(Some(parsed));
    }
    let agent = config.build().new_agent();
    let started = std::time::Instant::now();
    let mut response = agent
        .post(url)
        .header("authorization", &format!("Bearer {api_key}"))
        .header("content-type", "application/json")
        .send_json(build_payload(&request.state, &request.questions))
        .map_err(|error| {
            // 与模型出口同一条尺：Jev 回了状态码说明路走通了，只有没拿到头才算代理的事
            leg.finish(crate::proxy::outcome_of(&error));
            describe_send_error(error)
        })?;
    leg.note_head(started.elapsed());
    leg.finish(crate::proxy::Outcome::Reached);
    response
        .body_mut()
        .read_json::<serde_json::Value>()
        .map_err(|error| format!("Jev 响应不是合法 JSON：{error}"))
}

/// Jev 的请求体。state 是文本、questions 是结构化批量——一次请求把全部问题带全，
/// 这是「零输出 token」的前提，拆成多次往返就把批量前向的便宜丢光了
fn build_payload(state: &str, questions: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "state": state,
        "model": MODEL_ID,
        "questions": questions,
    })
}

/// 错误翻译。HTTP 状态码是调用方能读懂的故障语言（401 = key 不对），
/// 其余（断连、TLS）统一归成「请求失败」——与 chat.rs 的 describe_status 同一口径
fn describe_send_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::StatusCode(code) => format!("Jev HTTP {code}"),
        other => format!("Jev 请求失败：{other}"),
    }
}

/* ---- Laya sidecar：一键启动与停止 ---- */

/// 前端没指定端口时用它。与 config.ts 里 `laya.sidecarEndpoint` 的默认值同口径
const DEFAULT_SIDECAR_PORT: u16 = 8787;
/// 1024 以下全是系统保留端口。0（让内核挑）也不行：端口是前端唯一用来找它的地址，
/// 内核挑完没人告诉 aglab，配置里就只剩一个连不上的服务商
const MIN_SIDECAR_PORT: u16 = 1024;

/// 一次拉起所需的全部事实，都是纯数据：所以"这个目录对不对"能在测试里问，
/// 而不用真的起一个 Node
#[derive(Debug, PartialEq, Eq)]
struct LaunchPlan {
    dir: PathBuf,
    script: PathBuf,
    port: u16,
    subfolder: Option<String>,
}

/// 校验用户挑的目录并组装启动计划。目录里必须有 `index.mjs`——
/// 挑错目录比挑不到更糟：那会起一个来路不明的脚本
fn plan_launch(dir: &str, port: Option<u16>, subfolder: Option<&str>) -> Result<LaunchPlan, String> {
    let trimmed = dir.trim();
    if trimmed.is_empty() {
        return Err("还没说 sidecar 在哪个目录。选 scripts/laya-sidecar 那一级。".to_string());
    }
    let directory = PathBuf::from(trimmed);
    if !directory.is_dir() {
        return Err(format!("那个目录不存在或不是目录：{}", directory.display()));
    }
    let script = directory.join("index.mjs");
    if !script.is_file() {
        return Err(format!(
            "{} 旁边没有 index.mjs——那不是 sidecar 的目录",
            directory.display()
        ));
    }
    let port = port.unwrap_or(DEFAULT_SIDECAR_PORT);
    if port < MIN_SIDECAR_PORT {
        return Err(format!(
            "端口 {port} 太低：保留端口和\"让内核挑一个\"都不收（默认 {DEFAULT_SIDECAR_PORT}）"
        ));
    }
    Ok(LaunchPlan {
        dir: directory,
        script,
        port,
        subfolder: validate_subfolder(subfolder)?,
    })
}

/// `LAYA_SUBFOLDER` 会被拼进 HuggingFace 的下载路径，所以只收一个扁平的目录名：
/// 字母数字与 `. _ -`，且不许出现 `..`。字符集本身已经挡住了 `/` 与 `\`
fn validate_subfolder(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(value) = raw.map(str::trim) else { return Ok(None) };
    if value.is_empty() {
        return Ok(None);
    }
    let shaped = value.len() <= 64
        && value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !value.contains("..");
    if !shaped {
        return Err(format!(
            "模型子目录只能是一个扁平名字（字母数字与 . _ -，不许 ..）：收到 {value}"
        ));
    }
    Ok(Some(value.to_string()))
}

/// aglab 拉起的那一个 sidecar 孩子。用户自己在终端里跑的那份不在这里——
/// 我们没那个句柄，也不该隔空去杀别人的进程
#[derive(Default)]
pub struct SidecarHub {
    child: Mutex<Option<Child>>,
}

impl SidecarHub {
    /// 我们手上那个还活着吗。已经退出的不留格子（下一次 start 可以顶上来）
    fn running_pid(&self) -> Option<u32> {
        let Ok(mut guard) = self.child.lock() else { return None };
        let Some(child) = guard.as_mut() else { return None };
        match child.try_wait() {
            Ok(None) => Some(child.id()),
            Ok(_) => {
                *guard = None;
                None
            }
            // 问不出死活就当它还活着：宁可让用户手动停一次，也不在他可能还在用的进程上再点一份
            Err(_) => Some(child.id()),
        }
    }

    fn remember(&self, child: Child) -> u32 {
        let pid = child.id();
        if let Ok(mut guard) = self.child.lock() {
            *guard = Some(child);
        }
        pid
    }

    /// 收掉自己的孩子连同它的子孙。返回 false = 本来就没有我们起的那个
    fn reap(&self) -> bool {
        let Ok(mut guard) = self.child.lock() else { return false };
        match guard.take() {
            Some(mut child) => {
                crate::tool_runtime::constrain::reap_tree(&mut child);
                true
            }
            None => false,
        }
    }
}

/// 起 sidecar。已经起着就报回那个 pid：为了一次多点的鼠标不重启已经热好的 1.7GB 模型
#[tauri::command]
pub fn decision_sidecar_start(
    hub: tauri::State<'_, SidecarHub>,
    dir: String,
    port: Option<u16>,
    subfolder: Option<String>,
) -> Result<u32, String> {
    if let Some(existing) = hub.running_pid() {
        return Ok(existing);
    }
    let plan = plan_launch(&dir, port, subfolder.as_deref())?;
    let child = spawn_sidecar(&plan)?;
    Ok(hub.remember(child))
}

/// 停掉 aglab 起的那个。false 说得很直白：那个不是 aglab 起的，请去它自己的终端里 Ctrl+C
#[tauri::command]
pub fn decision_sidecar_stop(hub: tauri::State<'_, SidecarHub>) -> Result<bool, String> {
    Ok(hub.reap())
}

fn spawn_sidecar(plan: &LaunchPlan) -> Result<Child, String> {
    let mut command = OsCommand::new("node");
    // 与模型请求同一条规矩：子进程不带着这台机器上凭据形状的环境变量。
    // sidecar 只需要 PATH / HOME / TEMP，多出来的它一个也用不上
    crate::tool_runtime::constrain::constrained(&mut command);
    command
        .arg(&plan.script)
        .current_dir(&plan.dir)
        .env("PORT", plan.port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(subfolder) = &plan.subfolder {
        command.env("LAYA_SUBFOLDER", subfolder);
    }
    command.spawn().map_err(|error| describe_spawn_error(&error))
}

/// Node 不在 PATH 里是这里最常见的那种失败，必须说得出"缺的是 node"，
/// 而不是留一句 `program not found` 让人去猜
fn describe_spawn_error(error: &std::io::Error) -> String {
    if matches!(error.kind(), std::io::ErrorKind::NotFound) {
        return "没找到 node：Laya sidecar 要 Node 20+。装一个，或者在终端里自己跑 \
                `cd scripts/laya-sidecar && npm i @receptron/laya && npm start`"
            .to_string();
    }
    format!("sidecar 没起来：{error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// via 在命令边界上仍是白名单：那两家的名字，或者 custom 配一个过校验的地址。
    /// 把 URL 塞进 `via` 打回——地址只有一个入口，就是 `baseUrl`
    #[test]
    fn via_accepts_the_two_vendors_custom_and_nothing_else() {
        assert_eq!(endpoint_of(None, None).unwrap(), JevVia::Typesafe.endpoint(), "缺省走 TypeSafe 主服务商");
        assert_eq!(
            endpoint_of(Some("openrouter"), None).unwrap(),
            "https://openrouter.ai/api/alpha/decisions"
        );
        assert_eq!(
            endpoint_of(Some("custom"), Some("http://localhost:8787/v1")).unwrap(),
            "http://localhost:8787/v1"
        );
        // 内置那两家不认 baseUrl：切回厂商之后，留着的那一格自定义地址不该还在决定去向
        assert_eq!(
            endpoint_of(Some("typesafe"), Some("http://169.254.169.254/latest/meta-data")).unwrap(),
            JevVia::Typesafe.endpoint()
        );
        assert!(endpoint_of(Some("http://169.254.169.254"), None).is_err());
        assert!(endpoint_of(Some(""), None).is_err());
        assert!(endpoint_of(Some("custom"), None).is_err(), "custom 不带地址就无从校起");
        assert!(endpoint_of(Some("custom"), Some("http://api.example.com/v1")).is_err());
    }

    /// 自定义服务商的形式规则。判的是"读不读得出来、是不是明文、有没有把凭据写进去"，
    /// 判不了"该不该发到这台主机"——那一半是出口名单的活
    #[test]
    fn custom_endpoint_needs_https_a_host_and_a_path() {
        assert_eq!(jev_endpoint_problem("https://api.example.com/v1"), None);
        assert_eq!(jev_endpoint_problem("  https://api.example.com/v1  "), None, "首尾空白不算内容");
        assert_eq!(jev_endpoint_problem("http://127.0.0.1:8787/v1"), None, "本机联调要留路");
        assert_eq!(jev_endpoint_problem("http://[::1]:8787/v1"), None);
        assert!(jev_endpoint_problem("http://api.example.com/v1").is_some(), "明文出外网");
        assert!(jev_endpoint_problem("http://10.0.0.5/v1").is_some(), "内网也不算本机");
        assert!(jev_endpoint_problem("https://api.example.com").is_some(), "只有站点根不算服务商");
        assert!(jev_endpoint_problem("https://u:t@api.example.com/v1").is_some(), "凭据写在地址里");
        assert!(jev_endpoint_problem("ftp://api.example.com/v1").is_some());
        assert!(jev_endpoint_problem("").is_some());
        // 形式合法不等于放行：这一发能不能出去仍由名单决定
        assert_eq!(jev_endpoint_problem("https://169.254.169.254/latest/meta-data"), None);
        let allow = vec!["api.example.com".to_string()];
        assert!(crate::egress::guard(&allow, "https://169.254.169.254/latest/meta-data").is_err());
    }

    /// 两边是同一条规则，所以拿前端那张表逐行对。表在 `JEV_URL_CASES`
    /// （providers/jev.ts）里，一条一行；改一边不同步另一边就红
    #[test]
    fn custom_endpoint_cases_match_the_frontend_table() {
        let text = include_str!("../../src/lib/decision/providers/jev.ts");
        // 表上应有几条，按 `{ url: "` 数一遍。只对"数得出来的条数"负责的话，
        // 表换了形状（比如 `ok:false` 少了个空格）就会一边没对上也一边绿
        let expected = text.matches("{ url: \"").count();
        let mut checked = 0usize;
        for line in text.lines() {
            let Some(after_url) = line.trim().strip_prefix("{ url: \"") else {
                continue;
            };
            let Some((url, rest)) = after_url.split_once("\",") else {
                continue;
            };
            let ok = rest.contains("ok: true");
            assert_eq!(
                jev_endpoint_problem(url).is_none(),
                ok,
                "这条两边判得不一样：{url}（前端 ok={ok}）"
            );
            checked += 1;
        }
        assert!(expected >= 18, "表只剩 {expected} 条，这条针大概已经没东西可钉了");
        assert_eq!(
            checked, expected,
            "数出 {expected} 条却只比对上 {checked} 条：那张表换了形状，这条针正在漏读"
        );
    }

    /// 载荷形状是 TypeSafe 的合同：state 文本、model 别名、questions 原样结构化
    #[test]
    fn payload_carries_state_model_and_questions() {
        let questions = serde_json::json!({ "urgency": { "type": "score", "criteria": ["low", "high"] } });
        let payload = build_payload("refund not received", &questions);
        assert_eq!(payload["state"], "refund not received");
        assert_eq!(payload["model"], "jev-latest");
        assert_eq!(payload["questions"], questions);
    }

    /// 超时钳进 sane 窗口：测的就是 send 用的那把钳（clamp_timeout），不是复述一遍
    #[test]
    fn timeouts_are_clamped_into_a_sane_window() {
        assert_eq!(clamp_timeout(Some(0)), TIMEOUT_MIN_MS);
        assert_eq!(clamp_timeout(Some(u64::MAX)), TIMEOUT_MAX_MS);
        assert_eq!(clamp_timeout(Some(3000)), 3000);
        assert_eq!(clamp_timeout(None), 5000);
    }

    /// 401 是用户能自己修的错（key 不对），必须说出状态码而不是一句「失败」
    #[test]
    fn send_errors_name_the_status_code() {
        let message = describe_send_error(ureq::Error::StatusCode(401));
        assert!(message.contains("401"), "{message}");
        assert!(message.contains("Jev"), "{message}");
    }

    /// 密钥取用的优先级：显式给的一言堂；空了才落 keyring；两边都没有要说清是哪种没有
    #[test]
    fn explicit_key_wins_then_keyring_then_a_honest_error() {
        let explicit = resolve_key("sk-explicit", || {
            Err(keyring::Error::NoEntry)
        })
        .unwrap();
        assert_eq!(explicit, "sk-explicit", "显式给 key 时根本不该碰 keyring");

        let from_ring = resolve_key("", || Ok("sk-from-ring".to_string())).unwrap();
        assert_eq!(from_ring, "sk-from-ring");

        let err = resolve_key("", || Err(keyring::Error::NoEntry)).unwrap_err();
        assert!(err.contains("未配置"), "{err}");
        // 空字符串的条目视同没有——半截密钥比没有更害人
        let blank = resolve_key("", || Ok("   ".to_string())).unwrap_err();
        assert!(blank.contains("未配置"), "{blank}");
    }

    /// 挑错目录比挑不到更糟：那会起一个来路不明的脚本。所以计划这一层只认
    /// "这个目录旁边真有 index.mjs"，别的都不猜
    #[test]
    fn plan_wants_a_directory_that_actually_holds_the_sidecar() {
        use crate::test_support::{remove_tree, temp_dir};
        let root = temp_dir("sidecar-plan");
        let missing = plan_launch(root.to_str().unwrap(), None, None).unwrap_err();
        assert!(missing.contains("index.mjs"), "{missing}");

        std::fs::write(root.join("index.mjs"), "// sidecar").unwrap();
        let plan = plan_launch(root.to_str().unwrap(), None, None).unwrap();
        assert_eq!(plan.dir, root);
        assert_eq!(plan.script, root.join("index.mjs"));
        assert_eq!(plan.port, DEFAULT_SIDECAR_PORT, "没指定端口就落在配置的默认值上");
        assert_eq!(plan.subfolder, None);
        remove_tree(&root);
    }

    #[test]
    fn a_directory_that_is_not_there_says_so() {
        let err = plan_launch("Z:\\没有这个目录\\也不可能", None, None).unwrap_err();
        assert!(err.contains("不存在"), "{err}");
        assert!(plan_launch("   ", None, None).unwrap_err().contains("还没说"));
    }

    /// 0 号端口（"内核自己挑"）也在这里拒掉：端口是前端唯一找得到这个进程的地址
    #[test]
    fn ports_below_the_reserved_range_and_zero_are_both_refused() {
        use crate::test_support::{remove_tree, temp_dir};
        let root = temp_dir("sidecar-port");
        std::fs::write(root.join("index.mjs"), "").unwrap();
        let base = root.to_str().unwrap();
        assert_eq!(plan_launch(base, Some(MIN_SIDECAR_PORT), None).unwrap().port, MIN_SIDECAR_PORT);
        assert_eq!(plan_launch(base, Some(65535), None).unwrap().port, 65535);
        for bad in [0u16, 1, 80, MIN_SIDECAR_PORT - 1] {
            let err = plan_launch(base, Some(bad), None).unwrap_err();
            assert!(err.contains("端口"), "{bad} → {err}");
        }
        remove_tree(&root);
    }

    #[test]
    fn subfolder_is_a_flat_name_or_nothing() {
        assert_eq!(validate_subfolder(None).unwrap(), None);
        assert_eq!(validate_subfolder(Some("   ")).unwrap(), None, "空白等于没说");
        assert_eq!(
            validate_subfolder(Some("multilingual")).unwrap().as_deref(),
            Some("multilingual")
        );
        assert_eq!(validate_subfolder(Some("v1.2_x")).unwrap().as_deref(), Some("v1.2_x"));
        for bad in ["../etc", "..", "a/b", "a\\b", "中文子目录", &"x".repeat(65)] {
            assert!(validate_subfolder(Some(bad)).is_err(), "{bad} 不该被当成一个目录名");
        }
    }

    /// 我们只管自己拉起的那一个孩子。用户自己在终端里跑的那份没有句柄，
    /// 隔空杀它是比留着更糟的选择——所以 stop 要如实返回 false
    #[test]
    fn the_hub_only_knows_about_the_child_it_spawned() {
        let hub = SidecarHub::default();
        assert_eq!(hub.running_pid(), None, "没起过就没有 pid");
        assert!(!hub.reap(), "本来就没有我们起的那个：stop 说 false");

        let args: Vec<&str> = if cfg!(windows) {
            vec!["-n", "5", "127.0.0.1"]
        } else {
            vec!["5"]
        };
        let child = OsCommand::new(if cfg!(windows) { "ping" } else { "sleep" })
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("起一个能活几秒的子进程");
        let pid = child.id();
        assert_eq!(hub.remember(child), pid);
        assert_eq!(hub.running_pid(), Some(pid), "还活着的孩子要报得出 pid");
        assert!(hub.reap());
        assert_eq!(hub.running_pid(), None, "收掉以后不该还占着那一格");
        assert!(!hub.reap());
    }

    #[test]
    fn a_missing_node_names_node() {
        let message = describe_spawn_error(&std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(message.contains("node"), "{message}");
        assert!(message.contains("npm start"), "{message}");
        let other = describe_spawn_error(&std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "access denied by policy",
        ));
        assert!(other.contains("access denied"), "{other}");
    }

    /// **真起一次 sidecar**：真 node、真 `scripts/laya-sidecar/index.mjs`、真端口，
    /// 等它把 /health 答出来，再按 pid 收掉并确认端口不再有人听。
    ///
    /// 为什么要有这条：`plan_launch` / `running_pid` / `reap` 的单测各自只测一格，
    /// "按下去到底起不起得来"这件事只有把它真起来一次才算数。界面上那两颗按钮
    /// 在没有输入注入的话题里点不动，这条就是那条机制的读数。
    ///
    /// `#[ignore]` 的理由：它要求跑测试的机器上有 node 和那份脚本——CI 上没有时会红，
    /// 而红的原因与代码无关。要读数时在仓库里执行：
    /// `cargo test --lib sidecar_actually_listens -- --ignored`
    #[test]
    #[ignore = "要本机有 node 与 scripts/laya-sidecar：手动取读数用"]
    fn sidecar_actually_listens_and_can_be_reaped() {
        use std::io::ErrorKind;
        use std::net::{TcpListener, TcpStream};
        use std::time::{Duration, Instant};

        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("scripts")
            .join("laya-sidecar");
        let dir_str = dir.to_string_lossy().to_string();
        assert!(dir.join("index.mjs").is_file(), "没有那份参考实现：{dir_str}");

        // 找一个空端口：bind 0 拿到号，放掉再交给 sidecar。抢回去的窗口极短
        let probe = TcpListener::bind("127.0.0.1:0").expect("要一个临时端口");
        let port = probe.local_addr().expect("拿得到地址").port() as u16;
        drop(probe);

        let plan = plan_launch(&dir_str, Some(port), Some("multilingual")).expect("计划该成立");
        let mut child = spawn_sidecar(&plan).expect("node 该起得来");
        let url = format!("http://127.0.0.1:{port}/health");

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            assert!(
                child.try_wait().ok().flatten().is_none(),
                "sidecar 起来了又自己退了：多半是 node 找不到那份脚本"
            );
            assert!(Instant::now() < deadline, "20 秒内 {url} 没人听");
            std::thread::sleep(Duration::from_millis(200));
        }

        // 端口在听还不算数：契约要答得出那三个字段
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(3)))
            .build()
            .new_agent();
        let body: serde_json::Value = agent
            .get(&url)
            .call()
            .expect("/health 该回话")
            .body_mut()
            .read_json()
            .expect("/health 该回 JSON");
        assert_eq!(body["ok"], serde_json::json!(true), "{body}");
        assert_eq!(body["loaded"], serde_json::json!(false), "刚起来不该已加载");

        crate::tool_runtime::constrain::reap_tree(&mut child);
        let quiet = Instant::now() + Duration::from_secs(5);
        loop {
            match TcpStream::connect(("127.0.0.1", port)) {
                Err(error) if matches!(error.kind(), ErrorKind::ConnectionRefused) => break,
                _ if Instant::now() < quiet => std::thread::sleep(Duration::from_millis(100)),
                other => panic!("收掉之后 {url} 还在被应答：{other:?}"),
            }
        }
    }
}
