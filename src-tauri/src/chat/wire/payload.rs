use super::content::{modal_inputs_of, project_content, ImageDialect, ModalInputs};
use crate::config::AppConfig;
use serde_json::{json, Value};

pub(crate) fn chat_payload(
    config: &AppConfig,
    thread: &[Value],
    declared: &[Value],
    cache_key: Option<&str>,
) -> Value {
    // 思考回放凭据只属于 responses/anthropic 线：chat 线剥掉——DeepSeek 明确
    // 不收 reasoning 字段，多余的键有被 400 的风险。没有凭据的行原样透传，
    // wire 字节与历史完全一致
    let carries_replay = thread
        .iter()
        .any(|row| row.get("thinking_signature").is_some() || row.get("reasoning_items").is_some());
    let inputs = modal_inputs_of(config);
    let messages: Vec<Value> = thread
        .iter()
        .map(|row| {
            let mut row = row.clone();
            if carries_replay {
                if let Some(object) = row.as_object_mut() {
                    object.remove("thinking");
                    object.remove("thinking_signature");
                    object.remove("reasoning_items");
                }
            }
            project_content(&row, ImageDialect::Chat, inputs)
        })
        .collect();
    let mut payload = json!({
        "model": config.model,
        "messages": messages,
        "temperature": config.temperature,
        "max_tokens": config.max_tokens,
        "stream": true,
        "stream_options": { "include_usage": true },
    });

    // 显式缓存身份：只有能力表说支持、且这一笔确实属于某个话题时才带。
    // 一次性调用（摘要/标题/任务/审查）传 None —— 它们的前缀不会有第二条请求来延伸，
    // 写进话题缓存就是纯支出（设计档 §5.2）
    if let (true, Some(identity)) = (config.capability().prompt_cache_key, cache_key) {
        payload["prompt_cache_key"] = json!(crate::provider::capability::cache_key(identity));
    }
    // "默认" 不发这个字段，交给服务商自己的档位
    if !config.reasoning_effort.is_empty() {
        payload["reasoning_effort"] = json!(config.reasoning_effort);
    }
    // 全部关掉时不能发 "tools": []，服务商会当成非法请求，所以整段省略
    if !declared.is_empty() {
        payload["tools"] = json!(declared);
        payload["tool_choice"] = json!("auto");
    }
    payload
}

/// /responses 的工具声明是平铺的（name 在顶层），chat 格式多包了一层 function
pub(crate) fn responses_tools(declared: &[Value]) -> Vec<Value> {
    let mut flat = Vec::new();
    for item in declared {
        let Some(function) = item["function"].as_object() else {
            continue;
        };
        let mut tool = json!({ "type": "function", "strict": false });
        for field in ["name", "description", "parameters"] {
            if let Some(value) = function.get(field) {
                tool[field] = value.clone();
            }
        }
        flat.push(tool);
    }
    flat
}

/// chat 消息数组 → responses 的 input 项。
/// 一条带工具调用的回复要拆成若干独立项：正文和每个 function_call 各一项，
/// 工具结果则变成指向 call_id 的 function_call_output。
pub(crate) fn responses_input(thread: &[Value], inputs: ModalInputs) -> Vec<Value> {
    let mut items = Vec::new();
    for message in thread {
        let message = project_content(message, ImageDialect::Responses, inputs);
        let role = message["role"].as_str().unwrap_or_default();
        // 带图的行 content 是数组。这里不能走 `as_str()`：那会把整行读成空串，
        // 于是"发图的那一问"在 responses 线直接消失，模型以为自己没收到问题
        if message["content"].is_array() {
            items.push(json!({ "role": role, "content": message["content"] }));
            continue;
        }
        let content = message["content"].as_str().unwrap_or_default();

        if role == "tool" {
            items.push(json!({
                "type": "function_call_output",
                "call_id": message["tool_call_id"],
                "output": content,
            }));
            continue;
        }

        // reasoning 项原样透传（store:false 回放）：OpenAI 按 id 把 rs_xxx 与
        // fc_xxx 配对，所以它必须排在它配对的 function_call 之前
        if role == "assistant" {
            if let Some(reasoning_items) = message["reasoning_items"].as_array() {
                for item in reasoning_items {
                    items.push(item.clone());
                }
            }
        }

        if let Some(calls) = message["tool_calls"].as_array() {
            if !content.is_empty() {
                items.push(json!({ "role": role, "content": content }));
            }
            for call in calls {
                items.push(json!({
                    "type": "function_call",
                    "call_id": call["id"],
                    "name": call["function"]["name"],
                    "arguments": call["function"]["arguments"],
                }));
            }
            continue;
        }

        if !content.is_empty() {
            items.push(json!({ "role": role, "content": content }));
        }
    }
    items
}

pub(crate) fn responses_payload(
    config: &AppConfig,
    thread: &[Value],
    declared: &[Value],
    cache_key: Option<&str>,
) -> Value {
    let mut payload = json!({
        "model": config.model,
        "input": responses_input(thread, modal_inputs_of(config)),
        "stream": true,
        // 每轮都重发完整上下文，不让服务端替我们存话题状态
        "store": false,
        "max_output_tokens": config.max_tokens,
        "temperature": config.temperature,
    });

    // 缓存身份与 chat 线同一套门控：能力表说支持、且这一笔属于某个话题才带。
    // responses 线此前漏了这个参数——OpenAI 的前缀缓存同样认它，补齐（pi 同款）
    if let (true, Some(identity)) = (config.capability().prompt_cache_key, cache_key) {
        payload["prompt_cache_key"] = json!(crate::provider::capability::cache_key(identity));
    }

    if !config.reasoning_effort.is_empty() {
        // summary 让服务商回摘要文本；encrypted_content 是 reasoning 项的回放载体——
        // store:false 的多轮里没有它，function_call 就配不上自己的 reasoning 项
        payload["reasoning"] = json!({ "effort": config.reasoning_effort, "summary": "auto" });
        payload["include"] = json!(["reasoning.encrypted_content"]);
    }
    let tools = responses_tools(declared);
    if !tools.is_empty() {
        payload["tools"] = json!(tools);
        payload["tool_choice"] = json!("auto");
    }
    payload
}

/// Anthropic Messages 线的协议版本头。官方要求显式带版本，中转站也认它
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";

/// ephemeral 断点：默认 5 分钟存活期。1 小时档要带 ttl 字段，
/// 而 aglab 的能力表按最短档保温，所以这里固定短档（长档留给未来的配置）
pub(crate) fn cache_control() -> Value {
    json!({ "type": "ephemeral" })
}

/// Anthropic 的工具声明：function 包装摊平，parameters 换名 input_schema
pub(crate) fn anthropic_tools(declared: &[Value]) -> Vec<Value> {
    declared
        .iter()
        .filter_map(|item| {
            let function = item["function"].as_object()?;
            let mut tool = json!({});
            for (wire, source) in [
                ("name", "name"),
                ("description", "description"),
                ("input_schema", "parameters"),
            ] {
                if let Some(value) = function.get(source) {
                    tool[wire] = value.clone();
                }
            }
            Some(tool)
        })
        .collect()
}

/// tool_calls 里存着的 arguments 是 JSON **字符串**；Anthropic 的 tool_use.input
/// 要对象。记录时刻它必然合法（执行前就验过 JSON）；回放撞上坏串（截断轮的
/// 半截参数）宁给空对象也不让整条请求 400——模型会从 tool_result 的说明里
/// 知道那次调用没有执行
pub(crate) fn tool_use_input(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| json!({}))
}

/// 连续同角色的块并进同一条消息。插话、工具结果、用户正文在日志里是三条
/// user 行，Anthropic 那边要合成一条——只画协议要求的消息边界，
/// 块的顺序原样保留，模型看到的字节序不变
pub(crate) fn merge_or_push(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    match messages.last_mut() {
        Some(last) if last["role"] == role => {
            if let Some(existing) = last["content"].as_array_mut() {
                existing.extend(blocks);
            }
        }
        _ => messages.push(json!({ "role": role, "content": blocks })),
    }
}

/// OpenAI 形的消息数组 → Anthropic 的顶层 system + messages。
///
/// - system 行全部抽到顶层 system 参数（Anthropic 不收 role=system 的消息），
///   每行一个 text 块，段落边界原样保留；
/// - user 行翻成 text 块、tool 行翻成 user 消息里的 tool_result 块、
///   assistant 的 tool_calls 翻成 tool_use 块；
/// - 连续同角色的行合并成一条消息（见 [`merge_or_push`]）
pub(crate) fn anthropic_system_and_messages(
    thread: &[Value],
    inputs: ModalInputs,
) -> (Vec<Value>, Vec<Value>) {
    let mut system: Vec<Value> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    for row in thread {
        let row = project_content(row, ImageDialect::Anthropic, inputs);
        let role = row["role"].as_str().unwrap_or_default().to_string();
        // 带图的 user 行：content 已经是本家的块数组（text 与 image 混排），原样并进
        // user 消息。下面的 `as_str()` 对数组会读成空串——那一问就这样消失了
        if role == "user" {
            if let Some(blocks) = row["content"].as_array() {
                if !blocks.is_empty() {
                    merge_or_push(&mut messages, "user", blocks.clone());
                    continue;
                }
            }
        }
        let content = row["content"].as_str().unwrap_or_default();

        if role == "system" {
            if !content.is_empty() {
                system.push(json!({ "type": "text", "text": content }));
            }
            continue;
        }

        if role == "assistant" {
            let mut blocks: Vec<Value> = Vec::new();
            // 思考块必须排在最前：带思考的助手消息要以 thinking 开头，
            // 签名对不上服务商就 400。没有签名的思考（老话题）不回放
            if let (Some(text), Some(signature)) =
                (row["thinking"].as_str(), row["thinking_signature"].as_str())
            {
                blocks.push(json!({
                    "type": "thinking",
                    "thinking": text,
                    "signature": signature,
                }));
            }
            if !content.is_empty() {
                blocks.push(json!({ "type": "text", "text": content }));
            }
            if let Some(calls) = row["tool_calls"].as_array() {
                for call in calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call["id"],
                        "name": call["function"]["name"],
                        "input": tool_use_input(
                            call["function"]["arguments"].as_str().unwrap_or_default(),
                        ),
                    }));
                }
            }
            if !blocks.is_empty() {
                merge_or_push(&mut messages, "assistant", blocks);
            }
            continue;
        }

        // user 与 tool 都翻成 user 角色：tool_result 必须坐在 user 消息里
        let mut blocks: Vec<Value> = Vec::new();
        if role == "tool" {
            blocks.push(json!({
                "type": "tool_result",
                "tool_use_id": row["tool_call_id"],
                "content": content,
            }));
        } else if !content.is_empty() {
            blocks.push(json!({ "type": "text", "text": content }));
        }
        if !blocks.is_empty() {
            merge_or_push(&mut messages, "user", blocks);
        }
    }
    (system, messages)
}

/// Anthropic 预算式思考的档位表（照 pi 的 DEFAULT_THINKING_BUDGETS）；
/// xhigh/max 收敛到 high。空串与未知档位 = 不启用思考
pub(crate) fn anthropic_thinking_budget(reasoning_effort: &str) -> Option<u32> {
    match reasoning_effort.trim() {
        "minimal" => Some(1024),
        "low" => Some(2048),
        "medium" => Some(8192),
        "high" | "xhigh" | "max" => Some(16384),
        _ => None,
    }
}

/// 思考预算与回答共享 max_tokens 上限时，至少给回答留这么多（pi 同款）
pub(crate) const MIN_ANSWER_TOKENS: u32 = 1024;

/// Anthropic Messages 的请求体。
///
/// 断点打三处——system 末块、最后一个工具、最后一条 user 消息的末块。
/// 位置随内容增长而前移正是增量缓存的打点方式：上一轮的断点永远落在
/// 这一轮前缀的内部， grows 的历史逐段落进缓存。
/// 配了思考档位时：预算与 max_tokens 共享上限（至少留 1024 给回答），
/// 且 temperature 整个省略——它与扩展思考不兼容（pi 同款）
pub(crate) fn anthropic_payload(config: &AppConfig, thread: &[Value], declared: &[Value]) -> Value {
    let (mut system, mut messages) = anthropic_system_and_messages(thread, modal_inputs_of(config));
    let mut payload = json!({
        "model": config.model,
        "stream": true,
    });

    // 思考档位：预算加进 max_tokens 再封顶，保证回答至少剩 MIN_ANSWER_TOKENS；
    // 预算太小放不下下限就整个退回无思考档
    let thinking = anthropic_thinking_budget(&config.reasoning_effort).map(|budget| {
        let total = config.max_tokens.saturating_add(budget);
        (total, budget.min(total.saturating_sub(MIN_ANSWER_TOKENS)))
    });
    match thinking {
        Some((total, budget)) if budget >= MIN_ANSWER_TOKENS => {
            payload["max_tokens"] = json!(total);
            payload["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
        }
        _ => {
            payload["max_tokens"] = json!(config.max_tokens);
            payload["temperature"] = json!(config.temperature);
        }
    }

    if !system.is_empty() {
        if let Some(last) = system.last_mut() {
            last["cache_control"] = cache_control();
        }
        payload["system"] = json!(system);
    }
    let mut tools = anthropic_tools(declared);
    if !tools.is_empty() {
        if let Some(last) = tools.last_mut() {
            last["cache_control"] = cache_control();
        }
        payload["tools"] = json!(tools);
    }
    // 末行不是 user（理论上来不到：请求总在用户输入或工具结果落地之后发出）
    // 就只靠 system 与工具两处断点，不硬加
    if let Some(last) = messages.last_mut() {
        if last["role"] == "user" {
            if let Some(blocks) = last["content"].as_array_mut() {
                if let Some(block) = blocks.last_mut() {
                    block["cache_control"] = cache_control();
                }
            }
        }
    }
    payload["messages"] = json!(messages);
    payload
}

// ---- Gemini generateContent（第四条线）----

/// Gemini 的思考档位表（generationConfig.thinkingConfig.thinkingBudget）。
/// 刻意不给 minimal→0：0 对 Flash 是"关闭思考"，对 Pro 是非法值——
/// 请求里不带 thinkingConfig 才是"交给模型默认"，两条模型线都安全
pub(crate) fn gemini_thinking_budget(reasoning_effort: &str) -> Option<i64> {
    match reasoning_effort.trim() {
        "low" => Some(1024),
        "medium" => Some(8192),
        "high" | "xhigh" | "max" => Some(24576),
        _ => None,
    }
}

/// Gemini 的工具声明：functionDeclarations 一层包，参数里的 `$schema`
/// 递归剥掉（Gemini 的 OpenAPI 子集不认它，带着会 400）
pub(crate) fn gemini_tools(declared: &[Value]) -> Vec<Value> {
    fn strip_schema(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.remove("$schema");
                for (_, item) in map.iter_mut() {
                    strip_schema(item);
                }
            }
            Value::Array(items) => {
                for item in items {
                    strip_schema(item);
                }
            }
            _ => {}
        }
    }
    let declarations: Vec<Value> = declared
        .iter()
        .filter_map(|item| {
            let function = item["function"].as_object()?;
            let mut tool = json!({});
            for (wire, source) in [
                ("name", "name"),
                ("description", "description"),
                ("parameters", "parameters"),
            ] {
                if let Some(mut value) = function.get(source).cloned() {
                    strip_schema(&mut value);
                    tool[wire] = value;
                }
            }
            Some(tool)
        })
        .collect();
    (declarations.is_empty())
        .then(Vec::new)
        .unwrap_or_else(|| vec![json!({ "functionDeclarations": declarations })])
}

/// OpenAI 形的消息数组 → Gemini 的 systemInstruction + contents。
///
/// - system 行进顶层 `systemInstruction`（parts 合并）；
/// - user 行翻 text/inline_data parts、assistant 行翻 model 角色（functionCall
///   的 args 从 JSON 串解析成对象）、tool 行翻成 user 消息里的 functionResponse
///   part（`name` 从前面 assistant 的 tool_calls 里按 call_id 反查）；
/// - 连续同角色的行合成一条 contents（Gemini 对连续同角色最友好，但合并后
///   与 Anthropic 线的消息边界一致，跨线切换时历史形状不漂）
pub(crate) fn gemini_system_and_contents(
    thread: &[Value],
    inputs: ModalInputs,
) -> (Option<Value>, Vec<Value>) {
    // call_id → name：functionResponse 必须带函数名，而 tool 行只有 call_id
    let mut call_names: std::collections::BTreeMap<String, String> = Default::default();
    for row in thread {
        for call in row["tool_calls"].as_array().into_iter().flatten() {
            if let (Some(id), Some(name)) = (call["id"].as_str(), call["function"]["name"].as_str())
            {
                call_names.insert(id.to_string(), name.to_string());
            }
        }
    }

    let mut system_instruction: Option<Value> = None;
    let mut contents: Vec<Value> = Vec::new();
    for row in thread {
        let row = project_content(row, ImageDialect::Gemini, inputs);
        let role = row["role"].as_str().unwrap_or_default().to_string();

        if role == "system" {
            let text = row["content"].as_str().unwrap_or_default();
            if !text.is_empty() {
                let part = json!({ "text": text });
                match &mut system_instruction {
                    Some(instruction) => {
                        if let Some(parts) = instruction["parts"].as_array_mut() {
                            parts.push(part);
                        }
                    }
                    None => system_instruction = Some(json!({ "parts": [part] })),
                }
            }
            continue;
        }

        let mut parts: Vec<Value> = Vec::new();
        if role == "user" {
            if let Some(blocks) = row["content"].as_array() {
                for block in blocks {
                    match block["type"].as_str() {
                        Some("image") => parts.push(block.clone()),
                        Some("text") | Some(_) | None => {
                            let text = block["text"].as_str().unwrap_or_default();
                            if !text.is_empty() {
                                parts.push(json!({ "text": text }));
                            }
                        }
                    }
                }
            }
        }
        let content = row["content"].as_str().unwrap_or_default();

        if role == "assistant" {
            if !content.is_empty() {
                parts.push(json!({ "text": content }));
            }
            for call in row["tool_calls"].as_array().into_iter().flatten() {
                parts.push(json!({
                    "functionCall": {
                        "name": call["function"]["name"],
                        "args": tool_use_input(
                            call["function"]["arguments"].as_str().unwrap_or_default(),
                        ),
                    }
                }));
            }
        } else if role == "tool" {
            let call_id = row["tool_call_id"].as_str().unwrap_or_default();
            let name = call_names
                .get(call_id)
                .cloned()
                .unwrap_or_else(|| call_id.to_string());
            parts.push(json!({
                "functionResponse": {
                    "name": name,
                    "response": { "result": content },
                }
            }));
        } else if !content.is_empty() {
            parts.push(json!({ "text": content }));
        }

        if parts.is_empty() {
            continue;
        }
        let wire_role = if role == "assistant" { "model" } else { "user" };
        merge_or_push(&mut contents, wire_role, parts);
    }
    (system_instruction, contents)
}

/// Gemini generateContent 的请求体。流式由服务商的 `?alt=sse` 决定，不在 body 里；
/// 频内缓存（context caching）靠隐式机制，没有显式开关可带
pub(crate) fn gemini_payload(config: &AppConfig, thread: &[Value], declared: &[Value]) -> Value {
    let (system_instruction, contents) =
        gemini_system_and_contents(thread, modal_inputs_of(config));
    let mut payload = json!({
        "contents": contents,
        "generationConfig": {
            "temperature": config.temperature,
            "maxOutputTokens": config.max_tokens,
        },
    });
    if let Some(instruction) = system_instruction {
        payload["systemInstruction"] = instruction;
    }
    if !config.reasoning_effort.is_empty() {
        if let Some(budget) = gemini_thinking_budget(&config.reasoning_effort) {
            payload["generationConfig"]["thinkingConfig"] = json!({ "thinkingBudget": budget });
        }
    }
    let tools = gemini_tools(declared);
    if !tools.is_empty() {
        payload["tools"] = json!(tools);
    }
    payload
}
