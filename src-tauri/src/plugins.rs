use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
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
    let base = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let dir = base.join("plugins");
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
        id,
        path: dir.to_path_buf(),
    })
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
    let disabled = config::load(app).disabled_plugins;
    installed(app)
        .into_iter()
        .filter(|plugin| !disabled.iter().any(|id| id == &plugin.id))
        .collect()
}

/// 启用中的插件通过 `.mcp.json` 贡献的 MCP 服务器。
/// id 带上插件前缀，避免两个插件用了同名服务器时互相顶掉。
pub fn mcp_servers(app: &AppHandle) -> Vec<(String, McpServer)> {
    let mut servers = Vec::new();

    for plugin in enabled(app) {
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
            let args = entry["args"]
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

            servers.push((
                plugin.name.clone(),
                McpServer {
                    id: format!("{}::{}", plugin.id, key),
                    name: key.clone(),
                    transport: "stdio".into(),
                    command,
                    args,
                    env,
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
}
