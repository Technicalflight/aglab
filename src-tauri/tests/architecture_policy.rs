//! 架构治理的静态闸（architecture-policy）：不启动应用，只读源码。
//!
//! chat.rs 是巨石（1.35 万行），巨石不是一天建成的——是每一条"就放这儿吧"累积的。
//! 治理从两件最小的事开始，都是只读检查：
//! 一、依赖方向：数据与策略层不得反向依赖执行引擎（chat）；
//! 二、体量棘轮：chat.rs 行数封顶——要加能力，先拆模块（先例：net.rs、command_policy.rs）。

use std::fs;
use std::path::PathBuf;

fn src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn read(rel: &str) -> String {
    fs::read_to_string(src_dir().join(rel)).unwrap_or_else(|e| panic!("读不到 {rel}：{e}"))
}

/// 这些模块是数据与策略层。它们要用的执行引擎公共件（超时装配等）住在
/// net.rs——反摸 crate::chat 就是反向依赖，执行引擎一改动它们全体陪葬
const CHAT_FORBIDDEN: [&str; 6] = [
    "knowledge",
    "policy.rs",
    "command_policy.rs",
    "command_rules.rs",
    "egress.rs",
    "session",
];

#[test]
fn data_and_policy_layers_do_not_depend_on_the_engine() {
    for entry in CHAT_FORBIDDEN {
        let path = src_dir().join(entry);
        let meta = fs::metadata(&path).unwrap_or_else(|e| panic!("{entry}：{e}"));
        if meta.is_file() {
            let text = fs::read_to_string(&path).unwrap();
            assert!(
                !text.contains("crate::chat"),
                "{entry} 反向依赖了 crate::chat。要用的公共件（超时装配等）搬进 net.rs，别让策略层摸执行引擎"
            );
        } else {
            for file in fs::read_dir(&path).unwrap().flatten() {
                let file = file.path();
                if file.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let rel = file
                    .strip_prefix(&src_dir())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let text = fs::read_to_string(&file).unwrap();
                assert!(
                    !text.contains("crate::chat"),
                    "{rel} 反向依赖了 crate::chat。要用的公共件搬进 net.rs，别让策略层摸执行引擎"
                );
            }
        }
    }
}

/// 体量棘轮。这个数是快照不是永久真理：真拆出了新模块，就把天花板往下调；
/// 反过来，任何"顺手加进 chat.rs"的新能力都会在这里撞墙
#[test]
fn the_chat_monolith_does_not_grow() {
    let lines = read("chat.rs").lines().count();
    // 14_400 = 解码链（GBK/charset 识别）落进来之后的实测水位；下一次增长仍会撞闸
    let cap = 14_400;
    assert!(
        lines <= cap,
        "chat.rs 已到 {lines} 行，超过治理上限 {cap}。往里加能力之前先拆模块——\
         已经搬出去的先例：net.rs（超时装配）、command_policy.rs（bash 只读判定）"
    );
}

/// 超时装配的唯一出处：net.rs 定义，别处只准调用。两份超时常量迟早漂移成
/// "一边 90 秒一边无限等"——那正是当年调度线程冻死在 8 分钟前那次请求里的病根
#[test]
fn timeouts_have_exactly_one_home() {
    assert!(
        read("net.rs").contains("pub fn with_timeouts"),
        "net.rs 是超时装配的家"
    );
    for rel in [
        "chat.rs",
        "probe.rs",
        "model_directory.rs",
        "knowledge/ocr.rs",
        "knowledge/embed.rs",
        "knowledge/mod.rs",
    ] {
        assert!(
            !read(rel).contains("fn with_timeouts"),
            "{rel} 不该再定义 with_timeouts——它只有一个家（net.rs），调用请走 crate::net"
        );
    }
}
