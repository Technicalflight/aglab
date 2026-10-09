use tauri::{AppHandle, WebviewWindow};

// 最小化与关闭不在 core:window:default 权限集里，走自定义 command 免申请权限。
// 最大化反过来：JS 的 toggleMaximize() 调的是 toggle_maximize，而 default 集里只有
// internal_toggle_maximize（那个只服务拖拽区双击），所以它必须在 capability 里单独放行。
#[tauri::command]
pub fn window_minimize(window: WebviewWindow) {
    let _ = window.minimize();
}

/// 标题栏的"关闭"。它只负责发起一次关闭请求：拦不拦、问不问在 `run()` 的
/// `CloseRequested` 里，所以 Alt+F4 与这里走的是同一条路，不会有两套语义
#[tauri::command]
pub fn window_close(window: WebviewWindow) {
    let _ = window.close();
}

/// 最小化到托盘：窗口藏起来，进程、回合与后台任务都还在。回来走托盘图标或托盘菜单。
/// 托盘没建起来时**拒绝**这一发：藏进一个没有入口的地方，比不让藏糟糕得多
#[tauri::command]
pub fn window_hide_to_tray(window: WebviewWindow) -> Result<(), String> {
    if !crate::tray::ready() {
        return Err(
            "托盘没建起来，藏起来就没有回来的入口了。这一格先别点，用「关闭」退出。".to_string(),
        );
    }
    window.hide().map_err(|e| format!("窗口没藏进去：{e}"))
}

/// 真的退出。只有三选框里的"关闭"与托盘菜单的"退出"走这里，
/// 所以"退出"这个词在界面上出现几次，就有几条路真的会退
#[tauri::command]
pub fn window_quit(app: AppHandle) {
    app.exit(0);
}

/// 界面缩放。走 webview 的原生 zoom：文字与界面一起缩，不糊。
/// 夹在 0.5–2.0：再小看不清，再大版面自己都撑不下
#[tauri::command]
pub fn window_zoom(window: WebviewWindow, scale: f64) -> Result<(), String> {
    window
        .set_zoom(scale.clamp(0.5, 2.0))
        .map_err(|e| format!("缩放没设上：{e}"))
}

/// 窗口置顶。幂等：重复设同一个值没有副作用，所以改动与启动恢复都放心走这一条
#[tauri::command]
pub fn window_set_always_on_top(window: WebviewWindow, on_top: bool) -> Result<(), String> {
    window
        .set_always_on_top(on_top)
        .map_err(|e| format!("置顶没设上：{e}"))
}

/// 全局快捷键的键位。固定值、不做自定义：改键位是配置界面的二期，
/// 注册失败的可解释性比"什么都能填"重要
pub const GLOBAL_SHORTCUT: &str = "ctrl+shift+g";

/// 打开 WebView2 开发者控制台。debug 构建里 devtools 本就常开；release 的正式包
/// 刻意不带 `devtools` feature（它的作用恰恰是把调试面板强开进发布物），方法
/// 只在 debug 下存在——这一条在正式包里注册着但落地为空操作，注册表保持同形
#[tauri::command]
pub fn open_devtools(window: WebviewWindow) {
    #[cfg(debug_assertions)]
    window.open_devtools();
    #[cfg(not(debug_assertions))]
    let _ = window;
}

/// 全局唤起：任何应用在前台时把它喊回来。显示 + 取最小化 + 抢焦点三连，
/// 顺序不能倒——藏着的窗口先 show 才轮得到 focus
pub fn summon(app: &AppHandle) {
    use tauri::Manager;
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// 注册/注销全局快捷键。**这是改配置的唯一入口**：注册失败时不落盘——
/// "配置写着开而系统里没注册上"正是 autostart 那一格要防的两件事各说各话
#[tauri::command]
pub fn global_shortcut_set(app: AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;

    if enabled {
        app.global_shortcut()
            .register(GLOBAL_SHORTCUT)
            .map_err(|e| format!("快捷键 Ctrl+Shift+G 没注册上（可能被别的程序占了）：{e}"))?;
    } else {
        // 没注册过时注销会报错：关掉这格的语义是"确保不在"，错误吞掉不算失败
        let _ = app.global_shortcut().unregister(GLOBAL_SHORTCUT);
    }

    let mut config = crate::config::load(&app);
    config.global_shortcut_enabled = enabled;
    crate::config::save(&app, &config)
}

/// 启动时按配置恢复注册。失败只报一声：快捷键缺席影响的是便利，不是数据
pub fn restore_global_shortcut(app: &AppHandle) {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;

    if !crate::config::load(app).global_shortcut_enabled {
        return;
    }
    if let Err(error) = app.global_shortcut().register(GLOBAL_SHORTCUT) {
        eprintln!("全局快捷键这次没注册上（可能被别的程序占了）：{error}");
    }
}
