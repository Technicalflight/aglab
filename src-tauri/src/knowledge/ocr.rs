//! Umi-OCR 引擎集成：资料库导入 PDF/图片时的文字提取。
//!
//! Umi-OCR（github.com/hiroi-sora/Umi-OCR）是本机 OCR 桌面应用，HTTP 服务
//! 默认开在 127.0.0.1:1224（仅本地环回）。三条路都走它：
//! - 图片 → `POST /api/ocr`（base64，`data.format=text` 直接回纯文本）；
//! - PDF/扫描件 → `POST /api/doc/upload` 三步任务流（上传拿 id → 轮询
//!   `/api/doc/result` 到 `is_done` → `/api/doc/clear` 清理）；
//! - 引擎管理：设置页可一键下载官方 Paddle 整合包（GitHub release 的 7z SFX，
//!   先 gh-proxy 镜像再直连），签名定位 7z 流解压到应用数据目录，`--hide` 后台启动。

use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::config::OcrConfig;

/// 引擎包体积不小（Paddle 整合包 134MB），同一时刻只允许一个下载任务
static DOWNLOADING: AtomicBool = AtomicBool::new(false);

/// 留空的 base_url 落到 Umi-OCR 的出厂地址
pub fn base_url(config: &OcrConfig) -> String {
    let raw = config.base_url.trim().trim_end_matches('/');
    if raw.is_empty() {
        "http://127.0.0.1:1224".into()
    } else {
        raw.to_string()
    }
}

/// 用户配置的可信端点：只放行 http/https（本机环回是它的默认形态，与 SearXNG 同先例）
fn checked_base(config: &OcrConfig) -> Result<String, String> {
    let base = base_url(config);
    if base.starts_with("http://") || base.starts_with("https://") {
        Ok(base)
    } else {
        Err(format!("OCR 服务地址不合法：{base}（要 http/https 开头）。"))
    }
}

fn agent() -> Result<ureq::Agent, String> {
    crate::proxy::agent_for(None)
}

fn read_body(reader: impl Read) -> Result<String, String> {
    let mut text = String::new();
    let mut reader = reader;
    Read::read_to_string(&mut reader, &mut text).map_err(|e| format!("{e}"))?;
    Ok(text)
}

/// 探活：GET /api/ocr/get_version → {code, data:"Umi-OCR v2.1.5"}
pub fn probe(config: &OcrConfig) -> Result<String, String> {
    let url = format!("{}/api/ocr/get_version", checked_base(config)?);
    let request = crate::chat::with_timeouts(agent()?.get(&url), Duration::from_secs(5));
    let response = request.call().map_err(|e| format!("连不上 OCR 服务：{e}"))?;
    let parsed: Value = serde_json::from_str(&read_body(response.into_body().into_reader())?).map_err(|e| format!("{e}"))?;
    if parsed["code"].as_i64() != Some(100) {
        return Err(format!(
            "OCR 服务应答异常：{}",
            parsed["data"].as_str().unwrap_or("未知原因")
        ));
    }
    Ok(parsed["data"].as_str().unwrap_or("Umi-OCR").to_string())
}

/// 图片 OCR：base64 进、纯文本出。code 101 = 图里没字，给空串不算错
pub fn ocr_image(config: &OcrConfig, image: &[u8]) -> Result<String, String> {
    use base64::Engine as _;
    let url = format!("{}/api/ocr", checked_base(config)?);
    let body = json!({
        "base64": base64::engine::general_purpose::STANDARD.encode(image),
        "options": { "data.format": "text" },
    });
    let request = crate::chat::with_timeouts(agent()?.post(&url), Duration::from_secs(120));
    let response = request.send_json(body).map_err(|e| format!("OCR 请求失败：{e}"))?;
    let parsed: Value = serde_json::from_str(&read_body(response.into_body().into_reader())?).map_err(|e| format!("{e}"))?;
    ocr_result_text(&parsed).map(str::to_string)
}

/// /api/ocr 响应 → 文本。100 成功、101 无文本，其余按失败并把原因带出来
fn ocr_result_text(parsed: &Value) -> Result<&str, String> {
    match parsed["code"].as_i64() {
        Some(100) => Ok(parsed["data"].as_str().unwrap_or("")),
        Some(101) => Ok(""),
        _ => Err(format!(
            "OCR 识别失败：{}",
            parsed["data"].as_str().unwrap_or("未知原因")
        )),
    }
}

/// PDF/文档 OCR：上传 → 轮询 → 清理。识别中文档以 mixed 模式提取
/// （有原生文本的页直接拷贝，扫描页走 OCR）
pub fn ocr_document(config: &OcrConfig, file_name: &str, bytes: &[u8]) -> Result<String, String> {
    let base = checked_base(config)?;
    let task_id = doc_upload(&base, file_name, bytes)?;
    let result = doc_poll(&base, &task_id);
    // 任务结束（无论成败）都清服务器上的临时文件，失败也不影响主结果
    let clear_url = format!("{base}/api/doc/clear/{task_id}");
    let request = crate::chat::with_timeouts(agent()?.get(&clear_url), Duration::from_secs(10));
    let _ = request.call();
    result
}

/// multipart/form-data 手工拼装：file（二进制）+ json（参数）
fn doc_upload(base: &str, file_name: &str, bytes: &[u8]) -> Result<String, String> {
    let boundary = "aglab-umi-ocr-boundary-8f3c1d";
    let mut body: Vec<u8> = Vec::with_capacity(bytes.len() + 512);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"json\"\r\n\r\n",
    );
    body.extend_from_slice(
        json!({ "doc.extractionMode": "mixed", "pageRangeStart": 1, "pageRangeEnd": -1 })
            .to_string()
            .as_bytes(),
    );
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let url = format!("{base}/api/doc/upload");
    let request = crate::chat::with_timeouts(agent()?.post(&url), Duration::from_secs(300))
        .header("content-type", format!("multipart/form-data; boundary={boundary}"));
    let response = request.send(body.as_slice()).map_err(|e| format!("上传文档失败：{e}"))?;
    let parsed: Value = serde_json::from_str(&read_body(response.into_body().into_reader())?).map_err(|e| format!("{e}"))?;
    if parsed["code"].as_i64() != Some(100) {
        return Err(format!(
            "上传文档失败：{}",
            parsed["data"].as_str().unwrap_or("未知原因")
        ));
    }
    Ok(parsed["data"].as_str().unwrap_or_default().to_string())
}

/// 轮询到任务结束。format=text 让 data 直接是全文；300 秒封顶
fn doc_poll(base: &str, task_id: &str) -> Result<String, String> {
    let url = format!("{base}/api/doc/result");
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        let request = crate::chat::with_timeouts(agent()?.post(&url), Duration::from_secs(30));
        let response = request
            .send_json(json!({ "id": task_id, "is_data": true, "format": "text" }))
            .map_err(|e| format!("查询识别进度失败：{e}"))?;
        let parsed: Value = serde_json::from_str(&read_body(response.into_body().into_reader())?)
            .map_err(|e| format!("{e}"))?;
        if parsed["code"].as_i64() != Some(100) {
            return Err(format!(
                "查询识别进度失败：{}",
                parsed["data"].as_str().unwrap_or("未知原因")
            ));
        }
        if parsed["is_done"].as_bool() == Some(true) && parsed["state"].as_str() == Some("failure") {
            return Err(format!(
                "文档识别失败：{}",
                parsed["message"].as_str().unwrap_or("未知原因")
            ));
        }
        if parsed["is_done"].as_bool() == Some(true) {
            return Ok(parsed["data"].as_str().unwrap_or_default().to_string());
        }
        if std::time::Instant::now() > deadline {
            return Err("文档识别超时（5 分钟还没跑完），试试只导入部分页。".into());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

// ---- 引擎管理：装在哪、跑没跑、下载、启动 ----

fn engine_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    use tauri::Manager;
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("umi-ocr"))
        .map_err(|e| format!("{e}"))
}

fn engine_exe(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(engine_dir(app)?.join("Umi-OCR.exe"))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStatus {
    pub installed: bool,
    pub running: bool,
    /// 探到的版本串，没跑起来就是空
    pub version: String,
    pub downloading: bool,
    pub engine_path: String,
}

// 命令包装住 knowledge/mod.rs（项目惯例：命令不进子模块，tauri 宏路径省心）
pub fn ocr_engine_status(app: tauri::AppHandle) -> Result<EngineStatus, String> {
    let config = crate::config::load(&app);
    let exe = engine_exe(&app)?;
    let installed = exe.exists();
    let (running, version) = if installed {
        match probe(&config.ocr) {
            Ok(version) => (true, version),
            Err(_) => (false, String::new()),
        }
    } else {
        (false, String::new())
    };
    Ok(EngineStatus {
        installed,
        running,
        version,
        downloading: DOWNLOADING.load(Ordering::Relaxed),
        engine_path: exe.to_string_lossy().into_owned(),
    })
}

/// 启动内置引擎。程序名按纪律走字面量（白名单式），引擎目录通过 PATH 注入
/// 进解析路径——效果等价于绝对路径启动，但命令面没有变量拼接
pub fn ocr_engine_start(app: tauri::AppHandle) -> Result<(), String> {
    let dir = engine_dir(&app)?;
    let exe = dir.join("Umi-OCR.exe");
    if !exe.exists() {
        return Err("引擎还没下载。先点「下载内置引擎」。".into());
    }
    let path_env = match std::env::var("PATH") {
        Ok(existing) => format!("{};{existing}", dir.display()),
        Err(_) => dir.display().to_string(),
    };
    // 构建器方法返回 &mut Command，一句一句叠；hide() 按值收走并叠好窗口旗标
    let mut command = std::process::Command::new("Umi-OCR.exe");
    command.arg("--hide");
    command.current_dir(&dir);
    command.env("PATH", path_env);
    crate::childproc::hide(command)
        .spawn()
        .map_err(|e| format!("启动 Umi-OCR 失败：{e}"))?;
    Ok(())
}

/// 官方发布资产的 gh-proxy 镜像（用户的 hosts 常屏蔽 GitHub，镜像不需要代理）
const PINNED_ASSET: &str =
    "https://gh-proxy.org/https://github.com/hiroi-sora/Umi-OCR/releases/download/v2.1.5/Umi-OCR_Paddle_v2.1.5.7z.exe";

/// 7z 文件签名：SFX 的 7z 流从这串字节开始
const SEVEN_Z_SIGNATURE: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];

pub async fn ocr_engine_download(app: tauri::AppHandle) -> Result<(), String> {
    if DOWNLOADING.swap(true, Ordering::Relaxed) {
        return Err("已经在下载中了。".into());
    }
    let result = tauri::async_runtime::spawn_blocking(move || download_and_install(&app))
        .await
        .unwrap_or_else(|e| Err(format!("下载任务中断：{e}")));
    DOWNLOADING.store(false, Ordering::Relaxed);
    result
}

fn download_and_install(app: &tauri::AppHandle) -> Result<(), String> {
    let dir = engine_dir(app)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{e}"))?;
    for url in resolve_asset_url()? {
        match download(&url) {
            Ok(bytes) => {
                extract_sfx(&bytes, &dir)?;
                if engine_exe(app)?.exists() {
                    let _ = std::fs::remove_file(dir.join("download.part"));
                    return Ok(());
                }
                return Err("解压完成但没找到 Umi-OCR.exe，引擎包结构可能变了。".into());
            }
            Err(error) => {
                eprintln!("引擎下载源失败（{url}）：{error}");
                continue;
            }
        }
    }
    Err("两个下载源都没拉到引擎包：需要代理池里有可用代理，或网络能直连 GitHub。".into())
}

/// 先问 GitHub API 拿最新 Paddle 包地址（拿不到就用钉死的 v2.1.5 镜像），
/// 下载按「gh-proxy 镜像 → 直连」的顺序试
fn resolve_asset_url() -> Result<Vec<String>, String> {
    let mut asset: Option<String> = None;
    let api = "https://api.github.com/repos/hiroi-sora/Umi-OCR/releases/latest";
    if let Ok(request) = crate::chat::with_timeouts(agent()?.get(api), Duration::from_secs(20))
        .header("accept", "application/vnd.github+json")
        .header("user-agent", "aglab")
        .call()
    {
        if let Ok(text) = read_body(request.into_body().into_reader()) {
            if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                asset = parsed["assets"].as_array().and_then(|items| {
                    items
                        .iter()
                        .find(|asset| {
                            asset["name"].as_str().is_some_and(|name| {
                                name.starts_with("Umi-OCR_Paddle") && name.ends_with(".exe")
                            })
                        })
                        .and_then(|asset| asset["browser_download_url"].as_str().map(String::from))
                });
            }
        }
    }
    // API 摸不到（没代理且 hosts 屏蔽）就退回钉死的版本镜像——旧版本也能用
    let direct = match asset {
        Some(url) => url,
        None => return Ok(vec![PINNED_ASSET.to_string()]),
    };
    let mirror = format!(
        "https://gh-proxy.org/https://github.com/hiroi-sora/Umi-OCR/releases/download/{}",
        direct.rsplit('/').next().unwrap_or_default()
    );
    Ok(vec![mirror, direct])
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    if !url.starts_with("https://") {
        return Err(format!("下载地址不合法：{url}"));
    }
    let request = crate::chat::with_timeouts(agent()?.get(url), Duration::from_secs(1800));
    let response = request.call().map_err(|e| format!("{url}：{e}"))?;
    if response.status().as_u16() != 200 {
        return Err(format!("{url}：HTTP {}", response.status().as_u16()));
    }
    let mut bytes = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(600 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("下载中断：{e}"))?;
    Ok(bytes)
}

/// SFX（7z 自解压 exe）= PE 头 + 附加的 7z 流。定位签名、截出 7z、解到目标目录
fn extract_sfx(bytes: &[u8], dest: &std::path::Path) -> Result<(), String> {
    let offset = bytes
        .windows(SEVEN_Z_SIGNATURE.len())
        .position(|window| window == SEVEN_Z_SIGNATURE)
        .ok_or("这不是 7z 自解压包（没找到 7z 签名）。")?;
    let payload = dest.join("download.part");
    std::fs::write(&payload, &bytes[offset..]).map_err(|e| format!("{e}"))?;
    let result = sevenz_rust::decompress_file(&payload, dest).map_err(|e| format!("解压引擎包失败：{e}"));
    let _ = std::fs::remove_file(&payload);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_base_falls_back_to_factory_address() {
        assert_eq!(base_url(&OcrConfig::default()), "http://127.0.0.1:1224");
        let custom = OcrConfig { base_url: "http://127.0.0.1:9999/".into() };
        assert_eq!(base_url(&custom), "http://127.0.0.1:9999");
    }

    #[test]
    fn ocr_result_codes_map_to_text_empty_and_error() {
        let ok = json!({"code": 100, "data": "第一行\n第二行"});
        assert_eq!(ocr_result_text(&ok).unwrap(), "第一行\n第二行");
        let empty = json!({"code": 101, "data": "未识别出文本"});
        assert_eq!(ocr_result_text(&empty).unwrap(), "");
        let bad = json!({"code": 902, "data": "疑似子进程已崩溃"});
        assert!(ocr_result_text(&bad).is_err());
    }

    #[test]
    fn sfx_payload_is_located_by_signature() {
        let scoped = crate::test_support::scoped_temp_dir("umi-sfx");
        let mut bytes = vec![0x4D, 0x5A, 0x90, 0x00, 0x00, 0x00];
        bytes.extend_from_slice(b"fake pe stub padding");
        bytes.extend_from_slice(&SEVEN_Z_SIGNATURE);
        bytes.extend_from_slice(b"not a real archive");
        // 伪 7z 解压必失败，但失败点必须是"解压错误"而不是"没找到签名"——
        // 签名定位错了，后面的引擎安装就全错位
        assert!(extract_sfx(&bytes, &scoped.path).is_err());
    }

    #[test]
    fn download_sources_are_https_only() {
        let pinned = PINNED_ASSET;
        assert!(pinned.starts_with("https://"));
        if let Ok(sources) = resolve_asset_url() {
            for url in sources {
                assert!(url.starts_with("https://"), "{url}");
            }
        }
    }
}
