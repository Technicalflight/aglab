//! 本地资料库：用户集中整理的文档与资料，供界面检索、供模型随时调用。
//!
//! 三条与 memory 同源的规矩：
//! 1. **一个库一个文件。** `kb-<id>.json` 整库存元数据与文档正文，原子写
//!    （tmp + rename）。删库就是删文件，整目录拷走即迁移——不存在"只有索引里有"的状态。
//! 2. **默认只写本地。** 例外只有两条，都是"用户亲手配的可信端点"：
//!    embedding/rerank 请求走服务商代理池（与模型请求同一套出口判定），
//!    OCR 走 Umi-OCR 本机服务（127.0.0.1:1224）。`kb_import_files` 读的是
//!    用户亲手选中的文件路径，与 read_attachment 同一性质。
//! 3. **不进 config.json。** 资料库是数据不是配置，配置合同（Rust↔TS 逐字段对账）
//!    不为它多背一行。

mod embed;
mod import;
mod ocr;
mod search;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager};

pub use search::{score_document, snippet, tokenize};

/// 全部写操作（以及读改写）都过这一把锁：命令在线程池里并发跑，
/// 而每个操作都是"读文件→改→写回"，没有锁就是丢更新
static LOCK: Mutex<()> = Mutex::new(());
/// 模型工具执行体没有 AppHandle（tools::execute 只收名字与参数），
/// 根目录在启动 setup 时定死在这里——与 tool_runtime::background::state() 同款全局
static TOOL_ROOT: OnceLock<PathBuf> = OnceLock::new();
/// 语义检索的配置快照：工具执行体没有 AppHandle，config 变更时经这里刷新。
/// 存整份 AppConfig——embed 客户端要出口名单、代理池与密钥解析，缺一不可
static EMBED_SNAPSHOT: OnceLock<crate::config::AppConfig> = OnceLock::new();

/// 单篇导入文件的上限。资料库是检索用的资料库，不是备份盘
const MAX_IMPORT_BYTES: u64 = 2 * 1024 * 1024;
/// 手动录入一篇文档的正文上限（字符）
const MAX_DOC_CHARS: usize = 2_000_000;
/// 单个库的文档数上限：一个 JSON 文件里的合理体量
const MAX_DOCS_PER_KB: usize = 2_000;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// id 由前端生成，落盘前再挡一次路径穿越（history/json_store 同款判据）
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct KbMeta {
    pub id: String,
    pub name: String,
    pub description: String,
    /// 空串 = 未绑定工作目录。工作目录就是 config.projects 里的一个项目
    pub project_id: String,
    pub created_at: u64,
    pub updated_at: u64,
}


#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct KbDoc {
    pub id: String,
    pub title: String,
    /// 出处：导入文件是它的路径，手动录入是「手动录入」
    pub source: String,
    pub content: String,
    pub created_at: u64,
    pub updated_at: u64,
}


/// 落盘的整份形状
#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct KbFile {
    meta: KbMeta,
    docs: Vec<KbDoc>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KbSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub project_id: String,
    pub doc_count: usize,
    pub chars: usize,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KbDocMeta {
    pub id: String,
    pub title: String,
    pub source: String,
    pub chars: usize,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KbDetail {
    #[serde(flatten)]
    pub kb: KbSummary,
    pub docs: Vec<KbDocMeta>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KbHit {
    pub kb_id: String,
    pub kb_name: String,
    pub doc_id: String,
    pub doc_title: String,
    pub snippet: String,
    pub score: f64,
    pub updated_at: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KbImportOutcome {
    pub added: usize,
    pub skipped: usize,
    /// 每一条被跳过的原因，直接展示给用户
    pub skipped_names: Vec<String>,
}

fn summary_of(meta: &KbMeta, docs: &[KbDoc]) -> KbSummary {
    KbSummary {
        id: meta.id.clone(),
        name: meta.name.clone(),
        description: meta.description.clone(),
        project_id: meta.project_id.clone(),
        doc_count: docs.len(),
        chars: docs.iter().map(|d| d.content.chars().count()).sum(),
        created_at: meta.created_at,
        updated_at: meta.updated_at,
    }
}

fn doc_meta_of(doc: &KbDoc) -> KbDocMeta {
    KbDocMeta {
        id: doc.id.clone(),
        title: doc.title.clone(),
        source: doc.source.clone(),
        chars: doc.content.chars().count(),
        created_at: doc.created_at,
        updated_at: doc.updated_at,
    }
}

// ---- 存储层（全部显式收根目录，测试不碰全局状态） ----

fn file_for(root: &Path, id: &str) -> Result<PathBuf, String> {
    if !valid_id(id) {
        return Err("非法的资料库 id。".into());
    }
    Ok(root.join(format!("kb-{id}.json")))
}

/// 先写临时文件再 rename：进程中途被杀不会留下半个损坏的库
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, text).map_err(|e| e.to_string())?;
    fs::rename(&temp, path).map_err(|e| e.to_string())?;
    Ok(())
}

/// 测试与重建路径要读整份库；对外只开这个读入口
fn read_kb(path: &Path) -> Option<KbFile> {
    let text = fs::read_to_string(path).ok()?;
    let kb = serde_json::from_str::<KbFile>(&text).ok()?;
    if kb.meta.id.is_empty() {
        return None;
    }
    Some(kb)
}

fn json_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        // 还没建过库时目录可能不存在，这不是错误
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    Ok(entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect())
}

fn save_kb(root: &Path, kb: &KbFile) -> Result<(), String> {
    fs::create_dir_all(root).map_err(|e| format!("创建资料库目录失败：{e}"))?;
    let path = file_for(root, &kb.meta.id)?;
    let text = serde_json::to_string(kb).map_err(|e| e.to_string())?;
    write_atomic(&path, &text)
}

fn load_one(root: &Path, id: &str) -> Result<KbFile, String> {
    let path = file_for(root, id)?;
    read_kb(&path).ok_or_else(|| "资料库不存在或已损坏。".into())
}

fn new_doc_id() -> String {
    // 冲突概率可忽略；就算真撞了，同库同毫秒替换的也只是同一篇正在写入的文档
    format!("d{}", (now_ms() as u128) ^ ((std::process::id() as u128) << 32))
}

// ---- 核心操作（供命令与测试共用，全部显式收 root） ----

pub fn list_at(root: &Path) -> Result<Vec<KbSummary>, String> {
    let mut items: Vec<KbSummary> = json_files(root)?
        .iter()
        .filter_map(|path| read_kb(path).map(|kb| summary_of(&kb.meta, &kb.docs)))
        .collect();
    items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.name.cmp(&b.name)));
    Ok(items)
}

pub fn create_at(root: &Path, name: &str, description: &str, project_id: &str) -> Result<KbSummary, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("资料库得有个名字。".into());
    }
    if project_id.chars().count() > 64 {
        return Err("工作目录标识不合法。".into());
    }
    let at = now_ms();
    let mut meta = KbMeta {
        id: format!("kb{}", at ^ ((std::process::id() as u64) << 24)),
        name: name.to_string(),
        description: description.trim().to_string(),
        project_id: project_id.to_string(),
        created_at: at,
        updated_at: at,
    };
    // 同毫秒建的第二个库不该顶掉第一个：撞名就再挪一格
    while file_for(root, &meta.id)?.exists() {
        meta.id = format!("{}x", meta.id);
    }
    let kb = KbFile { meta: meta.clone(), docs: Vec::new() };
    save_kb(root, &kb)?;
    Ok(summary_of(&kb.meta, &kb.docs))
}

pub fn update_at(root: &Path, id: &str, name: &str, description: &str) -> Result<KbSummary, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("资料库得有个名字。".into());
    }
    let mut kb = load_one(root, id)?;
    kb.meta.name = name.to_string();
    kb.meta.description = description.trim().to_string();
    kb.meta.updated_at = now_ms();
    save_kb(root, &kb)?;
    Ok(summary_of(&kb.meta, &kb.docs))
}

pub fn delete_at(root: &Path, id: &str) -> Result<(), String> {
    let path = file_for(root, id)?;
    if path.exists() {
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn get_at(root: &Path, id: &str) -> Result<KbDetail, String> {
    let kb = load_one(root, id)?;
    let mut docs: Vec<KbDocMeta> = kb.docs.iter().map(doc_meta_of).collect();
    docs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.title.cmp(&b.title)));
    Ok(KbDetail { kb: summary_of(&kb.meta, &kb.docs), docs })
}

fn touch_meta(meta: &mut KbMeta) {
    meta.updated_at = now_ms();
}

pub fn doc_add_at(
    root: &Path,
    id: &str,
    title: &str,
    content: &str,
    source: &str,
) -> Result<KbDocMeta, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("文档得有个标题。".into());
    }
    if content.trim().is_empty() {
        return Err("正文是空的：一篇没有内容的文档检索不到任何东西。".into());
    }
    if content.chars().count() > MAX_DOC_CHARS {
        return Err(format!("正文超过 {} 万字符，先拆一拆再入库。", MAX_DOC_CHARS / 10_000));
    }
    let mut kb = load_one(root, id)?;
    if kb.docs.len() >= MAX_DOCS_PER_KB {
        return Err(format!("这个库已有 {MAX_DOCS_PER_KB} 篇文档，先整理再添加。"));
    }
    let at = now_ms();
    let doc = KbDoc {
        id: new_doc_id(),
        title: title.to_string(),
        source: if source.trim().is_empty() { "手动录入".to_string() } else { source.trim().to_string() },
        content: content.to_string(),
        created_at: at,
        updated_at: at,
    };
    let meta = doc_meta_of(&doc);
    kb.docs.push(doc);
    touch_meta(&mut kb.meta);
    save_kb(root, &kb)?;
    Ok(meta)
}

pub fn doc_update_at(
    root: &Path,
    id: &str,
    doc_id: &str,
    title: &str,
    content: &str,
) -> Result<KbDocMeta, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("文档得有个标题。".into());
    }
    if content.trim().is_empty() {
        return Err("正文是空的：一篇没有内容的文档检索不到任何东西。".into());
    }
    if content.chars().count() > MAX_DOC_CHARS {
        return Err(format!("正文超过 {} 万字符，先拆一拆再入库。", MAX_DOC_CHARS / 10_000));
    }
    let mut kb = load_one(root, id)?;
    let doc = kb
        .docs
        .iter_mut()
        .find(|doc| doc.id == doc_id)
        .ok_or_else(|| "文档不存在，可能已被删除。".to_string())?;
    doc.title = title.to_string();
    doc.content = content.to_string();
    doc.updated_at = now_ms();
    let meta = doc_meta_of(doc);
    touch_meta(&mut kb.meta);
    save_kb(root, &kb)?;
    Ok(meta)
}

pub fn doc_delete_at(root: &Path, id: &str, doc_id: &str) -> Result<(), String> {
    let mut kb = load_one(root, id)?;
    let before = kb.docs.len();
    kb.docs.retain(|doc| doc.id != doc_id);
    if kb.docs.len() == before {
        return Err("文档不存在，可能已被删除。".into());
    }
    touch_meta(&mut kb.meta);
    save_kb(root, &kb)
}

pub fn doc_get_at(root: &Path, id: &str, doc_id: &str) -> Result<KbDoc, String> {
    let kb = load_one(root, id)?;
    kb.docs
        .into_iter()
        .find(|doc| doc.id == doc_id)
        .ok_or_else(|| "文档不存在，可能已被删除。".into())
}

pub fn search_at(
    root: &Path,
    query: &str,
    project_id: Option<&str>,
    limit: usize,
) -> Result<Vec<KbHit>, String> {
    let tokens = tokenize(query);
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    let mut hits: Vec<KbHit> = Vec::new();
    for path in json_files(root)? {
        let Some(kb) = read_kb(&path) else { continue };
        if let Some(wanted) = project_id.filter(|p| !p.is_empty()) {
            if kb.meta.project_id != wanted {
                continue;
            }
        }
        for doc in &kb.docs {
            let Some((score, at)) = score_document(&doc.title, &doc.content, &tokens) else {
                continue;
            };
            hits.push(KbHit {
                kb_id: kb.meta.id.clone(),
                kb_name: kb.meta.name.clone(),
                doc_id: doc.id.clone(),
                doc_title: doc.title.clone(),
                snippet: snippet(&doc.content, at, 120),
                score,
                updated_at: doc.updated_at,
            });
        }
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.updated_at.cmp(&a.updated_at))
    });
    hits.truncate(limit.max(1));
    Ok(hits)
}

/// 导入用户亲手选中的文件。跳过而非中断：一个坏文件不该挡住其余的好文件
pub fn import_files_at(root: &Path, id: &str, paths: &[String]) -> Result<KbImportOutcome, String> {
    // 独立小函数而不是闭包：闭包会整段循环持有 &mut outcome，
    // 与中途的 outcome.added += 1 打架（E0499）
    fn reject(outcome: &mut KbImportOutcome, name: &str, why: &str) {
        outcome.skipped += 1;
        outcome.skipped_names.push(format!("{name}：{why}"));
    }
    let ocr = EMBED_SNAPSHOT.get().cloned().unwrap_or_default().ocr;
    let mut outcome = KbImportOutcome { added: 0, skipped: 0, skipped_names: Vec::new() };
    for raw in paths {
        let path = Path::new(raw);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| raw.clone());
        let meta = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(e) => {
                reject(&mut outcome, &name, &format!("读不到（{e}）"));
                continue;
            }
        };
        if !meta.is_file() {
            reject(&mut outcome, &name, "不是文件");
            continue;
        }
        // 文本提取交给 import 模块：纯文本照旧直读，docx/xlsx/pptx 就地解，
        // PDF/图片走 Umi-OCR（没配引擎时给人话原因，跳过该文件继续导其余的）
        let (content, method) = match import::extract_file(path, &ocr) {
            Ok(extracted) => extracted,
            Err(why) => {
                reject(&mut outcome, &name, &why);
                continue;
            }
        };
        if content.trim().is_empty() {
            reject(&mut outcome, &name, "内容是空的");
            continue;
        }
        let source = format!("{raw} · {method}");
        doc_add_at(root, id, &name, &content, &source).map_err(|e| format!("{name}：{e}"))?;
        outcome.added += 1;
    }
    Ok(outcome)
}

// ---- 全局根目录（模型工具执行体用） ----

/// 启动时定一次根目录：工具执行体没有 AppHandle，只能从这里拿
pub fn init_root(app: &AppHandle) {
    if let Ok(dir) = root_dir(app) {
        let _ = TOOL_ROOT.set(dir);
    }
    on_config_changed(&crate::config::load(app));
}

/// 配置变更时刷新语义检索的快照（config_patch 与启动 setup 各调一次）。
/// 工具执行体没有 AppHandle，只能吃这里准备好的静态快照
pub fn on_config_changed(config: &crate::config::AppConfig) {
    let _ = EMBED_SNAPSHOT.set(config.clone());
}

fn root_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("knowledge"))
}

// ---- 命令（thin：锁 + root 解析 + 委托核心操作） ----

#[tauri::command]
pub fn kb_list(app: AppHandle) -> Result<Vec<KbSummary>, String> {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    list_at(&root_dir(&app)?)
}

#[tauri::command]
pub fn kb_create(app: AppHandle, name: String, description: String, project_id: Option<String>) -> Result<KbSummary, String> {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    create_at(&root_dir(&app)?, &name, &description, project_id.as_deref().unwrap_or(""))
}

#[tauri::command]
pub fn kb_update(app: AppHandle, id: String, name: String, description: String) -> Result<KbSummary, String> {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    update_at(&root_dir(&app)?, &id, &name, &description)
}

#[tauri::command]
pub fn kb_delete(app: AppHandle, id: String) -> Result<(), String> {
    {
        let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        delete_at(&root_dir(&app)?, &id)
    }?;
    embed::schedule_delete(&app, &id, None);
    Ok(())
}

#[tauri::command]
pub fn kb_get(app: AppHandle, id: String) -> Result<KbDetail, String> {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    get_at(&root_dir(&app)?, &id)
}

#[tauri::command]
pub fn kb_doc_add(
    app: AppHandle,
    id: String,
    title: String,
    content: String,
    source: Option<String>,
) -> Result<KbDocMeta, String> {
    let meta = {
        let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        doc_add_at(&root_dir(&app)?, &id, &title, &content, source.as_deref().unwrap_or(""))
    }?;
    embed::schedule_doc(&app, &id, &meta.id);
    Ok(meta)
}

#[tauri::command]
pub fn kb_doc_update(app: AppHandle, id: String, doc_id: String, title: String, content: String) -> Result<KbDocMeta, String> {
    let meta = {
        let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        doc_update_at(&root_dir(&app)?, &id, &doc_id, &title, &content)
    }?;
    embed::schedule_doc(&app, &id, &doc_id);
    Ok(meta)
}

#[tauri::command]
pub fn kb_doc_delete(app: AppHandle, id: String, doc_id: String) -> Result<(), String> {
    {
        let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        doc_delete_at(&root_dir(&app)?, &id, &doc_id)
    }?;
    // 向量行跟着文档走（纯本地操作，即时）
    embed::schedule_delete(&app, &id, Some(&doc_id));
    Ok(())
}

#[tauri::command]
pub fn kb_doc_get(app: AppHandle, id: String, doc_id: String) -> Result<KbDoc, String> {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    doc_get_at(&root_dir(&app)?, &id, &doc_id)
}

#[tauri::command]
pub fn kb_search(
    app: AppHandle,
    query: String,
    project_id: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<KbHit>, String> {
    let limit = limit.unwrap_or(20).clamp(1, 50);
    let root = root_dir(&app)?;
    let config = crate::config::load(&app);
    let keyword = {
        let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        search_at(&root, &query, project_id.as_deref(), limit)?
    };
    // 语义路：配置了 embedding 才走。失败静默回退关键词（关键词永远兜底）
    let vector = if embed::enabled(&config) {
        match crate::config::embedding_key(&config)
            .and_then(|key| embed::semantic_hits(&root, &config, &key, &query, limit))
        {
            Ok(hits) => hits,
            Err(err) => {
                eprintln!("语义检索回退关键词：{err}");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    Ok(refine_hits(&config, &query, keyword, vector, limit))
}

/// 融合 + 可选精排：两条检索路共同的收尾。
/// 配了 rerank 模型就把融合候选扩到 4 倍，拿各命中的片段文本让 /rerank
/// 精排一次再取 limit 条；没配或失败都静默回落融合序——精排是增益不是闸门
fn refine_hits(
    config: &crate::config::AppConfig,
    query: &str,
    keyword: Vec<KbHit>,
    vector: Vec<(String, String, String, f32)>,
    limit: usize,
) -> Vec<KbHit> {
    if !embed::rerank_enabled(config) {
        let mut hits = fuse_hits(keyword, vector, limit);
        hits.truncate(limit);
        return hits;
    }
    let mut fused = fuse_hits(keyword, vector, limit * 4);
    if fused.len() <= 1 {
        return fused;
    }
    let Ok(key) = crate::config::embedding_key(config) else {
        fused.truncate(limit);
        return fused;
    };
    let documents: Vec<String> = fused.iter().map(|hit| hit.snippet.clone()).collect();
    let order = match embed::rerank(config, &key, query, &documents) {
        Ok(order) => order,
        Err(error) => {
            eprintln!("精排失败，回落融合序：{error}");
            fused.truncate(limit);
            return fused;
        }
    };
    let mut out: Vec<KbHit> = Vec::with_capacity(limit);
    for (index, _) in order {
        if let Some(hit) = fused.get(index) {
            if !out.iter().any(|seen| seen.kb_id == hit.kb_id && seen.doc_id == hit.doc_id) {
                out.push(hit.clone());
            }
        }
        if out.len() >= limit {
            return out;
        }
    }
    // 精排响应漏了几条候选：按融合序补齐到 limit
    for hit in &fused {
        if out.len() >= limit {
            break;
        }
        if !out.iter().any(|seen| seen.kb_id == hit.kb_id && seen.doc_id == hit.doc_id) {
            out.push(hit.clone());
        }
    }
    out
}

/// 倒数排名融合（RRF）：两路各自的名次换成同一把尺子。键是 (kb_id, doc_id)。
/// 关键词命中精确词更准，向量命中语义改写更全——谁也别吞掉谁。
/// 向量命中可能落在关键词没命中的文档上：缺的标题/摘要从库文件里补
fn fuse_hits(keyword: Vec<KbHit>, vector: Vec<(String, String, String, f32)>, limit: usize) -> Vec<KbHit> {
    if vector.is_empty() {
        return keyword;
    }
    const K: f64 = 60.0;
    let mut fused: std::collections::HashMap<(String, String), (KbHit, f64)> = std::collections::HashMap::new();
    for (rank, hit) in keyword.iter().enumerate() {
        let key = (hit.kb_id.clone(), hit.doc_id.clone());
        let score = 1.0 / (K + rank as f64 + 1.0);
        fused
            .entry(key)
            .and_modify(|(_, s)| *s += score)
            .or_insert_with(|| (hit.clone(), score));
    }
    let mut doc_ranks: std::collections::HashMap<(String, String), usize> = std::collections::HashMap::new();
    for (rank, (kb_id, doc_id, _, _)) in vector.iter().enumerate() {
        doc_ranks
            .entry((kb_id.clone(), doc_id.clone()))
            .or_insert(rank);
    }
    for ((kb_id, doc_id), best) in &doc_ranks {
        let score = 1.0 / (K + *best as f64 + 1.0);
        fused
            .entry((kb_id.clone(), doc_id.clone()))
            .and_modify(|(_, s)| *s += score)
            .or_insert_with(|| {
                (
                    lookup_hit(kb_id, doc_id).unwrap_or_else(|| KbHit {
                        kb_id: kb_id.clone(),
                        kb_name: String::new(),
                        doc_id: doc_id.clone(),
                        doc_title: String::new(),
                        snippet: String::new(),
                        score: 0.0,
                        updated_at: 0,
                    }),
                    score,
                )
            });
    }
    let mut rows: Vec<(KbHit, f64)> = fused.into_values().collect();
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    rows.truncate(limit.max(1));
    rows.into_iter()
        .map(|(mut hit, fused)| {
            hit.score = fused * 100.0;
            hit
        })
        .collect()
}

/// 从库文件里补一份命中文档的标题与摘要（向量路的 KbHit 缺这两格）
fn lookup_hit(kb_id: &str, doc_id: &str) -> Option<KbHit> {
    let file = file_for(&tool_root_or_current()?, kb_id).ok()?;
    let kb = read_kb(&file)?;
    let doc = kb.docs.iter().find(|d| d.id == doc_id)?;
    Some(KbHit {
        kb_id: kb.meta.id.clone(),
        kb_name: kb.meta.name.clone(),
        doc_id: doc.id.clone(),
        doc_title: doc.title.clone(),
        snippet: snippet(&doc.content, Some(0), 120),
        score: 0.0,
        updated_at: doc.updated_at,
    })
}

fn tool_root_or_current() -> Option<PathBuf> {
    TOOL_ROOT.get().cloned()
}

/// 从 GitHub 仓库的 Wiki 拉取页面入资料库。Wiki 本身是个 git 仓库
/// （<repo>.wiki.git），浅克隆后读 Markdown 文件——与 worktree 同一条 git CLI 路
#[tauri::command]
pub fn kb_import_wiki(app: AppHandle, id: String, repo: String) -> Result<KbImportOutcome, String> {
    use std::process::Command;

    let repo = repo.trim().trim_end_matches('/').to_string();
    if repo.is_empty() {
        return Err("仓库名不能为空。".into());
    }
    // owner/repo 简写 → GitHub 全 URL；已是 URL 的原样用
    let wiki_url = if repo.matches('/').count() >= 1 && repo.starts_with("http") {
        format!("{repo}.wiki.git")
    } else if repo.matches('/').count() == 1 && !repo.contains('.') {
        format!("https://github.com/{repo}.wiki.git")
    } else {
        return Err(format!("认不出仓库：{repo}。填 owner/repo 或完整的 GitHub URL。"));
    };
    let scratch = std::env::temp_dir().join(format!(
        "aglab-wiki-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&scratch);

    // git CLI：与 worktree 同一条路。GIT_TERMINAL_PROMPT=0 挡住交互式凭据提示
    let output = Command::new("git")
        .args(["clone", "--depth", "1", &wiki_url])
        .arg(&scratch)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("调 git 失败：{e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(format!(
            "Wiki 克隆失败（{wiki_url}）：{}。私有仓库需要本机 git 已有该仓库的访问凭据。",
            stderr.trim().chars().take(160).collect::<String>()
        ));
    }

    // 读所有 .md 文件（Home.md 排最前当开门篇），大小与篇数照导入上限约束
    let mut pages: Vec<(String, String)> = Vec::new();
    let entries = std::fs::read_dir(&scratch).map_err(|e| format!("{e}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() > MAX_IMPORT_BYTES {
                continue;
            }
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            pages.push((stem, text));
        }
    }
    pages.sort_by(|a, b| {
        let home = |name: &String| name.eq_ignore_ascii_case("home");
        match (home(&a.0), home(&b.0)) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.0.cmp(&b.0),
        }
    });

    let mut outcome = KbImportOutcome { added: 0, skipped: 0, skipped_names: Vec::new() };
    {
        let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for (stem, text) in &pages {
            if text.trim().is_empty() {
                outcome.skipped += 1;
                continue;
            }
            match doc_add_at(
                &root_dir(&app)?,
                &id,
                stem,
                text,
                &format!("Wiki · {stem}.md"),
            ) {
                Ok(_) => outcome.added += 1,
                Err(error) if error.contains("超过") || error.contains("已有") => {
                    outcome.skipped += 1;
                    outcome.skipped_names.push(format!("{stem}：{error}"));
                }
                Err(error) => {
                    let _ = std::fs::remove_dir_all(&scratch);
                    return Err(error);
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    // 每篇排一次嵌入（后台线程）
    for doc in get_at(&root_dir(&app)?, &id)?.docs {
        embed::schedule_doc(&app, &id, &doc.id);
    }
    Ok(outcome)
}

/// 语义索引状态：配置的模型与向量库实际嵌过的模型是否一致
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbedStatusView {
    pub enabled: bool,
    pub model: String,
    pub indexed_model: String,
    pub chunks: usize,
    /// true = 向量库还没建过或模型不一致，点重建
    pub stale: bool,
}

#[tauri::command]
pub fn kb_embed_status(app: AppHandle) -> Result<EmbedStatusView, String> {
    let config = crate::config::load(&app);
    let status = embed::status_at(&root_dir(&app)?, &config);
    let stale = status.enabled
        && (status.indexed_model.is_empty() || status.indexed_model != status.model);
    Ok(EmbedStatusView {
        enabled: status.enabled,
        model: status.model,
        indexed_model: status.indexed_model,
        chunks: status.chunks,
        stale,
    })
}

#[tauri::command]
pub fn kb_reembed(app: AppHandle) -> Result<(), String> {
    embed::reembed_all(&app);
    Ok(())
}

// ---- Umi-OCR 引擎命令（实现在 ocr.rs，命令按项目惯例住 mod.rs）----

#[tauri::command]
pub fn ocr_engine_status(app: AppHandle) -> Result<ocr::EngineStatus, String> {
    ocr::ocr_engine_status(app)
}

#[tauri::command]
pub fn ocr_engine_start(app: AppHandle) -> Result<(), String> {
    ocr::ocr_engine_start(app)
}

#[tauri::command]
pub async fn ocr_engine_download(app: AppHandle) -> Result<(), String> {
    ocr::ocr_engine_download(app).await
}

/// 拉取端点的模型目录（OpenAI 兼容 GET /models），设置页的模型选择用。
/// 密钥、代理与出口口径同 embed 客户端；端点不鉴权也能拉（没 key 就不带头部）
#[tauri::command]
pub fn embedding_models(app: AppHandle, base_url: String) -> Result<Vec<String>, String> {
    use crate::proxy::plan;

    let config = crate::config::load(&app);
    let url = format!("{}/models", base_url.trim().trim_end_matches('/'));
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("端点基址要先填好（http/https 开头）。".into());
    }
    crate::egress::guard(&config.net_egress_allow, &url)?;
    let key = crate::config::embedding_key(&config).ok();
    let mut plan = plan(&config, &url)?;
    let mut last = String::from("请求未发出。");
    while let Some(leg) = plan.next() {
        let agent = crate::proxy::agent_for(leg.proxy_url())?;
        let mut request = crate::net::with_timeouts(agent.get(&url), std::time::Duration::from_secs(30));
        if let Some(key) = &key {
            request = request.header("authorization", format!("Bearer {key}"));
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(error) => {
                last = format!("{error}");
                continue;
            }
        };
        let status = response.status();
        let text = {
            use std::io::Read;
            let mut text = String::new();
            response
                .into_body()
                .into_reader()
                .read_to_string(&mut text)
                .map_err(|e| format!("{e}"))?;
            text
        };
        if status.as_u16() != 200 {
            last = format!("HTTP {}：{}", status.as_u16(), text.chars().take(160).collect::<String>());
            continue;
        }
        let parsed: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("响应不是合法 JSON：{e}"))?;
        let mut names: Vec<String> = parsed["data"]
            .as_array()
            .ok_or("响应缺 data 数组")?
            .iter()
            .filter_map(|item| item["id"].as_str().map(|s| s.to_string()))
            .collect();
        names.sort();
        names.dedup();
        return Ok(names);
    }
    Err(last)
}

#[tauri::command]
pub fn kb_import_files(app: AppHandle, id: String, paths: Vec<String>) -> Result<KbImportOutcome, String> {
    let outcome = {
        let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        import_files_at(&root_dir(&app)?, &id, &paths)
    }?;
    // 新增的每一篇排一次嵌入（后台线程，逐篇读回正文）
    for doc in get_at(&root_dir(&app)?, &id)?.docs {
        embed::schedule_doc(&app, &id, &doc.id);
    }
    Ok(outcome)
}

// ---- 模型工具入口（tools.rs 的 execute 路由到这里） ----

/// 给模型的检索结果。文本形状稳定：编号列表 + 出处 + 摘要，模型按行读
pub fn tool_search(args: &Value) -> Result<String, String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or("knowledge_search 缺少 query 参数")?;
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(8).clamp(1, 20) as usize;

    let root = TOOL_ROOT
        .get()
        .ok_or("资料库还没就绪（应用还在启动中），稍后再试。")?;
    let embed_config = EMBED_SNAPSHOT.get().cloned().unwrap_or_default();
    let keyword = search_at(root, query, None, limit)?;
    // 语义路：配置了 embedding 才走，失败静默回退（关键词永远兜底）。
    // 钥匙：embedding 专用槽优先，没设沿用主密钥
    let vector = if embed::enabled(&embed_config) {
        crate::config::embedding_key(&embed_config)
            .and_then(|key| embed::semantic_hits(root, &embed_config, &key, query, limit))
            .unwrap_or_else(|error| {
                eprintln!("语义检索回退关键词：{error}");
                Vec::new()
            })
    } else {
        Vec::new()
    };
    let hits = refine_hits(&embed_config, query, keyword, vector, limit);

    if hits.is_empty() {
        let any = list_at(root)?.is_empty();
        return Ok(if any {
            "资料库里还没有内容：让用户先在「资料库」页创建资料库并添加文档，之后才能检索。".to_string()
        } else {
            format!("资料库里没有关于「{query}」的内容。")
        });
    }
    let mut out = format!("资料库检索「{query}」共 {} 条命中：\n", hits.len());
    for (index, hit) in hits.iter().enumerate() {
        out.push_str(&format!(
            "{}. 「{}」{}（更新 {}）\n   {}\n",
            index + 1,
            hit.kb_name,
            hit.doc_title,
            fmt_time(hit.updated_at),
            hit.snippet.replace('\n', " ")
        ));
    }
    Ok(out.trim_end().to_string())
}

/// 毫秒时间戳 → 人话日期。只给模型与摘要用，精确到天就够了
pub fn fmt_time(at: u64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt((at / 1000) as i64, 0)
        .single()
        .map(|t| t.format("%m-%d").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scoped_temp_dir;

    fn root_of(label: &str) -> (crate::test_support::ScopedTempDir, PathBuf) {
        let scoped = scoped_temp_dir(label);
        let path = scoped.path.clone();
        (scoped, path)
    }

    #[test]
    fn crud_round_trips_through_the_file() {
        let (_scope, root) = root_of("kb-crud");

        let created = create_at(&root, "钓鱼笔记", "钓点与鱼情", "").unwrap();
        assert_eq!(created.name, "钓鱼笔记");
        assert_eq!(created.doc_count, 0);

        let listed = list_at(&root).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);

        let updated = update_at(&root, &created.id, "钓鱼笔记（改）", "补充").unwrap();
        assert_eq!(updated.name, "钓鱼笔记（改）");

        let doc = doc_add_at(&root, &created.id, "水库夜钓", "用发酵玉米打窝，钓草鱼。", "").unwrap();
        assert_eq!(doc.chars, "用发酵玉米打窝，钓草鱼。".chars().count());

        let detail = get_at(&root, &created.id).unwrap();
        assert_eq!(detail.docs.len(), 1);
        assert_eq!(detail.docs[0].source, "手动录入");
        assert_eq!(detail.kb.chars, doc.chars);

        let edited = doc_update_at(&root, &created.id, &doc.id, "水库夜钓（改）", "改用螺蛳。").unwrap();
        assert_eq!(edited.title, "水库夜钓（改）");

        doc_delete_at(&root, &created.id, &doc.id).unwrap();
        assert!(get_at(&root, &created.id).unwrap().docs.is_empty());

        delete_at(&root, &created.id).unwrap();
        assert!(list_at(&root).unwrap().is_empty());
        assert!(!root.join(format!("kb-{}.json", created.id)).exists());
    }

    #[test]
    fn empty_names_and_bodies_are_refused() {
        let (_scope, root) = root_of("kb-refuse");
        assert!(create_at(&root, "  ", "", "").is_err());
        let created = create_at(&root, "库", "", "").unwrap();
        assert!(doc_add_at(&root, &created.id, "标题", "  ", "").is_err());
        assert!(doc_add_at(&root, &created.id, " ", "正文", "").is_err());
        let doc = doc_add_at(&root, &created.id, "标题", "正文", "").unwrap();
        assert!(doc_update_at(&root, &created.id, &doc.id, "标题", "").is_err());
    }

    #[test]
    fn bad_ids_never_touch_the_filesystem() {
        let (_scope, root) = root_of("kb-ids");
        assert!(load_one(&root, "../escape").is_err());
        assert!(delete_at(&root, "a/b").is_err());
        assert!(get_at(&root, "").is_err());
        // 目录里什么都没写出来
        assert!(json_files(&root).unwrap().is_empty());
    }

    #[test]
    fn search_orders_by_score_and_filters_by_project() {
        let (_scope, root) = root_of("kb-search");
        let a = create_at(&root, "库A", "", "proj1").unwrap();
        let b = create_at(&root, "库B", "", "").unwrap();

        doc_add_at(&root, &a.id, "无关文档", "这里讲别的事。", "").unwrap();
        doc_add_at(&root, &a.id, "发酵玉米打窝", "发酵玉米是钓草鱼的好饵料，玉米要提前泡。", "").unwrap();
        doc_add_at(&root, &b.id, "玉米另一个库", "玉米", "").unwrap();

        // 全库检索：标题命中多篇「玉米」，覆盖与词频决定次序
        let hits = search_at(&root, "玉米", None, 10).unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert!(hits[0].score >= hits[1].score);
        assert!(hits[0].snippet.contains("玉米"), "{:?}", hits[0].snippet);

        // 按工作目录过滤：只看 proj1
        let scoped = search_at(&root, "玉米", Some("proj1"), 10).unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].kb_id, a.id);

        // 无关词不硬凑（夹具里有「无关文档」，所以查询词得避开它的 bigram）
        assert!(search_at(&root, "外星飞船", None, 10).unwrap().is_empty());
    }

    #[test]
    fn import_skips_bad_files_and_adds_good_ones() {
        let (_scope, root) = root_of("kb-import");
        let dir = scoped_temp_dir("kb-import-files");
        let good = dir.path.join("笔记.md");
        std::fs::write(&good, "# 夜钓心得\n\n夏夜钓草鱼，钓远不钓近。").unwrap();
        let binary = dir.path.join("blob.bin");
        std::fs::write(&binary, b"ok\x00binary").unwrap();
        let empty = dir.path.join("empty.txt");
        std::fs::write(&empty, "   \n").unwrap();
        let missing = dir.path.join("ghost.md");

        let created = create_at(&root, "资料", "", "").unwrap();
        let outcome = import_files_at(
            &root,
            &created.id,
            &[good.display().to_string(), binary.display().to_string(), empty.display().to_string(), missing.display().to_string()],
        )
        .unwrap();

        assert_eq!(outcome.added, 1, "{outcome:?}");
        assert_eq!(outcome.skipped, 3, "{outcome:?}");
        assert_eq!(outcome.skipped_names.len(), 3, "{outcome:?}");

        let detail = get_at(&root, &created.id).unwrap();
        assert_eq!(detail.docs.len(), 1);
        assert_eq!(detail.docs[0].title, "笔记.md");
        // source 是原路径 + 提取方式：以后能追溯它从哪来、怎么转的文
        assert_eq!(detail.docs[0].source, format!("{} · 纯文本", good.display()));

        // 导入的文件立刻检索得到
        let hits = search_at(&root, "草鱼", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
    }
}
