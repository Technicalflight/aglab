use std::collections::BTreeMap;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use sha2::Sha256;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::config;
use crate::config::McpServer;

/// 一个插件目录。按规范插件是"容器"：可以同时带命令、代理、技能、hooks 和 MCP 服务器。
/// aglab 消费技能、MCP 和钩子；命令和代理照样列出来并标清"暂未支持"，不假装读懂了。
#[derive(Debug, Clone)]
pub struct Plugin {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub author: String,
    pub category: String,
    pub path: PathBuf,
    /// manifest 的 userConfig 声明：插件向用户要的配置项（键/标签/类型/默认值）。
    /// 值存在 config.plugin_user_config 里，运行时经 ${aglab_user.KEY} 展开
    pub user_config: Vec<PluginUserConfigField>,
}

/// userConfig 的一条声明。type 只认 string/boolean/number——值在存储层统一是
/// 字符串（配置文件与变量展开都只认识字符串），类型信息归界面渲染用
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginUserConfigField {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub default_value: Option<String>,
    pub description: String,
}

impl Default for PluginUserConfigField {
    fn default() -> Self {
        Self {
            key: String::new(),
            label: String::new(),
            kind: "string".to_string(),
            default_value: None,
            description: String::new(),
        }
    }
}

fn parse_user_config(meta: &Value) -> Vec<PluginUserConfigField> {
    let Some(items) = meta.get("userConfig").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let key = item.get("key").and_then(Value::as_str)?.trim().to_string();
            if key.is_empty() {
                return None;
            }
            Some(PluginUserConfigField {
                key,
                label: item
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                kind: item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("string")
                    .to_string(),
                default_value: item
                    .get("default")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                description: item
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect()
}

impl Plugin {
    pub fn skills_dir(&self) -> PathBuf {
        self.path.join("skills")
    }

    fn mcp_file(&self) -> PathBuf {
        self.path.join(".mcp.json")
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSkill {
    /// 技能的全局键："来源/目录名"，和技能分区里的开关共用同一个 id
    pub id: String,
    pub name: String,
    pub description: String,
    pub chars: usize,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    /// plugin.json 里的作者和分类，详情页的"信息"就靠这两项
    pub author: String,
    pub category: String,
    pub path: String,
    pub enabled: bool,
    /// 这个插件带来的技能，正文不进上下文，只列清单
    pub skills: Vec<PluginSkill>,
    pub mcp_servers: Vec<String>,
    /// 生命周期钩子。没逐条确认过内容的，runs 一律是 false
    pub hooks: Vec<crate::hooks::HookView>,
    /// hooks.json 里被跳过的部分和原因。看不见自己写的钩子为什么没生效，是最难受的失败
    pub hook_notes: Vec<String>,
    pub commands: usize,
    pub agents: usize,
}

fn root(app: &AppHandle) -> Result<PathBuf, String> {
    root_in(&app.path().app_data_dir().map_err(|e| e.to_string())?)
}

fn root_in(data_dir: &std::path::Path) -> Result<PathBuf, String> {
    let dir = data_dir.join("plugins");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn count_md(dir: &Path) -> usize {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    entry
                        .path()
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
                })
                .count()
        })
        .unwrap_or(0)
}

fn read_plugin(dir: &Path) -> Option<Plugin> {
    let id = dir.file_name()?.to_string_lossy().trim().to_string();
    if id.is_empty() || !dir.is_dir() {
        return None;
    }

    // 元数据在 .claude-plugin/plugin.json，缺了就退回目录名
    let meta = fs::read_to_string(dir.join(".claude-plugin").join("plugin.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());

    let field = |key: &str| -> String {
        meta.as_ref()
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string()
    };

    // author 三种写法都见过："张三"、{"name":"张三"}、{"name":"张三","email":…}
    let author = meta
        .as_ref()
        .and_then(|value| value.get("author"))
        .map(|value| {
            value
                .as_str()
                .or_else(|| value.get("name").and_then(Value::as_str))
                .unwrap_or_default()
                .trim()
                .to_string()
        })
        .unwrap_or_default();

    Some(Plugin {
        name: {
            let value = field("name");
            if value.is_empty() {
                id.clone()
            } else {
                value
            }
        },
        description: field("description"),
        version: field("version"),
        author,
        category: field("category"),
        user_config: meta
            .as_ref()
            .map(parse_user_config)
            .unwrap_or_default(),
        id,
        path: dir.to_path_buf(),
    })
}

// ---- 运行时分区与 ${aglab_*} 变量 ----
//
// 插件有三块地皮：
// * **marketplace/安装区**（app_data/plugins/<id>，重装会重建）；
// * **data**（app_data/plugins-data/<id>，持久——重装/升级不动它）；
// * **cache**（app_data/plugins-cache/<id>，用完可扔，随时可清）。
// manifest 与 .mcp.json 里写 ${aglab_plugin_data} / ${aglab_plugin_cache} 等占位符，
// 在消费那一刻展开成真实路径——写死的绝对路径一搬家就断。

/// 插件的持久数据目录（按需创建）。重装与升级都不动它
pub fn plugin_data_dir(app: &AppHandle, plugin_id: &str) -> Result<PathBuf, String> {
    plugin_data_dir_in(&app.path().app_data_dir().map_err(|e| e.to_string())?, plugin_id)
}

fn plugin_data_dir_in(data_dir: &std::path::Path, plugin_id: &str) -> Result<PathBuf, String> {
    let dir = data_dir.join("plugins-data").join(sanitize_id(plugin_id));
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// 插件的缓存目录（按需创建）。里面是什么只有插件自己知道，随时可以整个清掉
pub fn plugin_cache_dir(app: &AppHandle, plugin_id: &str) -> Result<PathBuf, String> {
    plugin_cache_dir_in(&app.path().app_data_dir().map_err(|e| e.to_string())?, plugin_id)
}

fn plugin_cache_dir_in(data_dir: &std::path::Path, plugin_id: &str) -> Result<PathBuf, String> {
    let dir = data_dir.join("plugins-cache").join(sanitize_id(plugin_id));
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn sanitize_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if cleaned.is_empty() { "unknown".into() } else { cleaned }
}

/// ${aglab_*} 变量展开的上下文。与 AppHandle 解耦，纯函数可测
pub struct ExpansionContext {
    pub plugin_data: PathBuf,
    pub plugin_cache: PathBuf,
    pub workspace: PathBuf,
    pub os: &'static str,
    pub arch: &'static str,
    /// manifest userConfig 的当前值（${aglab_user.KEY} 的出处）
    pub user_values: BTreeMap<String, String>,
}

pub fn expansion_context(app: &AppHandle, plugin_id: &str) -> ExpansionContext {
    expansion_context_in(&config::load(app), &app.path().app_data_dir().map_err(|e| e.to_string()).unwrap_or_default(), plugin_id)
}

/// worker 进程的变体（M2 切片 4）：配置与数据目录由调用方传入
pub fn expansion_context_in(
    config: &config::AppConfig,
    data_dir: &std::path::Path,
    plugin_id: &str,
) -> ExpansionContext {
    let workspace = config
        .active_project()
        .map(|project| PathBuf::from(project.path.clone()))
        .unwrap_or_else(|| dirs_or_home(config));
    let user_values = config
        .plugin_user_config
        .get(plugin_id)
        .cloned()
        .unwrap_or_default();
    ExpansionContext {
        plugin_data: plugin_data_dir_in(data_dir, plugin_id).unwrap_or_else(|_| std::env::temp_dir()),
        plugin_cache: plugin_cache_dir_in(data_dir, plugin_id).unwrap_or_else(|_| std::env::temp_dir()),
        workspace,
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        user_values,
    }
}

fn dirs_or_home(config: &config::AppConfig) -> PathBuf {
    config
        .effective_root()
        .unwrap_or_else(|| std::env::var("USERPROFILE").map(PathBuf::from).unwrap_or_default())
}

/// 展开一段文本里的 ${aglab_*} 占位符。认不出的占位符原样保留——
/// 让作者一眼看见写错了的变量名，比悄悄替换成空串诚实
pub fn expand_with(context: &ExpansionContext, text: &str) -> String {
    let mut out = text.to_string();
    let pairs = [
        ("${aglab_plugin_data}", context.plugin_data.to_string_lossy().into_owned()),
        ("${aglab_plugin_cache}", context.plugin_cache.to_string_lossy().into_owned()),
        ("${aglab_workspace}", context.workspace.to_string_lossy().into_owned()),
        ("${aglab_os}", context.os.to_string()),
        ("${aglab_arch}", context.arch.to_string()),
    ];
    for (placeholder, value) in pairs {
        out = out.replace(placeholder, &value);
    }
    for (key, value) in &context.user_values {
        out = out.replace(&format!("${{aglab_user.{key}}}"), value);
    }
    out
}

/// 消费点的展开入口：按插件身份取上下文
pub fn expand_variables(app: &AppHandle, plugin_id: &str, text: &str) -> String {
    expand_with(&expansion_context(app, plugin_id), text)
}

/// 某插件的 userConfig：schema（声明）+ 当前值（缺省用声明里的 default 补）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginUserConfigView {
    pub schema: Vec<PluginUserConfigField>,
    pub values: BTreeMap<String, String>,
}

#[tauri::command]
pub fn plugin_user_config_get(app: AppHandle, id: String) -> Result<PluginUserConfigView, String> {
    let plugin = installed(&app)
        .into_iter()
        .find(|plugin| plugin.id == id)
        .ok_or_else(|| format!("插件「{id}」不存在。"))?;
    let config = config::load(&app);
    let values = config.plugin_user_config.get(&id).cloned().unwrap_or_default();
    // schema 里声明了而用户没存过的，用 default 预填——界面与展开看到的是同一份
    let mut values = values;
    for field in &plugin.user_config {
        values
            .entry(field.key.clone())
            .or_insert_with(|| field.default_value.clone().unwrap_or_default());
    }
    Ok(PluginUserConfigView { schema: plugin.user_config, values })
}

#[tauri::command]
pub fn plugin_user_config_set(
    app: AppHandle,
    id: String,
    key: String,
    value: String,
) -> Result<(), String> {
    let plugin = installed(&app)
        .into_iter()
        .find(|plugin| plugin.id == id)
        .ok_or_else(|| format!("插件「{id}」不存在。"))?;
    if !plugin.user_config.iter().any(|field| field.key == key) {
        return Err(format!("插件「{id}」没有声明配置项「{key}」，拒绝写入。"));
    }
    let mut config = config::load(&app);
    config
        .plugin_user_config
        .entry(id)
        .or_default()
        .insert(key, value);
    config::save(&app, &config)
}

#[tauri::command]
pub fn plugin_cache_clear(app: AppHandle, id: String) -> Result<(), String> {
    let base = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let dir = base.join("plugins-cache").join(sanitize_id(&id));
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|e| format!("清缓存失败：{e}"))?;
    }
    Ok(())
}

pub fn installed(app: &AppHandle) -> Vec<Plugin> {
    let Ok(dir) = root(app) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut plugins: Vec<Plugin> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter_map(|path| read_plugin(&path))
        .collect();
    plugins.sort_by(|a, b| a.name.cmp(&b.name));
    plugins
}

/// 启用中的插件。关掉插件是"一批一起停"：它带来的技能、MCP 服务和钩子同时消失
pub fn enabled(app: &AppHandle) -> Vec<Plugin> {
    enabled_in(&config::load(app), &app.path().app_data_dir().map_err(|e| e.to_string()).unwrap_or_default())
}

/// worker 进程的变体（M2 切片 4）：目录与配置都由调用方传入。
/// installed 走同一份 root_in，目录里有什么就是什么
pub fn enabled_in(config: &config::AppConfig, data_dir: &std::path::Path) -> Vec<Plugin> {
    installed_in(data_dir)
        .into_iter()
        .filter(|plugin| !config.disabled_plugins.iter().any(|id| id == &plugin.id))
        .collect()
}

/// worker 进程的变体（M2 切片 4）：插件安装区从 data 目录派生
pub fn installed_in(data_dir: &std::path::Path) -> Vec<Plugin> {
    let dir = root_in(data_dir).unwrap_or_default();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut plugins: Vec<Plugin> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter_map(|path| read_plugin(&path))
        .collect();
    plugins.sort_by(|a, b| a.name.cmp(&b.name));
    plugins
}

/// 启用中的插件通过 `.mcp.json` 贡献的 MCP 服务器。
/// id 带上插件前缀，避免两个插件用了同名服务器时互相顶掉。
/// command/args/env 里的 ${aglab_*} 变量在这里（消费那一刻）展开
pub fn mcp_servers(app: &AppHandle) -> Vec<(String, McpServer)> {
    mcp_servers_in(&config::load(app), &app.path().app_data_dir().map_err(|e| e.to_string()).unwrap_or_default())
}

/// worker 进程的变体（M2 切片 4）
pub fn mcp_servers_in(
    config: &config::AppConfig,
    data_dir: &std::path::Path,
) -> Vec<(String, McpServer)> {
    let mut servers = Vec::new();

    for plugin in enabled_in(config, data_dir) {
        let Ok(text) = fs::read_to_string(plugin.mcp_file()) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            eprintln!("插件「{}」的 .mcp.json 不是合法 JSON，已跳过", plugin.name);
            continue;
        };

        let Some(map) = value.get("mcpServers").and_then(Value::as_object) else {
            continue;
        };

        for (key, entry) in map {
            let command = entry["command"].as_str().unwrap_or_default().to_string();
            if command.is_empty() {
                continue;
            }
            let args: Vec<String> = entry["args"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let env: BTreeMap<String, String> = entry["env"]
                .as_object()
                .map(|map| {
                    map.iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|text| (key.clone(), text.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default();

            // 变量在消费那一刻展开：data/cache 目录按需建，user 值读当前配置
            let expand = |text: String| {
                expand_with(
                    &expansion_context_in(config, data_dir, &plugin.id),
                    &text,
                )
            };
            servers.push((
                plugin.name.clone(),
                McpServer {
                    id: format!("{}::{}", plugin.id, key),
                    name: key.clone(),
                    transport: "stdio".into(),
                    command: expand(command),
                    args: args.into_iter().map(expand).collect(),
                    env: env.into_iter().map(|(k, v)| (k, expand(v))).collect(),
                    url: String::new(),
                    headers: BTreeMap::new(),
                    oauth: false,
                    enabled: true,
                },
            ));
        }
    }

    servers
}

/// 插件页的出厂扩展条目（design-builtin-extensions.md）。定义住在
/// `crate::builtins`，这里只把名册翻成界面要的形状；整扩开关 = `disabled_builtins`
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuiltinView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    /// 它带的技能，正文不进上下文，只列清单（键 = 技能全局键，与技能分区共用开关）
    pub skills: Vec<PluginSkill>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginsListing {
    /// 插件目录，界面上要显示它，用户往这里放
    pub dir: String,
    pub plugins: Vec<PluginView>,
    pub builtins: Vec<BuiltinView>,
}

// ---- 官方市场与安装（CDN zip + sha256）----
//
// 市场清单是 aglab 官网发布的一份静态 JSON（GitHub Pages，走 gh-pages 的 CDN），
// 每个条目自带下载地址与内容指纹：安装 = 下载 → 先验 sha256 → 再解压 →
// 重扫描即见。指纹对不上就整个拒绝——CDN 可能被缓存污染，指纹是最后一道闸。

const MARKETPLACE_URL: &str = "https://technicalflight.github.io/aglab-site/plugins/marketplace.json";
const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MarketEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub author: String,
    pub download_url: String,
    /// 下载包的 sha256（小写十六进制）。缺了就没有可校验的指纹，不装
    pub sha256: String,
}

impl Default for MarketEntry {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            description: String::new(),
            version: String::new(),
            author: String::new(),
            download_url: String::new(),
            sha256: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketView {
    pub entries: Vec<MarketEntry>,
    /// 每个条目当前装没装（按 id 对目录名，装过的条目不再重复安装）
    pub installed_ids: Vec<String>,
    pub source: String,
}

#[tauri::command]
pub async fn plugin_market_list() -> Result<MarketView, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let response = crate::net::with_timeouts(
            ureq::get(MARKETPLACE_URL),
            std::time::Duration::from_secs(20),
        )
        .call()
        .map_err(|e| format!("拉取官方市场清单失败：{e}"))?;
        if !response.status().is_success() {
            return Err(format!("官方市场清单回了 {}。", response.status()));
        }
        let text = response
            .into_body()
            .read_to_string()
            .map_err(|e| format!("{e}"))?;
        let entries: Vec<MarketEntry> = serde_json::from_str(&text)
            .map_err(|e| format!("市场清单不是预期形状：{e}"))?;
        Ok(entries)
    })
    .await
    .map_err(|e| format!("市场任务中断：{e}"))?
    .map(|entries| {
        let installed_ids = entries
            .iter()
            .filter(|entry| !entry.id.is_empty())
            .map(|entry| entry.id.clone())
            .collect();
        MarketView {
            entries,
            installed_ids,
            source: MARKETPLACE_URL.to_string(),
        }
    })
}

/// 下载 zip → sha256 校验 → 解压进插件根的一个新目录。校验发生在解压之前：
/// 被污染的 CDN 包连落地的机会都没有
#[tauri::command]
pub async fn plugin_market_install(
    app: AppHandle,
    id: String,
    download_url: String,
    sha256: String,
) -> Result<String, String> {
    let wanted_id = id.trim().to_string();
    if wanted_id.is_empty() || !wanted_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("插件 id 只允许字母数字与连字符。".into());
    }
    if !download_url.starts_with("https://") {
        return Err("下载地址必须是 https。".into());
    }
    let wanted = sha256.trim().to_ascii_lowercase();
    if wanted.len() != 64 || !wanted.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("sha256 指纹缺了或形状不对，拒装——没有指纹的包无法校验。".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let response = crate::net::with_timeouts(
            ureq::get(&download_url),
            std::time::Duration::from_secs(600),
        )
        .call()
        .map_err(|e| format!("下载插件包失败：{e}"))?;
        if !response.status().is_success() {
            return Err(format!("下载回了 {}。", response.status()));
        }
        let mut bytes = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(MAX_DOWNLOAD_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("下载中断：{e}"))?;
        if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
            return Err("安装包超出 64 MB 上限。".into());
        }
        let actual = {
            use sha2::Digest as _;
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            format!("{:x}", hasher.finalize())
        };
        if actual != wanted {
            return Err(format!(
                "安装包指纹对不上：清单说 {wanted}，实际 {actual}。CDN 缓存可能被污染，拒绝安装。"
            ));
        }
        let dir = root(&app)?.join(&wanted_id);
        if dir.exists() {
            return Err(format!("插件目录「{wanted_id}」已存在，先卸载同名插件再装。"));
        }
        install_plugin_zip(&bytes, &dir)?;
        // 运行时重插件（manifest 声明 heavyRuntime）装上即禁用：它可能带
        // 常驻进程/整窗注入一类的重副作用，"装好就开跑"不是默认该有的行为——
        // 用户在插件页看过清单、点过启用，它才真正开始工作
        let mut note = String::new();
        if manifest_declares_heavy_runtime(&dir) {
            let mut config = config::load(&app);
            config.disabled_plugins.push(wanted_id.clone());
            config::save(&app, &config)?;
            note = "（声明了重运行时，已默认禁用——在插件页手动启用）".into();
        }
        Ok(format!("已安装到 {}{note}", dir.display()))
    })
    .await
    .map_err(|e| format!("安装任务中断：{e}"))?
}

/// manifest 声明的重运行时：`.claude-plugin/plugin.json` 里
/// `"aglab": {"heavyRuntime": true}`。认不出/读不到一律按"不是重插件"——
/// 拒装发生在更早的指纹闸，这里只决定装上后开不开
fn manifest_declares_heavy_runtime(dir: &Path) -> bool {
    fs::read_to_string(dir.join(".claude-plugin").join("plugin.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|meta| {
            meta.get("aglab")
                .and_then(|aglab| aglab.get("heavyRuntime"))
                .and_then(Value::as_bool)
        })
        .unwrap_or(false)
}

/// 插件包解压：与技能包同一套恶意条目守卫（zip-slip 越界、符号链接条目），
/// 但不要求根上有 SKILL.md——插件的身份是 .claude-plugin/plugin.json，
/// 解压完校验清单存在，没有清单的包当场清理
fn install_plugin_zip(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("安装包不是合法的 zip：{e}"))?;
    fs::create_dir_all(dest).map_err(|e| format!("创建插件目录失败：{e}"))?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("读取安装包条目失败：{e}"))?;
        let Some(relative) = entry.enclosed_name() else {
            return Err(format!(
                "安装包里有越界路径「{}」，拒绝安装。",
                entry.name()
            ));
        };
        if entry.unix_mode().is_some_and(|mode| (mode >> 12) & 0o17 == 0o12) {
            return Err(format!(
                "安装包里有符号链接条目「{}」，拒绝安装。",
                entry.name()
            ));
        }
        if entry.is_dir() {
            fs::create_dir_all(dest.join(&relative)).map_err(|e| format!("创建目录失败：{e}"))?;
            continue;
        }
        if let Some(parent) = dest.join(&relative).parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{e}"))?;
        }
        let mut out =
            fs::File::create(dest.join(&relative)).map_err(|e| format!("写文件失败：{e}"))?;
        std::io::copy(&mut entry, &mut out).map_err(|e| format!("写文件失败：{e}"))?;
    }
    if !dest.join(".claude-plugin").join("plugin.json").exists() {
        let _ = fs::remove_dir_all(dest);
        return Err("安装包里没有 .claude-plugin/plugin.json——这不是一个插件包，已清理。".into());
    }
    Ok(())
}

#[tauri::command]
pub fn plugins_list(app: AppHandle) -> Result<PluginsListing, String> {
    let config = config::load(&app);
    // 技能清单从 skills 那边取，键就是界面上那个开关的 id，两处不会各自数一遍
    let docs = crate::skills::scan(&app)?;

    Ok(PluginsListing {
        dir: root(&app)?.display().to_string(),
        builtins: crate::builtins::extensions()
            .iter()
            .map(|extension| BuiltinView {
                id: extension.id.to_string(),
                name: extension.name.to_string(),
                description: extension.description.to_string(),
                enabled: !config.disabled_builtins.iter().any(|id| id == extension.id),
                skills: extension
                    .skills
                    .iter()
                    .map(|skill| {
                        let key = extension.skill_key(skill.folder);
                        PluginSkill {
                            id: key.clone(),
                            name: skill.name.to_string(),
                            description: skill.description.to_string(),
                            chars: skill.body.chars().count(),
                            enabled: !config.disabled_skills.iter().any(|id| id == &key),
                        }
                    })
                    .collect(),
            })
            .collect(),
        plugins: installed(&app)
            .into_iter()
            .map(|plugin| {
                let declared: Vec<String> = fs::read_to_string(plugin.mcp_file())
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    .and_then(|value| {
                        Some(
                            value
                                .get("mcpServers")?
                                .as_object()?
                                .keys()
                                .cloned()
                                .collect(),
                        )
                    })
                    .unwrap_or_default();

                let prefix = format!("{}/", plugin.name);
                let skills = docs
                    .iter()
                    .filter(|doc| doc.key.starts_with(&prefix))
                    .map(|doc| PluginSkill {
                        id: doc.key.clone(),
                        name: doc.name.clone(),
                        description: doc.description.clone(),
                        chars: doc.body.chars().count(),
                        enabled: !config.disabled_skills.iter().any(|id| id == &doc.key),
                    })
                    .collect();

                let (found, hook_notes) = crate::hooks::for_plugin(&plugin);
                let hooks = found
                    .iter()
                    .map(|hook| crate::hooks::view_of(hook, &config))
                    .collect();

                PluginView {
                    skills,
                    mcp_servers: declared,
                    hooks,
                    hook_notes,
                    commands: count_md(&plugin.path.join("commands")),
                    agents: count_md(&plugin.path.join("agents")),
                    author: plugin.author,
                    category: plugin.category,
                    id: plugin.id.clone(),
                    enabled: !config.disabled_plugins.iter().any(|id| id == &plugin.id),
                    name: plugin.name,
                    description: plugin.description,
                    version: plugin.version,
                    path: plugin.path.display().to_string(),
                }
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn a_plugin_directory_reports_what_it_carries() {
        let base = crate::test_support::temp_dir("plugins");
        let plugin = base.join("frontend-design");

        write(
            &plugin.join(".claude-plugin").join("plugin.json"),
            r#"{"name":"frontend-design","description":"更好看的前端界面","version":"1.2.0","author":{"name":"前端组","email":"fe@example.com"},"category":"效率工具"}"#,
        );
        write(
            &plugin.join("skills").join("grid").join("SKILL.md"),
            "---\ndescription: 排版\n---\n正文\n",
        );
        write(&plugin.join("skills").join("notaskill.md"), "x");
        write(&plugin.join("commands").join("hello.md"), "x");
        write(&plugin.join("agents").join("helper.md"), "x");
        write(
            &plugin.join("hooks").join("hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"check.py"},{"type":"prompt","command":"提醒我看测试"}]}]}}"#,
        );
        write(
            &plugin.join(".mcp.json"),
            r#"{"mcpServers":{"postgres":{"command":"npx","args":["-y","pg-mcp"]},"tiny":{"command":"x"}}}"#,
        );

        let read = read_plugin(&plugin).expect("该认出这个插件");
        assert_eq!(read.id, "frontend-design");
        assert_eq!(read.name, "frontend-design");
        assert_eq!(read.version, "1.2.0");
        assert_eq!(read.author, "前端组", "author 是个对象时取里面的名字");
        assert_eq!(read.category, "效率工具");
        assert_eq!(count_md(&read.path.join("commands")), 1);
        assert_eq!(count_md(&read.path.join("agents")), 1);
        let (hooks, notes) = crate::hooks::for_plugin(&read);
        assert_eq!(hooks.len(), 1, "只有 command 处理器会被收进来");
        assert_eq!(notes.len(), 1, "被丢掉的那条要留下话：{notes:?}");
        assert!(
            notes[0].contains("prompt"),
            "得说清是哪一类处理器：{notes:?}"
        );

        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn the_plugin_payload_matches_the_frontend_types() {
        let skill = PluginSkill {
            id: "示例/技能".into(),
            name: "技能".into(),
            description: "什么时候用".into(),
            chars: 12,
            enabled: true,
        };
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&skill).unwrap(),
            "PluginSkill",
        );

        let view = PluginView {
            id: "p".into(),
            name: "示例".into(),
            description: String::new(),
            version: String::new(),
            author: String::new(),
            category: String::new(),
            path: String::new(),
            enabled: true,
            skills: vec![skill],
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            hook_notes: Vec::new(),
            commands: 0,
            agents: 0,
        };
        crate::test_support::assert_matches_ts(&serde_json::to_value(view).unwrap(), "PluginView");
    }

    /// 出厂扩展条目的 IPC 形状：插件页整组读 `builtins`，字段名对不上
    /// 就是卡片静默缺格——「出厂徽章怎么没了」先查这里
    #[test]
    fn the_builtin_view_payload_matches_the_frontend_type() {
        let extension = crate::builtins::extensions()
            .into_iter()
            .next()
            .expect("出厂名册不该是空的");
        let view = BuiltinView {
            id: extension.id.to_string(),
            name: extension.name.to_string(),
            description: extension.description.to_string(),
            enabled: true,
            skills: extension
                .skills
                .iter()
                .map(|skill| PluginSkill {
                    id: extension.skill_key(skill.folder),
                    name: skill.name.to_string(),
                    description: skill.description.to_string(),
                    chars: skill.body.chars().count(),
                    enabled: true,
                })
                .collect(),
        };
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&view).unwrap(),
            "BuiltinView",
        );
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&view.skills[0]).unwrap(),
            "PluginSkill",
        );
    }

    #[test]
    fn a_folder_without_manifest_is_still_a_plugin_named_after_the_folder() {
        let base = crate::test_support::temp_dir("plugins-bare");
        let plugin = base.join("loose");
        fs::create_dir_all(&plugin).unwrap();

        let read = read_plugin(&plugin).expect("空目录也该被认出来");
        assert_eq!(read.id, "loose");
        assert_eq!(read.name, "loose");
        assert!(read.description.is_empty());

        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn broken_mcp_manifests_are_skipped_not_fatal() {
        let base = crate::test_support::temp_dir("plugins-broken");
        let plugin = base.join("bad");
        fs::create_dir_all(&plugin).unwrap();
        write(&plugin.join(".mcp.json"), "{ this is not json");

        let read = read_plugin(&plugin).unwrap();
        let text = fs::read_to_string(read.mcp_file()).unwrap();
        assert!(serde_json::from_str::<Value>(&text).is_err());

        crate::test_support::remove_tree(&base);
    }


    #[test]
    fn enabled_in_reads_the_passed_data_dir_and_respects_disabled() {
        let base = crate::test_support::temp_dir("plugins-enabled-in");
        let manifest = base.join("plugins").join("demo").join(".claude-plugin");
        fs::create_dir_all(&manifest).unwrap();
        fs::write(manifest.join("plugin.json"), r#"{"name":"演示插件"}"#).unwrap();

        let config = crate::config::AppConfig::default();
        assert_eq!(enabled_in(&config, &base).len(), 1, "目录里的插件要读出来");

        let mut disabled = crate::config::AppConfig::default();
        disabled.disabled_plugins.push("demo".into());
        assert!(
            enabled_in(&disabled, &base).is_empty(),
            "关掉的插件一个不剩"
        );
        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn aglab_variables_expand_at_consume_time() {
        let context = ExpansionContext {
            plugin_data: PathBuf::from("/data/demo"),
            plugin_cache: PathBuf::from("/cache/demo"),
            workspace: PathBuf::from("/ws/demo"),
            os: "windows",
            arch: "x86_64",
            user_values: BTreeMap::from([("apiKey".into(), "sk-test".into())]),
        };

        let expanded = expand_with(
            &context,
            "node ${aglab_plugin_data}/server.js --cache=${aglab_plugin_cache} --root=${aglab_workspace} --os=${aglab_os}/${aglab_arch} --key=${aglab_user.apiKey}",
        );
        assert!(expanded.contains("/data/demo/server.js"), "{expanded}");
        assert!(expanded.contains("--cache=/cache/demo"), "{expanded}");
        assert!(expanded.contains("--root=/ws/demo"), "{expanded}");
        assert!(expanded.contains("--os=windows/x86_64"), "{expanded}");
        assert!(expanded.contains("--key=sk-test"), "{expanded}");

        // 认不出的占位符原样保留：让作者看见写错的变量名，比悄悄换成空串诚实
        let untouched = expand_with(&context, "${aglab_whoami}");
        assert_eq!(untouched, "${aglab_whoami}");
    }

    #[test]
    fn manifest_declaring_heavy_runtime_is_recognized() {
        let base = crate::test_support::temp_dir("plugins-heavy");
        let manifest = base.join(".claude-plugin").join("plugin.json");
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();

        fs::write(
            &manifest,
            r#"{"name":"重插件","aglab":{"heavyRuntime":true}}"#,
        )
        .unwrap();
        assert!(manifest_declares_heavy_runtime(&base));

        fs::write(&manifest, r#"{"name":"轻插件"}"#).unwrap();
        assert!(!manifest_declares_heavy_runtime(&base), "没声明就不是重插件");
        fs::write(&manifest, "不是 JSON").unwrap();
        assert!(
            !manifest_declares_heavy_runtime(&base),
            "读不出的 manifest 按\"不是重插件\"处理，拒装是更早那道闸的事"
        );
        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn user_config_schema_parses_from_the_manifest() {
        let base = crate::test_support::temp_dir("plugins-user-config");
        let manifest = base.join(".claude-plugin").join("plugin.json");
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        fs::write(
            &manifest,
            r#"{"name":"带配置的插件","userConfig":[
                {"key":"apiKey","label":"API Key","type":"string","default":"sk-x","description":"服务商密钥"},
                {"key":"verbose","type":"boolean"}
            ]}"#,
        )
        .unwrap();
        let plugin = read_plugin(&base).expect("插件要读得出来");
        assert_eq!(plugin.user_config.len(), 2);
        assert_eq!(plugin.user_config[0].key, "apiKey");
        assert_eq!(plugin.user_config[0].default_value.as_deref(), Some("sk-x"));
        assert_eq!(plugin.user_config[1].kind, "boolean");
        crate::test_support::remove_tree(&base);
    }
}
