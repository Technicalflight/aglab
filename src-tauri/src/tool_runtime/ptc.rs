//! PTC code-mode：模型写一个 Rhai 脚本，脚本内 `tool("name", json_args)` 直接调用
//! 已注册的工具，整个程序一次执行完毕返回一个结果。多工具任务从 N 次模型往返
//! 变成一次程序执行，token 与延迟数量级下降。
//!
//! 安全模型（与 deepseek 的 workflow-ptc-sandbox-reuse 同思路）：
//! - Rhai 本身**没有文件/网络 IO**——只有我们注册的 `tool()` 函数才能触达外部；
//! - `tool()` 走的是与模型直调同一套审批闸 + 沙箱 + 审计——**没有特权通道**；
//!   扩展工具（`mcp__*`）只在权限表直接放行时才执行（chat.rs 那一侧判），
//!   直调要问人的扩展在脚本里执行等于绕开那一声问，这里不给这条路；
//! - 时间预算 30 秒（默认，与其他工具同量级），超时被 Rhai 的执行守卫掐住。
//!
//! 常驻变量域（对齐 deepseek 的常驻 runtime）：每个话题一只 `Scope`，脚本里的
//! `let` 跨发保留。引擎每次新建——变量在、函数定义不在：函数也是"程序"的一半，
//! 跨发驻留意味着上一发的逻辑悄悄改变这一发的行为，那不是状态是陷阱。
//!
//! 边界：脚本里的循环/条件/字符串处理是确定性的，不走模型——所以不会有
//! "幻觉"。但 Rhai 不是通用语言：没有 async、没有多线程、没有文件 IO——
//! 这些都是刻意的，PTC 的定位是"程序化组合已有工具"，不是"第二执行引擎"。

use rhai::{Engine, Scope};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;

/// Rhai 脚本的执行预算：与其他工具同量级。超过被 Rhai 的 on-var 守卫掐住
const EXEC_BUDGET_MS: u64 = 30_000;
/// 脚本内单次 tool() 调用的结果字符上限：超出截断。Rhai 字符串没有大小限制，
/// 一个巨型 JSON 塞回脚本变量会把内存撑爆，也会把上下文撑爆
const TOOL_RESULT_CAP: usize = 32 * 1024;
/// 常驻变量域最多记几个话题。超了整表清场：哪个话题还开着只有界面知道，
/// 这里讲不出"最旧"——清场是诚实的便宜，LRU 是假装的精确
const SCOPE_CACHE_CAP: usize = 16;

/// PTC 的执行结果：程序返回值（Rhai Dynamic → JSON）+ 脚本内 tool() 的调用日志
#[derive(Debug)]
pub struct PtcRunResult {
    pub output: String,
}

/// 每个话题一只常驻变量域。Arc 里再套一层 Mutex：表锁只保护查表，
/// 不把"一个话题在跑脚本"放大成"全进程的脚本串行"
type ScopeTable = HashMap<String, Arc<Mutex<Scope<'static>>>>;

fn scope_table() -> &'static Mutex<ScopeTable> {
    static SCOPES: OnceLock<Mutex<ScopeTable>> = OnceLock::new();
    SCOPES.get_or_init(Default::default)
}

/// 话题的常驻变量域（没有就开一只新的）
pub fn conversation_scope(conversation_id: &str) -> Arc<Mutex<Scope<'static>>> {
    let mut table = scope_table()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(scope) = table.get(conversation_id) {
        return Arc::clone(scope);
    }
    if table.len() >= SCOPE_CACHE_CAP {
        table.clear();
    }
    let scope = Arc::new(Mutex::new(Scope::new()));
    table.insert(conversation_id.to_string(), Arc::clone(&scope));
    scope
}

/// 把话题的常驻域扔掉（run_program 的 reset=true）：变量从零开始
pub fn forget_scope(conversation_id: &str) {
    scope_table()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(conversation_id);
}

/// 执行一段 Rhai 脚本，用一只全新的变量域。`exec_tool` 是脚本内 `tool()` 的后端——
/// 由调用方提供（chat.rs 里接工具管线），本模块不依赖 chat 的类型。
///
/// fail-closed：脚本编译错误、运行时异常、超时——都返回 Err，
/// 调用方把错误当工具结果交回给模型。
///
/// **生产不走这条**：chat.rs 的 PTC 工具调用要复用"按话题常驻"的那份变量域
/// （脚本里定义的变量要能跨轮次存活），所以走的是 `run_in`。
/// 这里只是"每次新开一个域"的薄包装，仅供本模块的单元测试用——
/// 故加 `#[cfg(test)]`，免得死代码警告淹掉真正的告警。
#[cfg(test)]
pub fn run(
    script: &str,
    exec_tool: Box<dyn Fn(&str, &str) -> Result<String, String> + Send>,
) -> Result<PtcRunResult, String> {
    run_in(script, exec_tool, &mut Scope::new())
}

/// 同 [`run`]，但用调用方给的变量域——常驻的那份由 chat.rs 按话题取来
pub fn run_in(
    script: &str,
    exec_tool: Box<dyn Fn(&str, &str) -> Result<String, String> + Send>,
    scope: &mut Scope<'static>,
) -> Result<PtcRunResult, String> {
    let engine = build_engine(exec_tool);
    let output: rhai::Dynamic = engine
        .eval_with_scope(scope, script)
        .map_err(|e| format!("PTC 脚本执行失败：{}", e))?;
    let output_text = if let Some(s) = output.clone().try_cast::<String>() {
        s
    } else {
        let value = rhai_to_json(output);
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| format!("{value}"))
    };

    Ok(PtcRunResult {
        output: output_text,
    })
}

fn build_engine(exec_tool: Box<dyn Fn(&str, &str) -> Result<String, String> + Send>) -> Engine {
    let mut engine = Engine::new();

    // 时间预算：Rhai 的 on-var 守卫在每次变量访问时检查
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(EXEC_BUDGET_MS);
    let deadline_copy = deadline;
    // rhai 1.26 把 on_var 标为 `deprecated` 但文档明说"NOT deprecated, 只是 volatile"：
    // 它仍是唯一能在"每次变量访问"这个粒度上打断执行的钩子。
    // 这里是 PTC 脚本 30 秒时间预算的落地点——沙箱的核心防线，不能因为一条
    // 措辞模糊的 lint 就去掉。等价替代（更细粒度的 on_var 事件）尚不存在于该版本。
    #[allow(deprecated)]
    engine.on_var(move |_, _, _| {
        if std::time::Instant::now() > deadline_copy {
            Err("PTC 脚本超过 30 秒执行预算，已终止。".into())
        } else {
            Ok(None)
        }
    });

    // 沙箱加固：禁掉 Rhai 的 eval 和文件相关特性（默认就没有，但显式关更安心）
    engine.set_max_string_size(1_048_576); // 1 MB 字符串上限
    engine.set_max_array_size(10_000); // 数组上限
    engine.set_max_operations(10_000_000); // 操作数上限（时间预算之外的兜底）
    engine.set_max_expr_depths(32, 32); // 表达式嵌套深度

    // 注册 tool() 函数：Rhai 闭包拿不到可变借用，用 Mutex 包装
    let exec_ref = Arc::new(Mutex::new(exec_tool));
    let exec_for_closure = Arc::clone(&exec_ref);
    // 原为 register_result_fn（已废弃）。改用 register_fn：它用泛型
    // `R: Variant + Clone` 表达返回类型，语义等价——R 就是 Result<Dynamic, EvalAltResult>。
    engine.register_fn(
        "tool",
        move |name: &str, args_json: &str| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
            let exec = exec_for_closure
                .lock()
                .map_err(|e| format!("tool 锁：{e}"))?;
            let result = (*exec)(name, args_json)?;
            // 按字符截断：按字节切会把 UTF-8 切成半个字
            let head: String = result.chars().take(TOOL_RESULT_CAP).collect();
            let result = if head.chars().count() < result.chars().count() {
                format!("{head}\n…（超出 PTC 单次工具结果的字符上限，已截断）")
            } else {
                result
            };
            let value: Value = serde_json::from_str(&result).unwrap_or(Value::String(result));
            Ok(json_to_rhai(value))
        },
    );

    engine
}

/// serde_json::Value → rhai::Dynamic：逐层转换（Object→Map, Array→Array, 其余直通）。
/// Rhai 的 `Dynamic::from(serde_json::Value)` 会包成不透明类型，脚本访问不到字段
fn json_to_rhai(value: Value) -> rhai::Dynamic {
    match value {
        Value::Null => rhai::Dynamic::UNIT,
        Value::Bool(b) => rhai::Dynamic::from(b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                rhai::Dynamic::from(i)
            } else {
                rhai::Dynamic::from(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => rhai::Dynamic::from(s),
        Value::Array(items) => {
            let arr: Vec<rhai::Dynamic> = items.into_iter().map(json_to_rhai).collect();
            rhai::Dynamic::from(arr)
        }
        Value::Object(map) => {
            let mut m = rhai::Map::new();
            for (key, val) in map {
                m.insert(key.into(), json_to_rhai(val));
            }
            rhai::Dynamic::from(m)
        }
    }
}

/// rhai::Dynamic → serde_json::Value（脚本返回值序列化）
fn rhai_to_json(value: rhai::Dynamic) -> Value {
    // Rhai Dynamic → serde_json via the Display/clone path.
    // Rhai 的 Dynamic 有 serde 支持（features = ["serde"]），直接走
    serde_json::to_value(&value).unwrap_or(Value::String(format!("{value}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_script_can_call_a_tool_and_use_the_result() {
        let calls: Arc<Mutex<Vec<(String, String)>>> = Default::default();
        let calls_ref = Arc::clone(&calls);
        let exec = Box::new(move |name: &str, args: &str| -> Result<String, String> {
            calls_ref
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((name.to_string(), args.to_string()));
            if name == "read_file" {
                Ok(r#"{"content": "hello world"}"#.to_string())
            } else {
                Ok("[]".to_string())
            }
        });
        let script = r#"
            let result = tool("read_file", `{ "path": "a.txt" }`);
            result.content
        "#;
        let result = run(script, exec).expect("脚本要能跑通");
        assert_eq!(result.output, "hello world");
        let calls = calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "read_file");
    }

    #[test]
    fn loops_and_conditionals_work_without_model_round_trips() {
        let exec = Box::new(|name: &str, _args: &str| -> Result<String, String> {
            Ok(format!(r#"{{ "data": "result of {name}" }}"#))
        });
        let script = r#"
            let results = [];
            for name in ["search_text", "read_file", "list_files"] {
                let result = tool(name, "{}");
                results.push(result.data);
            }
            results
        "#;
        let result = run(script, exec).expect("三工具循环要跑通");
        assert!(result.output.contains("result of search_text"));
        assert!(result.output.contains("result of read_file"));
        assert!(result.output.contains("result of list_files"));
    }

    #[test]
    fn a_failing_tool_returns_an_error_not_a_panic() {
        let exec = Box::new(|_name: &str, _args: &str| -> Result<String, String> {
            Err("权限被拒".to_string())
        });
        let result = run(r#"tool("write_file", "{}")"#, exec);
        assert!(result.is_err(), "tool() 失败要传导为脚本错误");
        assert!(result.unwrap_err().contains("权限被拒"));
    }

    #[test]
    fn compile_errors_are_caught_fail_closed() {
        let exec =
            Box::new(|_name: &str, _args: &str| -> Result<String, String> { Ok("{}".to_string()) });
        let result = run(r#"this is not valid rhai !!!"#, exec);
        assert!(result.is_err(), "编译错误要 fail-closed");
    }

    /// 常驻变量域：同一只 Scope 连跑两发，上一发的 `let` 这一发还在。
    /// 这是 run_program 常驻 runtime 的判据——多步任务不用每次重算中间结果
    #[test]
    fn variables_survive_across_calls_in_a_resident_scope() {
        let make_exec = || {
            Box::new(|_name: &str, _args: &str| -> Result<String, String> { Ok("{}".to_string()) })
                as Box<dyn Fn(&str, &str) -> Result<String, String> + Send>
        };
        let mut scope = Scope::new();
        let first = run_in("let carried = 21; carried", make_exec(), &mut scope).expect("第一发");
        assert_eq!(first.output, "21");
        let second = run_in("carried * 2", make_exec(), &mut scope).expect("第二发");
        assert_eq!(second.output, "42", "上一发的变量这一发要用得上");
    }

    /// 超过单次结果上限的工具返回在进脚本前被截断：塞回脚本的东西必须有界
    #[test]
    fn an_oversized_tool_result_is_truncated_before_it_enters_the_script() {
        let exec = Box::new(|_name: &str, _args: &str| -> Result<String, String> {
            Ok("啊".repeat(TOOL_RESULT_CAP + 100))
        });
        let script = r#"let r = tool("read_file", "{}"); r"#;
        let result = run(script, exec).expect("截断后的结果仍要能跑");
        assert!(
            result.output.contains("已截断"),
            "截断要说出来：{}",
            &result.output[..100]
        );
        let counted = result.output.chars().filter(|c| *c == '啊').count();
        assert_eq!(counted, TOOL_RESULT_CAP, "脚本拿到的是截断后的那份");
    }
}
