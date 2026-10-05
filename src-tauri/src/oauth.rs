//! 官方订阅登录（OAuth）：打开系统浏览器完成授权，本地起一个一次性回调口接住
//! 授权码，换回 access token 直接当 API 密钥写进档案的凭据目标。
//!
//! v1 只收两个协议公开且稳定的提供方：
//! - **Anthropic（Claude Pro/Max）**：PKCE。换到的 `sk-ant-oat01-…` 订阅令牌
//!   走 Bearer + `anthropic-beta: oauth-2025-04-20` 头（线协议按 key 前缀特判，
//!   `x-api-key` 会直接 401）。
//! - **OpenRouter**：PKCE。换回一把专用 API key，标准 OpenAI 兼容线直接用。
//!
//! 其余订阅服务（Copilot 的两段 token、Kimi/xAI/Muse 的私有流）没有可靠的
//! 公开口径，不猜 client_id——等有据可查再加。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::net::TcpListener;
use std::time::{Duration, Instant};
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

/// 等授权回调的超时。浏览器里登录慢一点很正常，但 5 分钟还没回来多半是放弃了
pub(crate) const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// Anthropic OAuth 的公共 client_id（Claude Code 同款，官方口径）
const ANTHROPIC_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// 档案界面的提供方清单
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthProvider {
    pub id: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    /// 建议的服务商地址：登录成功后前端直接填进档案
    pub base_url: &'static str,
    pub api_format: &'static str,
    /// 常用模型提示，档案的模型表空着时前端拿它垫提示
    pub models_hint: &'static str,
}

pub fn providers() -> Vec<OAuthProvider> {
    vec![
        OAuthProvider {
            id: "anthropic-claude",
            label: "Anthropic（Claude Pro/Max）",
            hint: "用 Claude 官方账号授权，走订阅内额度。登录成功后到模型表补上常用模型（如 claude-sonnet-4-5）。",
            base_url: "https://api.anthropic.com",
            api_format: "anthropic",
            models_hint: "claude-sonnet-4-5,claude-opus-4-1,claude-haiku-4-5",
        },
        OAuthProvider {
            id: "openrouter",
            label: "OpenRouter OAuth",
            hint: "用 OpenRouter 账号授权，生成一把专用 API key。登录成功后点「拉取模型列表」补全模型表。",
            base_url: "https://openrouter.ai/api/v1",
            api_format: "openai",
            models_hint: "",
        },
        OAuthProvider {
            id: "openai-chatgpt",
            label: "OpenAI（ChatGPT Plus/Pro）",
            hint: "用 ChatGPT 账号授权（Codex 同款流程），走订阅内额度。注意：走 Codex 后端（responses 协议），属实验性接入。",
            base_url: "https://chatgpt.com/backend-api/codex",
            api_format: "responses",
            models_hint: "gpt-5.2-codex,gpt-5.2",
        },
        OAuthProvider {
            id: "github-copilot",
            label: "GitHub Copilot",
            hint: "GitHub 设备码登录，Copilot 订阅内额度。短期令牌自动换发，无需手动刷新。",
            base_url: "https://api.githubcopilot.com",
            api_format: "openai",
            models_hint: "gpt-4.1,gpt-4o,claude-sonnet-3.7",
        },
    ]
}

/// GitHub Copilot 的 device flow 公共 client_id（Copilot CLI 同款，官方口径）
const COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
/// OpenAI OAuth 的公共 client_id（Codex CLI 同款）
const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// 登录结果。密钥已经写进 keyring 目标，这里只报目标与建议值
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginOutcome {
    pub base_url: &'static str,
    pub api_format: &'static str,
    pub models_hint: &'static str,
    pub credential_service: String,
    pub credential_user: String,
    /// 非空时界面要转述的提示。目前只有一种：点名的凭据槽位里已有密钥，
    /// 为免覆盖改用了本提供方的专属槽位（见 oauth_slot_guard）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// OAuth 令牌的专属凭据槽位：每个提供方一把，跟档案的普通 API key 槽位完全隔离
fn oauth_dedicated_slot(provider_id: &str) -> (String, String) {
    (format!("aglab.oauth.{provider_id}"), "oauth".to_string())
}

/// 写入前的落位护栏。点名的槽位里已有非空密钥就绝不覆盖——默认槽位
/// （aglab/api-key.default 这类）是全局共享的，里面那把 key 多半是别的服务商
/// 在用的，顶掉它等于让共用槽位的所有档案一起 401（2026-10-01 真机事故）。
/// 改道本提供方的专属槽位，并把改道写进 notice 让界面说清楚密钥落在哪。
/// 返回 (service, user, notice)
fn oauth_slot_guard(
    service: String,
    user: String,
    provider_id: &str,
) -> (String, String, Option<String>) {
    let (dedicated_service, dedicated_user) = oauth_dedicated_slot(provider_id);
    // 点名的就是专属槽位（重复登录刷新令牌）：直接覆盖
    if service == dedicated_service && user == dedicated_user {
        return (service, user, None);
    }
    let occupied = keyring::Entry::new(&service, &user)
        .and_then(|entry| entry.get_password())
        .map(|secret| !secret.trim().is_empty())
        .unwrap_or(false); // NoEntry 等错误一律按空槽处理
    if occupied {
        // 先把提示格式化成局部量再组元组：元组元素按序求值，先 move 后借是 E0382
        let notice = format!(
            "凭据目标 {user}.{service} 里已存有密钥，为免覆盖你现有的 key，本次登录改用专属目标 {dedicated_user}.{dedicated_service}。"
        );
        (dedicated_service, dedicated_user, Some(notice))
    } else {
        (service, user, None)
    }
}

// ---- PKCE ----

/// URL-safe 的 code_verifier。一次性凭据（几分钟寿命），时间 + 进程熵的 xorshift
/// 就够——不为它引入一个随机数依赖
pub(crate) fn make_verifier() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let mut seed = nanos ^ ((std::process::id() as u64) << 32) ^ 0x9E37_79B9_7F4A_7C15;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut out = String::new();
    for _ in 0..64 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.push(ALPHABET[(seed % 64) as usize] as char);
    }
    out
}

/// S256：BASE64URL-NOPAD(SHA256(verifier))
pub(crate) fn challenge_of(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn url_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    index += 3;
                    continue;
                }
                out.push(bytes[index]);
                index += 1;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 在已绑定的监听口上等浏览器带着授权码打回来。`expected_state` 是 Some 时做
/// state 回验防串站；None（OpenRouter 的 /auth 不透传 state）就跳过——它的防伪造
/// 由 PKCE 的 code_verifier 承担：没有 verifier 的授权码换不出令牌
pub(crate) fn wait_for_code_on(listener: TcpListener, expected_state: Option<&str>) -> Result<String, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("回调端口设非阻塞失败：{e}"))?;
    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            return Err("等授权回调超时（5 分钟）：浏览器里没有完成登录。".into());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                // 请求行：GET /callback?code=…&state=… HTTP/1.1
                let path = request.split_whitespace().nth(1).unwrap_or("");
                let query = path.split('?').nth(1).unwrap_or("");
                let (mut code, mut got_state, mut error) =
                    (String::new(), String::new(), String::new());
                for pair in query.split('&').filter(|pair| !pair.is_empty()) {
                    let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                    match key {
                        "code" => code = url_decode(value),
                        "state" => got_state = url_decode(value),
                        "error" => error = url_decode(value),
                        _ => {}
                    }
                }
                let (status, note) = if error.is_empty() {
                    (
                        "200 OK",
                        "授权完成。回到 aglab 继续就行——这一页可以关掉了。",
                    )
                } else {
                    ("400 Bad Request", "授权没有完成，回到 aglab 看错误提示。")
                };
                let page = format!(
                    "<meta charset='utf-8'><body style='font-family:sans-serif;padding:40px'><h2>{status_line}</h2><p>{note}</p></body>",
                    status_line = if error.is_empty() { "授权完成" } else { "授权未完成" }
                );
                let _ = stream.write_all(
                    &format!(
                        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        page.len(),
                        page
                    )
                    .into_bytes(),
                );
                let _ = stream.flush();

                if !error.is_empty() {
                    return Err(format!("授权被拒绝或取消：{error}"));
                }
                if let Some(expected) = expected_state {
                    if got_state != expected {
                        return Err("回调的 state 对不上（防串站的校验失败），请重试一次。".into());
                    }
                }
                if code.is_empty() {
                    continue;
                }
                return Ok(code);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("回调端口接收失败：{e}")),
        }
    }
}

use std::io::Write;

/// OAuth 出站的 Agent 构造：跟随全局代理绑定（与模型流量同一裁决处），没配就
/// 直连。之前这里全部硬直连——github.com 在不少网络下直连不通（os error 10061），
/// 设备码/令牌服务商出不去，登录第一步就死
pub(crate) fn oauth_agent(proxy_default: &str) -> Result<ureq::Agent, String> {
    let from_config = proxy_default.trim();
    if !from_config.is_empty() {
        return crate::proxy::agent_for(Some(from_config));
    }
    // 全局没配代理时回退读系统环境变量（HTTPS_PROXY/HTTP_PROXY/ALL_PROXY）：
    // OAuth 是低频用户操作不是模型流量，跟随启动环境合理——直连 github.com
    // 在不少网络下不通（os error 10061），不回退设备码永远出不去
    for key in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            // 环境变量的值常见裸 host:port 形状，ureq 要协议前缀
            let url = if value.contains("://") {
                value.to_string()
            } else {
                format!("http://{value}")
            };
            return crate::proxy::agent_for(Some(&url));
        }
    }
    crate::proxy::agent_for(None)
}

fn exchange_token(token_url: &str, body: Value, proxy_default: &str) -> Result<Value, String> {
    let agent = oauth_agent(proxy_default)?;
    // ureq 3 默认把 4xx/5xx 直接转成 Err(Error::StatusCode)，服务商正文里的真实死因
    // 会被吞成一句 "http status: 404"——按请求关掉，让下面的 status 分支拿全正文
    let mut response = agent
        .post(token_url)
        .config()
        .http_status_as_error(false)
        .build()
        .header("content-type", "application/json")
        .send_json(body)
        .map_err(|e| format!("换令牌失败：{e}"))?;
    let status = response.status();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("读令牌应答失败：{e}"))?;
    if !status.is_success() {
        return Err(format!(
            "换令牌失败：{status} — {}",
            text.chars().take(300).collect::<String>()
        ));
    }
    serde_json::from_str(&text).map_err(|e| format!("令牌应答不是合法 JSON：{e}"))
}

/// 档案界面的提供方清单
#[tauri::command]
pub fn oauth_providers() -> Vec<OAuthProvider> {
    providers()
}

/// 官方订阅登录：开浏览器授权 → 本地回调接授权码 → 换令牌 → 写进档案的凭据目标。
/// 密钥落 keyring（这里直接写），返回值只带目标与建议值，界面拿去填档案并提示保存
#[tauri::command]
pub async fn oauth_login(
    app: AppHandle,
    provider_id: String,
    credential_service: Option<String>,
    credential_user: Option<String>,
) -> Result<LoginOutcome, String> {
    let meta = providers()
        .into_iter()
        .find(|provider| provider.id == provider_id)
        .ok_or_else(|| "没有这个登录服务。".to_string())?;
    // 凭据目标：档案 draft 里给了就用它（保存档案时目标跟着落盘）；缺省生成一组
    let service = credential_service
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| format!("aglab.oauth.{}", meta.id));
    let user = credential_user
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "oauth".to_string());

    let verifier = make_verifier();
    let challenge = challenge_of(&verifier);
    let state = verifier[..16].to_string();

    // 回调口：OpenAI 的 redirect_uri 注册死了 localhost:1455/auth/callback
    // （Codex CLI 同款）——随机端口会被 authorize 直接判 Invalid authorize request。
    // 其余提供方接受任意 localhost 端口，照旧随机
    let (listener, redirect) = if meta.id == "openai-chatgpt" {
        let listener = TcpListener::bind(("127.0.0.1", 1455)).map_err(|_| {
            "回调端口 1455 被占用：ChatGPT 登录固定用这个端口。若正跑着 Codex CLI，先退出它再试。".to_string()
        })?;
        (listener, "http://localhost:1455/auth/callback".to_string())
    } else {
        let listener =
            TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("回调端口监听失败：{e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("回调端口读取失败：{e}"))?
            .port();
        (listener, format!("http://localhost:{port}/callback"))
    };
    let redirect_encoded = {
        let mut out = String::new();
        for byte in redirect.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                out.push(byte as char);
            } else {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
        out
    };

    let authorize = match meta.id {
        "anthropic-claude" => format!(
            "https://claude.ai/oauth/authorize?client_id={ANTHROPIC_CLIENT_ID}&response_type=code&redirect_uri={redirect_encoded}&scope=oidc%20profile&code_challenge={challenge}&code_challenge_method=S256&state={state}"
        ),
        "openrouter" => format!(
            "https://openrouter.ai/auth?callback_url={redirect_encoded}&code_challenge={challenge}&code_challenge_method=S256"
        ),
        "openai-chatgpt" => format!(
            "https://auth.openai.com/oauth/authorize?response_type=code&client_id={OPENAI_CLIENT_ID}&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback&scope=openid%20profile%20email%20offline_access%20api.connectors.read%20api.connectors.invoke&code_challenge={challenge}&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&state={state}&originator=codex_cli_rs"
        ),
        "github-copilot" => {
            return Err("Copilot 走设备码流程：请改用「Copilot 设备码登录」入口。".into());
        }
        _ => return Err("没有这个登录服务。".into()),
    };

    app.opener()
        .open_url(authorize, None::<&str>)
        .map_err(|e| format!("打开浏览器失败：{e}"))?;

    // OAuth 出站跟随全局代理绑定（github.com / 中转直连不通的网络靠它出去）
    let proxy_default = crate::config::load(&app).proxy_default;
    let login = move || -> Result<LoginOutcome, String> {
        // OpenRouter 不透传 state（防伪造由 PKCE verifier 承担），这里不回验；
        // Anthropic / ChatGPT 的回调带 state，照常回验防串站
        let expected = (meta.id != "openrouter").then_some(state.as_str());
        let code = wait_for_code_on(listener, expected)?;
        let payload = match meta.id {
            "anthropic-claude" => exchange_token(
                "https://console.anthropic.com/v1/oauth/token",
                json!({
                    "grant_type": "authorization_code",
                    "client_id": ANTHROPIC_CLIENT_ID,
                    "code": code,
                    "redirect_uri": redirect,
                    "code_verifier": verifier,
                    "state": state,
                }),
                &proxy_default,
            )?,
            "openai-chatgpt" => exchange_token_form(
                "https://auth.openai.com/oauth/token",
                &[
                    ("grant_type", "authorization_code".into()),
                    ("client_id", OPENAI_CLIENT_ID.into()),
                    ("code", code),
                    ("redirect_uri", redirect),
                    ("code_verifier", verifier),
                ],
                &proxy_default,
            )?,
            _ => exchange_token(
                // 2026-10 核实：换令牌服务商是 /auth/keys（官方文档现口径）。旧的
                // /auth/exchange 已下线，打它永远 404——授权明明成功，死在最后一步
                "https://openrouter.ai/api/v1/auth/keys",
                json!({
                    "code": code,
                    "code_verifier": verifier,
                    // 第一步 authorize 用的就是 S256，按文档带上同款
                    "code_challenge_method": "S256",
                }),
                &proxy_default,
            )?,
        };
        let key_field = match meta.id {
            "openrouter" => "key",
            _ => "access_token",
        };
        let token = payload[key_field]
            .as_str()
            .ok_or_else(|| format!("令牌应答里没有 {key_field}。"))?
            .to_string();
        // 落位护栏：点名的槽位里已有密钥就改道专属槽位，绝不覆盖用户现有的 key
        let (service, user, notice) = oauth_slot_guard(service, user, meta.id);
        keyring::Entry::new(&service, &user)
            .map_err(|e| format!("凭据条目初始化失败：{e}"))?
            .set_password(&token)
            .map_err(|e| format!("写入凭据失败：{e}"))?;
        Ok(LoginOutcome {
            base_url: meta.base_url,
            api_format: meta.api_format,
            models_hint: meta.models_hint,
            credential_service: service,
            credential_user: user,
            notice,
        })
    };

    tauri::async_runtime::spawn_blocking(login)
        .await
        .map_err(|e| format!("登录任务失败：{e}"))?
}

// ---- form 编码的令牌交换（OpenAI 的 auth 服务商只收 form）----

fn form_encode(pairs: &[(&str, String)]) -> String {
    let mut out = String::new();
    for (key, value) in pairs {
        if !out.is_empty() {
            out.push('&');
        }
        let piece = |text: &str, out: &mut String| {
            for byte in text.bytes() {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                    out.push(byte as char);
                } else {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
        };
        piece(key, &mut out);
        out.push('=');
        piece(value, &mut out);
    }
    out
}

/// form 编码版的令牌交换。OpenAI 的 auth.openai.com 收这个格式
pub(crate) fn exchange_token_form(
    token_url: &str,
    pairs: &[(&str, String)],
    proxy_default: &str,
) -> Result<Value, String> {
    let body = form_encode(pairs);
    let agent = oauth_agent(proxy_default)?;
    // 同 exchange_token：状态码不当错误，正文留给下面的 status 分支
    let mut response = agent
        .post(token_url)
        .config()
        .http_status_as_error(false)
        .build()
        .header("content-type", "application/x-www-form-urlencoded")
        .send(body)
        .map_err(|e| format!("换令牌失败：{e}"))?;
    let status = response.status();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("读令牌应答失败：{e}"))?;
    if !status.is_success() {
        return Err(format!(
            "换令牌失败：{status} — {}",
            text.chars().take(300).collect::<String>()
        ));
    }
    serde_json::from_str(&text).map_err(|e| format!("令牌应答不是合法 JSON：{e}"))
}
// ---- 设备码流程（GitHub Copilot）：两阶段命令，中间把 user_code 亮给用户 ----

/// 设备码流程的第一阶段产出：user_code 给用户抄进浏览器，device_code 留给轮询命令
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStart {
    pub user_code: String,
    pub verification_uri: String,
    pub device_code: String,
    /// 建议的轮询间隔（秒），来自 GitHub 的响应
    pub interval: u64,
}

/// Copilot 设备码流程·第一步：要一个设备码，开浏览器到 github.com/login/device。
/// 拿到 user_code 亮给用户，然后调 oauth_device_poll 等用户确认
#[tauri::command]
pub async fn oauth_device_start(
    app: AppHandle,
    provider_id: String,
) -> Result<DeviceStart, String> {
    if provider_id != "github-copilot" {
        return Err("设备码流程目前只有 GitHub Copilot 在用。".into());
    }
    // OAuth 出站跟随全局代理绑定（github.com 直连不通的网络靠它出去）
    let proxy_default = crate::config::load(&app).proxy_default;
    let started = tauri::async_runtime::spawn_blocking(move || {
        let agent = oauth_agent(&proxy_default)?;
        let body = form_encode(&[("client_id", COPILOT_CLIENT_ID.into())]);
        let mut response = agent
            .post("https://github.com/login/device/code")
            .config()
            .http_status_as_error(false)
            .build()
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .send(body)
            .map_err(|e| {
                format!(
                    "申请设备码失败：{e}。出站按「全局代理绑定 → 系统代理环境变量 → 直连」取路；\
                     直连 github.com 不通时，请在「设置 → 代理」把全局默认指到一条可用代理，\
                     或从带 HTTPS_PROXY 的终端启动 aglab 后重试。"
                )
            })?;
        let status = response.status();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("读设备码应答失败：{e}"))?;
        if !status.is_success() {
            return Err(format!("申请设备码失败：{status} — {text}"));
        }
        let payload: Value =
            serde_json::from_str(&text).map_err(|e| format!("设备码应答不是合法 JSON：{e}"))?;
        let device_code = payload["device_code"]
            .as_str()
            .ok_or("设备码应答里没有 device_code。")?
            .to_string();
        let user_code = payload["user_code"]
            .as_str()
            .ok_or("设备码应答里没有 user_code。")?
            .to_string();
        let verification_uri = payload["verification_uri"]
            .as_str()
            .unwrap_or("https://github.com/login/device")
            .to_string();
        let interval = payload["interval"].as_u64().unwrap_or(5);
        Ok(DeviceStart {
            user_code,
            verification_uri,
            device_code,
            interval,
        })
    })
    .await
    .map_err(|e| format!("设备码任务失败：{e}"))??;

    app.opener()
        .open_url(started.verification_uri.clone(), None::<&str>)
        .map_err(|e| format!("打开浏览器失败：{e}"))?;
    Ok(started)
}

/// Copilot 设备码流程·第二步：拿 device_code 轮询令牌服务商，直到用户在浏览器里
/// 点完授权（成功）、明确拒绝（access_denied）或设备码过期（expired_token）。
/// 成功换到 ghu_ 主令牌后，当场换一份短期 Copilot token 验证通路，再写进 keyring
#[tauri::command]
pub async fn oauth_device_poll(
    app: AppHandle,
    provider_id: String,
    device_code: String,
    interval: Option<u64>,
    credential_service: Option<String>,
    credential_user: Option<String>,
) -> Result<LoginOutcome, String> {
    let meta = providers()
        .into_iter()
        .find(|provider| provider.id == "github-copilot")
        .ok_or_else(|| "没有这个登录服务。".to_string())?;
    let _ = provider_id;
    let service = credential_service
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| format!("aglab.oauth.{}", meta.id));
    let user = credential_user
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "oauth".to_string());

    // OAuth 出站跟随全局代理绑定（github.com 直连不通的网络靠它出去）
    let proxy_default = crate::config::load(&app).proxy_default;
    let login = move || -> Result<LoginOutcome, String> {
        let agent = oauth_agent(&proxy_default)?;
        let poll_interval = Duration::from_secs(interval.unwrap_or(5).max(2));
        let deadline = Instant::now() + Duration::from_secs(900);
        let ghu = loop {
            if Instant::now() >= deadline {
                return Err("设备码已超时（15 分钟），重新点一次登录吧。".into());
            }
            std::thread::sleep(poll_interval);
            let body = form_encode(&[
                ("client_id", COPILOT_CLIENT_ID.into()),
                ("device_code", device_code.clone()),
                (
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:device_code".into(),
                ),
            ]);
            let mut response = agent
                .post("https://github.com/login/oauth/access_token")
                .config()
                .http_status_as_error(false)
                .build()
                .header("content-type", "application/x-www-form-urlencoded")
                .header("accept", "application/json")
                .send(body)
                .map_err(|e| format!("轮询令牌失败：{e}"))?;
            let status = response.status();
            let text = response
                .body_mut()
                .read_to_string()
                .map_err(|e| format!("读轮询应答失败：{e}"))?;
            if !status.is_success() {
                return Err(format!("轮询令牌失败：{status} — {text}"));
            }
            let payload: Value =
                serde_json::from_str(&text).map_err(|e| format!("轮询应答不是合法 JSON：{e}"))?;
            if let Some(err) = payload["error"].as_str() {
                match err {
                    "authorization_pending" => continue,
                    "slow_down" => {
                        std::thread::sleep(Duration::from_secs(5));
                        continue;
                    }
                    "expired_token" => return Err("设备码过期了，重新点一次登录。".into()),
                    "access_denied" => return Err("授权在 GitHub 那边被拒绝了。".into()),
                    other => return Err(format!("GitHub 回了 {other}，登录中断。")),
                }
            }
            break payload["access_token"]
                .as_str()
                .ok_or("轮询应答里没有 access_token。")?
                .to_string();
        };

        // 登录完成当场验一次通路：拿 ghu 换短期 Copilot token，失败就整体失败，
        // 不留一个"看起来登录了但一条请求都发不出去"的半成品
        let _ = copilot_access_token(&ghu, &proxy_default)?;

        // 落位护栏：同 oauth_login，绝不顶掉点名槽位里已有的密钥
        let (service, user, notice) = oauth_slot_guard(service, user, meta.id);
        keyring::Entry::new(&service, &user)
            .map_err(|e| format!("凭据条目初始化失败：{e}"))?
            .set_password(&ghu)
            .map_err(|e| format!("写入凭据失败：{e}"))?;
        Ok(LoginOutcome {
            base_url: meta.base_url,
            api_format: meta.api_format,
            models_hint: meta.models_hint,
            credential_service: service,
            credential_user: user,
            notice,
        })
    };

    tauri::async_runtime::spawn_blocking(login)
        .await
        .map_err(|e| format!("轮询任务失败：{e}"))?
}

// ---- Copilot 短期令牌的动态换发 ----

thread_local! {
    /// ghu 主令牌 → (短期 Copilot token, 有效截止时刻)。请求线每次都要用它，
    /// 缓存到过期前 5 分钟，避免每发都多一次 GitHub 往返
    static COPILOT_TOKENS: std::cell::RefCell<std::collections::HashMap<String, (String, Instant)>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// 把 ghu 主令牌换成短期 Copilot token（带缓存：过期前 5 分钟视为已过期）。
/// base_url 指到 api.githubcopilot.com 的档案，请求线在发请求前走这一步
pub fn copilot_access_token(ghu: &str, proxy_default: &str) -> Result<String, String> {
    let cached = COPILOT_TOKENS.with(|tokens| {
        tokens
            .borrow()
            .get(ghu)
            .cloned()
            .filter(|(_, until)| Instant::now() < *until)
    });
    if let Some((token, _)) = cached {
        return Ok(token);
    }
    let agent = oauth_agent(proxy_default)?;
    // 状态码不当错误：401 要落到下面的专属提示（"主令牌已失效"），而不是
    // 被 ureq 抢先转成一句没有正文的 Err
    let mut response = agent
        .get("https://api.github.com/copilot_internal/v2/token")
        .config()
        .http_status_as_error(false)
        .build()
        .header("authorization", format!("Bearer {ghu}"))
        .header("accept", "application/json")
        .header("user-agent", "aglab")
        .header("editor-version", "vscode/1.95.0")
        .header("editor-plugin-version", "copilot-chat/0.24.0")
        .call()
        .map_err(|e| format!("换 Copilot 短期令牌失败：{e}"))?;
    let status = response.status();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("读 Copilot 令牌应答失败：{e}"))?;
    if status.as_u16() == 401 {
        COPILOT_TOKENS.with(|tokens| tokens.borrow_mut().remove(ghu));
        return Err("GitHub 主令牌已失效：请重新走一次 Copilot 设备码登录。".into());
    }
    if !status.is_success() {
        return Err(format!("换 Copilot 短期令牌失败：{status} — {text}"));
    }
    let payload: Value =
        serde_json::from_str(&text).map_err(|e| format!("Copilot 令牌应答不是合法 JSON：{e}"))?;
    let token = payload["token"]
        .as_str()
        .ok_or("Copilot 令牌应答里没有 token。")?
        .to_string();
    let expires_at = payload["expires_at"].as_i64().unwrap_or(0);
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let seconds_left = expires_at - now_secs;
    let valid_until = Instant::now()
        + Duration::from_secs(seconds_left.saturating_sub(300).max(60) as u64);
    COPILOT_TOKENS.with(|tokens| {
        tokens
            .borrow_mut()
            .insert(ghu.to_string(), (token.clone(), valid_until));
    });
    Ok(token)
}
