use std::fs;
use std::path::{Path, PathBuf};

use super::{meta_of, Conversation, ConversationMeta};

fn file_for(dir: &Path, id: &str) -> Result<PathBuf, String> {
    // 话题 id 由前端生成，落盘前再挡一次路径穿越
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("非法的话题 id。".into());
    }
    Ok(dir.join(format!("{id}.json")))
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
        .collect())
}

pub fn load_all(dir: &Path) -> Result<Vec<Conversation>, String> {
    let mut items: Vec<Conversation> = json_files(dir)?
        .iter()
        .filter_map(|path| read_conversation(path))
        .collect();
    items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(items)
}

pub fn list(dir: &Path) -> Result<Vec<ConversationMeta>, String> {
    // JSON 后端的固有代价：列表要读遍每个文件并解析正文
    Ok(load_all(dir)?.iter().map(meta_of).collect())
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
    Ok(meta_of(conversation))
}

pub fn remove(dir: &Path, id: &str) -> Result<(), String> {
    let path = file_for(dir, id)?;
    if path.exists() {
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn count(dir: &Path) -> Result<usize, String> {
    Ok(json_files(dir)?.len())
}
