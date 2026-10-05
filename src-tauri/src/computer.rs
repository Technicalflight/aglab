//! Computer Use：让模型看得见别的程序、并往它们身上动手。
//!
//! **这一切片里没有像素。** 全库的线协议（`provider/`、`session/`）到今天为止没有
//! 二进制内容那一格，`read_attachment` 只回文本，所以"截一张图给模型看"不是加一个工具，
//! 而是先改三套线协议的载荷形状。这里改走 **UI Automation**：列窗口、读某个窗口的控件树
//! （名字 + 控件类型 + 边界 + 可用状态）、按树里的位置点它、往焦点窗口敲字。
//! 它给的是文本，所以既进得了上下文，也进得了审计——`target` 是
//! 「记事本 · 菜单项「文件」」而不是「点了 (812, 407)」，后者事后没人读得懂。
//!
//! 两道不认档口的硬闸，写在动手之前：
//! 1. **敏感窗口直接拒**（[`sensitive_title`]）：标题里带着口令/支付/凭据类词的窗口，
//!    连它的控件树都不给读，权限档开到「完全访问」也不动。理由与 `is_catastrophic` 同源——
//!    有些动作的后果不是"这一档放不放行"该决定的。
//! 2. **敲进去的字不进审计、不进摘要**：`type` 与 `set_value` 的正文极可能就是口令，
//!    而审计是往盘上写的。那里只留字符数。
//!
//! 窗口把手与控件下标都会过期：界面一变，之前读到的那条编号就可能指向别的东西。
//! 所以每次动手都重新解析、重新遍历，取不到就报错让模型重新 inspect，而不是猜。

use serde_json::Value;

/// 一次最多读多少个控件。UIA 遍历整棵子树不便宜，而超过这个数的树里，
/// 模型真正要点的那一个几乎都在前面
pub const MAX_CONTROLS: usize = 200;
/// 名字里留多少字。控件名经常整段塞说明文字
const MAX_NAME: usize = 120;
/// 一次敲进去的字符上限。合成输入没有别的刹车，这一条就是它的刹车
pub const MAX_TYPED_CHARS: usize = 4_000;

/// 敏感窗口的判词。全部按**小写子串**匹配（[`sensitive_title`] 自己转小写），
/// 所以英文写小写，中文原样。
///
/// 这份名单挡的是"一次误点与被诱导的一次点击"，不是"一个铁了心的攻击者"：
/// 标题是应用自己写的，改个窗口名就能绕过去。界面上那句话必须照这个口径写，
/// 不许说成"机密内容受保护"
const SENSITIVE_PATTERNS: [&str; 18] = [
    "密码",
    "口令",
    "私钥",
    "助记词",
    "钱包",
    "网银",
    "银行",
    "支付",
    "password",
    "passkey",
    "credential",
    "keychain",
    "1password",
    "bitwarden",
    "lastpass",
    "keepass",
    "bitlocker",
    "vault",
];

/// 这个窗口标题是不是敏感窗口。命中就返回命中的那个词——报错时要说得出**是哪一条**拦的，
/// 只说"被安全策略拦下"会让人以为是自己点错了
pub fn sensitive_title(title: &str) -> Option<&'static str> {
    let lowered = title.to_lowercase();
    SENSITIVE_PATTERNS
        .iter()
        .copied()
        .find(|word| lowered.contains(word))
}

/// 敏感窗口那道闸的说话方式。工具与测试都读它，别处再拼一遍就是一句会过期的话
pub fn refusal(word: &str, title: &str) -> String {
    format!(
        "已按安全策略拦下：窗口标题里带着「{word}」，这一类窗口 aglab 不读也不动（权限档开到「完全访问」也不动）。换一个目标，或者你自己动手做完那一步。\n被拦下的窗口：{title}"
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    /// 十六进制的 HWND。模型只把它当不透明的把手用
    pub id: String,
    pub title: String,
    /// 左、上、宽、高
    pub rect: (i32, i32, i32, i32),
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlInfo {
    /// 它在控件树遍历结果里的下标。动手时用它指名点哪一个
    pub index: usize,
    pub name: String,
    pub control_type: String,
    pub enabled: bool,
    /// 这个控件认哪些动作：invoke / value / toggle / expand
    pub actions: Vec<&'static str>,
}

fn clip(text: &str, cap: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= cap {
        return trimmed.to_string();
    }
    let mut kept: String = trimmed.chars().take(cap - 1).collect();
    kept.push('…');
    kept
}

fn one_line(text: &str) -> String {
    text.split(['\r', '\n'])
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// 窗口名单那一屏文本。带编号，模型照编号点
pub fn render_windows(windows: &[WindowInfo]) -> String {
    if windows.is_empty() {
        return "没有可操作的窗口（只列可见、有标题、且没最小化的）".to_string();
    }
    let mut out = format!(
        "可见窗口 {} 个（把手会变，界面动过就重新列一次）：\n",
        windows.len()
    );
    for (number, window) in windows.iter().enumerate() {
        let (left, top, width, height) = window.rect;
        out.push_str(&format!(
            "{}. [{}] {} · {}×{} @ {},{}{}\n",
            number + 1,
            window.id,
            clip(&one_line(&window.title), MAX_NAME),
            width,
            height,
            left,
            top,
            if window.focused { " · 当前焦点" } else { "" }
        ));
    }
    out
}

/// 控件树那一屏文本。编号就是动手时的那个 `element`
pub fn render_tree(window: &WindowInfo, controls: &[ControlInfo], truncated: bool) -> String {
    let mut out = format!(
        "窗口 [{}] {} 的控件 {} 个：\n",
        window.id,
        clip(&one_line(&window.title), MAX_NAME),
        controls.len()
    );
    for control in controls {
        out.push_str(&format!(
            "{}. {}{}{} · {}\n",
            control.index,
            control.control_type,
            if control.name.is_empty() {
                String::new()
            } else {
                format!("「{}」", clip(&one_line(&control.name), MAX_NAME))
            },
            if control.enabled { "" } else { " · 不可用" },
            if control.actions.is_empty() {
                "只能看".to_string()
            } else {
                control.actions.join("/")
            }
        ));
    }
    if truncated {
        out.push_str(&format!(
            "…（只列了前 {MAX_CONTROLS} 个。要更里面的控件，先把窗口缩小或换个入口）\n"
        ));
    }
    out
}

/// 一次动手要做什么。载荷已经过校验，正文只活在这里，不进审计
#[derive(Debug)]
pub enum Act {
    Focus,
    Invoke(usize),
    SetValue(usize, String),
    Toggle(usize),
    Expand(usize),
    Type(String),
    Keys(String),
}

/// 参数解析。错误文案是给模型看的：说清缺什么、给到了什么
pub fn parse_act(args: &Value) -> Result<Act, String> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .ok_or("action 必填：focus / invoke / set_value / toggle / expand / type / keys")?;
    let element = || -> Result<usize, String> {
        args.get("element")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .ok_or_else(|| format!("{action} 需要 element：先 inspect_window 拿到控件编号"))
    };
    match action {
        "focus" => Ok(Act::Focus),
        "invoke" => Ok(Act::Invoke(element()?)),
        "toggle" => Ok(Act::Toggle(element()?)),
        "expand" => Ok(Act::Expand(element()?)),
        "set_value" => {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or("set_value 需要 text")?;
            Ok(Act::SetValue(element()?, text.to_string()))
        }
        "type" => {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or("type 需要 text")?;
            let count = text.chars().count();
            if count > MAX_TYPED_CHARS {
                return Err(format!("一次最多敲 {MAX_TYPED_CHARS} 个字符，这次给了 {count}"));
            }
            Ok(Act::Type(text.to_string()))
        }
        "keys" => {
            let keys = args
                .get("keys")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .ok_or("keys 需要 keys，形如 ctrl+s")?;
            Ok(Act::Keys(keys.to_string()))
        }
        other => Err(format!("没有 {other} 这个动作")),
    }
}

/// 审计与摘要里对这一次动手的说法。**`type` / `set_value` 的正文不进这里**——
/// 那极可能就是口令
pub fn describe_act(act: &Act) -> String {
    match act {
        Act::Focus => "focus".to_string(),
        Act::Invoke(index) => format!("invoke #{index}"),
        Act::Toggle(index) => format!("toggle #{index}"),
        Act::Expand(index) => format!("expand #{index}"),
        Act::SetValue(index, text) => {
            format!("set_value #{index} ‹{} 字不入审计›", text.chars().count())
        }
        Act::Type(text) => format!("type ‹{} 字不入审计›", text.chars().count()),
        Act::Keys(keys) => format!("keys {keys}"),
    }
}

/// 把 "ctrl+alt+del" 这种写法折成虚拟键码序列。认得出的键全在这张表里；
/// 认不出来就报错——**静默少按一个键的组合比不按更危险**
pub fn key_sequence(spec: &str) -> Result<Vec<u16>, String> {
    let parts: Vec<String> = spec
        .split('+')
        .map(|part| part.trim().to_lowercase())
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return Err("keys 是空的".to_string());
    }
    parts
        .iter()
        .map(|part| -> Result<u16, String> {
            Ok(match part.as_str() {
                "ctrl" | "control" => 0x11,
                "alt" | "menu" => 0x12,
                "shift" => 0x10,
                "win" | "meta" => 0x5B,
                "enter" | "return" => 0x0D,
                "tab" => 0x09,
                "esc" | "escape" => 0x1B,
                "space" => 0x20,
                "backspace" => 0x08,
                "up" => 0x26,
                "down" => 0x28,
                "left" => 0x25,
                "right" => 0x27,
                other => {
                    let bytes = other.as_bytes();
                    if bytes.len() != 1 {
                        return Err(format!("认不出按键「{other}」"));
                    }
                    match bytes[0] {
                        b'a'..=b'z' => (bytes[0] - b'a') as u16 + 0x41,
                        b'0'..=b'9' => bytes[0] as u16,
                        _ => return Err(format!("认不出按键「{other}」")),
                    }
                }
            })
        })
        .collect()
}

#[cfg(windows)]
pub mod win {
    use super::{ControlInfo, WindowInfo};
    use windows::core::{BSTR, BOOL, Interface};
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IExpandCollapseProvider, IInvokeProvider, IToggleProvider, IUIAutomation,
        IUIAutomationElement, IValueProvider, TreeScope_Descendants, UIA_ButtonControlTypeId,
        UIA_CONTROLTYPE_ID, UIA_CheckBoxControlTypeId,
        UIA_ComboBoxControlTypeId, UIA_DocumentControlTypeId, UIA_EditControlTypeId,
        UIA_ExpandCollapsePatternId, UIA_GroupControlTypeId, UIA_HyperlinkControlTypeId,
        UIA_ImageControlTypeId, UIA_InvokePatternId, UIA_ListItemControlTypeId,
        UIA_ListControlTypeId, UIA_MenuBarControlTypeId, UIA_MenuItemControlTypeId, UIA_PATTERN_ID,
        UIA_RadioButtonControlTypeId, UIA_ScrollBarControlTypeId, UIA_SliderControlTypeId,
        UIA_TabControlTypeId, UIA_TabItemControlTypeId, UIA_TextControlTypeId, UIA_TogglePatternId,
        UIA_TreeControlTypeId, UIA_TreeItemControlTypeId, UIA_ValuePatternId,
        UIA_WindowControlTypeId,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
        MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetForegroundWindow, GetSystemMetrics, GetWindowRect, GetWindowTextLengthW,
        GetWindowTextW, IsWindow, IsWindowVisible, SetForegroundWindow, SM_CXSCREEN, SM_CYSCREEN,
    };

    /// COM 的初始化对。UIA 要它，每次调用自己起、自己收。
    /// 只有真的从我们手里起来的那个才收回去：别的线程初始化过的，我们收了就是替人拆台
    struct Com(bool);

    impl Com {
        fn new() -> Self {
            use windows::core::HRESULT;
            // 同线程重复初始化返回 S_FALSE（非零的"成"码），那一次不算我们起的
            let code = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            Com(code == HRESULT(0))
        }
    }

    impl Drop for Com {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    fn text_of(window: HWND) -> String {
        let length = unsafe { GetWindowTextLengthW(window) };
        if length <= 0 {
            return String::new();
        }
        let mut buffer = vec![0u16; (length as usize) + 1];
        let written = unsafe { GetWindowTextW(window, &mut buffer) };
        String::from_utf16_lossy(&buffer[..written.max(0) as usize])
    }

    unsafe extern "system" fn collect(window: HWND, state: LPARAM) -> BOOL {
        let sink = &mut *(state.0 as *mut Vec<WindowInfo>);
        if window.is_invalid() || !IsWindowVisible(window).as_bool() {
            return BOOL(1);
        }
        let title = text_of(window);
        if title.trim().is_empty() {
            return BOOL(1);
        }
        let mut rect = RECT::default();
        if GetWindowRect(window, &mut rect).is_err() {
            return BOOL(1);
        }
        // 面积为零的窗口点不到（最小化或正在销毁）。列出来只会让模型挑一个动不了的目标
        if rect.right <= rect.left || rect.bottom <= rect.top {
            return BOOL(1);
        }
        sink.push(WindowInfo {
            id: format!("{:x}", window.0 as usize),
            title,
            rect: (rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top),
            focused: GetForegroundWindow() == window,
        });
        BOOL(1)
    }

    fn parse_id(id: &str) -> Result<HWND, String> {
        let raw = usize::from_str_radix(id.trim().trim_start_matches("0x"), 16)
            .map_err(|_| format!("窗口把手「{id}」读不出来：用 list_windows 给的那个"))?;
        let window = HWND(raw as *mut _);
        if unsafe { IsWindow(Some(window)).as_bool() } {
            Ok(window)
        } else {
            Err(format!("窗口 [{id}] 已经不在了。界面变了，重新列一次"))
        }
    }

    pub fn title_of(id: &str) -> Result<String, String> {
        Ok(text_of(parse_id(id)?))
    }

    pub fn list_windows() -> Vec<WindowInfo> {
        let mut sink: Vec<WindowInfo> = Vec::new();
        let state = LPARAM(&mut sink as *mut _ as isize);
        // EnumWindows 的返回值说的是"有没有被回调中止"。我们一路返回真，所以这里不接
        unsafe {
            let _ = EnumWindows(Some(collect), state);
        }
        sink
    }

    pub fn focus(id: &str) -> Result<(), String> {
        let window = parse_id(id)?;
        // 前台窗口由系统管着：从别的线程抢前台可能被拒（用户正按着键盘）。
        // 所以这一句失败要说人话，不许假装切过去了
        if !unsafe { SetForegroundWindow(window) }.as_bool() {
            return Err(
                "那个窗口不肯到前台（系统只让当前活动的应用抢前台）。让用户点它一下，或者改走 keys"
                    .to_string(),
            );
        }
        Ok(())
    }

    fn automation() -> Result<(Com, IUIAutomation), String> {
        let com = Com::new();
        let instance: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|error| format!("UI Automation 起不来：{error}"))?;
        Ok((com, instance))
    }

    /// UIA 的控制类型常量在 windows crate 里就是这种小写混排的名字，拿来当模式匹配
    /// 会被 Rust 的命名规范警告一次。这里不改名也不逐条压：改不了上游，逐条压会压掉真的问题
    #[allow(non_upper_case_globals)]
    fn control_name(kind: UIA_CONTROLTYPE_ID) -> &'static str {
        match kind {
            UIA_ButtonControlTypeId => "按钮",
            UIA_EditControlTypeId => "输入框",
            UIA_TextControlTypeId => "文本",
            UIA_DocumentControlTypeId => "文档",
            UIA_ListControlTypeId => "列表",
            UIA_ListItemControlTypeId => "列表项",
            UIA_MenuBarControlTypeId => "菜单栏",
            UIA_MenuItemControlTypeId => "菜单项",
            UIA_ComboBoxControlTypeId => "下拉框",
            UIA_CheckBoxControlTypeId => "复选框",
            UIA_RadioButtonControlTypeId => "单选钮",
            UIA_TabControlTypeId => "标签页",
            UIA_TabItemControlTypeId => "标签",
            UIA_TreeControlTypeId => "树",
            UIA_TreeItemControlTypeId => "树项",
            UIA_GroupControlTypeId => "分组",
            UIA_HyperlinkControlTypeId => "链接",
            UIA_ImageControlTypeId => "图片",
            UIA_ScrollBarControlTypeId => "滚动条",
            UIA_SliderControlTypeId => "滑块",
            UIA_WindowControlTypeId => "窗口",
            _ => "控件",
        }
    }

    /// 一次遍历的全部控件。`tree` 与 `with_element` 都走它，
    /// 所以读出来的编号与动手时取的那一个是同一套顺序
    fn descendants(
        root: &IUIAutomationElement,
        automation: &IUIAutomation,
    ) -> Result<(i32, Vec<IUIAutomationElement>), String> {
        let condition = unsafe { automation.CreateTrueCondition() }
            .map_err(|error| format!("条件建不出来：{error}"))?;
        let found = unsafe { root.FindAll(TreeScope_Descendants, &condition) }
            .map_err(|error| format!("读不到控件树：{error}"))?;
        let total = unsafe { found.Length() }.map_err(|error| format!("数不出控件：{error}"))?;
        let mut items = Vec::new();
        for index in 0..total {
            if let Ok(item) = unsafe { found.GetElement(index) } {
                items.push(item);
            }
        }
        Ok((total, items))
    }

    fn pattern<P: Interface>(item: &IUIAutomationElement, id: UIA_PATTERN_ID) -> Option<P> {
        unsafe { item.GetCurrentPatternAs::<P>(id).ok() }
    }

    /// 读控件树。返回 (控件列表, 是否被截断)
    pub fn tree(id: &str, cap: usize) -> Result<(Vec<ControlInfo>, bool), String> {
        let (_com, automation) = automation()?;
        let root = unsafe { automation.ElementFromHandle(parse_id(id)?) }
            .map_err(|error| format!("读不到那个窗口：{error}"))?;
        let (total, items) = descendants(&root, &automation)?;

        let mut controls = Vec::new();
        for (index, item) in items.iter().take(cap).enumerate() {
            let name = unsafe { item.CurrentName() }
                .map(|value| value.to_string())
                .unwrap_or_default();
            let kind = unsafe { item.CurrentControlType() }.unwrap_or(UIA_CONTROLTYPE_ID(0));
            let enabled = unsafe { item.CurrentIsEnabled() }
                .map(|value| value.as_bool())
                .unwrap_or(true);
            let mut actions: Vec<&'static str> = Vec::new();
            if pattern::<IInvokeProvider>(item, UIA_InvokePatternId).is_some() {
                actions.push("invoke");
            }
            if pattern::<IValueProvider>(item, UIA_ValuePatternId).is_some() {
                actions.push("value");
            }
            if pattern::<IToggleProvider>(item, UIA_TogglePatternId).is_some() {
                actions.push("toggle");
            }
            if pattern::<IExpandCollapseProvider>(item, UIA_ExpandCollapsePatternId).is_some() {
                actions.push("expand");
            }
            controls.push(ControlInfo {
                index,
                name,
                control_type: control_name(kind).to_string(),
                enabled,
                actions,
            });
        }
        let shown = controls.len();
        Ok((controls, total as usize > shown))
    }

    fn with_element(
        id: &str,
        index: usize,
        do_it: impl Fn(&IUIAutomationElement) -> Result<(), String>,
    ) -> Result<(), String> {
        let (_com, automation) = automation()?;
        let root = unsafe { automation.ElementFromHandle(parse_id(id)?) }
            .map_err(|error| format!("读不到那个窗口：{error}"))?;
        let (total, items) = descendants(&root, &automation)?;
        let Some(item) = items.into_iter().nth(index) else {
            return Err(format!(
                "控件 #{index} 取不到了（这棵树现在数出 {total} 个）。界面变了，重新 inspect_window 再动手"
            ));
        };
        do_it(&item)
    }

    /// 没有 Invoke 模式时的退路：按它的中心点一下。这一步是真的动鼠标
    unsafe fn click_point(x: i32, y: i32) {
        let screen_w = GetSystemMetrics(SM_CXSCREEN).max(1);
        let screen_h = GetSystemMetrics(SM_CYSCREEN).max(1);
        let absolute =
            |value: i32, span: i32| ((value as f64 / span as f64) * 65535.0).round() as i32;
        let mouse = |flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32| INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let inputs = [
            mouse(
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                absolute(x, screen_w),
                absolute(y, screen_h),
            ),
            mouse(MOUSEEVENTF_LEFTDOWN, 0, 0),
            mouse(MOUSEEVENTF_LEFTUP, 0, 0),
        ];
        let _ = SendInput(&inputs, core::mem::size_of::<INPUT>() as i32);
    }

    unsafe fn send_key(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) {
        let inputs = [INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }];
        let _ = SendInput(&inputs, core::mem::size_of::<INPUT>() as i32);
    }

    pub fn type_text(text: &str) {
        for unit in text.encode_utf16() {
            unsafe {
                // wVk=0 + KEYEVENTF_UNICODE：让系统按当前键盘布局与输入法自己翻。
                // 我们不去猜"这个字符在这套键盘上是哪个键"——猜错就是敲进别的东西
                send_key(0, unit, KEYEVENTF_UNICODE);
                send_key(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
            }
        }
    }

    pub fn press_keys(sequence: &[u16]) {
        unsafe {
            for vk in sequence {
                send_key(*vk, 0, KEYBD_EVENT_FLAGS(0));
            }
            // 反序松开：按住 ctrl 再按 s 才是 Ctrl+S，先松 ctrl 就变成两次独立按键
            for vk in sequence.iter().rev() {
                send_key(*vk, 0, KEYEVENTF_KEYUP);
            }
        }
    }

    pub fn invoke(id: &str, index: usize) -> Result<(), String> {
        with_element(id, index, |item| {
            if let Some(provider) = pattern::<IInvokeProvider>(item, UIA_InvokePatternId) {
                return unsafe { provider.Invoke() }.map_err(|error| format!("点它没成：{error}"));
            }
            let rect = unsafe { item.CurrentBoundingRectangle() }
                .map_err(|error| format!("这个控件读不到位置，也点不到：{error}"))?;
            let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
            // 位置为零就拒：那一下会落在屏幕角落，谁也不知道点到什么
            if width <= 0 || height <= 0 {
                return Err("这个控件既不支持 Invoke，也没有可见的位置。换一个入口".to_string());
            }
            unsafe { click_point(rect.left + width / 2, rect.top + height / 2) };
            Ok(())
        })
    }

    pub fn set_value(id: &str, index: usize, text: &str) -> Result<(), String> {
        with_element(id, index, |item| {
            let provider = pattern::<IValueProvider>(item, UIA_ValuePatternId).ok_or(
                "这个控件不认 set_value（它没有 Value 模式）。改用 type：先 focus 窗口再敲字",
            )?;
            unsafe { provider.SetValue(&BSTR::from(text)) }
                .map_err(|error| format!("填不进去：{error}"))
        })
    }

    pub fn toggle(id: &str, index: usize) -> Result<(), String> {
        with_element(id, index, |item| {
            let provider = pattern::<IToggleProvider>(item, UIA_TogglePatternId)
                .ok_or("这个控件没有可切换的状态")?;
            unsafe { provider.Toggle() }.map_err(|error| format!("切换没成：{error}"))
        })
    }

    pub fn expand(id: &str, index: usize) -> Result<(), String> {
        with_element(id, index, |item| {
            let provider = pattern::<IExpandCollapseProvider>(item, UIA_ExpandCollapsePatternId)
                .ok_or("这个控件不能展开")?;
            unsafe { provider.Expand() }.map_err(|error| format!("展开没成：{error}"))
        })
    }
}

#[cfg(not(windows))]
pub mod win {
    // 没有 Windows 的编译环境时（别人的 Linux、CI）这一层整个退成错误。
    // 不许退成"成功但什么都没做"
    use super::{ControlInfo, WindowInfo};

    const UNAVAILABLE: &str = "Computer Use 只在 Windows 上可用";

    pub fn list_windows() -> Vec<WindowInfo> {
        Vec::new()
    }
    pub fn title_of(_id: &str) -> Result<String, String> {
        Err(UNAVAILABLE.to_string())
    }
    pub fn focus(_id: &str) -> Result<(), String> {
        Err(UNAVAILABLE.to_string())
    }
    pub fn tree(_id: &str, _cap: usize) -> Result<(Vec<ControlInfo>, bool), String> {
        Err(UNAVAILABLE.to_string())
    }
    pub fn invoke(_id: &str, _index: usize) -> Result<(), String> {
        Err(UNAVAILABLE.to_string())
    }
    pub fn set_value(_id: &str, _index: usize, _text: &str) -> Result<(), String> {
        Err(UNAVAILABLE.to_string())
    }
    pub fn toggle(_id: &str, _index: usize) -> Result<(), String> {
        Err(UNAVAILABLE.to_string())
    }
    pub fn expand(_id: &str, _index: usize) -> Result<(), String> {
        Err(UNAVAILABLE.to_string())
    }
    pub fn type_text(_text: &str) {}
    pub fn press_keys(_sequence: &[u16]) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_titles_are_caught_whatever_the_case() {
        assert_eq!(sensitive_title("Password Manager — Vault"), Some("password"));
        assert_eq!(sensitive_title("输入密码 - 记事本"), Some("密码"));
        assert_eq!(sensitive_title("BITLOCKER (D:)"), Some("bitlocker"));
        assert_eq!(sensitive_title("main.rs — aglab"), None);
        assert!(refusal("密码", "输入密码").contains("完全访问"));
    }

    #[test]
    fn typed_text_never_reaches_the_audit_line() {
        let secret = "hunter2-not-a-real-password";
        for line in [
            describe_act(&Act::Type(secret.to_string())),
            describe_act(&Act::SetValue(3, secret.to_string())),
        ] {
            assert!(!line.contains(secret), "正文漏进了审计说法里：{line}");
            assert!(line.contains("不入审计"), "这一行得说清正文为什么不在：{line}");
        }
        // 组合键与点哪个控件是要留下的
        assert_eq!(describe_act(&Act::Keys("ctrl+s".to_string())), "keys ctrl+s");
        assert_eq!(describe_act(&Act::Invoke(7)), "invoke #7");
    }

    #[test]
    fn key_combos_are_named_or_refused_not_guessed() {
        assert_eq!(key_sequence("Ctrl+S").unwrap(), vec![0x11, 0x53]);
        assert_eq!(key_sequence("alt+tab").unwrap(), vec![0x12, 0x09]);
        assert_eq!(key_sequence("win+e").unwrap(), vec![0x5B, 0x45]);
        assert!(key_sequence("ctrl+f13").is_err(), "认不出的键不许静默少按");
        assert!(key_sequence("  ").is_err());
    }

    #[test]
    fn typed_input_is_capped_and_actions_are_checked() {
        let long = "字".repeat(MAX_TYPED_CHARS + 1);
        let error = parse_act(&serde_json::json!({ "action": "type", "text": long })).unwrap_err();
        assert!(error.contains(&MAX_TYPED_CHARS.to_string()), "{error}");
        assert!(parse_act(&serde_json::json!({ "action": "type" })).is_err());
        assert!(parse_act(&serde_json::json!({ "action": "dance" })).is_err());
        // element 类动作不给编号就报错，不许默默作用在第一个控件上
        assert!(parse_act(&serde_json::json!({ "action": "invoke" })).is_err());
        assert!(matches!(
            parse_act(&serde_json::json!({ "action": "focus" })),
            Ok(Act::Focus)
        ));
    }

    #[test]
    fn windows_and_trees_render_as_text_a_model_can_point_at() {
        let list = vec![
            WindowInfo {
                id: "1a2b".to_string(),
                title: "第一行\n第二行".to_string(),
                rect: (10, 20, 800, 600),
                focused: true,
            },
            WindowInfo {
                id: "3c4d".to_string(),
                title: "另一个".to_string(),
                rect: (0, 0, 100, 100),
                focused: false,
            },
        ];
        let rendered = render_windows(&list);
        assert!(rendered.contains("[1a2b] 第一行"), "{rendered}");
        assert!(!rendered.contains("第二行"), "标题只取第一行：{rendered}");
        assert!(rendered.contains("当前焦点"));
        assert!(rendered.contains("2 个"));
        assert!(render_windows(&[]).contains("没有可操作的窗口"));

        let tree = render_tree(
            &list[0],
            &[ControlInfo {
                index: 4,
                name: "保存".to_string(),
                control_type: "按钮".to_string(),
                enabled: false,
                actions: vec!["invoke"],
            }],
            true,
        );
        assert!(tree.contains("4. 按钮「保存」 · 不可用 · invoke"), "{tree}");
        assert!(tree.contains("只列了前"), "被截断必须说出来：{tree}");

        let plain = render_tree(
            &list[0],
            &[ControlInfo {
                index: 0,
                name: String::new(),
                control_type: "分组".to_string(),
                enabled: true,
                actions: vec![],
            }],
            false,
        );
        assert!(plain.contains("只能看"), "{plain}");
    }

    /// 真机读数：那条 unsafe 的 EnumWindows 路径要真的列得出窗口。
    /// 平时不跑（它读的是这台机器此刻的桌面，换一台机器结果就不同），
    /// 取读数：`cargo test --lib computer:: -- --ignored`
    #[test]
    #[ignore = "要一个有窗口的桌面话题：它枚举的是这台机器此刻的窗口名单"]
    fn the_desktop_actually_lists_its_windows() {
        let listed = win::list_windows();
        assert!(!listed.is_empty(), "一个窗口都没列出来：EnumWindows 那条路没走通");
        for window in &listed {
            assert!(!window.title.trim().is_empty(), "标题为空的窗口不该进名单");
            assert!(!window.id.is_empty());
            let (left, top, width, height) = window.rect;
            assert!(width > 0 && height > 0, "零面积的不该列：{window:?}");
            let _ = (left, top);
        }
        // 把手要能读回来——动手那一步就是靠它找回同一个窗口
        let first = &listed[0];
        assert_eq!(win::title_of(&first.id).ok().as_deref(), Some(first.title.as_str()));
        assert!(render_windows(&listed).contains("可见窗口"));
    }
}
