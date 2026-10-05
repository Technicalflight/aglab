use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::origin::Origin;

/// 一条记录文件（MEMORY.md / daily/*.md）里可以放多条记忆，
/// 每条以自己的 `---\n` frontmatter 开头。真相源是这个文件，不是索引
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRecord {
    pub id: String,
    /// frontmatter 里的 `type`。Rust 里 `type` 是关键字，字段名用 kind
    pub kind: MemoryKind,
    pub scope: MemoryScope,
    pub project_id: Option<String>,
    pub status: MemoryStatus,
    /// 1..=5。影响是否长期保留
    pub importance: u32,
    /// 0.0..=1.0。低于阈值进 candidate 等用户确认
    pub confidence: f64,
    pub stability: Stability,
    pub source: MemorySource,
    /// 外发分级（见 [`MemorySensitivity` 的文档](MemorySensitivity)）。
    /// **非默认才写那一行**：多写一行会让每条既有记录的正文哈希变掉，
    /// 于是"升级一次"看起来像"整库记忆都被改过一次"
    #[serde(default, skip_serializing_if = "MemorySensitivity::is_unwritten")]
    pub sensitivity: MemorySensitivity,
    pub created_at: String,
    pub updated_at: String,
    /// 事情发生的时间，与"这条记录什么时候写下"是两回事。
    /// 时间线按它排；没人在正文里说过什么时候发生，它就是 None
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
    /// 最近一次真的被用上（注入给模型）的时间。编辑不动它，它也不是编辑：
    /// 新鲜度读它，所以"被用过"会让记忆重新变新鲜，而"被我改过字"不会
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reinforced_at: Option<String>,
    /// 从哪次对话、哪几条消息里长出来的。只存标识
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_days: Option<u32>,
    pub tags: Vec<String>,
    /// 这条记录讲到的东西：`名字|kind`，kind 可以不给（不给就由规则认）。
    /// 它不是第二个真相：正文说过的东西在这里只是一个可重建的投影输入，
    /// 删了 `memory_entities` 还能从正文与标签里再抽一遍
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<String>,
    /// 被这条取代的旧记忆 id。历史不删，只标
    pub supersedes: Vec<String>,
    pub content: String,
    /// 用户手加的、本系统不认识的字段。回写时原样带回去——
    /// 手编辑是受支持的用法，不能因为一次保存就把人家的字段吞了
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    Preference,
    Fact,
    Decision,
    Event,
    Rule,
    Profile,
    Relationship,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScope {
    Global,
    Project,
    Session,
    Temp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryStatus {
    Candidate,
    Active,
    Archived,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stability {
    Stable,
    Volatile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemorySource {
    User,
    Assistant,
    Inferred,
    Import,
}

/// 一条记忆的**外发分级**。判据只有一条问题：这段内容允许被送到推理服务商吗，以什么名义。
///
/// - `public`：默认。既进检索注入，也进后台自动外发的那两份材料（反思的"已经记着的"清单、
///   提取时的已知清单）。
/// - `private`：进注入——那是用户正在问的那件事的上下文，他主动发起了这一轮；
///   但**不进任何后台自动外发**，没人问的时候它不出门。
/// - `secret`：哪儿都不去。它仍然能被检索到、能在面板与时间线里看，但模型永远看不到这一条。
///
/// 默认值是 `public` 而不是 `secret`：既有记录一份都不该改（写盘时非默认才落那一行，
/// 否则升级一次就像"所有记忆都被改过一次"）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemorySensitivity {
    #[default]
    Public,
    Private,
    Secret,
}

impl MemorySensitivity {
    pub const ALL: [MemorySensitivity; 3] = [
        MemorySensitivity::Public,
        MemorySensitivity::Private,
        MemorySensitivity::Secret,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
            Self::Secret => "secret",
        }
    }

    /// 能不能进"发给模型当上下文"的那一段
    pub fn injectable(self) -> bool {
        self != MemorySensitivity::Secret
    }

    /// 能不能进后台自动外发的材料。它比 [`Self::injectable`] 严：那几份材料不是用户问出来的，
    /// 而"没人问也往外发"正是这条分级要拦住的事
    pub fn sendable_as_material(self) -> bool {
        self == MemorySensitivity::Public
    }

    /// 序列化时"这一行不必写"。它就是 `public` 的另一种说法，起这个名字是因为读的人
    /// 要判断的是"文件里为什么没有这一行"，而不是"这个函数在测什么"
    pub fn is_unwritten(level: &Self) -> bool {
        *level == MemorySensitivity::Public
    }

    /// 读索引里那一列时用的解析：**认不出的值按最严的那一档处理**。
    /// 一个手打成 `secrt` 的标记如果退化成 `public`，这条就照常出门了——
    /// 红名单读不懂时必须当它还在
    pub fn parse_loose(raw: &str) -> Self {
        raw.trim()
            .parse()
            .unwrap_or(MemorySensitivity::Secret)
    }
}

impl std::str::FromStr for MemorySensitivity {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let needle = value.trim().to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|level| level.as_str() == needle)
            .ok_or_else(|| format!(
                "敏感度「{needle}」不是认识的取值（可用：{}）",
                Self::ALL.iter().map(|level| level.as_str()).collect::<Vec<_>>().join(" | ")
            ))
    }
}

impl MemoryKind {
    /// 类型清单的唯一出处：解析认的、提示词里列的、UI 下拉给的都从这里派生。
    /// 各写一份的话，加一个 kind 就会有一处漏掉，模型给出新类型时整条被丢
    pub const ALL: [MemoryKind; 7] = [
        MemoryKind::Preference,
        MemoryKind::Fact,
        MemoryKind::Decision,
        MemoryKind::Event,
        MemoryKind::Rule,
        MemoryKind::Profile,
        MemoryKind::Relationship,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preference => "preference",
            Self::Fact => "fact",
            Self::Decision => "decision",
            Self::Event => "event",
            Self::Rule => "rule",
            Self::Profile => "profile",
            Self::Relationship => "relationship",
        }
    }

    /// 提示词里那一串取值说明
    pub fn contract() -> String {
        Self::ALL.iter().map(|kind| kind.as_str()).collect::<Vec<_>>().join("|")
    }
}

impl std::str::FromStr for MemoryKind {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let needle = value.trim().to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == needle)
            .ok_or_else(|| format!(
                "类型「{needle}」不是认识的取值（可用：{}）",
                Self::ALL.iter().map(|kind| kind.as_str()).collect::<Vec<_>>().join(" | ")
            ))
    }
}

impl MemoryScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::Session => "session",
            Self::Temp => "temp",
        }
    }

    /// 作用域越近权重越高。检索打分用它
    pub fn weight(self) -> f64 {
        match self {
            Self::Temp => 1.0,
            Self::Session => 0.9,
            Self::Project => 0.8,
            Self::Global => 0.6,
        }
    }
}

impl MemoryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Active => "active",
            Self::Archived => "archived",
            Self::Deleted => "deleted",
        }
    }
}

impl Stability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Volatile => "volatile",
        }
    }
}

impl MemorySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Inferred => "inferred",
            Self::Import => "import",
        }
    }
}

macro_rules! enum_parse {
    ($ty:ty, $name:literal, [ $($variant:ident => $text:literal),+ $(,)? ]) => {
        impl std::str::FromStr for $ty {
            type Err = String;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value.trim().to_ascii_lowercase().as_str() {
                    $( $text => Ok(Self::$variant), )+
                    other => Err(format!(
                        "{}「{}」不是认识的取值（可用：{}）",
                        $name, other,
                        [ $( $text ),+ ].join(" | ")
                    )),
                }
            }
        }
    };
}

// serde 的 #[serde(default)] 需要一个落点：老文件缺这个字段时该当成什么。
// 全部选保守的那一个——猜错也不冒充更强的话语权
impl std::default::Default for MemoryScope {
    fn default() -> Self { Self::Global }
}
impl std::default::Default for MemoryStatus {
    fn default() -> Self { Self::Candidate }
}
impl std::default::Default for Stability {
    fn default() -> Self { Self::Stable }
}
impl std::default::Default for MemorySource {
    fn default() -> Self { Self::User }
}

// MemoryKind 的取值表就是它自己的 ALL，所以它的 FromStr 手写在那儿，不再列一份
enum_parse!(MemoryScope, "作用域", [
    Global => "global", Project => "project", Session => "session", Temp => "temp",
]);
enum_parse!(MemoryStatus, "状态", [
    Candidate => "candidate", Active => "active", Archived => "archived", Deleted => "deleted",
]);
enum_parse!(Stability, "稳定性", [ Stable => "stable", Volatile => "volatile" ]);
enum_parse!(MemorySource, "来源", [
    User => "user", Assistant => "assistant", Inferred => "inferred", Import => "import",
]);

/// 新记忆 id。日期前缀让人在文件里一眼看出这条什么时候写的
pub fn new_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let today = chrono::Local::now().format("%Y%m%d");
    // 纳秒低位 + 一个进程内计数，防同一纳秒内连续两条撞 id
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("mem_{today}_{:x}{:x}", (now as u32) ^ seq, seq)
}

pub fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339()
}

fn scalar(value: &str) -> String {
    let trimmed = value.trim();
    trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(trimmed)
        .to_string()
}

fn string_list(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    let inner = match trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        Some(inner) => inner,
        None => return Vec::new(),
    };
    inner
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty() && *item != "null")
        .map(|item| scalar(item))
        .collect()
}

fn required_number(key: &str, value: &str) -> Result<f64, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("{key} 得是个数字，实际是「{}」", value.trim()))
}

/// 解析一个 frontmatter 块。`lines` 已经去掉首尾的 `---`
fn parse_fields(lines: &[&str]) -> Result<Vec<(String, String)>, String> {
    let mut fields = Vec::new();
    for line in lines {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        // 缩进行是块状列表或嵌套映射，本格式不用。见到就明说，别静默丢内容
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(format!("frontmatter 不支持缩进行：「{line}」"));
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(format!("frontmatter 行缺冒号：「{line}」"));
        };
        fields.push((key.trim().to_string(), value.trim().to_string()));
    }
    Ok(fields)
}

const KNOWN_KEYS: [&str; 20] = [
    "id",
    "type",
    "scope",
    "project_id",
    "status",
    "importance",
    "confidence",
    "stability",
    "source",
    "sensitivity",
    "created_at",
    "updated_at",
    "occurred_at",
    "reinforced_at",
    "origin",
    "last_used_at",
    "ttl_days",
    "tags",
    "entities",
    "supersedes",
];

/// `conflicts_with` 住在 extra 里而不是顶层字段：它是提取时打的标记，
/// 用户在文件里删掉它就等于"我看过了，别再说它们打架"
const CONFLICT_KEY: &str = "conflicts_with";
/// 用户看过、并且决定"两条都留着"的那一对。裁决必须留在真相源里：只清标记的话，
/// 下次现算又会把它端上来，等于逼用户裁决同一件事两次
const SETTLED_KEY: &str = "conflict_settled";

fn values_under(extra: &[(String, String)], key: &str) -> Vec<String> {
    extra
        .iter()
        .filter(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
        .filter(|value| !value.is_empty())
        .collect()
}

impl MemoryRecord {
    fn from_fields(fields: &[(String, String)], content: &str) -> Result<Self, String> {
        let get = |key: &str| {
            fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        let optional = |key: &str| {
            get(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty() && value != "null")
        };
        let need = |key: &str| {
            optional(key).ok_or_else(|| format!("frontmatter 缺 {key}"))
        };

        let importance = need("importance")?;
        let importance = required_number("importance", &importance)? as u32;
        let confidence = need("confidence")?;
        let confidence = required_number("confidence", &confidence)?;
        // 出处读不懂就整条报错。当成"没有出处"放行，等于让一条来历不明的记录
        // 冒充用户自己写的，而它还会继续被注入
        let origin = match optional("origin") {
            Some(text) => Some(Origin::decode(&text)?),
            None => None,
        };

        let record = Self {
            id: need("id")?,
            kind: need("type")?.parse()?,
            scope: need("scope")?.parse()?,
            project_id: optional("project_id"),
            status: need("status")?.parse()?,
            importance,
            confidence,
            stability: need("stability")?.parse()?,
            source: need("source")?.parse()?,
            // 没写就是 `public`。写了却读不懂要报错，不能当成没写——
            // 一个人写 `sensitivity: secrt` 是想让这条不出去，退化成 public 就是反着来
            sensitivity: optional("sensitivity")
                .map(|value| value.parse::<MemorySensitivity>())
                .transpose()?
                .unwrap_or_default(),
            created_at: need("created_at")?,
            updated_at: need("updated_at")?,
            occurred_at: optional("occurred_at"),
            reinforced_at: optional("reinforced_at"),
            origin,
            last_used_at: optional("last_used_at"),
            ttl_days: optional("ttl_days").and_then(|value| value.parse().ok()),
            tags: string_list(&get("tags").unwrap_or_default()),
            entities: string_list(&get("entities").unwrap_or_default()),
            supersedes: string_list(&get("supersedes").unwrap_or_default()),
            content: content.trim().to_string(),
            extra: fields
                .iter()
                .filter(|(name, _)| !KNOWN_KEYS.contains(&name.as_str()))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        };
        record.validate()?;
        Ok(record)
    }

    /// 手编辑的文件是主要错误来源，所以校验要说清是哪一条哪个字段
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("id 不能为空。".into());
        }
        if !(1..=5).contains(&self.importance) {
            return Err(format!(
                "{} 的 importance 得在 1–5 之间，实际是 {}",
                self.id, self.importance
            ));
        }
        if !(0.0..=1.0).contains(&self.confidence) {
            return Err(format!(
                "{} 的 confidence 得在 0–1 之间，实际是 {}",
                self.id, self.confidence
            ));
        }
        if self.content.trim().is_empty() {
            return Err(format!("{} 的正文是空的。", self.id));
        }
        if self.scope == MemoryScope::Project && self.project_id.as_deref().unwrap_or("").is_empty() {
            return Err(format!("{} 是项目作用域，但没写 project_id。", self.id));
        }
        if let Some(origin) = &self.origin {
            origin.check()?;
        }
        // 时间语义要站得住：`occurred_at` 会被时间线拿去排序，认不出日期的
        // 一句话（"上周三"）留在文件里只会排出一个假顺序，所以写的时候就要挡住
        if let Some(occurred) = self.occurred_at.as_deref() {
            if super::decay::parse_stamp(occurred).is_none() {
                return Err(format!(
                    "{} 的 occurred_at「{occurred}」读不出日期。写 YYYY-MM-DD 或 RFC3339，\
                     或者干脆别写——没人说过事情什么时候发生，就让它空着。",
                    self.id
                ));
            }
        }
        if let Some(reinforced) = self.reinforced_at.as_deref() {
            if super::decay::parse_stamp(reinforced).is_none() {
                return Err(format!("{} 的 reinforced_at「{reinforced}」读不出日期。", self.id));
            }
        }
        Ok(())
    }

    /// 参与检索的可索引文本：正文 + 标签。标签要进 FTS，否则
    /// "按 tag 过滤"只能靠元数据列，用户搜「沟通风格」搜不到
    pub fn searchable(&self) -> String {
        if self.tags.is_empty() {
            return self.content.clone();
        }
        format!("{}\n{}", self.content, self.tags.join(" "))
    }

    /// 提取时打下的"这条和那条撞了"标记。可能不止一条：同一条说法后来又能撞上别人
    pub fn conflict_peers(&self) -> Vec<String> {
        values_under(&self.extra, CONFLICT_KEY)
    }

    /// 用户已经裁决过"两条都留"的那些对手
    pub fn settled_peers(&self) -> Vec<String> {
        values_under(&self.extra, SETTLED_KEY)
    }

    /// 打过"两个都留"的裁决没有。冲突视图正反两个方向都要问，所以两边各记一份
    pub fn settled_with(&self, peer: &str) -> bool {
        self.settled_peers().iter().any(|held| held == peer)
    }

    pub fn to_markdown(&self) -> String {
        let mut text = String::from("---\n");
        let _ = writeln!(text, "id: {}", self.id);
        let _ = writeln!(text, "type: {}", self.kind.as_str());
        let _ = writeln!(text, "scope: {}", self.scope.as_str());
        let _ = writeln!(
            text,
            "project_id: {}",
            self.project_id.clone().unwrap_or_else(|| "null".into())
        );
        let _ = writeln!(text, "status: {}", self.status.as_str());
        let _ = writeln!(text, "importance: {}", self.importance);
        let _ = writeln!(text, "confidence: {}", self.confidence);
        let _ = writeln!(text, "stability: {}", self.stability.as_str());
        let _ = writeln!(text, "source: {}", self.source.as_str());
        // 只有收紧到 private / secret 才落这一行，理由见字段上的注释
        if self.sensitivity != MemorySensitivity::Public {
            let _ = writeln!(text, "sensitivity: {}", self.sensitivity.as_str());
        }
        let _ = writeln!(text, "created_at: {}", self.created_at);
        let _ = writeln!(text, "updated_at: {}", self.updated_at);
        let _ = writeln!(
            text,
            "occurred_at: {}",
            self.occurred_at.clone().unwrap_or_else(|| "null".into())
        );
        let _ = writeln!(
            text,
            "reinforced_at: {}",
            self.reinforced_at.clone().unwrap_or_else(|| "null".into())
        );
        if let Some(origin) = &self.origin {
            let _ = writeln!(text, "origin: {}", origin.encode());
        }
        let _ = writeln!(
            text,
            "last_used_at: {}",
            self.last_used_at.clone().unwrap_or_else(|| "null".into())
        );
        let _ = writeln!(
            text,
            "ttl_days: {}",
            self.ttl_days.map(|value| value.to_string()).unwrap_or_else(|| "null".into())
        );
        let _ = writeln!(text, "tags: [{}]", self.tags.join(", "));
        // 空就不写这一行：多写一行会让每条既有记录的正文哈希变掉，
        // 于是"升级"看起来像"所有记忆都被改过一次"
        if !self.entities.is_empty() {
            let _ = writeln!(text, "entities: [{}]", self.entities.join(", "));
        }
        let _ = writeln!(text, "supersedes: [{}]", self.supersedes.join(", "));
        for (key, value) in &self.extra {
            let _ = writeln!(text, "{key}: {value}");
        }
        let _ = write!(text, "---\n\n{}\n", self.content.trim());
        text
    }

    /// 注入给模型的那一行。来源与时间必须带着——不然模型无从判断
    /// 这条是用户明说的还是它自己三周前推的，也无从判断说的是"什么时候的事"。
    /// 记录自己不知道该文件叫什么（同一份正文可能被写进不同文件），所以这一路
    /// 只给作用域；知道路径的那一路（检索注入）给的是文件。
    /// 生产注入走 [`injection_line_for`]（inject.rs 传得进路径），这个方法只有测试在用
    #[cfg(test)]
    pub fn injection_line(&self) -> String {
        let scope = match self.project_id.as_deref() {
            Some(id) => format!("{}:{id}", self.scope.as_str()),
            None => self.scope.as_str().to_string(),
        };
        injection_line_for(
            self.kind.as_str(),
            &scope,
            &self.updated_at,
            self.occurred_at.as_deref(),
            self.confidence,
            &self.content,
        )
    }
}

impl MemoryRecord {
    /// 打上冲突标记。由记录自己管这些字段，别让调用方直接往 extra 里塞字符串：
    /// 那两个键名同时也是索引里 `conflicts_with` / `conflict_settled` 边的来源
    pub fn mark_conflict(&mut self, peer: &str) {
        if !self.conflict_peers().iter().any(|held| held == peer) {
            self.extra.push((CONFLICT_KEY.into(), peer.into()));
        }
    }

    /// 用户选了"两个都留着"：把待裁决的标记换成"已裁决"。
    /// 留着标记，设置页会一直报同一件已经没人需要裁决的事
    pub fn settle_with(&mut self, peer: &str) {
        self.extra.retain(|(key, value)| !(key == CONFLICT_KEY && value == peer));
        if !self.settled_with(peer) {
            self.extra.push((SETTLED_KEY.into(), peer.into()));
        }
    }

    /// 把冲突账整个清掉（蒸馏产物、以及选边之后）。裁决记录 `conflict_settled`
    /// 不归这里管——它是用户的决定，不是系统的待办
    pub fn clear_conflicts(&mut self) {
        self.extra.retain(|(key, _)| key != CONFLICT_KEY);
    }

    /// 一条待填的记录：id 与时间戳就地生成，其余字段留成调用方一定会覆写的默认值。
    /// 有它，显式记住 / 自动提取 / 蒸馏落地才不会各写一份结构体字面量
    pub fn draft(scope: MemoryScope, content: &str) -> Self {
        let now = now_rfc3339();
        Self {
            id: new_id(),
            kind: MemoryKind::Preference,
            scope,
            project_id: None,
            status: MemoryStatus::Active,
            importance: 3,
            confidence: 1.0,
            stability: Stability::Stable,
            source: MemorySource::User,
            sensitivity: MemorySensitivity::Public,
            created_at: now.clone(),
            updated_at: now,
            occurred_at: None,
            reinforced_at: None,
            origin: None,
            last_used_at: None,
            ttl_days: None,
            tags: Vec::new(),
            entities: Vec::new(),
            supersedes: Vec::new(),
            content: content.to_string(),
            extra: Vec::new(),
        }
    }
}

/// 注入行的唯一拼法。记录侧和索引命中侧都走这里——两处各写一句格式，
/// 早晚会出现"面板上看到的和模型收到的不一样"。
/// "更新"是这条记录被写下的时间，"发生"是事情发生的时间：两个都给出来，
/// 模型才知道"三年前记下、上周又发生的事"和"上周记下的猜测"不是一回事。
/// `occurred_at` 缺席时不多嘴——没人说过事情什么时候发生，就没有这一段
pub fn injection_line_for(
    kind: &str,
    source: &str,
    updated_at: &str,
    occurred_at: Option<&str>,
    confidence: f64,
    content: &str,
) -> String {
    fn day(stamp: &str) -> &str {
        stamp.split('T').next().unwrap_or(stamp)
    }
    let flat = content.trim().replace('\n', " ");
    match occurred_at {
        Some(occurred) => format!(
            "- [{kind} | 来源: {source} | 更新: {} | 发生: {} | 置信: {confidence:.2}] {flat}",
            day(updated_at),
            day(occurred)
        ),
        None => format!(
            "- [{kind} | 来源: {source} | 更新: {} | 置信: {confidence:.2}] {flat}",
            day(updated_at)
        ),
    }
}

/// 解析整个记录文件。没有 frontmatter 的文件（比如用户新建的空文件）返回空表
pub fn parse_records(text: &str) -> Result<Vec<MemoryRecord>, String> {
    let mut records = Vec::new();
    let mut block: Vec<String> = Vec::new();
    let mut in_block = false;
    let mut line_no = 0usize;

    let lines: Vec<&str> = text.lines().collect();
    while line_no < lines.len() {
        let line = lines[line_no];
        line_no += 1;
        if !in_block {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            if line.trim_end() != "---" {
                // 块外的散文忽略但不报错：daily 日志允许有抬头说明
                continue;
            }
            in_block = true;
            block.clear();
            continue;
        }
        if line.trim_end() == "---" {
            // 剩下到下一个 `---` 之前都是正文
            let mut body: Vec<&str> = Vec::new();
            while line_no < lines.len() && lines[line_no].trim_end() != "---" {
                body.push(lines[line_no]);
                line_no += 1;
            }
            let fields = parse_fields(&block.iter().map(String::as_str).collect::<Vec<_>>())?;
            records.push(MemoryRecord::from_fields(&fields, &body.join("\n"))?);
            in_block = false;
            continue;
        }
        block.push(line.to_string());
    }

    if in_block {
        return Err("文件结尾少了一个 `---`，frontmatter 块没闭合。".into());
    }
    Ok(records)
}

pub fn render_records(records: &[MemoryRecord]) -> String {
    let mut text = String::new();
    for record in records {
        text.push_str(&record.to_markdown());
        text.push('\n');
    }
    text
}

// 敏感信息判定已经抽到 `crate::secrets`：写记忆、外发出口、审计行共用同一份规则。
// 这里只做转发，让 `memory::record` 的老调用方一行不改也能编译。
pub use crate::secrets::{leaks_sensitive, marked_do_not_store};

