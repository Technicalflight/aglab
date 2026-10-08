use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::config;
use crate::plugins;

/// 市场请求的等待上限。列表页不大，20 秒足够；不设上限会把设置页挂死
const SKILLHUB_TIMEOUT: Duration = Duration::from_secs(20);

/// 系统提示词里只放清单，正文由模型自己调 load_skill 取，所以这里限制的是"清单条目数"
/// 而不是正文字数。
pub const LISTING_CAP: usize = 60;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    /// 全局键：来源目录 + 技能目录名
    pub id: String,
    pub name: String,
    pub description: String,
    /// SKILL.md 里声明的工具白名单，空表示不额外限制
    pub allowed_tools: Vec<String>,
    pub chars: usize,
    /// 正文开头的一小段，给列表用。注入给模型的只有清单，这段不进上下文
    pub preview: String,
    pub path: String,
    /// "个人" 或插件名
    pub source: String,
    pub enabled: bool,
}

pub(crate) struct Doc {
    pub key: String,
    pub path: PathBuf,
    pub name: String,
    pub description: String,
    pub allowed_tools: Vec<String>,
    pub body: String,
}

pub(crate) fn skills_root(app: &AppHandle) -> Result<PathBuf, String> {
    skills_root_in(&app.path().app_data_dir().map_err(|e| e.to_string())?)
}

pub(crate) fn skills_root_in(data_dir: &std::path::Path) -> Result<PathBuf, String> {
    let dir = data_dir.join("skills");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// 一个技能来源目录：个人 skills/ 或某个插件的 skills/
pub(crate) struct Source {
    pub label: String,
    pub dir: PathBuf,
}

pub(crate) fn sources(app: &AppHandle) -> Result<Vec<Source>, String> {
    sources_in(&app.path().app_data_dir().map_err(|e| e.to_string())?)
}

/// worker 进程的变体（M3 第 2 档）
pub(crate) fn sources_in(data_dir: &std::path::Path) -> Result<Vec<Source>, String> {
    let mut list = vec![Source {
        label: "个人".into(),
        dir: skills_root_in(data_dir)?,
    }];

    // 四生态目录约定（zcode/claude/codex/cursor）：别的工具攒下的技能照单全收，
    // 只读不写——写回哪个生态都是替用户做决定。目录不存在就跳过，不报错：
    // 没装 Claude Code 的人不该在技能页看到一个报错
    list.extend(ecosystem_sources());

    for plugin in plugins::installed_in(data_dir) {
        list.push(Source {
            label: plugin.name.clone(),
            dir: plugin.skills_dir(),
        });
    }

    Ok(list)
}

/// 三个外部生态的技能目录约定。标签就是 key 的来源段（claude/xxx），
/// 与个人技能同粒度地进启用/禁用清单
fn ecosystem_sources() -> Vec<Source> {
    let Some(home) = crate::config::home_root() else {
        return Vec::new();
    };
    [
        ("claude", home.join(".claude").join("skills")),
        ("codex", home.join(".codex").join("skills")),
        ("cursor", home.join(".cursor").join("skills")),
    ]
    .into_iter()
    .filter(|(_, dir)| dir.is_dir())
    .map(|(label, dir)| Source { label: label.to_string(), dir })
    .collect()
}

/// YAML 块标量的两种写法。`>` 折叠：同段行与行之间用空格接；`|` 字面：保留换行。
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockStyle {
    Folded,
    Literal,
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// 值的位置上只写了 `>`/`|`（可带 chomping 的 `-`/`+` 与一位缩进数字）就是块标量的开头。
/// 认不出后缀时按普通值处理：`description: |pipe 分隔` 这种写法不该被吞掉。
/// chomping 的三种差别只在末尾空白，而清单与预览两头都要 trim，所以这里不区分
fn block_style_of(value: &str) -> Option<BlockStyle> {
    let style = match value.chars().next()? {
        '>' => BlockStyle::Folded,
        '|' => BlockStyle::Literal,
        _ => return None,
    };
    if value
        .chars()
        .skip(1)
        .all(|mark| mark.is_ascii_digit() || mark == '-' || mark == '+')
    {
        Some(style)
    } else {
        None
    }
}

/// 从 `rest` 的开头收一整块。块的基准缩进是第一行非空行的缩进，它为零说明值就写在
/// 同一行、没有块。收进来的行已剥掉基准缩进，返回的第二个值是"吃掉了多少行"——
/// 主循环要靠它跳过这些行，否则块里带冒号的那一行会被当成另一条键
fn take_block(rest: &[&str]) -> (Vec<String>, usize) {
    let Some(base) = rest.iter().find(|line| !line.trim().is_empty()).map(|line| indent_of(line)) else {
        return (Vec::new(), 0);
    };
    if base == 0 {
        return (Vec::new(), 0);
    }

    let mut body: Vec<String> = Vec::new();
    for line in rest {
        if line.trim().is_empty() {
            body.push(String::new());
            continue;
        }
        if indent_of(line) < base {
            break;
        }
        body.push(line[base..].to_string());
    }
    // 块尾的空行不属于值，也不属于下一条目——交回主循环，"吃掉多少行"按退回去的量算
    let taken = body.len() - body.iter().rev().take_while(|line| line.is_empty()).count();
    body.truncate(taken);
    (body, taken)
}

fn join_block(body: &[String], style: BlockStyle) -> String {
    if style == BlockStyle::Literal {
        return body.join("\n").trim_end().to_string();
    }
    let mut out = String::new();
    let mut previous: Option<bool> = None;
    for line in body {
        let text = line.trim_end();
        if text.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            previous = None;
            continue;
        }
        // 比基准更深缩进的行（列表项、代码片段）之间不折叠——YAML 的"更缩进即字面"，
        // 摊成一行会让"Use when the user says:" 后面那串触发词糊成一句话
        let deep = indent_of(line) > 0;
        if let Some(previous_deep) = previous {
            out.push(if deep || previous_deep { '\n' } else { ' ' });
        }
        out.push_str(text);
        previous = Some(deep);
    }
    out
}

/// frontmatter。按行切第一个冒号，**外加** YAML 的块标量：`description: >` 后面那几行
/// 属于这个值。少了块标量这一半，用折叠写法（Agent Skills 的常见形状）的技能会读成一个
/// 字面量 ">"，而清单里那一行描述正是模型决定"要不要 load 它"的唯一线索——等于让技能隐身。
/// 更糟的是续行里带冒号的那一行还会被当成另一条键记进账
fn parse(text: &str) -> (BTreeMap<String, String>, String) {
    let mut meta = BTreeMap::new();
    let trimmed = text.trim_start_matches('\u{feff}');

    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let lines: Vec<&str> = rest[..end].lines().collect();
            let mut index = 0;
            while index < lines.len() {
                let line = lines[index];
                index += 1;
                let Some((key, value)) = line.split_once(':') else {
                    continue;
                };
                let key = key.trim().to_string();
                if key.is_empty() {
                    continue;
                }
                let value = value.trim();
                match block_style_of(value) {
                    Some(style) => {
                        let (body, taken) = take_block(&lines[index..]);
                        index += taken;
                        meta.insert(key, join_block(&body, style));
                    }
                    None => {
                        meta.insert(
                            key,
                            value.trim_matches('"').trim_matches('\'').to_string(),
                        );
                    }
                }
            }
            return (meta, rest[end + 4..].trim_start().to_string());
        }
    }

    (meta, trimmed.to_string())
}

fn first_paragraph(body: &str) -> String {
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cut: String = line.chars().take(100).collect();
        return if line.chars().count() > 100 {
            format!("{cut}…")
        } else {
            cut
        };
    }
    "（没有描述）".into()
}

/// 列表里给人看的那一眼正文。换行压成空格，免得一行卡片被拉成一屏
fn preview_of(body: &str) -> String {
    let joined: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();

    joined.join(" ").chars().take(160).collect()
}

fn allowed_tools_of(meta: &BTreeMap<String, String>) -> Vec<String> {
    let raw = match (meta.get("allowed-tools"), meta.get("allowed_tools")) {
        (Some(value), _) | (None, Some(value)) => value.clone(),
        (None, None) => return Vec::new(),
    };

    raw.split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// 路径上任一组件是符号链接即为真。技能是从市场/仓库来的第三方内容，
/// 一个指向 ~/.ssh 的链接就能把"读技能"变成"读密钥"——白名单挡不住链接，
/// 唯一稳妥的办法是不为含链接的路径解析任何东西
pub(crate) fn path_contains_symlink(path: &Path) -> bool {
    let mut walked = PathBuf::new();
    for component in path.components() {
        walked.push(component);
        match fs::symlink_metadata(&walked) {
            Ok(meta) if meta.file_type().is_symlink() => return true,
            // 读不到的尾部（还不存在的部分）无从判起，到此为止
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

/// 一个 `<dir>/SKILL.md`。目录名是兜底的 name，frontmatter 的 name 优先。
/// 技能目录来自市场/仓库/四生态约定，全是第三方来源：SKILL.md 或它的任何
/// 上级是符号链接，这个技能整个不收——链接是路径逃逸的跳板
fn read_skill_dir(dir: &Path, source: &str) -> Option<Doc> {
    if path_contains_symlink(&dir.join("SKILL.md")) {
        return None;
    }
    let text = fs::read_to_string(dir.join("SKILL.md")).ok()?;
    let folder = dir.file_name()?.to_string_lossy().trim().to_string();
    if folder.is_empty() {
        return None;
    }

    let (meta, body) = parse(&text);
    let name = meta
        .get("name")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or(folder.clone());
    let description = meta
        .get("description")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| first_paragraph(&body));

    Some(Doc {
        key: format!("{source}/{folder}"),
        path: dir.join("SKILL.md"),
        name,
        description,
        allowed_tools: allowed_tools_of(&meta),
        body,
    })
}

/// 出厂扩展的技能并进同一本账（design-builtin-extensions.md §4）：被关的扩展
/// 整批不产出——清单、load_skill 取用、插件页都吃这份结果，扩展开关零特判。
/// 内置技能没有磁盘文件，path 留空；与磁盘技能唯一的结构差异就在这一格
pub(crate) fn merge_builtins(docs: &mut Vec<Doc>, disabled_builtins: &[String]) {
    for extension in crate::builtins::extensions() {
        if disabled_builtins.iter().any(|id| id == extension.id) {
            continue;
        }
        for skill in &extension.skills {
            docs.push(Doc {
                key: extension.skill_key(skill.folder),
                path: PathBuf::new(),
                name: skill.name.to_string(),
                description: skill.description.to_string(),
                allowed_tools: skill.allowed_tools.iter().map(|tool| tool.to_string()).collect(),
                body: skill.body.to_string(),
            });
        }
    }
}

pub(crate) fn scan(app: &AppHandle) -> Result<Vec<Doc>, String> {
    scan_in(
        &app.path().app_data_dir().map_err(|e| e.to_string())?,
        &config::load(app).disabled_builtins,
    )
}

/// worker 进程的变体（M3 第 2 档）：目录与关闭名单由调用方传入
pub(crate) fn scan_in(data_dir: &std::path::Path, disabled_builtins: &[String]) -> Result<Vec<Doc>, String> {
    let mut docs = Vec::new();

    for source in sources_in(data_dir)? {
        let Ok(entries) = fs::read_dir(&source.dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Some(doc) = read_skill_dir(&path, &source.label) {
                docs.push(doc);
            }
        }
    }

    merge_builtins(&mut docs, disabled_builtins);

    docs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(docs)
}

fn is_disabled(id: &str, disabled: &[String]) -> bool {
    disabled.iter().any(|item| item == id)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillsListing {
    /// 个人技能目录，界面上要显示它，用户往这里放
    pub dir: String,
    pub skills: Vec<Skill>,
}

#[tauri::command]
pub fn skills_list(app: AppHandle) -> Result<SkillsListing, String> {
    let disabled = config::load(&app).disabled_skills;
    Ok(SkillsListing {
        dir: skills_root(&app)?.display().to_string(),
        skills: scan(&app)?
            .into_iter()
            .map(|doc| {
                let preview = preview_of(&doc.body);
                Skill {
                    chars: doc.body.chars().count(),
                    preview,
                    id: doc.key.clone(),
                    name: doc.name,
                    description: doc.description,
                    allowed_tools: doc.allowed_tools,
                    path: doc.path.display().to_string(),
                    source: doc.key.split('/').next().unwrap_or("个人").to_string(),
                    enabled: !is_disabled(&doc.key, &disabled),
                }
            })
            .collect(),
    })
}

// ---- SkillHub 市场（skillhub.cn）----
//
// 设置页「市场」的数据源：skillhub 的公开列表接口。**只做浏览与跳转**——
// SKILL.md 本体不在公开接口里（详情内容要登录态，路径探测全是 405），所以
// 不假装能一键拉正文：安装走 skillhub 详情页（网页端在线安装/下载 zip）或
// 上游仓库，下载好的技能目录放回个人技能目录、点「重新扫描」即可用。

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillhubEntryView {
    pub name: String,
    pub slug: String,
    /// `@作者/技能` 的规范名，详情页 URL 由它拼出
    pub handle: String,
    pub page_url: String,
    pub description: String,
    pub version: String,
    pub category: String,
    pub downloads: u64,
    pub stars: u64,
    pub verified: bool,
    /// 同步自 GitHub 的条目才有的上游仓库
    pub upstream_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillhubPageView {
    pub entries: Vec<SkillhubEntryView>,
    /// 该过滤条件下的总数（分页展示用）
    pub total: u64,
    pub page: usize,
}

pub const SKILLHUB_API: &str = "https://api.skillhub.cn/api/skills";
pub const SKILLHUB_SITE: &str = "https://skillhub.cn/skills";

/// skillhub 的应答 → 界面视图。解析与网络分离：形状测试拿真样本喂这里。
/// 应答形状 `{ code: 0, data: { skills: [...], total } }`，条目里 namespace
/// 带规范名（`@作者/技能`），详情页 URL 由它拼出
fn parse_skillhub_page(value: &Value, page: usize) -> Result<SkillhubPageView, String> {
    let code = value["code"].as_i64().unwrap_or(-1);
    if code != 0 {
        return Err(format!("SkillHub 市场回了错误码 {code}。"));
    }
    let data = &value["data"];
    let entries = data["skills"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| {
            let slug = entry["slug"].as_str().filter(|slug| !slug.is_empty())?;
            let handle = entry["namespace"]["canonicalName"]
                .as_str()
                .filter(|name| !name.is_empty())?
                .to_string();
            Some(SkillhubEntryView {
                name: entry["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .unwrap_or(slug)
                    .to_string(),
                slug: slug.to_string(),
                // canonicalName 是 `@作者/slug`，本身就是详情页路径的后半段
                page_url: format!("{SKILLHUB_SITE}/{handle}"),
                handle: handle.clone(),
                description: entry["description_zh"]
                    .as_str()
                    .or_else(|| entry["description"].as_str())
                    .unwrap_or_default()
                    .to_string(),
                version: entry["version"].as_str().unwrap_or_default().to_string(),
                category: entry["category"].as_str().unwrap_or_default().to_string(),
                downloads: entry["downloads"].as_u64().unwrap_or(0),
                stars: entry["stars"].as_u64().unwrap_or(0),
                verified: entry["verified"].as_bool().unwrap_or(false),
                upstream_url: entry["upstream_url"]
                    .as_str()
                    .filter(|url| !url.is_empty())
                    .map(str::to_string),
            })
        })
        .collect();
    Ok(SkillhubPageView {
        entries,
        total: data["total"].as_u64().unwrap_or(0),
        page,
    })
}

fn fetch_skillhub(keyword: Option<&str>, sort_by: &str, page: usize) -> Result<SkillhubPageView, String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(SKILLHUB_TIMEOUT))
        .build()
        .new_agent();
    let mut request = agent
        .get(SKILLHUB_API)
        .query("sortBy", sort_by)
        .query("page", &page.to_string())
        .query("pageSize", "20");
    if let Some(word) = keyword.map(str::trim).filter(|word| !word.is_empty()) {
        request = request.query("keyword", word);
    }
    let mut response = request.call().map_err(|e| format!("请求 SkillHub 市场失败：{e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.body_mut().read_to_string().unwrap_or_default();
        return Err(format!(
            "SkillHub 市场回了 {status}：{}",
            body.chars().take(200).collect::<String>()
        ));
    }
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("读 SkillHub 市场响应失败：{e}"))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("SkillHub 市场响应不是合法 JSON：{e}"))?;
    parse_skillhub_page(&value, page)
}

/// 设置页「技能商店」的搜索入口。公开接口、无凭据；阻塞的网络调用放阻塞池。
/// `sort_by` 只认 score / downloads / trending / updated，认不出的落回评分排序
#[tauri::command]
pub async fn skillhub_search(
    keyword: Option<String>,
    sort_by: Option<String>,
    page: Option<usize>,
) -> Result<SkillhubPageView, String> {
    let requested = sort_by.as_deref().unwrap_or("score");
    let sort_by = match requested {
        "downloads" | "trending" | "updated" => requested,
        _ => "score",
    }
    .to_string();
    let page = page.unwrap_or(1).max(1);
    tauri::async_runtime::spawn_blocking(move || {
        fetch_skillhub(keyword.as_deref(), &sort_by, page)
    })
    .await
    .map_err(|e| format!("市场查询任务失败：{e}"))?
}

// ---- SkillHub 一键安装 ----
//
// 安装包是 zip（SKILL.md 在根，scripts/references 等子目录随行），下载服务商 302 到
// 腾讯 COS。安装 = 下载 → 解压进个人技能目录下的一个新子目录 → 重新扫描即可见。
// 防线三道：zip-slip（entry 名清洗）、解压总量上限（防 zip 炸弹）、同名目录不覆盖。

/// 解压后的落盘读数。前端拿它拼安装成功的提示
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillhubInstallReport {
    pub dir: String,
    pub files: u32,
    pub bytes: u64,
}

/// 下载体与解压总量的上限。当前最大的一批发布物在 MB 量级，50MB 已经宽裕；
/// zip 炸弹的特征是解压后比下载体大出几个数量级，总量一卡就炸不动
const SKILLHUB_MAX_DOWNLOAD: u64 = 50 * 1024 * 1024;
const SKILLHUB_MAX_UNPACKED: u64 = 64 * 1024 * 1024;
const SKILLHUB_MAX_FILES: usize = 500;

/// 把 zip 字节解压进 `dest`。SKILL.md 必须在包里（在根或唯一顶层目录都行——
/// 单层顶层目录会被剥掉，技能目录名由安装目标决定，不由 zip 内部结构决定）。
/// 恶意条目（路径穿越、绝对路径）直接拒绝整个包：少装几个文件不如不装
fn install_zip_bytes(bytes: &[u8], dest: &Path) -> Result<SkillhubInstallReport, String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("安装包不是合法的 zip：{e}"))?;
    if archive.len() > SKILLHUB_MAX_FILES {
        return Err(format!(
            "安装包里有 {} 个条目，超过上限 {SKILLHUB_MAX_FILES}，拒绝安装。",
            archive.len()
        ));
    }

    let names: Vec<String> = archive.file_names().map(str::to_string).collect();
    // 官方发布物是平铺的（SKILL.md 在 zip 根）；带单层顶层目录的也兼容——
    // 根上没有 SKILL.md、且全部条目都在同一个目录层里、那一层里有 SKILL.md
    // 时才剥掉它。平铺包里带 scripts/ 之类子目录不会触发剥离
    let has_root_skill = names.iter().any(|name| name == "SKILL.md");
    let mut roots: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for name in &names {
        if let Some((first, _)) = name.split_once('/') {
            roots.insert(first);
        }
    }
    let strip: Option<String> = if !has_root_skill && roots.len() == 1 {
        let root_dir = roots.iter().next().unwrap();
        let wanted = format!("{root_dir}/SKILL.md");
        names
            .iter()
            .any(|name| name == &wanted)
            .then(|| format!("{root_dir}/"))
    } else {
        None
    };
    if !has_root_skill && strip.is_none() {
        return Err("安装包里没有 SKILL.md——这不是一个技能包。".into());
    }

    fs::create_dir_all(dest).map_err(|e| format!("创建技能目录失败：{e}"))?;
    let mut files: u32 = 0;
    let mut bytes_out: u64 = 0;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("读取安装包条目失败：{e}"))?;
        // enclosed_name 清洗：`..`、绝对路径、盘符全是 None——zip-slip 拒收整包
        let Some(relative) = entry.enclosed_name() else {
            return Err(format!(
                "安装包里有越界路径「{}」，拒绝安装。",
                entry.name()
            ));
        };
        // zip 里的符号链接条目（unix mode 高位 0o12）直接拒：解出来的链接
        // 指向哪是打包者说了算，指向 ~/.ssh 的"技能"就是越界读的跳板
        if entry.unix_mode().is_some_and(|mode| (mode >> 12) & 0o17 == 0o12) {
            return Err(format!(
                "安装包里有符号链接条目「{}」，拒绝安装。",
                entry.name()
            ));
        }
        let relative = match &strip {
            Some(prefix) => match relative.strip_prefix(prefix) {
                Ok(rest) if !rest.as_os_str().is_empty() => rest.to_path_buf(),
                // 被剥掉的顶层目录条目本身
                _ => continue,
            },
            None => relative,
        };
        if entry.is_dir() {
            fs::create_dir_all(dest.join(&relative)).map_err(|e| format!("创建目录失败：{e}"))?;
            continue;
        }
        if let Some(parent) = dest.join(&relative).parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{e}"))?;
        }
        let mut out = fs::File::create(dest.join(&relative)).map_err(|e| format!("写文件失败：{e}"))?;
        let written = std::io::copy(&mut entry, &mut out).map_err(|e| format!("写文件失败：{e}"))?;
        bytes_out += written;
        if bytes_out > SKILLHUB_MAX_UNPACKED {
            return Err("解压总量超出上限，疑似 zip 炸弹，已中止。".into());
        }
        files += 1;
    }

    if !dest.join("SKILL.md").exists() {
        return Err("安装包里没有 SKILL.md——这不是一个技能包。".into());
    }
    Ok(SkillhubInstallReport {
        dir: dest.display().to_string(),
        files,
        bytes: bytes_out,
    })
}

fn fetch_and_install(slug: &str, personal_root: &Path) -> Result<SkillhubInstallReport, String> {
    // slug 只认 `@作者/技能` 形状：它进的是固定服务商的 query，形状不对的拒绝在门外
    let trimmed = slug.trim();
    if !trimmed.starts_with('@') || trimmed.split('/').count() != 2 {
        return Err("slug 要是「@作者/技能」的形状。".into());
    }
    // 安装目录名取 slug 的技能段（`@indiv-ebandao/dev-expert` → `dev-expert`）：
    // skillhub 的 slug 规范已约束字符，这里再挡一遍不合文件名的
    let dir_name = trimmed.split('/').next_back().unwrap_or_default().to_string();
    if dir_name.is_empty()
        || !dir_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(format!("slug 的技能段「{dir_name}」不适合做目录名。"));
    }
    let dest = personal_root.join(&dir_name);
    if dest.exists() {
        return Err(format!(
            "技能目录「{dir_name}」已经存在——同名不覆盖，先删掉或改名再装。"
        ));
    }

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(SKILLHUB_TIMEOUT))
        .build()
        .new_agent();
    let mut response = agent
        .get("https://api.skillhub.cn/api/v1/download")
        .query("slug", trimmed)
        .call()
        .map_err(|e| format!("下载安装包失败：{e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "下载安装包失败：市场回了 {}。",
            response.status()
        ));
    }
    // 限长交给 ureq 的 body 配置：超 50MB 直接报错，zip 炸弹在下载这一步就卡死
    let bytes = response
        .body_mut()
        .with_config()
        .limit(SKILLHUB_MAX_DOWNLOAD)
        .read_to_vec()
        .map_err(|e| format!("读安装包失败（可能超出 50MB 上限）：{e}"))?;

    install_zip_bytes(&bytes, &dest)
}

/// 一键安装：下载 zip → 解压进个人技能目录 → 前端重新扫描即可见。同名目录不覆盖
#[tauri::command]
pub async fn skillhub_install(app: AppHandle, slug: String) -> Result<SkillhubInstallReport, String> {
    // 路径解析在命令入口做（拿 AppHandle），阻塞线程只管下载与落盘
    let root = skills_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || fetch_and_install(&slug, &root))
        .await
        .map_err(|e| format!("安装任务失败：{e}"))?
}

// ---- 自定义 GitHub 仓库的技能识别与安装 ----
//
// 输入 github.com 的仓库地址（根，或 /tree/{分支}/{子路径}），识别树里所有
// 带 SKILL.md 的目录作为候选；安装走 raw.githubusercontent.com 逐文件拉取。
// GitHub API 对无凭据请求限速（每小时 60 次），识别只花 1~2 次，安装本身走
// raw 域不占配额。

/// 一个候选 = 仓库里一个带 SKILL.md 的目录。name/description 从它的 frontmatter 读，
/// 读不到就用目录名兜底
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubCandidateView {
    /// 仓库内相对目录；根候选是空串
    pub path: String,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubProbeView {
    pub owner: String,
    pub repo: String,
    pub branch: String,
    pub candidates: Vec<GithubCandidateView>,
}

/// 解析仓库地址。认三种形状：`{o}/{r}`、`{o}/{r}.git`、`{o}/{r}/tree/{分支}/{子路径}`。
/// 返回 (owner, repo, tree)，tree = (分支, 子路径)，子路径可能为空串
fn parse_github_url(raw: &str) -> Result<(String, String, Option<(String, String)>), String> {
    let text = raw.trim().trim_end_matches('/');
    let text = text.strip_suffix(".git").unwrap_or(text);
    let rest = text
        .strip_prefix("https://github.com/")
        .or_else(|| text.strip_prefix("http://github.com/"))
        .or_else(|| text.strip_prefix("github.com/"))
        .ok_or_else(|| "只认 github.com 的仓库地址。".to_string())?;

    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return Err("地址里要有 {所有者}/{仓库名} 两段。".into());
    }
    let owner = segments[0].to_string();
    let repo = segments[1].to_string();

    let tree = match segments.len() {
        2 => None,
        _ if segments[2] == "tree" => {
            if segments.len() < 4 {
                return Err("tree 地址里缺分支名。".into());
            }
            Some((segments[3].to_string(), segments[4..].join("/")))
        }
        _ => {
            return Err(format!(
                "地址多了「{}」——只认仓库地址或 /tree/分支/子路径。",
                segments[2]
            ))
        }
    };
    Ok((owner, repo, tree))
}

/// 市场安装的位置校验与这里共用一条：能做技能目录名的才放行
fn skill_dir_name_problem(name: &str) -> Option<String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        Some(format!("「{name}」不适合做技能目录名（只许字母、数字、连字符、下划线、点）。"))
    } else {
        None
    }
}

fn github_api_json(url: &str) -> Result<Value, String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(SKILLHUB_TIMEOUT))
        .build()
        .new_agent();
    let mut response = agent
        .get(url)
        .header("Accept", "application/vnd.github+json")
        // GitHub API 强制要求 UA；没有它一律 403
        .header("User-Agent", "aglab")
        .call()
        .map_err(|e| format!("请求 GitHub 失败：{e}"))?;
    let status = response.status();
    let text = response.body_mut().read_to_string().map_err(|e| e.to_string())?;
    if status.as_u16() == 403 {
        return Err(
            "GitHub 拒绝了请求（多半是匿名限速：每小时 60 次）。稍后再试。".into(),
        );
    }
    if !status.is_success() {
        return Err(format!("GitHub 回了 {status}：{}", text.chars().take(200).collect::<String>()));
    }
    serde_json::from_str(&text).map_err(|e| format!("GitHub 响应不是合法 JSON：{e}"))
}

fn fetch_raw_text(url: &str) -> Result<String, String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(SKILLHUB_TIMEOUT))
        .build()
        .new_agent();
    let mut response = agent.get(url).call().map_err(|e| format!("请求 raw 内容失败：{e}"))?;
    if !response.status().is_success() {
        return Err(format!("raw 内容请求回了 {}。", response.status()));
    }
    response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("读 raw 内容失败：{e}"))
}

/// 仓库树的 blob 清单 → 所有带 SKILL.md 的目录（根 = 空串）。树被 GitHub
/// 截断（超大仓库）时报错：截断的清单会漏技能，漏着漏着就装错
fn skill_dirs_from_tree(tree_json: &Value) -> Result<Vec<String>, String> {
    if tree_json["truncated"].as_bool().unwrap_or(false) {
        return Err("仓库文件树太大，GitHub 返回了截断清单，无法可靠识别。".into());
    }
    let mut dirs: Vec<String> = Vec::new();
    for entry in tree_json["tree"].as_array().cloned().unwrap_or_default() {
        let path = entry["path"].as_str().unwrap_or_default();
        if entry["type"].as_str() == Some("blob") && path.ends_with("/SKILL.md") {
            dirs.push(path.trim_end_matches("SKILL.md").trim_end_matches('/').to_string());
        } else if entry["type"].as_str() == Some("blob") && path == "SKILL.md" {
            dirs.push(String::new());
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn probe_github(url: &str) -> Result<GithubProbeView, String> {
    let (owner, repo, tree) = parse_github_url(url)?;

    let branch = match &tree {
        Some((branch, _)) => branch.clone(),
        None => {
            let meta = github_api_json(&format!("https://api.github.com/repos/{owner}/{repo}"))?;
            meta["default_branch"]
                .as_str()
                .ok_or_else(|| "拿不到这个仓库的默认分支。".to_string())?
                .to_string()
        }
    };

    let tree_json = github_api_json(&format!(
        "https://api.github.com/repos/{owner}/{repo}/git/trees/{branch}?recursive=1"
    ))?;
    let dirs = skill_dirs_from_tree(&tree_json)?;
    if dirs.is_empty() {
        return Err("这棵仓库树里没找到任何 SKILL.md。".into());
    }

    // 候选的名字与描述从各自的 SKILL.md frontmatter 读。候选多于 20 个时只读
    // 前 20 个：剩下的用路径名兜底，别为一次识别把匿名配额烧光
    let raw_base = format!("https://raw.githubusercontent.com/{owner}/{repo}/{branch}");
    let candidates = dirs
        .iter()
        .enumerate()
        .map(|(index, dir)| {
            let fallback = dir.split('/').next_back().unwrap_or(&repo).to_string();
            if index >= 20 {
                return GithubCandidateView {
                    path: dir.clone(),
                    name: fallback,
                    description: String::new(),
                };
            }
            let md_url = if dir.is_empty() {
                format!("{raw_base}/SKILL.md")
            } else {
                format!("{raw_base}/{dir}/SKILL.md")
            };
            let (name, description) = match fetch_raw_text(&md_url) {
                Ok(text) => {
                    let (meta, _) = parse(&text);
                    let name = meta.get("name").cloned().unwrap_or(fallback);
                    let description = meta.get("description").cloned().unwrap_or_default();
                    (name, description)
                }
                Err(_) => (fallback, String::new()),
            };
            GithubCandidateView {
                path: dir.clone(),
                name,
                description,
            }
        })
        .collect();

    Ok(GithubProbeView {
        owner,
        repo,
        branch,
        candidates,
    })
}

/// 安装一个候选：按目录前缀从树里挑出全部 blob，逐个 raw 下载落盘。
/// 总量预检（树里的 size 字段）+ 同名目录不覆盖，与市场安装同一套守则
#[allow(clippy::too_many_arguments)]
fn install_github_skill(
    owner: &str,
    repo: &str,
    branch: &str,
    dir: &str,
    install_name: &str,
    personal_root: &Path,
) -> Result<SkillhubInstallReport, String> {
    if let Some(problem) = skill_dir_name_problem(install_name) {
        return Err(problem);
    }
    let dest = personal_root.join(install_name);
    if dest.exists() {
        return Err(format!(
            "技能目录「{install_name}」已经存在——同名不覆盖，先删掉或改名再装。"
        ));
    }

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(SKILLHUB_TIMEOUT))
        .build()
        .new_agent();
    let tree_json = github_api_json(&format!(
        "https://api.github.com/repos/{owner}/{repo}/git/trees/{branch}?recursive=1"
    ))?;

    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    // 前缀过滤出的 blobs：路径与大小都在 GitHub 的树清单里，总量先算后下
    let mut chosen: Vec<(String, u64)> = Vec::new();
    let mut total: u64 = 0;
    for entry in tree_json["tree"].as_array().cloned().unwrap_or_default() {
        let path = entry["path"].as_str().unwrap_or_default();
        if entry["type"].as_str() != Some("blob") || !path.starts_with(&prefix) {
            continue;
        }
        let size = entry["size"].as_u64().unwrap_or(0);
        total += size;
        if total > SKILLHUB_MAX_UNPACKED {
            return Err("这个技能的文件总量超出上限，拒绝安装。".into());
        }
        chosen.push((path.to_string(), size));
    }
    if !chosen.iter().any(|(path, _)| path == format!("{prefix}SKILL.md").as_str()) {
        return Err("选中的目录里没有 SKILL.md。".into());
    }

    fs::create_dir_all(&dest).map_err(|e| format!("创建技能目录失败：{e}"))?;
    let mut files: u32 = 0;
    for (path, _) in &chosen {
        let relative = path.strip_prefix(&prefix).unwrap_or(path);
        let target = dest.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{e}"))?;
        }
        let raw_url = format!(
            "https://raw.githubusercontent.com/{owner}/{repo}/{branch}/{relative}"
        );
        let mut response = agent
            .get(&raw_url)
            .call()
            .map_err(|e| format!("下载 {relative} 失败：{e}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "下载 {relative} 失败：raw 回了 {}。",
                response.status()
            ));
        }
        let content = response
            .body_mut()
            .read_to_vec()
            .map_err(|e| format!("读 {relative} 失败：{e}"))?;
        fs::write(&target, content).map_err(|e| format!("写文件失败：{e}"))?;
        files += 1;
    }

    Ok(SkillhubInstallReport {
        dir: dest.display().to_string(),
        files,
        bytes: total,
    })
}

#[tauri::command]
pub async fn github_skill_probe(url: String) -> Result<GithubProbeView, String> {
    tauri::async_runtime::spawn_blocking(move || probe_github(&url))
        .await
        .map_err(|e| format!("识别任务失败：{e}"))?
}

#[tauri::command]
pub async fn github_skill_install(
    app: AppHandle,
    url: String,
    path: String,
    branch: String,
    name: String,
) -> Result<SkillhubInstallReport, String> {
    let root = skills_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let (owner, repo, tree) = parse_github_url(&url)?;
        // 分支优先用识别时定下的那份；地址里没带 tree 时也拿得到默认分支
        let branch = if branch.is_empty() {
            tree.map(|(branch, _)| branch).ok_or_else(|| "缺分支名。".to_string())?
        } else {
            branch
        };
        install_github_skill(&owner, &repo, &branch, &path, &name, &root)
    })
    .await
    .map_err(|e| format!("安装任务失败：{e}"))?
}

/// 清单式提示词：只给"有什么、什么时候用"，正文留给模型自己去取。
pub fn prompt(app: &AppHandle) -> Result<Option<String>, String> {
    prompt_in(
        &app.path().app_data_dir().map_err(|e| e.to_string())?,
        &app.path().app_config_dir().map_err(|e| e.to_string())?,
    )
}

/// worker 进程的变体（M3 第 2 档）
pub fn prompt_in(data_dir: &std::path::Path, config_dir: &std::path::Path) -> Result<Option<String>, String> {
    let disabled = config::load_from_dir(config_dir).disabled_skills;
    prompt_for(&scan_in(data_dir, &config::load_from_dir(config_dir).disabled_builtins)?, &disabled)
}

pub(crate) fn prompt_for(docs: &[Doc], disabled: &[String]) -> Result<Option<String>, String> {
    let enabled: Vec<&Doc> = docs
        .iter()
        .filter(|doc| !is_disabled(&doc.key, disabled))
        .collect();
    if enabled.is_empty() {
        return Ok(None);
    }

    // XML 结构是刻意的（pi 的 formatSkillsForSystemPrompt 同款）：
    // 描述里的换行、尖括号不会污染清单边界，模型解析 <skill> 块比解析纯文本行可靠得多
    let mut text = String::from(
        "以下技能是用户积累的操作清单，提供特定任务的专业指引。请求内容匹配某个技能的描述时，\
         先调用 load_skill 取回正文再照着做，不必询问用户。\
         技能正文里的相对路径相对该技能目录（SKILL.md 所在目录）解析，请用绝对路径传给文件类工具。\n\n<available_skills>",
    );

    for doc in enabled.iter().take(LISTING_CAP) {
        // 内置技能没有磁盘文件：正文在二进制里，位置一栏说清即可
        let location = if doc.path.as_os_str().is_empty() {
            "（内置，load_skill 直接取正文）".to_string()
        } else {
            doc.path.display().to_string()
        };
        text.push_str(&format!(
            "\n  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>",
            escape_xml(&doc.name),
            escape_xml(&doc.description),
            escape_xml(&location),
        ));
        if !doc.allowed_tools.is_empty() {
            text.push_str(&format!(
                "\n    <allowed-tools>{}</allowed-tools>",
                escape_xml(&doc.allowed_tools.join(", "))
            ));
        }
        text.push_str("\n  </skill>");
    }
    text.push_str("\n</available_skills>");

    if enabled.len() > LISTING_CAP {
        text.push_str(&format!(
            "\n（另有 {} 个技能未列出）",
            enabled.len() - LISTING_CAP
        ));
    }

    Ok(Some(text))
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// load_skill 工具的执行体：按名字（或 "来源/目录名"）取回正文。
/// 某个技能声明的工具白名单。闸门要用它（`tool_runtime::note_skill`），
/// 以前它只被印进提示词，等于一句愿望
pub fn declared_tools(app: &AppHandle, requested: &str) -> Vec<String> {
    declared_tools_in(
        &app.path().app_data_dir().map_err(|e| e.to_string()).unwrap_or_default(),
        &app.path().app_config_dir().map_err(|e| e.to_string()).unwrap_or_default(),
        requested,
    )
}

/// worker 进程的变体（M3 第 2 档）
pub fn declared_tools_in(data_dir: &std::path::Path, config_dir: &std::path::Path, requested: &str) -> Vec<String> {
    let wanted = requested.trim();
    if wanted.is_empty() {
        return Vec::new();
    }
    scan_in(
        data_dir,
        &config::load_from_dir(config_dir).disabled_builtins,
    )
        .unwrap_or_default()
        .into_iter()
        .find(|doc| doc.name == wanted || doc.key == wanted)
        .map(|doc| doc.allowed_tools)
        .unwrap_or_default()
}

pub fn load_body(app: &AppHandle, requested: &str) -> Result<String, String> {
    let disabled = config::load(app).disabled_skills;
    let wanted = requested.trim();
    if wanted.is_empty() {
        return Err("要读取的技能名为空。".into());
    }

    let doc = scan(app)?
        .into_iter()
        .find(|doc| doc.name == wanted || doc.key == wanted)
        .ok_or_else(|| format!("没有叫「{wanted}」的技能。"))?;

    if is_disabled(&doc.key, &disabled) {
        return Err("这个技能已被用户关闭。".into());
    }

    let mut text = format!("# 技能：{}\n\n", doc.name);
    if !doc.allowed_tools.is_empty() {
        text.push_str(&format!(
            "本技能声明只使用这些工具：{}。\n\n",
            doc.allowed_tools.join(", ")
        ));
    }
    text.push_str(&doc.body);
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, folder: &str, text: &str) {
        let dir = root.join(folder);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), text).unwrap();
    }

    #[test]
    fn the_skill_view_carries_every_field_the_frontend_reads() {
        let skill = Skill {
            id: "个人/a".into(),
            name: "a".into(),
            description: String::new(),
            allowed_tools: Vec::new(),
            chars: 1,
            preview: String::new(),
            path: String::new(),
            source: "个人".into(),
            enabled: true,
        };
        crate::test_support::assert_matches_ts(&serde_json::to_value(skill).unwrap(), "Skill");
    }

    #[test]
    fn reads_frontmatter_and_falls_back_to_the_folder_name() {
        let root = crate::test_support::temp_dir("skills");
        write_skill(
            &root,
            "daily-report",
            "---\nname: daily-report\ndescription: \"写日报时使用\"\nallowed-tools: Read, Grep\n---\n# 日报\n\n先给结论。\n",
        );
        write_skill(&root, "plain", "# 只有标题\n这是描述段落的开头。\n");

        let docs: Vec<Doc> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|entry| read_skill_dir(&entry.path(), "个人"))
            .collect::<Option<Vec<_>>>()
            .unwrap();

        let report = docs.iter().find(|doc| doc.name == "daily-report").unwrap();
        assert_eq!(report.description, "写日报时使用");
        assert_eq!(report.allowed_tools, vec!["Read", "Grep"]);
        assert!(report.body.starts_with("# 日报"));
        assert!(!report.body.contains("---"), "frontmatter 不该进正文");

        let plain = docs.iter().find(|doc| doc.name == "plain").unwrap();
        assert_eq!(plain.description, "这是描述段落的开头。");

        crate::test_support::remove_tree(&root);
    }

    /// 折叠块标量（`description: >`）：续行属于这个值。这是 Agent Skills 里最常见的写法，
    /// 也是此前读坏的那一种——描述变成 ">" 就等于技能隐身，而带冒号的续行还会漏成假键
    #[test]
    fn a_folded_block_belongs_to_its_key() {
        let (meta, body) = parse(
            "---\nname: ponytail-audit\ndescription: >\n  Whole-repo audit for over-engineering. Like ponytail-review, but scans the\n  entire codebase instead of a diff: a ranked list of what to delete, simplify,\n  or replace with stdlib/native equivalents.\nallowed-tools: Read, Grep\n---\n正文\n",
        );

        assert_eq!(
            meta.get("description").map(String::as_str),
            Some(
                "Whole-repo audit for over-engineering. Like ponytail-review, but scans the \
                 entire codebase instead of a diff: a ranked list of what to delete, simplify, \
                 or replace with stdlib/native equivalents."
            )
        );
        assert_eq!(meta.get("name").map(String::as_str), Some("ponytail-audit"));
        assert_eq!(meta.get("allowed-tools").map(String::as_str), Some("Read, Grep"));
        assert!(
            !meta.keys().any(|key| key.contains("diff")),
            "带冒号的续行不该被当成另一条键：{:?}",
            meta.keys().collect::<Vec<_>>()
        );
        assert_eq!(body, "正文\n");
    }

    /// chomping 后缀（`>-`）认得，且与 `>` 同结果：差别只在末尾空白，两头都要 trim
    #[test]
    fn a_chomping_marker_does_not_change_the_value() {
        let (meta, _) = parse("---\ndescription: >-\n  甲\n  乙\n---\n正文\n");
        assert_eq!(meta.get("description").map(String::as_str), Some("甲 乙"));
    }

    /// 字面块（`|`）保留换行。清单是 XML 结构，换行不会污染边界；界面那一格由 HTML 折叠
    #[test]
    fn a_literal_block_keeps_its_line_breaks() {
        let (meta, _) = parse("---\ndescription: |\n  第一行\n  第二行\n---\n正文\n");
        assert_eq!(
            meta.get("description").map(String::as_str),
            Some("第一行\n第二行")
        );
    }

    /// 折叠块里更深缩进的行（触发词列表）不摊成一句——摊了模型就数不清有几个触发词
    #[test]
    fn deeper_indented_lines_stay_split_inside_a_fold() {
        let (meta, _) = parse(
            "---\ndescription: >\n  Use when the user says:\n    - audit this codebase\n    - find bloat\n  One-shot report.\n---\n正文\n",
        );
        assert_eq!(
            meta.get("description").map(String::as_str),
            Some(
                "Use when the user says:\n  - audit this codebase\n  - find bloat\n\
                 One-shot report."
            )
        );
    }

    /// 值本身长得像块标量（`|` 开头的普通文字）不能被吞：那不是 YAML 的块写法
    #[test]
    fn a_value_that_only_looks_like_a_block_stays_plain() {
        let (meta, _) = parse("---\ndescription: |pipe 分隔的说明\n---\n正文\n");
        assert_eq!(
            meta.get("description").map(String::as_str),
            Some("|pipe 分隔的说明")
        );
    }

    /// 市场解析：v0 的真样本形状。详情页 URL 由规范名拼出，中文描述优先，
    /// 错误码非 0 要报错而不是给一张空页
    #[test]
    fn skillhub_page_parsing_builds_page_urls_and_prefers_zh() {
        let page = parse_skillhub_page(
            &serde_json::json!({
                "code": 0,
                "data": {
                    "total": 176512,
                    "skills": [
                        {
                            "name": "编程专家.Skill",
                            "slug": "dev-expert",
                            "namespace": { "canonicalName": "@indiv-ebandao/dev-expert" },
                            "description": "English text",
                            "description_zh": "中文描述优先",
                            "version": "2.0.3",
                            "category": "dev-programming",
                            "downloads": 2161478,
                            "stars": 294,
                            "verified": true,
                            "upstream_url": null
                        },
                        {
                            "name": "无规范名",
                            "description": "缺 namespace 的条目要被丢掉"
                        }
                    ]
                }
            }),
            1,
        )
        .expect("code=0 的应答要解析成功");
        assert_eq!(page.total, 176512);
        assert_eq!(page.page, 1);
        assert_eq!(page.entries.len(), 1, "缺规范名的条目不进视图");
        let entry = &page.entries[0];
        assert_eq!(entry.description, "中文描述优先", "中文描述优先于英文");
        assert!(entry.verified);
        assert_eq!(entry.downloads, 2_161_478);
        assert_eq!(
            entry.page_url,
            "https://skillhub.cn/skills/@indiv-ebandao/dev-expert"
        );
        assert!(entry.upstream_url.is_none());

        let refused = parse_skillhub_page(&serde_json::json!({ "code": 401 }), 1);
        assert!(refused.is_err(), "错误码非 0 要报错");
    }

    /// 内存里造一个 zip，喂给安装解压。返回字节
    fn zip_bytes(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut cursor);
        for (name, body) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut writer, body.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
        cursor.into_inner()
    }

    /// 安装解压的四个判据：平铺包全量落盘、越界路径拒收整包、缺 SKILL.md 拒收、
    /// 单层顶层目录的包剥掉目录层
    #[test]
    fn skillhub_install_extracts_and_refuses_slip() {
        let bytes = zip_bytes(&[
            ("SKILL.md", "---\ndescription: x\n---\n# 标题"),
            ("scripts/run.sh", "echo hi"),
            ("_meta.json", "{}"),
        ]);
        let root = crate::test_support::temp_dir("skillhub-install");
        let dest = root.join("flat");
        let report = install_zip_bytes(&bytes, &dest).unwrap();
        assert_eq!(report.files, 3);
        assert!(dest.join("SKILL.md").exists());
        assert!(dest.join("scripts/run.sh").exists(), "子目录随包落盘");

        // zip-slip：`../` 越界的条目让整包被拒，且不许有任何文件落在外面
        let evil = zip_bytes(&[("SKILL.md", "# ok"), ("../evil.txt", "boom")]);
        assert!(install_zip_bytes(&evil, &root.join("evil")).is_err());
        assert!(!root.join("evil.txt").exists(), "越界文件不许落盘");

        // 缺 SKILL.md 的包不是技能包
        let plain = zip_bytes(&[("README.md", "hi")]);
        assert!(install_zip_bytes(&plain, &root.join("no-skill")).is_err());

        // 单层顶层目录的包：剥掉目录层，SKILL.md 落在安装目录根
        let wrapped = zip_bytes(&[("pkg/SKILL.md", "# 包在目录里"), ("pkg/a.txt", "a")]);
        let wrapped_dir = root.join("wrapped");
        install_zip_bytes(&wrapped, &wrapped_dir).unwrap();
        assert!(wrapped_dir.join("SKILL.md").exists());
        assert!(wrapped_dir.join("a.txt").exists(), "剥掉的只是目录层");
    }

    #[test]
    fn the_prompt_lists_names_without_bodies() {
        let root = crate::test_support::temp_dir("skills-listing");
        write_skill(
            &root,
            "a",
            "---\ndescription: 甲的用\n---\n正文甲不该出现在清单里\n",
        );
        write_skill(&root, "b", "---\ndescription: 乙的用\n---\n正文乙\n");

        let docs: Vec<Doc> = ["a", "b"]
            .iter()
            .filter_map(|folder| read_skill_dir(&root.join(folder), "个人"))
            .collect();

        let prompt = prompt_for(&docs, &["个人/b".to_string()]).unwrap().unwrap();
        assert!(prompt.contains("<name>a</name>"), "技能名在 skill 块里");
        assert!(prompt.contains("<description>甲的用</description>"));
        assert!(prompt.contains("<location>"), "要给出正文位置");
        assert!(prompt.contains("load_skill"), "要告诉模型怎么取正文");
        assert!(!prompt.contains("正文甲"), "正文不该被全量注入");
        assert!(!prompt.contains("乙的用"), "关闭的技能不列");

        crate::test_support::remove_tree(&root);
    }

    #[test]
    fn listing_is_capped_and_says_so() {
        let docs: Vec<Doc> = (0..(LISTING_CAP + 3))
            .map(|index| Doc {
                key: format!("个人/s{index}"),
                path: PathBuf::from("/tmp/does-not-matter"),
                name: format!("s{index}"),
                description: "描述".into(),
                allowed_tools: Vec::new(),
                body: "正文".into(),
            })
            .collect();

        let prompt = prompt_for(&docs, &[]).unwrap().unwrap();
        assert!(prompt.contains("另有 3 个技能未列出"));
    }

    /// 出厂技能并账：键是「内置/扩展/目录」三段式，没有磁盘路径，
    /// 清单里的位置一栏标「内置」——正文在二进制里，不给假路径
    #[test]
    fn builtin_skills_merge_into_the_same_listing() {
        let mut docs = Vec::new();
        merge_builtins(&mut docs, &[]);

        let guide = docs
            .iter()
            .find(|doc| doc.key == "内置/guide/guide")
            .expect("出厂使用指南应该在账上");
        assert_eq!(guide.name, "aglab 使用指南");
        assert!(guide.body.starts_with("# aglab"), "正文从 include_str! 原样进来");
        assert!(
            guide.path.as_os_str().is_empty(),
            "内置技能没有磁盘文件，不给假路径"
        );

        let prompt = prompt_for(&docs, &[]).unwrap().unwrap();
        assert!(prompt.contains("<name>故障诊断</name>"));
        assert!(
            prompt.contains("（内置，load_skill 直接取正文）"),
            "位置一栏要能看出这不是一个要打开的文件：{prompt}"
        );
    }

    /// 关掉扩展 = 它的技能整批从账上消失，别的扩展不受牵连；
    /// 单条技能的开关照旧走 disabled_skills（消费方职责，scan 不管）
    #[test]
    fn a_disabled_builtin_extension_vanishes_wholesale() {
        let mut docs = Vec::new();
        merge_builtins(
            &mut docs,
            &["guide".to_string(), "not-an-extension".to_string()],
        );

        assert!(
            !docs.iter().any(|doc| doc.key.starts_with("内置/guide/")),
            "关掉的扩展连故障诊断一起消失"
        );
        assert!(
            docs.iter().any(|doc| doc.key == "内置/skill-forge/skill-forge"),
            "别的扩展不受牵连"
        );
        // 不认识的 id 静默忽略：名册升级删掉某扩展后，旧配置里留下的 id 不该报错。
        // 在册 10 条、关掉 guide（带 2 条技能）后剩 8 条扩展各 1 条 = 9
        assert_eq!(docs.len(), 9, "名单扩了这里要跟着数");
    }
}
