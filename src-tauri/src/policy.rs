//! 权限分级表。**全应用只在这个文件里定义 capability 与 level**——
//! 工具运行时、任务引擎、编排器都从这里 import，别处复制一份枚举就是第二真相
//! （`deliverables/design-security-permission.md` §边界）。
//!
//! 一次动作先被翻译成 `(capability, target, fingerprint)`，再由 [`Policy::check`]
//! 决定 Allow / Ask / Deny。表是纯数据，判定是纯函数，两侧都能单测。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 路径范围。`Any` 表示"项目根之外"——今天这条路本来就是硬错，落成 Deny 而不是新行为
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PathScope {
    /// 当前项目根之内
    ProjectRoot,
    /// 客户端自己的工作目录（缓存、临时目录）
    Workspace,
    /// 根之外的任意路径
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileMode {
    Read,
    Write,
    /// 删除是一等操作（design-security-center.md D1）：`delete_file` 工具是唯一的
    /// 生产者，shell 里的 rm/del 不算——回收站、批量阈值这些删除特有的语义
    /// 只有在自家工具里才做得到
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NetScope {
    /// 已配置的模型服务商。它本来就是必发的，不为一句话弹一次窗
    Provider,
    Localhost,
    /// 用户在配置里点过名的 host
    Configured,
    /// 任意目标。今天这一维根本不存在，本表把它补上
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExecScope {
    Git,
    Build,
    /// 认不出类别的那一类命令行（以及来路不明的工具名）。**不带"是哪条命令"那一格**：
    /// 判定、审批键、权限表读的都只有 `exec.arbitrary` 这一段，带上一个没人读的
    /// `pattern` 就是让类型看着比表格细，而真要让 `exec.notion` 那种键生效，
    /// 那是一次放宽（见 `is_known_key` 那段），不是把这个字段接上就算完
    Arbitrary,
    /// 模型点名动了沙箱边界（全局开着要 `sandbox=off` 脱壳，或全局没开要
    /// `sandbox=on` 给项目文件打完整性标签）。这不是"跑一条命令"，是改这道命令
    /// 在哪层隔离里跑——所以单独一行：逐调用策略（对齐 deepseek 的 per-call
    /// sandbox policy）要让人看得见、答得了，不能藏在命令那一行里悄悄放行
    SandboxOverride,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryMode {
    Read,
    Write,
    Export,
    Wipe,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputScope {
    /// 看得见别的程序：列窗口、读某个窗口的控件树
    Observe,
    /// 动得了解别的程序：合成鼠标与键盘输入
    Act,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "dimension")]
pub enum Capability {
    File { scope: PathScope, mode: FileMode },
    Net { scope: NetScope },
    Exec { scope: ExecScope },
    Tool { name: String },
    Memory { mode: MemoryMode },
    /// Computer Use。它和 `exec.arbitrary` 不是一回事：命令跑的是这台机器授权给
    /// 这个进程的事，而合成输入做的是**用户能做的事**——点哪个窗口、往哪敲字，
    /// 客户端一概看不见也拦不住，所以它要自己一档
    Input { scope: InputScope },
}

impl Capability {
    /// 覆盖项用的稳定键。改键名等于改配置文件的格式，要连带改迁移
    pub fn key(&self) -> String {
        match self {
            Capability::File { scope, mode } => format!(
                "file.{}.{}",
                match mode {
                    FileMode::Read => "read",
                    FileMode::Write => "write",
                    FileMode::Delete => "delete",
                },
                match scope {
                    PathScope::ProjectRoot => "projectRoot",
                    PathScope::Workspace => "workspace",
                    PathScope::Any => "any",
                }
            ),
            Capability::Net { scope } => format!(
                "net.{}",
                match scope {
                    NetScope::Provider => "provider",
                    NetScope::Localhost => "localhost",
                    NetScope::Configured => "configured",
                    NetScope::Any => "any",
                }
            ),
            Capability::Exec { scope } => format!(
                "exec.{}",
                match scope {
                    ExecScope::Git => "git",
                    ExecScope::Build => "build",
                    ExecScope::Arbitrary => "arbitrary",
                    ExecScope::SandboxOverride => "sandboxOverride",
                }
            ),
            Capability::Tool { name } => format!("tool.{name}"),
            Capability::Memory { mode } => format!(
                "memory.{}",
                match mode {
                    MemoryMode::Read => "read",
                    MemoryMode::Write => "write",
                    MemoryMode::Export => "export",
                    MemoryMode::Wipe => "wipe",
                }
            ),
            Capability::Input { scope } => format!(
                "input.{}",
                match scope {
                    InputScope::Observe => "observe",
                    InputScope::Act => "act",
                }
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// 直接拒，问都不问
    Deny,
    /// 必须有人点头。缺省就是它：认不出来的写法一律退回最严的那一档，
    /// 与 `mode_from_legacy` 那条同方向
    #[default]
    Ask,
    /// 在声明的范围内自动放行，越界退化成 Ask
    Scoped,
    /// 放行
    Allow,
}

/// 全局档。与 `config.permission` 的三个旧字符串一一对应，`ask` 之外的一律按更严处理
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Ask,
    Auto,
    Full,
}

/// 旧值迁移：认不出来就退回最严的那一档，而不是退回到 `full`
pub fn mode_from_legacy(raw: &str) -> Mode {
    match raw.trim().to_ascii_lowercase().as_str() {
        "auto" => Mode::Auto,
        "full" => Mode::Full,
        _ => Mode::Ask,
    }
}

/// 这一条话题现在准不准动手。它与 [`Mode`] 是**两个独立的轴**：`Mode` 问"这一下要不要
/// 有人点头"，`Phase` 问"这一阶段允不允许有这一下"。
///
/// 它是红线不是问句：`Plan` 下执行侧直接拒，`full` 档开不动它，覆盖项也撤不掉它——
/// 与 `constrain::is_catastrophic` 同一条路。理由是这个应用自己写过的教训：往轻了说的
/// 那道闸门比没有那道闸门更危险，而"切了个全局档位就把规划模式解开"正是那种轻描淡写
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Phase {
    /// 按权限档位走
    #[default]
    Chat,
    /// 规划模式：只读的那几条路还能走，会动东西的一律拒
    Plan,
}

/// 一条覆盖项。`key` 是 [`Capability::key`] 的前缀（分段匹配，不做子串），
/// `level` 只能往严了改——放松要过全局那一档，见 [`Policy::resolve`]。
///
/// 它是配置里的形状而不是 `Capability` 本身：项目那一份要能写 `exec`（整条都收紧），
/// 而 `exec.<某个命令>` 那一档永远命中不了：它的键 collapse 成 `exec.arbitrary`
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PermissionOverride {
    pub key: String,
    pub level: Level,
}

/// 这张表认得的键前缀。写错一个字母的键永远不会命中任何一次判定，界面上却显示着
/// "我已经拦了 git"——所以存进去之前先问这一句，把它当场拒掉
pub const KNOWN_KEYS: [&str; 20] = [
    "file",
    "file.read",
    "file.read.projectRoot",
    "file.read.workspace",
    "file.read.any",
    "file.write",
    "file.write.projectRoot",
    "file.write.workspace",
    "file.write.any",
    "net",
    "net.provider",
    "net.localhost",
    "net.configured",
    "net.any",
    "exec",
    "tool",
    "memory",
    "input",
    "input.observe",
    "input.act",
];

/// 能不能认这个键。规则只有一条：**这条键得有机会命中一次判定**。
/// 覆盖项是按最长前缀去配 `capability.key()` 的，所以 `exec.notion` 与 `memory.notion`
/// 这种"看着像一条规则"的键永远不会生效——`ExecScope::Arbitrary` 的键 collapse 成
/// `exec.arbitrary`，而 memory 只有那四个动作。存一条永不命中的键，等于在表上挂一条装饰，
/// 而界面会照着它显示"我已经拦了 notion"。判据不另写一份：拿 [`capability_of`] 把键换算成
/// capability，再问它 `key()` 是不是原来那一串。`tool.<名字>` 仍然只按前缀判：
/// 工具名是开放的（内置 + 每台扩展各自的名字），写错的那个只会是"没人调它"，不会伪装成拦住了什么
pub fn is_known_key(key: &str) -> bool {
    let key = key.trim();
    if key.is_empty() {
        return false;
    }
    if KNOWN_KEYS.contains(&key) {
        return true;
    }
    if let Some(name) = key.strip_prefix("tool.") {
        return !name.trim().is_empty();
    }
    capability_of(key).is_some_and(|cap| cap.key() == key)
}

/// 键 → 一次判定要问的那条 capability。表、校验、覆盖项预览都读它：
/// 别处再写一份 switch 就是第二真相
pub fn capability_of(key: &str) -> Option<Capability> {
    let (head, rest) = key.split_once('.')?;
    let file = |scope: &str, mode: FileMode| {
        let scope = match scope {
            "projectRoot" => PathScope::ProjectRoot,
            "workspace" => PathScope::Workspace,
            "any" => PathScope::Any,
            _ => return None,
        };
        Some(Capability::File { scope, mode })
    };
    match (head, rest) {
        ("file", rest) if rest.starts_with("read.") => file(&rest[5..], FileMode::Read),
        ("file", rest) if rest.starts_with("write.") => file(&rest[6..], FileMode::Write),
        ("file", rest) if rest.starts_with("delete.") => file(&rest[7..], FileMode::Delete),
        ("net", "provider") => Some(Capability::Net { scope: NetScope::Provider }),
        ("net", "localhost") => Some(Capability::Net { scope: NetScope::Localhost }),
        ("net", "configured") => Some(Capability::Net { scope: NetScope::Configured }),
        ("net", "any") => Some(Capability::Net { scope: NetScope::Any }),
        ("exec", "git") => Some(Capability::Exec { scope: ExecScope::Git }),
        ("exec", "build") => Some(Capability::Exec { scope: ExecScope::Build }),
        ("exec", "sandboxOverride") => Some(Capability::Exec { scope: ExecScope::SandboxOverride }),
        ("exec", pattern) if !pattern.is_empty() => Some(Capability::Exec { scope: ExecScope::Arbitrary }),
        ("tool", name) if !name.is_empty() => Some(Capability::Tool { name: name.to_string() }),
        ("memory", "read") => Some(Capability::Memory { mode: MemoryMode::Read }),
        ("memory", "write") => Some(Capability::Memory { mode: MemoryMode::Write }),
        ("memory", "export") => Some(Capability::Memory { mode: MemoryMode::Export }),
        ("memory", "wipe") => Some(Capability::Memory { mode: MemoryMode::Wipe }),
        ("input", "observe") => Some(Capability::Input { scope: InputScope::Observe }),
        ("input", "act") => Some(Capability::Input { scope: InputScope::Act }),
        _ => None,
    }
}

/// 界面上那张表要列的行。列的是**判定面**而不是"谁被改过"：一张只列出被改过那几行的表，
/// 回答不了"这一行今天到底是几档"，而那正是这张表存在的全部理由
/// 这张表上每一行都要有人在运行时 `resolve` 它。加一行的正确顺序是先找到那个人的位置、
/// 再往这里写：一行没人问的表比没有这一行更坏，它会一直显示着"我拦住了"
/// （`memory.*` 与 `net.*` 那两批各有过一次，判据见 design-security-permission.md §18/§19）
pub const TABLE: [&str; 23] = [
    "file.read.projectRoot",
    "file.read.workspace",
    "file.read.any",
    "file.write.projectRoot",
    "file.write.workspace",
    "file.write.any",
    "file.delete.projectRoot",
    "file.delete.workspace",
    "file.delete.any",
    "net.provider",
    "net.localhost",
    "net.configured",
    "net.any",
    "exec.git",
    "exec.build",
    "exec.arbitrary",
    "exec.sandboxOverride",
    "memory.read",
    "memory.write",
    "memory.export",
    "memory.wipe",
    "input.observe",
    "input.act",
];

/// 一行表：键、这一刻的档位、这一行的数字是谁给的。
///
/// `source` 说的是"屏幕上这个数来自哪一层"（`档位` / `全局` / `项目`）；
/// 只显示一个数而不说它是哪一层给的，用户就没法核对"我给这个项目加的那一行生效了没有"
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRow {
    pub key: String,
    pub level: Level,
    pub source: &'static str,
}

/// 这张表。`global` 与 `project` 是那两层原始覆盖项，只用来判 `source`——
/// 档位本身由 `policy` 回答，这里不再算一遍判定（两处判定就是两份真相）
pub fn table(
    policy: &Policy,
    global: &[PermissionOverride],
    project: &[PermissionOverride],
) -> Vec<PermissionRow> {
    TABLE
        .iter()
        .filter_map(|key| Some((*key, capability_of(key)?)))
        .map(|(key, cap)| {
            let level = policy.resolve(&cap);
            // 从最具体的一层往回问：谁的数字正好是屏幕上这个，就记在它头上
            let source = if matched(project, key) == Some(level) {
                "项目"
            } else if matched(global, key) == Some(level) {
                "全局"
            } else {
                "档位"
            };
            PermissionRow { key: key.to_string(), level, source }
        })
        .collect()
}

/// 这组覆盖项里，最长前缀命中的那一条给的是几档。与 [`Policy::override_level`] 同一条规则，
/// 所以表上看到的来源与判定实际用的那一层是同一件事
fn matched(overrides: &[PermissionOverride], key: &str) -> Option<Level> {
    let mut best: Option<(usize, Level)> = None;
    for item in overrides {
        let pattern = item.key.trim();
        if !pattern.is_empty() && hits(pattern, key) {
            if best.map(|(len, _)| pattern.len() > len).unwrap_or(true) {
                best = Some((pattern.len(), item.level));
            }
        }
    }
    best.map(|(_, level)| level)
}

/// 前缀命中：整键相等，或者按**分段**相等（`tool.read` 不该命中 `tool.read_file`）
fn hits(pattern: &str, key: &str) -> bool {
    key == pattern || (key.starts_with(pattern) && key.as_bytes().get(pattern.len()) == Some(&b'.'))
}

/// 全局档 + 两份覆盖项 → 这一刻生效的那张表。
///
/// **合并规则只有一条：越具体的层只能往严了改，不能往松了改。**
/// 项目那一条与全局那一条同键时取更严的那个，所以一个项目撤销不了用户在总设置里划的红线；
/// 而"全局 `exec`、项目 `exec.git`"这种更长前缀仍然由 `Policy::override_level` 按最长命中处理
pub fn effective(
    mode: Mode,
    global: &[PermissionOverride],
    project: &[PermissionOverride],
    delete_batch_ask: usize,
) -> Policy {
    let mut overrides: Vec<(String, Level)> = Vec::new();
    for item in global.iter().chain(project.iter()) {
        let key = item.key.trim();
        if key.is_empty() {
            continue;
        }
        match overrides.iter_mut().find(|(held, _)| held == key) {
            // 同一个键被两层各写了一次：赢的是更严的那一条，与"谁写在后面"无关
            Some(slot) => {
                if strictness(item.level) > strictness(slot.1) {
                    slot.1 = item.level;
                }
            }
            None => overrides.push((key.to_string(), item.level)),
        }
    }
    Policy {
        mode,
        overrides,
        phase: Phase::default(),
        delete_batch_ask,
        file_rules: Vec::new(),
        command_blocklist: Vec::new(),
        command_rules: Vec::new(),
        network_rules: Vec::new(),
        net_http_remote: crate::file_rules::RuleAction::Ask,
        net_http_local: crate::file_rules::RuleAction::Allow,
    }
}

#[derive(Debug, Clone)]
pub struct Policy {
    pub mode: Mode,
    /// 覆盖项：`(键前缀, level)`。后写的赢，所以用户可以拿一条精确键盖过一条宽前缀
    pub overrides: Vec<(String, Level)>,
    /// 这一张表服务的那条话题准不准动手。它不从配置里来，由话题的作业模式在每回合
    /// 现读现打（`chat.rs` 里那句 `with_phase`），所以切话题不会解开另一条话题的红线
    pub phase: Phase,
    /// 批量删除审批阈值（design-security-center.md D1）：一次 `delete_file` 的路径数
    /// 达到它就强制问人，档位与覆盖项都压不住——这是用户显式配的闸，不是表的判断。
    /// 0 = 不设阈值
    pub delete_batch_ask: usize,
    /// 文件安全规则表（design-security-center.md D2）。项目表在前、全局表在后这个
    /// 顺序由装配处（`AppConfig::policy`）保证——这里只存合并好的那一份
    pub file_rules: Vec<crate::file_rules::FileRule>,
    /// 命令黑名单（design-security-center.md D4）：机器级，没有项目粒度。
    /// 命中即拒，档位与规则放行都翻不动它
    pub command_blocklist: Vec<String>,
    /// 命令前缀规则。项目表在前、全局表在后（同 file_rules 的合并口径）
    pub command_rules: Vec<crate::command_rules::CommandRule>,
    /// 网络安全规则（design-security-center.md D5）：域后缀 → 动作，机器级没有项目粒度
    pub network_rules: Vec<crate::egress::NetworkRule>,
    /// HTTP 明文分档：远程默认问（明文凭据上线的代价问一次不算贵）
    pub net_http_remote: crate::file_rules::RuleAction,
    /// HTTP 明文分档：回环默认放（本机端口调用天天有，问就是路障）
    pub net_http_local: crate::file_rules::RuleAction,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            mode: Mode::Ask,
            overrides: Vec::new(),
            phase: Phase::default(),
            delete_batch_ask: 50,
            file_rules: Vec::new(),
            command_blocklist: Vec::new(),
            command_rules: Vec::new(),
            network_rules: Vec::new(),
            net_http_remote: crate::file_rules::RuleAction::Ask,
            net_http_local: crate::file_rules::RuleAction::Allow,
        }
    }
}

impl Policy {
    /// 测试专用的直构造器。生产的唯一入口是 `AppConfig::policy()`（config.rs 里写明了
    /// 为什么三处各自 new 被收编成一处）；跨模块的四批测试用 new 直接摆档位
    #[cfg(test)]
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            overrides: Vec::new(),
            phase: Phase::default(),
            delete_batch_ask: 50,
            file_rules: Vec::new(),
            command_blocklist: Vec::new(),
            command_rules: Vec::new(),
            network_rules: Vec::new(),
            net_http_remote: crate::file_rules::RuleAction::Ask,
            net_http_local: crate::file_rules::RuleAction::Allow,
        }
    }

    /// 换成某一阶段的判定。档位与覆盖项一条不改——阶段只往上加红线，不改档位本来
    /// 许不许的事；反过来也没有任何覆盖项能把阶段解开
    #[must_use]
    pub fn with_phase(mut self, phase: Phase) -> Self {
        self.phase = phase;
        self
    }

    /// 装上合并好的文件规则表。项目在前、全局在后是**装配处的责任**：
    /// 首条命中即停的语义下，谁排在前面谁说了算
    #[must_use]
    pub fn with_file_rules(mut self, rules: Vec<crate::file_rules::FileRule>) -> Self {
        self.file_rules = rules;
        self
    }
    /// 装上合并好的命令黑名单与前缀规则（D4）。顺序责任同上：项目表在前
    #[must_use]
    pub fn with_command_rules(
        mut self,
        blocklist: Vec<String>,
        rules: Vec<crate::command_rules::CommandRule>,
    ) -> Self {
        self.command_blocklist = blocklist;
        self.command_rules = rules;
        self
    }
    /// 装上网络安全规则与 HTTP 明文分档（D5）
    #[must_use]
    pub fn with_net_rules(
        mut self,
        rules: Vec<crate::egress::NetworkRule>,
        http_remote: crate::file_rules::RuleAction,
        http_local: crate::file_rules::RuleAction,
    ) -> Self {
        self.network_rules = rules;
        self.net_http_remote = http_remote;
        self.net_http_local = http_local;
        self
    }

    /// 默认表：这一格就是"今天实际发生的事"的显式版本，按档写死，不再靠事后收紧推导。
    /// 读自动放行、项目内写在 `ask` 下要问（`auto` 起自动过）、命令与任意外发在 `ask`/`auto`
    /// 下都要问、根外写入直接拒——它今天在執行侧本来就是硬错，落成 Deny 而不是新行为
    fn grants(mode: Mode, cap: &Capability) -> Level {
        match cap {
            Capability::File { scope: PathScope::Any, mode: FileMode::Read } => Level::Ask,
            Capability::File { mode: FileMode::Read, .. } => Level::Allow,
            Capability::File { scope: PathScope::Any, mode: FileMode::Write } => Level::Deny,
            Capability::File { mode: FileMode::Write, .. } => match mode {
                // 项目内写入：ask 要问、auto 自动过、full 明确放行——与今天的
                // Elevated 三档行为一一对应，只是它现在写在纸上
                Mode::Ask => Level::Ask,
                Mode::Auto => Level::Scoped,
                Mode::Full => Level::Allow,
            },
            // 删除比写严一档：回收站兜得住误删，兜不住"删错了还没发现"。
            // 根外删除与根外写入同罪（硬错），项目内删除在 auto 档也要点头——
            // 这是把参考产品的「删除 = 询问」默认档写成纸面（design-security-center.md D1）
            Capability::File { scope: PathScope::Any, mode: FileMode::Delete } => Level::Deny,
            Capability::File { mode: FileMode::Delete, .. } => match mode {
                Mode::Full => Level::Allow,
                _ => Level::Ask,
            },
            Capability::Net { scope: NetScope::Provider } => Level::Allow,
            // 回环：今天真实发生的事是"不发问就投出去"（钩子指向本机端口时没人拦），
            // 所以这一档写 Allow。这一行新增的是"用户可以把它收紧"——收得住的那一半之前没人执行
            Capability::Net { scope: NetScope::Localhost } => Level::Allow,
            Capability::Net { scope: NetScope::Configured } => Level::Scoped,
            // 剩下这一档（`Any`＝没被分类的去处）取 Ask 是**地板**：新加一条出口时忘了分类自己，
            // 撞上的是"问一句"而不是"直接放行"。
            // 别把它当成 MCP 那一行——扩展与钩子的对外投递问的是 `net.configured`（已实现，有执行者）。
            // `Any` 今天**没有任何生产者**：全库构造的只有 Provider / Localhost / Configured 三种，
            // 所以把这一行划成红线目前改变不了任何行为。这条明账由
            // `the_net_rows_either_have_a_producer_or_are_named_exceptions` 钉着，接上时要一起改
            Capability::Net { .. } => {
                if mode == Mode::Full {
                    Level::Allow
                } else {
                    Level::Ask
                }
            }
            Capability::Exec { scope: ExecScope::SandboxOverride } => {
                // 动沙箱边界与跑命令本身同档：full 是用户明说过的口径，它说了算；
                // 其余档都问一句——脱壳（或第一次给文件打标签）不该趁人不注意
                if mode == Mode::Full {
                    Level::Allow
                } else {
                    Level::Ask
                }
            }
            Capability::Exec { .. } => {
                if mode == Mode::Full {
                    Level::Allow
                } else {
                    Level::Ask
                }
            }
            Capability::Memory { mode: MemoryMode::Wipe } => {
                if mode == Mode::Full {
                    Level::Allow
                } else {
                    Level::Ask
                }
            }
            // 记忆读写与导出：入口是用户自己按的，自动提取那条已经排在 write_gate 后面了，
            // 为它弹窗只会把"记住"变成"每轮都要点头"
            Capability::Memory { .. } => Level::Allow,
            // 看得见别的程序：与读项目内文件同档。列窗口与读控件树不改变任何状态
            Capability::Input { scope: InputScope::Observe } => Level::Allow,
            // 动得了解别的程序：ask / auto 都要点头。「完全访问」按他的口径直接放行——
            // 但敏感窗口那道闸在 computer.rs 里，它不认档口，full 也照拦
            Capability::Input { scope: InputScope::Act } => {
                if mode == Mode::Full {
                    Level::Allow
                } else {
                    Level::Ask
                }
            }
            // 具体工具名的级别由白名单与 scope 决定，档位在这里不额外收紧
            Capability::Tool { .. } => Level::Scoped,
        }
    }

    /// 覆盖项按最长前缀命中（键分段匹配，不做子串匹配：`tool.read` 不该命中 `tool.read_file`）。
    /// 命中规则与 [`matched`] 是同一个 `hits`，所以表上标的来源与实际生效的那一条不会分家
    fn override_level(&self, key: &str) -> Option<Level> {
        let mut best: Option<(usize, Level)> = None;
        for (pattern, level) in &self.overrides {
            if hits(pattern, key) {
                if best.map(|(len, _)| pattern.len() > len).unwrap_or(true) {
                    best = Some((pattern.len(), *level));
                }
            }
        }
        best.map(|(_, level)| level)
    }

    /// 全局档是天花板：覆盖项只能往严了改，放松要过这一档。
    /// 显式 `Deny` 永远赢，否则一个开关就能绕过用户自己划的红线
    pub fn resolve(&self, cap: &Capability) -> Level {
        let floor = Self::grants(self.mode, cap);
        match self.override_level(&cap.key()) {
            None => floor,
            Some(level) => {
                if level == Level::Deny || strictness(level) >= strictness(floor) {
                    level
                } else {
                    floor
                }
            }
        }
    }

    /// 一次动作同时要好几项能力时，取最严的那一条。
    /// 返回它对应的 capability，调用方拿它去 `check`，拒绝原因才说得出是哪一行拦的
    pub fn strictest<'c>(&self, caps: &'c [Capability]) -> Option<&'c Capability> {
        caps.iter()
            .max_by_key(|cap| strictness(self.resolve(cap)))
    }

    /// 判定。`fingerprint` 是"到底要跑哪一下"的规范化哈希，确认的就是这一份
    pub fn check(&self, cap: &Capability, target: &str, fingerprint: &str) -> Decision {
        match self.resolve(cap) {
            Level::Deny => Decision::Deny {
                reason: format!("权限表禁止这个动作（{}）。要放开就去设置 → 权限改这一行。", cap.key()),
            },
            Level::Ask => Decision::Ask {
                reason: format!("要执行「{target}」，先确认这一份：{}", short(fingerprint)),
                fingerprint: fingerprint.to_string(),
            },
            // Scoped 与 Allow 的差别只体现在越界时：越界由调用方换一条 capability 再问一次
            Level::Scoped | Level::Allow => Decision::Allow,
        }
    }
}

/// 表上那一档在**记忆那几个动作**上挡不挡人。
///
/// 这一族动作不过 [`Policy::check`]：那一条要弹审批，而 `grants` 里那句理由今天仍然成立
/// ——为"记住"弹窗会把每轮对话变成一次点头。所以这里只问一句"这一下有没有人点过头"：
/// - `Deny` 永远挡；
/// - `Ask` 只在 `attended` 为真时动手。`attended` = 这一下是人在界面上点的（点了"清空"、
///   点了"导入"），不是后台自己生产的（自动注入、自动提取、反思、蒸馏）。把 Ask 读成
///   Allow，那一行就又只剩装饰；
/// - `Allow` / `Scoped` 永远动手——表上这四行的默认值正落在这两档里，所以默认路径不变。
pub fn memory_acts(level: Level, attended: bool) -> bool {
    match level {
        Level::Deny => false,
        Level::Ask => attended,
        Level::Scoped | Level::Allow => true,
    }
}

/// 拦下来得说得出"哪一行、为什么、去哪儿改"。理由里不复述正文：那一格可能是用户自己
/// 标了私密的东西，而审计与错误文案都会落到盘上
pub fn memory_refused(cap: &Capability, level: Level) -> String {
    match level {
        Level::Deny => format!(
            "权限表把 {} 划成了红线：这一件事不做。要放开去设置 → 权限表改那一行。",
            cap.key()
        ),
        _ => format!(
            "{} 那一行写的是「要有人点头」，而这一件事是后台自己生产的，没有人为它点过头：不做。",
            cap.key()
        ),
    }
}

/// 严格程度排序：覆盖项与全局档比的是"谁更严"，不是"谁写在后面"
pub fn strictness(level: Level) -> u8 {
    match level {
        Level::Allow => 0,
        Level::Scoped => 1,
        Level::Ask => 2,
        Level::Deny => 3,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask { reason: String, fingerprint: String },
    Deny { reason: String },
}

fn short(fingerprint: &str) -> String {
    fingerprint.chars().take(12).collect()
}

/// 规范化指纹：把动作的零件按长度前缀拼起来再哈希。
/// 长度前缀是为了让 `["ab", "c"]` 与 `["a", "bc"]` 不撞成同一份
pub fn fingerprint(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.len().to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 命令规范化：折叠空白、去首尾。同一句 `git  status` 与 `git status` 必须是同一个指纹
pub fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(mode: FileMode, scope: PathScope) -> Capability {
        Capability::File { scope, mode }
    }

    #[test]
    fn legacy_permission_strings_migrate_to_modes() {
        assert_eq!(mode_from_legacy("ask"), Mode::Ask);
        assert_eq!(mode_from_legacy("auto"), Mode::Auto);
        assert_eq!(mode_from_legacy("full"), Mode::Full);
        assert_eq!(
            mode_from_legacy("yolo"),
            Mode::Ask,
            "认不出的值退回最严的一档：写错一个字母就变成完全访问，是不能接受的失败方式"
        );
    }

    #[test]
    fn an_override_cannot_loosen_the_global_mode() {
        // 这条是权限表最关键的一条：设置里勾一下就绕过弹窗，等于没有闸门
        let policy = Policy {
            mode: Mode::Ask,
            overrides: vec![("exec".into(), Level::Allow), ("file.write".into(), Level::Allow)], phase: Phase::Chat, delete_batch_ask: 50, file_rules: Vec::new(), command_blocklist: Vec::new(), command_rules: Vec::new(), network_rules: Vec::new(), net_http_remote: crate::file_rules::RuleAction::Ask, net_http_local: crate::file_rules::RuleAction::Allow}
        ;
        assert_eq!(policy.resolve(&Capability::Exec { scope: ExecScope::Git }), Level::Ask);
        assert_eq!(
            policy.resolve(&file(FileMode::Write, PathScope::ProjectRoot)),
            Level::Ask
        );
        // 读项目内文件在三档下都不弹，那是今天的行为，被写进表里而不是被某条覆盖项赏来的
        assert_eq!(
            policy.resolve(&file(FileMode::Read, PathScope::ProjectRoot)),
            Level::Allow,
            "读文件本来就不弹窗，把它算成「用户放松了限制」会让每一次读都要点头"
        );
        // 但收紧仍然有效
        let tighter = Policy {
            mode: Mode::Ask,
            overrides: vec![("file.read".into(), Level::Deny)], phase: Phase::Chat, delete_batch_ask: 50, file_rules: Vec::new(), command_blocklist: Vec::new(), command_rules: Vec::new(), network_rules: Vec::new(), net_http_remote: crate::file_rules::RuleAction::Ask, net_http_local: crate::file_rules::RuleAction::Allow}
        ;
        assert_eq!(tighter.resolve(&file(FileMode::Read, PathScope::ProjectRoot)), Level::Deny);
    }

    /// 那张表自己写着一句规矩："每一行都要有人在运行时 `resolve` 它，一行没人问的表比没有这一行
    /// 更坏"。`net.*` 这一族现在按它机械核一遍：每个 `NetScope` 要么在生产代码里被构造过，
    /// 要么被**点名成例外**。今天唯一的例外是 `Any`——那是"新出口忘了给自己分类"时撞上的地板，
    /// 本身还没有生产者，所以把 `net.any` 划成红线目前不改变任何行为。
    /// 两头都红得起来：有人接上 `Any` 而不清例外 → 红；新增一档 `NetScope` 而没安排出口 → 也红。
    ///
    /// **限度**：扫的是这三个文件（今天全部的构造点）。在**新文件**里造一个 `Net{Any}` 它看不见——
    /// 那一案仍靠"加一行前先找到那个人的位置"这条规矩，别把这根针当成全库的闸
    #[test]
    fn the_net_rows_either_have_a_producer_or_are_named_exceptions() {
        let exceptions: [&str; 1] = ["Any"];
        let net_sites = [
            include_str!("chat.rs"),
            include_str!("tool_runtime/mod.rs"),
            include_str!("tasks/hook.rs"),
        ];
        let code: String = net_sites
            .iter()
            .map(|file| file.replace('\r', ""))
            .collect::<Vec<_>>()
            .join("\n");
        for file in &net_sites {
            assert!(
                file.to_string().replace('\r', "").contains("Capability::Net"),
                "这张出口清单里有一个文件已经不再问网络那一行了：删掉它，否则这条针数的是幽灵"
            );
        }

        let policy_code = include_str!("policy.rs").replace('\r', "");
        let body = policy_code
            .split("pub enum NetScope {")
            .nth(1)
            .expect("NetScope 是个枚举")
            .split("\n}")
            .next()
            .unwrap_or_default();
        let scopes: Vec<String> = body
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//") && !line.starts_with('#'))
            .filter_map(|line| line.split(['{', '(', ',', ' ']).next().map(String::from))
            .filter(|name| name.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
            .collect();
        assert_eq!(
            scopes.len(),
            4,
            "NetScope 变了（{scopes:?}）：出口、表上那一行与那道地板要一起安排"
        );

        let mut unproduced: Vec<&str> = scopes
            .iter()
            .filter(|scope| !code.contains(&format!("NetScope::{scope}")))
            .map(String::as_str)
            .collect();
        unproduced.sort_unstable();
        assert_eq!(
            unproduced, exceptions,
            "表上有没人构造的 net 行，或那条例外已经落地却没清掉：{unproduced:?}"
        );
    }

    #[test]
    fn auto_mode_still_asks_for_commands_network_and_wipe() {
        let policy = Policy::new(Mode::Auto);
        assert_eq!(policy.resolve(&Capability::Exec { scope: ExecScope::Git }), Level::Ask);
        assert_eq!(
            policy.resolve(&Capability::Exec { scope: ExecScope::Arbitrary }),
            Level::Ask
        );
        assert_eq!(policy.resolve(&Capability::Net { scope: NetScope::Any }), Level::Ask);
        assert_eq!(policy.resolve(&Capability::Memory { mode: MemoryMode::Wipe }), Level::Ask);
        // 模型服务商本身不弹（每条消息都要发一次，弹窗会把它变成噪音）
        assert_eq!(
            policy.resolve(&Capability::Net { scope: NetScope::Provider }),
            Level::Allow,
            "每次对话都要做的出口不该问人"
        );
        // 读在任何档都不弹；项目内写入在 auto 下自动过（今天的 Elevated），在 ask 下要问
        assert_eq!(policy.resolve(&file(FileMode::Read, PathScope::ProjectRoot)), Level::Allow);
        assert_eq!(
            policy.resolve(&file(FileMode::Write, PathScope::ProjectRoot)),
            Level::Scoped,
            "auto 档的意义就是项目内写不再问"
        );
        assert_eq!(
            Policy::new(Mode::Ask).resolve(&file(FileMode::Write, PathScope::ProjectRoot)),
            Level::Ask
        );
    }

    #[test]
    fn full_mode_lifts_prompts_but_not_explicit_denies() {
        let policy = Policy {
            mode: Mode::Full,
            overrides: vec![("net.any".into(), Level::Deny)], phase: Phase::Chat, delete_batch_ask: 50, file_rules: Vec::new(), command_blocklist: Vec::new(), command_rules: Vec::new(), network_rules: Vec::new(), net_http_remote: crate::file_rules::RuleAction::Ask, net_http_local: crate::file_rules::RuleAction::Allow}
        ;
        assert_eq!(policy.resolve(&file(FileMode::Write, PathScope::Workspace)), Level::Allow);
        assert_eq!(
            policy.resolve(&Capability::Net { scope: NetScope::Any }),
            Level::Deny,
            "一个开关就能绕过用户自己划的红线，那红线不算存在"
        );
    }

    #[test]
    fn writing_outside_the_root_is_denied_not_asked() {
        let policy = Policy::new(Mode::Auto);
        let decision = policy.check(
            &file(FileMode::Write, PathScope::Any),
            "C:\\Windows\\system.ini",
            "deadbeefdeadbeef",
        );
        assert!(matches!(decision, Decision::Deny { .. }), "越界写入今天就是硬错，别退化成弹窗：{decision:?}");
    }

    #[test]
    fn asking_carries_the_fingerprint_being_confirmed() {
        let policy = Policy::new(Mode::Ask);
        let command = normalize_command("git  push   origin main");
        let decision = policy.check(
            &Capability::Exec { scope: ExecScope::Git },
            &command,
            &fingerprint(&["exec.git", &command]),
        );
        let Decision::Ask { fingerprint: held, .. } = decision else {
            panic!("ask 档下的 git 执行必须问人：{decision:?}");
        };
        // 规范化：换一种空格写法不该变成另一个动作
        assert_eq!(held, fingerprint(&["exec.git", "git push origin main"]));
        assert_eq!(normalize_command("  a \t b  "), "a b");
        // 而零件边界不能糊过去：这是长度前缀存在的唯一理由
        assert_ne!(fingerprint(&["ab", "c"]), fingerprint(&["a", "bc"]));
    }

    #[test]
    fn override_prefix_match_is_segment_bounded() {
        let policy = Policy {
            mode: Mode::Ask,
            overrides: vec![("tool.read".into(), Level::Deny)], phase: Phase::Chat, delete_batch_ask: 50, file_rules: Vec::new(), command_blocklist: Vec::new(), command_rules: Vec::new(), network_rules: Vec::new(), net_http_remote: crate::file_rules::RuleAction::Ask, net_http_local: crate::file_rules::RuleAction::Allow}
        ;
        assert_ne!(
            policy.resolve(&Capability::Tool { name: "read_file".into() }),
            Level::Deny,
            "`tool.read` 不该顺手命中 `tool.read_file`"
        );
        let exact = Policy {
            mode: Mode::Ask,
            overrides: vec![
                ("tool.read".into(), Level::Deny),
                ("tool.read_file".into(), Level::Ask),
            ], phase: Phase::Chat, delete_batch_ask: 50, file_rules: Vec::new(), command_blocklist: Vec::new(), command_rules: Vec::new(), network_rules: Vec::new(), net_http_remote: crate::file_rules::RuleAction::Ask, net_http_local: crate::file_rules::RuleAction::Allow}
        ;
        assert_eq!(
            exact.resolve(&Capability::Tool { name: "read_file".into() }),
            Level::Ask,
            "更长（更精确）的键要赢"
        );
    }

    #[test]
    fn a_multi_capability_action_is_judged_by_its_strictest_part() {
        // 一次 MCP 调用同时是"用某个工具"和"执行一个来路不明的程序"：
        // 按工具那条本来能自动过，按程序那条必须问——取严的那一条才是真的闸
        let policy = Policy::new(Mode::Auto);
        let tool = Capability::Tool { name: "mcp__notion__create_page".into() };
        let exec = Capability::Exec { scope: ExecScope::Arbitrary };
        assert_eq!(policy.resolve(&tool), Level::Scoped);
        let caps = vec![tool, exec.clone()];
        let picked = policy.strictest(&caps).expect("两条都在");
        assert_eq!(picked, &exec);
        assert_eq!(policy.strictest(&[]), None);
    }

    fn ov(key: &str, level: Level) -> PermissionOverride {
        PermissionOverride { key: key.to_string(), level }
    }

    /// 判据："全局划的红线，项目撤销不了"。两层同键时取更严的那一条，而不是"谁写在后面"——
    /// 后者是任何"配置 + 覆盖"拼接最容易踩的洞，而且它坏了是静默的
    #[test]
    fn a_project_cannot_undo_a_global_red_line() {
        let global = vec![ov("exec", Level::Deny)];
        let loosened = vec![ov("exec", Level::Ask)];
        let policy = effective(Mode::Auto, &global, &loosened, 50);
        assert_eq!(
            policy.resolve(&Capability::Exec { scope: ExecScope::Git }),
            Level::Deny,
            "项目那一条更松，它不该赢"
        );
        // 反向也要成立：项目收紧正是这条存在的理由
        let tighter = vec![ov("exec", Level::Deny)];
        assert_eq!(
            effective(Mode::Full, &[], &tighter, 50).resolve(&Capability::Exec { scope: ExecScope::Git }),
            Level::Deny
        );
    }

    /// 全局档仍然是天花板：项目那一层不能把它想放松的放松掉
    #[test]
    fn a_project_override_cannot_loosen_the_global_mode() {
        let policy = effective(Mode::Ask, &[], &[ov("file.write", Level::Allow)], 50);
        assert_eq!(
            policy.resolve(&Capability::File { scope: PathScope::ProjectRoot, mode: FileMode::Write }),
            Level::Ask,
            "勾选一个项目就绕过弹窗，等于没有闸门"
        );
    }

    /// 另一个项目读不到这个项目的收紧。漏了这一条，"按项目覆盖"就退化成第二条"整机一个口径"
    #[test]
    fn a_project_override_applies_only_to_that_project() {
        let locked = vec![ov("net.any", Level::Deny)];
        let strict = effective(Mode::Full, &[], &locked, 50).resolve(&Capability::Net { scope: NetScope::Any });
        let neighbour = effective(Mode::Full, &[], &[], 50).resolve(&Capability::Net { scope: NetScope::Any });
        assert_eq!(strict, Level::Deny);
        assert_eq!(neighbour, Level::Allow, "没写覆盖项的那个项目不该跟着变");
    }

    /// 键写错一个字母，那条覆盖永远不会命中，而界面上写着"我拦了 git"——所以存之前问这一句。
    /// 不止拼写：`exec.notion` 与 `memory.notion` 拼写没错，却也是死键——覆盖项配的是
    /// `capability.key()`，而任意命令那一档 collapse 成 `exec.arbitrary`、memory 只有四个动作。
    /// 收下一条永远不命中的键，等于让用户以为红线划下了
    #[test]
    fn an_unknown_override_key_is_refused_rather_than_ignored() {
        for good in [
            "file",
            "file.write.projectRoot",
            "exec",
            "exec.git",
            "exec.arbitrary",
            "tool.read_file",
            "memory.wipe",
            "net.any",
        ] {
            assert!(is_known_key(good), "认识的键被拒了：{good}");
        }
        for bad in [
            "",
            "exc",
            "file.wrote",
            "tool.",
            "net.anywhere.extra",
            "exec.notion",
            "memory.notion",
            "net.anything",
            "file.read.projectroot",
        ] {
            assert!(!is_known_key(bad), "不认识的键被收下了：{bad}");
        }
        // `file.read` 与 `exec` 这种"半截键"是**活的**：覆盖项按最长前缀命中，
        // 它管的是这一类动作的全部。别把它和 `exec.notion` 混为一谈（后者永远命不中，
        // 因为任意命令那一档的键是 `exec.arbitrary`）
        assert!(is_known_key("file.read"), "前缀行是该表的一等公民");
        // 死键的判据不是"我列不全"，而是"这条键翻不出它自己"：正对着看一眼
        let dead = capability_of("exec.notion").expect("`exec.notion` 翻得出一条 capability");
        assert_eq!(dead.key(), "exec.arbitrary", "任意命令的键会改掉，所以原名是死键");
        // 表上每一行都要能翻成一条 capability：列的与判的必须是同一批东西
        for key in TABLE {
            assert!(capability_of(key).is_some(), "表里的「{key}」翻不成一条 capability");
            assert!(is_known_key(key), "表里的「{key}」过不了自己的键校验");
        }
    }

    /// 一行表有没有人问它，是这三条测试在答的。判据集中在 [`memory_acts`] 一处，
    /// 界面上那几行才有一个共同的答案：`Deny` 永远挡得住，哪怕这一下是人在界面上点的
    #[test]
    fn a_deny_row_on_the_memory_side_holds_even_against_a_click() {
        for attended in [true, false] {
            assert!(!memory_acts(Level::Deny, attended), "红线不能因为'这一下有人点过'就放行");
        }
        // 反证：Deny 之外没有一档挡得住人在界面上点的那一下
        for level in [Level::Allow, Level::Scoped, Level::Ask] {
            assert!(memory_acts(level, true), "{level:?} 不该挡住有人点过的那一下");
        }
    }

    #[test]
    fn ask_binds_the_memory_producers_nobody_nods_at() {
        assert!(!memory_acts(Level::Ask, false), "自动注入与自动提取没有点头的人");
        assert!(memory_acts(Level::Allow, false), "Allow 才是'没人点也做'那一档");
    }

    /// 这一条是"不破坏现有功能"的钉子：三档全局档下，四条 memory 行的默认值全都还是动手，
    /// 连没人点头的那一路也不挡（`wipe` 的默认是 Ask，而清空那一下正是人点的）
    #[test]
    fn the_default_memory_rows_still_do_what_they_did() {
        for mode in [Mode::Ask, Mode::Auto, Mode::Full] {
            let policy = Policy::new(mode);
            for action in [MemoryMode::Read, MemoryMode::Write, MemoryMode::Export] {
                let level = policy.resolve(&Capability::Memory { mode: action });
                assert_eq!(level, Level::Allow, "{mode:?} 档下 memory 的默认不该是别的");
                assert!(memory_acts(level, false), "默认档不该拦住后台那一路");
            }
            let wipe = policy.resolve(&Capability::Memory { mode: MemoryMode::Wipe });
            assert!(memory_acts(wipe, true), "{mode:?} 档下有人点的清空会被挡住");
        }
    }

    /// 四个动作都要在表上：不在表上就没法收紧，在表上而没人 resolve 它就是装饰，
    /// 两头都得钉
    #[test]
    fn every_memory_action_has_a_row_to_tighten() {
        for action in
            [MemoryMode::Read, MemoryMode::Write, MemoryMode::Export, MemoryMode::Wipe]
        {
            let cap = Capability::Memory { mode: action };
            let key = cap.key();
            assert!(TABLE.contains(&key.as_str()), "{key} 不在表上，用户就没法改它");
            assert_eq!(capability_of(&key), Some(cap), "{key} 翻回来的不是同一条判定");
        }
    }

    /// 拦下来的那句要指得出是哪一行、去哪儿改；正文一个字都不进理由
    #[test]
    fn a_refusal_names_the_row_and_not_the_payload() {
        let cap = Capability::Memory { mode: MemoryMode::Wipe };
        let reason = memory_refused(&cap, Level::Deny);
        assert!(reason.contains("memory.wipe"), "要说清是哪一行拦的：{reason}");
        assert!(reason.contains("权限表"), "要指出去哪儿改：{reason}");
        let asked = memory_refused(&cap, Level::Ask);
        assert!(asked.contains("点头"), "Ask 与 Deny 是两件事，理由不能同一句：{asked}");
        assert_ne!(reason, asked);
    }

    /// 表要说清"这个数是哪一层给的"。只有一个数，用户就没法核对刚加的那一行生效了没有
    #[test]
    fn the_table_says_which_layer_gave_the_number() {
        let global = vec![ov("net.any", Level::Deny)];
        let project = vec![ov("exec", Level::Deny)];
        let policy = effective(Mode::Ask, &global, &project, 50);
        let rows = table(&policy, &global, &project);
        assert_eq!(rows.len(), TABLE.len(), "表要列全判定面，不是只列被改过的");
        let source = |want: &str| -> &'static str {
            rows.iter().find(|row| row.key == want).expect("表里要有这一行").source
        };
        assert_eq!(source("net.any"), "全局");
        assert_eq!(source("exec.git"), "项目");
        assert_eq!(source("file.read.any"), "档位", "没人改过的行要老实说它来自档位");
    }

    // ---- delete（design-security-center.md D1）----

    #[test]
    fn delete_keys_parse_and_delete_is_one_level_stricter_than_write() {
        assert_eq!(
            capability_of("file.delete.projectRoot"),
            Some(file(FileMode::Delete, PathScope::ProjectRoot))
        );
        assert_eq!(
            capability_of("file.delete.any"),
            Some(file(FileMode::Delete, PathScope::Any))
        );
        // 删除比写严一档：ask/auto 都要点头，只有 full 放行——
        // 回收站兜得住误删，兜不住"删错了还没发现"
        for (mode, expected) in [
            (Mode::Ask, Level::Ask),
            (Mode::Auto, Level::Ask),
            (Mode::Full, Level::Allow),
        ] {
            assert_eq!(
                Policy::new(mode).resolve(&file(FileMode::Delete, PathScope::ProjectRoot)),
                expected,
                "{mode:?} 档下项目内删除的档位"
            );
        }
        // 根外删除与根外写入同罪：硬错，任何档都不问
        assert_eq!(
            Policy::new(Mode::Full).resolve(&file(FileMode::Delete, PathScope::Any)),
            Level::Deny
        );
    }
}
