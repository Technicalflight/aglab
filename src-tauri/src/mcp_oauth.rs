//! MCP OAuth（http 型远程服务器）：MCP 授权规范的最小闭环。
//!
//! 流程（RFC 9728 资源发现 → RFC 7591 动态注册 → PKCE 授权码）：
//! 1. 从服务 URL 派生保护资源元数据候选地址，读出 `authorization_servers`；
//!    全部 404 时退回"服务自己就是授权服务器"（同源 AS）；
//! 2. 读授权服务器元数据（authorize/token/registration 服务商）；
//! 3. 没有 client_id 就动态注册一个公共客户端（`token_endpoint_auth_method: none`）；
//! 4. 浏览器 PKCE 授权（本地回调收 code，state 回验），表单换令牌；
//! 5. 令牌（access + refresh + 过期时刻 + client_id）以 JSON blob 存 keyring
//!    （服务 `aglab.mcp.<server_id>`）——与 API key 同一处住所，不进 config.json。
//!
//! 传输侧（mcp.rs）构造 HTTP 传输时经 [`bearer_token`] 取 Bearer：过期用
//! refresh_token 静默续期；用户在 headers 里手写了 Authorization 时不覆盖。
//!
//! 边界：MCP 的 HTTP 传输目前直连（不走代理池），OAuth 的发现/换取同口径直连；
//! 网络不通的服务器在连接时报错，与既有行为一致。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

use crate::config;

/// 令牌 blob 的形状。refresh_token 缺失 = 服务器没发，过期只能重登
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TokenBlob {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// 过期时刻（毫秒）。已扣除 60 秒提前量
    pub expires_at_ms: u64,
    pub client_id: String,
    pub token_endpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
}

fn keyring_entry(server_id: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(&format!("aglab.mcp.{server_id}"), "oauth")
        .map_err(|e| format!("凭据条目初始化失败：{e}"))
}

fn read_blob(server_id: &str) -> Option<TokenBlob> {
    let entry = keyring_entry(server_id).ok()?;
    let text = entry.get_password().ok()?;
    serde_json::from_str(&text).ok()
}

fn write_blob(server_id: &str, blob: &TokenBlob) -> Result<(), String> {
    let text = serde_json::to_string(blob).map_err(|e| e.to_string())?;
    keyring_entry(server_id)?
        .set_password(&text)
        .map_err(|e| format!("写入凭据失败：{e}"))
}

fn delete_blob(server_id: &str) -> bool {
    keyring_entry(server_id)
        .and_then(|entry| entry.delete_credential().map_err(|e| e.to_string()))
        .is_ok()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 给一个服务/AS 根地址生成 well-known 候选：先插到路径前（RFC 9728 的口径），
/// 再退回根下。有路径的服务器（https://host/mcp）两个都要试
fn well_known_candidates(base: &str, file: &str) -> Vec<String> {
    let base = base.trim_end_matches('/');
    let mut out = Vec::new();
    if let Some((origin, rest)) = base.split_once("://") {
        let (host, tail) = rest.split_once('/').map_or((rest, ""), |(h, tl)| (h, tl));
        if !tail.is_empty() {
            out.push(format!("{origin}://{host}/.well-known/{file}/{tail}"));
        }
        out.push(format!("{origin}://{host}/.well-known/{file}"));
    }
    out
}

/// 直连 GET 一个 JSON（与 MCP 的 HTTP 传输同口径：不走代理池）。
/// 非成功状态返回 None（发现层的候选地址本来就大多 404，不当错误）
fn get_json(url: &str) -> Option<Value> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .new_agent();
    let mut response = agent
        .get(url)
        .header("accept", "application/json")
        .call()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = response.body_mut().read_to_string().ok()?;
    serde_json::from_str(&body).ok()
}

/// 授权服务器的元数据形状。缺 authorize/token 服务商的不算数
#[derive(Debug, Clone)]
pub struct AsMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
}

/// 从保护资源/AS 元数据 JSON 里挑授权服务器地址
pub fn parse_authorization_servers(metadata: &Value) -> Vec<String> {
    metadata["authorization_servers"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 从 AS 元数据 JSON 里读服务商。authorize/token 缺任一个返回 None
pub fn parse_as_metadata(metadata: &Value) -> Option<AsMetadata> {
    let authorization_endpoint = metadata["authorization_endpoint"].as_str()?.to_string();
    let token_endpoint = metadata["token_endpoint"].as_str()?.to_string();
    Some(AsMetadata {
        authorization_endpoint,
        token_endpoint,
        registration_endpoint: metadata["registration_endpoint"]
            .as_str()
            .map(str::to_string),
        scopes_supported: metadata["scopes_supported"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// 发现授权服务器：保护资源元数据 → 第一个 AS → AS 元数据。
/// 保护资源整条路 404 时，退回"服务自己就是 AS"（同源元数据）
pub fn discover_as_metadata(server_url: &str) -> Result<AsMetadata, String> {
    for candidate in well_known_candidates(server_url, "oauth-protected-resource") {
        if let Some(metadata) = get_json(&candidate) {
            let servers = parse_authorization_servers(&metadata);
            if let Some(as_url) = servers.first() {
                for candidate in well_known_candidates(as_url, "oauth-authorization-server") {
                    if let Some(metadata) = get_json(&candidate) {
                        if let Some(meta) = parse_as_metadata(&metadata) {
                            return Ok(meta);
                        }
                    }
                }
                return Err(format!(
                    "授权服务器 {as_url} 的元数据读不到（authorize/token 服务商缺失）。"
                ));
            }
        }
    }
    // 退路：服务自己就是授权服务器（MCP 规范允许这种最简形态）
    for candidate in well_known_candidates(server_url, "oauth-authorization-server") {
        if let Some(metadata) = get_json(&candidate) {
            if let Some(meta) = parse_as_metadata(&metadata) {
                return Ok(meta);
            }
        }
    }
    Err(format!(
        "发现不到授权服务器：{server_url} 的保护资源与同源元数据都没回有效 JSON。"
    ))
}

/// 动态客户端注册（RFC 7591）：公共客户端（无 secret 检查），返回 client_id
pub fn dynamic_register(
    registration_endpoint: &str,
    redirect_uri: &str,
    proxy_default: &str,
) -> Result<String, String> {
    let payload = serde_json::json!({
        "client_name": "aglab",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let agent = crate::oauth::oauth_agent(proxy_default)?;
    let mut response = agent
        .post(registration_endpoint)
        .config()
        .http_status_as_error(false)
        .build()
        .header("content-type", "application/json")
        .send_json(&payload)
        .map_err(|e| format!("动态注册失败：{e}"))?;
    let status = response.status();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("读注册应答失败：{e}"))?;
    if !status.is_success() {
        return Err(format!(
            "动态注册失败：{status} — {}",
            text.chars().take(300).collect::<String>()
        ));
    }
    let parsed: Value =
        serde_json::from_str(&text).map_err(|e| format!("注册应答不是 JSON：{e}"))?;
    parsed["client_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "注册应答里没有 client_id。".to_string())
}

/// 授权地址：PKCE + state + resource（RFC 8707，服务器不认就忽略）
pub fn authorize_url(
    as_meta: &AsMetadata,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
    resource: Option<&str>,
) -> String {
    let scopes = if as_meta.scopes_supported.is_empty() {
        String::new()
    } else {
        format!(
            "&scope={}",
            as_meta.scopes_supported.join(" ").replace(' ', "%20")
        )
    };
    let resource = resource
        .map(|r| format!("&resource={}", r.replace(':', "%3A").replace('/', "%2F")))
        .unwrap_or_default();
    format!(
        "{}?response_type=code&client_id={client_id}&redirect_uri={}\
         &code_challenge={challenge}&code_challenge_method=S256&state={state}{scopes}{resource}",
        as_meta.authorization_endpoint,
        redirect_uri.replace(':', "%3A").replace('/', "%2F"),
    )
}

/// 传输侧的 Bearer：读 blob，过期（提前 60 秒）用 refresh_token 静默续期。
/// 返回 None = 这台服务器没配 OAuth（headers 里的静态凭据照旧生效）
pub fn bearer_token(server: &config::McpServer) -> Result<Option<String>, String> {
    if !server.oauth {
        return Ok(None);
    }
    let Some(mut blob) = read_blob(&server.id) else {
        return Ok(None);
    };
    if now_ms() < blob.expires_at_ms {
        return Ok(Some(blob.access_token));
    }
    let Some(refresh) = blob.refresh_token.clone() else {
        return Err(format!(
            "「{}」的 OAuth 令牌已过期且没有 refresh_token，请重新登录。",
            server.name
        ));
    };
    let mut pairs = vec![
        ("grant_type", "refresh_token".to_string()),
        ("client_id", blob.client_id.clone()),
        ("refresh_token", refresh),
    ];
    if let Some(secret) = &blob.client_secret {
        pairs.push(("client_secret", secret.clone()));
    }
    let payload = crate::oauth::exchange_token_form(&blob.token_endpoint, &pairs, "")?;
    blob.access_token = payload["access_token"]
        .as_str()
        .ok_or_else(|| "刷新应答里没有 access_token。".to_string())?
        .to_string();
    if let Some(rotated) = payload["refresh_token"].as_str() {
        blob.refresh_token = Some(rotated.to_string());
    }
    if let Some(seconds) = payload["expires_in"].as_u64() {
        blob.expires_at_ms = now_ms() + seconds.saturating_sub(60) * 1000;
    }
    write_blob(&server.id, &blob)?;
    Ok(Some(blob.access_token))
}

/// 登录结果给界面的一句话
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOAuthOutcome {
    pub server: String,
    pub scopes: Vec<String>,
    pub client_id: String,
}

/// 浏览器 PKCE 登录。**命令是 async 的**：整个流程含等回调（最长 5 分钟），
/// 绝不能卡住主线程——oauth_login 同款先例
#[tauri::command]
pub async fn mcp_oauth_login(app: AppHandle, id: String) -> Result<McpOAuthOutcome, String> {
    let server = crate::mcp::find_server(&app, &id)?;
    if server.transport != "http" || server.url.trim().is_empty() {
        return Err("OAuth 只支持 http 型服务器（要有服务地址）。".to_string());
    }
    let server_url = server.url.trim().to_string();
    let server_id = server.id.clone();
    let server_name = server.name.clone();

    let verifier = crate::oauth::make_verifier();
    let challenge = crate::oauth::challenge_of(&verifier);
    let state = verifier[..16].to_string();

    let login = move || -> Result<McpOAuthOutcome, String> {
        // 回调口随机：MCP 服务器没有注册死端口一说，动态注册时把 redirect 报给它们
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .map_err(|e| format!("回调端口监听失败：{e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("回调端口读取失败：{e}"))?
            .port();
        let redirect = format!("http://localhost:{port}/callback");

        let as_meta = discover_as_metadata(&server_url)?;

        // 先查已有 blob 的 client_id：同一台服务器重复登录不重新注册
        let client_id = match read_blob(&server_id) {
            Some(blob) if !blob.client_id.is_empty() => blob.client_id,
            _ => match &as_meta.registration_endpoint {
                Some(endpoint) => dynamic_register(endpoint, &redirect, "")?,
                None => {
                    return Err(
                        "授权服务器没有提供动态注册服务商，也没有已注册的 client_id。\
                         请在服务器控制台手动注册一个客户端（redirect 回调到本地随机端口）。"
                            .to_string(),
                    )
                }
            },
        };

        let authorize = authorize_url(
            &as_meta,
            &client_id,
            &redirect,
            &challenge,
            &state,
            Some(&server_url),
        );
        app.opener()
            .open_url(authorize, None::<&str>)
            .map_err(|e| format!("打开浏览器失败：{e}"))?;

        let code = crate::oauth::wait_for_code_on(listener, Some(&state))?;
        let mut pairs = vec![
            ("grant_type", "authorization_code".to_string()),
            ("client_id", client_id.clone()),
            ("code", code),
            ("redirect_uri", redirect),
            ("code_verifier", verifier),
        ];
        if let Some(blob) = read_blob(&server_id) {
            if let Some(secret) = &blob.client_secret {
                pairs.push(("client_secret", secret.clone()));
            }
        }
        let payload = crate::oauth::exchange_token_form(&as_meta.token_endpoint, &pairs, "")?;
        let access_token = payload["access_token"]
            .as_str()
            .ok_or_else(|| "令牌应答里没有 access_token。".to_string())?
            .to_string();
        let expires_in = payload["expires_in"].as_u64().unwrap_or(3600);
        let blob = TokenBlob {
            access_token,
            refresh_token: payload["refresh_token"].as_str().map(str::to_string),
            expires_at_ms: now_ms() + expires_in.saturating_sub(60) * 1000,
            client_id,
            token_endpoint: as_meta.token_endpoint.clone(),
            client_secret: None,
        };
        write_blob(&server_id, &blob)?;
        Ok(McpOAuthOutcome {
            server: server_name,
            scopes: as_meta.scopes_supported,
            client_id: blob.client_id,
        })
    };

    tauri::async_runtime::spawn_blocking(login)
        .await
        .map_err(|e| format!("MCP OAuth thread failed: {e}"))?
}

/// 登录状态：有没有 blob、什么时候过期
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOAuthStatus {
    pub logged_in: bool,
    pub expires_at_ms: Option<u64>,
}

#[tauri::command]
pub fn mcp_oauth_status(id: String) -> McpOAuthStatus {
    match read_blob(&id) {
        Some(blob) => McpOAuthStatus {
            logged_in: !blob.access_token.is_empty(),
            expires_at_ms: Some(blob.expires_at_ms),
        },
        None => McpOAuthStatus {
            logged_in: false,
            expires_at_ms: None,
        },
    }
}

/// 登出：删凭据。返回 false = 本来就没有登录记录
#[tauri::command]
pub fn mcp_oauth_logout(id: String) -> Result<bool, String> {
    Ok(delete_blob(&id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn well_known_candidates_insert_before_path_then_root() {
        let candidates =
            well_known_candidates("https://mcp.notion.com/mcp", "oauth-protected-resource");
        assert_eq!(
            candidates,
            vec![
                "https://mcp.notion.com/.well-known/oauth-protected-resource/mcp",
                "https://mcp.notion.com/.well-known/oauth-protected-resource",
            ],
            "带路径的服务器两个候选都要试"
        );
        let root_only =
            well_known_candidates("https://mcp.example.com", "oauth-authorization-server");
        assert_eq!(
            root_only,
            vec!["https://mcp.example.com/.well-known/oauth-authorization-server"],
            "无路径的服务器只有根候选"
        );
    }

    #[test]
    fn metadata_parsers_read_the_spec_fields() {
        let servers = parse_authorization_servers(&json!({
            "resource": "https://mcp.notion.com/mcp",
            "authorization_servers": ["https://mcp.notion.com/"]
        }));
        assert_eq!(servers, vec!["https://mcp.notion.com/"]);
        assert!(
            parse_authorization_servers(&json!({})).is_empty(),
            "没有字段就空表"
        );

        let meta = parse_as_metadata(&json!({
            "authorization_endpoint": "https://as.example.com/authorize",
            "token_endpoint": "https://as.example.com/token",
            "registration_endpoint": "https://as.example.com/register",
            "scopes_supported": ["read", "write"]
        }))
        .expect("完整元数据要解析得出");
        assert_eq!(
            meta.authorization_endpoint,
            "https://as.example.com/authorize"
        );
        assert_eq!(
            meta.registration_endpoint.as_deref(),
            Some("https://as.example.com/register")
        );
        assert!(
            parse_as_metadata(&json!({"token_endpoint": "https://x"})).is_none(),
            "缺 authorize 服务商不算数"
        );
    }

    #[test]
    fn authorize_url_carries_pkce_state_and_resource() {
        let meta = AsMetadata {
            authorization_endpoint: "https://as.example.com/authorize".into(),
            token_endpoint: "https://as.example.com/token".into(),
            registration_endpoint: None,
            scopes_supported: vec!["read".into()],
        };
        let url = authorize_url(
            &meta,
            "client-1",
            "http://localhost:5173/callback",
            "challenge-xyz",
            "state-abc",
            Some("https://mcp.notion.com/mcp"),
        );
        assert!(url.starts_with("https://as.example.com/authorize?"));
        for needle in [
            "response_type=code",
            "client_id=client-1",
            "redirect_uri=http%3A%2F%2Flocalhost%3A5173%2Fcallback",
            "code_challenge=challenge-xyz",
            "code_challenge_method=S256",
            "state=state-abc",
            "scope=read",
            "resource=https%3A%2F%2Fmcp.notion.com%2Fmcp",
        ] {
            assert!(url.contains(needle), "{needle} 要在 {url}");
        }
    }

    #[test]
    fn capability_expiry_and_refresh_shape() {
        // blob 往返：keyring 不进单测（要真凭据库），JSON 形状在这里钉
        let blob = TokenBlob {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at_ms: 1_234_567_890,
            client_id: "client-1".into(),
            token_endpoint: "https://as/token".into(),
            client_secret: None,
        };
        let text = serde_json::to_string(&blob).unwrap();
        let round = serde_json::from_str::<TokenBlob>(&text).unwrap();
        assert_eq!(round.access_token, "at");
        assert_eq!(round.expires_at_ms, 1_234_567_890);
    }
}
