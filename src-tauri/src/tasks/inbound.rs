//! inbound webhook：本机的另一个进程敲一下，让某条任务跑一次（design-task-engine.md §15）。
//!
//! 两条让它不成其为"网络服务"的硬条件：**只绑 127.0.0.1**（绑定地址不写在配置里——
//! 一个"可以改成对全网开放"的开关不是这一格该提供的东西），以及**默认关**。
//! 触发走的是界面里"立刻运行"那同一个入口（占位 → 跑），所以 §13 那条"同一任务不叠开发"
//! 在这里免费生效：外部敲得再快，同一任务也只有一发在跑。

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use tauri::AppHandle;

use crate::audit::{self, Actor, Outcome};
use crate::config::ScheduledTask;
use crate::tasks::runs::StartedBy;
use crate::tasks::{run_now, ALREADY_RUNNING};

/// 一条请求的结论。`Reject` 覆盖"路径不对""令牌不对""没这条任务"三种——
/// 对陌生人只回一句 404；回 403 等于告诉他"这个令牌后面确实有一条任务"
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Run(String),
    Reject,
    /// 令牌对上了，但那一条正在跑。这不是秘密（去重本来就是用户自己配的），
    /// 说清楚比回 404 有用
    Busy(String),
}

/// 只认这一种形状：`POST /hook/<令牌> HTTP/1.x`。其余一律 `None`
pub fn token_of(request_line: &str) -> Option<&str> {
    let mut words = request_line.split_whitespace();
    if words.next()? != "POST" {
        return None;
    }
    let rest = words.next()?.strip_prefix("/hook/")?;
    let version = words.next()?;
    if !version.starts_with("HTTP/1.") || rest.is_empty() {
        return None;
    }
    Some(rest)
}

/// 定长时间的逐字节比较。这是回环上的一个本地端口，计时攻击在这儿不现实；
/// 长度差本来就看得见，所以只把"内容"那一段拉平
fn same_bytes(candidate: &str, expected: &str) -> bool {
    let (a, b) = (candidate.as_bytes(), expected.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 令牌 → 任务 id。规则只有一条：**没配令牌的那条任务不参与匹配**——
/// 否则 `POST /hook/`（空的那一段）就能点开所有"没配"的。整段字节相等才算，
/// 前缀不算。读的是配置当前这一份，所以改了令牌不必重启
pub fn lookup(tasks: &[ScheduledTask], token: &str) -> Option<String> {
    tasks.iter().find_map(|task| {
        (!task.webhook_token.is_empty() && same_bytes(token, &task.webhook_token))
            .then(|| task.id.clone())
    })
}

/// 结论 → HTTP 响应。**正文里没有令牌、没有任务名**：那些是给自己看的，写在审计里
pub fn response(decision: &Decision) -> String {
    let (status, body) = match decision {
        Decision::Run(_) => ("200 OK", "已排入。\n"),
        Decision::Busy(_) => ("409 Conflict", "这个任务已经在跑，没有第二发。\n"),
        Decision::Reject => ("404 Not Found", "没有这一发。\n"),
    };
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// 令牌对上之后还要问一句"起得起吗"。`start` 就是"立刻运行"那一步（占位 → 起跑），
/// 抽出来当参数是因为它需要一个真的 AppHandle 才能跑，而这里那条判断——
/// "正在跑"回 409、别的错回 404——不该等到集成测试那天才第一次被验证
pub fn decide(
    token: &str,
    tasks: &[ScheduledTask],
    start: impl Fn(&str) -> Result<(), String>,
) -> Decision {
    let Some(id) = lookup(tasks, token) else {
        return Decision::Reject;
    };
    match start(&id) {
        Ok(()) => Decision::Run(id),
        // 拿常量比而不是找子串：`claim` 那句文案改了这里编译不过，
        // 而不是悄悄把"正在跑"全报成 404
        Err(error) => {
            if error == ALREADY_RUNNING {
                Decision::Busy(id)
            } else {
                Decision::Reject
            }
        }
    }
}

/// 起监听。没开就在这第一行返回：不绑端口、不起线程——一份没打算用这格的配置
/// 不该多出一个 socket
pub fn serve(app: &AppHandle) {
    let config = crate::config::load(app);
    if !config.webhook_in_enabled {
        return;
    }
    let port = config.webhook_in_port;
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("inbound webhook 没能监听 127.0.0.1:{port}：{error}。定时任务不受影响。");
            return;
        }
    };
    eprintln!("inbound webhook 在 127.0.0.1:{port} 上听着（只此地址）");
    for incoming in listener.incoming() {
        let Ok(stream) = incoming else { continue };
        let app = app.clone();
        thread::spawn(move || handle(&app, stream));
    }
}

/// 一发请求。头部读完就停：**正文一概不读**——一个能被外部敲的端口上，解析别人的 body
/// 就是把自己的解析器借给别人攻击，而"敲一下，跑一次"这个语义本来不需要 body
fn handle(app: &AppHandle, mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let Ok(peer) = stream.peer_addr() else { return };
    // 绑定已经限定了监听地址；这一条是防"绑对了却被转发过来"那类意外
    if !peer.ip().is_loopback() {
        let _ = stream.write_all(response(&Decision::Reject).as_bytes());
        return;
    }
    let Ok(clone) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(clone);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.len() > 2048 {
        return;
    }
    let mut header = String::new();
    for _ in 0..64 {
        header.clear();
        if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim_end().is_empty() {
            break;
        }
    }

    let tasks = crate::config::load(app).tasks;
    let decision = match token_of(&line) {
        Some(token) => decide(token, &tasks, |id| run_now(app, id, StartedBy::Webhook)),
        None => Decision::Reject,
    };
    record(app, &decision);
    let _ = stream.write_all(response(&decision).as_bytes());
    let _ = stream.flush();
}

/// 审计记的是动作与任务 id，**不记令牌**：令牌是凭据
fn record(app: &AppHandle, decision: &Decision) {
    let Ok(root) = crate::tasks::runs::data_root(app) else {
        return;
    };
    let (id, outcome) = match decision {
        Decision::Run(id) => (id.clone(), Outcome::Ok),
        Decision::Busy(id) => (id.clone(), Outcome::Blocked),
        Decision::Reject => ("(无匹配)".to_string(), Outcome::Denied),
    };
    let _ = audit::record(&root, Actor::Scheduler, "task:webhook-in", &id, outcome);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ScheduledTask;

    fn task(id: &str, token: &str) -> ScheduledTask {
        ScheduledTask {
            id: id.into(),
            name: id.into(),
            webhook_token: token.into(),
            ..Default::default()
        }
    }

    /// 只认那一种形状：GET、别的路径、缺版本、空令牌全部拒
    #[test]
    fn only_one_request_shape_is_understood() {
        assert_eq!(token_of("POST /hook/abc123 HTTP/1.1"), Some("abc123"));
        assert_eq!(token_of("POST /hook/abc123 HTTP/1.0"), Some("abc123"));
        assert_eq!(token_of("get /hook/abc123 HTTP/1.1"), None);
        assert_eq!(token_of("GET /hook/abc123 HTTP/1.1"), None);
        assert_eq!(token_of("POST /hooks/abc123 HTTP/1.1"), None);
        assert_eq!(token_of("POST /hook/ HTTP/1.1"), None, "空令牌不是触发器");
        assert_eq!(
            token_of("POST /hook/abc123"),
            None,
            "没有版本那段就不算一条完整的请求行"
        );
        assert_eq!(token_of("POST /hook/abc123 FTP/1.1"), None);
    }

    /// "没配令牌"那条不参与匹配；配了的必须整段相等
    #[test]
    fn an_unset_token_can_never_be_knocked_open() {
        let tasks = vec![task("没配", ""), task("配了", "s3cr3t-token")];
        assert_eq!(
            lookup(&tasks, ""),
            None,
            "空的那一段会把所有「没配」的任务点开——那正是坏法"
        );
        assert_eq!(lookup(&tasks, "s3cr3t"), None, "前缀不算匹配");
        assert_eq!(
            lookup(&tasks, "S3CR3T-TOKEN"),
            None,
            "令牌是字节串，不是域名"
        );
        assert_eq!(lookup(&tasks, "s3cr3t-token").as_deref(), Some("配了"));
        assert_eq!(lookup(&[], "s3cr3t-token"), None);
    }

    /// 令牌对上之后那一步的三种结论。这里测的是"正在跑"与"别的错"分得开：
    /// 它俩在 HTTP 上一个是 409 一个是 404，搞混了就是对外漏信息或对内漏事故
    #[test]
    fn a_busy_task_says_busy_and_an_unmatched_token_says_nothing() {
        let tasks = vec![task("配了", "tok")];
        assert_eq!(
            decide("tok", &tasks, |_| Ok(())),
            Decision::Run("配了".into()),
            "Run 里带的必须是匹配到的那个 id，不是敲进来的那段字节"
        );
        assert_eq!(
            decide("tok", &tasks, |_| Err(
                crate::tasks::ALREADY_RUNNING.to_string()
            )),
            Decision::Busy("配了".into()),
            "同一任务不叠开发要说成 409，不是 404"
        );
        assert_eq!(
            decide("tok", &tasks, |_| Err("任务不存在。".into())),
            Decision::Reject,
            "别的错一概不透露"
        );

        // 正控制：陌生令牌一次都不该去动任务引擎——不然"猜令牌"就变成"每次都真起跑"
        let asked = std::cell::Cell::new(0usize);
        let decision = decide("蒙一个", &tasks, |_| {
            asked.set(asked.get() + 1);
            Ok(())
        });
        assert_eq!(decision, Decision::Reject);
        assert_eq!(asked.get(), 0, "令牌没对上就不该起一发");
    }

    /// 三种结论的状态码。拒的那一条不透露任何映射
    #[test]
    fn the_three_answers_say_no_more_than_the_owner_already_knows() {
        let run = response(&Decision::Run("t-1".into()));
        assert!(run.starts_with("HTTP/1.1 200 OK"), "{run}");
        assert!(
            run.contains("content-length"),
            "少了长度与 close 语义会挂住客户端"
        );
        let busy = response(&Decision::Busy("t-1".into()));
        assert!(busy.starts_with("HTTP/1.1 409"), "{busy}");
        let reject = response(&Decision::Reject);
        assert!(reject.starts_with("HTTP/1.1 404"), "{reject}");
        assert!(
            !reject.contains("t-1") && !reject.contains("令牌"),
            "拒的那句不该透露任何映射：{reject}"
        );
    }

    #[test]
    fn the_byte_compare_is_length_honest_and_value_exact() {
        assert!(same_bytes("abc", "abc"));
        assert!(!same_bytes("abc", "abd"));
        assert!(!same_bytes("abc", "abcd"), "长度差本来就看得见");
    }
}
