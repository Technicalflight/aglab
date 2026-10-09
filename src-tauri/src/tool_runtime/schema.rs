//! **最小** JSON Schema 子集校验器。
//!
//! 只覆盖本项目自己声明里用到的关键字：`type` / `required` / `properties` / `enum` /
//! `minimum` / `maximum` / `maxLength` / `minLength` / `items`。**不是** JSON Schema 全量实现：
//! 不做 `$ref`，不做 `oneOf`/`anyOf`，不做 `pattern`，不认 draft 版本。
//! 这条边界是有意写在纸面上的——一个"看起来什么都能校验"的校验器，一旦有人往声明里塞
//! `$ref`，它会安静地忽略掉，那就又回到了"schema 是装饰"。所以 [`unsupported`] 存在，
//! 并且有一条测试拿它自检所有内置工具。谁要用新的关键字，就先在这里加那一条，并带上测试。

#[cfg(test)]
use std::collections::BTreeSet;

use serde_json::Value;

/// 本校验器实现了的关键字。列表外、且不属于注释类的，都算"没实现"，由 [`unsupported`] 报出来
#[cfg(test)]
const IMPLEMENTED: [&str; 9] = [
    "type",
    "properties",
    "required",
    "enum",
    "minimum",
    "maximum",
    "maxLength",
    "minLength",
    "items",
];

/// 纯注释：它们不约束取值，忽略掉不会造成"以为校验过了"的错觉。
/// 注意 `additionalProperties` 不在这里——那条真的改语义，我们没实现就得报出来
#[cfg(test)]
const ANNOTATIONS: [&str; 3] = ["description", "title", "default"];

/// 校验入参。返回的错误文案是**给模型看的**：说清哪个字段、要什么、实际给到了什么。
/// 否则模型只能靠猜再试一次，而每一次猜都可能是一次真的副作用
pub fn validate(schema: &Value, args: &Value) -> Result<(), String> {
    let mut problems = Vec::new();
    check("$", schema, args, &mut problems);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("；"))
    }
}

/// 列出声明里用到的、本校验器没实现的关键字。
/// 新增工具声明时拿它自检，别把"没校验"当成"校验通过"。
/// 生产代码不调它（校验走 [`validate`]），它是给守卫测试与人工自检用的
#[cfg(test)]
pub fn unsupported(schema: &Value) -> String {
    let mut found = BTreeSet::new();
    walk(schema, &mut found);
    if found.is_empty() {
        "(声明里的关键字都实现了)".to_string()
    } else {
        found.into_iter().collect::<Vec<_>>().join(", ")
    }
}

/// 只活在测试构建里：它唯一的调用方是 [`unsupported`]（自检工具）
#[cfg(test)]
fn walk(schema: &Value, found: &mut BTreeSet<String>) {
    // 只有对象才是 schema。`required` / `enum` 的值是字符串数组，
    // 把它们当 schema 走会把参数名 `path` 误报成"用了没实现的关键字"
    let Some(map) = schema.as_object() else {
        return;
    };
    for (key, value) in map {
        match key.as_str() {
            "required" | "enum" => {}
            "properties" => {
                if let Some(object) = value.as_object() {
                    for sub in object.values() {
                        walk(sub, found);
                    }
                }
            }
            "items" => walk(value, found),
            "additionalProperties" => {
                // 我们一律按 true 处理（多带字段放过）。声明成 false 或子 schema 时，
                // 那条约束等于没实现，必须报出来
                let unrestricted = value.as_bool().unwrap_or(true);
                if !unrestricted {
                    found.insert("additionalProperties".to_string());
                }
                if value.is_object() {
                    walk(value, found);
                }
            }
            other => {
                if !IMPLEMENTED.contains(&other) && !ANNOTATIONS.contains(&other) {
                    found.insert(other.to_string());
                }
            }
        }
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// `integer` 在服务商的 schema 写法里与 `number` 是两回事，而 serde_json 不区分 i64/u64/f64：
/// 把 `1.0` 判成非法的话，所有回浮点写法的整数都会被误杀
fn type_matches(expected: &str, value: &Value) -> bool {
    let actual = type_name(value);
    if expected == actual {
        return true;
    }
    if expected == "integer" {
        return value
            .as_f64()
            .map(|number| number.fract() == 0.0)
            .unwrap_or(false);
    }
    false
}

fn describe(value: &Value) -> String {
    match value {
        Value::String(text) => {
            format!("字符串 {:?}（{} 字）", clip(text, 40), text.chars().count())
        }
        other => format!("{} {}", type_name(other), clip(&other.to_string(), 40)),
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

fn check(path: &str, schema: &Value, value: &Value, out: &mut Vec<String>) {
    if let Some(expected) = schema.get("type").and_then(Value::as_str) {
        if !type_matches(expected, value) {
            out.push(format!(
                "{path} 要的是 {expected}，给到的是{}",
                describe(value)
            ));
            // 类型都不对，再往下查约束只会产出第二条噪音
            return;
        }
    }

    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.iter().any(|option| option == value) {
            let list = allowed
                .iter()
                .map(|option| option.to_string())
                .collect::<Vec<_>>()
                .join(" / ");
            out.push(format!(
                "{path} 只能是其中之一：{list}；给到的是{}",
                describe(value)
            ));
        }
    }

    if let Some(max) = schema.get("maximum").and_then(Value::as_f64) {
        if let Some(given) = value.as_f64() {
            if given > max {
                out.push(format!("{path} 不能超过 {max}，给到了 {given}"));
            }
        }
    }
    if let Some(min) = schema.get("minimum").and_then(Value::as_f64) {
        if let Some(given) = value.as_f64() {
            if given < min {
                out.push(format!("{path} 不能小于 {min}，给到了 {given}"));
            }
        }
    }

    if let Some(text) = value.as_str() {
        let chars = text.chars().count();
        if let Some(max) = schema.get("maxLength").and_then(Value::as_u64) {
            if chars > max as usize {
                out.push(format!("{path} 最长 {max} 字，给到了 {chars} 字"));
            }
        }
        if let Some(min) = schema.get("minLength").and_then(Value::as_u64) {
            if chars < min as usize {
                out.push(format!("{path} 至少 {min} 字，给到了 {chars} 字"));
            }
        }
    }

    if let Some(object) = value.as_object() {
        if let Some(list) = schema.get("required").and_then(Value::as_array) {
            for key in list.iter().filter_map(Value::as_str) {
                let present = object
                    .get(key)
                    .map(|given| !given.is_null())
                    .unwrap_or(false);
                if !present {
                    out.push(format!("少了 {path}.{key} 这个参数"));
                }
            }
        }
        if let Some(properties) = schema.get("properties") {
            for (key, given) in object {
                // 未声明的字段按 JSON Schema 的默认语义放过：模型多带一个字段不是错误，
                // 把它当错误拒掉只会让工具越来越难调
                if let Some(sub) = properties.get(key) {
                    check(&format!("{path}.{key}"), sub, given, out);
                }
            }
        }
    }

    if let (Some(items), Value::Array(list)) = (schema.get("items"), value) {
        for (index, given) in list.iter().enumerate() {
            check(&format!("{path}[{index}]"), items, given, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "maxLength": 500 },
                "content": { "type": "string" },
                "overwrite": { "type": "boolean" },
                "mode": { "enum": ["append", "replace"] }
            },
            "required": ["path", "content"]
        })
    }

    #[test]
    fn a_well_formed_call_passes() {
        assert!(validate(&write_schema(), &json!({ "path": "a.md", "content": "x" })).is_ok());
    }

    #[test]
    fn a_missing_required_field_is_named_instead_of_guessed() {
        let error = validate(&write_schema(), &json!({ "path": "a.md" }))
            .expect_err("少了 content 必须报错");
        assert!(error.contains("content"), "错误要说清缺了哪个字段：{error}");
        // 缺参数与给错类型是两种不同的失败，文案不该混
        let null = validate(&write_schema(), &json!({ "path": "a.md", "content": null }))
            .expect_err("null 不是字符串");
        assert!(null.contains("content"), "null 按缺失处理并点名：{null}");
    }

    #[test]
    fn a_wrong_type_is_rejected_rather_than_coerced() {
        // 判别性：`path: 3` 若被静默转成 "3"，就会往一个叫 "3" 的文件里写东西
        let error = validate(&write_schema(), &json!({ "path": 3, "content": "x" }))
            .expect_err("数字不是路径");
        assert!(
            error.contains("path") && error.contains("string"),
            "要说清字段、要什么、给到什么：{error}"
        );
    }

    #[test]
    fn length_and_range_bounds_are_enforced() {
        let long = "字".repeat(501);
        let error = validate(&write_schema(), &json!({ "path": long, "content": "x" }))
            .expect_err("超长的路径该被拦");
        assert!(error.contains("500"), "边界值要出现在文案里：{error}");

        let sized = json!({
            "type": "object",
            "properties": { "limit": { "type": "integer", "minimum": 1, "maximum": 100 } },
            "required": ["limit"]
        });
        assert!(validate(&sized, &json!({ "limit": 0 })).is_err());
        assert!(validate(&sized, &json!({ "limit": 101 })).is_err());
        assert!(validate(&sized, &json!({ "limit": 50 })).is_ok());
        assert!(
            validate(&sized, &json!({ "limit": 1.0 })).is_ok(),
            "1.0 是整数写法的一种"
        );
        assert!(
            validate(&sized, &json!({ "limit": 1.5 })).is_err(),
            "1.5 不是整数"
        );
    }

    #[test]
    fn an_enum_value_outside_the_list_is_rejected_with_the_choices() {
        let error = validate(
            &write_schema(),
            &json!({ "path": "a", "content": "x", "mode": "delete" }),
        )
        .expect_err("enum 外的值必须被拦");
        assert!(
            error.contains("append") && error.contains("replace"),
            "要把可选项列出来，模型才知道该改哪个：{error}"
        );
    }

    #[test]
    fn an_undeclared_extra_field_is_passed_through_not_refused() {
        // JSON Schema 默认 additionalProperties 为 true。改成"多带字段就拒"
        // 会让模型每一次多写一个 key 都变成失败，那是给自己加路障
        assert!(validate(
            &write_schema(),
            &json!({ "path": "a", "content": "x", "note": "y" })
        )
        .is_ok());
    }

    #[test]
    fn nested_arrays_are_checked_element_by_element() {
        let schema = json!({
            "type": "object",
            "properties": { "paths": { "type": "array", "items": { "type": "string" } } },
            "required": ["paths"]
        });
        assert!(validate(&schema, &json!({ "paths": ["a", "b"] })).is_ok());
        let error =
            validate(&schema, &json!({ "paths": ["a", 7] })).expect_err("数组里的数字不是路径");
        assert!(error.contains("paths[1]"), "要指到是第几个元素：{error}");
    }

    #[test]
    fn a_keyword_we_do_not_implement_is_reported_not_silently_ignored() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "$ref": "#/definitions/p" } },
            "required": ["path"]
        });
        let report = unsupported(&schema);
        assert!(
            report.contains("$ref"),
            "要报出用了哪些没实现的关键字：{report}"
        );
        assert!(
            validate(&schema, &json!({ "path": 3 })).is_ok(),
            "没实现的关键字被忽略，所以这条过得去——这正是 unsupported 存在的理由"
        );
    }

    #[test]
    fn every_declared_builtin_schema_is_one_we_actually_check() {
        // 这条把"装饰性 schema"钉死：内置工具的参数声明里出现没实现的关键字，测试就红
        let declared = crate::tools::parameter_schemas();
        assert!(!declared.is_empty(), "读不到声明就等于这条测试在空转");
        for (name, schema) in &declared {
            let report = unsupported(schema);
            assert!(
                report.starts_with('('),
                "工具 {name} 的声明用了本校验器不认的关键字，那部分等于没校验：{report}"
            );
        }
    }
}
