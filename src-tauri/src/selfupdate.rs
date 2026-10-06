//! 应用自更新。检查与下载都走**后端 + 应用代理池**：前端直连 api.github.com 会被
//! 用户侧的 hosts/防火墙挡掉（真机踩过），而代理池是这一份配置里已经被信任的出网路径。
//! 签名校验、NSIS 静默安装与装后重启交给 tauri-plugin-updater——自己手搓等于把
//! "更新包能不能被信任"这道题做错。
//!
//! 更新清单住在官网 Pages（`technicalflight.github.io/aglab-site/updater/latest.json`）：
//! 发版时由带签名的构建产出 `.sig`，连同安装包一起发布，latest.json 引用它的下载地址。

use serde::Serialize;
use tauri::{AppHandle, Url, ipc::Channel};
use tauri_plugin_updater::UpdaterExt;

/// 手动「检查更新」与 24 小时自动检查共用的读数
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub notes: Option<String>,
    pub has_update: bool,
}

/// 下载与安装的进度事件。camelCase 直接喂给前端弹窗里的进度条
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum UpdateProgress {
    Downloading { received: u64, total: Option<u64> },
    Installing,
    Done,
}

/// 更新器要试的代理候选：配置里启用的池成员在前（用户填了代理的机器就靠它们出网），
/// 直连垫底（没被墙的用户直连就够）。顺序即尝试顺序，一个成功就停
fn candidate_proxies(app: &AppHandle) -> Vec<Option<String>> {
    let config = crate::config::load(app);
    let mut candidates: Vec<Option<String>> = config
        .proxy_pool
        .proxies
        .iter()
        .filter(|entry| entry.enabled && !entry.url.trim().is_empty())
        .map(|entry| Some(entry.url.clone()))
        .collect();
    candidates.push(None);
    candidates
}

async fn check_once(
    app: &AppHandle,
    proxy: Option<&str>,
) -> Result<Option<tauri_plugin_updater::Update>, String> {
    let mut builder = app.updater_builder();
    if let Some(proxy_url) = proxy {
        let url = Url::parse(proxy_url).map_err(|e| format!("代理地址解析失败：{e}"))?;
        builder = builder.proxy(url);
    }
    builder
        .build()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())
}

/// 检查更新：逐个代理候选尝试，第一个成功者定音。
/// `has_update` 为 false 表示已是最新；Err 是最后一个候选的报错
#[tauri::command]
pub async fn update_check(app: AppHandle) -> Result<UpdateStatus, String> {
    let proxies = candidate_proxies(&app);
    let mut last_err = String::new();
    for proxy in &proxies {
        match check_once(&app, proxy.as_deref()).await {
            Ok(Some(update)) => {
                return Ok(UpdateStatus {
                    current_version: app.package_info().version.to_string(),
                    latest_version: Some(update.version.clone()),
                    notes: update.body.clone(),
                    has_update: true,
                });
            }
            // 清单上的版本不比当前新：已最新
            Ok(None) => {
                return Ok(UpdateStatus {
                    current_version: app.package_info().version.to_string(),
                    latest_version: None,
                    notes: None,
                    has_update: false,
                });
            }
            Err(error) => last_err = error,
        }
    }
    Err(last_err)
}

/// 下载并安装。进度经 `Channel` 推给前端弹窗；Windows 安装器跑完后
/// 由插件自动重启应用（`restart_after_install` 默认开）——进程退出即成功
#[tauri::command]
pub async fn update_install(
    app: AppHandle,
    on_event: Channel<UpdateProgress>,
) -> Result<(), String> {
    let proxies = candidate_proxies(&app);
    let mut last_err = String::new();
    for proxy in &proxies {
        let builder = {
            let mut builder = app.updater_builder();
            if let Some(proxy_url) = proxy {
                let url =
                    Url::parse(proxy_url).map_err(|e| format!("代理地址解析失败：{e}"))?;
                builder = builder.proxy(url);
            }
            builder
        };
        let update = match builder.build().map_err(|e| e.to_string()) {
            Ok(updater) => match updater.check().await {
                Ok(update) => update,
                Err(error) => {
                    last_err = error.to_string();
                    continue;
                }
            },
            Err(error) => {
                last_err = error;
                continue;
            }
        };
        let Some(update) = update else {
            // 清单上没有新版本：按钮不该走到这，但如实说
            return Err("当前已经是最新版本，没有可安装的更新。".into());
        };
        let channel = on_event.clone();
        update
            .download_and_install(
                |received, total| {
                    let _ = channel.send(UpdateProgress::Downloading { received: received as u64, total });
                },
                || {
                    let _ = on_event.send(UpdateProgress::Installing);
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        let _ = on_event.send(UpdateProgress::Done);
        return Ok(());
    }
    Err(last_err)
}
