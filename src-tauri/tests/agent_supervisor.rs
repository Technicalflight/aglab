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

#[test]
fn the_worker_reads_the_user_config_through_the_passed_data_dir() {
    // M2 第一切片的端到端验收：Main 传来的数据目录 → worker 的 load_from_dir
    // → config.read 回的就是那份 config.json 里的 model。目录传递链一旦断，
    // 这里第一时间亮红
    if environment_blocks_piped_spawn() {
        eprintln!("宿主拦截带管道的 spawn（os 231 指纹）：进程级验收在真机桌面跑。");
        return;
    }
    let base = std::env::temp_dir().join(format!("aglab-agent-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("建临时数据目录");
    std::fs::write(base.join("config.json"), r#"{"model":"配置甲"}"#).expect("写临时配置");

    let mut child = Command::new(env!("CARGO_BIN_EXE_aglab"))
        .args([
            "--agent-worker",
            "--agent-fence",
            "1",
            "--agent-data-dir",
        ])
        .arg(&base)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("拉起 agent 子进程");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = std::io::BufReader::new(child.stdout.take().expect("stdout"));

    let reply = roundtrip(&mut stdin, &mut stdout, 1, r#"{"v":1,"id":1,"kind":"req","method":"config.read","params":{}}"#);
    assert!(
        reply.contains(r#""model":"配置甲""#),
        "worker 读到的是临时目录里的真实配置：{reply}"
    );
    // 回程只有 model 一个字段：整份配置里有密钥，诊断面不带密钥出门
    let parsed: serde_json::Value = serde_json::from_str(reply.trim()).expect("回程是合法 JSON");
    let result_fields = parsed["result"].as_object().expect("resp.result 是对象");
    assert_eq!(result_fields.len(), 1, "config.read 只回 model 一个字段");

    // sessions 定位链：同一对目录开一条不存在的话题 = 空日志 0 条（不报错、
    // 不挂死）——turn.start 的读写坐在同一个 open_session_in 上
    let reply = roundtrip(
        &mut stdin,
        &mut stdout,
        2,
        r#"{"v":1,"id":2,"kind":"req","method":"session.peek","params":{"conversationId":"m2-slice-2"}}"#,
    );
    assert!(
        reply.contains(r#""entries":0"#),
        "新话题的空日志要读成 0 条：{reply}"
    );

    drop(stdin);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&base);
}


#[test]
fn turn_once_validates_through_the_real_worker_process() {
    if environment_blocks_piped_spawn() {
        eprintln!("宿主拦截带管道的 spawn（os 231 指纹）：进程级验收在真机桌面跑。");
        return;
    }
    // turn.once 的进程级验收：校验闸在真子进程里生效——不碰网络的两闸
    let base = std::env::temp_dir().join(format!("aglab-agent-turn-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("建临时数据目录");
    std::fs::write(base.join("config.json"), r#"{"model":"甲","base_url":""}"#).expect("写临时配置");

    let mut child = Command::new(env!("CARGO_BIN_EXE_aglab"))
        .args(["--agent-worker", "--agent-fence", "1", "--agent-config-dir"])
        .arg(&base)
        .args(["--agent-data-dir"])
        .arg(&base)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("拉起 agent 子进程");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = std::io::BufReader::new(child.stdout.take().expect("stdout"));

    let reply = roundtrip(&mut stdin, &mut stdout, 1, r#"{"v":1,"id":1,"kind":"req","method":"turn.once","params":{"prompt":"你好"}}"#);
    assert!(
        reply.contains(r#""code":"no_provider""#),
        "没配服务商地址要在真子进程里明确报错：{reply}"
    );

    drop(stdin);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&base);
}


#[test]
fn turn_start_answers_started_immediately_and_reports_the_gate_error() {
    if environment_blocks_piped_spawn() {
        eprintln!("宿主拦截带管道的 spawn（os 231 指纹）：进程级验收在真机桌面跑。");
        return;
    }
    // 异步回合的两段形状：started 立即回执，收尾 err（no_provider——
    // 空配置目录 base_url 为空）随后到达。事件/收尾的通道化交给写线程排序
    let base = std::env::temp_dir().join(format!("aglab-agent-start-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("建临时目录");

    let mut child = Command::new(env!("CARGO_BIN_EXE_aglab"))
        .args(["--agent-worker", "--agent-fence", "1", "--agent-config-dir"])
        .arg(&base)
        .args(["--agent-data-dir"])
        .arg(&base)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("拉起 agent 子进程");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = std::io::BufReader::new(child.stdout.take().expect("stdout"));

    let reply = roundtrip(&mut stdin, &mut stdout, 1, r#"{"v":1,"id":1,"kind":"req","method":"turn.start","params":{"prompt":"你好"}}"#);
    assert!(reply.contains(r#""started":true"#), "异步回合立即回执 started：{reply}");

    // 回合线程的 no_provider 错误随后到达（err 信封）
    let mut line = String::new();
    std::io::BufRead::read_line(&mut stdout, &mut line).expect("回合收尾 err");
    assert!(line.contains(r#""no_provider""#), "回合闸错误：{line}");

    // 收尾 ev("turn.done") 最后到达
    let mut line = String::new();
    std::io::BufRead::read_line(&mut stdout, &mut line).expect("turn.done");
    assert!(line.contains(r#""turn.done""#), "收尾事件：{line}");

    drop(stdin);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&base);
}
