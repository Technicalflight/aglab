//! 托盘，以及"关闭到底是什么意思"。
//!
//! 标题栏的关闭按钮、Alt+F4、任务栏右键关闭，最后都汇成同一条 `CloseRequested`。
//! 这里一律先拦下来、把问题抛给界面去问：OS 发起的关闭前端根本看不见，不拦就等于
//! 替用户选了"退出"——而他想留着的往往正是那条还在跑的回合。

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewWindow};

/// 界面收到这条事件就弹三选框：最小化到托盘 / 退出 / 当作没点
pub const CLOSE_ASK_EVENT: &str = "window-close-asked";

const TRAY_ID: &str = "main";
const SHOW_ID: &str = "tray-show";
const QUIT_ID: &str = "tray-quit";

/// 托盘到底建起来没有。它决定「最小化到托盘」能不能做：没有托盘还把窗口藏起来，
/// 用户手上就一个入口都不剩了——那比不让藏糟糕得多
static TRAY_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn ready() -> bool {
    TRAY_READY.load(std::sync::atomic::Ordering::Relaxed)
}

/// 从托盘里把窗口叫回来。隐藏与最小化是两种状态，两种都得能救回来
pub fn show(window: &WebviewWindow) {
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}

pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, SHOW_ID, "显示 aglab", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, QUIT_ID, "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("aglab")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            SHOW_ID => {
                if let Some(window) = app.get_webview_window("main") {
                    show(&window);
                }
            }
            // 从托盘菜单点退出是用户明说的意图，不再问第二遍
            QUIT_ID => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            else {
                return;
            };
            if let Some(window) = tray.app_handle().get_webview_window("main") {
                show(&window);
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    TRAY_READY.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}
