//! agent 子进程的集成测试（M1 验收，蓝图 §A4）：用 **真的可执行文件**
//! （CARGO_BIN_EXE_aglab + `--agent-worker`）走完整链路——拉起、信封往返、
//! EOF 干净退场、杀进程后管道立刻断。模块内的纯逻辑（协议形状、fence 闸、
//! 工作循环、监督者状态机）由各自的单元测试钉住，这里钉的是物理事实。

use std::io::Write;
use std::process::{Command, Stdio};

/// 宿主环境指纹闸：带 stdin 管道的 spawn 在个别宿主（WorkBuddy 作业对象的
/// 调用链）会确定性撞 os error 231——真机桌面没有这条路。撞上指纹就跳过
/// 进程级验收（模块内的纯逻辑单测照跑），与 chat.rs 的劫持指纹闸同一策略：
/// **不是无条件跳过，是换环境照跑**。
fn environment_blocks_piped_spawn() -> bool {
    let probe = Command::new("cmd")
        .args(["/c", "echo", "probe"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = probe {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    }
    let error = probe.expect_err("上一行已确认失败");
    let message = error.to_string();
    message.contains("231") || message.contains("管道范例")
}

fn spawn_worker(fence: u64) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_aglab"))
        .args(["--agent-worker", "--agent-fence", &fence.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("拉起 agent 子进程")
}

/// 发一帧请求，收一帧回程（文本行）
fn roundtrip(stdin: &mut dyn Write, stdout: &mut dyn std::io::BufRead, id: u64, frame: &str) -> String {
    writeln!(stdin, "{frame}").expect("写入请求帧");
    stdin.flush().expect("冲刷 stdin");
    let mut line = String::new();
    std::io::BufRead::read_line(stdout, &mut line).expect("等到回程帧");
    line
}

#[test]
fn the_agent_worker_answers_the_protocol_over_real_stdio() {
    if environment_blocks_piped_spawn() {
        eprintln!("宿主拦截带管道的 spawn（os 231 指纹）：进程级验收在真机桌面跑。");
        return;
    }
    let mut child = spawn_worker(3);
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = std::io::BufReader::new(child.stdout.take().expect("stdout"));

    // ping
    let reply = roundtrip(&mut stdin, &mut stdout, 1, r#"{"v":1,"id":1,"kind":"req","method":"ping","params":{}}"#);
    assert!(reply.contains(r#""pong":true"#), "ping 回程：{reply}");

    // echo（中文与空格：JSON 转义不走样）
    let reply = roundtrip(
        &mut stdin,
        &mut stdout,
        2,
        r#"{"v":1,"id":2,"kind":"req","method":"echo","params":{"note":"你 好 aglab"}}"#,
    );
    assert!(reply.contains("你 好 aglab"), "echo 回程：{reply}");

    // status：CLI 授予的 fence=3 要原样报出来
    let reply = roundtrip(&mut stdin, &mut stdout, 3, r#"{"v":1,"id":3,"kind":"req","method":"agent.status","params":{}}"#);
    assert!(reply.contains(r#""fence":3"#), "status 回程带 CLI 授予的 fence：{reply}");

    // 未知方法：明确的 err 信封，不是哑掉
    let reply = roundtrip(&mut stdin, &mut stdout, 4, r#"{"v":1,"id":4,"kind":"req","method":"nope","params":{}}"#);
    assert!(reply.contains(r#""code":"unknown_method""#), "未知方法回程：{reply}");

    // 流式：3 条 ev 保序 + 1 条 resp 终结——ev 通道的物理形状
    writeln!(
        stdin,
        r#"{{"v":1,"id":5,"kind":"req","method":"stream.demo","params":{{"count":3,"prefix":"tick"}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    for index in 0..3 {
        let mut line = String::new();
        std::io::BufRead::read_line(&mut stdout, &mut line).expect("ev 帧");
        assert!(line.contains(r#""kind":"ev""#), "第 {index} 帧该是 ev：{line}");
        assert!(line.contains(&format!(r#""i":{index}"#)), "ev 保序：{line}");
    }
    let mut line = String::new();
    std::io::BufRead::read_line(&mut stdout, &mut line).expect("终答");
    assert!(line.contains(r#""delivered":3"#), "resp 终结帧：{line}");

    drop(stdin); // 关 stdin = EOF = agent 干净退场
    let status = child.wait().expect("等子进程退场");
    assert!(
        status.success() || status.code() == Some(0),
        "EOF 是正常退场：{status:?}"
    );
}

#[test]
fn a_killed_agent_breaks_the_pipe_immediately() {
    if environment_blocks_piped_spawn() {
        eprintln!("宿主拦截带管道的 spawn（os 231 指纹）：进程级验收在真机桌面跑。");
        return;
    }
    // 物理事实钉死：杀掉的子进程让管道对端立刻 EOF——监督者的孤儿判定
    // （EOF → dead → 下次请求重启并 fence+1）吃的正是这个信号
    let mut child = spawn_worker(1);
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = std::io::BufReader::new(child.stdout.take().expect("stdout"));

    let reply = roundtrip(&mut stdin, &mut stdout, 1, r#"{"v":1,"id":1,"kind":"req","method":"ping","params":{}}"#);
    assert!(reply.contains(r#""pong":true"#), "先确认活着");

    child.kill().expect("杀掉 agent");
    let _ = child.wait();

    let mut line = String::new();
    let read = std::io::BufRead::read_line(&mut stdout, &mut line);
    assert!(
        read.is_err() || matches!(read, Ok(0)),
        "进程死后管道必须立刻断，而不是挂着等：{read:?}"
    );
}
