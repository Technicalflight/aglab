//! 资料库的语义检索层：embedding 客户端、向量存储与混合检索。
//!
//! 设计口径（与 search.rs 的预留接缝对齐）：
//! - embedding 档在 config.embedding（OpenAI 兼容 /embeddings 端点，密钥沿用主密钥）；
//! - 没配置或调用失败 = 静默回退纯关键词检索（调用方形状不变）；
//! - 向量存 SQLite（knowledge 目录下 embeddings.db），按 (kb_id, doc_id, chunk) 主键
//!   原位更新——文档重写就是整篇重嵌，不用 diff；
//! - 换 embedding 模型 = 向量空间作废，按 model 字段过滤，重嵌由设置页显式触发。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Manager};

use rusqlite::params;

use crate::config::AppConfig;

/// 单块目标长度（字符）。资料库文档以段落为主，按空行切再合并到这个尺寸
const CHUNK_CHARS: usize = 800;
/// 单篇文档的块数上限：embedding 是按块计费的，一篇超长文档截到前 N 块
const MAX_CHUNKS_PER_DOC: usize = 64;
/// 批量 embedding 一次请求最多带多少块
const BATCH: usize = 16;

pub fn enabled(config: &AppConfig) -> bool {
    !config.embedding.base_url.trim().is_empty() && !config.embedding.model.trim().is_empty()
}

fn embeddings_url(config: &AppConfig) -> String {
    format!("{}/embeddings", config.embedding.base_url.trim_end_matches('/'))
}

/// 文档正文切块：按空行切段、顺序合并到 CHUNK_CHARS。
/// 不做重叠——资料库检索要的是"哪篇文档相关"，不是逐字定位
pub fn chunk_content(content: &str) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for para in content.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        if current.chars().count() + para.chars().count() + 2 > CHUNK_CHARS && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        // 单段超长：硬切成 CHUNK_CHARS 窗口（资料库正文上限 2M 字，防御性兜底）
        if para.chars().count() > CHUNK_CHARS {
            let chars: Vec<char> = para.chars().collect();
            for window in chars.chunks(CHUNK_CHARS) {
                let piece: String = window.iter().collect();
                if current.is_empty() {
                    chunks.push(piece);
                } else {
                    chunks.push(std::mem::take(&mut current));
                    chunks.push(piece);
                }
            }
            continue;
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(para);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks.truncate(MAX_CHUNKS_PER_DOC);
    chunks
}

/// OpenAI 兼容 /embeddings：批量输入 → 逐条向量。走出口名单与代理池逐腿尝试。
/// 密钥沿用主密钥——中转站同一把钥匙开 chat 与 embeddings 两个端点是常态
fn embed_texts(config: &AppConfig, key: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    use crate::proxy::plan;
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let url = embeddings_url(config);
    crate::egress::guard(&config.net_egress_allow, &url)?;
    let body = serde_json::json!({
        "model": config.embedding.model,
        "input": texts,
    });
    let mut plan = plan(config, &url)?;
    let mut last = String::from("请求未发出。");
    while let Some(leg) = plan.next() {
        let agent = crate::proxy::agent_for(leg.proxy_url())?;
        let request = crate::chat::with_timeouts(agent.post(&url), Duration::from_secs(60))
            .header("authorization", format!("Bearer {key}"));
        let response = match request.send_json(body.clone()) {
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
        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| format!("响应不是合法 JSON：{e}"))?;
        let mut out: Vec<Vec<f32>> = Vec::new();
        for item in parsed["data"].as_array().ok_or("响应缺 data 数组")? {
            let vec = item["embedding"]
                .as_array()
                .ok_or("embedding 条目缺向量数组")?
                .iter()
                .filter_map(|v| v.as_f64().map(|f| f as f32))
                .collect::<Vec<f32>>();
            if vec.is_empty() {
                return Err("embedding 条目里的向量是空的".into());
            }
            out.push(vec);
        }
        if out.len() != texts.len() {
            return Err(format!("embedding 返回 {} 条，期望 {} 条", out.len(), texts.len()));
        }
        return Ok(out);
    }
    Err(last)
}

// ---- 向量存储（knowledge 目录下 embeddings.db，随资料库一起拷走迁移） ----

fn db_path(root: &Path) -> PathBuf {
    root.join("embeddings.db")
}

fn with_conn<T>(
    root: &Path,
    run: impl FnOnce(&rusqlite::Connection) -> Result<T, String>,
) -> Result<T, String> {
    let conn = rusqlite::Connection::open(db_path(root)).map_err(|e| format!("打开向量库失败：{e}"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS chunks (
            kb_id       TEXT NOT NULL,
            doc_id      TEXT NOT NULL,
            chunk_index INTEGER NOT NULL,
            content     TEXT NOT NULL,
            embedding   BLOB NOT NULL,
            dims        INTEGER NOT NULL,
            model       TEXT NOT NULL,
            updated_at  INTEGER NOT NULL,
            PRIMARY KEY (kb_id, doc_id, chunk_index)
        );
        CREATE INDEX IF NOT EXISTS idx_chunks_doc ON chunks (kb_id, doc_id);",
    )
    .map_err(|e| format!("初始化向量库失败：{e}"))?;
    run(&conn)
}

fn vec_to_blob(vec: &[f32]) -> Vec<u8> {
    vec.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn blob_to_vec(blob: &[u8]) -> Vec<f32> {
    blob.as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes))
        .collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom <= 0.0 {
        0.0
    } else {
        dot / denom
    }
}

/// 整篇文档的向量原位替换（旧块全删再插）。rows = (chunk_index, content, vector)
fn upsert_doc(
    root: &Path,
    kb_id: &str,
    doc_id: &str,
    model: &str,
    rows: &[(usize, String, Vec<f32>)],
) -> Result<(), String> {
    with_conn(root, |conn| {
        conn.execute(
            "DELETE FROM chunks WHERE kb_id = ?1 AND doc_id = ?2",
            params![kb_id, doc_id],
        )
        .map_err(|e| e.to_string())?;
        let now = super::now_ms() as i64;
        for (index, content, vector) in rows {
            conn.execute(
                "INSERT INTO chunks (kb_id, doc_id, chunk_index, content, embedding, dims, model, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![kb_id, doc_id, *index as i64, content, vec_to_blob(vector), vector.len() as i64, model, now],
            )
            .map_err(|e| format!("写入向量失败：{e}"))?;
        }
        Ok(())
    })
}

fn delete_doc_rows(root: &Path, kb_id: &str, doc_id: &str) -> Result<(), String> {
    with_conn(root, |conn| {
        conn.execute(
            "DELETE FROM chunks WHERE kb_id = ?1 AND doc_id = ?2",
            params![kb_id, doc_id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })
}

fn delete_kb_rows(root: &Path, kb_id: &str) -> Result<(), String> {
    with_conn(root, |conn| {
        conn.execute("DELETE FROM chunks WHERE kb_id = ?1", params![kb_id])
            .map_err(|e| e.to_string())?;
        Ok(())
    })
}

/// 向量命中：内容 + 相似度（0-1 余弦）。只认当前配置模型的向量
fn search_vectors(
    root: &Path,
    model: &str,
    query_vec: &[f32],
    limit: usize,
) -> Result<Vec<(String, String, String, f32)>, String> {
    with_conn(root, |conn| {
        let mut stmt = conn
            .prepare(
                "SELECT kb_id, doc_id, content, embedding FROM chunks
                 WHERE model = ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![model], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut scored: Vec<(String, String, String, f32)> = rows
            .filter_map(|row| {
                let row = row.ok()?;
                let score = cosine(&blob_to_vec(&row.3), query_vec);
                Some((row.0, row.1, row.2, score))
            })
            .collect();
        scored.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit.max(1));
        Ok(scored)
    })
}

// ---- 编排：嵌入一篇 / 全量重建 / 状态 ----

/// 后台嵌一篇文档：读内容 → 切块 → 批量 embed → 原位替换。
/// 配置未启用或请求失败都静默（eprintln 留痕）——关键词检索仍然兜底
pub fn schedule_doc(app: &AppHandle, kb_id: &str, doc_id: &str) {
    let app = app.clone();
    let kb_id = kb_id.to_string();
    let doc_id = doc_id.to_string();
    std::thread::spawn(move || {
        let config = crate::config::load(&app);
        if !enabled(&config) {
            return;
        }
        let root = match root_of(&app) {
            Some(root) => root,
            None => return,
        };
        let Ok(doc) = super::doc_get_at(&root, &kb_id, &doc_id) else { return };
        let key = match crate::config::api_key(&config) {
            Ok(key) => key,
            Err(error) => {
                eprintln!("embedding 跳过（拿不到密钥）：{error}");
                return;
            }
        };
        let chunks = chunk_content(&doc.content);
        if chunks.is_empty() {
            let _ = delete_doc_rows(&root, &kb_id, &doc_id);
            return;
        }
        let mut rows: Vec<(usize, String, Vec<f32>)> = Vec::new();
        for batch in chunks.chunks(BATCH) {
            match embed_texts(&config, &key, batch) {
                Ok(vectors) => {
                    for (index, vector) in batch.iter().zip(vectors) {
                        rows.push((rows.len(), (*index).clone(), vector));
                    }
                }
                Err(error) => {
                    eprintln!("embedding 失败（{}/{}）：{error}", kb_id, doc_id);
                    return;
                }
            }
        }
        if let Err(error) = upsert_doc(&root, &kb_id, &doc_id, &config.embedding.model, &rows) {
            eprintln!("向量写入失败：{error}");
        }
    });
}

/// 文档或整库被删：向量行跟着走（纯本地操作，即时）
pub fn schedule_delete(app: &AppHandle, kb_id: &str, doc_id: Option<&str>) {
    let Some(root) = root_of(app) else { return };
    let result = match doc_id {
        Some(doc_id) => delete_doc_rows(&root, kb_id, doc_id),
        None => delete_kb_rows(&root, kb_id),
    };
    if let Err(error) = result {
        eprintln!("向量清理失败：{error}");
    }
}

fn root_of(app: &AppHandle) -> Option<PathBuf> {
    Some(app.path().app_data_dir().ok()?.join("knowledge"))
}

/// 索引状态：配置的模型 vs 向量库里实际嵌过的模型，不一致 = 需要重建
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbedStatus {
    pub enabled: bool,
    pub model: String,
    pub indexed_model: String,
    pub chunks: usize,
}

pub fn status_at(root: &Path, config: &AppConfig) -> EmbedStatus {
    let enabled = enabled(config);
    let mut indexed_model = String::new();
    let mut chunks = 0usize;
    if enabled {
        let _ = with_conn(root, |conn| {
            if let Ok((model, count)) = conn.query_row(
                "SELECT model, COUNT(*) FROM chunks GROUP BY model ORDER BY updated_at DESC LIMIT 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?)),
            ) {
                indexed_model = model;
                chunks = count;
            }
            Ok(())
        });
    }
    EmbedStatus {
        enabled,
        model: config.embedding.model.clone(),
        indexed_model,
        chunks,
    }
}

/// 全量重建：清空向量表，逐库逐篇重嵌。在后台线程跑，完成前检索退化为关键词
pub fn reembed_all(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let config = crate::config::load(&app);
        if !enabled(&config) {
            return;
        }
        let Some(root) = root_of(&app) else { return };
        let Ok(key) = crate::config::api_key(&config) else { return };
        let _ = with_conn(&root, |conn| {
            conn.execute("DELETE FROM chunks", []).map_err(|e| e.to_string())
        });
        for path in match super::json_files(&root) {
            Ok(paths) => paths,
            Err(_) => return,
        } {
            let Some(kb) = super::read_kb(&path) else { continue };
            for doc in &kb.docs {
                let chunks = chunk_content(&doc.content);
                let mut rows: Vec<(usize, String, Vec<f32>)> = Vec::new();
                for batch in chunks.chunks(BATCH) {
                    match embed_texts(&config, &key, batch) {
                        Ok(vectors) => {
                            for (index, vector) in batch.iter().zip(vectors) {
                                rows.push((rows.len(), (*index).clone(), vector));
                            }
                        }
                        Err(error) => {
                            eprintln!("重建索引：{} 嵌入失败 {error}", doc.title);
                            break;
                        }
                    }
                }
                if !rows.is_empty() {
                    let _ = upsert_doc(&root, &kb.meta.id, &doc.id, &config.embedding.model, &rows);
                }
            }
        }
    });
}

// ---- 混合检索 ----

/// 向量命中的中间形状（kb_id/doc_id/块内容/相似度）转成 KbHit 需要
/// 库名与文档标题——调用方拿 kb 文件补齐
pub fn semantic_hits(
    root: &Path,
    config: &AppConfig,
    key: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<(String, String, String, f32)>, String> {
    if !enabled(config) {
        return Ok(Vec::new());
    }
    let vectors = embed_texts(config, key, &[query.to_string()])?;
    let Some(query_vec) = vectors.into_iter().next() else {
        return Ok(Vec::new());
    };
    search_vectors(root, &config.embedding.model, &query_vec, limit.max(8))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scoped_temp_dir;

    #[test]
    fn chunks_merge_paragraphs_and_hard_split_oversize_ones() {
        // 空正文不切块
        assert!(chunk_content("   \n\n  ").is_empty());

        // 短段落合并进同一块
        let merged = chunk_content("第一段。\n\n第二段。\n\n第三段。");
        assert_eq!(merged.len(), 1);
        assert!(merged[0].starts_with("第一段。"));
        assert!(merged[0].ends_with("第三段。"));

        // 单段超长：硬切成 CHUNK_CHARS 窗口，窗口不丢字
        let long = "字".repeat(CHUNK_CHARS * 2 + 10);
        let split = chunk_content(&long);
        let total: usize = split.iter().map(|c| c.chars().count()).sum();
        assert_eq!(total, CHUNK_CHARS * 2 + 10);
        assert!(split.iter().all(|c| c.chars().count() <= CHUNK_CHARS));
    }

    #[test]
    fn chunks_carry_a_per_doc_ceiling() {
        // 每段 600 字、120 段：合并后远超 64 块，截到 MAX_CHUNKS_PER_DOC
        let body = (0..120).map(|i| format!("{i:03}{}", "段".repeat(600))).collect::<Vec<_>>().join("\n\n");
        let chunks = chunk_content(&body);
        assert_eq!(chunks.len(), MAX_CHUNKS_PER_DOC);
    }

    #[test]
    fn cosine_scores_shapes_and_zero_vectors() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert!((cosine(&[1.0, 1.0], &[2.0, 2.0]) - 1.0).abs() < 1e-6);
        // 维度不一致与零向量都不给分数，而不是 panic 或 NaN
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine(&[], &[1.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn vector_store_round_trips_and_filters_by_model() {
        let scoped = scoped_temp_dir("kb-embed-store");
        let root = scoped.path.clone();

        let doc_vec = vec![1.0_f32, 0.0, 0.0];
        upsert_doc(
            &root,
            "kb1",
            "doc1",
            "embed-a",
            &[(0, "钓草鱼".into(), doc_vec.clone()), (1, "钓鲢鳙".into(), vec![0.9, 0.1, 0.0])],
        )
        .unwrap();
        // 别的模型的向量：不该被搜出来
        upsert_doc(&root, "kb1", "doc2", "embed-b", &[(0, "旧模型".into(), doc_vec.clone())]).unwrap();

        let hits = search_vectors(&root, "embed-a", &doc_vec, 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].1, "doc1");
        assert!(hits[0].3 > hits[1].3, "完全同向的要排前面");

        // 换模型查询：一行都没有
        assert!(search_vectors(&root, "embed-c", &doc_vec, 10).unwrap().is_empty());

        delete_doc_rows(&root, "kb1", "doc1").unwrap();
        assert!(search_vectors(&root, "embed-a", &doc_vec, 10).unwrap().is_empty());
        // doc2 的旧模型行不受牵连
        assert_eq!(search_vectors(&root, "embed-b", &doc_vec, 10).unwrap().len(), 1);
    }

    #[test]
    fn upsert_replaces_previous_rows_in_place() {
        let scoped = scoped_temp_dir("kb-embed-upsert");
        let root = scoped.path.clone();

        upsert_doc(&root, "kb1", "doc1", "embed-a", &[(0, "旧".into(), vec![1.0])]).unwrap();
        // 重写同一篇：旧块整篇作废，只剩新块
        upsert_doc(&root, "kb1", "doc1", "embed-a", &[(0, "新一".into(), vec![1.0, 0.0]), (1, "新二".into(), vec![0.0, 1.0])]).unwrap();
        let hits = search_vectors(&root, "embed-a", &[1.0, 0.0], 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(!hits.iter().any(|h| h.2 == "旧"), "重嵌后旧块不该残留");
    }
}
