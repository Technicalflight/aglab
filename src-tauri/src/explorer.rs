//! 资源管理器（分区 3）的两个 IPC：列一层目录、预览工作目录内的任意文件。
//!
//! 与 `files_suggest` 的分工：那个是**输入框的候选**（20 条、按查询打分、
//! 一次打平整个仓库）；这个是**给人看的目录树**（一次只列一层、目录在前、
//! 逐层展开）。两者的忽略规则共用 [`SEARCH_SKIP_DIRS`]——用户在输入框里
//! 看不到的东西（node_modules、target、.git…），在资源管理器里也不该看到。
//!
//! **看哪个目录**：`dir_list` 由前端点名一个项目 id（话题自己的归属优先，
//! 见前端 `lib/workspace.ts`），空则回落到应用级默认。不能让后端自己读
//! `config.active_project()`——那是应用级默认，与"我正在看的那条话题属于
//! 哪个工作目录"是两回事；切了话题而资源管理器还停在上一个目录，
//! 用户看到的就不是这条会话的现场。
//!
//! 路径判据：**读进来的必须落在某个已绑定的工作目录内**。前端送来的 `rel`
//! 与 `path` 都是它自己拼的字符串，一律当不可信输入——不设这道关，
//! `rel = "../../../etc"` 就能把工程外的文件读进来。

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use tauri::AppHandle;

use crate::config;
use crate::edits::{read_preview, EditPreview};
use crate::tools::SEARCH_SKIP_DIRS;

/// 一层目录的条目。`rel` 是正斜杠的相对路径，也是下一层 `dir_list` 的入参
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirEntryView {
    pub name: String,
    pub rel: String,
    pub abs: String,
    pub is_dir: bool,
    /// 文件字节数；目录给 0（列一层不去统计子树，那会在大仓库上卡住）
    pub bytes: u64,
}

/// 单层条目上限。十万个文件的目录不该把界面噎住；超了如实截断，
/// 前端在列表尾部说明"还有更多"，而不是悄悄少几条
const MAX_ENTRIES: usize = 2_000;

/// 解析出要浏览的工作目录根。`project_id` 为空 = 跟应用级默认走。
fn project_root(app: &AppHandle, project_id: &str) -> Result<PathBuf, String> {
    let config = config::load(app);
    let project = if project_id.trim().is_empty() {
        config.active_project()
    } else {
        config.project_by_id(project_id)
    }
    .ok_or_else(|| "还没有绑定工作目录。".to_string())?;
    fs::canonicalize(&project.path).map_err(|e| format!("工作目录读不到：{e}"))
}

/// 把相对路径关进根目录，返回规范化后的绝对路径。
///
/// 两道关缺一不可：先拒掉 `..`、盘符、根这类组件（纯字符串判断，不碰盘），
/// 再规范化后比前缀——符号链接、Windows 短名与大小写差异都在第二道现形。
///
/// 根在这里**自己再规范化一次**，不假定调用方给的是规范形式：Windows 上
/// `C:\a\b` 与 `\\?\C:\a\b` 是同一个目录的两种写法，前缀比较会当场判错。
/// 调用方给未规范化的路径是很容易犯的错（本模块的测试第一版就栽在这里），
/// 与其在每个调用点重复一次 `canonicalize`，不如把这道保险焊死在这里。
fn resolve_in_root(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(rel);
    if candidate.is_absolute()
        || candidate.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        return Err("路径越出了工作目录。".into());
    }
    let root = fs::canonicalize(root).map_err(|e| format!("工作目录读不到：{e}"))?;
    let real = fs::canonicalize(root.join(candidate)).map_err(|e| format!("路径读不到：{e}"))?;
    if !real.starts_with(&root) {
        return Err("路径越出了工作目录。".into());
    }
    Ok(real)
}

/// 列一层目录。`rel` 为空 = 工作目录根。
/// 列一层目录。`rel` 为空 = 该工作目录的根。
///
/// `project_id` 由前端点名（话题自己的归属优先，空则跟应用级默认）——
/// 切话题就换目录这件事必须由前端驱动，后端读不到"用户正在看哪条话题"。
#[tauri::command]
pub fn dir_list(
    app: AppHandle,
    project_id: String,
    rel: String,
) -> Result<Vec<DirEntryView>, String> {
    let root = project_root(&app, &project_id)?;
    let dir = if rel.trim().is_empty() {
        root.clone()
    } else {
        resolve_in_root(&root, &rel)?
    };
    if !dir.is_dir() {
        return Err("这不是一个目录。".into());
    }

    let entries = fs::read_dir(&dir).map_err(|e| format!("目录读不到：{e}"))?;
    let mut items: Vec<DirEntryView> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        // 元数据取不到的条目（断链的符号链接、没权限）直接跳过：
        // 一棵树里挂一行点不动的死项，比少一行更烦人
        let Ok(meta) = entry.metadata() else { continue };
        let is_dir = meta.is_dir();
        if is_dir && SEARCH_SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        items.push(DirEntryView {
            name,
            rel,
            abs: path.to_string_lossy().into_owned(),
            is_dir,
            bytes: if is_dir { 0 } else { meta.len() },
        });
    }
    // 目录在前、同类按名字排（不分大小写）：同一份目录每次展开都是同一个顺序，
    // 树不会因为文件系统的返回次序不同而跳一下
    items.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    items.truncate(MAX_ENTRIES);
    Ok(items)
}

/// 预览任意一个**已绑定工作目录**内的文件。点资源管理器里的一行就走这里，
/// 渲染形状与编辑台账的预览完全一致（同一份 [`read_preview`]）。
///
/// 判据是"落在某个已配置的项目里"，而不是"落在某个特定项目里"：这份预览是
/// 用户点开之后就挂在工具栏上的，切了话题它还在——它属于哪个项目不该因为
/// 切话题而失效（按项目 id 判会让切话题后的预览直接报错）。
#[tauri::command]
pub fn file_preview(app: AppHandle, path: String) -> Result<EditPreview, String> {
    let config = config::load(&app);
    if config.projects.is_empty() {
        return Err("还没有绑定工作目录。".into());
    }
    let target = fs::canonicalize(&path).map_err(|e| format!("路径读不到：{e}"))?;
    let inside = config.projects.iter().any(|project| {
        fs::canonicalize(&project.path)
            .map(|root| target.starts_with(&root))
            .unwrap_or(false)
    });
    if !inside {
        return Err("这个文件不在任何已绑定的工作目录里。".into());
    }
    read_preview(&target.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 越界判据是这个模块唯一的安全承重件，正反两面都要钉住。
    /// 根**故意**给未规范化的 `temp_dir()`：这正是第一版踩过的坑——
    /// Windows 上 canonicalize 会带上 `\\?\` 前缀，拿它跟裸路径比前缀必错
    #[test]
    fn rejects_paths_that_climb_out_of_the_root() {
        let root = std::env::temp_dir();
        for bad in ["../secret.txt", "a/../../secret.txt", "..", "/etc/passwd"] {
            assert!(
                resolve_in_root(&root, bad).is_err(),
                "{bad} 必须被拒掉，而不是解析成工作目录外的路径"
            );
        }
    }

    #[test]
    fn accepts_a_plain_relative_path_that_exists() {
        let root = std::env::temp_dir();
        let file = root.join("aglab-explorer-guard-probe.txt");
        fs::write(&file, b"ok").expect("写探针文件");
        let resolved = resolve_in_root(&root, "aglab-explorer-guard-probe.txt").expect("应当放行");
        assert!(resolved.starts_with(fs::canonicalize(&root).unwrap()));
        let _ = fs::remove_file(&file);
    }
}
