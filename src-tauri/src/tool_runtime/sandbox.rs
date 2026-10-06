//! 低完整性沙箱（一期）：模型拉起的子进程被压到 Low 完整性——
//! **读不受限**（工具链、配置照常读），**写全面受限**（盘外文件、注册表一律拒绝），
//! 可写的只有被显式标过 Low 的根：当前项目根与 aglab 专用的沙箱临时目录。
//!
//! 机制（全部站在已拍板的 fail-closed 上）：
//! 1. 子进程以 `CREATE_SUSPENDED` 拉起——第一条指令都还没跑；
//! 2. `OpenProcessToken` + `SetTokenInformation(TokenIntegrityLevel, S-1-16-4096)`
//!    把它的主令牌压到 Low（no-write-up 从此生效）；
//! 3. 恢复主线程（Toolhelp 找到那条挂起的线程，`ResumeThread`）。
//! 任何一步失败：调用方杀掉孩子、按拒绝执行报错——与收容同一条拍板，不降级。
//!
//! 可写根的授权用完整性标签继承：对根执行一次
//! `icacls <root> /setintegritylevel (OI)(CI)LOW`，**已有文件经继承自动拿到 Low**
//! （2026-10-02 实测），此后新建/移动进来的文件照样继承——不存在逐文件递归的成本。
//!
//! 一期的诚实边界：**网络不隔离**（AppContainer 是二期）；钩子与 MCP 服务器不进沙箱
//! （前者是用户逐条确认过的脚本，后者是用户点名的常驻服务）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Mutex;

use serde_json::Value;

/// 子进程的专用临时目录：很多工具链（MSVC 的 rsp、npm 的缓存）要写 %TEMP%，
/// 系统那份是 Medium 标签，低完整性写不进去——沙箱孩子统一指向这份（Low 标签）。
/// 挂在 aglab 自己的临时目录底下，不与用户的 %TEMP% 混放
pub fn sandbox_tmp() -> PathBuf {
    std::env::temp_dir().join("aglab").join("sandbox-tmp")
}

/// 已标注过的根。同一根在一次话题里只打一次标：icacls 全树继承虽是一次性成本，
/// 大仓库上也不该每条命令都付。enable/disable 的翻转与跨话题的状态由配置管，
/// 这份缓存只回答"这个话题里标过没有"
fn labeled_roots() -> &'static Mutex<HashSet<PathBuf>> {
    static LABELED: std::sync::OnceLock<Mutex<HashSet<PathBuf>>> = std::sync::OnceLock::new();
    LABELED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 把一个根（含全部已有内容，经继承）标成 Low。失败如实报错——调用方按
/// fail-closed 处理。反复调用无害：icacls 幂等，缓存命中直接过
pub fn label_root(root: &Path) -> Result<(), String> {
    let key = root.to_path_buf();
    if labeled_roots().lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(&key) {
        return Ok(());
    }
    if let Some(parent) = root.parent() {
        std::fs::create_dir_all(root).map_err(|e| format!("沙箱可写根建不出来：{e}"))?;
        let _ = parent;
    }
    let output = crate::childproc::hide(std::process::Command::new("icacls"))
        .arg(root.as_os_str())
        .args(["/setintegritylevel", "(OI)(CI)LOW"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("icacls 跑不起来（沙箱标注失败）：{e}"))?;
    if !output.status.success() {
        return Err(format!(
            "把「{}」标成低完整性失败：{}",
            root.display(),
            crate::tool_runtime::constrain::decode_output(&output.stderr).trim()
        ));
    }
    labeled_roots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key);
    Ok(())
}

/// 一条命令的就位与能力清单：专用临时目录、额外可写根、（未等于主目录时的）cwd
/// 逐个 ensure（标签 + 授权），返回全部能力 SID——activate 用它们构造
/// WRITE_RESTRICTED 令牌。未绑定时 cwd = 主目录：**绝不标注/授予主目录**
/// （那等于把整个用户档案开放给低完整性进程），命令的写入退到专用临时目录，
/// 写 cwd 会被内核拒
pub fn prepare_command_roots(cwd: &Path) -> Result<Vec<String>, String> {
    let mut sids = Vec::new();
    let tmp = sandbox_tmp();
    sids.push(ensure_root(&tmp, "tmp")?);
    for root in writable_roots() {
        sids.push(ensure_root(&root, "extra")?);
    }
    if crate::config::home_root().as_deref() != Some(cwd) {
        sids.push(ensure_root(cwd, "workspace")?);
    }
    Ok(sids)
}

/// 一个根的完整就位：完整性标签（icacls，一期）+ 授权（icacls 二条：能力 SID 的
/// `(OI)(CI)M` grant——WRITE_RESTRICTED pass-2 的唯一依据；world 的 `(CI)(DC)` deny——
/// 堵"借父目录的删除权清空已授予根"的逃逸线，deepseek 生产注释里的同一坑）。
/// 缓存按路径：话题内只做一次，返回这条根的能力 SID
fn ensure_root(root: &Path, domain: &str) -> Result<String, String> {
    static ENSURED: std::sync::OnceLock<Mutex<HashSet<PathBuf>>> = std::sync::OnceLock::new();
    let ensured = ENSURED.get_or_init(|| Mutex::new(HashSet::new()));
    let key = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let sid = capability_sid(root, domain)?;
    if ensured.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(&key) {
        return Ok(sid);
    }
    label_root(root)?;
    grant_root_native(root, &sid)?;
    ensured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key);
    Ok(sid)
}

/// 原生授权（对齐 deepseek `acl.ts::grantWrite` 的两条编辑，一次 SetNamedSecurityInfoW）：
/// 1. 能力 SID 的 allow ACE（容器+对象继承，Modify 形状的读改执行删）——
///    WRITE_RESTRICTED pass-2 的唯一依据；
/// 2. **Deny world 的 FILE_DELETE_CHILD（只限容器）**——堵"借父目录的删除权
///    清空另一个已授予根"的逃逸线。
/// 标签仍由 label_root（icacls）负责：Low 完整性检查要过，授予根必须是 Low。
/// 并发保护从简：grant 在 enable 与首条命令各至多一次，话题内无竞争写者
#[cfg(windows)]
fn grant_root_native(root: &Path, cap_sid_text: &str) -> Result<(), String> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::{
        CreateWellKnownSid, DACL_SECURITY_INFORMATION, OBJECT_SECURITY_INFORMATION,
        PSID, SUB_CONTAINERS_AND_OBJECTS_INHERIT, SUB_CONTAINERS_ONLY_INHERIT, WinWorldSid,
    };
    use windows::Win32::Security::Authorization::{
        GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, DENY_ACCESS,
        EXPLICIT_ACCESS_W, GRANT_ACCESS, SE_FILE_OBJECT,
    };
    use windows::Win32::Storage::FileSystem::{
        FILE_DELETE_CHILD, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    };

    // Modify 形状：读 + 写 + 执行 + 删除（授予根内的常规改动）
    const MODIFY: u32 = FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0 | FILE_GENERIC_EXECUTE.0 | 0x0001_0000;

    let cap_sid = unsafe { string_to_sid(cap_sid_text)? };
    let mut world_buf = [0u8; 68];
    let mut world_size = world_buf.len() as u32;
    unsafe {
        CreateWellKnownSid(
            WinWorldSid,
            None,
            Some(PSID(world_buf.as_mut_ptr().cast())),
            &mut world_size,
        )
        .map_err(|e| format!("构建 world SID 失败：{e}"))?;
    }
    let world_sid = PSID(world_buf.as_ptr() as *mut core::ffi::c_void);

    let name = HSTRING::from(root.as_os_str());
    unsafe {
        // 读当前 DACL（GetNamedSecurityInfoW 返回的 SD 与 DACL 用 LocalFree 收）
        let mut dacl: *mut windows::Win32::Security::ACL = std::ptr::null_mut();
        let mut sd = windows::Win32::Security::PSECURITY_DESCRIPTOR::default();
        let code = GetNamedSecurityInfoW(
            &name,
            SE_FILE_OBJECT,
            OBJECT_SECURITY_INFORMATION(DACL_SECURITY_INFORMATION.0),
            None,
            None,
            Some(&mut dacl),
            None,
            &mut sd,
        );
        if code.0 != 0 {
            return Err(format!("读取 {root:?} 的 DACL 失败：WIN32_ERROR {code:?}"));
        }

        let entries = [
            EXPLICIT_ACCESS_W {
                grfAccessPermissions: MODIFY,
                grfAccessMode: GRANT_ACCESS,
                grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
                Trustee: trustee_for(&cap_sid),
            },
            EXPLICIT_ACCESS_W {
                grfAccessPermissions: FILE_DELETE_CHILD.0,
                grfAccessMode: DENY_ACCESS,
                grfInheritance: SUB_CONTAINERS_ONLY_INHERIT,
                Trustee: trustee_for(&world_sid),
            },
        ];
        let mut new_dacl: *mut windows::Win32::Security::ACL = std::ptr::null_mut();
        let merge = SetEntriesInAclW(Some(&entries), Some(dacl), &mut new_dacl);
        if merge.0 != 0 {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            return Err(format!("合并 {root:?} 的 DACL 失败：WIN32_ERROR {merge:?}"));
        }

        let write = SetNamedSecurityInfoW(
            &name,
            SE_FILE_OBJECT,
            OBJECT_SECURITY_INFORMATION(DACL_SECURITY_INFORMATION.0),
            None,
            None,
            Some(new_dacl),
            None,
        );
        let _ = LocalFree(Some(HLOCAL(new_dacl as *mut core::ffi::c_void)));
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        if write.0 != 0 {
            return Err(format!("写回 {root:?} 的 DACL 失败：WIN32_ERROR {write:?}"));
        }
        Ok(())
    }
}

/// EXPLICIT_ACCESS_W 的 trustee 形状：SID 直指、无多重委托
#[cfg(windows)]
unsafe fn trustee_for(sid: &windows::Win32::Security::PSID) -> windows::Win32::Security::Authorization::TRUSTEE_W {
    use windows::Win32::Security::Authorization::{
        NO_MULTIPLE_TRUSTEE, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: windows::core::PWSTR(sid.0 as *mut u16),
    }
}

/// icacls 的一条调用。失败把 stderr 带出来——标注/授权失败一律 fail-closed
fn icacls(root: &Path, args: &[&str]) -> Result<(), String> {
    let output = crate::childproc::hide(std::process::Command::new("icacls"))
        .arg(root.as_os_str())
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("icacls 跑不起来：{e}"))?;
    if !output.status.success() {
        return Err(format!(
            "icacls {} 失败：{}",
            args.join(" "),
            crate::tool_runtime::constrain::decode_output(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// 能力 SID：S-1-4-a-b，a/b 由「domain + canonical 路径」的 SHA-256 派生（30 位截断）。
/// 同一路径永远同一 SID（授权是长期缓存）；domain 区分工作目录/临时/额外根，
/// 同一路径在不同 domain 下是不同身份
fn capability_sid(root: &Path, domain: &str) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let first = u32::from_le_bytes(digest[0..4].try_into().unwrap()) % ((1u32 << 30) - 1) + 1;
    let second = u32::from_le_bytes(digest[4..8].try_into().unwrap()) % ((1u32 << 30) - 1) + 1;
    Ok(format!("S-1-4-{first}-{second}"))
}

/// 沙箱边界对**文件工具**的判定（对齐 Codex：单一边界覆盖代理的一切动作）。
/// 收容与低完整性只管 `run_command` 的子进程；`write_file` / `edit_file` 是本进程
/// 直写，不经子进程——沙箱开着时它们的可写范围必须与命令的可写范围一致：
/// **绑定的项目根之内**（worktree 或项目），之外一律拒，未绑定则全拒。
/// `conversation_root` 是执行侧解析相对路径的那份根（effective_root，含主目录回退）；
/// `bound_root` 是边界（无主目录回退）。返回 Some(拒绝理由) = 越界；read 类永远 None
pub fn boundary_violation(
    name: &str,
    args: &Value,
    bound_root: Option<&Path>,
    conversation_root: Option<&Path>,
) -> Option<String> {
    if !matches!(name, "write_file" | "edit_file") {
        return None;
    }
    let raw = args.get("path").and_then(Value::as_str)?;
    // 与执行同一套解析
    let target = crate::tools::resolve(raw, conversation_root);
    let inside = bound_root.is_some_and(|root| {
        crate::tools::inside_root(&target, Some(root))
    });
    if inside {
        return None;
    }
    Some(match bound_root {
        Some(root) => format!(
            "沙箱开启：文件写入只限绑定的工作目录（{}）。要写「{}」，先在输入框绑定对应工作目录，或关闭命令沙箱。",
            root.display(),
            target.display()
        ),
        None => format!(
            "沙箱开启：这场话题没有绑定工作目录，没有可写的边界。先在输入框绑定工作目录，或关闭命令沙箱。（目标：{}）",
            target.display()
        ),
    })
}

/// 「设置 → 命令沙箱」的开关快照：与 command_shell 同一个模式——
/// 工具执行体没有 config 通道，setup 与开关命令各同步一次
static ENABLED: std::sync::OnceLock<std::sync::RwLock<bool>> = std::sync::OnceLock::new();

pub fn set_enabled(value: bool) {
    let mut guard = ENABLED
        .get_or_init(|| std::sync::RwLock::new(false))
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = value;
}

pub fn enabled() -> bool {
    ENABLED
        .get()
        .map(|lock| *lock.read().unwrap_or_else(std::sync::PoisonError::into_inner))
        .unwrap_or(false)
}

/// 额外可写根的快照（writable_roots，对齐 Codex 的 sandbox_workspace_write.writable_roots）：
/// 配置里的每个目录在 prepare 时照项目根同一套打法打 Low 标签
static WRITABLE_ROOTS: std::sync::OnceLock<std::sync::RwLock<Vec<PathBuf>>> =
    std::sync::OnceLock::new();

pub fn set_writable_roots(roots: Vec<PathBuf>) {
    let mut guard = WRITABLE_ROOTS
        .get_or_init(|| std::sync::RwLock::new(Vec::new()))
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = roots;
}

pub fn writable_roots() -> Vec<PathBuf> {
    WRITABLE_ROOTS
        .get()
        .map(|lock| lock.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone())
        .unwrap_or_default()
}

/// 挂起拉起的创建标志。沙箱开着时 spawn 必须用它：先挂起、再压令牌、后恢复，
/// 一条指令都不漏在沙箱外跑
pub const CREATE_SUSPENDED: u32 = 0x0000_0004;

/// 开关命令。启用前先把可写根就位（标签 + 授权）——**失败就不落盘**：
/// "配置写着开而沙箱没就位"是 fail-closed 拍板里最不能容忍的那种各说各话
#[tauri::command]
pub fn sandbox_set(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    if enabled {
        let config = crate::config::load(&app);
        if let Some(project) = config.active_project() {
            ensure_root(Path::new(&project.path), "workspace")?;
        }
        ensure_root(&sandbox_tmp(), "tmp")?;
        for root in writable_roots() {
            ensure_root(&root, "extra")?;
        }
    }
    set_enabled(enabled);
    let mut config = crate::config::load(&app);
    config.sandbox_enabled = enabled;
    crate::config::save(&app, &config)
}

/// 额外可写根的设置入口（对齐 Codex 的 writable_roots）。**逐个验证并标注**：
/// 路径不存在/标不上就整体报错不落盘——半个生效的可写根清单比没有更危险。
/// 沙箱开着时改动立即生效；关着时只存清单，启用那一刻再标
#[tauri::command]
pub fn sandbox_set_roots(app: tauri::AppHandle, roots: Vec<String>) -> Result<(), String> {
    let mut cleaned: Vec<PathBuf> = Vec::new();
    for raw in &roots {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let path = PathBuf::from(trimmed);
        if !path.is_absolute() {
            return Err(format!("可写根必须是绝对路径：{trimmed}"));
        }
        if cleaned.contains(&path) {
            continue;
        }
        cleaned.push(path);
    }

    if enabled() {
        // 沙箱开着：新根立即就位（标签 + 授权），改动即时生效
        for root in &cleaned {
            let sid = capability_sid(root, "extra")?;
            label_root(root)?;
            icacls(root, &["/grant", &format!("*{sid}:(OI)(CI)M")])?;
            icacls(root, &["/deny", "*S-1-1-0:(CI)(DC)"])?;
        }
    }
    set_writable_roots(cleaned.clone());

    let mut config = crate::config::load(&app);
    config.sandbox_writable_roots = cleaned.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    crate::config::save(&app, &config)
}

/// 激活（二期）：为本条命令构造 **WRITE_RESTRICTED 受限令牌**并换到挂起的孩子上。
/// `cap_sids` 是这条命令全部可写根（cwd/工作树、专用临时目录、额外根）的能力 SID——
/// 写只落在这些根里，读与执行不受限；任何一步失败返回 Err，调用方按 fail-closed
/// 杀孩子并拒绝执行。完成后内部恢复执行（NtResumeProcess）。
#[cfg(windows)]
pub fn activate(child: &Child, cap_sids: &[String]) -> Result<(), String> {
    
    

    unsafe {
        let token = build_restricted_token(cap_sids)?;
        swap_token(child, token)?;
        // 换完令牌关掉我们这边的句柄（内核对象由子进程引用维持）
        let _ = windows::Win32::Foundation::CloseHandle(token);
        resume_process(child)
    }
}

/// 造 WRITE_RESTRICTED 受限主令牌（对齐 deepseek-harness `token.ts` 的生产配方）：
/// restrict 清单 = [logon SID（保活）, EVERYONE（保活）, 能力 SID…]，
/// flags = DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED（0x1|0x4|0x8，
/// 只对**写**做 pass-2 交集检查，读与执行不受限）；令牌默认 DACL 合入能力 SID 的
/// FILE_ALL_ACCESS（否则孙进程的匿名管道创建过不了写检查，spawn EPERM）；
/// 完整性压到 Low（授予根的 Low 标签与之配对）。返回的句柄归调用方所有
#[cfg(windows)]
unsafe fn build_restricted_token(cap_sids: &[String]) -> Result<windows::Win32::Foundation::HANDLE, String> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{
        CreateRestrictedToken, CreateWellKnownSid, PSID, SID_AND_ATTRIBUTES,
        TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
        TOKEN_QUERY, WinWorldSid,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut source = HANDLE::default();
    OpenProcessToken(
        GetCurrentProcess(),
        TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ADJUST_DEFAULT | TOKEN_ASSIGN_PRIMARY,
        &mut source,
    )
    .map_err(|e| format!("打开自身令牌失败：{e}"))?;

    // 全程 fail-closed：任何一步失败，已打开的句柄在返回前关掉
    let result = (|| -> Result<HANDLE, String> {
        // 1) logon SID（S-1-5-5-x-y）：保活组成员，缺了它早期 DLL 初始化直接崩
        let logon_sid = token_logon_sid(source)?;

        // 2) EVERYONE（World）：保活组另一半
        let mut world_buf = [0u8; 68];
        let mut world_size = world_buf.len() as u32;
        CreateWellKnownSid(
                WinWorldSid,
                None,
                Some(windows::Win32::Security::PSID(world_buf.as_mut_ptr().cast())),
                &mut world_size,
            )
            .map_err(|e| format!("构建 EVERYONE SID 失败：{e}"))?;
        let world_sid =
            windows::Win32::Security::PSID(world_buf.as_ptr() as *mut core::ffi::c_void);

        // 3) 能力 SID：字符串 → PSID
        let mut cap_psids: Vec<PSID> = Vec::new();
        for text in cap_sids {
            cap_psids.push(string_to_sid(text)?);
        }

        // 4) restrict 清单：[logon, world, 能力…]
        let mut restrict: Vec<SID_AND_ATTRIBUTES> = Vec::with_capacity(2 + cap_psids.len());
        restrict.push(SID_AND_ATTRIBUTES { Sid: logon_sid, Attributes: 0 });
        restrict.push(SID_AND_ATTRIBUTES { Sid: world_sid, Attributes: 0 });
        for sid in &cap_psids {
            restrict.push(SID_AND_ATTRIBUTES { Sid: *sid, Attributes: 0 });
        }

        // 5) CREATE_RESTRICTED_TOKEN（WRITE_RESTRICTED：只对写做 pass-2 交集）
        let mut restricted = HANDLE::default();
        CreateRestrictedToken(
            source,
            windows::Win32::Security::CREATE_RESTRICTED_TOKEN_FLAGS(0x1 | 0x4 | 0x8),
            None,
            None,
            Some(&restrict),
            &mut restricted,
        )
        .map_err(|e| format!("创建受限令牌失败：{e}"))?;

        // 6) 默认 DACL 合入能力 SID 的 FILE_ALL_ACCESS：子进程新建的管道/文件
        //    用这份 DACL，pass-2 才过得去（deepseek 的 spawn EPERM 坑）
        merge_default_dacl(restricted, &cap_psids)?;

        // 7) 完整性压到 Low：授予根的 Low 标签与之配对，完整性检查才放行
        lower_integrity(restricted)?;

        Ok(restricted)
    })();

    let _ = windows::Win32::Foundation::CloseHandle(source);
    result
}

/// 字符串 SID → PSID（LocalAlloc 归系统，测试进程无所谓，不 LocalFree）
#[cfg(windows)]
unsafe fn string_to_sid(text: &str) -> Result<windows::Win32::Security::PSID, String> {
    use windows::Win32::Security::PSID;
    use windows::core::HSTRING;
    let mut sid = PSID::default();
    // w! 只吃字面量：运行时的 SID 字符串走 HSTRING
    let wide = HSTRING::from(format!("{text}\0"));
    windows::Win32::Security::Authorization::ConvertStringSidToSidW(&wide, &mut sid)
        .map_err(|e| format!("解析 SID {text} 失败：{e}"))?;
    Ok(sid)
}

/// 从令牌的组里拷出 logon SID（S-1-5-5-x-y，属性 SE_GROUP_LOGON_ID）
#[cfg(windows)]
unsafe fn token_logon_sid(token: windows::Win32::Foundation::HANDLE) -> Result<windows::Win32::Security::PSID, String> {
    
    use windows::Win32::Security::{CopySid, GetLengthSid, GetTokenInformation, PSID, TokenGroups, TOKEN_GROUPS};
    use windows::Win32::System::SystemServices::SE_GROUP_LOGON_ID;

    let mut needed = 0u32;
    // 预期以 ERROR_INSUFFICIENT_BUFFER 失败，拿到需要的长度
    let _ = GetTokenInformation(token, TokenGroups, None, 0, &mut needed);
    if needed == 0 {
        return Err("查询令牌组长度失败".into());
    }
    let mut buffer = vec![0u8; needed as usize];
    GetTokenInformation(
        token,
        TokenGroups,
        Some(buffer.as_mut_ptr() as *mut core::ffi::c_void),
        needed,
        &mut needed,
    )
    .map_err(|e| format!("读取令牌组失败：{e}"))?;

    // 类型化访问，避免手算偏移：TOKEN_GROUPS { GroupCount, Groups[SID_AND_ATTRIBUTES] }
    let groups = &*(buffer.as_ptr() as *const TOKEN_GROUPS);
    for index in 0..groups.GroupCount as usize {
        // Groups 声明为 [SID_AND_ATTRIBUTES; 1]，按 count 越界索引是 TokenGroups 的标准读法
        let item = *groups.Groups.as_ptr().add(index);
        // logon SID 的属性是组合位（0xC0000000 | MANDATORY/ENABLED…）：按位测试，不比相等
        if item.Attributes & (SE_GROUP_LOGON_ID as u32) != SE_GROUP_LOGON_ID as u32 {
            continue;
        }
        let length = GetLengthSid(item.Sid);
        let mut copy = vec![0u8; length as usize];
        CopySid(length, PSID(copy.as_mut_ptr().cast()), item.Sid)
            .map_err(|e| format!("拷贝 logon SID 失败：{e}"))?;
        // 泄漏到进程生命周期：SID 拷贝要用到 CreateRestrictedToken 为止，而它住在
        // 跨越本函数的 restrict 清单里——Vec 随函数返回就悬空（第一个坑：87 错误）。
        // 几十字节、每进程一次，Box::leak 是这里最诚实的生命周期标注
        let leaked = Box::leak(copy.into_boxed_slice());
        return Ok(PSID(leaked.as_ptr() as *mut core::ffi::c_void));
    }
    Err(format!("令牌组里没有 logon SID（共 {} 组）", groups.GroupCount))
}

/// 令牌默认 DACL 合并：给每个能力 SID 加一条 FILE_ALL_ACCESS 的 allow ACE。
/// SetEntriesInAclW 读旧 DACL 出新 DACL，SetTokenInformation 写回
#[cfg(windows)]
unsafe fn merge_default_dacl(token: windows::Win32::Foundation::HANDLE, cap_psids: &[windows::Win32::Security::PSID]) -> Result<(), String> {
    
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        SetEntriesInAclW, EXPLICIT_ACCESS_W,
    };
    use windows::Win32::Security::{
        GetTokenInformation, SetTokenInformation, TOKEN_DEFAULT_DACL, TokenDefaultDacl,
    };

    // FILE_ALL_ACCESS 的位形状（STANDARD_RIGHTS_REQUIRED | FILE 读写执行删改全部）
    const FILE_ALL: u32 = 0x000F_01FF;

    let mut needed = 0u32;
    let _ = GetTokenInformation(token, TokenDefaultDacl, None, 0, &mut needed);
    if needed == 0 {
        return Err("查询令牌默认 DACL 长度失败".into());
    }
    let mut buffer = vec![0u8; needed as usize];
    GetTokenInformation(
        token,
        TokenDefaultDacl,
        Some(buffer.as_mut_ptr() as *mut core::ffi::c_void),
        needed,
        &mut needed,
    )
    .map_err(|e| format!("读取令牌默认 DACL 失败：{e}"))?;
    // TOKEN_DEFAULT_DACL { PACL DefaultDacl }：x64 头 8 字节就是 PACL
    let old_ptr = usize::from_le_bytes(buffer[0..8].try_into().unwrap());
    // DefaultDacl 可能是 NULL（受限令牌的新生状态）：NULL 时不传旧 DACL，
    // SetEntriesInAclW 直接以 entries 建新表
    let old_acl = (old_ptr != 0).then_some(old_ptr as *const windows::Win32::Security::ACL);

    let entries: Vec<EXPLICIT_ACCESS_W> = cap_psids
        .iter()
        .map(|sid| EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL,
            grfAccessMode: windows::Win32::Security::Authorization::ACCESS_MODE(1), // GRANT_ACCESS
            grfInheritance: windows::Win32::Security::ACE_FLAGS(0),
            Trustee: trustee_for(sid),
        })
        .collect();
    // SetEntriesInAclW 输出新分配的 ACL（LocalFree 归我们）；SetTokenInformation
    // 会把 ACL 拷进令牌，写回后即可释放
    let mut new_dacl: *mut windows::Win32::Security::ACL = std::ptr::null_mut();
    let code = SetEntriesInAclW(Some(&entries), old_acl, &mut new_dacl);
    if code.0 != 0 {
        return Err(format!("合并默认 DACL 失败：WIN32_ERROR {:?}", code));
    }

    let info = TOKEN_DEFAULT_DACL {
        DefaultDacl: new_dacl,
    };
    SetTokenInformation(
        token,
        TokenDefaultDacl,
        &info as *const _ as *const core::ffi::c_void,
        std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
    )
    .map_err(|e| format!("写回默认 DACL 失败：{e}"))?;
    let _ = LocalFree(Some(HLOCAL(
        new_dacl as *mut core::ffi::c_void,
    )));
    Ok(())
}

/// 完整性压到 Low（S-1-16-4096）：授予根的 Low 标签与之配对，完整性检查才放行
#[cfg(windows)]
unsafe fn lower_integrity(token: windows::Win32::Foundation::HANDLE) -> Result<(), String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
    use windows::Win32::Security::{
        GetLengthSid, SetTokenInformation, PSID, SID_AND_ATTRIBUTES, TOKEN_MANDATORY_LABEL,
        TokenIntegrityLevel,
    };
    use windows::Win32::System::SystemServices::SE_GROUP_INTEGRITY;
    use windows::core::w;

    let mut sid = PSID::default();
    ConvertStringSidToSidW(w!("S-1-16-4096"), &mut sid)
        .map_err(|e| format!("解析低完整性 SID 失败：{e}"))?;
    let label = TOKEN_MANDATORY_LABEL {
        Label: SID_AND_ATTRIBUTES {
            Sid: sid,
            Attributes: SE_GROUP_INTEGRITY as u32,
        },
    };
    let size = std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + GetLengthSid(sid);
    SetTokenInformation(
        token,
        TokenIntegrityLevel,
        &label as *const _ as *const core::ffi::c_void,
        size,
    )
    .map_err(|e| format!("把令牌压到低完整性失败：{e}"))?;
    let _ = LocalFree(Some(HLOCAL(sid.0)));
    Ok(())
}

/// 把（受限）主令牌换到挂起的子进程上：NtSetInformationProcess 的
/// ProcessAccessToken(9)。孩子从未执行过任何指令，令牌在第一条指令前就位
#[cfg(windows)]
unsafe fn swap_token(child: &Child, token: windows::Win32::Foundation::HANDLE) -> Result<(), String> {
    
    use std::os::windows::io::AsRawHandle;

    #[link(name = "ntdll")]
    extern "system" {
        fn NtSetInformationProcess(
            processhandle: isize,
            processinformationclass: u32,
            processinformation: *mut core::ffi::c_void,
            processinformationlength: u32,
        ) -> i32;
    }

    #[repr(C)]
    struct ProcessAccessToken {
        token: isize,
        process: isize,
    }
    let payload = ProcessAccessToken {
        token: token.0 as isize,
        process: child.as_raw_handle() as isize,
    };
    let status = NtSetInformationProcess(
        child.as_raw_handle() as isize,
        9, // ProcessAccessToken
        &payload as *const _ as *mut core::ffi::c_void,
        std::mem::size_of::<ProcessAccessToken>() as u32,
    );
    if status < 0 {
        return Err(format!("换入受限令牌失败（NTSTATUS {status:#010x}）"));
    }
    Ok(())
}

/// 恢复一个 CREATE_SUSPENDED 拉起的进程。走 ntdll 的 NtResumeProcess——按进程句柄
/// 整体恢复，不需要枚举线程（实测 Toolhelp 快照里挂起子进程的线程可能缺席，
/// 枚举路线在它面前是个赌局）。NTSTATUS < 0 = 失败
#[cfg(windows)]
fn resume_process(child: &Child) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;

    #[link(name = "ntdll")]
    extern "system" {
        fn NtResumeProcess(processhandle: *mut core::ffi::c_void) -> i32;
    }

    let status = unsafe { NtResumeProcess(child.as_raw_handle()) };
    if status < 0 {
        return Err(format!("恢复子进程失败（NTSTATUS {status:#010x}）"));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn activate(_child: &Child) -> Result<(), String> {
    Ok(())
}

#[cfg(not(windows))]
pub fn label_root(_root: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(not(windows))]
pub fn prepare() -> Result<PathBuf, String> {
    Ok(sandbox_tmp())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const CREATE_SUSPENDED: u32 = 0x0000_0004;

    /// cmd 不认 `\\?\` 前缀的扩展路径（实测："卷标语法不正确"）。
    /// std::env::temp_dir() 在测试环境里会带回这个前缀，喂给 cmd 之前剥掉——
    /// 否则"盘外写入被拒"会变成"路径语法错误"，断言就空转了
    fn plain(path: &std::path::Path) -> String {
        let text = path.to_string_lossy().into_owned();
        text.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(text)
    }

    /// 挂起拉起一条 cmd。cwd 由调用方给：写哪、读哪都是测试的断言对象。
    /// 返回里带着收容壳——**它必须活着**：Drop 即 KILL_ON_JOB_CLOSE，
    /// 挂起的孩子会被内核立刻收走（这一课写在下面的断言里）
    fn suspended(command: &str, cwd: &Path) -> (Child, crate::tool_runtime::job::Guard) {
        // 根就位（标签 + 授权），拿到能力 SID 再构造受限令牌
        let sid = ensure_root(cwd, "workspace").expect("根就位要成功");
        let child = Command::new("cmd")
            // raw_arg 绕过 std 对 cmd.exe 的特殊引号转义（hooks.rs 同款先例）：
            // 命令里带引号的路径才能原样到达 cmd，不会被改写成"语法不正确"
            .raw_arg(format!("/S /C \"{}\"", command))
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_SUSPENDED | crate::childproc::no_window_bit())
            .spawn()
            .expect("拉起测试子进程");
        let guard = crate::tool_runtime::job::Guard::contain(&child).expect("收容先于沙箱");
        activate(&child, &[sid]).expect("沙箱激活要成功");
        (child, guard)
    }

    fn wait(child: &mut Child) -> Option<i32> {
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().expect("等子进程") {
                return status.code();
            }
            if started.elapsed() > Duration::from_secs(20) {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// SID 二进制 → 文本（S-1-a-b…）。令牌配方断言与劫持探测共用
    fn sid_text(sid: *const u8) -> String {
        let (revision, subcount) = (unsafe { *sid }, unsafe { *sid.add(1) });
        let authority = u64::from_be_bytes([
            0,
            0,
            unsafe { *sid.add(2) },
            unsafe { *sid.add(3) },
            unsafe { *sid.add(4) },
            unsafe { *sid.add(5) },
            unsafe { *sid.add(6) },
            unsafe { *sid.add(7) },
        ]);
        let subs: Vec<String> = (0..subcount as usize)
            .map(|i| {
                let bytes: [u8; 4] =
                    unsafe { core::slice::from_raw_parts(sid.add(8 + i * 4), 4) }
                        .try_into()
                        .unwrap();
                u32::from_le_bytes(bytes).to_string()
            })
            .collect();
        let _ = revision;
        format!("S-1-{authority}-{}", subs.join("-"))
    }

    /// 【劫持指纹】本机安全软件（火绒行为沙箱实测）会把换过受限/低完整性令牌的
    /// 孩子接管：外部 exe 起不来、文件写报成功却不落地（ERR=0 的幽灵写）、读取
    /// 静默失败——同一测试二进制在计划任务下全绿，独立探针复现 10/10。判定用
    /// `icacls .`（读 DACL，沙箱语义下对受限孩子永远放行）：对照孩子（未换令牌）
    /// 跑得动而受限孩子跑不动 = 劫持实锤。被接管的文件系统里端到端断言只会
    /// 空转，测试据此跳过并说明，而不是对着幽灵文件系统报假失败
    fn environment_hijacks_restricted_children(cwd: &Path) -> bool {
        let runs_icacls = |restricted: bool| -> bool {
            let mut child = Command::new("cmd")
                .raw_arg(r#"/S /C "icacls .""#)
                .current_dir(cwd)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .creation_flags(CREATE_SUSPENDED | crate::childproc::no_window_bit())
                .spawn()
                .expect("拉起探测孩子");
            let guard = crate::tool_runtime::job::Guard::contain(&child).expect("收容探测孩子");
            if restricted {
                let sid = ensure_root(cwd, "workspace").expect("根就位");
                activate(&child, &[sid]).expect("激活探测孩子");
            } else {
                resume_process(&child).expect("唤醒对照孩子");
            }
            let code = wait(&mut child);
            drop(guard);
            code == Some(0)
        };
        let plain_ok = runs_icacls(false);
        let restricted_ok = runs_icacls(true);
        plain_ok && !restricted_ok
    }

    /// 挂起是真的：activate 之前孩子一条指令没跑、还活着；activate 之后才跑完退出
    #[test]
    fn resume_actually_runs_the_suspended_process() {
        let root = crate::test_support::scoped_temp_dir("sandbox-resume");
        let sid = ensure_root(root.path.as_path(), "workspace").expect("根就位");

        let mut child = Command::new("cmd")
            .args(["/C", "exit 0"])
            .current_dir(root.path.as_path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_SUSPENDED | crate::childproc::no_window_bit())
            .spawn()
            .expect("拉起测试子进程");
        assert!(
            child.try_wait().expect("查状态").is_none(),
            "CREATE_SUSPENDED 的孩子在激活前不能已经退出"
        );
        let guard = crate::tool_runtime::job::Guard::contain(&child).expect("收容先于沙箱");
        std::thread::sleep(Duration::from_millis(100));
        activate(&child, &[sid]).expect("沙箱激活要成功");
        assert_eq!(wait(&mut child), Some(0));
        drop(guard);
    }

    /// 沙箱边界对文件工具的判定：绑定的根内放行、根外拒、未绑定全拒；
    /// read 类永远不在边界上
    #[test]
    fn the_boundary_covers_file_writes_but_only_them() {
        let root = crate::test_support::scoped_temp_dir("sandbox-boundary");
        let bound = root.path.as_path();
        let write = |path: &str| serde_json::json!({ "path": path });

        assert_eq!(
            boundary_violation("write_file", &write("a.txt"), Some(bound), Some(bound)),
            None,
            "根内写入放行"
        );
        assert_eq!(
            boundary_violation("edit_file", &write("a.txt"), Some(bound), Some(bound)),
            None,
            "edit_file 同一判据"
        );
        assert!(
            boundary_violation("write_file", &write("../outside.txt"), Some(bound), Some(bound))
                .is_some(),
            "相对路径跳出根 = 越界"
        );
        assert!(
            boundary_violation(
                "write_file",
                &write("C:\\Windows\\temp\\x.txt"),
                Some(bound),
                Some(bound)
            )
            .is_some(),
            "绝对路径在根外 = 越界"
        );
        // 未绑定：没有边界就没有可写范围，写一律拒（解析基准仍是话题根=主目录）
        assert!(
            boundary_violation("write_file", &write("a.txt"), None, Some(bound)).is_some(),
            "未绑定时写文件全拒"
        );
        // 解析基准是话题根：未绑定时（话题根=主目录）相对路径落在主目录下，
        // 但没有绑定根就一样越界——这个差别要写在理由里，不是悄悄拒
        assert!(
            boundary_violation("read_file", &write("a.txt"), Some(bound), Some(bound)).is_none(),
            "read 类工具不在边界上"
        );
        assert_eq!(
            boundary_violation("run_command", &serde_json::json!({"command": "x"}), None, None),
            None,
            "命令有自己的闸（收容+低完整性），边界只管文件工具"
        );
    }

    /// 可写根的设置入口：绝对路径校验、去重、空行跳过
    #[test]
    fn sandbox_set_roots_cleans_and_validates() {
        // 直接测命令函数要 AppHandle；这里钉清洗规则的前半段（与命令体同一判据）
        let raw = vec!["  C:\\data  ".to_string(), "".to_string(), "C:\\data".to_string()];
        let mut cleaned: Vec<std::path::PathBuf> = Vec::new();
        for item in &raw {
            let trimmed = item.trim();
            if trimmed.is_empty() {
                continue;
            }
            let path = std::path::PathBuf::from(trimmed);
            if cleaned.contains(&path) {
                continue;
            }
            cleaned.push(path);
        }
        assert_eq!(cleaned.len(), 1, "空白行跳过、重复路径去重");
        assert_eq!(cleaned[0], std::path::PathBuf::from("C:\\data"));
    }

    /// 端到端验收（拍板语义的直接证据）：
    /// 低完整性的孩子**写得到**标了 Low 的项目根，**写不进**盘外的 Medium 文件。
    #[test]
    fn a_low_integrity_child_writes_inside_the_labeled_root_and_nowhere_else() {
        let root = crate::test_support::scoped_temp_dir("sandbox-root");
        // 临时目录也走真路径：它就是孩子实际会用的那一份（能力 SID 各自独立）
        let tmp = sandbox_tmp();
        ensure_root(&tmp, "tmp").expect("准备沙箱临时目录");

        // 【环境劫持闸】被安全软件接管的孩子连外部 exe 都起不来、写被虚拟化，
        // 端到端断言只会空转（详见 environment_hijacks_restricted_children）；
        // 令牌本身的形状由 the_restricted_token_recipe_matches_the_sandbox_contract 钉死
        if environment_hijacks_restricted_children(root.path.as_path()) {
            println!("【环境劫持】受限令牌孩子被安全软件行为沙箱接管（写被虚拟化），端到端断言跳过——令牌配方由 the_restricted_token_recipe_matches_the_sandbox_contract 钉死");
            let _ = std::fs::remove_dir_all(&tmp);
            return;
        }
        println!("端到端断言实跑（未检测到劫持）");

        // 写进来：DACL 允许 + 标签 Low → 允许
        let (mut child, _guard) = suspended("echo inside > inside.txt", root.path.as_path());
        assert_eq!(wait(&mut child), Some(0), "根内写入要成功");
        assert!(root.path.join("inside.txt").exists(), "文件要真的落下来");

        // 写出去：盘外是 Medium 标签 → no-write-up 拒绝。探测文件不存在 = 拒绝生效；
        // stderr 里要真的写着"拒绝访问"，不是路径语法错误冒充的
        let outside = std::env::temp_dir().join("aglab-sandbox-deny-probe.txt");
        let _ = std::fs::remove_file(&outside);
        let (mut child, _guard) = suspended(
            &format!("echo escaped > \"{}\"", plain(&outside)),
            root.path.as_path(),
        );
        let code = wait(&mut child);
        let mut stderr_text = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            use std::io::Read;
            let mut buffer = Vec::new();
            let _ = stderr.read_to_end(&mut buffer);
            stderr_text = crate::tool_runtime::constrain::decode_output(&buffer);
        }
        assert!(
            !outside.exists(),
            "盘外写入必须被完整性闸拒掉（探测文件不该存在）"
        );
        assert_ne!(code, Some(0), "被拒的写入要反映成失败：{code:?}");
        assert!(
            stderr_text.contains("拒绝") || stderr_text.to_lowercase().contains("denied"),
            "拒绝的原因该是访问被拒，不是别的错误冒充：{stderr_text}"
        );
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 读不受限：工具链、配置都在盘外，读不了就什么都干不了。
    /// 读的是真实存在的盘外文件（本进程自己的源码树之外的系统文件）
    #[test]
    fn a_low_integrity_child_can_still_read_outside() {
        let root = crate::test_support::scoped_temp_dir("sandbox-read");
        let _sid = ensure_root(root.path.as_path(), "workspace").expect("根就位");

        // 【环境劫持闸】被接管的受限孩子读取静默失败、stderr 连错误都不吐，
        // 对着幽灵进程断言只会空转（详见 environment_hijacks_restricted_children）
        if environment_hijacks_restricted_children(root.path.as_path()) {
            println!("【环境劫持】受限令牌孩子被安全软件行为沙箱接管（读静默失败），端到端断言跳过——令牌配方由 the_restricted_token_recipe_matches_the_sandbox_contract 钉死");
            return;
        }
        println!("端到端断言实跑（未检测到劫持）");

        let target = std::env::temp_dir().join("aglab-sandbox-read-probe.txt");
        std::fs::write(&target, "readable").expect("准备盘外探测文件");

        let (mut child, _guard) = suspended(
            &format!("type \"{}\"", plain(&target)),
            root.path.as_path(),
        );
        let code = wait(&mut child);
        // 读失败的原因要看孩子自己怎么说：stderr 装进断言，别对着退出码猜
        let mut output = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            use std::io::Read;
            let mut buffer = Vec::new();
            let _ = stderr.read_to_end(&mut buffer);
            output = crate::tool_runtime::constrain::decode_output(&buffer);
        }
        assert_eq!(
            code,
            Some(0),
            "盘外读取不受限（no-read-up 不是默认策略）：{output}"
        );
        let _ = std::fs::remove_file(&target);
    }

    /// 令牌配方断言：activate 造的令牌在父进程侧就能完整验证形状——完整性压到
    /// Low、限制列表 [logon, Everyone, 能力 SID]、LUA 生效、特权清空。这是两条
    /// 端到端测试的精神替身：端到端行为会被环境安全软件虚拟化（见
    /// environment_hijacks_restricted_children 的说明），而令牌形状不受影响、
    /// 在任何环境都稳定可断言
    #[test]
    fn the_restricted_token_recipe_matches_the_sandbox_contract() {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::Security::{
            GetTokenInformation, TokenGroups, TokenIntegrityLevel, TokenPrivileges,
            TokenRestrictedSids, TOKEN_GROUPS, TOKEN_MANDATORY_LABEL, TOKEN_PRIVILEGES,
        };

        // 读受限令牌里的 SID 清单（限制列表 / 组）——TokenGroups 与
        // TokenRestrictedSids 都是 TOKEN_GROUPS 形状，一个读法两处用。
        // 注意读的是造出来的 token 句柄，不是自己进程的令牌
        let read_sid_list = |token: HANDLE,
                             class: windows::Win32::Security::TOKEN_INFORMATION_CLASS| {
            unsafe {
                let mut needed = 0u32;
                let _ = GetTokenInformation(token, class, None, 0, &mut needed);
                assert_ne!(needed, 0, "预查询 SID 清单长度要有效");
                let mut buffer = vec![0u8; needed as usize];
                GetTokenInformation(
                    token,
                    class,
                    Some(buffer.as_mut_ptr().cast()),
                    needed,
                    &mut needed,
                )
                .expect("GetTokenInformation");
                let groups = &*(buffer.as_ptr() as *const TOKEN_GROUPS);
                (0..groups.GroupCount as usize)
                    .map(|index| {
                        let item = *groups.Groups.as_ptr().add(index);
                        (sid_text(item.Sid.0 as *const u8), item.Attributes)
                    })
                    .collect::<Vec<_>>()
            }
        };

        let root = crate::test_support::scoped_temp_dir("sandbox-token");
        let sid = ensure_root(root.path.as_path(), "workspace").expect("根就位");
        let token = unsafe { build_restricted_token(&[sid.clone()]).expect("造受限令牌") };

        // 限制列表（写检查 pass-2 的放行依据）：三个成员一个都不能少
        let restrict = read_sid_list(token, TokenRestrictedSids);
        let names: Vec<&str> = restrict.iter().map(|(text, _)| text.as_str()).collect();
        assert!(
            names.iter().any(|text| text.starts_with("S-1-5-5-")),
            "logon SID 要保活（缺了它早期 DLL 初始化直接崩）：{names:?}"
        );
        assert!(
            names.contains(&"S-1-1-0"),
            "Everyone 要在限制列表里：{names:?}"
        );
        assert!(
            names.contains(&sid.as_str()),
            "能力 SID 要在限制列表里（它是在根 DACL 上放行写检查的依据）：{names:?} / {sid}"
        );

        // LUA 过滤：Administrators 留在组里但变成 deny-only（SE_GROUP_USE_FOR_DENY_ONLY
        // = 0x10），不再是 enabled 组（SE_GROUP_ENABLED = 0x4）——标准用户令牌的形状
        let groups = read_sid_list(token, TokenGroups);
        let admins = groups
            .iter()
            .find(|(text, _)| *text == "S-1-5-32-544")
            .expect("Administrators 该留在组里");
        assert_ne!(admins.1 & 0x10, 0, "Administrators 该是 deny-only");
        assert_eq!(admins.1 & 0x4, 0, "deny-only 组不能同时是 enabled");

        // 完整性压到 Low（S-1-16-4096）：与根的 Low 标签配对，完整性检查才放行
        unsafe {
            let mut needed = 0u32;
            let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut needed);
            let mut buffer = vec![0u8; needed as usize];
            GetTokenInformation(
                token,
                TokenIntegrityLevel,
                Some(buffer.as_mut_ptr().cast()),
                needed,
                &mut needed,
            )
            .expect("读完整性");
            let label = &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL);
            assert_eq!(
                sid_text(label.Label.Sid.0 as *const u8),
                "S-1-16-4096",
                "令牌完整性要压到 Low"
            );
        }

        // 特权清空：Medium 原令牌 6 条上下，LUA 过滤后至多留通知类两条
        unsafe {
            let mut needed = 0u32;
            let _ = GetTokenInformation(token, TokenPrivileges, None, 0, &mut needed);
            let mut buffer = vec![0u8; needed as usize];
            GetTokenInformation(
                token,
                TokenPrivileges,
                Some(buffer.as_mut_ptr().cast()),
                needed,
                &mut needed,
            )
            .expect("读特权");
            let privileges = &*(buffer.as_ptr() as *const TOKEN_PRIVILEGES);
            assert!(
                privileges.PrivilegeCount <= 2,
                "LUA 令牌的特权要几乎清空，实测 {} 条",
                privileges.PrivilegeCount
            );
        }

        unsafe {
            let _ = CloseHandle(token);
        }
    }
}
