//! 屏幕顶部的胶囊悬浮岛：电脑控制（computer_act）执行期间浮现。
//! 模型在动你鼠标键盘时，这个警示必须**在应用之外**也看得见——主窗口可能被
//! 别的应用盖住或最小化。窗口特性：无边框透明、置顶、跳过任务栏、不抢焦点、
//! **鼠标穿透**——它只是块警示牌，绝不能挡住被控制的那个应用。
//!
//! 显隐节奏：每个控制动作都 poke 一次（连发不闪），最后一次动作之后
//! 闲置 4 秒自动收起——工具层不知道"回合"边界，闲置超时是自包含的收法。
//!
//! **测试编译不含此模块的实现**：岛的窗口链会拉起 muda/rfd 的
//! TaskDialogIndirect（comctl32 v6-only 入口），而测试二进制没有 v6 清单
//! （tauri-build 只给主程序嵌），加载即 STATUS_ENTRYPOINT_NOT_FOUND。
//! 测试也不覆盖这块 UI——stub 即诚实。
//!
//! 建窗的约束备忘：不要在 build.rs 给全部链接目标嵌 MANIFEST（会与
//! tauri-build 的主程序清单撞成 CVT1100 资源重复，cargo 1.98 也没有
//! tests 作用域的 rustc-link-arg 变体）。

#[cfg(not(test))]
mod imp {
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;
    use tauri::{AppHandle, Emitter, LogicalPosition, Manager, WebviewUrl, WebviewWindowBuilder};

    const WINDOW: &str = "control-island";
    const WIDTH: f64 = 320.0;
    const HEIGHT: f64 = 48.0;
    const IDLE_HIDE_MS: u64 = 4000;

    /// setup 时挂一次：工具线程（run_turn 的裸线程）从这份句柄建窗、发事件
    static HANDLE: std::sync::OnceLock<AppHandle> = std::sync::OnceLock::new();
    /// poke 代次：只有最后一次 poke 的收起定时器有资格真收，动作连发不闪
    static GENERATION: AtomicU64 = AtomicU64::new(0);

    pub fn install(app: &AppHandle) {
        let _ = HANDLE.set(app.clone());
    }

    /// 控制动作发生时调用。窗口的创建/显隐/落位都在主线程做（Tauri 的规矩），
    /// 工具线程只管摇人
    pub fn poke(label: impl Into<String>) {
        let Some(app) = HANDLE.get() else { return };
        let app = app.clone();
        let label = label.into();
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        let app_for_main = app.clone();
        let _ = app.run_on_main_thread(move || {
            let Some(window) = ensure_window(&app_for_main) else { return };
            position_top_center(&app_for_main, &window);
            let _ = window.show();
            let _ = app_for_main.emit("control-island", json!({ "show": true, "label": label }));
        });
        let handle = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(IDLE_HIDE_MS));
            if GENERATION.load(Ordering::Relaxed) == generation {
                if let Some(window) = handle.get_webview_window(WINDOW) {
                    let _ = window.hide();
                }
                let _ = handle.emit("control-island", json!({ "show": false }));
            }
        });
    }

    /// 惰性建窗：第一次控制动作才创建，藏在后台的应用不白背这扇窗
    fn ensure_window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
        if let Some(window) = app.get_webview_window(WINDOW) {
            return Some(window);
        }
        let window = WebviewWindowBuilder::new(app, WINDOW, WebviewUrl::App("island.html".into()))
            .title("aglab 控制指示")
            .decorations(false)
            .transparent(true)
            .always_on_top(true)
            .skip_taskbar(true)
            .resizable(false)
            .maximizable(false)
            .minimizable(false)
            .shadow(false)
            .focused(false)
            .visible(false)
            .inner_size(WIDTH, HEIGHT)
            .build()
            .ok()?;
        // 鼠标穿透：警示牌绝不拦被控制应用的点击（顶栏正中是最常被点到的地方）
        let _ = window.set_ignore_cursor_events(true);
        Some(window)
    }

    /// 主屏顶部居中。monitor 的 size 是物理像素，窗口坐标用逻辑像素——除以缩放
    fn position_top_center(app: &AppHandle, window: &tauri::WebviewWindow) {
        let Ok(Some(monitor)) = app.primary_monitor() else { return };
        let scale = monitor.scale_factor();
        let width = monitor.size().width as f64 / scale;
        let _ = window.set_position(LogicalPosition::new((width - WIDTH) / 2.0, 10.0));
    }
}

#[cfg(test)]
mod imp {
    /// 测试编译的诚实空壳：岛是 UI 功能，测试不覆盖它；
    /// 真实现会拉起 comctl32 v6-only 的链接面，见模块级注释
    pub fn install(_app: &tauri::AppHandle) {}
    pub fn poke(_label: impl Into<String>) {}
}

pub use imp::{install, poke};
