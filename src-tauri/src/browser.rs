//! 内置浏览器控制（design-browser-control.md）：用专用 profile 拉起本机的
//! Chrome/Edge，挂 CDP 调试通道，模型按 browser-use 的「快照 → 按编号操作 →
//! 再快照」循环驱动它。不引 Python/Playwright：Windows 上 Edge 必在、Chrome
//! 常在，页面动作全部经由注入的 JS（Runtime.evaluate）完成。
//!
//! 安全边界在两处钉死：模型可见的每一次导航都过 web_fetch 同样的两道闸
//! （出口域名名单 + 内网地址拒）；CDP 通道连的是本进程自己拉起的子进程
//! （127.0.0.1 随机端口），是控制面不是数据面，与 webhook 入站绑回环同类。
//!
//! 状态：整机能力已实现但主链路尚未接线（优化路线 O2-1）——M3.5 阶段子进程里
//! 诚实拒绝，本模块整体 dead_code 是暂态，接线时移除这条 allow。
#![allow(dead_code)]

use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::WebSocket;

/// 模型可调的七种动作。参数校验在这里做完——报错话术是给模型看的，说清缺什么
#[derive(Debug, PartialEq)]
pub(crate) enum Action {
    Open { url: String },
    Snapshot,
    Click { index: u32 },
    Type { index: u32, text: String, submit: bool },
    Press { key: String },
    Scroll { amount: i32 },
    Back,
}

pub(crate) fn parse_action(args: &Value) -> Result<Action, String> {
    let name = args["action"].as_str().unwrap_or_default().trim();
    let need = |field: &str, what: &str| -> String {
        format!("{name} 需要 {field}：{what}。")
    };
    match name {
        "open" => {
            let url = args["url"].as_str().unwrap_or_default().trim().to_string();
            if url.is_empty() {
                return Err(need("url", "要打开的地址，仅 http/https"));
            }
            Ok(Action::Open { url })
        }
        "snapshot" => Ok(Action::Snapshot),
        "click" => Ok(Action::Click {
            index: index_of(args)?,
        }),
        "type" => Ok(Action::Type {
            index: index_of(args)?,
            text: args["text"].as_str().unwrap_or_default().to_string(),
            submit: args["submit"].as_bool().unwrap_or(false),
        }),
        "press" => {
            let key = args["key"].as_str().unwrap_or_default().trim().to_string();
            if key.is_empty() {
                return Err(need("key", "键名，如 Enter、Escape、Tab、ArrowDown"));
            }
            Ok(Action::Press { key })
        }
        "scroll" => Ok(Action::Scroll {
            amount: args["amount"].as_i64().unwrap_or(600).clamp(-5000, 5000) as i32,
        }),
        "back" => Ok(Action::Back),
        other => Err(format!(
            "不认识的 browser action「{other}」：open / snapshot / click / type / press / scroll / back 选一个。"
        )),
    }
}

fn index_of(args: &Value) -> Result<u32, String> {
    args["index"]
        .as_u64()
        .map(|value| value as u32)
        .ok_or_else(|| "需要 index：最近一次快照里 [数字] 给的那个元素编号。".into())
}

/// 导航闸：http/https 之外一律拒；出口域名名单（空 = 不收紧）与内网地址两道闸
/// 与 web_fetch 同一份。内网管理页不是模型该逛的地方
pub(crate) fn check_navigation(config: &crate::config::AppConfig, url: &str) -> Result<(), String> {
    let trimmed = url.trim();
    let scheme_ok = ["http://", "https://"]
        .iter()
        .any(|scheme| trimmed.to_ascii_lowercase().starts_with(scheme));
    if !scheme_ok {
        return Err(format!("只支持 http/https 的地址，这个不给打开：{trimmed}"));
    }
    crate::egress::guard(&config.net_egress_allow, trimmed)?;
    crate::egress::refuse_private_target(trimmed)
}

/// 一条 CDP 请求的线格式。锁步协议：发一条、等同 id 的回包
pub(crate) fn request_text(id: u64, method: &str, params: &Value) -> String {
    json!({ "id": id, "method": method, "params": params }).to_string()
}

/// 常用键的 keyCode。不少站点还在听 keyCode，派发事件时带上
fn key_code(key: &str) -> i64 {
    match key {
        "Enter" => 13,
        "Escape" => 27,
        "Tab" => 9,
        "Backspace" => 8,
        "Delete" => 46,
        "ArrowUp" => 38,
        "ArrowDown" => 40,
        "ArrowLeft" => 37,
        "ArrowRight" => 39,
        "PageUp" => 33,
        "PageDown" => 34,
        "Home" => 36,
        "End" => 35,
        " " => 32,
        _ => 0,
    }
}

/// 元素索引快照（browser-use 同款思路）：给可见的交互元素编号并写入
/// data-aglab-idx，动作按这个属性回找。编号只在两次快照之间有意义
const SNAPSHOT_JS: &str = r#"(() => {
  const els = [];
  const nodes = document.querySelectorAll('a[href],button,input,select,textarea,[onclick],[role="button"],[role="link"],[role="tab"],[role="checkbox"],[role="menuitem"],summary,[contenteditable="true"]');
  let i = 0;
  for (const el of nodes) {
    if (els.length >= 120) break;
    const r = el.getBoundingClientRect();
    if (r.width < 2 || r.height < 2) continue;
    const s = getComputedStyle(el);
    if (s.visibility === 'hidden' || s.display === 'none') continue;
    const label = (el.getAttribute('aria-label') || el.placeholder || el.value || el.textContent || el.getAttribute('title') || '').trim().replace(/\s+/g, ' ').slice(0, 80);
    el.setAttribute('data-aglab-idx', String(i));
    els.push('[' + i + '] ' + el.tagName.toLowerCase() + (label ? ' ' + label : ''));
    i++;
  }
  const text = (document.body ? document.body.innerText : '').replace(/\n{3,}/g, '\n\n').slice(0, 12000);
  return JSON.stringify({ url: location.href, title: document.title, ready: document.readyState, elements: els, text: text });
})()"#;

/// 动作脚本的公共骨架：按 data-aglab-idx 找元素，找不到给一句模型能自救的话。
/// `__IDX__` 只会被快照产出的整数替换——脚本里没有自由文本可拼接的口子
const ACTION_JS: &str = r#"(() => {
  const el = document.querySelector('[data-aglab-idx="__IDX__"]');
  if (!el) return JSON.stringify({ ok: false, error: '元素不在了，页面可能变过；重新 snapshot 再看一次' });
__BODY__
})()"#;

const CLICK_BODY: &str = r#"  el.scrollIntoView({ block: 'center' });
  el.click();
  return JSON.stringify({ ok: true });"#;

/// 输入：原生 setter + input/change 事件（React 受控组件认这套）；
/// 下拉按文本/值选中；submit = 提交所在表单。`__TEXT__` 是 JSON 转义后的
/// 字符串字面量，`__SUBMIT__` 是 true/false
const TYPE_BODY: &str = r#"  el.focus();
  if (el.tagName === 'SELECT') {
    const want = __TEXT__.trim();
    let matched = false;
    for (const opt of el.options) {
      if (opt.text.trim() === want || opt.value === want) { el.value = opt.value; matched = true; break; }
    }
    if (!matched) return JSON.stringify({ ok: false, error: '下拉里没有「' + want + '」；选项有：' + Array.from(el.options).slice(0, 20).map(o => o.text.trim()).join(' / ') });
    el.dispatchEvent(new Event('change', { bubbles: true }));
  } else {
    const proto = el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : (el.isContentEditable ? null : HTMLInputElement.prototype);
    if (proto) {
      const setter = Object.getOwnPropertyDescriptor(proto, 'value').set;
      if (setter) setter.call(el, __TEXT__); else el.value = __TEXT__;
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
    } else {
      el.textContent = __TEXT__;
      el.dispatchEvent(new Event('input', { bubbles: true }));
    }
  }
  if (__SUBMIT__) {
    const form = el.closest('form');
    if (form) { if (form.requestSubmit) form.requestSubmit(); else form.submit(); }
  }
  return JSON.stringify({ ok: true });"#;

/// `__KEY__` 是 JSON 转义后的键名，`__CODE__` 是 key_code 给的整数
const PRESS_BODY: &str = r#"  const el = document.activeElement || document.body;
  const opts = { key: __KEY__, bubbles: true, cancelable: true, keyCode: __CODE__, which: __CODE__ };
  el.dispatchEvent(new KeyboardEvent('keydown', opts));
  el.dispatchEvent(new KeyboardEvent('keypress', opts));
  el.dispatchEvent(new KeyboardEvent('keyup', opts));
  return JSON.stringify({ ok: true });"#;

const SCROLL_BODY: &str = r#"  const sc = document.scrollingElement || document.documentElement;
  sc.scrollBy(0, __AMOUNT__);
  return JSON.stringify({ ok: true, y: sc.scrollTop });"#;

pub struct Hub {
    inner: Mutex<Option<Session>>,
}

impl Default for Hub {
    fn default() -> Self {
        Self { inner: Mutex::new(None) }
    }
}

/// 一段拉起的浏览器：子进程 + CDP 连接。丢弃 = 杀进程——替换死话题、
/// 应用退出、清除全部，走的是同一条收尾
pub struct Session {
    child: Child,
    ws: Cdp,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Cdp {
    ws: WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    next_id: u64,
}

impl Cdp {
    fn connect(url: &str) -> Result<Self, String> {
        let (ws, _response) =
            tungstenite::connect(url).map_err(|error| format!("连不上浏览器的调试通道：{error}"))?;
        Ok(Self { ws, next_id: 1 })
    }

    /// 发一条、等同 id 的回包；事件与旧回包跳过。锁步够用：一次只有一问
    fn call(&mut self, method: &str, params: &Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        if let MaybeTlsStream::Plain(stream) = self.ws.get_ref() {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
        }
        self.ws
            .send(tungstenite::Message::text(request_text(id, method, params)))
            .map_err(|error| format!("调试通道断了：{error}"))?;
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() >= deadline {
                return Err(format!("浏览器对 {method} 没有应答（超时）。"));
            }
            match self.ws.read() {
                Ok(tungstenite::Message::Text(text)) => {
                    let value: Value = serde_json::from_str(&text)
                        .map_err(|error| format!("调试通道回了不该回的东西：{error}"))?;
                    if value["id"].as_u64() == Some(id) {
                        if let Some(error) = value["error"].as_str() {
                            return Err(format!("{method} 被浏览器拒绝：{error}"));
                        }
                        return Ok(value["result"].clone());
                    }
                    // 事件与陈年回包：不是这一问的答案，看都不看
                }
                Ok(tungstenite::Message::Ping(payload)) => {
                    let _ = self.ws.send(tungstenite::Message::Pong(payload));
                }
                Ok(tungstenite::Message::Close(frame)) => {
                    return Err(format!(
                        "浏览器把调试通道关了（{}）。下次调用会重新拉起。",
                        frame.map(|f| f.reason.to_string()).unwrap_or_default()
                    ));
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    // 读超时只是这 250ms 没等到，离截止还早
                }
                Err(error) => return Err(format!("调试通道出错：{error}")),
            }
        }
    }

    /// 在页面里跑一段 JS。弹窗（alert/confirm）会卡死 evaluate：
    /// 失败先灭掉对话框再重试一次
    fn evaluate(&mut self, expression: &str) -> Result<Value, String> {
        let params = json!({ "expression": expression, "returnByValue": true });
        match self.call("Runtime.evaluate", &params, Duration::from_secs(10)) {
            Ok(result) => Self::evaluate_value(result),
            Err(_) => {
                let _ = self.call(
                    "Page.handleJavaScriptDialog",
                    &json!({ "accept": false }),
                    Duration::from_secs(2),
                );
                let result = self.call("Runtime.evaluate", &params, Duration::from_secs(10))?;
                Self::evaluate_value(result)
            }
        }
    }

    fn evaluate_value(result: Value) -> Result<Value, String> {
        if let Some(details) = result["exceptionDetails"].as_object() {
            let text = details["exception"]["description"]
                .as_str()
                .or(details["text"].as_str())
                .unwrap_or("页面里的脚本出错了");
            return Err(format!("页面脚本执行失败：{text}"));
        }
        Ok(result["result"]["value"].clone())
    }
}

/// 本机浏览器探测的白名单：四个**编译期字面量**安装位置，Chrome 优先、
/// Edge 兜底（Windows 必在）。这里刻意不接任何运行时拼出来的路径——
/// 进程要跑谁，只看这份名单（per-user 安装找不到，Edge 恒兜底）
const BROWSER_CANDIDATES: [&str; 4] = [
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
];

fn find_exe() -> Option<&'static str> {
    BROWSER_CANDIDATES
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file())
}

impl Session {
    fn launch(app: &AppHandle, config: &crate::config::AppConfig) -> Result<Session, String> {
        let exe = find_exe().ok_or(
            "这台机器上没找到 Chrome 或 Edge。内置浏览器用它们拉起；装一个再试。".to_string(),
        )?;
        let port = free_port()?;
        let profile = profile_dir(app)?;
        // 可执行文件只从白名单里来，每个分支都是编译期字面量——进程要跑谁，
        // 不看任何运行时算出来的东西；其余参数一条一个 arg，路径与端口
        // 永远是自己独立的参数段，不拼任何含它们的串
        let mut command = match exe {
            _ if exe == BROWSER_CANDIDATES[0] => Command::new(
                r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            ),
            _ if exe == BROWSER_CANDIDATES[1] => Command::new(
                r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
            ),
            _ if exe == BROWSER_CANDIDATES[2] => Command::new(
                r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            ),
            _ => Command::new(
                r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            ),
        };
        command
            .arg("--remote-debugging-port")
            .arg(port.to_string())
            .arg("--user-data-dir")
            .arg(&profile)
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-session-crashed-bubble")
            .arg("--disable-features=Translate");
        if config.browser_ignore_cert_errors {
            // 证书开关只进启动参数：改配置要重启内置浏览器才生效（设置页写明了）
            command.arg("--ignore-certificate-errors");
        }
        command.arg("about:blank");
        let child = command
            .spawn()
            .map_err(|error| format!("拉起浏览器失败（{exe}）：{error}"))?;

        // 调试端口就绪要等：起得慢是常态，15 秒内轮询
        let version = format!("http://127.0.0.1:{port}/json/version");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if Instant::now() >= deadline {
                let mut child = child;
                let _ = child.kill();
                let _ = child.wait();
                return Err("浏览器起了但调试端口一直没就绪（15 秒）。下次调用再试一次。".into());
            }
            if probe_ready(&version) {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }

        // 现有页面目标里挑一个：新实例至少带着 about:blank 那一页
        let list = http_get_json(&format!("http://127.0.0.1:{port}/json/list"))?;
        let target = list
            .as_array()
            .and_then(|items| {
                items.iter().find(|item| item["type"].as_str() == Some("page"))
            })
            .ok_or("浏览器调试端口通了但没有可用的页面目标。")?;
        let ws_url = target["webSocketDebuggerUrl"]
            .as_str()
            .ok_or("页面目标没给调试通道地址。")?
            .to_string();

        Ok(Session {
            child,
            ws: Cdp::connect(&ws_url)?,
        })
    }

    /// 页面里跑一段 JS。话题层只是转发；对话框自救在 Cdp::evaluate 里
    fn evaluate(&mut self, expression: &str) -> Result<Value, String> {
        self.ws.evaluate(expression)
    }

    fn navigate(&mut self, url: &str) -> Result<(), String> {
        self.ws
            .call("Page.navigate", &json!({ "url": url }), Duration::from_secs(15))?;
        Ok(())
    }

    /// 等页面加载完。SPA 等不满也照常放行——快照里带着 readyState，模型看得见
    fn wait_ready(&mut self, cap_ms: u64) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_millis(cap_ms);
        loop {
            let state = self.evaluate("document.readyState")?
                .as_str()
                .unwrap_or("complete")
                .to_string();
            if state == "complete" {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    fn snapshot(&mut self) -> Result<Value, String> {
        let value = self.evaluate(SNAPSHOT_JS)?;
        serde_json::from_str(value.as_str().unwrap_or("{}"))
            .map_err(|error| format!("快照不是预期的形状：{error}"))
    }

    /// 快照渲染成人话。元素清单是模型操作页面的唯一抓手
    fn render_snapshot(snapshot: &Value) -> String {
        let elements = snapshot["elements"].as_array().cloned().unwrap_or_default();
        let mut out = format!(
            "地址：{}\n标题：{}\n\n可交互元素（动作后编号会变，以最新快照为准）：",
            snapshot["url"].as_str().unwrap_or("（未知）"),
            snapshot["title"].as_str().unwrap_or("（无标题）"),
        );
        if elements.is_empty() {
            out.push_str("\n（这页没有可交互元素）");
        } else {
            for element in elements {
                out.push_str(&format!("\n{}", element.as_str().unwrap_or_default()));
            }
        }
        let text = snapshot["text"].as_str().unwrap_or_default();
        if !text.trim().is_empty() {
            out.push_str("\n\n正文：\n");
            out.push_str(text);
        }
        if snapshot["ready"].as_str() != Some("complete") {
            out.push_str("\n\n（页面还没加载完，编号可能不全；等一下再 snapshot 一次）");
        }
        out
    }

    fn act_js(&mut self, expression: &str) -> Result<(), String> {
        let value = self.evaluate(expression)?;
        let parsed: Value = serde_json::from_str(value.as_str().unwrap_or("{}"))
            .map_err(|error| format!("动作结果不是预期的形状：{error}"))?;
        if parsed["ok"].as_bool() == Some(true) {
            Ok(())
        } else {
            Err(parsed["error"].as_str().unwrap_or("动作没有生效").to_string())
        }
    }

    fn click(&mut self, index: u32) -> Result<(), String> {
        self.act_js(&replace_action(index, CLICK_BODY))
    }

    fn type_text(&mut self, index: u32, text: &str, submit: bool) -> Result<(), String> {
        let literal = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
        let submit_literal = if submit { "true" } else { "false" };
        let body = TYPE_BODY
            .replace("__TEXT__", &literal)
            .replace("__SUBMIT__", submit_literal);
        self.act_js(&replace_action(index, &body))
    }

    fn press(&mut self, key: &str) -> Result<(), String> {
        let literal = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into());
        let body = PRESS_BODY
            .replace("__KEY__", &literal)
            .replace("__CODE__", &key_code(key).to_string());
        self.act_js(&replace_action(0, &body))
    }

    fn scroll(&mut self, amount: i32) -> Result<(), String> {
        let body = SCROLL_BODY.replace("__AMOUNT__", &amount.to_string());
        self.act_js(&replace_action(0, &body))
    }
}

/// 骨架 + 身体 = 一段动作脚本。占位符替换而不是拼接：进脚本里去的
/// 只有整数与 JSON 字面量
fn replace_action(index: u32, body: &str) -> String {
    ACTION_JS
        .replace("__IDX__", &index.to_string())
        .replace("__BODY__", body)
}

/// 拉起或复用当前话题，把话题交给调用方的闭包用完即还。
/// 话题死了（用户关了窗口）就重拉：浏览器不在是常态，不是错误
fn with_session<R>(
    app: &AppHandle,
    config: &crate::config::AppConfig,
    run: impl FnOnce(&mut Session) -> Result<R, String>,
) -> Result<R, String> {
    let hub = app.state::<Hub>();
    let mut guard = hub.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let alive = match guard.as_mut() {
        Some(session) => matches!(session.child.try_wait(), Ok(None)),
        None => false,
    };
    if !alive {
        *guard = Some(Session::launch(app, config)?);
    }
    run(guard.as_mut().expect("刚补上的话题"))
}

/// 工具执行体：chat 循环路由外接走（要 BrowserHub 状态）。
/// 每个动作之后都回一份新快照——模型不需要第二次调用就知道页面变成了什么
pub fn handle_tool(
    app: &AppHandle,
    config: &crate::config::AppConfig,
    args: &Value,
) -> Result<String, String> {
    if !config.browser_control_enabled {
        return Err("内置浏览器控制没开：去「设置 → Agent → 浏览器控制」打开总开关。".into());
    }
    match parse_action(args)? {
        Action::Open { url } => {
            check_navigation(config, &url)?;
            with_session(app, config, |session| {
                session.navigate(&url)?;
                session.wait_ready(8_000)?;
                let snapshot = session.snapshot()?;
                Ok(Session::render_snapshot(&snapshot))
            })
        }
        Action::Snapshot => with_session(app, config, |session| {
            let snapshot = session.snapshot()?;
            Ok(Session::render_snapshot(&snapshot))
        }),
        Action::Click { index } => with_session(app, config, |session| {
            session.click(index)?;
            session.wait_ready(5_000)?;
            let snapshot = session.snapshot()?;
            Ok(Session::render_snapshot(&snapshot))
        }),
        Action::Type { index, text, submit } => with_session(app, config, |session| {
            session.type_text(index, &text, submit)?;
            session.wait_ready(5_000)?;
            let snapshot = session.snapshot()?;
            Ok(Session::render_snapshot(&snapshot))
        }),
        Action::Press { key } => with_session(app, config, |session| {
            session.press(&key)?;
            session.wait_ready(5_000)?;
            let snapshot = session.snapshot()?;
            Ok(Session::render_snapshot(&snapshot))
        }),
        Action::Scroll { amount } => with_session(app, config, |session| {
            session.scroll(amount)?;
            let snapshot = session.snapshot()?;
            Ok(Session::render_snapshot(&snapshot))
        }),
        Action::Back => with_session(app, config, |session| {
            session.evaluate("history.back()")?;
            session.wait_ready(8_000)?;
            let snapshot = session.snapshot()?;
            Ok(Session::render_snapshot(&snapshot))
        }),
    }
}

/// 清缓存：浏览器在跑就走 CDP（只清 HTTP 缓存）；没跑就删磁盘上的缓存目录。
/// Cookie 与站点数据两档都保留
#[tauri::command]
pub fn browser_clear_cache(app: AppHandle) -> Result<String, String> {
    let hub = app.state::<Hub>();
    let mut guard = hub.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(session) = guard.as_mut() {
        session
            .ws
            .call("Network.clearBrowserCache", &json!({}), Duration::from_secs(10))?;
        return Ok("已清除内置浏览器的 HTTP 缓存（Cookie 与站点数据保留）。".into());
    }
    drop(guard);
    let profile = profile_dir(&app)?;
    let mut removed = 0usize;
    for dir in ["Cache", "Code Cache", "Service Worker", "GPUCache"] {
        if std::fs::remove_dir_all(profile.join(dir)).is_ok() {
            removed += 1;
        }
    }
    Ok(format!(
        "内置浏览器没在运行，已直接清掉磁盘上的 {removed} 个缓存目录（Cookie 与站点数据保留）。"
    ))
}

/// 清除全部：先杀浏览器进程，再整删 profile 目录。Cookie、登录态、缓存全没，
/// 不可撤销——界面那颗红按钮上写明了
#[tauri::command]
pub fn browser_clear_all(app: AppHandle) -> Result<String, String> {
    let hub = app.state::<Hub>();
    let mut guard = hub.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = None; // Session::drop 杀进程
    drop(guard);
    let profile = profile_dir(&app)?;
    match std::fs::remove_dir_all(&profile) {
        Ok(_) => Ok("内置浏览器的全部数据已删除（Cookie、站点数据、缓存）。".into()),
        Err(error) => Err(format!(
            "浏览器进程已关闭，但数据目录没能删净（{error}）。再点一次「清除全部」通常就好。"
        )),
    }
}

/// 应用退出时的收尾：内置浏览器是 aglab 拉起的，aglab 走它也该走
pub fn shutdown(app: &AppHandle) {
    let hub = app.state::<Hub>();
    let mut guard = hub.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = None;
}

fn profile_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let dir = base.join("browser-profile");
    std::fs::create_dir_all(&dir).map_err(|error| format!("建浏览器数据目录失败：{error}"))?;
    Ok(dir)
}

fn free_port() -> Result<u16, String> {
    TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("挑不到本地端口：{error}"))?
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|error| format!("挑不到本地端口：{error}"))
}

/// 探测调试端口就绪。失败只意味着"还没好"，调用方继续轮询
fn probe_ready(version_url: &str) -> bool {
    http_get_json(version_url).is_ok()
}

fn http_get_json(url: &str) -> Result<Value, String> {
    let body = ureq::get(url)
        .call()
        .map_err(|error| format!("调试端口没应答：{error}"))?
        .into_body()
        .read_to_string()
        .map_err(|error| format!("调试端口应答读不出来：{error}"))?;
    serde_json::from_str(&body).map_err(|error| format!("调试端口应答不是 JSON：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(allow: &[&str]) -> crate::config::AppConfig {
        let mut config = crate::config::AppConfig::default();
        config.net_egress_allow = allow.iter().map(|entry| entry.to_string()).collect();
        config
    }

    #[test]
    fn action_arguments_are_validated_with_honest_messages() {
        assert!(parse_action(&json!({"action": "browse"})).is_err());
        assert!(parse_action(&json!({})).is_err(), "没有 action 就是坏调用");
        assert_eq!(
            parse_action(&json!({"action": "open"})).unwrap_err(),
            "open 需要 url：要打开的地址，仅 http/https。"
        );
        assert_eq!(
            parse_action(&json!({"action": "click"})).unwrap_err(),
            "需要 index：最近一次快照里 [数字] 给的那个元素编号。"
        );
        assert_eq!(
            parse_action(&json!({"action": "press"})).unwrap_err(),
            "press 需要 key：键名，如 Enter、Escape、Tab、ArrowDown。"
        );

        let action = parse_action(&json!({"action": "type", "index": 3, "text": "你好", "submit": true})).unwrap();
        assert_eq!(
            action,
            Action::Type { index: 3, text: "你好".into(), submit: true }
        );
        // scroll 的量是夹过的：模型手滑写个天文数字也不至于把页面滚穿
        assert_eq!(
            parse_action(&json!({"action": "scroll", "amount": 999_999})).unwrap(),
            Action::Scroll { amount: 5000 }
        );
    }

    /// 导航闸与 web_fetch 同一份：出口名单收紧时名单外的域发不出去，
    /// 内网地址一律拒——浏览器不是绕过内网闸的侧门。
    /// 测试只用 IP 字面量与真实存在的域名：闸会做 DNS 解析，解析不了本身就拒
    #[test]
    fn navigation_gates_match_web_fetch() {
        let config = config_with(&["example.com"]);
        assert!(check_navigation(&config, "https://example.com/").is_ok());
        assert_eq!(
            check_navigation(&config, "https://notexample.com/").unwrap_err(),
            "出口被拦下：notexample.com 不在网络出口的域名名单里。要放行它，去设置 → 权限 → 出口域名名单；名单清空 = 不收紧。"
        );
        assert!(check_navigation(&config, "ftp://example.com/file").is_err(), "协议只认 http/https");

        // 名单不收紧（空）时公网随便去，内网照旧没门。IP 字面量不过 DNS，
        // 判据是纯本地的，测试不赌网络
        let config = config_with(&[]);
        assert!(check_navigation(&config, "http://93.184.216.34/").is_ok());
        assert!(check_navigation(&config, "http://localhost:8080/admin").is_err());
        assert!(check_navigation(&config, "http://192.168.1.1/").is_err());
        assert!(check_navigation(&config, "http://127.0.0.1:9222/json").is_err());
        assert!(check_navigation(&config, "http://10.0.0.5/x").is_err());
    }

    /// CDP 线格式：id、method、params 三件套——锁步读回包靠它对得上号
    #[test]
    fn cdp_requests_carry_id_method_and_params() {
        let text = request_text(7, "Page.navigate", &json!({ "url": "https://example.com/" }));
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["id"].as_u64(), Some(7));
        assert_eq!(value["method"].as_str(), Some("Page.navigate"));
        assert_eq!(value["params"]["url"].as_str(), Some("https://example.com/"));
    }

    /// 快照渲染：元素清单与正文都在，没元素时明说
    #[test]
    fn snapshots_render_elements_and_text() {
        let snapshot = json!({
            "url": "https://example.com/", "title": "示例页", "ready": "complete",
            "elements": ["[0] a 示例域名", "[1] button 更多信息"],
            "text": "这是正文。"
        });
        let text = Session::render_snapshot(&snapshot);
        assert!(text.contains("地址：https://example.com/"));
        assert!(text.contains("[0] a 示例域名"));
        assert!(text.contains("这是正文。"));

        let empty = Session::render_snapshot(&json!({"url": "u", "title": "t", "ready": "complete", "elements": [], "text": ""}));
        assert!(empty.contains("没有可交互元素"), "空页要说明，不是静默：{empty}");
    }

    /// 动作脚本以 data-aglab-idx 找元素；进脚本里去的只有整数与 JSON 字面量。
    /// 编号来自快照的整数，不来自自由文本——脚本里没有可注入的口子
    #[test]
    fn action_scripts_target_the_indexed_element() {
        let js = replace_action(12, CLICK_BODY);
        assert!(js.contains(r#"[data-aglab-idx="12"]"#));
        assert!(js.contains("重新 snapshot"), "元素没了要教模型怎么自救");
        assert!(!js.contains("__IDX__") && !js.contains("__BODY__"), "占位符要被替换干净");

        let typing = replace_action(3, &TYPE_BODY.replace("__TEXT__", "\"你好\"").replace("__SUBMIT__", "true"));
        assert!(typing.contains("\"你好\"") && typing.contains("requestSubmit"));

        assert!(parse_action(&json!({"action": "click", "index": -1})).is_err());
    }
}
