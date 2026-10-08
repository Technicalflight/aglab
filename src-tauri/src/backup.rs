//! 自动备份（design-security-center.md D3）：写/删之前把当前文件存一份可恢复的副本。
//!
//! 与编辑台账的快照分工一句话：快照管 diff 展示（每话题首写一份、预算写死），
//! 备份管恢复（每改必存、内容没变就跳过、总量上限可配）。备份是**尽力而为**：
//! 哪一步失败都不挡原操作——挡住用户的活去保护用户的活，是本末倒置——
//! 只把"这次没备份成"写进审计，让人知道这一改没有退路。
//!
//! 副本住在 `app_data_dir/backups/<话题id>/<时刻>-<文件名>`。去重的判据在台账侧
//! （edits.rs）：上一条记录记过备份、且它的 hash_after 等于这一改动手前的指纹，
//! 内容就没变过——那份副本还在，不重复存。
//! Main 与 worker 都走 [`store_in`]（数据目录由调用方给）。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// 备份选项。每回合从配置装配一次（chat.rs），执行侧不回读配置文件
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub enabled: bool,
    /// 备份总量上限（MB），按最老先删的 LRU 清。0 = 不设上限
    pub total_mb: u32,
}

/// 存一份副本，返回落点（存成了才有）。`bytes` 是动手前读到的文件正文——
/// 判定层已经为 diff 读过一遍，这里不二次读盘。任何失败都返回 `None` 并落审计，
/// 绝不向上传错误：调用方（写/删）照常进行
/// 存一份副本，返回落点（存成了才有）。`bytes` 是动手前读到的文件正文——
/// 判定层已经为 diff 读过一遍，这里不二次读盘。任何失败都返回 `None` 并落审计，
/// 绝不向上传错误：调用方（写/删）照常进行。Main 与 worker 都走这里
/// （数据目录由调用方给：Main 从 app 派生，worker 从 CLI 传来）
pub fn store_in(
    root: &Path,
    conversation_id: &str,
    abs_path: &Path,
    bytes: &[u8],
    options: Options,
) -> Option<PathBuf> {
    if !options.enabled {
        return None;
    }
    let dir = root.join("backups").join(sanitize_component(conversation_id));
    if let Err(problem) = fs::create_dir_all(&dir) {
        note_failure_in(root, abs_path, &format!("备份目录建不出来：{problem}"));
        return None;
    }
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!(
        "{stamp}-{}",
        sanitize_component(&abs_path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default())
    );
    let dest = dir.join(name);
    if let Err(problem) = fs::write(&dest, bytes) {
        note_failure_in(root, abs_path, &format!("副本写不进去：{problem}"));
        return None;
    }
    enforce_cap(root, options.total_mb, &dest);
    Some(dest)
}

/// 备份是尽力而为：失败不挡原操作，只落一条审计账
fn note_failure_in(root: &Path, abs_path: &Path, problem: &str) {
    eprintln!("自动备份没有成（原操作照常进行）：{abs_path:?}：{problem}");
    let _ = crate::audit::record_detail(
        root,
        crate::audit::Actor::Model,
        "backup",
        &abs_path.to_string_lossy(),
        crate::audit::Outcome::Failed,
        Some(format!("这次改动没有备份：{problem}")),
    );
}

/// 总量上限：超过就按 mtime 从最老的开始删，刚写进去的这份不动。
/// 删不动就停手并说明——宁可超上限，也不为清空间把"删不掉的旧副本"伪装成删掉了
fn enforce_cap(root: &Path, total_mb: u32, keep: &Path) {
    if total_mb == 0 {
        return;
    }
    let cap = total_mb as u64 * 1024 * 1024;
    let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    let mut stack = vec![root.join("backups")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            files.push((modified, meta.len(), path));
        }
    }
    let total: u64 = files.iter().map(|(_, size, _)| size).sum();
    if total <= cap {
        return;
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    let mut remaining = total;
    for (_, size, path) in &files {
        if remaining <= cap {
            return;
        }
        if path == keep {
            continue;
        }
        if fs::remove_file(path).is_ok() {
            remaining -= size;
        }
    }
    if remaining > cap {
        eprintln!("备份总量超出上限且清不动：新副本已保留，旧的删不掉。上限 {total_mb} MB，现占 {remaining} 字节。");
    }
}

/// 文件名/话题id 里能进备份路径的字符。路径穿越在这里被拆掉：`..`、分隔符、
/// Windows 保留字符一律变下划线，再掐掉 80 字符
fn sanitize_component(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('.').trim();
    let mut out: String = trimmed.chars().take(80).collect();
    if out.is_empty() {
        out = "unnamed".into();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aglab-backup-{tag}-{}",
            std::time::SystemTime::now()
                .elapsed()
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn hostile_names_cannot_escape_or_survive_as_dots() {
        // 分隔符被拆掉之后整个名字只剩一个组件，内部的 ".." 再也成不了"上一级"；
        // 首尾的点被剥掉（Windows 不收），所以开头那两个点没了、中间的留下
        assert_eq!(sanitize_component("..\\..\\windows"), "_.._windows");
        assert_eq!(sanitize_component("...."), "unnamed", "全点的名字剥完就是空的，不能当路径");
        assert_eq!(sanitize_component("conv:bad/name*"), "conv_bad_name_");
        let long = "长".repeat(200);
        assert_eq!(sanitize_component(&long).chars().count(), 80);
        // 话题 id 常见形状原样保留
        assert_eq!(sanitize_component("conv_5a9c83f7"), "conv_5a9c83f7");
    }

    #[test]
    fn the_cap_evicts_oldest_first_across_conversations_and_never_the_fresh_copy() {
        use std::fs::FileTimes;
        let root = temp_dir("cap");
        // 两份各 700 KB：conv_a 一份（显式拨成最老）、conv_b 一份（刚写的这份不动）。
        // 上限 1 MB：两份共 1.4 MB 超限 → 最老的 conv_a 先走 → 剩 700 KB 达标
        let old = root.join("backups").join("conv_a").join("x.bin");
        let keep = root.join("backups").join("conv_b").join("x.bin");
        for path in [&old, &keep] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, vec![0u8; 700_000]).unwrap();
        }
        let file = fs::File::options().write(true).open(&old).unwrap();
        file.set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH)).unwrap();

        // 0 = 不设上限：谁都不删
        enforce_cap(&root, 0, &keep);
        assert!(old.exists() && keep.exists());

        enforce_cap(&root, 1, &keep);
        assert!(!old.exists(), "LRU 不分话题：最老的先走");
        assert!(keep.exists(), "刚写的副本不动");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_cap_keeps_evicting_until_under_the_limit() {
        use std::fs::FileTimes;
        let root = temp_dir("cap-multi");
        let dir = root.join("backups").join("conv_a");
        fs::create_dir_all(&dir).unwrap();
        // 三份各 700 KB，上限 1 MB：清到剩一份（最新的）为止
        let names = ["1", "2", "3"];
        for (index, name) in names.iter().enumerate() {
            let path = dir.join(format!("{name}.bin"));
            fs::write(&path, vec![0u8; 700_000]).unwrap();
            let file = fs::File::options().write(true).open(&path).unwrap();
            file.set_times(FileTimes::new().set_modified(
                SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(index as u64),
            )).unwrap();
        }
        let keep = dir.join("3.bin");
        enforce_cap(&root, 1, &keep);
        assert!(!dir.join("1.bin").exists() && !dir.join("2.bin").exists());
        assert!(keep.exists(), "清到达标即停，最新的那份永远在");
        fs::remove_dir_all(&root).ok();
    }
}

/// 打开备份目录（design-security-center.md D3 的"打开备份目录"入口）。
/// 目录不存在就先建——开一个空目录好过报一条"还没备份过"
#[tauri::command]
pub fn backup_open_dir(app: tauri::AppHandle) -> Result<String, String> {
    use tauri::Manager;
    let root = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let dir = root.join("backups");
    fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败：{e}", dir.display()))?;
    crate::tools::open_target(crate::tools::OpenTarget::Path(dir.clone()))
        .map_err(|e| format!("打开备份目录失败：{e}"))?;
    Ok(dir.to_string_lossy().to_string())
}
