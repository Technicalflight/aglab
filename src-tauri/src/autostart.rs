//! 开机自启。用户自选：默认关，开关在设置页「常规」。
//!
//! 这件事的真相有两份——OS 里那条注册（插件的 `is_enabled`）与配置里用户的意图
//! （`config.autostart`）。启动时把后者套到前者上：配置被拷到另一台机器、或注册被
//! 系统清掉，都以意图为准重建。不然就会出现"界面上写着开、其实没注册"这种半真话，
//! 而它只在某次开机没起来的时候才暴露。

use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt as _;

/// 把意图套到 OS 注册上，返回套完之后系统里的实际状态
pub fn apply(app: &AppHandle, wanted: bool) -> Result<bool, String> {
    let launcher = app.autolaunch();
    let now = launcher.is_enabled().map_err(|e| e.to_string())?;
    if wanted && !now {
        launcher.enable().map_err(|e| e.to_string())?;
    }
    if !wanted && now {
        launcher.disable().map_err(|e| e.to_string())?;
    }
    launcher.is_enabled().map_err(|e| e.to_string())
}

/// 界面读的那一格：OS 里到底注册了没有。读注册而不是读配置，
/// 是因为"配置写着开"和"开机真的会起"是两件事，这一格只许说后者
#[tauri::command]
pub fn autostart_state(app: AppHandle) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

/// 开或关，并把意图写进配置，下次启动按它重建注册
#[tauri::command]
pub fn autostart_set(app: AppHandle, enabled: bool) -> Result<bool, String> {
    let actual = apply(&app, enabled)?;
    let mut config = crate::config::load(&app);
    config.autostart = enabled;
    crate::config::save(&app, &config)?;
    Ok(actual)
}
