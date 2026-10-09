use std::fs;
use std::path::{Path, PathBuf};

use super::{meta_of, Conversation, ConversationMeta};

fn checked_id(id: &str) -> Result<(), String> {
    // 话题 id 由前端生成，落盘前再挡一次路径穿越
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("非法的话题 id。".into());
    }
    Ok(())
}

fn file_for(dir: &Path, id: &str) -> Result<PathBuf, String> {
    checked_id(id)?;
    Ok(dir.join(format!("{id}.json")))
}

/// 侧栏元数据脚注。侧栏只要标题/条数/预览这几格，为它们整份解析每个正文文件
/// 是白付的（语料一大 list 就卡）。保存时顺手把这几格写在旁边，list 优先读
/// 脚注；正文比脚注新（老档案、外部手改、脚注写失败）才整解析并当场补上。
/// 扩展名用 .meta.json 也要被 json_files 排除掉，不然 count/list 会把它当正文
fn meta_file_for(dir: &Path, id: &str) -> Result<PathBuf, String> {
    checked_id(id)?;
    Ok(dir.join(format!("{id}.meta.json")))
}

/// 先写临时文件再 rename：进程中途被杀不会留下半个损坏的话题。
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, text).map_err(|e| e.to_string())?;
    fs::rename(&temp, path).map_err(|e| e.to_string())?;
    Ok(())
}

fn read_conversation(path: &Path) -> Option<Conversation> {
    let text = fs::read_to_string(path).ok()?;
    let conversation = serde_json::from_str::<Conversation>(&text).ok()?;
    if conversation.id.is_empty() {
        return None;
    }
    Some(conversation)
}

fn json_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        // 还没写过任何话题时目录可能不存在，这不是错误
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };

    Ok(entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .filter(|path| {
            !path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem.ends_with(".meta"))
        })
        .collect())
}

pub fn load_all(dir: &Path) -> Result<Vec<Conversation>, String> {
    let mut items: Vec<Conversation> = json_files(dir)?
        .iter()
        .filter_map(|path| read_conversation(path))
        .collect();
    items.sort_by_key(|item| std::cmp::Reverse(item.updated_at));
    Ok(items)
}

/// 脚注比正文一样新（保存先写正文后写脚注）才可信
fn fresh_meta(data: &Path, sidecar: &Path) -> Option<ConversationMeta> {
    let data_time = fs::metadata(data).ok()?.modified().ok()?;
    let sidecar_time = fs::metadata(sidecar).ok()?.modified().ok()?;
    if sidecar_time < data_time {
        return None;
    }
    serde_json::from_str(&fs::read_to_string(sidecar).ok()?).ok()
}

pub fn list(dir: &Path) -> Result<Vec<ConversationMeta>, String> {
    let mut metas = Vec::new();
    for path in json_files(dir)? {
        let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if let Ok(meta_path) = meta_file_for(dir, id) {
            if let Some(meta) = fresh_meta(&path, &meta_path) {
                metas.push(meta);
                continue;
            }
        }
        // 脚注缺失/陈旧/损坏：整解析正文，当场补脚注。补写失败不致命——
        // 这份列表已经是对的，下次保存或下次 list 还会再试
        let Some(conversation) = read_conversation(&path) else {
            continue;
        };
        let meta = meta_of(&conversation);
        if let Ok(meta_path) = meta_file_for(dir, &meta.id) {
            if let Ok(text) = serde_json::to_string(&meta) {
                let _ = write_atomic(&meta_path, &text);
            }
        }
        metas.push(meta);
    }
    // 与 load_all 的排序口径一致（load_all 本来就是侧栏的序）
    metas.sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
    Ok(metas)
}

pub fn load(dir: &Path, id: &str) -> Result<Conversation, String> {
    let path = file_for(dir, id)?;
    read_conversation(&path).ok_or_else(|| "话题文件缺失或已损坏。".into())
}

/// 只取话题的项目归属。JSON 后端没有便宜的部分读（serde 要整份解析），
/// 这里只是把"取一个字段"的意图放进名字，调用点不必 `.map` 自己拼
pub fn load_project_id(dir: &Path, id: &str) -> Result<String, String> {
    Ok(load(dir, id)?.project_id)
}

pub fn save(dir: &Path, conversation: &Conversation) -> Result<ConversationMeta, String> {
    let text = serde_json::to_string(conversation).map_err(|e| e.to_string())?;
    write_atomic(&file_for(dir, &conversation.id)?, &text)?;
    // 脚注跟在正文后面写：后写保证脚注的 mtime 更新，list 才认。写失败不挡保存——
    // list 发现脚注陈旧会自己整解析补上
    let meta = meta_of(conversation);
    if let Ok(meta_text) = serde_json::to_string(&meta) {
        if let Ok(meta_path) = meta_file_for(dir, &conversation.id) {
            let _ = write_atomic(&meta_path, &meta_text);
        }
    }
    Ok(meta)
}

pub fn remove(dir: &Path, id: &str) -> Result<(), String> {
    let path = file_for(dir, id)?;
    if path.exists() {
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    // 脚注跟着正文走，免得留孤儿文件；失败不挡删除本身
    if let Ok(meta_path) = meta_file_for(dir, id) {
        let _ = fs::remove_file(meta_path);
    }
    Ok(())
}

pub fn count(dir: &Path) -> Result<usize, String> {
    Ok(json_files(dir)?.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aglab-json-{label}-{nanos}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn conversation(id: &str, updated_at: i64) -> Conversation {
        Conversation {
            id: id.into(),
            project_id: String::new(),
            title: format!("标题-{id}"),
            created_at: 1,
            updated_at,
            pinned: false,
            kind: "chat".to_string(),
            messages: Vec::new(),
            usage: None,
            video_nodes: Vec::new(),
            video_edges: Vec::new(),
        }
    }

    /// 脚注丢了、坏了、不存在，list 都得回到正确答案：老档案没有脚注，
    /// 这条兜底是唯一保证侧栏不缺话题的线
    #[test]
    fn list_falls_back_to_full_parse_when_sidecar_is_missing_or_broken() {
        let root = dir("fallback");
        save(&root, &conversation("a", 100)).unwrap();
        save(&root, &conversation("b", 200)).unwrap();

        // 正常路径：脚注直接命中
        let metas = list(&root).unwrap();
        assert_eq!(metas.len(), 2);
        assert_eq!(metas[0].id, "b");

        // 删光脚注：整解析兜底，结果一样
        for entry in fs::read_dir(&root).unwrap().flatten() {
            if entry
                .file_name()
                .to_str()
                .unwrap_or("")
                .ends_with(".meta.json")
            {
                fs::remove_file(entry.path()).unwrap();
            }
        }
        let metas = list(&root).unwrap();
        assert_eq!(metas.len(), 2);
        assert_eq!(metas[0].id, "b");

        // 兜底路径当场把脚注补回来
        assert!(meta_file_for(&root, "a").unwrap().exists());

        // 脚注是垃圾：不认，整解析
        fs::write(meta_file_for(&root, "a").unwrap(), "不是JSON").unwrap();
        let metas = list(&root).unwrap();
        assert_eq!(metas.len(), 2);
        assert_eq!(
            metas.iter().find(|meta| meta.id == "a").unwrap().title,
            "标题-a"
        );
    }

    /// 脚注不进 count/json_files 的视野：它不是一条话题
    #[test]
    fn sidecar_never_counts_as_a_conversation() {
        let root = dir("count");
        save(&root, &conversation("a", 1)).unwrap();
        assert_eq!(count(&root).unwrap(), 1);
        assert_eq!(list(&root).unwrap().len(), 1);

        remove(&root, "a").unwrap();
        assert_eq!(count(&root).unwrap(), 0);
        // 脚注也一并清了
        assert!(!meta_file_for(&root, "a").unwrap().exists());
    }
}
