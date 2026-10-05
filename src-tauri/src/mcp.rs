use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command as OsCommand, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::config::{self, AppConfig, McpServer};

/// 一次 JSON-RPC 往返的等待上限。npx 冷启动会慢，但也不该把一轮对话挂死。
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Clone)]
pub struct ToolInfo {
    /// 服务器自己声明的名字
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// 握手回来时那两格能力。**没声明过的那一格我们不去问**：问了必失败的那一次会以
/// `-32601` 的样子出现在工具结果里，而模型会把它当成服务器的真实回答——
/// 我们只是自己戳了它一下，还留下一条读起来像事实的噪音（design-tool-runtime.md §11）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub resources: bool,
    pub prompts: bool,
}

/// MCP 的规矩是"有能力就有一个对象（可以是空对象）"，所以判的是形状不是真假值
pub fn capabilities_of(handshake: &Value) -> Capabilities {
    let caps = &handshake["capabilities"];
    Capabilities {
        resources: caps["resources"].is_object(),
        prompts: caps["prompts"].is_object(),
    }
}

/// `resources/list` 里的一行。`uri` 是之后读它时唯一要带的东西
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceInfo {
    pub uri: String,
    pub name: String,
    pub description: String,
}

fn str_field(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

fn as_array(value: &Value, key: &str) -> Vec<Value> {
    value[key].as_array().cloned().unwrap_or_default()
}

pub fn parse_resources(listed: &Value) -> Vec<ResourceInfo> {
    as_array(listed, "resources")
        .into_iter()
        .filter_map(|item| {
            let uri = str_field(&item, "uri");
            (!uri.is_empty()).then(|| ResourceInfo {
                name: str_field(&item, "name"),
                description: str_field(&item, "description"),
                uri,
            })
        })
        .collect()
}

/// `resources/read` 的 contents → 文本。二进制**不进上下文**：base64 塞进历史是每一发
/// 都要重付的一坨字节，而模型读不懂它——只留一句"没读进来"，让它知道有这回事
pub fn render_resources_read(read: &Value) -> String {
    let parts = as_array(read, "contents");
    if parts.is_empty() {
        return "这台服务器没返回任何内容。".into();
    }
    let mut text = String::new();
    for part in &parts {
        let uri = str_field(part, "uri");
        let body = match part["text"].as_str() {
            Some(body) => body.to_string(),
            None => match part["blob"].as_str() {
                Some(_) => "（二进制内容，没读进上下文）".into(),
                None => "（这一格没有可读正文）".into(),
            },
        };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("{uri}\n{body}"));
    }
    text
}

/// `prompts/list` 里的一行。参数只保留"名字 + 是否必填"：那是模型填不填得对的唯一依据，
/// 而完整 schema 在这条路上没人执行
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptInfo {
    pub name: String,
    pub description: String,
    pub arguments: Vec<String>,
}

pub fn parse_prompts(listed: &Value) -> Vec<PromptInfo> {
    as_array(listed, "prompts")
        .into_iter()
        .filter_map(|item| {
            let name = str_field(&item, "name");
            (!name.is_empty()).then(|| PromptInfo {
                description: str_field(&item, "description"),
                arguments: as_array(&item, "arguments")
                    .iter()
                    .map(|arg| {
                        let held = str_field(arg, "name");
                        if arg["required"].as_bool().unwrap_or(false) {
                            format!("{held}（必填）")
                        } else {
                            held
                        }
                    })
                    .collect(),
                name,
            })
        })
        .collect()
}

/// `prompts/get` 返回的是一批 message。这里**不把它们当成已经发生过的对话**，
/// 而是渲染成可读的一屏——提示词是建议发的内容，不是历史（§11）
pub fn render_prompt(got: &Value) -> String {
    let mut text = String::new();
    for message in as_array(got, "messages") {
        let role = str_field(&message, "role");
        let body = match &message["content"] {
            Value::String(body) => body.clone(),
            content => match content["text"].as_str() {
                Some(body) => body.to_string(),
                None => "（这一格没有可读正文）".into(),
            },
        };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("{role}：{body}"));
    }
    if text.is_empty() {
        return "这台服务器没返回任何提示词内容。".into();
    }
    text
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 交给模型的名字带上服务器前缀，两个服务器有同名工具时也不会撞车
pub fn exposed_name(server_id: &str, tool: &str) -> String {
    format!("mcp__{}__{}", sanitize(server_id), sanitize(tool))
}

fn split_exposed(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    rest.split_once("__")
}

fn spawn_command(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> std::io::Result<Child> {
    let build = || {
        let mut spawn = OsCommand::new(command);
        // 环境先过那一份共用的约束（凭据形状的不进子进程），再把这台服务器自己配置的
        // 变量加回来：用户显式给的那几个照旧生效（MCP 服务器常常要自己的 token），
        // 但这个进程旁边躺着的 ambient 凭据不再顺手继承给一个 `npx` 包
        crate::tool_runtime::constrain::constrained(&mut spawn);
        spawn
            .args(args)
            .envs(env.iter())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    };

    match build() {
        Ok(child) => Ok(child),
        // Windows 上 npx / npm 其实是 .cmd，CreateProcess 不按 PATHEXT 找。
        // 只在直接启动失败时退到 cmd /C，能直接跑的程序仍然可以被精确终止。
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(windows)]
            {
                // 这一条走的是 cmd.exe，环境更要过一遍约束：退路不是免检的理由
                let mut spawn = OsCommand::new("cmd.exe");
                crate::tool_runtime::constrain::constrained(&mut spawn);
                spawn
                    .arg("/C")
                    .arg(command)
                    .args(args)
                    .envs(env.iter())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            }
            #[cfg(not(windows))]
            {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

/// 连接的载体。stdio 是子进程管道（异步读线程 + 队列等响应）；
/// http 是 streamable HTTP 的同步往返（POST 一发拿一答，响应可能是 JSON 或 SSE 流）
enum Transport {
    Stdio {
        child: Mutex<Child>,
        stdin: Mutex<ChildStdin>,
        inbox: Arc<Mutex<VecDeque<Value>>>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        /// initialize 响应头里服务器发的 Mcp-Session-Id，之后每个请求都要带上
        session: Mutex<Option<String>>,
    },
}

/// 从 SSE 文本里抽出 id 匹配的那条 JSON-RPC 回应。streamable HTTP 的 POST 响应
/// 可能是 `event: message\ndata: {...}` 流；MCP 服务器把整条 JSON 放在一个 data 行里。
/// 没有匹配项返回 None——调用方把空流当错误报
fn parse_sse_reply(text: &str, id: u64) -> Option<Value> {
    for line in text.lines() {
        let data = line.strip_prefix("data:").map(str::trim).unwrap_or("");
        if data.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if value.get("id").and_then(Value::as_u64) == Some(id) {
            return Some(value);
        }
    }
    None
}

/// HTTP 型的一发 JSON-RPC。回（回应 JSON、服务器新发的话题 id）；通知类没有回应，
/// 回 None。两种响应形状都接：`application/json` 单条，`text/event-stream` 走 SSE 解析。
fn http_exchange(
    url: &str,
    headers: &BTreeMap<String, String>,
    session: Option<&str>,
    payload: &Value,
    expect_reply: bool,
) -> Result<(Option<Value>, Option<String>), String> {
    // 全局超时挂在 agent 配置上：stdio 与 http 共用同一个等待上限，
    // HTTP 没有 stdio 那种"子进程自己卡死"的形态，但一个不回话的服务器同样不能把调用挂死
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(CALL_TIMEOUT))
        .build()
        .new_agent();
    let mut request = agent
        .post(url)
        .header("Content-Type", "application/json")
        // 规范要求同时声明两种可接受形状：服务器二选一回
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", PROTOCOL_VERSION);
    if let Some(id) = session {
        request = request.header("Mcp-Session-Id", id);
    }
    for (key, value) in headers {
        request = request.header(key.as_str(), value.as_str());
    }

    let mut response = request
        .send_json(payload)
        .map_err(|e| format!("请求 MCP 服务失败：{e}"))?;

    let session_id = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();

    if !response.status().is_success() {
        let status = response.status();
        let body = response.body_mut().read_to_string().unwrap_or_default();
        return Err(format!(
            "MCP 服务回了 {status}：{}",
            body.chars().take(200).collect::<String>()
        ));
    }

    if !expect_reply {
        // 通知：202 或空响应体都算送达
        return Ok((None, session_id));
    }

    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("读 MCP 服务响应失败：{e}"))?;
    if content_type.contains("text/event-stream") {
        return match parse_sse_reply(&text, payload["id"].as_u64().unwrap_or_default()) {
            Some(reply) => Ok((Some(reply), session_id)),
            None => Err("MCP 服务的 SSE 流里没有带匹配 id 的回应。".into()),
        };
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(reply) => Ok((Some(reply), session_id)),
        Err(_) => Err("MCP 服务响应不是合法的 JSON。".into()),
    }
}

/// 一个连上的 MCP 服务器。stdio：读线程把每一行 JSON 塞进队列，请求串行发出，
/// 按 id 在队列里找响应就够了；http：每次 POST 独立往返，同样串行发出
pub struct Connection {
    transport: Transport,
    tools: Mutex<Vec<ToolInfo>>,
    /// 握手那一份能力声明。它决定我们**会不会**去问 `resources/list`
    capabilities: Mutex<Capabilities>,
    serial: Mutex<()>,
    next_id: std::sync::atomic::AtomicU64,
}

impl Connection {
    fn start(server: &McpServer) -> Result<Arc<Connection>, String> {
        let transport = match server.transport.as_str() {
            "http" => {
                let url = server.url.trim().to_string();
                if url.is_empty() {
                    return Err("HTTP 型 MCP 服务缺服务地址。".into());
                }
                // OAuth 服务器：构造传输前换 Bearer（过期静默续期）。
                // 用户手写的 Authorization 头优先——手写的凭据不该被 OAuth 盖掉
                let mut headers = server.headers.clone();
                if server.oauth {
                    if let Some(token) = crate::mcp_oauth::bearer_token(server)? {
                        headers
                            .entry("Authorization".to_string())
                            .or_insert_with(|| format!("Bearer {token}"));
                    }
                }
                Transport::Http {
                    url,
                    headers,
                    session: Mutex::new(None),
                }
            }
            _ => {
                let mut child = spawn_command(&server.command, &server.args, &server.env)
                    .map_err(|e| format!("启动 {} 失败：{e}", server.command))?;

                let stdin = child
                    .stdin
                    .take()
                    .ok_or_else(|| "子进程没有标准输入，无法通信。".to_string())?;
                let stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| "子进程没有标准输出，无法通信。".to_string())?;
                let stderr = child.stderr.take();

                let (tx, rx): (Sender<String>, Receiver<String>) = mpsc::channel();
                thread::spawn(move || {
                    let mut reader = BufReader::new(stdout);
                    let mut line = String::new();
                    loop {
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => return,
                            Ok(_) => {}
                        }
                        let trimmed = line.trim().to_string();
                        line.clear();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if tx.send(trimmed).is_err() {
                            return;
                        }
                    }
                });

                if let Some(stderr) = stderr {
                    thread::spawn(move || {
                        // 只消费不显示：stderr 管道堵死会把子进程卡在那里
                        let mut reader = BufReader::new(stderr);
                        let mut line = String::new();
                        while reader.read_line(&mut line).unwrap_or(0) > 0 {
                            line.clear();
                        }
                    });
                }

                // 读线程 → 队列：只收合法 JSON，服务器打的纯文本日志丢掉
                let inbox: Arc<Mutex<VecDeque<Value>>> = Arc::new(Mutex::new(VecDeque::new()));
                let queue = Arc::clone(&inbox);
                thread::spawn(move || {
                    for raw in rx {
                        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                            if let Ok(mut queue) = queue.lock() {
                                queue.push_back(value);
                            }
                        }
                    }
                });

                Transport::Stdio {
                    child: Mutex::new(child),
                    stdin: Mutex::new(stdin),
                    inbox,
                }
            }
        };

        let conn = Arc::new(Connection {
            transport,
            tools: Mutex::new(Vec::new()),
            capabilities: Mutex::new(Capabilities::default()),
            serial: Mutex::new(()),
            next_id: std::sync::atomic::AtomicU64::new(1),
        });

        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "aglab", "version": env!("CARGO_PKG_VERSION") },
        });
        // 握手的返回以前是被扔掉的，于是"这台到底有没有 resources"没人知道。留下来，
        // 后面每一问都以它为依据（§11）
        let handshake = conn.request("initialize", params)?;
        if let Ok(mut slot) = conn.capabilities.lock() {
            *slot = capabilities_of(&handshake);
        }
        conn.notify("notifications/initialized")?;

        let listed = conn.request("tools/list", json!({}))?;
        let tools: Vec<ToolInfo> = listed["tools"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|tool| {
                let name = tool["name"].as_str()?.to_string();
                Some(ToolInfo {
                    description: tool["description"].as_str().unwrap_or_default().to_string(),
                    input_schema: tool["inputSchema"].clone(),
                    name,
                })
            })
            .collect();
        if let Ok(mut slot) = conn.tools.lock() {
            *slot = tools;
        }

        Ok(conn)
    }

    fn stdio_send(stdin: &Mutex<ChildStdin>, payload: Value) -> Result<(), String> {
        let mut stdin = stdin
            .lock()
            .map_err(|_| "输入锁被卡住。".to_string())?;
        let text = serde_json::to_string(&payload).map_err(|e| e.to_string())?;
        stdin
            .write_all(text.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush())
            .map_err(|e| format!("写入子进程失败：{e}"))
    }

    fn stdio_wait(inbox: &Arc<Mutex<VecDeque<Value>>>, id: u64) -> Result<Value, String> {
        let deadline = Instant::now() + CALL_TIMEOUT;
        while Instant::now() < deadline {
            let found = {
                let mut queue = inbox
                    .lock()
                    .map_err(|_| "队列锁被卡住。".to_string())?;
                let index = queue
                    .iter()
                    .position(|item| item.get("id").and_then(Value::as_u64) == Some(id));
                index.and_then(|index| queue.remove(index))
            };
            if let Some(value) = found {
                return Ok(value);
            }
            thread::sleep(Duration::from_millis(20));
        }
        Err("等 MCP 服务器回应超时了。".into())
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let _serial = self
            .serial
            .lock()
            .map_err(|_| "请求锁被卡住。".to_string())?;
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let payload = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });

        let reply = match &self.transport {
            Transport::Stdio { stdin, inbox, .. } => {
                Self::stdio_send(stdin, payload)?;
                Self::stdio_wait(inbox, id)?
            }
            Transport::Http { url, headers, session } => {
                let current = session.lock().ok().and_then(|held| held.clone());
                let (reply, new_session) =
                    http_exchange(url, headers, current.as_deref(), &payload, true)?;
                if let Some(new) = new_session {
                    if let Ok(mut held) = session.lock() {
                        *held = Some(new);
                    }
                }
                reply.ok_or_else(|| "MCP 服务回了空回应。".to_string())?
            }
        };

        if let Some(error) = reply.get("error") {
            let message = error["message"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| error.to_string());
            return Err(format!("MCP 报错：{message}"));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    fn notify(&self, method: &str) -> Result<(), String> {
        let payload = json!({ "jsonrpc": "2.0", "method": method });
        match &self.transport {
            Transport::Stdio { stdin, .. } => Self::stdio_send(stdin, payload),
            Transport::Http { url, headers, session } => {
                let current = session.lock().ok().and_then(|held| held.clone());
                let (_, new_session) =
                    http_exchange(url, headers, current.as_deref(), &payload, false)?;
                if let Some(new) = new_session {
                    if let Ok(mut held) = session.lock() {
                        *held = Some(new);
                    }
                }
                Ok(())
            }
        }
    }

    fn tools(&self) -> Vec<ToolInfo> {
        self.tools
            .lock()
            .map(|tools| tools.clone())
            .unwrap_or_default()
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.lock().map(|held| *held).unwrap_or_default()
    }

    /// 只有服务器声明过那一格才去问。判据写在这一处，不在各调用点各判一遍
    fn wants(&self, which: impl FnOnce(Capabilities) -> bool, named: &str) -> Result<(), String> {
        if which(self.capabilities()) {
            return Ok(());
        }
        Err(format!("这台服务器没说它有{named}（握手时的 capabilities 里没有这一格）。"))
    }

    fn list_resources(&self) -> Result<String, String> {
        self.wants(|caps| caps.resources, "资源")?;
        let listed = self.request("resources/list", json!({ "cursor": Value::Null }))?;
        Ok(render_list(&parse_resources(&listed), |item| {
            format!("{}｜{}｜{}", item.uri, item.name, item.description)
        }))
    }

    fn read_resource(&self, uri: &str) -> Result<String, String> {
        self.wants(|caps| caps.resources, "资源")?;
        let read = self.request("resources/read", json!({ "uri": uri }))?;
        Ok(render_resources_read(&read))
    }

    fn list_prompts(&self) -> Result<String, String> {
        self.wants(|caps| caps.prompts, "提示词")?;
        let listed = self.request("prompts/list", json!({ "cursor": Value::Null }))?;
        Ok(render_list(&parse_prompts(&listed), |item| {
            let arguments = if item.arguments.is_empty() {
                "无参数".into()
            } else {
                item.arguments.join("、")
            };
            format!("{}｜{}｜参数：{}", item.name, item.description, arguments)
        }))
    }

    fn get_prompt(&self, name: &str, arguments: &Value) -> Result<String, String> {
        self.wants(|caps| caps.prompts, "提示词")?;
        let got = self.request("prompts/get", json!({ "name": name, "arguments": arguments }))?;
        Ok(render_prompt(&got))
    }

    fn call(&self, tool: &str, arguments: Value) -> Result<String, String> {
        let result = self.request(
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        )?;

        let mut text = String::new();
        if let Some(parts) = result["content"].as_array() {
            for part in parts {
                if !text.is_empty() {
                    text.push('\n');
                }
                match part["type"].as_str().unwrap_or_default() {
                    "text" => text.push_str(part["text"].as_str().unwrap_or_default()),
                    other => text.push_str(&format!("（{other} 类型的内容暂未支持）")),
                }
            }
        }

        if result["isError"].as_bool().unwrap_or(false) {
            return Err(if text.is_empty() {
                "工具执行失败。".into()
            } else {
                text
            });
        }
        Ok(text)
    }

    fn shutdown(&self) {
        let Transport::Stdio { child, stdin, .. } = &self.transport else {
            // HTTP 型没有进程可收：无状态往返，断开就是不再发请求
            return;
        };
        let _ = Self::stdio_send(stdin, json!({ "jsonrpc": "2.0", "method": "exit" }));
        // npx 这类会被包在 cmd.exe 外面，只杀 cmd 那一层会留下常驻的 node：
        // 每停一次服务器就漏一个进程。收树这件事与工具命令、插件钩子共用一份实现
        // （`constrain::reap_tree`），三份各写一遍的代价是哪一份没跟上没人知道
        if let Ok(mut child) = child.lock() {
            crate::tool_runtime::constrain::reap_tree(&mut child);
        }
    }
}

#[derive(Clone, Default)]
pub struct Hub {
    connections: Arc<Mutex<HashMap<String, Arc<Connection>>>>,
}

impl Hub {
    fn with<T>(&self, run: impl FnOnce(&mut HashMap<String, Arc<Connection>>) -> T) -> T {
        let mut map = self
            .connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        run(&mut map)
    }

    fn ensure(&self, server: &McpServer) -> Result<Arc<Connection>, String> {
        if let Some(existing) = self.with(|map| map.get(&server.id).cloned()) {
            return Ok(existing);
        }
        let conn = Connection::start(server)?;
        self.with(|map| map.insert(server.id.clone(), Arc::clone(&conn)));
        Ok(conn)
    }

    fn stop(&self, id: &str) {
        if let Some(conn) = self.with(|map| map.remove(id)) {
            conn.shutdown();
        }
    }

    fn connected(&self) -> Vec<String> {
        self.with(|map| map.keys().cloned().collect())
    }
}

/// 独立配置的，加上启用中插件带来的。插件给的服务器不能在界面里单独删，只能停整个插件。
pub fn all_servers(app: &AppHandle, config: &AppConfig) -> Vec<McpServer> {
    // 自定义 MCP 总开关（design-security-center.md D7）：关掉 = 用户自配的服务器
    // 整批不声明、不连接。出厂扩展与插件自带的不受它管——它们的开关在各自那页
    let mut servers = match config.user_mcp_enabled {
        true => config.mcp_servers.clone(),
        false => Vec::new(),
    };
    servers.extend(
        crate::plugins::mcp_servers(app)
            .into_iter()
            .map(|(_, server)| server),
    );
    // 顺序 = 配置顺序 + 插件顺序，不再排序（§6.2：pi 用首次声明的插入序，只增不减）。
    // 以前这里 sort_by(id) 是为了防目录遍历抖动，但排序只能让"同一批声明"稳定，
    // 表达不了"新增一条该追加在尾部"；现在声明数组在话题首轮就定形并落进日志（
    // `chat.rs::Send::declarations`），抖动与新增都影响不到已发出的字节
    servers
}

/// 声明给模型的 MCP 工具。连不上的服务器只记一条日志，不拖垮整轮对话。
pub fn schemas(servers: &[McpServer], config: &AppConfig, hub: &Hub) -> Vec<Value> {
    let mut declared = Vec::new();
    let mut caps_of: Vec<(String, Capabilities)> = Vec::new();

    for server in servers.iter().filter(|server| server.enabled) {
        let conn = match hub.ensure(server) {
            Ok(conn) => conn,
            Err(error) => {
                eprintln!("MCP 服务器「{}」连接失败：{error}", server.name);
                continue;
            }
        };

        for tool in conn.tools() {
            let exposed = exposed_name(&server.id, &tool.name);
            if config
                .disabled_mcp_tools
                .iter()
                .any(|item| item == &exposed)
            {
                continue;
            }
            // MCP 的 schema 带一个 $schema 方言标记，服务商会当成未知关键字，去掉再发
            let parameters = match tool.input_schema {
                mut schema if schema.is_object() => {
                    if let Some(map) = schema.as_object_mut() {
                        map.remove("$schema");
                    }
                    schema
                }
                _ => json!({ "type": "object", "properties": {} }),
            };
            declared.push(json!({
                "type": "function",
                "function": {
                    "name": exposed,
                    "description": tool.description,
                    "parameters": parameters,
                }
            }));
        }
        caps_of.push((sanitize(&server.id), conn.capabilities()));
    }

    declared.extend(browser_declarations(&caps_of, &config.disabled_mcp_tools));
    declared
}

/// 空清单也要说一句"没有"。交回空字符串时，模型读到的是"这台什么都没给"还是
/// "我调用失败了"就全看运气了
fn render_list<T>(items: &[T], line: impl Fn(&T) -> String) -> String {
    if items.is_empty() {
        return "（这台服务器没有可列出的条目）".into();
    }
    items.iter().map(line).collect::<Vec<_>>().join("
")
}

/// 两台风口上的浏览器。**不是每个资源一个工具**：那样声明数组按条目数长，
/// 而声明每一发都要重付一遍 token（§11）
pub const RESOURCES_TOOL: &str = "mcp_resources";
pub const PROMPTS_TOOL: &str = "mcp_prompts";

fn is_browser(name: &str) -> bool {
    name == RESOURCES_TOOL || name == PROMPTS_TOOL
}

/// 只有至少一台**连上的**服务器声明过那一格，才把对应的浏览器放进声明里；
/// 在 `disabledMcpTools` 里写它的名字就连声明都不出现——留着一个必然报错的工具
/// 比没有更坏（模型第一次调用就撞上一句"这台没说它有资源"）
fn browser_declarations(connected: &[(String, Capabilities)], disabled: &[String]) -> Vec<Value> {
    let mut declared = Vec::new();
    let named = |which: fn(&Capabilities) -> bool| -> Vec<String> {
        connected
            .iter()
            .filter(|(_, caps)| which(caps))
            .map(|(id, _)| id.clone())
            .collect()
    };
    let servers = named(|caps| caps.resources);
    if !servers.is_empty() && !disabled.iter().any(|item| item == RESOURCES_TOOL) {
        declared.push(json!({
            "type": "function",
            "function": {
                "name": RESOURCES_TOOL,
                "description": "列出或读取一个 MCP 服务器暴露的资源。action=list 列清单；action=read 读一个 uri。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "server": { "type": "string", "enum": servers, "description": "要问哪一台服务器" },
                        "action": { "type": "string", "enum": ["list", "read"] },
                        "uri": { "type": "string", "description": "action=read 时要读的 uri" }
                    },
                    "required": ["server", "action"]
                }
            }
        }));
    }
    let servers = named(|caps| caps.prompts);
    if !servers.is_empty() && !disabled.iter().any(|item| item == PROMPTS_TOOL) {
        declared.push(json!({
            "type": "function",
            "function": {
                "name": PROMPTS_TOOL,
                "description": "列出或取出一个 MCP 服务器的提示词（prompt）。action=list 列清单；action=get 按名字取，arguments 填它要的键。取回来的是**建议发的内容**，不是已经发生过的对话。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "server": { "type": "string", "enum": servers, "description": "要问哪一台服务器" },
                        "action": { "type": "string", "enum": ["list", "get"] },
                        "name": { "type": "string", "description": "action=get 时的提示词名字" },
                        "arguments": { "type": "object", "description": "提示词要的键值" }
                    },
                    "required": ["server", "action"]
                }
            }
        }));
    }
    declared
}/// 有几台启用中的服务器把自己的 id 折成这一个前缀。0 / 1 才是"唯一解析"，
/// ≥2 是撞车：暴露名一样，而审批键里那一格写的是 `tool.<暴露名>`，
/// 用户为 A 点的那一次头会原样顶到 B 那台程序身上
fn claim_count(servers: &[McpServer], prefix: &str) -> usize {
    servers.iter().filter(|server| server.enabled && sanitize(&server.id) == prefix).count()
}

/// 这个名字背后有几个可能的执行体。**唯一解析才是解析**：
/// 撞车时不许"择一"（§8 风险 5 写明的关闭方式）。
/// 浏览器那两格自己带 `{server, action}`，是显式指名的，恒为 1
pub fn resolvers(servers: &[McpServer], name: &str) -> usize {
    if is_browser(name) {
        return 1;
    }
    split_exposed(name).map(|(prefix, _)| claim_count(servers, prefix)).unwrap_or(0)
}

/// 这个名字是否归某个已配置的扩展服务器管
pub fn owns(servers: &[McpServer], name: &str) -> bool {
    if is_browser(name) {
        // 浏览器那两格是"有服务器就有的能力"，与哪一台、开没开无关（`browse` 自己按
        // `{server, action}` 逐条拒：没指名、指了台停用的、指了个不存在的名字）
        return !servers.is_empty();
    }
    resolvers(servers, name) > 0
}

/// 命中某个 MCP 工具时执行它。返回 None 表示这根本不是 MCP 工具，交给内置工具处理。
/// "连不上/起不来"那一句的开头。它是**给路由认的记号**，不是给人看的文案的一部分：
/// `tool_runtime::source` 靠它把这次失败分成"没送到执行体"（可以自动重试）与
/// "执行体答了但失败"（绝不自动重放）。改这句话要同时想清楚那一条分类会不会跟着变
pub const TRANSPORT_MARK: &str = "MCP 服务器不可用：";

/// 名字撞车的记号。它和上面那条一样是**给路由认的**，不是文案的一部分：
/// 撞车时没有任何执行体收到这一发，所以它该被分成 `Absent` 而不是 `Content`
/// ——后者说的是"程序答了，但答的是失败"，那是两件事
pub const AMBIGUOUS_MARK: &str = "MCP 工具名撞车：";

pub fn call(
    servers: &[McpServer],
    config: &AppConfig,
    hub: &Hub,
    name: &str,
    args: Value,
) -> Option<Result<String, String>> {
    if is_browser(name) {
        return Some(browse(servers, config, hub, name, &args));
    }
    let (prefix, _) = split_exposed(name)?;
    // 撞车先问一句"有几台认得这个名字"：择一执行等于把用户为 A 点的那次头
    // 用到 B 那台程序上，而界面上看不出区别
    let claimants = claim_count(servers, prefix);
    if claimants > 1 {
        return Some(Err(format!(
            "{AMBIGUOUS_MARK}{claimants} 台扩展服务器的 id 都折成「{prefix}」，\
             无法唯一确定这一发给谁，所以没有执行。请给其中一台换一个 id（id 是名字的一部分，改名不够）。"
        )));
    }
    let server = servers
        .iter()
        .filter(|server| server.enabled)
        .find(|server| sanitize(&server.id) == prefix)?;

    if config.disabled_mcp_tools.iter().any(|item| item == name) {
        return Some(Err("这个扩展工具已被关闭，没有执行。".into()));
    }

    let conn = match hub.ensure(server) {
        Ok(conn) => conn,
        Err(error) => return Some(Err(format!("{TRANSPORT_MARK}{error}"))),
    };

    let original = conn
        .tools()
        .into_iter()
        .find(|tool| exposed_name(&server.id, &tool.name) == name)
        .map(|tool| tool.name);

    match original {
        Some(original) => Some(conn.call(&original, args)),
        None => Some(Err("这个工具不在服务器当前声明的清单里。".into())),
    }
}

/// 一台浏览器工具的那一次调用。它**不新开权限面**：走的还是 MCP 那三条 capability
/// （一次具体调用 + 一个无法约束的执行体 + 把参数交给那个进程），
/// 也就是同一张审批表与同一行 `net.configured`
fn browse(
    servers: &[McpServer],
    config: &AppConfig,
    hub: &Hub,
    name: &str,
    args: &Value,
) -> Result<String, String> {
    if config.disabled_mcp_tools.iter().any(|item| item == name) {
        return Err("这个扩展工具已被关闭，没有执行。".into());
    }
    let wanted = args["server"].as_str().unwrap_or_default();
    let known: Vec<String> = servers.iter().map(|server| sanitize(&server.id)).collect();
    let server = servers
        .iter()
        .find(|server| sanitize(&server.id) == wanted)
        .ok_or_else(|| format!("没有这台服务器「{wanted}」。可选的：{}", known.join("、")))?;
    let resources = name == RESOURCES_TOOL;
    let action = args["action"].as_str().unwrap_or_default();
    let (this, unusable) = if resources {
        ("read", "get")
    } else {
        ("get", "read")
    };
    if action != "list" && action != this {
        return Err(format!(
            "action 只认 list / {this}，收到「{action}」（「{unusable}」是另一台浏览器的动作）。"
        ));
    }
    // 参数问完了才去动那个进程：为一发已知畸形的请求起一个第三方进程，
    // 是拿别人的成本赌我们自己不检查输入
    let conn = hub.ensure(server).map_err(|error| format!("{TRANSPORT_MARK}{error}"))?;

    match (resources, action) {
        (true, "list") => conn.list_resources(),
        (true, _) => {
            let uri = args["uri"].as_str().unwrap_or_default();
            if uri.is_empty() {
                return Err(format!("action={this} 要带 uri（先用 action=list 看清单）。"));
            }
            conn.read_resource(uri)
        }
        (false, "list") => conn.list_prompts(),
        (false, _) => {
            let prompt = args["name"].as_str().unwrap_or_default();
            if prompt.is_empty() {
                return Err(format!("action={this} 要带 name（先用 action=list 看清单）。"));
            }
            conn.get_prompt(prompt, &args["arguments"])
        }
    }
}#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolView {
    pub exposed: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerView {
    pub id: String,
    pub name: String,
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    /// 定义的一部分，不是运行状态：界面上"编辑"那一下是从这一份预填的，
    /// 少带一格就等于把用户那份 env 抹掉（cc-switch 导入的服务器普遍靠它传路径与凭据）
    pub env: std::collections::BTreeMap<String, String>,
    pub url: String,
    pub headers: std::collections::BTreeMap<String, String>,
    pub oauth: bool,
    pub enabled: bool,
    /// "独立配置" 或插件名：插件带来的只能整插件停
    pub source: String,
    pub connected: bool,
    pub tools: Vec<McpToolView>,
    /// 握手声明过的两格能力。没连上或没声明就是 `false`——"没声明"与"声明了但是空的"
    /// 是两件事，所以这里不报条目数：为了显示一个计数而每次开面板都敲一遍别人的进程，
    /// 是拿别人的成本填自己的界面（§11）
    pub can_resources: bool,
    pub can_prompts: bool,
}

#[tauri::command]
pub fn mcp_list(app: AppHandle, hub: tauri::State<'_, Hub>) -> Result<Vec<McpServerView>, String> {
    let config = config::load(&app);
    let connected = hub.connected();
    let servers = all_servers(&app, &config);
    let mut sources: HashMap<String, String> = config
        .mcp_servers
        .iter()
        .map(|server| (server.id.clone(), "独立配置".to_string()))
        .collect();
    for (plugin, server) in crate::plugins::mcp_servers(&app) {
        sources.insert(server.id.clone(), plugin);
    }

    Ok(servers
        .iter()
        .map(|server| {
            let live = connected.iter().any(|id| id == &server.id);
            // 那两格能力读的是握手那一份，不再发一次 list 去数条目
            let caps = if live {
                hub.ensure(server)
                    .map(|conn| conn.capabilities())
                    .unwrap_or_default()
            } else {
                Capabilities::default()
            };
            let tools = if live {
                hub.ensure(server)
                    .map(|conn| {
                        conn.tools()
                            .into_iter()
                            .map(|tool| {
                                let exposed = exposed_name(&server.id, &tool.name);
                                McpToolView {
                                    enabled: !config
                                        .disabled_mcp_tools
                                        .iter()
                                        .any(|item| item == &exposed),
                                    exposed,
                                    name: tool.name,
                                    description: tool.description,
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            view_of(
                server,
                sources
                    .get(&server.id)
                    .cloned()
                    .unwrap_or_else(|| "独立配置".into()),
                live,
                &caps,
                tools,
            )
        })
        .collect())
}

/// 定义 + 那一刻的连线状态 → 界面那一份。单独拆出来是为了让契约测试能拿**真的**构造代码
/// 去比键集合：TS 那句 `McpServerView extends McpServer` 声明的是"投影带着定义"，
/// 而 `env` 长期没带着——界面上"编辑"是从这一份预填的，少一格就是在替用户删那一格
fn view_of(
    server: &McpServer,
    source: String,
    live: bool,
    caps: &Capabilities,
    tools: Vec<McpToolView>,
) -> McpServerView {
    McpServerView {
        id: server.id.clone(),
        name: server.name.clone(),
        transport: server.transport.clone(),
        command: server.command.clone(),
        args: server.args.clone(),
        env: server.env.clone(),
        url: server.url.clone(),
        headers: server.headers.clone(),
        oauth: server.oauth,
        enabled: server.enabled,
        source,
        connected: live,
        tools,
        // 没连上就无从知道它声明过什么：能力只能来自那一握手，所以它与 `connected` 同生死
        can_resources: live && caps.resources,
        can_prompts: live && caps.prompts,
    }
}

/// 按 id 找那台服务器。连接与刷新共用这一处，"找不到"那句话只住在这里
pub(crate) fn find_server(app: &AppHandle, id: &str) -> Result<McpServer, String> {
    let config = config::load(app);
    all_servers(app, &config)
        .into_iter()
        .find(|server| server.id == id)
        .ok_or_else(|| "没有这台 MCP 服务器，或它由插件提供、需要停整个插件。".to_string())
}

// ---- MCP 市场官方注册表（registry.modelcontextprotocol.io）----
//
// 设置页「市场」的数据源：公开目录，无需凭据。这里只做**浏览与预填**——
// 挑中一条就把它带进既有的添加表单，保存仍由用户亲手点。目录里的请求头
// 与环境变量的「值」是 `{placeholder}` 模板不是真凭据，只带名字不带值。

/// registry 条目的 remote 请求头一行。值不进视图：目录里的值是模板
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryHeaderView {
    pub name: String,
    pub description: Option<String>,
    pub is_required: bool,
    pub is_secret: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryRemoteView {
    pub transport_type: String,
    pub url: String,
    pub headers: Vec<RegistryHeaderView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryEnvView {
    pub name: String,
    pub description: Option<String>,
    pub is_required: bool,
    pub is_secret: bool,
}

/// registry 的 package（stdio 包）。`args` 是 runtimeArguments 里的 positional 值；
/// 运行命令由 runtimeHint 给（npx / uvx），前端拿它当启动命令
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryPackageView {
    pub registry_type: String,
    pub identifier: String,
    pub runtime_hint: Option<String>,
    pub args: Vec<String>,
    pub env: Vec<RegistryEnvView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryEntryView {
    pub name: String,
    pub title: Option<String>,
    pub description: String,
    pub version: String,
    pub repository: Option<String>,
    pub remotes: Vec<RegistryRemoteView>,
    pub package: Option<RegistryPackageView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryPage {
    pub entries: Vec<RegistryEntryView>,
    pub next_cursor: Option<String>,
}

const REGISTRY_URL: &str = "https://registry.modelcontextprotocol.io/v0/servers";

fn opt_str(value: &Value, key: &str) -> Option<String> {
    value[key]
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn credential_rows(value: &Value, key: &str) -> Vec<RegistryHeaderView> {
    as_array(value, key)
        .iter()
        .filter_map(|row| {
            let name = str_field(row, "name");
            (!name.is_empty()).then(|| RegistryHeaderView {
                name,
                description: opt_str(row, "description"),
                is_required: row["isRequired"].as_bool().unwrap_or(false),
                is_secret: row["isSecret"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

fn env_rows(value: &Value, key: &str) -> Vec<RegistryEnvView> {
    as_array(value, key)
        .iter()
        .filter_map(|row| {
            let name = str_field(row, "name");
            (!name.is_empty()).then(|| RegistryEnvView {
                name,
                description: opt_str(row, "description"),
                is_required: row["isRequired"].as_bool().unwrap_or(false),
                is_secret: row["isSecret"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

/// registry 的 JSON → 界面视图。解析与网络分离：形状测试拿真样本喂这里。
/// 形状参照 v0 API：`{ servers: [{ server: {...}, _meta }], metadata: { nextCursor } }`
fn parse_registry_page(value: &Value) -> RegistryPage {
    let entries = as_array(value, "servers")
        .iter()
        .filter_map(|item| {
            let server = &item["server"];
            let name = str_field(server, "name");
            if name.is_empty() {
                return None;
            }
            let remotes: Vec<RegistryRemoteView> = as_array(server, "remotes")
                .iter()
                .filter_map(|remote| {
                    let url = str_field(remote, "url");
                    (!url.is_empty()).then(|| RegistryRemoteView {
                        transport_type: str_field(remote, "type"),
                        url,
                        headers: credential_rows(remote, "headers"),
                    })
                })
                .collect();
            let package = as_array(server, "packages").first().map(|package| {
                RegistryPackageView {
                    registry_type: str_field(package, "registryType"),
                    identifier: str_field(package, "identifier"),
                    runtime_hint: opt_str(package, "runtimeHint"),
                    args: as_array(package, "runtimeArguments")
                        .iter()
                        .filter_map(|arg| arg["value"].as_str().map(str::to_string))
                        .collect(),
                    env: env_rows(package, "environmentVariables"),
                }
            });
            Some(RegistryEntryView {
                name,
                title: opt_str(server, "title"),
                description: str_field(server, "description"),
                version: str_field(server, "version"),
                repository: opt_str(&server["repository"], "url"),
                remotes,
                package,
            })
        })
        .collect();
    RegistryPage {
        entries,
        next_cursor: opt_str(&value["metadata"], "nextCursor"),
    }
}

fn fetch_registry(search: Option<&str>, cursor: Option<&str>, limit: usize) -> Result<RegistryPage, String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(CALL_TIMEOUT))
        .build()
        .new_agent();
    let mut request = agent
        .get(REGISTRY_URL)
        .query("version", "latest")
        .query("limit", &limit.to_string());
    // search 词与 cursor 都走 .query() 编码，不手拼 URL——空格与中文是常客
    if let Some(query) = search.map(str::trim).filter(|query| !query.is_empty()) {
        request = request.query("search", query);
    }
    if let Some(page) = cursor.map(str::trim).filter(|page| !page.is_empty()) {
        request = request.query("cursor", page);
    }
    let mut response = request.call().map_err(|e| format!("请求 MCP 市场失败：{e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.body_mut().read_to_string().unwrap_or_default();
        return Err(format!(
            "MCP 市场回了 {status}：{}",
            body.chars().take(200).collect::<String>()
        ));
    }
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("读 MCP 市场响应失败：{e}"))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("MCP 市场响应不是合法 JSON：{e}"))?;
    Ok(parse_registry_page(&value))
}

/// 设置页「市场」的搜索入口。公开目录、无凭据；阻塞的网络调用放阻塞池
/// （与 `pool_catalog` 同一条通道），不占界面
#[tauri::command]
pub async fn registry_search(
    search: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
) -> Result<RegistryPage, String> {
    let limit = limit.unwrap_or(20).clamp(1, 50);
    tauri::async_runtime::spawn_blocking(move || {
        fetch_registry(search.as_deref(), cursor.as_deref(), limit)
    })
    .await
    .map_err(|e| format!("市场查询任务失败：{e}"))?
}

/// 重开这一条连接并回读工具数。**先停再起**是这条式子的全部意义：
/// 少了那一次 `stop`，`ensure` 会原样还你那只旧连接，"刷新"就变成一个不发 `tools/list` 的空操作
fn reconnect(hub: &Hub, server: &McpServer) -> Result<usize, String> {
    hub.stop(&server.id);
    hub.ensure(server).map(|conn| conn.tools().len())
}

/// 后台跑一次重开，结果照旧走 `mcp-status`（面板本来就在听这一条）。
/// 冷启动要几秒，不该卡住界面，所以两条入口都是异步的
fn spawn_reconnect(app: AppHandle, hub: Hub, server: McpServer) {
    thread::spawn(move || {
        let (connected, error, count) = match reconnect(&hub, &server) {
            Ok(count) => (true, String::new(), count),
            Err(error) => (false, error, 0),
        };
        let _ = app.emit(
            "mcp-status",
            json!({ "id": server.id, "connected": connected, "error": error, "toolCount": count }),
        );
    });
}

/// 连接放在后台线程：冷启动可能要几秒，不该卡住界面
#[tauri::command]
pub fn mcp_connect(app: AppHandle, hub: tauri::State<'_, Hub>, id: String) -> Result<(), String> {
    let server = find_server(&app, &id)?;
    spawn_reconnect(app, hub.inner().clone(), server);
    Ok(())
}

/// 刷新工具清单的闸门。**刷新 = 断连**：清单住在握手那一份里（见 `mcp_list` 里那句
/// "不再发一次 list 去数条目"），要拿到新清单只能重开连接，而重开会打断正在跑的调用。
/// 所以这是一次危险动作：要确认，而且不许把停用中的服务器悄悄叫回来
fn refresh_blocker(confirm: bool, server: &McpServer) -> Option<String> {
    if !confirm {
        return Some(
            "刷新工具清单要重开这条连接：它正在跑的调用会被打断。确认要重开再叫这一次。".to_string(),
        );
    }
    if !server.enabled {
        return Some(
            "这台是停用中的服务器：刷新会把它重新叫起来。要用它请先启用，别借刷新绕那一步。"
                .to_string(),
        );
    }
    None
}

/// 显式刷新某台服务器的工具清单 = 授权断连。命令只回"受理了"，
/// 新清单与工具数由 `mcp-status` 那条事件带回来（与连接同一条路，不另起一份真相）
#[tauri::command]
pub fn mcp_refresh(
    app: AppHandle,
    hub: tauri::State<'_, Hub>,
    id: String,
    confirm: bool,
) -> Result<(), String> {
    let server = find_server(&app, &id)?;
    if let Some(reason) = refresh_blocker(confirm, &server) {
        return Err(reason);
    }
    spawn_reconnect(app, hub.inner().clone(), server);
    Ok(())
}

#[tauri::command]
pub fn mcp_stop(hub: tauri::State<'_, Hub>, id: String) -> Result<(), String> {
    hub.stop(&id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 界面上"编辑"是从这一份视图预填的：投影少带一格定义，用户点一次保存就少一格。
    /// `env` 就漏过——cc-switch 导入的服务器普遍靠它传路径与凭据，编辑一次全没了
    #[test]
    fn the_server_view_carries_the_definition_back_to_the_editor() {
        let server = McpServer {
            id: "srv".into(),
            name: "一切".into(),
            transport: "stdio".into(),
            command: "npx".into(),
            args: vec!["-y".into(), "@modelcontextprotocol/server-everything".into()],
            env: std::collections::BTreeMap::from([("MCP_PORT".into(), "3000".into())]),
            url: String::new(),
            headers: BTreeMap::new(),
            oauth: false,
            enabled: true,
        };
        let view = view_of(&server, "独立配置".into(), false, &Capabilities::default(), Vec::new());
        let value = serde_json::to_value(&view).expect("视图该能序列化");
        crate::test_support::assert_matches_ts(&value, "McpServerView");
        assert_eq!(value["env"]["MCP_PORT"].as_str(), Some("3000"), "编辑时该原样带回去");
        // 没连上就是没连上：两格能力不该因为"这台服务器存在"而假装声明过
        assert_eq!(value["connected"].as_bool(), Some(false));
        assert_eq!(value["canResources"].as_bool(), Some(false));
        assert_eq!(value["canPrompts"].as_bool(), Some(false));

        // 同一份"声明过"，断着线时不许亮、连上了必须亮：只有这一对能把 `live &&` 那半证明出来
        // （永远 false 写得出去，前三条照样绿）
        let declared = Capabilities { resources: true, prompts: true };
        let dark =
            serde_json::to_value(view_of(&server, "独立配置".into(), false, &declared, Vec::new()))
                .expect("视图该能序列化");
        assert_eq!(dark["canResources"].as_bool(), Some(false), "没连上就没有\"声明过\"这回事");
        assert_eq!(dark["canPrompts"].as_bool(), Some(false));
        let lit =
            serde_json::to_value(view_of(&server, "独立配置".into(), true, &declared, Vec::new()))
                .expect("视图该能序列化");
        assert_eq!(lit["canResources"].as_bool(), Some(true), "连上且声明过还报暗，就是界面在装看不见");
        assert_eq!(lit["canPrompts"].as_bool(), Some(true));
        // 编辑器要的原样：命令行参数一格都不能少
        assert_eq!(lit["args"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn exposed_names_round_trip_back_to_the_server() {
        let name = exposed_name("mcp-2f", "get.time");
        assert_eq!(name, "mcp__mcp-2f__get_time");
        assert_eq!(split_exposed(&name), Some(("mcp-2f", "get_time")));
        assert_eq!(split_exposed("read_file"), None);
    }

    #[test]
    fn a_command_that_is_not_mcp_falls_through() {
        let config = AppConfig::default();
        let hub = Hub::default();
        assert!(call(&[], &config, &hub, "write_file", json!({})).is_none());
        // 前缀像 MCP 但服务器不存在时也不该假装执行成功
        let servers = config.mcp_servers.clone();
        assert!(!owns(&servers, "mcp__ghost__thing"));
        assert!(call(&servers, &config, &hub, "mcp__ghost__thing", json!({})).is_none());
    }

    fn server(id: &str, enabled: bool) -> McpServer {
        McpServer {
            id: id.into(),
            name: id.into(),
            transport: "stdio".into(),
            command: "noop".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            url: String::new(),
            headers: BTreeMap::new(),
            oauth: false,
            enabled,
        }
    }

    /// 市场解析：v0 的真样本形状。remote 的 headers 只带名字不带值（目录里的值
    /// 是 `{placeholder}` 模板）；package 的 positional 参数抽成 args；nextCursor 透传
    #[test]
    fn registry_page_parsing_keeps_remotes_packages_and_credential_names_only() {
        let page = parse_registry_page(&json!({
            "servers": [
                {
                    "server": {
                        "name": "ai.smithery/obsidian-github-mcp",
                        "title": "Obsidian GitHub",
                        "description": "Connect AI assistants to your GitHub-hosted vault.",
                        "version": "0.4.0",
                        "repository": { "url": "https://github.com/Hint-Services/obsidian-github-mcp" },
                        "remotes": [{
                            "type": "streamable-http",
                            "url": "https://server.smithery.ai/@x/mcp",
                            "headers": [{
                                "description": "Bearer token",
                                "isRequired": true,
                                "value": "Bearer {smithery_api_key}",
                                "isSecret": true,
                                "name": "Authorization"
                            }]
                        }]
                    },
                    "_meta": {}
                },
                {
                    "server": {
                        "name": "com.pulsemcp/remote-filesystem",
                        "description": "Remote filesystem operations.",
                        "version": "0.1.5",
                        "packages": [{
                            "registryType": "npm",
                            "identifier": "remote-filesystem-mcp-server",
                            "runtimeHint": "npx",
                            "runtimeArguments": [{ "value": "-y", "type": "positional" }],
                            "environmentVariables": [
                                { "name": "GCS_BUCKET", "isRequired": true },
                                { "isSecret": true, "name": "GCS_PRIVATE_KEY" }
                            ]
                        }]
                    },
                    "_meta": {}
                },
                { "server": { "description": "没有 name 的条目要被丢掉" }, "_meta": {} }
            ],
            "metadata": { "nextCursor": "com.pulsemcp/remote-filesystem:0.1.5", "count": 3 }
        }));

        assert_eq!(page.entries.len(), 2, "没有 name 的条目不进视图");
        let first = &page.entries[0];
        assert_eq!(first.title.as_deref(), Some("Obsidian GitHub"));
        assert_eq!(first.remotes.len(), 1);
        assert_eq!(first.remotes[0].transport_type, "streamable-http");
        let header = &first.remotes[0].headers[0];
        assert_eq!(header.name, "Authorization");
        assert!(header.is_required && header.is_secret);
        assert!(first.package.is_none(), "纯 remote 条目没有 package");
        assert_eq!(
            page.next_cursor.as_deref(),
            Some("com.pulsemcp/remote-filesystem:0.1.5")
        );

        let pkg = page.entries[1].package.as_ref().expect("package 条目要解析出来");
        assert_eq!(pkg.identifier, "remote-filesystem-mcp-server");
        assert_eq!(pkg.runtime_hint.as_deref(), Some("npx"));
        assert_eq!(pkg.args, vec!["-y".to_string()], "positional 参数原样保留");
        assert_eq!(pkg.env.len(), 2);
        assert!(pkg.env[0].is_required);
        assert!(page.entries[1].remotes.is_empty());

        assert!(parse_registry_page(&json!({"servers": [], "metadata": {}})).next_cursor.is_none());
    }

    /// HTTP 型的 SSE 解析：streamable HTTP 的 POST 响应可能是事件流，回应藏在
    /// data 行里。id 对不上的（服务器主动通知之类）不冒领
    #[test]
    fn sse_reply_parsing_finds_the_matching_id() {
        let text = "event: message\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\
                    \n\
                    event: message\n\
                    data: {\"jsonrpc\":\"2.0\",\"method\":\"update\",\"params\":{}}\n";
        let reply = parse_sse_reply(text, 7).expect("id 匹配的回应该被抽出来");
        assert_eq!(reply["result"]["ok"], json!(true));

        assert!(parse_sse_reply(text, 9).is_none(), "id 对不上就不该领走");
        assert!(parse_sse_reply("event: ping\n\n", 7).is_none(), "非 JSON 的行跳过");
        assert!(
            parse_sse_reply("data: {\"jsonrpc\":\"2.0\",\"id\":3,\"error\":{\"code\":-32601,\"message\":\"no\"}}", 3).is_some(),
            "错误回应也是回应：原样交上去，由统一的错误分支说人话"
        );
    }

    /// 两台服务器折进同一段前缀时，这一发**谁都不许拿到**。"择一"不是无害的降级：
    /// 审批键里那一格写的是 `tool.<暴露名>`，两台算出来是同一个键，
    /// 于是用户为 A 点的那一次点头会原样顶到 B 那台程序上，而界面上看不出区别
    #[test]
    fn two_servers_under_one_prefix_refuse_the_call_instead_of_picking_one() {
        // `sanitize` 是多对一的：`fs:prod` 与 `fs_prod` 是两个 id、一段前缀。
        // 这就是 §8 风险 5 当初写"改名就能撞"时想指的那件事（撞的其实是 id，不是名字）
        let twins = [server("fs:prod", true), server("fs_prod", true)];
        let name = exposed_name("fs_prod", "read");
        assert_eq!(resolvers(&twins, &name), 2, "两台都认得这个名字");
        assert!(owns(&twins, &name), "它确实归扩展管，只是无法唯一定位——这两件事不能混");

        let mut config = AppConfig::default();
        let hub = Hub::default();
        let outcome = call(&twins, &config, &hub, &name, json!({}))
            .expect("撞了也要给一句结果，不许当成\"不是 MCP\"甩回内置");
        let text = outcome.expect_err("撞车不是成功");
        assert!(text.starts_with(AMBIGUOUS_MARK), "要说清是没执行、为什么：{text}");
        assert!(
            text.contains("2 台") && text.contains("换一个 id"),
            "话要照着能修：{text}"
        );

        // 正对照一：一台关掉就只剩一台，那是唯一解析，不该被这条判据误伤
        let one_live = [server("fs:prod", false), server("fs_prod", true)];
        assert_eq!(resolvers(&one_live, &name), 1, "禁用的那台不该算进撞车");
        config.disabled_mcp_tools.push(name.clone());
        let passed = call(&one_live, &config, &hub, &name, json!({}))
            .expect("归扩展管就有结果")
            .expect_err("这条是关掉的工具，走到下一步才停");
        assert!(
            !passed.starts_with(AMBIGUOUS_MARK),
            "唯一解析不该被当成撞车：{passed}"
        );
        // 正对照二：浏览器那两格自己带 `{server, action}`，是显式指名的
        assert_eq!(resolvers(&twins, RESOURCES_TOOL), 1, "显式指名的那格永远不该算撞车");
    }

    /// **一处行为变化（本轮改的，说清楚）**：停用（`enabled: false`）那台以前只影响
    /// **声明**——模型看不见它，可名字被硬调回来时 `call` 照样把它找出来起进程。
    /// 现在执行这一侧认同一条判据：停用了就是不归它管，交回 `None` 让内置去报
    /// "没有名为 X 的工具"。这与 §12 那条"看不见与调得动不许分家"是同一件事的另一头
    #[test]
    fn a_stopped_server_does_not_come_back_through_the_execution_side() {
        let stopped = [server("mcp-9", false)];
        let name = exposed_name("mcp-9", "read");
        assert!(!owns(&stopped, &name), "停用了就不该再抢这个名字");
        let config = AppConfig::default();
        let hub = Hub::default();
        assert!(
            call(&stopped, &config, &hub, &name, json!({})).is_none(),
            "None = 不当成扩展工具，也就不会替一台停用中的服务器起进程"
        );
        // 正对照：启用中同名的那一台照常认领
        let live = [server("mcp-9", true)];
        assert!(owns(&live, &name), "这条判据不许退化成\"扩展工具一概不认\"");
    }

    /// 刷新 = 断连，所以它得先过一道闸：没确认就拒，且要说得出断的是什么；
    /// 停用中的那台不许借刷新被悄悄叫回来。正反两对照都在下面——闸门的坏法常常是"退化成一概拒"
    #[test]
    fn a_refresh_is_refused_until_the_disconnect_is_authorized() {
        let live = server("mcp-1", true);
        let stopped = server("mcp-2", false);

        let reason = refresh_blocker(false, &live).expect("没确认的一律先拒");
        assert!(reason.contains("打断"), "拒要说得出它会断掉什么：{reason}");

        let resurrect = refresh_blocker(true, &stopped).expect("停用中的那台要拒");
        assert!(resurrect.contains("启用"), "要说清该走的是启用那一步：{resurrect}");

        assert!(
            refresh_blocker(true, &live).is_none(),
            "确认过的正常刷新不许被拦——闸门不能退化成『刷新这功能一概不做』"
        );
    }

    /// 闸必须长在动手**之前**，而且重开只能是"先停再起"那一条式子：
    /// `ensure` 单独叫一次会把旧连接原样还回来，那样的"刷新"连 `tools/list` 都不发
    #[test]
    fn the_refresh_gate_sits_before_the_disconnect_and_the_reconnect_stops_first() {
        let source = include_str!("mcp.rs").replace('\r', "");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or_default();
        let command = production
            .split("pub fn mcp_refresh(").nth(1)
            .expect("命令在的");
        let gate = command
            .find("refresh_blocker(")
            .expect("刷新命令要问那道闸");
        let drop_first = command
            .find("spawn_reconnect(")
            .expect("闸门放行之后才重开");
        assert!(gate < drop_first, "闸必须在断连之前：先断了再问，那道闸就只是事后道歉");

        let reconnect = production
            .split("fn reconnect(").nth(1)
            .expect("重开那一条式子在的");
        let stop = reconnect.find("hub.stop(").expect("重开要先停");
        let ensure = reconnect.find("hub.ensure(").expect("停了再起");
        assert!(stop < ensure, "顺序反了就是『刷新』不发 tools/list：ensure 会把旧连接原样还给你");
        assert_eq!(
            production.matches("spawn_reconnect(").count(),
            3,
            "定义 + 连接 + 刷新两处入口共用同一条重开。多一处就是有人另写了一份断连逻辑"
        );
    }

    /// 真打一个 MCP 服务器：initialize → tools/list → tools/call，以及 §11 那四问
    /// （resources/list、resources/read、prompts/list、prompts/get）全链路。
    /// 需要 node 和网络，所以默认不跑：
    /// `cargo test --lib -- --ignored mcp::tests::talks_to_a_real_mcp_server`。
    /// 2026-10-04 本机实跑读数：红在「启动 npx 失败：os error 231」——与 hooks×4
    /// 同源（WorkBuddy 调用链内 spawn 带 stdin 管道被确定性拦截）。与 hooks 不同：
    /// MCP stdio 是**持久双向管道**（宿主要持续写请求），文件-stdin 兜底那套救不了；
    /// 生产 aglab 不在沙箱里跑、不撞 231，所以它只作沙箱外（计划任务）的手动读数，
    /// 不进常规套件。它还要 `npx` 从网上拉第三方包并起子进程——动用户的环境，先点头。
    #[test]
    #[ignore = "要 node 与网络（npx 拉包要先点头）；WorkBuddy 沙箱内跑还撞 231，与 hooks×4 同源——沙箱外/计划任务跑才作数"]
    fn talks_to_a_real_mcp_server() {
        let server = McpServer {
            id: "mcp-itest".into(),
            name: "everything".into(),
            transport: "stdio".into(),
            command: "npx".into(),
            args: vec![
                "-y".into(),
                "@modelcontextprotocol/server-everything".into(),
            ],
            env: BTreeMap::new(),
            url: String::new(),
            headers: BTreeMap::new(),
            oauth: false,
            enabled: true,
        };
        let hub = Hub::default();
        let conn = hub.ensure(&server).expect("连不上 MCP 服务器");

        let tools = conn.tools();
        assert!(
            tools.iter().any(|tool| tool.name == "get-sum"),
            "清单里该有 get-sum，实际 {} 个",
            tools.len()
        );

        let answer = conn
            .call("get-sum", json!({ "a": 21, "b": 22 }))
            .expect("get-sum 该成功");
        assert!(answer.contains("43"), "实际回的是：{answer}");

        let config = AppConfig {
            mcp_servers: vec![server.clone()],
            ..Default::default()
        };
        let declared = schemas(&[server.clone()], &config, &hub);
        assert_eq!(declared.len(), tools.len());
        assert!(declared
            .iter()
            .any(|item| item["function"]["name"].as_str() == Some("mcp__mcp-itest__get-sum")));
        assert!(
            declared
                .iter()
                .all(|item| item["function"]["parameters"].get("$schema").is_none()),
            "MCP 的 $schema 方言不该发给服务商"
        );

        // 模型看到的暴露名要能翻回真实工具
        let routed = call(
            &[server.clone()],
            &config,
            &hub,
            "mcp__mcp-itest__get-sum",
            json!({ "a": 2, "b": 3 }),
        )
        .expect("这个名字应该被路由到扩展")
        .expect("调用该成功");
        assert!(routed.contains('5'), "实际回的是：{routed}");

        // 这一格是 §11 的传输侧：握手声明读得到，四问都能走通。
        // 断言故意写得宽松——具体的 uri 与提示词名是那台服务器自己的事，
        // 我们只保证"声明过就能问、问回来是正文"
        let caps = conn.capabilities();
        assert!(caps.resources && caps.prompts, "everything 服务器两格都有：{caps:?}");
        let listed = conn.list_resources().expect("列资源该成功");
        assert!(!listed.contains("没有可列出的条目"), "清单是空的？{listed}");
        let first = listed.lines().next().expect("至少一行").split("｜").next().expect("uri 那一段").to_string();
        let body = conn.read_resource(&first).expect("读回第一项该成功");
        assert!(!body.contains("iVBOR"), "二进制不该被 base64 灌进上下文：{body}");
        let prompts = conn.list_prompts().expect("列提示词该成功");
        assert!(prompts.contains("｜"), "那一行该是 名字｜描述｜参数：{prompts}");
        let prompt_name = prompts.lines().next().expect("至少一行").split("｜").next().expect("名字那一段").to_string();
        conn.get_prompt(&prompt_name, &json!({})).ok();

        hub.stop("mcp-itest");
        assert!(hub.connected().is_empty(), "stop 之后连接表该清空");
    }

    #[test]
    fn disabled_extension_tools_are_refused_before_dispatch() {
        let mut config = AppConfig::default();
        config.mcp_servers.push(McpServer {
            id: "mcp-1".into(),
            name: "演练".into(),
            transport: "stdio".into(),
            command: " nonexistent-server ".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            url: String::new(),
            headers: BTreeMap::new(),
            oauth: false,
            enabled: true,
        });
        config.disabled_mcp_tools.push("mcp__mcp-1__ping".into());

        let hub = Hub::default();
        let servers = config.mcp_servers.clone();
        assert!(owns(&servers, "mcp__mcp-1__ping"));
        let outcome = call(&servers, &config, &hub, "mcp__mcp-1__ping", json!({})).unwrap();
        assert!(outcome.unwrap_err().contains("已被关闭"));
    }
    /// 能力来自握手，不来自猜：判的是"有没有那个对象"，所以空对象也算声明过
    #[test]
    fn capabilities_come_from_the_handshake_not_from_guessing() {
        let both = capabilities_of(&json!({ "capabilities": { "resources": {}, "prompts": { "list": {} } } }));
        assert!(both.resources && both.prompts, "两格都声明过：{both:?}");

        let none = capabilities_of(&json!({ "capabilities": {} }));
        assert!(!none.resources && !none.prompts, "没声明就是没有，别去问：{none:?}");

        let empty = capabilities_of(&json!({}));
        assert!(!empty.resources, "连 capabilities 这一格都没给");
    }

    /// 清单解析：没有 uri / 没有 name 的那一行不进清单，参数带上"必填"
    #[test]
    fn lists_are_parsed_into_only_what_can_actually_be_used() {
        let resources = parse_resources(&json!({
            "resources": [
                { "uri": "file:///a", "name": "a", "description": "第一个" },
                { "name": "没有 uri，读不了" },
                { "uri": "res://b" }
            ]
        }));
        assert_eq!(resources.len(), 2, "缺 uri 的那条不该进清单：{resources:?}");
        assert_eq!(resources[0].uri, "file:///a");
        assert_eq!(resources[1].name, "");

        let prompts = parse_prompts(&json!({
            "prompts": [
                { "name": "review", "description": "看一段改动",
                  "arguments": [ { "name": "path", "required": true }, { "name": "tone" } ] },
                { "description": "没有名字，取不到" }
            ]
        }));
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert_eq!(prompts[0].arguments, vec!["path（必填）".to_string(), "tone".to_string()]);
    }

    /// 二进制**不进上下文**：base64 塞进历史是每一发都要重付的一坨，而模型读不懂它
    #[test]
    fn a_binary_resource_is_reported_instead_of_being_dumped_into_context() {
        let read = render_resources_read(&json!({
            "contents": [
                { "uri": "file:///a", "mimeType": "text/plain", "text": "第一行正文" },
                { "uri": "img://x", "mimeType": "image/png", "blob": "iVBORw0KGgoAAAANSUhEUg==" }
            ]
        }));
        assert!(read.contains("file:///a"), "{read}");
        assert!(read.contains("第一行正文"), "{read}");
        assert!(read.contains("二进制内容"), "二进制要说\"没读进来\"：{read}");
        assert!(!read.contains("iVBOR"), "base64 不该出现在上下文里");

        assert!(render_resources_read(&json!({ "contents": [] })).contains("没返回"));
    }

    /// 提示词是**建议发的内容**，不是已经发生的对话：渲染成一屏，不塞进历史
    #[test]
    fn a_prompt_comes_back_as_readable_text_not_as_history() {
        let text = render_prompt(&json!({
            "description": "评审",
            "messages": [
                { "role": "user", "content": { "type": "text", "text": "看这段改动" } },
                { "role": "assistant", "content": "字符串形态的正文" }
            ]
        }));
        assert_eq!(text, "user：看这段改动\nassistant：字符串形态的正文", "{text}");
        assert!(render_prompt(&json!({ "messages": [] })).contains("没返回"));
    }

    /// 声明面的三条规矩：没能力就不出现、只列出声明过的那几家、关掉了连声明都不留
    #[test]
    fn the_browsers_are_declared_only_for_the_servers_that_offer_them() {
        let none = browser_declarations(&[], &[]);
        assert!(none.is_empty(), "一家都没有就说不出\"可以列资源\"：{none:?}");

        let connected = vec![
            ("alpha".to_string(), Capabilities { resources: true, prompts: false }),
            ("beta".to_string(), Capabilities { resources: false, prompts: true }),
        ];
        let declared = browser_declarations(&connected, &[]);
        assert_eq!(declared.len(), 2, "{declared:?}");
        let names: Vec<&str> = declared
            .iter()
            .map(|item| item["function"]["name"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(names, vec![RESOURCES_TOOL, PROMPTS_TOOL]);
        assert_eq!(
            serde_json::to_string(&declared[0]["function"]["parameters"]["properties"]["server"]["enum"]).unwrap(),
            "[\"alpha\"]",
            "资源那台的 enum 里不该出现没声明过的 beta"
        );
        assert_eq!(
            serde_json::to_string(&declared[1]["function"]["parameters"]["properties"]["server"]["enum"]).unwrap(),
            "[\"beta\"]"
        );

        let off = browser_declarations(&connected, &[PROMPTS_TOOL.to_string()]);
        assert_eq!(off.len(), 1, "关掉的那台连声明都不该留：{off:?}");
        assert_eq!(off[0]["function"]["name"], RESOURCES_TOOL);
    }

    /// 空清单也要说"没有"，而不是交回一个空字符串让模型自己猜这次算成功还是失败
    #[test]
    fn an_empty_list_says_so_instead_of_returning_nothing() {
        assert_eq!(
            render_list(&Vec::<ResourceInfo>::new(), |item| item.uri.clone()),
            "（这台服务器没有可列出的条目）"
        );
    }

    /// 路由：浏览器工具的名字归 MCP 管，但一台服务器都没配时不归（那时无从路由）
    #[test]
    fn the_browser_names_route_to_mcp_only_when_a_server_exists() {
        let one = vec![McpServer {
            id: "mcp-1".into(),
            name: "演练".into(),
            command: " nonexistent ".into(),
            ..Default::default()
        }];
        assert!(owns(&one, RESOURCES_TOOL));
        assert!(owns(&one, PROMPTS_TOOL));
        assert!(!owns(&[], RESOURCES_TOOL), "没配服务器时它不该抢走这个名字");
    }

    /// 关掉与选错服务器都在真正问进程之前就拦住（`browse` 里那三条拒绝）
    #[test]
    fn a_browser_call_is_refused_before_it_touches_a_process() {
        let server = McpServer {
            id: "mcp-1".into(),
            name: "演练".into(),
            command: " nonexistent ".into(),
            enabled: true,
            ..Default::default()
        };
        let mut config = AppConfig::default();
        config.mcp_servers.push(server.clone());
        let hub = Hub::default();
        let servers = config.mcp_servers.clone();

        // 名字在 disabledMcpTools 里：不执行
        config.disabled_mcp_tools.push(RESOURCES_TOOL.into());
        let outcome = call(&servers, &config, &hub, RESOURCES_TOOL, json!({"server":"mcp-1","action":"list"}));
        assert!(outcome.unwrap().unwrap_err().contains("已被关闭"));
        config.disabled_mcp_tools.clear();

        // 服务器名不对：说得出可选的有谁，且不启动那个进程（命令根本不存在）
        let error = browse(&servers, &config, &hub, RESOURCES_TOOL, &json!({"server":"ghost","action":"list"}))
            .expect_err("没有这台服务器");
        assert!(error.contains("ghost") && error.contains("mcp-1"), "{error}");

        // action 认错了
        let error = browse(&servers, &config, &hub, PROMPTS_TOOL, &json!({"server":"mcp-1","action":"fetch"}))
            .expect_err("这个 action 不存在");
        assert!(error.contains("list / get"), "{error}");
    }

}
