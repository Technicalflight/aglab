mod approvals;
mod audit;
mod backup;
mod browser;
mod builtins;
mod ccswitch;
mod chat;
mod command_rules;
mod computer;
mod config;
mod decision;
mod decision_bridge;
mod edits;
mod egress;
mod file_rules;
mod goal;
mod history;
mod hooks;
mod import;
mod knowledge;
mod lsp_host;
mod mcp;
mod memory;
mod mcp_oauth;
mod media;
mod orchestra;
mod plugins;
mod oauth;
mod observations;
mod pool;
mod policy;
pub mod provider;
mod proxy;
mod quota;
mod review;
mod route;
mod search;
mod secrets;
pub mod session;
mod skills;
mod slash;
mod spawn;
mod tasks;
mod toast;
mod tools;
mod tool_runtime;
mod usage;
mod warm;
mod autostart;
mod envelope;
mod tray;
mod window;
mod worktree;

#[cfg(test)]
pub(crate) mod test_support {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// 测试用的假 HTTP 代理：只干一件事——把请求读干净，按设定回一段 SSE 或一个状态码。
    /// 两个 Windows 坑留在注释里，改它之前先读：
    /// ① ureq 3 即使目标是 `http://` 也走 CONNECT 隧道，所以要先答应建立隧道再在隧道里答，
    ///    否则客户端会把自家响应体当状态行解析（`http parse fail: invalid HTTP version`）；
    /// ② 关闭时若还有收到的数据没读走，Windows 发 RST 而不是 FIN，已经写出去的响应一起作废
    ///    （客户端 `os error 10053`）——所以应答完再读到超时才关。
    pub mod fake_http {
        /// 回一段正常 SSE（或指定状态码）的代理，返回它的地址 `http://127.0.0.1:端口`
        pub fn proxy(status: u16) -> String {
            proxy_mode(status, false)
        }

        /// `truncate` 为真时只吐出一个事件就断流——那是"头拿到了、正文没拿完"的那一类
        pub fn proxy_mode(status: u16, truncate: bool) -> String {
            proxy_moving(move |_| (status, truncate))
        }

        /// 第 n 发（从 0 数）回 `statuses[min(n, 最后一个)]` 的代理，正文永远是那段正常 SSE。
        /// 验"限流时无限重试"必须要它会**变**：一个恒定回 429 的假代理只能证明"这一发失败了"，
        /// 证不了"重试到第 k 发真的放行"，而那正是无限重试的全部意义
        pub fn proxy_sequence(statuses: Vec<u16>) -> String {
            proxy_moving(move |n| (statuses[n.min(statuses.len() - 1)], false))
        }

        /// 每一发自己决定回什么状态码、要不要掐流。`decide` 收到的是这一发的序号（从 0 数）
        pub fn proxy_moving(
            decide: impl Fn(usize) -> (u16, bool) + Send + 'static,
        ) -> String {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("假代理该绑得到一个本地端口");
            let addr = listener.local_addr().expect("假代理该报得出自己的地址");
            std::thread::spawn(move || {
                const BODY: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\ndata: [DONE]\n\n";
                const FIRST: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\n";
                let mut served = 0usize;
                for stream in listener.incoming().flatten() {
                    let mut stream = stream;
                    let (status, truncate) = decide(served);
                    served += 1;
                    let head = drain_request(&mut stream);
                    if head.starts_with("connect") {
                        if stream
                            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                            .and_then(|_| stream.flush())
                            .is_err()
                        {
                            continue;
                        }
                        drain_request(&mut stream); // 隧道里的内层请求
                    }
                    let body = if truncate { FIRST } else { BODY };
                    let head = if status == 200 {
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
                            BODY.len()
                        )
                    } else {
                        format!("HTTP/1.1 {status} Oops\r\ncontent-length: 0\r\n\r\n")
                    };
                    let written = stream
                        .write_all(head.as_bytes())
                        .and_then(|_| {
                            if status == 200 {
                                stream.write_all(body.as_bytes())
                            } else {
                                Ok(())
                            }
                        })
                        .and_then(|_| stream.flush());
                    if written.is_err() {
                        continue;
                    }
                    let mut byte = [0u8; 1];
                    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(300)));
                    while stream.read(&mut byte).unwrap_or(0) != 0 {}
                }
            });
            format!("http://{addr}")
        }

        /// 把头与其声明的正文读掉，返回请求头首行（小写，用来认 CONNECT）
        fn drain_request(stream: &mut std::net::TcpStream) -> String {
            use std::io::Read;
            let mut got: Vec<u8> = Vec::new();
            let mut byte = [0u8; 1];
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(1_500)));
            while !got.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap_or(0) == 0 {
                    return String::new();
                }
                got.push(byte[0]);
            }
            let text = String::from_utf8_lossy(&got).to_ascii_lowercase();
            let length = text
                .lines()
                .find_map(|line| line.trim().strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut left = length;
            while left > 0 {
                match stream.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => left -= 1,
                }
            }
            text.lines().next().unwrap_or_default().to_string()
        }
    }

    /// 每次调用都换一个全新目录，避免两轮测试互相看见对方的文件
    pub fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aglab-{label}-{nanos}"));
        fs::create_dir_all(&path).expect("建临时目录");
        path
    }

    /// Windows 上 git 会把对象文件标成只读，刚写下的文件也可能被索引服务短暂占用，
    /// 所以直接 remove_dir_all 会静默失败、每跑一次测试就在 TEMP 里留一个目录。
    pub fn remove_tree(path: &Path) {
        for attempt in 0..8 {
            clear_readonly(path);
            if fs::remove_dir_all(path).is_ok() || !path.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50 * (attempt + 1)));
        }
        eprintln!("临时目录没删掉：{}", path.display());
    }

    fn clear_readonly(path: &Path) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let child = entry.path();
            if let Ok(mut perms) = entry.metadata().map(|m| m.permissions()) {
                if perms.readonly() {
                    perms.set_readonly(false);
                    let _ = fs::set_permissions(&child, perms);
                }
            }
            if child.is_dir() {
                clear_readonly(&child);
            }
        }
    }

    /// 出作用域就自己清掉的临时目录。少了这一层，每跑一次测试就会在 TEMP 里留下一个
    /// **带内容**的目录——这套测试最早积累到上千个才被发现
    pub struct ScopedTempDir {
        pub path: PathBuf,
    }

    pub fn scoped_temp_dir(label: &str) -> ScopedTempDir {
        ScopedTempDir {
            path: temp_dir(label),
        }
    }

    impl std::ops::Deref for ScopedTempDir {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for ScopedTempDir {
        fn drop(&mut self) {
            remove_tree(&self.path);
        }
    }

    /// 前端某个 interface 的字段名列表。
    /// IPC 的键是 camelCase 字符串，改了 Rust 字段名而忘了改 TS 时，编译器和 tsc
    /// 都不会响——要等到界面上那一栏悄悄变成 undefined。所以拿 TS 原文当断言对象。
    pub fn ts_interface_fields(name: &str) -> Vec<String> {
        // 前端的形状分三个文件写：跨进程的钉在 types/chat.ts，编排面板自己的在
        // lib/orchestra.ts，而一批"命令 + 它的返回类型"住在一个模块里在
        // lib/chat-transport.ts（审计那一页就是这么写的）。逐个找过去——只查一个文件的时候，
        // 另一个文件里的类型加了字段、少字段都没人拦（PlanView 就真的少过两个字段而全绿）
        for text in [
            include_str!("../../src/types/chat.ts"),
            include_str!("../../src/lib/orchestra.ts"),
            include_str!("../../src/lib/chat-transport.ts"),
        ] {
            if let Some(fields) = interface_fields_in(text, name) {
                return fields;
            }
        }
        panic!("前端类型里找不到 {name}");
    }

    fn interface_fields_in(text: &str, name: &str) -> Option<Vec<String>> {
        let head = format!("export interface {name}");
        // 名字要整段对上：找 `McpServer` 时不能一头扎进 `McpServerView`，
        // 所以只认紧跟 `{` 或 `extends` 的那一处
        let rest = text
            .match_indices(&head)
            .map(|(offset, _)| &text[offset + head.len()..])
            .find(|rest| {
                let trimmed = rest.trim_start();
                trimmed.starts_with('{') || trimmed.starts_with("extends")
            })?;
        let brace = rest.find('{')?;
        // `extends A, B` 的字段要一起算进这张形状：`TaskView extends ScheduledTask` 就是在
        // 声明"投影带着定义"，而投影丢字段丢的正是定义那一半（graph 真丢过）。
        // 不比基类的话，这条断言只对自己那一半负责
        let mut fields: Vec<String> = rest[..brace]
            .trim()
            .strip_prefix("extends")
            .into_iter()
            .flat_map(|bases| bases.split(','))
            .filter_map(|base| {
                let base = base.trim();
                (!base.is_empty()).then(|| ts_interface_fields(base))
            })
            .flatten()
            .collect();

        let mut depth = 1usize;
        for line in rest[brace + 1..].lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.is_empty() {
                continue;
            }

            if depth == 1 {
                // 只取第一层的 `名字: 类型`，嵌套对象字面量的内部字段不算
                if let Some((key, _)) = trimmed.split_once(':') {
                    let key = key.trim().trim_end_matches('?').trim_end_matches(',');
                    if key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !key.is_empty()
                    {
                        fields.push(key.to_string());
                    }
                }
            }

            depth += trimmed.matches('{').count();
            depth = depth.saturating_sub(trimmed.matches('}').count());
            if depth == 0 {
                break;
            }
        }

        fields.sort();
        fields.dedup();
        Some(fields)
    }

    /// 把一个 serde 产物的顶层键名和 TS 接口逐字对齐
    pub fn assert_matches_ts(value: &serde_json::Value, name: &str) {
        let mut keys: Vec<String> = value
            .as_object()
            .unwrap_or_else(|| panic!("{name} 序列化出来必须是个对象"))
            .keys()
            .cloned()
            .collect();
        keys.sort();
        let fields = ts_interface_fields(name);
        assert_eq!(
            keys, fields,
            "{name} 的 IPC 字段和 src/types/chat.ts 对不上：多出的少了的都在上面这两个列表里"
        );
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // on_window_event 里要拿 app_handle 读配置：Window 的这个方法在 Manager trait 上
    use tauri::Manager as _;
    // 这台机器上的并发额度只有一份，编排器与定时任务共用。两边各建一个的话，
    // 面板上那个"全局 x/y"就只是"编排器的 x/y"——一句半真话比一句假话更难发现
    let slots = std::sync::Arc::new(quota::Quota::new(orchestra::orchestrator::DEFAULT_TOTAL_PARALLEL));
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        // 开机自启的注册由插件管，意图在配置里；两者在 setup 里对一遍（见 autostart.rs）
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        // 系统通知（Windows toast）：审批等待 / 定时任务收尾 / 目标停下三个落点，
        // 门控与"窗口在前台就不打扰"的判断住在 toast.rs
        .plugin(tauri_plugin_notification::init())
        // 全局唤起（Ctrl+Shift+G）：任何应用在前台时把窗口喊回来。
        // 注册与否由配置与 global_shortcut_set 管，这里只挂"按下了怎么办"
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        crate::window::summon(app);
                    }
                })
                .build(),
        )
        .manage(approvals::ApprovalHub::default())
        .manage(chat::StopHub::default())
        .manage(chat::PauseHub::default())
        .manage(chat::ModeHub::default())
        .manage(chat::GoalGuards::default())
        .manage(chat::SteeringHub::default())
        .manage(chat::FollowUpHub::default())
        .manage(mcp::Hub::default())
        .manage(warm::Hub::default())
        .manage(browser::Hub::default())
        .manage(decision::SidecarHub::default())
        .manage(std::sync::Arc::clone(&slots))
        .manage(tasks::Lanes::shared())
        .manage(orchestra::orchestrator::Hub::with_slots(slots))
        .invoke_handler(tauri::generate_handler![
            chat::chat_send,
            chat::enhance_prompt,
            chat::chat_abort,
            chat::chat_steer,
            chat::chat_follow_up,
            chat::conversation_fork,
            chat::compact_history,
            chat::context_breakdown,
            chat::generate_title,
            chat::terminal_exec,
            chat::list_models,
            chat::read_attachment,
            chat::builtin_subagents_list,
            chat::save_clipboard_image,
            chat::fetch_url_text,
            tool_runtime::background::background_commands_list,
            tools::builtin_tools_list,
            tools::files_suggest,
            slash::slash_commands_list,
            approvals::ask_user_respond,
            approvals::tool_allow_always,
            usage::usage_conversations,
            window::global_shortcut_set,
            window::open_devtools,
            tool_runtime::sandbox::sandbox_set,
            tool_runtime::sandbox::sandbox_set_roots,
            config::auto_review_set,
            config::delete_to_trash_set,
            config::secret_scan_set,
            config::secret_rules_list,
            audit::audit_export,
            audit::audit_clear,
            backup::backup_open_dir,
            config::user_mcp_enabled_set,
            media::media_generate,
            mcp_oauth::mcp_oauth_login,
            mcp_oauth::mcp_oauth_status,
            mcp_oauth::mcp_oauth_logout,
            search::session_search,
            search::session_search_status,
            search::session_search_rebuild,
            skills::skills_list,
            skills::skillhub_search,
            skills::skillhub_install,
            skills::github_skill_probe,
            skills::github_skill_install,
            plugins::plugins_list,
            mcp::mcp_list,
            mcp::mcp_connect,
            mcp::mcp_stop,
            mcp::mcp_refresh,
            mcp::registry_search,
            approvals::tool_decision,
            approvals::tool_allow_session,
            approvals::tool_rules,
            approvals::tool_rule_forget,
            approvals::permission_table,
            approvals::tool_rules_clear,
            orchestra::orchestrator::orchestra_start,
            orchestra::orchestrator::orchestra_plan_brief,
            orchestra::orchestrator::orchestra_plans,
            orchestra::orchestrator::orchestra_pause,
            orchestra::orchestrator::orchestra_resume,
            orchestra::orchestrator::orchestra_cancel,
            orchestra::orchestrator::orchestra_status,
            orchestra::orchestrator::orchestra_board,
            orchestra::orchestrator::orchestra_rerun_node,
            orchestra::orchestrator::orchestra_edit_edge,
            orchestra::orchestrator::orchestra_set_edge_kind,
            orchestra::orchestrator::orchestra_ledger,
            chat::context_inspect,
            chat::compact_layer,
            chat::context_undo_compaction,
            ccswitch::ccswitch_candidates,
            ccswitch::ccswitch_import_mcp,
            ccswitch::ccswitch_import_pricing,
            ccswitch::ccswitch_import_provider,
            ccswitch::ccswitch_import_skills,
            ccswitch::ccswitch_mcp_candidates,
            ccswitch::ccswitch_skill_candidates,
            browser::browser_clear_cache,
            browser::browser_clear_all,
            config::config_file_path,
            config::workspace_open,
            config::open_log_dir,
            memory::memory_add,
            memory::memory_list,
            memory::memory_search,
            memory::memory_recall_hints,
            memory::memory_timeline,
            memory::memory_forget,
            memory::memory_rebuild,
            memory::memory_stats,
            memory::memory_config_get,
            memory::memory_config_set,
            memory::memory_why,
            memory::memory_extract,
            memory::memory_reflect,
            memory::memory_distill,
            memory::memory_distill_preview,
            memory::memory_export_agents_md,
            memory::memory_edit,
            memory::memory_export,
            memory::memory_import,
            memory::memory_import_file,
            memory::memory_wipe,
            memory::memory_conflicts,
            proxy::proxy_test,
            proxy::proxy_pool_stats,
            proxy::proxy_pool_test_all,
            proxy::proxy_import,
            chat::conversation_tree,
            chat::session_mode_get,
            chat::goals_overview,
            chat::session_mode_set,
            chat::session_goal_set,
            chat::session_goal_edit,
            chat::goal_criteria_draft,
            chat::command_risk,
            chat::session_goal_pause,
            chat::session_goal_resume,
            chat::session_goal_discard,
            chat::conversation_navigate,
            memory::memory_conflict_resolve,
            memory::memory_source,
            audit::audit_rotate,
            audit::audit_view,
            tasks::tasks_runs_list,
            tasks::tasks_runs_purge,
            tasks::tasks_run_resume,
            tasks::tasks_pending_approvals,
            tasks::tasks_approval_decide,
            tasks::tasks_approval_history,
            tasks::tasks_approval_forget,
            config::config_get,
            config::config_patch,
            config::credential_probe,
            config::credential_set,
            oauth::oauth_device_poll,
            oauth::oauth_device_start,
            oauth::oauth_login,
            oauth::oauth_providers,
            config::profile_delete,
            config::profile_create,
            config::profile_rename,
            config::profile_switch,
            config::profile_update,
            config::project_add,
            config::project_select,
            config::project_remove,
            decision::decision_jev_system_one,
            decision::decision_jev_key_set,
            decision::decision_jev_key_state,
            decision::decision_jev_key_delete,
            decision_bridge::decision_bridge_answer,
            decision::decision_sidecar_start,
            decision::decision_sidecar_stop,
            history::history_list,
            history::history_load,
            history::history_save,
            history::history_remove,
            history::storage_info,
            history::storage_switch,
            import::import_from_app,
            import::import_scan,
            knowledge::kb_list,
            knowledge::kb_create,
            knowledge::kb_update,
            knowledge::kb_delete,
            knowledge::kb_get,
            knowledge::kb_doc_add,
            knowledge::kb_doc_update,
            knowledge::kb_doc_delete,
            knowledge::kb_doc_get,
            knowledge::kb_search,
            knowledge::kb_import_files,
            review::review_info,
            review::review_file_diff,
            review::review_draft,
            review::review_save,
            edits::edits_for_session,
            edits::edit_revert,
            edits::edit_preview,
            tasks::tasks_list,
            tasks::tasks_run,
            usage::pricing_list,
            usage::pricing_remove,
            usage::pricing_upsert,
            usage::usage_export_csv,
            usage::usage_recent,
            usage::usage_report,
            usage::usage_session_cache,
            pool::pool_catalog,
            pool::pool_stats,
            window::window_minimize,
            window::window_close,
            window::window_hide_to_tray,
            window::window_quit,
            window::window_zoom,
            window::window_set_always_on_top,
            autostart::autostart_state,
            autostart::autostart_set,
            worktree::worktree_attach,
            worktree::worktree_detach,
            worktree::worktree_status,
            worktree::worktree_branches
        ])
        // 关闭请求一律先拦下来：是问、是藏、是退，以此刻配置里用户选的为准（close_action）。
        // OS 发起的关闭前端看不见，不拦就等于替用户选了"退出"
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                match crate::config::load(window.app_handle()).close_action.as_str() {
                    // 托盘真建起来了才硬藏：藏进没有入口的地方，比多问一句糟糕得多。
                    // 托盘没就绪时退回"每次问"，三条路里它永远有得选
                    "tray" if crate::tray::ready() => {
                        let _ = window.hide();
                    }
                    "quit" => window.app_handle().exit(0),
                    _ => {
                        let _ = tauri::Emitter::emit(window, tray::CLOSE_ASK_EVENT, ());
                    }
                }
            }
        })
        .setup(|app| {
            // 主窗口程序化建（tauri.conf.json 的 windows 是空表）：代理是 WebView2
            // 环境创建时的一次性决定，配置窗口在建它时已经起好了，递不进去——
            // 所以建窗这一步得在 setup 里亲手来，参数串里坐着代理解析的结果。
            // 尺寸与观感照 tauri.conf.json 旧声明原样抄，别顺手"优化"掉哪一项
            {
                let prefs = config::load(app.handle());
                // 子进程（MCP/命令/钩子）的代理环境快照：spawn 时读这份，改配置即生效
                crate::proxy::on_config_changed(&prefs);
                crate::tools::set_command_shell(&prefs.command_shell);
                crate::tools::set_ssh_hosts(&prefs.ssh_hosts);
                crate::lsp_host::set_server_overrides(&prefs.lsp_servers);
                let mut window = tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::default())
                    .title("aglab")
                    .inner_size(1344.0, 800.0)
                    .min_inner_size(1044.0, 620.0)
                    .center()
                    .decorations(false)
                    .additional_browser_args(&crate::proxy::webview_browser_args(&prefs));
                if prefs.always_on_top {
                    window = window.always_on_top(true);
                }
                window.build()?;
            }
            // 托盘建不起来不该让整个应用起不来：少一个图标是缺憾，起不来是事故
            if let Err(error) = tray::build(app.handle()) {
                eprintln!("托盘没建起来：{error}");
            }
            // 开机自启：配置里是意图、OS 里是注册，启动时把前者套到后者上。
            // 套不上只报一声：它影响的是下次开机，不是这一次能不能用
            match autostart::apply(app.handle(), config::load(app.handle()).autostart) {
                Ok(_) => {}
                Err(error) => eprintln!("开机自启这次没套上：{error}"),
            }
            // 缩放与置顶是启动时的一次决定：按配置把窗口收拾好，别让用户看见
            // 先 100% 再跳档的那一下。前端挂载后还会再对一遍（useTheme），这里只管第一帧
            {
                use tauri::Manager as _;
                let prefs = config::load(app.handle());
                if let Some(window) = app.get_webview_window("main") {
                    if (prefs.ui_zoom - 1.0).abs() > f64::EPSILON {
                        let _ = window.set_zoom(prefs.ui_zoom);
                    }
                    if prefs.always_on_top {
                        let _ = window.set_always_on_top(true);
                    }
                }
            }
            let handle = app.handle().clone();
            std::thread::spawn(move || tasks::watch(handle));
            // 资料库的工具执行体没有 AppHandle：根目录在启动时定死在这里
            knowledge::init_root(app.handle());
            // inbound webhook：默认关，所以这一行在没开的配置上只是立刻返回，
            // 不多出一个 socket。开了要重启才听——它是启动时读的一次决定
            let listen = app.handle().clone();
            std::thread::spawn(move || tasks::inbound::serve(&listen));
            {
                use tauri::Manager;
                // 审计保留策略在启动时跑一次：这是唯一会移动审计文件的操作，
                // 趁着还没有线程往里追加的时候做最省事。搬不动就报一声，不静默——
                // "以为归档了、其实没有"和"没有归档"不一样
                let keep = config::load(app.handle()).audit_keep_days;
                match app
                    .path()
                    .app_data_dir()
                    .map_err(|e| e.to_string())
                    .and_then(|root| audit::rotate(&root, keep))
                {
                    Ok(moved) if !moved.is_empty() => {
                        eprintln!("审计按 {keep} 天归档：{} 个旧分片搬进 audit/archive/", moved.len());
                    }
                    Ok(_) => {}
                    Err(error) => eprintln!("审计分片这次没搬动：{error}"),
                }
                // 上一个进程里没跑完的计划：从账本里那条 checkpoint 重新登记成暂停着的句柄。
                // 界面上从此看得见它、能恢复它，但它不会自己开始花钱
                let restored = app.state::<orchestra::orchestrator::Hub>().restore(app.handle());
                if restored > 0 {
                    eprintln!("从账本恢复了 {restored} 份未跑完的计划（暂停着，等用户点恢复）");
                }
            }
            // 话题开始钩子：插件的 SessionStart 在这里落地。同步跑——
            // 作者给启动钩子写的应当是快脚本，慢活该写 async
            crate::hooks::fire_app_event(app.handle(), "SessionStart");
            // 全局快捷键按配置恢复注册：失败只报一声，不挡启动
            crate::window::restore_global_shortcut(app.handle());
            // 沙箱开关的快照：工具执行体没有 config 通道（command_shell 同一模式）
            let sandbox_config = config::load(app.handle());
            crate::tool_runtime::sandbox::set_enabled(sandbox_config.sandbox_enabled);
            crate::tool_runtime::sandbox::set_writable_roots(
                sandbox_config
                    .sandbox_writable_roots
                    .iter()
                    .map(std::path::PathBuf::from)
                    .collect(),
            );
            // 删除保护的执行侧开关（design-security-center.md D1）：与沙箱同一模式
            crate::tools::set_delete_to_trash(sandbox_config.delete_to_trash);
            // 敏感保护的执行侧开关（design-security-center.md D6）：同一模式
            crate::secrets::set_scan_options(
                sandbox_config.secret_scan_enabled,
                sandbox_config.disabled_secret_rules.clone(),
                sandbox_config.custom_secret_rules.clone(),
                sandbox_config.secret_rule_pattern_edits.clone(),
            );
            // 「以后都允许」的持久放行回灌审批中心：判定面与话题内规则从此同一条，
            // 忘记/清空两个撤销口也只对着这一张表
            app.state::<approvals::ApprovalHub>().restore(
                &config::load(app.handle())
                    .allow_rules
                    .iter()
                    .map(|rule| (rule.key.clone(), rule.label.clone()))
                    .collect::<Vec<_>>(),
            );
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // 内置浏览器是 aglab 拉起的子进程：aglab 退出时带走它，
            // 不给用户留一个挂着专用 profile 的孤儿浏览器。
            // 后台命令的孩子同一批收：句柄已作废，进程树不该留下来
            if matches!(event, tauri::RunEvent::Exit | tauri::RunEvent::ExitRequested { .. }) {
                browser::shutdown(app);
                crate::tool_runtime::background::shutdown_all();
                // 话题结束钩子只认 Exit（真正退出）这一次：ExitRequested 可能被
                // 关闭确认框拦回去——拦回去的退出不该跟插件说"话题结束了"
                if matches!(event, tauri::RunEvent::Exit) {
                    crate::hooks::fire_app_event(app, "SessionEnd");
                }
            }
        });
}

/// 命令的**定义**与**注册表**对账。写了 tauri 的命令属性却没把名字填进注册表的那一条，
/// 界面上每一次调用都只会得到一句「命令未找到」——而 IPC 的键是字符串，
/// 编译器与 `tsc` 谁都不会响。这一格此前只靠人记（手工对账时确认过：两边现在各 118 条）
#[cfg(test)]
mod command_surface_tests {
    /// 属性单独占一行才算一条命令的声明：注释里提到那个字面串、字符串里拼它都不算数。
    /// 前缀用 `concat!` 拼出来，免得这个函数自己的源码长成一行的样子
    fn defined() -> Vec<String> {
        let attr = concat!("#[tauri::", "command");
        let mut out = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|value| value.to_str()) != Some("rs") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                let file = path.display().to_string();
                let mut lines = text.lines();
                while let Some(line) = lines.next() {
                    if !line.trim().starts_with(attr) {
                        continue;
                    }
                    // 属性与声明之间只许隔注释、别的属性和空行
                    let decl = lines
                        .find(|line| {
                            let t = line.trim();
                            !t.is_empty() && !t.starts_with("//") && !t.starts_with("#")
                        })
                        .unwrap_or_else(|| panic!("{file}：属性后面再没有别的行，这条命令没有声明"));
                    let Some(at) = decl.find("fn ") else {
                        panic!("{file}：属性后面第一个非注释行不是 fn 声明：{decl}");
                    };
                    let name: String = decl[at + 3..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    assert!(!name.is_empty(), "{file}：属性挂在了一个取不出名字的声明上");
                    out.push(name);
                }
            }
        }
        out
    }

    /// 注册表里那一串 `模块::名字,`：只吃 `generate_handler` 展开之后、`]` 之前的整行条目，
    /// 最后一项没有逗号也照样算注册过
    fn registry() -> Vec<String> {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/", "lib.rs"))
            .expect("读得到 lib.rs");
        let open = concat!("generate_handler", "![");
        let at = text.find(open).expect("lib.rs 里找得到那份注册表");
        let mut out = Vec::new();
        for line in text[at + open.len()..].lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("//") {
                continue;
            }
            if trimmed.starts_with(']') {
                break;
            }
            let last = trimmed.rsplit("::").next().unwrap_or(trimmed);
            let name: String = last
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            assert!(!name.is_empty(), "注册表里有一格取不出名字：{trimmed}");
            out.push(name);
        }
        out
    }

    #[test]
    fn every_command_written_is_also_registered() {
        let held = defined();
        let listed = registry();
        let (held_n, listed_n) = (held.len(), listed.len());
        assert!(
            held_n > 100 && listed_n > 100,
            "只扫到 {held_n} 条定义、{listed_n} 条注册——两个数一起掉下来是扫描自己坏了"
        );
        let mut sorted = held.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            held_n,
            "有两条命令重名：它们在 Rust 里合法（住不同模块），但注册表那一格只能认出一个，另一个永远调不到"
        );

        let missing: Vec<&String> = held.iter().filter(|name| !listed.contains(name)).collect();
        assert!(
            missing.is_empty(),
            "写了命令却没填进 invoke_handler，界面上每一次调用都是一句「命令未找到」：{missing:?}"
        );
        // 反方向同样要对上：注册表里的每一格都得有它自己的定义
        let ghosts: Vec<&String> = listed.iter().filter(|name| !held.contains(name)).collect();
        assert!(
            ghosts.is_empty(),
            "注册表里有 {ghosts:?} 找不到对应的命令定义——改名或删命令时漏了这一格"
        );
    }

    /// 前端那一头的调用点。只认 `invoke("字面量"` 与 `invoke<…>("字面量"`：
    /// 泛型实参按 `<` `>` 深度配对吃过去（`invoke<Omit<A, "id"> & { t: string }>(…)` 是同一条形状），
    /// `import { invoke } from …` 后面跟的是 `}` 而不是 `(`，那是引用不是调用点；
    /// 真的动态调用名（`invoke(cmd)`）单列进 blind——它不是"没有"，是"这条针看不见"
    fn frontend_callsites() -> (Vec<String>, Vec<String>) {
        fn ident_byte(c: u8) -> bool {
            c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
        }
        fn skip_space(bytes: &[u8], mut k: usize) -> usize {
            while matches!(bytes.get(k), Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n')) {
                k += 1;
            }
            k
        }
        let mut called = Vec::new();
        let mut blind = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("src")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let file_name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
                if !(file_name.ends_with(".ts") || file_name.ends_with(".tsx")) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                let file = path.display().to_string();
                let bytes = text.as_bytes();
                let mut at = 0usize;
                while let Some(found) = text[at..].find("invoke") {
                    let start = at + found;
                    let mut k = start + 6;
                    at = k;
                    let before = if start == 0 { b' ' } else { bytes[start - 1] };
                    let after = *bytes.get(k).unwrap_or(&b' ');
                    if ident_byte(before) || ident_byte(after) {
                        continue; // 更长标识符的一部分，或注释里的词
                    }
                    k = skip_space(bytes, k);
                    if bytes.get(k) == Some(&b'<') {
                        let mut depth = 0i32;
                        while let Some(c) = bytes.get(k) {
                            k += 1;
                            depth += match c {
                                b'<' => 1,
                                b'>' => -1,
                                _ => 0,
                            };
                            if depth == 0 {
                                break;
                            }
                        }
                        k = skip_space(bytes, k);
                    }
                    if bytes.get(k) != Some(&b'(') {
                        continue; // 引用，不是调用点
                    }
                    let line = text[..start].matches('\n').count() + 1;
                    let spot = format!("{file}:{line}");
                    k = skip_space(bytes, k + 1);
                    let quote = match bytes.get(k) {
                        Some(b'"') | Some(b'\'') => bytes[k],
                        _ => {
                            blind.push(spot);
                            continue; // 调用点，但名字不是字面量
                        }
                    };
                    match (k + 1..text.len()).find(|&j| bytes[j] == quote) {
                        Some(end) => {
                            called.push(text[k + 1..end].to_string());
                            at = end + 1;
                        }
                        None => blind.push(spot),
                    }
                }
            }
        }
        (called, blind)
    }

    #[test]
    fn every_name_the_frontend_calls_is_registered() {
        let (called, blind) = frontend_callsites();
        assert!(
            blind.is_empty(),
            "前端有 {blind:?} 处的调用名不是字符串字面量——这条针看不见它：要么把名字写回字面量，要么就改这条针，别让它装作什么都查到了"
        );
        assert!(
            called.len() > 100,
            "只扫到 {} 处调用点——这个数掉下来是扫描自己坏了",
            called.len()
        );
        let listed = registry();
        let mut missing: Vec<&String> = called
            .iter()
            .filter(|name| !listed.contains(name))
            .collect();
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "前端叫的名字在注册表里没有，点一次就是一句「命令未找到」：{missing:?}"
        );
    }
}
