//! LSP 语义导航：常驻语言服务器替模型做四类只读查询——跳转定义、找引用、
//! 悬停文档、列文档符号。文本搜索答不了"这个符号的**定义**在哪"（改名后失效、
//! 分不清同名重载），这类问题语言服务器一眼就有答案。
//!
//! 形态（对齐 deepseek 的 LSP 工具，按 aglab 的工具管线收口）：
//! - 服务器按**扩展名**挑（默认表覆盖 rust/ts/py/go/c 的主流服务器），
//!   `lsp_servers` 配置（`ext=命令` 行）逐扩展覆盖；进程常驻缓存，
//!   键是「语言 + 项目根」——换项目就换服务器；
//! - 协议是最小闭环：initialize → initialized → didOpen → 四类查询之一。
//!   不做的：diagnostics 推送（模型没订阅它的渠道）、workspace 级符号、
//!   改名/格式化（那是写操作，PTC 与文件工具的边界不因 LSP 重画）；
//! - 只读不写，判据与 read_file 同款：项目内 Safe，项目外按 High 过闸。
//!
//! 口径陷阱都在本模块内消化，外层看不到：
//! - LSP 的行列是 **0 起**、列按 **UTF-16** 码元数；工具入参是 1 起、列按字符
//!   （与编辑器显示一致），换算只在这里发生一次；
//! - cron 那次的教训同款：别信"人写的口径 = 库的口径"，换算要钉测试。

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
// 与 mcp.rs 同一条先例：spawn 的是**配置里**用户亲手给的命令（设置页那一格），
// 不是模型传参——别名只让这一事实少被安全钩子误读成"拼接 shell"
use std::process::Command as OsCommand;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// initialize 往返的预算。语言服务器冷启动（rust-analyzer 首次要几秒）都在这之内
const STARTUP_BUDGET: Duration = Duration::from_secs(20);
/// 单次查询的预算
const QUERY_BUDGET: Duration = Duration::from_secs(10);

/// 扩展名 →（语言 id、默认服务器命令）。命令从 PATH 找，找不到就是诚实报错
fn default_server(ext: &str) -> Option<(&'static str, &'static [&'static str])> {
    match ext {
        "rs" => Some(("rust", &["rust-analyzer"])),
        "ts" | "mts" | "cts" => Some(("typescript", &["typescript-language-server", "--stdio"])),
        "tsx" => Some(("typescriptreact", &["typescript-language-server", "--stdio"])),
        "js" | "jsx" | "mjs" | "cjs" => {
            Some(("javascript", &["typescript-language-server", "--stdio"]))
        }
        "py" | "pyi" => Some(("python", &["pyright-langserver", "--stdio"])),
        "go" => Some(("go", &["gopls"])),
        "c" | "h" => Some(("c", &["clangd"])),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => Some(("cpp", &["clangd"])),
        _ => None,
    }
}

/// 服务器命令覆盖表的全局快照（`lsp_servers` 配置，`ext=命令` 行）：
/// 执行体没有 config 通道（command_shell 同款先例），setup 与配置变更钩子各同步一次
static OVERRIDES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

pub fn set_server_overrides(lines: &[String]) {
    let mut table = OVERRIDES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    table.clear();
    for line in lines {
        if let Some((ext, command)) = line.split_once('=') {
            let ext = ext.trim().trim_start_matches('.').to_string();
            let command = command.trim().to_string();
            if !ext.is_empty() && !command.is_empty() {
                table.insert(ext, command);
            }
        }
    }
}

/// 一个查询位置：给了 line+column（1 起）就用它，否则在文件里找标识符首现
pub enum Position {
    At(u64, u64),
    Symbol(String),
}

/// 四类查询
pub enum Query {
    Definition,
    References,
    Hover,
    Symbols,
}

impl Query {
    fn method(&self) -> &'static str {
        match self {
            Query::Definition => "textDocument/definition",
            Query::References => "textDocument/references",
            Query::Hover => "textDocument/hover",
            Query::Symbols => "textDocument/documentSymbol",
        }
    }
}

/// 查询入口。`root` 是项目根：它既做 initialize 的 rootUri，也当缓存键的一半。
/// 服务器**住在表里**，整场查询持表锁走完——不只是图省事：同一只服务器的 stdin
/// 不该有两个线程同时写，表锁就是那道闸（查询有预算，串行不会饿死人）
pub fn query(
    file: &Path,
    root: Option<&Path>,
    query: Query,
    position: Position,
) -> Result<String, String> {
    let root = root.ok_or("LSP 查询需要项目根（语言服务器要 rootUri）：先绑定一个项目。")?;
    let ext = file
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let (language_id, command) = server_command(&ext, file)?;

    let text =
        std::fs::read_to_string(file).map_err(|e| format!("读不了 {}：{e}", file.display()))?;
    let (line, character) = resolve_position(&text, &position, file)?;

    let uri = file_uri(file);
    let params = match query.method() {
        "textDocument/definition" | "textDocument/hover" => json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
        }),
        "textDocument/references" => json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
            "context": { "includeDeclaration": true },
        }),
        _ => json!({ "textDocument": { "uri": uri } }),
    };

    let key = format!("{ext}\u{0}{}", root.display());
    let table = SERVERS.get_or_init(Default::default);
    let mut table = table.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // 死进程的条目不复活：摘掉，下面当新的起（模型重试一次就好）
    if let Some(server) = table.get_mut(&key) {
        if !server.alive() {
            table.remove(&key);
        }
    }
    if !table.contains_key(&key) {
        let server = Server::start(language_id, &command, root)?;
        table.insert(key.clone(), server);
    }
    let server = table.get_mut(&key).expect("上面刚放进去");
    server.open_if_needed(file, language_id, &text)?;
    let result = server.request(query.method(), Some(params), QUERY_BUDGET);
    match result {
        Ok(result) => Ok(match query {
            Query::Definition => format_locations(&result, "定义"),
            Query::References => format_locations(&result, "引用"),
            Query::Hover => format_hover(&result),
            Query::Symbols => format_symbols(&result),
        }),
        // 出错的话题整只扔掉：写坏了一半的 stdin 没有救回来的价值，重试会重启
        Err(error) => {
            table.remove(&key);
            Err(error)
        }
    }
}

/// 扩展名 →（didOpen 的 languageId、启动命令）。配置覆盖优先于默认表
fn server_command(ext: &str, file: &Path) -> Result<(&'static str, String), String> {
    if let Some((id, parts)) = default_server(ext) {
        let overridden = OVERRIDES
            .get()
            .and_then(|lock| lock.lock().ok())
            .and_then(|table| table.get(ext).cloned());
        return Ok((id, overridden.unwrap_or_else(|| parts.join(" "))));
    }
    Err(format!(
        "不认识 .{ext} 这个扩展名（{}），没有默认的语言服务器。\
         认识的：rs、ts/tsx/js/jsx、py、go、c/cpp。别的语言到 设置 → Agent → LSP 服务器 加一行：.{ext}=启动命令",
        file.display()
    ))
}

/// 进程内常驻的服务器缓存，键「扩展名\u{0}项目根」。服务器不出这个表：
/// 查询持锁进来、办完事出去，所有权从不需要离开
static SERVERS: OnceLock<Mutex<HashMap<String, Server>>> = OnceLock::new();

struct Server {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: i64,
    opened: HashSet<String>,
}

impl Server {
    fn start(language_id: &'static str, command: &str, root: &Path) -> Result<Server, String> {
        let mut parts = command.split_whitespace();
        let program = parts.next().ok_or("LSP 服务器命令是空的")?;
        let mut child = OsCommand::new(program)
            .args(parts)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                format!(
                    "启动语言服务器「{command}」失败：{e}。它在 PATH 里吗？\
                     设置 → Agent → LSP 服务器 里可以给 .{language_id} 指定完整路径"
                )
            })?;
        let stdin = child.stdin.take().expect("刚 piped");
        let stdout = child.stdout.take().expect("刚 piped");

        let mut server = Server {
            child,
            stdin,
            reader: BufReader::new(stdout),
            next_id: 0,
            opened: HashSet::new(),
        };
        // initialize 往返：rootUri 指项目根。首查前必须完成，这是协议的门
        let id = server.send_request(
            "initialize",
            Some(json!({
                "processId": std::process::id(),
                "rootUri": file_uri(root),
                "capabilities": {},
            })),
        )?;
        let deadline = Instant::now() + STARTUP_BUDGET;
        let _ = server.wait_response(id, deadline);
        server.notify("initialized", Some(json!({})));
        Ok(server)
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn send_request(&mut self, method: &str, params: Option<Value>) -> Result<i64, String> {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        self.write_frame(&message)?;
        Ok(id)
    }

    fn notify(&mut self, method: &str, params: Option<Value>) {
        let mut message = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        let _ = self.write_frame(&message);
    }

    fn write_frame(&mut self, message: &Value) -> Result<(), String> {
        let body = serde_json::to_vec(message).map_err(|e| format!("LSP 消息序列化失败：{e}"))?;
        self.stdin
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .and_then(|_| self.stdin.write_all(&body))
            .and_then(|_| self.stdin.flush())
            .map_err(|e| {
                let _ = self.child.kill();
                format!("语言服务器进程写不进去了（可能已退出）：{e}。重试一次查询会重启它")
            })
    }

    /// 发请求并等到 id 对上的那条响应；通知与别人的响应跳过。
    /// EOF 与超时都是诚实报错，不假装"没有结果"
    fn request(
        &mut self,
        method: &str,
        params: Option<Value>,
        budget: Duration,
    ) -> Result<Value, String> {
        let id = self.send_request(method, params)?;
        self.wait_response(id, Instant::now() + budget)
    }

    fn wait_response(&mut self, id: i64, deadline: Instant) -> Result<Value, String> {
        loop {
            if Instant::now() > deadline {
                return Err("语言服务器没有在预算内答话。".into());
            }
            let Some(message) = read_frame(&mut self.reader, deadline)? else {
                return Err(
                    "语言服务器进程提前退出了（stderr 里通常有原因）。重试一次查询会重启它。".into(),
                );
            };
            // 带 method 的是请求或通知（服务器也会反问），不是我们要的响应
            if message.get("method").is_some() {
                continue;
            }
            if message.get("id").and_then(Value::as_i64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(format!(
                    "语言服务器答了错误：{}",
                    error.get("message").and_then(Value::as_str).unwrap_or("（无说明）")
                ));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// 文件还没开过就 didOpen。不关：服务器端的诊断与缓存留着更好用
    fn open_if_needed(&mut self, file: &Path, language_id: &str, text: &str) -> Result<(), String> {
        let uri = file_uri(file);
        if self.opened.contains(&uri) {
            return Ok(());
        }
        self.notify(
            "textDocument/didOpen",
            Some(json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text,
                },
            })),
        );
        self.opened.insert(uri);
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // 好聚好散：shutdown 往返不给预算了（等得起的都已等过），exit 通知一发就杀
        if let Ok(id) = self.send_request("shutdown", None) {
            let _ = self.wait_response(id, Instant::now() + Duration::from_millis(500));
        }
        self.notify("exit", None);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 1 起的入参位置 → LSP 的 0 起行列（列按 UTF-16 码元）。symbol 那条路
/// 在文件正文里找标识符首现
fn resolve_position(text: &str, position: &Position, file: &Path) -> Result<(u64, u64), String> {
    match position {
        Position::At(line, column) => {
            if *line == 0 {
                return Err("行号从 1 起（与编辑器一致），0 不是合法行号。".into());
            }
            Ok((line - 1, char_column(text, *line as usize, *column as usize, file)?))
        }
        Position::Symbol(symbol) => {
            let at = text.find(symbol.as_str()).ok_or_else(|| {
                format!("「{symbol}」在 {} 里没找到：给它行号+列号，或换个写法。", file.display())
            })?;
            let before = &text[..at];
            let line = before.matches('\n').count() as u64;
            let line_start = before.rfind('\n').map(|at| at + 1).unwrap_or(0);
            let character = before[line_start..].chars().count() as u64;
            Ok((line, character))
        }
    }
}

/// 1 起的字符列 → 0 起的 UTF-16 列。非 BMP 字符（emoji、生僻字）一个字符占两个码元
fn char_column(text: &str, line: usize, column: usize, file: &Path) -> Result<u64, String> {
    if column == 0 {
        return Err("列号从 1 起（与编辑器一致）。".into());
    }
    let current = text
        .split('\n')
        .nth(line - 1)
        .ok_or_else(|| format!("行号 {line} 超出 {} 的行数。", file.display()))?;
    let head: String = current.chars().take(column - 1).collect();
    Ok(head.chars().map(|c| c.len_utf16() as u64).sum::<u64>())
}

/// 带百分号编码的 file:// URI（LSP 的 uri 字段）。空格与非 ASCII 都要编码
fn file_uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut uri = String::from("file:///");
    for ch in text.trim_start_matches('/').chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '/' | ':' | '.' | '-' | '_' | '~' | '(' | ')' => {
                uri.push(ch)
            }
            _ => {
                let mut buffer = [0u8; 4];
                for byte in ch.encode_utf8(&mut buffer).as_bytes() {
                    uri.push_str(&format!("%{byte:02X}"));
                }
            }
        }
    }
    uri
}

/// 从 stdio 读一条 Content-Length 框架消息。EOF 给 None，超时给 Err
fn read_frame<R: BufRead>(reader: &mut R, deadline: Instant) -> Result<Option<Value>, String> {
    let mut line = String::new();
    // 头部：一行行读到空行为止，Content-Length 在最后那行非空的里
    let mut header = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|e| format!("LSP 流读失败：{e}"))?;
        if read == 0 {
            return Ok(None);
        }
        if line.trim().is_empty() {
            break;
        }
        if Instant::now() > deadline {
            return Err("语言服务器握手没在预算内完成。".into());
        }
        header = line.clone();
    }
    let length: usize = header
        .trim()
        .strip_prefix("Content-Length:")
        .and_then(|value| value.trim().parse().ok())
        .ok_or("LSP 响应缺 Content-Length 头。")?;
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .map_err(|e| format!("LSP 响应体读不满（进程可能退了）：{e}"))?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| format!("LSP 响应不是合法 JSON：{e}"))
}

/// Location / LocationLink / null → 人读的位置清单（行号列号换回 1 起）
fn format_locations(result: &Value, label: &str) -> String {
    let items = match result {
        Value::Null => {
            return format!("{label}：无（可能已经到了定义处本身，或符号没有引用）。")
        }
        Value::Array(items) => items.clone(),
        one @ Value::Object(_) => vec![one.clone()],
        _ => return format!("{label}：服务器回了认不出的形状。"),
    };
    let mut lines = Vec::new();
    for item in items {
        // LocationLink 包一层 target，位置在 targetSelectionRange
        let (uri, range) = match (item.get("uri"), item.get("range")) {
            (Some(uri), Some(range)) => (uri.clone(), range.clone()),
            _ => match item.get("target") {
                Some(target) => (
                    target.get("uri").cloned().unwrap_or(Value::Null),
                    target
                        .get("targetSelectionRange")
                        .or_else(|| target.get("range"))
                        .cloned()
                        .unwrap_or(Value::Null),
                ),
                None => continue,
            },
        };
        let path = uri.as_str().unwrap_or("").trim_start_matches("file://");
        let (line, column) = range_point(&range);
        lines.push(format!("{path}:{line}:{column}"));
    }
    if lines.is_empty() {
        return format!("{label}：无。");
    }
    format!("{label}（{} 处）：\n{}", lines.len(), lines.join("\n"))
}

fn range_point(range: &Value) -> (u64, u64) {
    let start = range.get("start").cloned().unwrap_or(Value::Null);
    (
        start.get("line").and_then(Value::as_u64).unwrap_or(0) + 1,
        start.get("character").and_then(Value::as_u64).unwrap_or(0) + 1,
    )
}

/// hover → 正文文本。contents 有三种合法形状（Markup/字符串/带语言标注/数组）
fn format_hover(result: &Value) -> String {
    if result.is_null() {
        return "悬停：无文档。".into();
    }
    let contents = result.get("contents").cloned().unwrap_or(Value::Null);
    let mut text = String::new();
    match contents {
        Value::String(s) => text.push_str(&s),
        Value::Array(items) => {
            for item in items {
                push_hover_part(&item, &mut text);
            }
        }
        other => push_hover_part(&other, &mut text),
    }
    if text.trim().is_empty() {
        return "悬停：无文档。".into();
    }
    text
}

fn push_hover_part(part: &Value, text: &mut String) {
    match part {
        Value::String(s) => {
            text.push_str(s);
            text.push('\n');
        }
        Value::Object(_) => {
            if let Some(value) = part.get("value").and_then(Value::as_str) {
                if let Some(language) = part.get("language").and_then(Value::as_str) {
                    text.push_str("```");
                    text.push_str(language);
                    text.push('\n');
                    text.push_str(value);
                    text.push_str("\n```\n");
                } else {
                    text.push_str(value);
                    text.push('\n');
                }
            }
        }
        _ => {}
    }
}

/// documentSymbol → 扁平的符号清单。DocumentSymbol（层级）递归压平，SymbolInformation（平表）直读
fn format_symbols(result: &Value) -> String {
    let items = match result {
        Value::Null => return "符号：无。".into(),
        Value::Array(items) => items.clone(),
        _ => return "符号：服务器回了认不出的形状。".into(),
    };
    let mut lines = Vec::new();
    fn walk(item: &Value, depth: usize, out: &mut Vec<String>) {
        let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
        let kind = symbol_kind(item.get("kind").and_then(Value::as_u64).unwrap_or(0));
        let range = item
            .get("selectionRange")
            .or_else(|| item.get("location"))
            .or_else(|| item.get("range"))
            .cloned()
            .unwrap_or(Value::Null);
        let (line, column) = range_point(&range);
        out.push(format!("{}{name}（{kind}）@ {line}:{column}", "  ".repeat(depth)));
        for child in item
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            walk(child, depth + 1, out);
        }
    }
    for item in &items {
        walk(item, 0, &mut lines);
    }
    if lines.is_empty() {
        return "符号：无。".into();
    }
    format!("符号（{} 个）：\n{}", lines.len(), lines.join("\n"))
}

/// LSP SymbolKind 数字 → 人话。1..26 之外的数字原样报"符号"
fn symbol_kind(kind: u64) -> &'static str {
    match kind {
        1 => "文件",
        2 => "模块",
        3 => "命名空间",
        4 => "包",
        5 => "类",
        6 => "方法",
        7 => "属性",
        8 => "字段",
        9 => "构造器",
        10 => "枚举",
        11 => "接口",
        12 => "函数",
        13 => "变量",
        14 => "常量",
        15 => "字符串",
        16 => "数字",
        17 => "布尔",
        18 => "数组",
        19 => "对象",
        20 => "键",
        21 => "null",
        22 => "枚举成员",
        23 => "结构体",
        24 => "事件",
        25 => "运算符",
        26 => "类型参数",
        _ => "符号",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_through_content_length() {
        let message = json!({ "jsonrpc": "2.0", "id": 7, "result": { "hi": ["中文", "emoji 🎈"] } });
        let body = serde_json::to_vec(&message).unwrap();
        let mut wire = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        wire.extend_from_slice(&body);

        // 走内存切片而不是子进程：框架编解码是纯函数的事
        let mut reader = BufReader::new(wire.as_slice());
        let parsed = read_frame(&mut reader, Instant::now() + Duration::from_secs(1))
            .expect("框架要能解回来")
            .expect("不该是 EOF");
        assert_eq!(parsed["id"], 7);
        assert_eq!(parsed["result"]["hi"][0], "中文");
    }

    #[test]
    fn eof_is_reported_as_none_not_an_error() {
        let mut reader = BufReader::new(&b""[..]);
        let parsed =
            read_frame(&mut reader, Instant::now() + Duration::from_secs(1)).expect("EOF 不是错误");
        assert!(parsed.is_none(), "对端关闭 = 没有消息了");
    }

    #[test]
    fn position_conversion_is_one_based_chars_in_zero_based_utf16_out() {
        let text = "fn main() {\n    let 你好🎈 = 1;\n}\n";
        // 第二行第 9 个字符（1 起）之前是 8 个 BMP 字符（4 空格 + "let " + 你 + 好）
        assert_eq!(char_column(text, 2, 9, Path::new("x")).expect("列要换算得出"), 8);
        // 第 10 个字符是 🎈：非 BMP，占 2 个码元，所以第 11 列 = 8 + 2 = 10
        assert_eq!(char_column(text, 2, 11, Path::new("x")).expect("列要换算得出"), 10);
    }

    #[test]
    fn symbol_position_lands_on_its_first_occurrence() {
        let text = "let a = 1;\nlet b = a + 1;\n";
        let (line, column) =
            resolve_position(text, &Position::Symbol("b =".into()), Path::new("x")).expect("要找得到");
        assert_eq!((line, column), (1, 4), "0 起的行列直接给 LSP");
    }

    #[test]
    fn unknown_extensions_are_an_honest_error_and_overrides_win() {
        let error = server_command("zig", Path::new("x.zig")).unwrap_err();
        assert!(error.contains("zig"), "报错要带上扩展名：{error}");
        assert!(error.contains("LSP 服务器"), "报错要指到设置那一格：{error}");

        set_server_overrides(&["rs=C:\\tools\\my-ra.exe".into()]);
        let (_, command) = server_command("rs", Path::new("x.rs")).expect("覆盖后要能取到命令");
        assert_eq!(command, "C:\\tools\\my-ra.exe", "配置覆盖优先于默认表");
        set_server_overrides(&[]);
    }

    #[test]
    fn a_server_that_cannot_start_is_an_honest_error() {
        let error = match Server::start("rust", "aglab-no-such-lsp-binary-4fa9", Path::new(".")) {
            Err(error) => error,
            Ok(_) => panic!("不存在的二进制不该起得来"),
        };
        assert!(error.contains("aglab-no-such-lsp-binary-4fa9"), "报错要带上命令：{error}");
        assert!(error.contains("PATH") || error.contains("LSP 服务器"), "报错要指路：{error}");
    }

    #[test]
    fn locations_hover_and_symbols_render_from_their_wire_shapes() {
        let locations = json!([
            { "uri": "file:///C:/proj/a.rs", "range": { "start": { "line": 9, "character": 7 } } },
            { "target": { "uri": "file:///C:/proj/b.rs", "targetSelectionRange": { "start": { "line": 0, "character": 3 } } } }
        ]);
        let rendered = format_locations(&locations, "定义");
        assert!(rendered.contains("C:/proj/a.rs:10:8"), "行号列号换回 1 起：{rendered}");
        assert!(rendered.contains("C:/proj/b.rs:1:4"), "LocationLink 也要认：{rendered}");
        assert!(format_locations(&Value::Null, "定义").contains("无"));

        let hover = json!({ "contents": { "kind": "markdown", "value": "pub fn **f**()" } });
        assert_eq!(format_hover(&hover), "pub fn **f**()\n");
        assert!(format_hover(&Value::Null).contains("无文档"));

        let symbols = json!([{
            "name": "f", "kind": 12, "selectionRange": { "start": { "line": 4, "character": 3 } },
            "children": [{ "name": "x", "kind": 8, "selectionRange": { "start": { "line": 5, "character": 11 } } }]
        }]);
        let rendered = format_symbols(&symbols);
        assert!(rendered.contains("f（函数）@ 5:4"), "顶层符号：{rendered}");
        assert!(rendered.contains("x（字段）@ 6:12"), "嵌套符号缩进压平：{rendered}");
    }

    #[test]
    fn the_uri_escapes_what_the_wire_requires() {
        assert_eq!(file_uri(Path::new(r"C:\my proj\源.rs")), "file:///C:/my%20proj/%E6%BA%90.rs");
        assert_eq!(file_uri(Path::new("/home/u/a.rs")), "file:///home/u/a.rs");
    }
}
