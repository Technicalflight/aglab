//! 读 cc-switch 的库存。
//!
//! cc-switch 是另一个应用：它的 SQLite 我们只读，不改也不迁走。
//! 密钥读出来直接写进 Windows 凭据管理器，不回前端——界面上只出现"带不带密钥"这个布尔值。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use tauri::AppHandle;

use crate::config::{self, AppConfig, McpServer};
use crate::skills;
use crate::usage;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub source_id: String,
    /// claude / codex：cc-switch 存配置的两种壳子
    pub app_type: String,
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub reasoning_effort: String,
    pub api_format: String,
    pub has_key: bool,
    pub is_current: bool,
    /// 派生出来的完整请求地址。先给用户看见，才能判断路径猜得对不对
    pub endpoint: String,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCandidate {
    pub source_id: String,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env_keys: Vec<String>,
    /// 看着像密钥的环境变量名。导入会把它们的值明文写进 config.json
    pub secret_env_keys: Vec<String>,
    pub enabled_for: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpImportResult {
    pub config: AppConfig,
    pub added: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillCandidate {
    pub source_id: String,
    pub name: String,
    pub description: String,
    /// cc-switch 侧的目录名，同时也是 aglab 侧的目标目录名
    pub directory: String,
    /// ~/.cc-switch/skills/<directory>/SKILL.md 是否存在
    pub has_skill_md: bool,
    /// aglab 个人技能目录下是否已有同名目录（导入会跳过）
    pub exists: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillImportResult {
    pub added: usize,
    pub skipped: usize,
    /// "daily-report（aglab 里已有同名技能）" 这样的目录名+原因
    pub skipped_names: Vec<String>,
}

struct Extracted {
    base_url: String,
    key: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
    api_format: String,
    note: String,
}

fn db_path() -> Result<PathBuf, String> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| "找不到主目录，定位不到 cc-switch 的数据。".to_string())?;
    Ok(PathBuf::from(home).join(".cc-switch").join("cc-switch.db"))
}

fn open_readonly() -> Result<Connection, String> {
    let path = db_path()?;
    if !path.is_file() {
        return Err("没找到 cc-switch 的数据库（应在 ~/.cc-switch/cc-switch.db）。".into());
    }
    Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("打开 cc-switch 数据库失败：{e}"))
}

/// aglab 会在 base_url 后面拼 /chat/completions 或 /responses。
/// cc-switch 的 claude 壳子常常只存域名，codex 壳子已经带 /v1，这里统一
fn openai_base(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

fn looks_secret(key: &str) -> bool {
    let low = key.to_lowercase();
    ["key", "token", "secret", "password", "authorization"]
        .iter()
        .any(|hint| low.contains(hint))
}

/// 两种壳子各自的字段位置完全不同，拆出来才能不碰数据库地测
fn extract(app_type: &str, settings: &Value, meta: &Value) -> Option<Extracted> {
    let (base_raw, key, model, effort, wire) = match app_type {
        "claude" => {
            let env = &settings["env"];
            (
                env["ANTHROPIC_BASE_URL"].as_str().map(str::to_string),
                env["ANTHROPIC_AUTH_TOKEN"].as_str().map(str::to_string),
                env["ANTHROPIC_MODEL"].as_str().map(str::to_string),
                None,
                None,
            )
        }
        "codex" => {
            // toml 0.9 起 Value 的 FromStr 只吃单个值表达式，整份文档要走 from_str，
            // 否则 codex 壳子的候选在运行时会被整批丢掉
            let parsed: Option<toml::Value> = settings["config"]
                .as_str()
                .and_then(|text| toml::from_str(text).ok());
            let provider_key = parsed
                .as_ref()
                .and_then(|table| table.get("model_provider"))
                .and_then(|value| value.as_str())
                .unwrap_or("custom");
            let table = parsed
                .as_ref()
                .and_then(|table| table.get("model_providers"))
                .and_then(|providers| providers.get(provider_key));
            (
                table
                    .and_then(|entry| entry.get("base_url"))
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                settings["auth"]["OPENAI_API_KEY"]
                    .as_str()
                    .map(str::to_string),
                parsed
                    .as_ref()
                    .and_then(|table| table.get("model"))
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                parsed
                    .as_ref()
                    .and_then(|table| table.get("model_reasoning_effort"))
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                table
                    .and_then(|entry| entry.get("wire_api"))
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
            )
        }
        _ => return None,
    };

    // 内置官方条目的 env / config 是空的，没有服务商可导
    let base_raw = base_raw.filter(|base| !base.trim().is_empty())?;

    // meta.apiFormat 是 cc-switch 替用户记下的结论，比从 wire_api 反推更可信
    let api_format = match meta["apiFormat"].as_str() {
        Some("openai_responses") => "responses",
        Some("openai_chat") => "chat",
        _ => match wire.as_deref() {
            Some("responses") => "responses",
            _ => "chat",
        },
    };

    let note = if app_type == "claude" {
        "这条原本是给 Anthropic /v1/messages 用的，aglab 会按 OpenAI 兼容路径试它。".to_string()
    } else {
        String::new()
    };

    Some(Extracted {
        base_url: base_raw,
        key: key.filter(|secret| !secret.trim().is_empty()),
        model,
        reasoning_effort: effort,
        api_format: api_format.to_string(),
        note,
    })
}

fn endpoint_of(base: &str, api_format: &str) -> String {
    if api_format == "responses" {
        format!("{base}/responses")
    } else {
        format!("{base}/chat/completions")
    }
}

#[tauri::command]
pub fn ccswitch_candidates() -> Result<Vec<Candidate>, String> {
    let conn = open_readonly()?;
    let mut stmt = conn
        .prepare(
            "SELECT id, app_type, name, settings_config, meta, is_current
             FROM providers
             ORDER BY is_current DESC, sort_index ASC",
        )
        .map_err(|e| format!("读 cc-switch 的 provider 失败：{e}"))?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(|e| e.to_string())?;

    let mut out = Vec::new();
    for row in rows {
        let (source_id, app_type, name, settings_text, meta_text, is_current) =
            row.map_err(|e| e.to_string())?;
        let settings: Value = serde_json::from_str(&settings_text).unwrap_or(Value::Null);
        let meta: Value = serde_json::from_str(&meta_text).unwrap_or(Value::Null);

        let Some(extracted) = extract(&app_type, &settings, &meta) else {
            continue;
        };
        let base_url = openai_base(&extracted.base_url);
        out.push(Candidate {
            source_id,
            app_type,
            name,
            endpoint: endpoint_of(&base_url, &extracted.api_format),
            base_url,
            model: extracted.model.unwrap_or_default(),
            reasoning_effort: extracted.reasoning_effort.unwrap_or_default(),
            api_format: extracted.api_format,
            has_key: extracted.key.is_some(),
            is_current: is_current != 0,
            note: extracted.note,
        });
    }
    Ok(out)
}

#[tauri::command]
pub fn ccswitch_import_provider(app: AppHandle, source_id: String) -> Result<AppConfig, String> {
    let conn = open_readonly()?;
    let row = conn
        .query_row(
            "SELECT app_type, name, settings_config, meta FROM providers WHERE id = ?1",
            params![source_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|e| format!("读 cc-switch 失败：{e}"))?;
    let Some((app_type, provider_name, settings_text, meta_text)) = row else {
        return Err("cc-switch 里已经没有这一条了。".into());
    };

    let settings: Value = serde_json::from_str(&settings_text).unwrap_or(Value::Null);
    let meta: Value = serde_json::from_str(&meta_text).unwrap_or(Value::Null);
    let extracted = extract(&app_type, &settings, &meta)
        .ok_or_else(|| "这一条里没有可用的服务商地址。".to_string())?;

    let mut config = config::load(&app);
    config.base_url = openai_base(&extracted.base_url);
    if let Some(model) = extracted.model.filter(|model| !model.trim().is_empty()) {
        config.model = model.trim().to_string();
    }
    if let Some(effort) = extracted
        .reasoning_effort
        .filter(|effort| !effort.trim().is_empty())
    {
        config.reasoning_effort = effort.trim().to_string();
    }
    config.api_format = extracted.api_format;

    // 导入即卡片：一条 cc-switch 供应商落成一张服务商档案并立即生效。
    // 凭据目标按来源隔离——每个导入的档案一把自己的钥匙，
    // 导入第二个供应商不再覆盖第一把
    let profile_name = match provider_name.trim() {
        "" => "导入的配置",
        name => name,
    };
    let mut profile = config::profile_from_config(String::new(), profile_name, &config);
    profile.credential_service = "aglab/ccswitch".into();
    profile.credential_user = format!("provider-{source_id}");
    config.credential_service = profile.credential_service.clone();
    config.credential_user = profile.credential_user.clone();

    // 密钥只在这里过手：直接进该档案自己的凭据目标，不回前端也不进配置文件
    if let Some(key) = &extracted.key {
        keyring::Entry::new(&profile.credential_service, &profile.credential_user)
            .map_err(|e| format!("凭据条目初始化失败：{e}"))?
            .set_password(key.trim())
            .map_err(|e| format!("写入凭据失败：{e}"))?;
    }

    config::upsert_new_profile(&mut config, profile);
    config::save(&app, &config)?;
    Ok(config)
}

#[tauri::command]
pub fn ccswitch_import_pricing(app: AppHandle) -> Result<usize, String> {
    let source = open_readonly()?;
    let mut stmt = source
        .prepare(
            "SELECT model_id, display_name, input_cost_per_million, output_cost_per_million,
                    cache_read_cost_per_million, cache_creation_cost_per_million
             FROM model_pricing",
        )
        .map_err(|e| format!("读 cc-switch 价表失败：{e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(usage::Price {
                model_id: row.get(0)?,
                display_name: row.get(1)?,
                input_usd_per_m: row.get(2)?,
                output_usd_per_m: row.get(3)?,
                cache_read_usd_per_m: row.get(4)?,
                cache_creation_usd_per_m: row.get(5)?,
            })
        })
        .map_err(|e| e.to_string())?;
    let prices: Vec<usage::Price> = rows.collect::<Result<_, _>>().map_err(|e| e.to_string())?;

    let now = usage::now_ms();
    usage::with_connection(&app, |conn| {
        // 过的是和面板那条命令同一道判据。cc-switch 那张表是别人写的，
        // 一条 `-3` 进去就会把那个模型在成本面板上变成"免费"，而它其实是"没定价"——
        // 这两件事在别处是分开算的，所以宁可少写几条，也别写一条会骗人的
        let mut written = 0;
        for price in prices.iter().filter(|price| usage::check_price(price).is_ok()) {
            usage::upsert_price(conn, price, now)?;
            written += 1;
        }
        Ok(written)
    })
}

#[tauri::command]
pub fn ccswitch_mcp_candidates() -> Result<Vec<McpCandidate>, String> {
    let conn = open_readonly()?;
    let mut stmt = conn
        .prepare(
            "SELECT id, name, server_config, enabled_claude, enabled_codex, enabled_gemini
             FROM mcp_servers ORDER BY name COLLATE NOCASE",
        )
        .map_err(|e| format!("读 cc-switch 的 MCP 库失败：{e}"))?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(|e| e.to_string())?;

    let mut out = Vec::new();
    for row in rows {
        let (source_id, name, server_text, claude, codex, gemini) =
            row.map_err(|e| e.to_string())?;
        let server: Value = serde_json::from_str(&server_text).unwrap_or(Value::Null);
        // aglab 只说 stdio；http / sse 的条目导进来也起不来
        if server["type"].as_str() != Some("stdio") {
            continue;
        }
        let command = server["command"].as_str().unwrap_or_default().to_string();
        if command.trim().is_empty() {
            continue;
        }

        let env = server["env"].as_object();
        let env_keys: Vec<String> = env
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default();
        let secret_env_keys = env_keys
            .iter()
            .filter(|key| looks_secret(key))
            .cloned()
            .collect();

        let mut enabled_for = Vec::new();
        for (label, flag) in [("Claude", claude), ("Codex", codex), ("Gemini", gemini)] {
            if flag != 0 {
                enabled_for.push(label.to_string());
            }
        }

        out.push(McpCandidate {
            source_id,
            name,
            command,
            args: server["args"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            env_keys,
            secret_env_keys,
            enabled_for,
        });
    }
    Ok(out)
}

#[tauri::command]
pub fn ccswitch_import_mcp(app: AppHandle, ids: Vec<String>) -> Result<McpImportResult, String> {
    let conn = open_readonly()?;
    let mut config = config::load(&app);
    let mut added = 0usize;
    let mut skipped = 0usize;

    for id in &ids {
        if config.mcp_servers.iter().any(|server| server.id == *id) {
            skipped += 1;
            continue;
        }
        let row = conn
            .query_row(
                "SELECT name, server_config FROM mcp_servers WHERE id = ?1",
                params![id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|e| format!("读 cc-switch 失败：{e}"))?;
        let Some((name, server_text)) = row else {
            skipped += 1;
            continue;
        };
        let server: Value = serde_json::from_str(&server_text).unwrap_or(Value::Null);
        if server["type"].as_str() != Some("stdio") {
            skipped += 1;
            continue;
        }

        let env: BTreeMap<String, String> = server["env"]
            .as_object()
            .map(|map| {
                map.iter()
                    .filter_map(|(key, value)| {
                        value.as_str().map(|text| (key.clone(), text.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // cc-switch 的库里有 stdio（command+args）也有 http（type=http + url + headers）
        // 两种：按 type 分流，command 缺席而 url 在场也当 http 型收
        let is_http = server["type"].as_str() == Some("http")
            || (server["command"].as_str().unwrap_or_default().is_empty()
                && !server["url"].as_str().unwrap_or_default().is_empty());
        let headers: BTreeMap<String, String> = server["headers"]
            .as_object()
            .map(|map| {
                map.iter()
                    .filter_map(|(key, value)| {
                        value.as_str().map(|value| (key.clone(), value.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();

        config.mcp_servers.push(McpServer {
            id: id.clone(),
            oauth: false,
            name,
            transport: if is_http { "http".into() } else { "stdio".into() },
            command: server["command"].as_str().unwrap_or_default().to_string(),
            args: server["args"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            env,
            url: server["url"].as_str().unwrap_or_default().to_string(),
            headers,
            enabled: true,
        });
        added += 1;
    }

    config::save(&app, &config)?;
    Ok(McpImportResult {
        config,
        added,
        skipped,
    })
}

fn cc_skills_home() -> Result<PathBuf, String> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| "找不到主目录，定位不到 cc-switch 的技能库。".to_string())?;
    Ok(PathBuf::from(home).join(".cc-switch").join("skills"))
}

/// 把一个技能目录从 cc-switch 复制进 aglab。
/// 判定收在这里是为了测：给两个临时目录就能跑全三条路径，不用碰 Tauri。
/// Ok = 复制完成；Err(原因) = 跳过，原因原样给界面。
fn import_one(src_root: &Path, dest_root: &Path, directory: &str) -> Result<(), String> {
    let source = src_root.join(directory);
    if !source.join("SKILL.md").is_file() {
        return Err("没有 SKILL.md".into());
    }
    let dest = dest_root.join(directory);
    if dest.exists() {
        return Err("aglab 里已有同名技能".into());
    }
    copy_tree(&source, &dest).map_err(|e| format!("复制失败：{e}"))
}

/// 逐文件复制整个目录。源只是读，不动它的任何属性；
/// 目标全部新建，中途失败会留下半成品——所以调用方在判定失败时不能把 added 记上。
fn copy_tree(source: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let target = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[tauri::command]
pub fn ccswitch_skill_candidates(app: AppHandle) -> Result<Vec<SkillCandidate>, String> {
    let conn = open_readonly()?;
    let mut stmt = conn
        .prepare(
            "SELECT id, name, description, directory
             FROM skills ORDER BY name COLLATE NOCASE",
        )
        .map_err(|e| format!("读 cc-switch 的技能库失败：{e}"))?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?;

    let home = cc_skills_home()?;
    let dest_root = skills::skills_root(&app)?;

    let mut out = Vec::new();
    for row in rows {
        let (source_id, name, description, directory) = row.map_err(|e| e.to_string())?;
        // 没有目录名的行没有实体可导，界面上列出来也是死条目
        if directory.trim().is_empty() {
            continue;
        }
        out.push(SkillCandidate {
            source_id,
            name,
            description,
            directory: directory.clone(),
            has_skill_md: home.join(&directory).join("SKILL.md").is_file(),
            exists: dest_root.join(&directory).exists(),
        });
    }
    Ok(out)
}

#[tauri::command]
pub fn ccswitch_import_skills(app: AppHandle, ids: Vec<String>) -> Result<SkillImportResult, String> {
    let conn = open_readonly()?;
    let home = cc_skills_home()?;
    let dest_root = skills::skills_root(&app)?;

    let mut result = SkillImportResult {
        added: 0,
        skipped: 0,
        skipped_names: Vec::new(),
    };

    for id in &ids {
        let row = conn
            .query_row(
                "SELECT directory FROM skills WHERE id = ?1",
                params![id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| format!("读 cc-switch 失败：{e}"))?;
        let Some(directory) = row else {
            result.skipped += 1;
            result.skipped_names.push(format!("{id}（cc-switch 里已没有）"));
            continue;
        };
        if directory.trim().is_empty() {
            result.skipped += 1;
            result.skipped_names.push(format!("{directory}（没有 SKILL.md）"));
            continue;
        }

        match import_one(&home, &dest_root, &directory) {
            Ok(()) => result.added += 1,
            Err(reason) => {
                result.skipped += 1;
                result.skipped_names.push(format!("{directory}（{reason}）"));
            }
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_claude_shell_gives_its_base_url_and_key_from_env() {
        let settings = json!({"env": {
            "ANTHROPIC_AUTH_TOKEN": "sk-abc",
            "ANTHROPIC_BASE_URL": "https://ss2a.top"
        }});
        let extracted = extract("claude", &settings, &json!({})).expect("claude 壳子该解析出来");

        assert_eq!(openai_base(&extracted.base_url), "https://ss2a.top/v1");
        assert_eq!(extracted.key.as_deref(), Some("sk-abc"));
        assert_eq!(extracted.api_format, "chat");
        // claude 壳子原本是 Anthropic 协议，界面上必须说出来，不能假装它天然是 OpenAI 的
        assert!(extracted.note.contains("Anthropic"));
    }

    #[test]
    fn a_codex_shell_gives_model_effort_and_the_provider_table() {
        let settings = json!({
            "auth": { "OPENAI_API_KEY": "sk-x" },
            "config": "model_provider = \"custom\"\nmodel = \"glm-5.3-flash\"\nmodel_reasoning_effort = \"high\"\n\n[model_providers.custom]\nname = \"My Codex\"\nbase_url = \"https://ai.xmiaom.com/v1\"\nwire_api = \"responses\"\n"
        });
        let extracted = extract("codex", &settings, &json!({})).expect("codex 壳子该解析出来");

        assert_eq!(extracted.base_url, "https://ai.xmiaom.com/v1");
        assert_eq!(extracted.model.as_deref(), Some("glm-5.3-flash"));
        assert_eq!(extracted.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(extracted.api_format, "responses");
        // 已经带 /v1 的不能再拼一层
        assert_eq!(openai_base(&extracted.base_url), "https://ai.xmiaom.com/v1");
    }

    /// meta.apiFormat 是 cc-switch 替用户记下的结论，比 wire_api 反推更可信
    #[test]
    fn the_recorded_api_format_wins_over_the_toml_wire_api() {
        let settings = json!({
            "auth": { "OPENAI_API_KEY": "sk-x" },
            "config": "model = \"m\"\n\n[model_providers.custom]\nbase_url = \"https://x/v1\"\nwire_api = \"responses\"\n"
        });
        let extracted =
            extract("codex", &settings, &json!({"apiFormat": "openai_chat"})).expect("该解析出来");
        assert_eq!(extracted.api_format, "chat");
    }

    #[test]
    fn official_entries_without_an_endpoint_are_not_offered() {
        let settings = json!({"env": {}});
        assert!(extract("claude", &settings, &json!({})).is_none());
        assert!(extract("gemini", &json!({"config": {}}), &json!({})).is_none());
    }

    #[test]
    fn broken_toml_still_yields_the_key_but_no_model() {
        let settings = json!({
            "auth": { "OPENAI_API_KEY": "sk-x" },
            "config": "model = \"没闭合的字符串"
        });
        let extracted = extract("codex", &settings, &json!({}));
        // TOML 解析失败时服务商也没了，所以整条不该出现在清单里：
        // 拿一条连地址都读不出来的配置去覆盖用户现在的配置，是最坏的失败方式
        assert!(extracted.is_none());
    }

    #[test]
    fn env_names_that_look_like_secrets_are_flagged_before_import() {
        assert!(looks_secret("OPENAI_API_KEY"));
        assert!(looks_secret("Authorization"));
        assert!(!looks_secret("NODE_REPL_NODE_PATH"));
    }

    #[test]
    fn the_endpoint_preview_matches_the_wire_format() {
        assert_eq!(
            endpoint_of("https://x/v1", "responses"),
            "https://x/v1/responses"
        );
        assert_eq!(
            endpoint_of("https://x/v1", "chat"),
            "https://x/v1/chat/completions"
        );
    }

    /// 界面读的是这些键名，少一个就是一片 undefined。
    /// McpImportResult 里嵌着整份 AppConfig，用 default 填满再序列化断言。
    #[test]
    fn the_ccswitch_payloads_match_the_frontend_types() {
        let candidate = Candidate {
            source_id: String::new(),
            app_type: "claude".into(),
            name: String::new(),
            base_url: String::new(),
            model: String::new(),
            reasoning_effort: String::new(),
            api_format: "chat".into(),
            has_key: false,
            is_current: false,
            endpoint: String::new(),
            note: String::new(),
        };
        assert_eq!(
            crate::test_support::ts_interface_fields("CcswitchCandidate").len(),
            11,
            "CcswitchCandidate 在 TS 侧该有 11 个字段"
        );
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&candidate).unwrap(),
            "CcswitchCandidate",
        );

        let mcp = McpCandidate {
            source_id: String::new(),
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            env_keys: Vec::new(),
            secret_env_keys: Vec::new(),
            enabled_for: Vec::new(),
        };
        assert_eq!(
            crate::test_support::ts_interface_fields("McpCandidate").len(),
            7,
            "McpCandidate 在 TS 侧该有 7 个字段"
        );
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&mcp).unwrap(),
            "McpCandidate",
        );

        let result = McpImportResult {
            config: AppConfig::default(),
            added: 0,
            skipped: 0,
        };
        assert_eq!(
            crate::test_support::ts_interface_fields("McpImportResult").len(),
            3,
            "McpImportResult 在 TS 侧该有 3 个字段"
        );
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&result).unwrap(),
            "McpImportResult",
        );
    }

    /// P1 的技能候选与导入结果同样要对齐 TS。
    #[test]
    fn the_skill_payloads_match_the_frontend_types() {
        let candidate = SkillCandidate {
            source_id: String::new(),
            name: String::new(),
            description: String::new(),
            directory: String::new(),
            has_skill_md: false,
            exists: false,
        };
        assert_eq!(
            crate::test_support::ts_interface_fields("SkillCandidate").len(),
            6,
            "SkillCandidate 在 TS 侧该有 6 个字段"
        );
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&candidate).unwrap(),
            "SkillCandidate",
        );

        let result = SkillImportResult {
            added: 0,
            skipped: 0,
            skipped_names: Vec::new(),
        };
        assert_eq!(
            crate::test_support::ts_interface_fields("SkillImportResult").len(),
            3,
            "SkillImportResult 在 TS 侧该有 3 个字段"
        );
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&result).unwrap(),
            "SkillImportResult",
        );
    }

    /// 复制的三条判定路径：缺 SKILL.md、重名、正常复制。
    /// 全用临时目录，结束必须 remove_tree 收尾。
    #[test]
    fn a_skill_import_follows_all_three_paths() {
        use std::fs;

        let root = crate::test_support::temp_dir("skill-import");
        let src_root = root.join("src");
        let dest_root = root.join("dest");
        fs::create_dir_all(&src_root).unwrap();
        fs::create_dir_all(&dest_root).unwrap();

        // 1) 缺 SKILL.md → 跳过
        fs::create_dir_all(src_root.join("incomplete")).unwrap();
        let reason = import_one(&src_root, &dest_root, "incomplete").unwrap_err();
        assert!(reason.contains("SKILL.md"), "缺正文的跳过原因要写清楚：{reason}");

        // 2) 正常复制 → 目录结构和内容逐字一致
        let good = src_root.join("good-skill");
        fs::create_dir_all(good.join("references")).unwrap();
        fs::write(good.join("SKILL.md"), "---\nname: good\n---\n正文").unwrap();
        fs::write(good.join("references").join("note.md"), "附注").unwrap();
        import_one(&src_root, &dest_root, "good-skill").expect("完好的技能该复制成功");
        assert_eq!(
            fs::read_to_string(dest_root.join("good-skill").join("SKILL.md")).unwrap(),
            "---\nname: good\n---\n正文"
        );
        assert_eq!(
            fs::read_to_string(dest_root.join("good-skill").join("references").join("note.md"))
                .unwrap(),
            "附注"
        );

        // 3) 重名 → 跳过，且不覆盖已有的那份
        fs::write(dest_root.join("good-skill").join("SKILL.md"), "旧内容").unwrap();
        let reason = import_one(&src_root, &dest_root, "good-skill").unwrap_err();
        assert!(reason.contains("同名"), "重名的跳过原因要写清楚：{reason}");
        assert_eq!(
            fs::read_to_string(dest_root.join("good-skill").join("SKILL.md")).unwrap(),
            "旧内容",
            "重名跳过不能把 aglab 侧已有的技能覆盖掉"
        );

        crate::test_support::remove_tree(&root);
    }
}
