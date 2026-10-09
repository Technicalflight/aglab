//! 生图/视频的独立 REST 管线（能力会话的执行侧）。
//!
//! 与对话管线并行的一条路：POST 到模型端点的 `images/generations` /
//! `videos/generations` 接口（cogview、seedream、cogvideo 这类独立 API 的
//! OpenAI 兼容形状），产物落盘成**普通附件**——与粘贴截图同一目录、同一
//! 渲染链路，气泡里的图片/文件 chip 全部复用既有显示。
//!
//! 端点是**用户配置的可信端点**（模型 baseUrl 本身）：模型只给提示词，
//! 动不了请求发往哪台端点，所以私网/回环的自建端点放行——SSRF 闸
//! （refuse_private_target）对它不适用，出口名单照常在。

use base64::Engine as _;
use serde_json::{json, Value};
use std::io::Read as _;
use std::path::Path;
use tauri::AppHandle;

use crate::config::AppConfig;

const GEN_TIMEOUT_SECS: u64 = 120;
/// 视频生成是异步任务：创建之后轮询，5 分钟内每 10 秒问一次
const VIDEO_POLL_TRIES: u64 = 30;
const VIDEO_POLL_INTERVAL_SECS: u64 = 10;

fn agent_with_timeout() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(GEN_TIMEOUT_SECS)))
        // 非 2xx 不自动转 Err：错误响应体里坐着中转的真实原因（无权限/分组不含
        // 视频/令牌无效），先读体再按状态报错，用户才不用对着一个状态码猜
        .http_status_as_error(false)
        .build()
        .new_agent()
}

fn base_of(config: &AppConfig) -> Result<String, String> {
    let base = config.base_url.trim().trim_end_matches('/').to_string();
    crate::egress::require_http_url(&base, "模型端点")?;
    Ok(base)
}

/// 发一发 JSON 请求。响应体整体读回（生成接口的响应很小，没有流），
/// 非 2xx 时把状态码与能拿到的正文片段一起报出来
fn request_json(
    config: &AppConfig,
    url: &str,
    method: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    crate::egress::guard(&config.net_egress_allow, url)?;
    // 鉴权与对话轮同一凭据源：401 的教训——生成请求不带 key 等于必然被拒
    let auth = crate::config::api_key(config)
        .map(|key| format!("Bearer {key}"))
        .unwrap_or_default();
    let agent = agent_with_timeout();
    let result = match (method, body) {
        ("POST", Some(payload)) => agent
            .post(url)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("authorization", &auth)
            .send_json(&payload),
        _ => agent
            .get(url)
            .header("accept", "application/json")
            .header("authorization", &auth)
            .call(),
    };
    let mut response = result.map_err(|error| format!("生成请求失败：{error}"))?;
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(8 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("读取响应失败：{e}"))?;
    if status >= 400 {
        let snippet = String::from_utf8_lossy(&bytes[..bytes.len().min(300)]).trim().to_string();
        return Err(match (status, snippet.is_empty()) {
            // 404 多半是端点/模型根本没有生成接口（选了个对话模型）：
            // 生图会话里的对话模型照样有用——让它帮忙把描述打磨成生图提示词，
            // 再切回生图模型生成
            (404, _) => "生成接口返回 HTTP 404——当前端点或模型没有这一类生成接口。\n生图/视频会话里的对话模型可以这样用：让它把你的描述打磨成一份专业的生成提示词，然后在模型选择器里切回生图/视频模型再生成。".to_string(),
            (_, true) => format!("生成接口返回 HTTP {status}"),
            (_, false) => format!("生成接口返回 HTTP {status}：{snippet}"),
        });
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("响应不是合法 JSON：{e}"))
}

fn sniff_image_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF8") {
        Some("gif")
    } else if bytes.len() > 12 && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

fn save_generated(app: &AppHandle, bytes: &[u8], ext: &str) -> Result<Value, String> {
    use tauri::Manager;
    // 持久目录（app_data_dir/gen）而不是 Temp：Temp 会被系统清理，
    // 存档里的附件路径指过去就成死链——用户重开话题图就没了
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("拿不到数据目录：{e}"))?
        .join("gen");
    std::fs::create_dir_all(&dir).map_err(|e| format!("建生成目录失败：{e}"))?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%3f");
    let name = format!("gen-{stamp}.{ext}");
    let path = dir.join(&name);
    std::fs::write(&path, bytes).map_err(|e| format!("写生成产物失败：{e}"))?;
    Ok(json!({
        "path": path.to_string_lossy(),
        "name": name,
        "bytes": bytes.len(),
    }))
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    crate::egress::guard(&[], url).ok(); // 生成产物地址跟随端点，名单拦不住也要能取回
    let mut response = agent_with_timeout()
        .get(url)
        .header("accept", "image/*,video/*")
        .call()
        .map_err(|error| format!("下载生成产物失败：{error}"))?;
    let status = response.status().as_u16();
    if status >= 400 {
        // 404 的错误页不能被当成视频存下来
        return Err(format!("生成产物下载返回 HTTP {status}：{url}"));
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(64 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("下载生成产物失败：{e}"))?;
    Ok(bytes)
}

/// DMXAPI /responses 形：output[].content[].audio（output_format=url 时是
/// http 地址，24 小时有效；hex 形态在这里不认，避免把十六进制串当地址下载）
fn dmx_output_audio_url(value: &Value) -> Option<String> {
    let output = value["output"].as_array()?;
    for item in output {
        let Some(content) = item.get("content").and_then(|content| content.as_array()) else {
            continue;
        };
        for piece in content {
            if let Some(url) = piece.get("audio").and_then(|audio| audio.as_str()) {
                if url.starts_with("http") {
                    return Some(url.to_string());
                }
            }
        }
    }
    None
}

/// 十六进制串 → 字节（MiniMax 缺省 output_format=hex 时的音频本体）。
/// 长度奇偶或字符不合法都回 None，让调用方自然落到"没找到产物"的报错上
fn hex_decode(value: &str) -> Option<Vec<u8>> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(2) {
        return None;
    }
    (0..bytes.len() / 2)
        .map(|index| {
            let high = (*bytes.get(2 * index)? as char).to_digit(16)?;
            let low = (*bytes.get(2 * index + 1)? as char).to_digit(16)?;
            Some((high * 16 + low) as u8)
        })
        .collect()
}

/// 生成结果里的产物地址：不同端点各家形状不一，按常见位置逐个找。
/// New API 系视频规范（chinallmapi 文档）把产物放**顶层 video_url**，排最前
#[allow(clippy::let_and_return)] // 先绑定再返回：迭代器临时借用不能早于它掉落
fn extract_media_url(value: &Value) -> Option<String> {
    let dmx_audio = dmx_output_audio_url(value);
    let top_audio = value["audio"].as_str().filter(|url| url.starts_with("http"));
    let dashscope_audio = value["output"]["audio"]["url"].as_str();
    let sunoapi_audio = value["data"]["response"]["data"][0]["audio_url"]
        .as_str()
        .or_else(|| value["response"]["data"][0]["audio_url"].as_str());
    // Mureka 官方镜像 chat 形：产物在 choices[0].message.audio（非 http 不当地址）
    let mureka_audio = value["choices"][0]["message"]["audio"]
        .as_str()
        .filter(|url| url.starts_with("http"));
    // MiniMax：output_format=url 时产物在 data.audio（缺省 hex，非 http 不当地址）
    let minimax_audio = value["data"]["audio"]
        .as_str()
        .filter(|url| url.starts_with("http"));
    // RunningHub：结果在 results[].url
    let runninghub_audio = value["results"][0]["url"].as_str();
    let candidates = [
        dmx_audio.as_deref(),
        top_audio,
        dashscope_audio,
        sunoapi_audio,
        mureka_audio,
        minimax_audio,
        runninghub_audio,
        value["choices"][0]["message"]["audio_url"].as_str(),
        value["audio_url"].as_str(),
        value["data"]["audio_url"].as_str(),
        value["data"][0]["audio_url"].as_str(),
        value["video_url"].as_str(),
        value["data"][0]["url"].as_str(),
        value["data"][0]["video_url"].as_str(),
        value["video_result"][0]["url"].as_str(),
        value["output"]["video_url"].as_str(),
        value["output"][0]["url"].as_str(),
        value["url"].as_str(),
    ];
    // 先绑定再返回：迭代器临时借用的局部量（dmx_audio）不能晚于它掉落
    let found = candidates.into_iter().flatten().map(str::to_string).next();
    found
}

/// 各能力档的提示词组装。生图/视频模型把整段 prompt 当**画面/镜头描述**——
/// 系统式前缀（"你是一个…"）会被当成画面内容真的画出来，所以这里刻意
/// 不加任何前缀：匹配能力档的"系统提示"住在界面上（占位词、能力徽标、
/// 产物文案都已按档切换），而不是塞进生成请求。这个接缝留给将来
/// 确认支持系统式引导的端点。
fn compose_prompt(kind: &str, prompt: &str) -> String {
    let _ = kind;
    prompt.to_string()
}

fn generate_image(
    app: &AppHandle,
    config: &AppConfig,
    model: &str,
    prompt: &str,
    options: &Value,
    references: &[String],
) -> Result<Value, String> {
    if !references.is_empty() {
        return generate_image_edits(app, config, model, prompt, options, references);
    }
    let base = base_of(config)?;
    let url = format!("{base}/images/generations");
    crate::egress::guard(&config.net_egress_allow, &url)?;

    let count = options["count"].as_u64().unwrap_or(1).clamp(1, 10) as usize;
    let size = options["size"].as_str().unwrap_or_default();
    let quality = options["quality"].as_str().unwrap_or_default();

    let save_one = |item: &Value, index: usize| -> Result<Option<Value>, String> {
        if let Some(url) = extract_media_url(item) {
            let bytes = download(&url)?;
            let ext = sniff_image_ext(&bytes).unwrap_or("png");
            let mut entry = save_generated(app, &bytes, ext)?;
            entry["index"] = json!(index);
            Ok(Some(entry))
        } else if let Some(b64) = item["b64_json"].as_str() {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| format!("图片数据解码失败：{e}"))?;
            let ext = sniff_image_ext(&bytes).unwrap_or("png");
            let mut entry = save_generated(app, &bytes, ext)?;
            entry["index"] = json!(index);
            Ok(Some(entry))
        } else {
            Ok(None)
        }
    };

    // **并行点火 count 发（每发 n=1）**：认 n 的上游与不认的中转（nano-banana/
    // seedream 系每发只回一张）都走同一个形状——并发把总数凑满，不用串行等。
    // 每发独立失败互不拖累；全部失败才把第一个错误报出去
    let build_payload = || {
        json!({ "model": model, "prompt": compose_prompt("image", prompt), "n": 1,
                "size": if !size.is_empty() { json!(size) } else { Value::Null },
                "quality": if !quality.is_empty() { json!(quality) } else { Value::Null } })
    };
    let mut saved: Vec<Value> = Vec::new();
    let mut first_error: Option<String> = None;
    // 两轮机会：首轮 count 发并发；上游吞掉/失败几张（429、瞬时空响应）时，
    // 第二轮按差数补发。两轮都凑不满才按现状交卷
    for _round in 0..2 {
        if saved.len() >= count {
            break;
        }
        let missing = count - saved.len();
        let mut handles = Vec::new();
        for _ in 0..missing {
            let config_clone = config.clone();
            let url_clone = url.clone();
            let payload = build_payload();
            handles.push(std::thread::spawn(move || {
                request_json(&config_clone, &url_clone, "POST", Some(payload))
            }));
        }
        for handle in handles {
            let value = match handle.join() {
                Ok(Ok(value)) => value,
                Ok(Err(problem)) => {
                    if first_error.is_none() {
                        first_error = Some(problem);
                    }
                    continue;
                }
                Err(_) => continue, // 线程异常：按失败处理，不拖累其他发
            };
            for item in value["data"].as_array().cloned().unwrap_or_default() {
                if saved.len() >= count {
                    break;
                }
                if let Some(entry) = save_one(&item, saved.len())? {
                    saved.push(entry);
                }
            }
        }
    }
    if saved.is_empty() {
        return Err(first_error.unwrap_or_else(|| "生成接口没有回任何图片".into()));
    }
    Ok(json!({ "images": saved }))
}

fn generate_image_edits(
    app: &AppHandle,
    config: &AppConfig,
    model: &str,
    prompt: &str,
    options: &Value,
    references: &[String],
) -> Result<Value, String> {
    use ureq::unversioned::multipart::{Form, Part};

    let base = base_of(config)?;
    let url = format!("{base}/images/edits");
    crate::egress::guard(&config.net_egress_allow, &url)?;

    let count = options["count"].as_u64().unwrap_or(1).clamp(1, 10);
    let size = options["size"].as_str().unwrap_or_default();
    let quality = options["quality"].as_str().unwrap_or_default();
    // Form 的 text 借用 &str：先把 owned 值落成局部变量再组装
    let model_text = model.to_string();
    let prompt_text = compose_prompt("image", prompt);
    let count_text = count.to_string();

    let mut form = Form::new()
        .text("model", &model_text)
        .text("prompt", &prompt_text)
        .text("n", &count_text);
    if !size.is_empty() {
        form = form.text("size", size);
    }
    if !quality.is_empty() {
        form = form.text("quality", quality);
    }
    let mut image_parts: Vec<(&'static str, Part)> = Vec::new();
    let mut attached = 0usize;
    for reference in references {
        let path = std::path::Path::new(reference);
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mime = match sniff_image_ext(&bytes) {
            Some("png") => "image/png",
            Some("jpg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            _ => "application/octet-stream",
        };
        // gpt-image 系的参考图字段名是 image[]（多张逐个 part）；
        // 单图端点（dall-e-2）认 image——两个字段都带通常被忽略其一，先按多张口径
        image_parts.push((
            "image[]",
            Part::owned_reader(std::io::Cursor::new(bytes))
                .file_name(name)
                .mime_str(mime)
                .map_err(|e| format!("参考图的 multipart 头组装失败：{e}"))?,
        ));
        attached += 1;
    }
    if attached == 0 {
        return Err("参考图一张都没读出来：检查附件路径是否还存在。".into());
    }
    for (name, part) in image_parts {
        form = form.part(name, part);
    }

    crate::egress::guard(&config.net_egress_allow, &url)?;
    let auth = crate::config::api_key(config)
        .map(|key| format!("Bearer {key}"))
        .unwrap_or_default();
    let agent = agent_with_timeout();
    let mut response = agent
        .post(&url)
        .header("accept", "application/json")
        .header("authorization", &auth)
        .send(form)
        .map_err(|error| format!("图生图请求失败：{error}"))?;
    let status = response.status().as_u16();
    let mut body = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(64 * 1024 * 1024)
        .read_to_end(&mut body)
        .map_err(|e| format!("读取图生图响应失败：{e}"))?;
    if status >= 400 {
        let snippet = String::from_utf8_lossy(&body[..body.len().min(300)]).trim().to_string();
        return Err(if snippet.is_empty() {
            format!("图生图接口返回 HTTP {status}")
        } else {
            format!("图生图接口返回 HTTP {status}：{snippet}")
        });
    }
    let value: Value = serde_json::from_slice(&body).map_err(|e| format!("图生图响应不是合法 JSON：{e}"))?;

    let items = value["data"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return Err(format!(
            "图生图接口没有回图片：{}",
            serde_json::to_string(&value).unwrap_or_default().chars().take(200).collect::<String>()
        ));
    }
    let mut saved = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let media = if let Some(url) = extract_media_url(item) {
            let bytes = download(&url)?;
            let ext = sniff_image_ext(&bytes).unwrap_or("png");
            save_generated(app, &bytes, ext)?
        } else if let Some(b64) = item["b64_json"].as_str() {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| format!("图片数据解码失败：{e}"))?;
            let ext = sniff_image_ext(&bytes).unwrap_or("png");
            save_generated(app, &bytes, ext)?
        } else {
            continue;
        };
        let mut entry = media;
        entry["index"] = json!(index);
        saved.push(entry);
    }
    if saved.is_empty() {
        return Err("图生图接口回了结果但没有可落盘的图片".into());
    }
    Ok(json!({ "images": saved }))
}

/// 从异步任务的响应里挖任务 id：New API 规范是顶层 id/task_id，
/// 有的包在 data[] 里——逐个位置找
fn extract_task_id(value: &Value) -> Option<String> {
    value["id"]
        .as_str()
        .or_else(|| value["task_id"].as_str())
        .or_else(|| value["taskId"].as_str())
        .or_else(|| value["data"].as_str())
        .or_else(|| value["data"][0]["id"].as_str())
        .or_else(|| value["data"][0]["task_id"].as_str())
        .or_else(|| value["data"]["id"].as_str())
        .or_else(|| value["data"]["task_id"].as_str())
        .or_else(|| value["data"]["taskId"].as_str())
        .map(str::to_string)
}

fn poll_video_task(
    config: &AppConfig,
    query_url: &str,
    poll_body: Option<Value>,
) -> Result<Value, String> {
    let url = query_url.to_string();
    // 阶跃星辰的查询是 POST {task_id}，其余家族是 GET——体在轮询里反复用，逐次克隆
    let (method, body) = match poll_body {
        Some(body) => ("POST", Some(body)),
        None => ("GET", None),
    };
    for _ in 0..VIDEO_POLL_TRIES {
        std::thread::sleep(std::time::Duration::from_secs(VIDEO_POLL_INTERVAL_SECS));
        let value = request_json(config, &url, method, body.clone())?;
        let status = value["task_status"]
            .as_str()
            .or_else(|| value["status"].as_str())
            .or_else(|| value["data"]["status"].as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        // New API 规范用 completed（chinallmapi 文档），智谱系用 success
        if matches!(status.as_str(), "success" | "succeeded" | "completed") {
            return Ok(value);
        }
        if matches!(status.as_str(), "failed" | "error" | "cancelled") {
            // 规范的失败原因在 error 字段（字符串），智谱系在 fail_reason；
            // error 是对象时序列化整个带出来
            let reason = value["fail_reason"]
                .as_str()
                .or_else(|| value["error"].as_str())
                .or_else(|| value["errorMessage"].as_str())
                .or_else(|| value["data"]["errorMessage"].as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    let error = &value["error"];
                    if error.is_object() {
                        serde_json::to_string(error).unwrap_or_default()
                    } else {
                        "未知原因".into()
                    }
                });
            return Err(format!("视频生成失败：{reason}"));
        }
    }
    Err(format!(
        "视频生成超时（{} 秒还没有完成）。任务可能仍在端点上跑，稍后重试。",
        VIDEO_POLL_TRIES * VIDEO_POLL_INTERVAL_SECS
    ))
}

/// 视频生成的接口形状没有行业标准：New API 规范系 /videos（chinallmapi 文档，
/// ai.xmiaom 实测存在）、聚合中转系 /video/generations——按名单逐个试，
/// 哪个不返回"此路不通"（404/403/405）就用哪个。任务查询路径跟随命中的家族。
/// /videos/generations 用户实测无中转在用，已从名单移除
fn video_api_families() -> Vec<(&'static str, &'static str)> {
    vec![
        ("/videos", "/videos"),
        ("/video/generations", "/video/generations"),
    ]
}

/// 参照素材的体积上限：base64 后体积 ×4/3，JSON 请求体扛不动太大的素材
const IMAGE_MATERIAL_MAX_BYTES: u64 = 10 * 1024 * 1024;
const VIDEO_MATERIAL_MAX_BYTES: u64 = 30 * 1024 * 1024;
/// 转写的音频上限（Whisper 族常见 25MB 上限，留点余量）
const AUDIO_MATERIAL_MAX_BYTES: u64 = 25 * 1024 * 1024;

fn material_mime(path: &str) -> &'static str {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        _ => "application/octet-stream",
    }
}

/// 本地参照素材 → base64 data URL。文档口径的 image_url/video_url 都吃
/// 公网 URL 或平台资产 id，但 base64 data URL 是 New API 系普遍接受的形状
/// （劝退 base64 是因为体积不是格式）；失败时错误原样透传给节点对话
fn material_data_url(path: &str, max_bytes: u64) -> Result<String, String> {
    use base64::Engine as _;
    let metadata = std::fs::metadata(path).map_err(|e| format!("无法访问 {path}: {e}"))?;
    if metadata.len() > max_bytes {
        return Err(format!(
            "参照素材超过 {} MB：{path}。先压一压再当参照用。",
            max_bytes / 1024 / 1024
        ));
    }
    let mut buffer = Vec::new();
    std::fs::File::open(path)
        .and_then(|mut handle| handle.read_to_end(&mut buffer))
        .map_err(|e| format!("读取失败: {e}"))?;
    Ok(format!(
        "data:{};base64,{}",
        material_mime(path),
        base64::engine::general_purpose::STANDARD.encode(&buffer)
    ))
}

/// 视频生成的请求体装配（三种生成模式共用）：
/// - 无素材：顶层 prompt 的老形状（实测通行的最小形状）
/// - 全能参考（omni）：图片按 reference_image 角色进 content 数组
/// - 首尾帧（frames）：第一张 first_frame、第二张 last_frame
/// - 视频编辑（edit）：素材视频按 video_url 进 content 数组
///   参数（resolution/duration/ratio）有才带。纯装配不发包，测试可以直接喂
fn video_payload(
    model: &str,
    prompt: &str,
    options: &Value,
    mode: &str,
    image_paths: &[String],
    video_path: Option<&str>,
) -> Result<Value, String> {
    let mut payload = json!({ "model": model });
    if let Some(resolution) = options["resolution"].as_str().filter(|v| !v.is_empty()) {
        // 分辨率口径：面板档位是大写 P（480P/720P/1080P），端点的 Zod 枚举全小写
        // （360p/480p/720p/768p/1080p…）——真机 400（"invalid_value"）的教训。只把
        // 结尾的 P 落小写，"1K"/"2K" 这类 K 档本来就以大写合法，不动
        let normalized = match resolution.strip_suffix('P') {
            Some(base) => format!("{base}p"),
            None => resolution.to_string(),
        };
        payload["resolution"] = json!(normalized);
    }
    if let Some(duration) = options["duration"].as_u64() {
        payload["duration"] = json!(duration);
    }
    if let Some(ratio) = options["ratio"].as_str().filter(|v| !v.is_empty()) {
        payload["ratio"] = json!(ratio);
    }

    let mut content = vec![json!({ "type": "text", "text": compose_prompt("video", prompt) })];
    let mut has_material = false;
    for (index, path) in image_paths.iter().enumerate() {
        let data_url = material_data_url(path, IMAGE_MATERIAL_MAX_BYTES)?;
        let role = if mode == "frames" {
            if index == 0 { "first_frame" } else { "last_frame" }
        } else {
            "reference_image"
        };
        content.push(json!({
            "type": "image_url",
            "role": role,
            "image_url": { "url": data_url },
        }));
        has_material = true;
    }
    if let Some(path) = video_path {
        let data_url = material_data_url(path, VIDEO_MATERIAL_MAX_BYTES)?;
        content.push(json!({
            "type": "video_url",
            "video_url": { "url": data_url },
        }));
        has_material = true;
    }

    if has_material {
        payload["content"] = Value::Array(content);
    } else {
        payload["prompt"] = json!(compose_prompt("video", prompt));
    }
    Ok(payload)
}

fn generate_video(
    app: &AppHandle,
    config: &AppConfig,
    model: &str,
    prompt: &str,
    options: &Value,
    reference_images: &[String],
    video_reference: Option<&str>,
) -> Result<Value, String> {
    let _ = app;
    let base = base_of(config)?;
    let mode = options["mode"].as_str().unwrap_or("omni");
    let payload = video_payload(model, prompt, options, mode, reference_images, video_reference)?;
    let mut attempts: Vec<String> = Vec::new();

    for (create_path, query_path) in video_api_families() {
        let url = format!("{base}{create_path}");
        crate::egress::guard(&config.net_egress_allow, &url)?;
        let created = match request_json(config, &url, "POST", Some(payload.clone())) {
            Ok(value) => value,
            Err(problem) => {
                // 额度类 403 是账户级拒绝（余额不够预扣费），与路径无关——
                // 换家族重试必然同样失败，直接上报省得报两条一样的
                if problem.contains("insufficient_user_quota") || problem.contains("quota") {
                    return Err(problem);
                }
                // 404/403/405 都可能是"这条家族不存在"的表达：多数网关回 404，
                // 但也有对未知路径回 403（前置防护）或 405 的。换下一条家族再试；
                // 401 这类真鉴权问题不试，照实报
                let shape_miss = ["HTTP 404", "HTTP 403", "HTTP 405"]
                    .iter()
                    .any(|marker| problem.contains(marker));
                if shape_miss {
                    attempts.push(format!("{create_path} → {problem}"));
                    continue;
                }
                return Err(problem); // 真鉴权/参数问题照实报
            }
        };
        // 同步端点：创建响应里直接带产物地址，不用轮询
        if let Some(url) = extract_media_url(&created) {
            let bytes = download(&url)?;
            return save_generated(app, &bytes, "mp4");
        }
        let Some(task_id) = extract_task_id(&created) else {
            // 创建已被接受（200），再换家族重试同一个 prompt 会重复建任务重复计费
            // ——直接带出响应体报错，把新形状补进 extract_task_id 就能认
            return Err(format!(
                "生成接口没有回任务 id（视频生成是异步任务）：{}",
                serde_json::to_string(&created).unwrap_or_default().chars().take(200).collect::<String>()
            ));
        };
        let query_url = format!("{base}{query_path}/{task_id}");
        let done = poll_video_task(config, &query_url, None)?;
        return match extract_media_url(&done) {
            Some(url) => {
                let bytes = download(&url)?;
                save_generated(app, &bytes, "mp4")
            }
            None => Err(format!(
                "视频任务完成但没有回产物地址：{}",
                serde_json::to_string(&done).unwrap_or_default().chars().take(200).collect::<String>()
            )),
        };
    }
    if attempts.is_empty() {
        return Err("视频生成失败：没有可尝试的接口路径".into());
    }
    // 每条家族的真实响应都带出来——下次失败不用再猜是路径、渠道还是参数
    Err(format!(
        "视频生成失败——所有已知的生成接口路径都不可用：\n{}",
        attempts.join("\n")
    ))
}

/// 文本生成：一次性 chat completions（非流式）。视频画布的节点里"让模型先帮你
/// 把灵感写成分镜/文案"用的就是它——不走对话轮（没有系统提示词/工具/历史），
/// 一句进一句出，结果作为文本气泡落在节点自己的对话里
fn generate_text(config: &AppConfig, model: &str, prompt: &str) -> Result<Value, String> {
    let base = base_of(config)?;
    let url = format!("{base}/chat/completions");
    crate::egress::guard(&config.net_egress_allow, &url)?;
    let payload = json!({
        "model": model,
        "messages": [{ "role": "user", "content": compose_prompt("text", prompt) }],
        "stream": false,
    });
    let value = request_json(config, &url, "POST", Some(payload))?;
    let text = value["choices"][0]["message"]["content"]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "文本生成没有回内容：{}",
                serde_json::to_string(&value).unwrap_or_default().chars().take(200).collect::<String>()
            )
        })?;
    Ok(json!({ "text": text }))
}

/// 音频生成：OpenAI 兼容族的 /audio/speech（TTS）。响应就是音频字节，
/// 不是 JSON——按 content-type 判断形状，错误时才是 JSON
fn generate_audio(app: &AppHandle, config: &AppConfig, model: &str, prompt: &str) -> Result<Value, String> {
    use std::io::Read as _;

    let base = base_of(config)?;
    let url = format!("{base}/audio/speech");
    crate::egress::guard(&config.net_egress_allow, &url)?;
    let auth = crate::config::api_key(config)
        .map(|key| format!("Bearer {key}"))
        .unwrap_or_default();
    let payload = json!({
        "model": model,
        "input": compose_prompt("audio", prompt),
        // 声色先用通用默认：各家中转的音色名单不一致，接用户可配时再放开
        "voice": "alloy",
        "response_format": "mp3",
    });
    let mut response = agent_with_timeout()
        .post(&url)
        .header("content-type", "application/json")
        .header("accept", "audio/mpeg")
        .header("authorization", &auth)
        .send_json(&payload)
        .map_err(|error| format!("音频请求失败：{error}"))?;
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(64 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("读取音频响应失败：{e}"))?;
    if status >= 400 {
        let snippet = String::from_utf8_lossy(&bytes[..bytes.len().min(300)]).trim().to_string();
        return Err(if snippet.is_empty() {
            format!("音频接口返回 HTTP {status}")
        } else {
            format!("音频接口返回 HTTP {status}：{snippet}")
        });
    }
    // 成功的响应体应该是音频；有些网关 200 也回 JSON 错误说明——按魔数分辨
    if bytes.starts_with(b"{") {
        let value: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&bytes)}));
        return Err(format!(
            "音频接口没有回音频字节：{}",
            serde_json::to_string(&value).unwrap_or_default().chars().take(200).collect::<String>()
        ));
    }
    save_generated(app, &bytes, "mp3")
}

#[cfg(test)]
mod video_payload_tests {
    use super::*;
    use std::fs;

    fn material(dir: &std::path::Path, name: &str, bytes: &[u8]) -> String {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn png_bytes() -> Vec<u8> {
        vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A]
    }

    #[test]
    fn no_material_keeps_the_top_level_prompt_shape() {
        let options = json!({ "resolution": "720P", "duration": 5, "ratio": "16:9" });
        let payload =
            video_payload("veo-3.1-lite", "一只跳跃的猫", &options, "omni", &[], None).unwrap();
        assert_eq!(payload["prompt"], "一只跳跃的猫");
        assert!(payload.get("content").is_none(), "没素材不造 content 数组");
        // 面板的大写 P 档位发出去要落成端点枚举的小写口径
        assert_eq!(payload["resolution"], "720p");
        assert_eq!(payload["duration"], 5);
        assert_eq!(payload["model"], "veo-3.1-lite");
    }

    #[test]
    fn omni_mode_marks_every_image_as_reference() {
        let root = crate::test_support::scoped_temp_dir("video-payload-omni");
        let first = material(root.path.as_path(), "a.png", &png_bytes());
        let second = material(root.path.as_path(), "b.png", &png_bytes());
        let payload = video_payload(
            "seedance",
            "参考这两张图的风格",
            &json!({}),
            "omni",
            &[first, second],
            None,
        )
        .unwrap();
        let content = payload["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text", "第一格是提示词");
        assert_eq!(content[1]["role"], "reference_image");
        assert_eq!(content[2]["role"], "reference_image");
        assert!(
            content[1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,"),
            "本地素材以 base64 data URL 进请求"
        );
        assert!(payload.get("prompt").is_none(), "有素材时提示词住在 content 里");
    }

    #[test]
    fn frames_mode_marks_first_and_last() {
        let root = crate::test_support::scoped_temp_dir("video-payload-frames");
        let first = material(root.path.as_path(), "first.png", &png_bytes());
        let last = material(root.path.as_path(), "last.png", &png_bytes());
        let payload = video_payload(
            "seedance",
            "首帧到尾帧",
            &json!({}),
            "frames",
            &[first, last],
            None,
        )
        .unwrap();
        let content = payload["content"].as_array().unwrap();
        assert_eq!(content[1]["role"], "first_frame");
        assert_eq!(content[2]["role"], "last_frame");
    }

    #[test]
    fn edit_mode_carries_the_source_video() {
        let root = crate::test_support::scoped_temp_dir("video-payload-edit");
        let clip = material(root.path.as_path(), "clip.mp4", b"not-a-real-video");
        let payload = video_payload(
            "seedance",
            "把画面调成雪天",
            &json!({}),
            "edit",
            &[],
            Some(&clip),
        )
        .unwrap();
        let content = payload["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], "video_url");
        assert!(
            content[1]["video_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:video/mp4;base64,")
        );
    }

    #[test]
    fn oversized_material_is_refused_before_any_request() {
        let root = crate::test_support::scoped_temp_dir("video-payload-size");
        let big = material(root.path.as_path(), "big.png", &vec![0u8; 11 * 1024 * 1024]);
        let outcome = video_payload("veo", "x", &json!({}), "omni", &[big], None);
        assert!(outcome.is_err(), "超限素材在装配时就被拒，不发请求");
        // base64 编码确有发生：小素材解得回来
        let small = material(root.path.as_path(), "s.png", &png_bytes());
        let encoded = material_data_url(&small, IMAGE_MATERIAL_MAX_BYTES).unwrap();
        let raw = encoded.split(",").nth(1).unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD.decode(raw).unwrap(),
            png_bytes()
        );
    }

    #[test]
    fn dmx_responses_audio_is_extracted() {
        // DMXAPI /responses 形（output_format=url）：音频地址住在 output[].content[].audio
        let created = json!({
            "model": "music-2.0",
            "output": [
                { "type": "message", "content": [
                    { "type": "output_audio", "audio": "https://cdn.example.com/out.mp3" }
                ]}
            ]
        });
        assert_eq!(
            extract_media_url(&created).as_deref(),
            Some("https://cdn.example.com/out.mp3")
        );
    }

    #[test]
    fn dmx_hex_audio_is_not_taken_as_a_url() {
        // output_format 缺省是 hex：那段十六进制不是地址，不能抓去下载
        let created = json!({
            "output": [
                { "content": [ { "audio": "1a2b3c4d5e6f" } ] }
            ]
        });
        assert_eq!(extract_media_url(&created), None);
    }

    #[test]
    fn dashscope_audio_url_is_extracted() {
        // 百聆 Fun-Music 形：产物在 output.audio.url（24 小时有效）
        let created = json!({
            "model": "fun-music-v1",
            "output": { "audio": { "url": "https://dashscope-result.oss.example.com/song.mp3" } }
        });
        assert_eq!(
            extract_media_url(&created).as_deref(),
            Some("https://dashscope-result.oss.example.com/song.mp3")
        );
    }

    #[test]
    fn sunoapi_task_id_and_audio_are_extracted() {
        // sunoapi.org 创建响应：{code: 200, data: {taskId}}（data 是对象，taskId 驼峰）
        let created = json!({ "code": 200, "message": "success", "data": { "taskId": "abc-123" } });
        assert_eq!(extract_task_id(&created).as_deref(), Some("abc-123"));

        // 轮询完成响应：产物在 data.response.data[].audio_url
        let done = json!({
            "code": 200,
            "data": {
                "taskId": "abc-123",
                "status": "SUCCESS",
                "response": { "data": [ { "audio_url": "https://cdn.example.com/song.mp3" } ] }
            }
        });
        assert_eq!(
            extract_media_url(&done).as_deref(),
            Some("https://cdn.example.com/song.mp3")
        );
    }

    #[test]
    fn string_business_code_is_not_mistaken_for_an_error() {
        // New API suno 网关的 code 是字符串 "success"——业务码透传只认数字非 200
        let created = json!({ "code": "success", "data": ["task-id"] });
        assert!(
            created["code"].as_i64().filter(|code| *code != 200).is_none(),
            "字符串 code 不能触发业务错误透传"
        );
    }

    #[test]
    fn mureka_official_audio_is_extracted() {
        // Mureka 官方镜像 chat 形：产物在 choices[0].message.audio（不是 audio_url）
        let done = json!({
            "id": "task-9",
            "status": "succeeded",
            "choices": [
                { "message": { "role": "assistant", "content": "歌词…",
                    "audio": "https://cdn.mureka.cn/song.mp3" } }
            ]
        });
        assert_eq!(
            extract_media_url(&done).as_deref(),
            Some("https://cdn.mureka.cn/song.mp3")
        );
        // 非 http 的串（比如 hex）不当地址
        let hexed = json!({
            "choices": [ { "message": { "audio": "1a2b3c" } } ]
        });
        assert_eq!(extract_media_url(&hexed), None);
    }

    #[test]
    fn hex_decode_roundtrips_audio_bytes() {
        assert_eq!(hex_decode("1a2b3c"), Some(vec![0x1a, 0x2b, 0x3c]));
        assert_eq!(hex_decode(""), None, "空串不是音频");
        assert_eq!(hex_decode("1a2"), None, "奇数长度不合法");
        assert_eq!(hex_decode("zz"), None, "非十六进制字符不合法");
    }

    #[test]
    fn minimax_and_runninghub_shapes_are_extracted() {
        // MiniMax（output_format=url）：产物在 data.audio；hex 形态不当地址
        assert_eq!(
            extract_media_url(&json!({
                "data": { "audio": "https://cdn.minimax.io/song.mp3", "status": 2 }
            })).as_deref(),
            Some("https://cdn.minimax.io/song.mp3")
        );
        assert_eq!(
            extract_media_url(&json!({ "data": { "audio": "fffe0102" } })),
            None
        );
        // RunningHub：任务 id 在顶层 taskId（驼峰），产物在 results[].url
        let submitted = json!({ "taskId": "2013508786", "status": "RUNNING", "results": null });
        assert_eq!(extract_task_id(&submitted).as_deref(), Some("2013508786"));
        assert_eq!(
            extract_media_url(&json!({
                "status": "SUCCESS",
                "results": [ { "url": "https://cos.example.com/song.mp3", "outputType": "mp3" } ]
            })).as_deref(),
            Some("https://cos.example.com/song.mp3")
        );
    }

    #[test]
    fn music_families_carry_the_dmx_shape() {
        let families = music_api_families();
        assert_eq!(
            families.last().copied(),
            Some(("/responses", "", "dmx")),
            "DMXAPI 家族收尾：/responses 同步形，无轮询路径"
        );
        assert!(
            families.iter().any(|(path, query, style)| {
                *path == "/services/audio/music/generation" && query.is_empty() && *style == "dashscope"
            }),
            "百聆 Fun-Music 家族在列（DashScope 原生形，同步无轮询）"
        );
        assert!(
            families.iter().any(|(path, query, style)| {
                *path == "/generate" && *query == "/generate/record-info?taskId=" && *style == "sunoapi"
            }),
            "sunoapi.org 家族在列（query string 轮询形）"
        );
        assert!(
            families.iter().any(|(path, query, style)| {
                *path == "/song/generate" && *query == "/song/query" && *style == "mureka-official"
            }) && families.iter().any(|(path, query, style)| {
                *path == "/audio/music/submit" && *query == "/audio/music/query" && *style == "stepfun"
            }),
            "Mureka 官方与阶跃星辰家族在列（创建端点随选项重选/POST 轮询）"
        );
        assert!(
            families.iter().any(|(path, query, style)| {
                *path == "/music_generation" && query.is_empty() && *style == "minimax"
            }) && families.iter().any(|(path, query, style)| {
                *path == "/openapi/v2/rhart-audio/suno-v5.5/single"
                    && *query == "/openapi/v2/query"
                    && *style == "runninghub"
            }),
            "MiniMax 与 RunningHub 家族在列（同步形/POST 轮询形）"
        );
        assert!(
            families.iter().all(|(path, _, _)| path.starts_with('/')),
            "家族路径都以 / 开头，拼 base 时不会少斜杠"
        );
    }
}

/// 音乐生成的接口形状也没有行业标准：
/// Mureka 官方形 /music/generations（响应镜像 chat 形，产物在
/// choices[0].message.audio_url，也可能是任务 id 再轮询同路径）；
/// New API 的 Suno 网关 /suno/submit/music + /suno/fetch/{id}；
/// 阿里云百聆 Fun-Music /services/audio/music/generation（DashScope
/// 原生形，input.prompt|lyrics 二选一——同时传仅 lyrics 生效，产物在
/// output.audio.url；只出人声歌曲，没有纯音乐参数）；
/// sunoapi.org 的 /generate + /generate/record-info?taskId=（query string
/// 轮询、状态大写、产物在 data.response.data[].audio_url，customMode 下
/// prompt=歌词/风格/title 必填）；HTTP 200 里数字 code!=200 是业务错误，透传；
/// Mureka 官方 v1 按选项动态选端点（歌词/纯描述/纯乐器三态，查询 /query/{id}，
/// 产物在 choices[0].message.audio）；阶跃星辰 /audio/music/submit + **POST**
/// /audio/music/query（body {task_id}），产物是 audio 字段的 base64 本体；
/// MiniMax 官方 /music_generation（同步，{model, prompt, lyrics?, is_instrumental?,
/// audio_setting, output_format:"url"}——没词又非纯音乐开 lyrics_optimizer 自动写词；
/// 业务码 base_resp.status_code，产物 data.audio 的 url/hex 二态，hex 走解码）；
/// RunningHub /openapi/v2/rhart-audio/suno-v5.5/single|custom + **POST**
/// /openapi/v2/query（{taskId}，信封 {status 大写, errorMessage, results[].url}）；
/// DMXAPI 的 /responses（OpenAI Responses 壳，output[].content[].audio）。
/// 按名单逐个试，参照素材/参数（歌词、纯乐器）由 options 带
fn music_api_families() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("/music/generations", "/music/generations", "mureka"),
        ("/suno/submit/music", "/suno/fetch", "suno"),
        // 阿里云百聆 Fun-Music（DashScope 原生形）：同步回 output.audio.url
        ("/services/audio/music/generation", "", "dashscope"),
        // sunoapi.org 形：/generate 异步拿 data.taskId，轮询 ?taskId= 形（query string，
        // 不是路径拼接），状态大写 SUCCESS/FAILED，产物在 data.response.data[].audio_url
        ("/generate", "/generate/record-info?taskId=", "sunoapi"),
        // Mureka 官方 v1：按选项动态选创建端点（有歌词→歌词生成歌曲 /song/generate、
        // 无歌词→纯描述 /song/easy-generate、纯乐器→/instrumental/generate），
        // 查询跟随 GET {create 家}/query/{task_id}，产物在 choices[0].message.audio
        ("/song/generate", "/song/query", "mureka-official"),
        // 阶跃星辰 step-music：提交 /audio/music/submit，**POST** 轮询 /audio/music/query
        //（body {task_id}），成功产物不是地址是 base64 音频本体（audio 字段）
        ("/audio/music/submit", "/audio/music/query", "stepfun"),
        // MiniMax 官方：同步回产物（output_format=url 拿 http 地址，缺省 hex 是
        // 音频本体），业务码在 base_resp.status_code（0 为成功）
        ("/music_generation", "", "minimax"),
        // RunningHub：任务形，有歌词换 custom 端点（{title,prompt=歌词,tags=风格}），
        // 无歌词 single（{description, make_instrumental}）；POST 轮询 {taskId}，
        // 产物在 results[].url，状态大写 SUCCESS/FAILED
        ("/openapi/v2/rhart-audio/suno-v5.5/single", "/openapi/v2/query", "runninghub"),
        // DMXAPI 形：音乐走 /responses（OpenAI Responses 壳），同步回音频
        ("/responses", "", "dmx"),
    ]
}

fn generate_music(
    app: &AppHandle,
    config: &AppConfig,
    model: &str,
    prompt: &str,
    options: &Value,
) -> Result<Value, String> {
    let base = base_of(config)?;
    let lyrics = options["lyrics"].as_str().filter(|v| !v.is_empty());
    let instrumental = options["instrumental"].as_bool() == Some(true);
    let mut attempts: Vec<String> = Vec::new();

    for (create_path, query_path, style) in music_api_families() {
        // 请求体按家族装配：Mureka/Suno 吃 prompt/lyrics/instrumental；
        // DMXAPI 的 /responses 吃 input/lyrics + audio_setting（output_format=url 拿回 http 地址）
        let payload = match style {
            "dmx" => {
                let mut input = compose_prompt("audio", prompt);
                if instrumental {
                    input.push_str("
纯音乐，无人声。");
                }
                let mut body = json!({
                    "model": model,
                    "input": input,
                    "audio_setting": {
                        "sample_rate": 44100,
                        "bitrate": 256000,
                        "format": "mp3",
                    },
                    "stream": false,
                    "output_format": "url",
                });
                if let Some(lyrics) = lyrics {
                    body["lyrics"] = json!(lyrics);
                }
                body
            }
            "dashscope" => {
                // Fun-Music 只生成人声歌曲（无纯音乐参数）：有歌词就只发歌词
                //（同时传仅 lyrics 生效，prompt 白带），没歌词才发描述
                let input = if let Some(lyrics) = lyrics {
                    json!({ "lyrics": lyrics })
                } else {
                    json!({ "prompt": compose_prompt("audio", prompt) })
                };
                json!({ "model": model, "input": input })
            }
            "mureka-official" => {
                // 官方 v1：歌词生成歌曲吃 {model, lyrics, prompt?}（prompt=风格描述）、
                // 纯描述走 easy-generate、纯乐器走 instrumental/generate（只吃 prompt）
                let mut body = json!({ "model": model });
                match (instrumental, lyrics) {
                    (true, _) => {
                        body["prompt"] = json!(compose_prompt("audio", prompt));
                    }
                    (false, Some(lyrics)) => {
                        body["lyrics"] = json!(lyrics);
                        if !prompt.trim().is_empty() {
                            body["prompt"] = json!(prompt);
                        }
                    }
                    (false, None) => {
                        body["prompt"] = json!(compose_prompt("audio", prompt));
                    }
                }
                body
            }
            "stepfun" => {
                // 阶跃：caption 必填（风格描述；空则取歌词首行兜底），
                // instrumental 时不能带 lyrics；要 mp3 就显式要（默认 wav）
                let caption = if prompt.trim().is_empty() {
                    lyrics
                        .and_then(|text| {
                            text.lines().map(str::trim).find(|line| !line.is_empty())
                        })
                        .unwrap_or("一首动听的歌曲")
                } else {
                    prompt
                };
                let mut body = json!({
                    "task": "text_to_music",
                    "model_id": model,
                    "caption": caption,
                    "response_format": "mp3",
                });
                if instrumental {
                    body["instrumental"] = json!(true);
                } else if let Some(lyrics) = lyrics {
                    body["lyrics"] = json!(lyrics);
                }
                body
            }
            "minimax" => {
                // MiniMax：prompt/caption 式描述必填；非纯音乐时 lyrics 必填——
                // 没词就开 lyrics_optimizer 让上游自动写词；output_format=url 拿地址
                let mut body = json!({
                    "model": model,
                    "prompt": compose_prompt("audio", prompt),
                    "audio_setting": {
                        "sample_rate": 44100,
                        "bitrate": 256000,
                        "format": "mp3",
                    },
                    "output_format": "url",
                });
                if instrumental {
                    body["is_instrumental"] = json!(true);
                } else {
                    match lyrics {
                        Some(lyrics) => {
                            body["lyrics"] = json!(lyrics);
                        }
                        None => {
                            body["lyrics_optimizer"] = json!(true);
                        }
                    }
                }
                body
            }
            "runninghub" => {
                // single 吃 {description, make_instrumental}（字符串布尔）；有歌词
                // 换 custom：title 必填（取歌词首行）、prompt=歌词、tags=风格描述
                let body = match lyrics {
                    Some(lyrics) => {
                        let title = lyrics
                            .lines()
                            .map(str::trim)
                            .find(|line| !line.is_empty())
                            .map(|line| line.chars().take(80).collect::<String>())
                            .unwrap_or_else(|| "未命名歌曲".to_string());
                        let mut custom = json!({
                            "title": title,
                            "prompt": lyrics,
                            "tags": prompt,
                        });
                        if instrumental {
                            custom["make_instrumental"] = json!("true");
                        }
                        custom
                    }
                    None => json!({
                        "description": compose_prompt("audio", prompt),
                        "make_instrumental": if instrumental { "true" } else { "false" },
                    }),
                };
                body
            }
            "sunoapi" => {
                // customMode=true：prompt=歌词、style=风格描述、title 必填（取歌词首行）
                let mut body = match lyrics {
                    Some(lyrics) => {
                        let title = lyrics
                            .lines()
                            .map(str::trim)
                            .find(|line| !line.is_empty())
                            .map(|line| line.chars().take(30).collect::<String>())
                            .unwrap_or_else(|| "未命名歌曲".to_string());
                        let mut custom = json!({
                            "prompt": lyrics,
                            "title": title,
                            "customMode": true,
                            "model": model,
                        });
                        if !prompt.trim().is_empty() {
                            custom["style"] = json!(prompt);
                        }
                        custom
                    }
                    None => json!({
                        "prompt": compose_prompt("audio", prompt),
                        "customMode": false,
                        "model": model,
                    }),
                };
                if instrumental {
                    body["instrumental"] = json!(true);
                }
                body
            }
            _ => {
                let mut body = json!({ "model": model, "prompt": compose_prompt("audio", prompt) });
                if let Some(lyrics) = lyrics {
                    body["lyrics"] = json!(lyrics);
                }
                if instrumental {
                    body["instrumental"] = json!(true);
                }
                body
            }
        };
        // 创建端点跟选项走的家族：Mureka 官方三态（歌词/纯描述/纯乐器）、
        // RunningHub 有歌词换 custom 端点（查询路径不变）
        let (create_path, query_path) = match style {
            "mureka-official" if instrumental => ("/instrumental/generate", "/instrumental/query"),
            "mureka-official" if lyrics.is_some() => ("/song/generate", "/song/query"),
            "mureka-official" => ("/song/easy-generate", "/song/query"),
            "runninghub" if lyrics.is_some() => {
                ("/openapi/v2/rhart-audio/suno-v5.5/custom", query_path)
            }
            _ => (create_path, query_path),
        };
        let url = format!("{base}{create_path}");
        crate::egress::guard(&config.net_egress_allow, &url)?;
        let created = match request_json(config, &url, "POST", Some(payload.clone())) {
            Ok(value) => value,
            Err(problem) => {
                if problem.contains("insufficient_user_quota") || problem.contains("quota") {
                    return Err(problem);
                }
                let shape_miss = ["HTTP 404", "HTTP 403", "HTTP 405"]
                    .iter()
                    .any(|marker| problem.contains(marker));
                if shape_miss {
                    attempts.push(format!("{create_path} → {problem}"));
                    continue;
                }
                return Err(problem);
            }
        };
        // 业务码错误（HTTP 200 但 body 里 code 是非 200 的数字，sunoapi 系的
        // 错误形如 {code:400, msg:"..."}）：原样透传，不要当"没音频没任务"换家族
        let business_code = created["code"]
            .as_i64()
            .filter(|code| *code != 200)
            .or_else(|| {
                created["base_resp"]["status_code"]
                    .as_i64()
                    .filter(|code| *code != 0)
            });
        if let Some(code) = business_code {
            let reason = created["msg"]
                .as_str()
                .or_else(|| created["message"].as_str())
                .or_else(|| created["base_resp"]["status_msg"].as_str())
                .unwrap_or("未知原因");
            return Err(format!("上游业务错误（{code}）：{reason}"));
        }
        // 同步端点（Mureka 常见）：响应里直接带音频地址
        if let Some(url) = extract_media_url(&created) {
            let bytes = download(&url)?;
            return save_generated(app, &bytes, "mp3");
        }
        // MiniMax 缺省 output_format=hex：data.audio 是 hex 编码的音频本体
        if style == "minimax" {
            if let Some(encoded) = created["data"]["audio"].as_str() {
                if let Some(bytes) = hex_decode(encoded) {
                    return save_generated(app, &bytes, "mp3");
                }
            }
        }
        // 异步端点：任务 id 再轮询（查询路径跟随家族；dmx 为同步，没有音频直接报）
        let Some(task_id) = extract_task_id(&created).filter(|_| !query_path.is_empty()) else {
            attempts.push(format!(
                "{create_path} → 没有回音频地址也没有回任务 id：{}",
                serde_json::to_string(&created).unwrap_or_default().chars().take(200).collect::<String>()
            ));
            continue;
        };
        // 轮询地址按家族拼：sunoapi 是 query string 形（?taskId= 直接续 id），
        // 别家是路径拼接（{query_path}/{task_id}）
        let query_url = if style == "sunoapi" {
            format!("{base}{query_path}{task_id}")
        } else {
            format!("{base}{query_path}/{task_id}")
        };
        // 阶跃/RunningHub 的查询是 POST 带 id 体（字段名还不一样：task_id/taskId），
        // 其余家族 GET
        let poll_body = match style {
            "stepfun" => Some(json!({ "task_id": task_id })),
            "runninghub" => Some(json!({ "taskId": task_id })),
            _ => None,
        };
        let done = poll_video_task(config, &query_url, poll_body)?;
        return match extract_media_url(&done) {
            Some(url) => {
                let bytes = download(&url)?;
                save_generated(app, &bytes, "mp3")
            }
            None => {
                // 阶跃的产物不是地址，是 audio 字段里的 base64 音频本体（mp3）
                if style == "stepfun" {
                    if let Some(encoded) = done["audio"].as_str() {
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(encoded)
                            .map_err(|error| format!("音频 base64 解码失败：{error}"))?;
                        return save_generated(app, &bytes, "mp3");
                    }
                }
                Err(format!(
                "音乐任务完成但没有回音频地址：{}",
                serde_json::to_string(&done).unwrap_or_default().chars().take(200).collect::<String>()
            ))
            }
        };
    }
    Err(format!(
        "音乐生成失败——所有已知的音乐接口路径都不可用：
{}",
        attempts.join("
")
    ))
}

/// 语音转写（OpenAI 兼容族 /audio/transcriptions，Whisper 系）：
/// multipart 上传音频文件，回 `{text}`。模型行走 kindModels.transcribe
/// （前端传，缺省 whisper-1）；语言不指定，随上游默认
fn generate_transcription(
    config: &AppConfig,
    model: &str,
    audio_path: &str,
) -> Result<Value, String> {
    use ureq::unversioned::multipart::{Form, Part};

    let base = base_of(config)?;
    let url = format!("{base}/audio/transcriptions");
    crate::egress::guard(&config.net_egress_allow, &url)?;
    let auth = crate::config::api_key(config)
        .map(|key| format!("Bearer {key}"))
        .unwrap_or_default();

    let metadata = std::fs::metadata(audio_path).map_err(|e| format!("无法访问 {audio_path}: {e}"))?;
    if metadata.len() > AUDIO_MATERIAL_MAX_BYTES {
        return Err(format!(
            "音频超过 {} MB，先压一压再转写。",
            AUDIO_MATERIAL_MAX_BYTES / 1024 / 1024
        ));
    }
    let mut buffer = Vec::new();
    std::fs::File::open(audio_path)
        .and_then(|mut handle| handle.read_to_end(&mut buffer))
        .map_err(|e| format!("读取失败: {e}"))?;
    let file_name = Path::new(audio_path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("audio.mp3")
        .to_string();
    let mime = material_mime(audio_path).to_string();
    let model_text = model.to_string();

    let form = Form::new()
        .text("model", &model_text)
        .part(
            "file",
            Part::owned_reader(std::io::Cursor::new(buffer))
                .file_name(&file_name)
                .mime_str(&mime)
                .map_err(|e| format!("音频的 multipart 头组装失败：{e}"))?,
        );
    let mut response = agent_with_timeout()
        .post(&url)
        .header("accept", "application/json")
        .header("authorization", &auth)
        .send(form)
        .map_err(|error| format!("转写请求失败：{error}"))?;
    let status = response.status().as_u16();
    let mut body = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(8 * 1024 * 1024)
        .read_to_end(&mut body)
        .map_err(|e| format!("读取转写响应失败：{e}"))?;
    if status >= 400 {
        let snippet = String::from_utf8_lossy(&body[..body.len().min(300)]).trim().to_string();
        return Err(if snippet.is_empty() {
            format!("转写接口返回 HTTP {status}")
        } else {
            format!("转写接口返回 HTTP {status}：{snippet}")
        });
    }
    let value: Value =
        serde_json::from_slice(&body).map_err(|e| format!("转写响应不是合法 JSON：{e}"))?;
    let text = value["text"]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "转写接口没有回文字：{}",
                serde_json::to_string(&value).unwrap_or_default().chars().take(200).collect::<String>()
            )
        })?;
    Ok(json!({ "text": text }))
}

/// 能力会话的生成入口：kind 由前端传（会话开档即定），prompt 是用户输入。
/// 返回 `{path, name, bytes}`——前端把它挂成一条带附件的助手气泡并持久化
#[tauri::command]
pub async fn media_generate(
    app: tauri::AppHandle,
    kind: String,
    prompt: String,
    options: Option<Value>,
    reference_images: Option<Vec<String>>,
    model: Option<String>,
    video_reference: Option<String>,
    profile_id: Option<String>,
) -> Result<Value, String> {
    let prompt = prompt.trim().to_string();
    // 转写不需要提示词：素材就是附件里的音频
    if prompt.is_empty() && kind != "transcribe" {
        return Err("提示词是空的".into());
    }
    let mut config = crate::config::load(&app);
    // 每类模型可能住在不同的服务商（对话模型在 A 站、生图在 B 站）：
    // 前端按模型名解析出所属档案后点名，这里把连接域整体换过去
    // （与池成员路由同一套抄写）——不路由的话文字请求会打到图片站上（真机踩过）
    if let Some(profile_id) = profile_id.filter(|id| !id.trim().is_empty()) {
        let profile = config
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .cloned()
            .ok_or_else(|| format!("模型所在的档案已经不存在了（{profile_id}）。去「设置 → 服务商档案」重新选一次模型。"))?;
        crate::config::apply_profile_connection(&mut config, &profile);
    }
    let options = options.unwrap_or_else(|| json!({}));
    let references = reference_images.unwrap_or_default();
    // 每类生成用各自的模型行（kindModels）：视频画布的四类页签各选各的，
    // 不点名时才落回当前连接的 model
    let model = model
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| config.model.clone());
    match kind.as_str() {
        "transcribe" => {
            let Some(audio_path) = references.first() else {
                return Err("转写需要先挂一段音频（+ 菜单选择音频文件）。".into());
            };
            generate_transcription(&config, &model, audio_path)
        }
        "text" => generate_text(&config, &model, &prompt),
        "image" => generate_image(&app, &config, &model, &prompt, &options, &references),
        "video" => generate_video(
            &app,
            &config,
            &model,
            &prompt,
            &options,
            &references,
            video_reference.as_deref(),
        ),
        "audio" => generate_audio(&app, &config, &model, &prompt),
        "music" => generate_music(&app, &config, &model, &prompt, &options),
        _ => Err(format!("未知的能力档：{kind}（只认 text / image / video / audio）")),
    }
}
