//! 反思（P2）：隔一段时间问一次"最近学到了什么，是长期记忆还没有的"。
//!
//! 它和提取的区别只写在 `Provenance` 那一个参数里：主体是 reflection、没有对话出处、
//! **产物不许自动转正**。元认知的输出天然是推断，让它直接成为一条无人质疑的独立事实，
//! 比漏掉一条有用得多——所以这一路只能往候选区放东西，转正由人点头。

use std::fs;
use std::path::Path;

use rusqlite::Connection;

use super::extract::{self, Accepted, Provenance};
use super::{daily_files, list_all, MemoryConfig, MemorySource, MemoryStatus, Paths};

/// 日志取最近这几篇，每篇截到这个长度。反思不是重读历史：它要的是"最近"那一点
const DAILY_LOGS: usize = 3;
const DAILY_CHARS: usize = 1200;
/// 已有记忆的清单长度。给得太短，模型就会把已经记着的东西再"反思"一遍
const KNOWN_RECORDS: usize = 40;
const KNOWN_CHARS: usize = 90;

/// 该不该花这一次服务商调用。命令层先问它——"关掉之后还在打电话"是最贵的一种没做干净
pub fn should_ask(config: &MemoryConfig) -> bool {
    config.enabled && config.reflect_enabled
}

/// 反思的原料：最近的每日日志 + 长期记忆里已经有的那一份清单。
///
/// 清单必须一起给。不告诉模型"这些已经记着了"，它每次反思都会把同一条偏好重新生产一遍
pub fn material_of(
    paths: &Paths,
    conn: &Connection,
    workspace: Option<&Path>,
) -> Result<String, String> {
    let mut out = String::new();
    out.push_str("最近的日志：\n");
    let mut logs: Vec<std::path::PathBuf> = Vec::new();
    for dir in daily_dirs_of(paths, workspace) {
        logs.extend(daily_files(&dir));
    }
    // 文件名字典序就是日期序（`YYYY-MM-DD.md`），倒过来取就是"最近优先"
    logs.sort();
    for file in logs.into_iter().rev().take(DAILY_LOGS) {
        let text = read(&file)?;
        let body: String = text.chars().take(DAILY_CHARS).collect();
        let day = file
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        out.push_str(&format!("【{day}】{body}\n"));
    }
    out.push_str("\n已经记着的（不要重复产出）：\n");
    let views = list_all(conn)?;
    let mut listed = 0usize;
    // 这份清单是**没人问也往外发**的那一类，所以它读的是比"能不能注入"更严的一档：
    // `private` 与 `secret` 都不进来。少了这一道，"标成不外发"就只挡住了注入那一路
    for view in views
        .iter()
        .filter(|view| view.record.status == MemoryStatus::Active)
        .filter(|view| view.record.sensitivity.sendable_as_material())
    {
        if listed >= KNOWN_RECORDS {
            break;
        }
        let line: String = view.record.content.chars().take(KNOWN_CHARS).collect();
        out.push_str(&format!("- {}\n", line.trim()));
        listed += 1;
    }
    if listed == 0 {
        out.push_str("（还没有任何长期记忆）\n");
    }
    Ok(out)
}

fn daily_dirs_of(paths: &Paths, workspace: Option<&Path>) -> Vec<std::path::PathBuf> {
    let mut dirs = vec![paths.global_daily()];
    if let Some(workspace) = workspace {
        dirs.push(Paths::workspace_daily(workspace));
    }
    dirs
}

/// 问的那一句。JSON 契约与提取同一份，所以解析复用 `extract::parse_candidates`
pub fn prompt_for(material: &str) -> String {
    format!(
        "下面是这台机器最近的日志，以及长期记忆里已经记着的东西。\n\
         只说一件事：从这些里面能推出什么**清单里还没有**、且对未来对话有用的结论。\n\
         只输出一个 JSON 数组，不要解释、不要代码块围栏。每个元素形如：\n\
         {{\"type\":\"{}\",\
         \"content\":\"一句话陈述，不要引用原文\",\
         \"scope\":\"global|project\",\"importance\":1-5,\"confidence\":0-1,\
         \"stability\":\"stable|volatile\",\"ttl_days\":null,\
         \"occurred_at\":null,\"tags\":[\"标签\"],\"entities\":[]}}\n\
         规则：\n\
         1. 清单里已经有的、或只是换个说法的，一律不要再产出。\n\
         2. 推不出来就返回 []——没有结论比一个编出来的结论值钱。\n\
         3. 密码、密钥、token、证件号、银行卡、手机号这类敏感信息一律不要提。\n\
         4. 用户明确说过\"不要记\"的内容不要提。\n\
         5. 不要把日志里的一次性事务（某个具体任务跑完了）当成长期结论。\n\n\
         材料：\n{material}",
        super::MemoryKind::contract()
    )
}

/// 服务商返回的东西要经过这几道才成为记录：来源只能是推断，且不许冒充别人的出处
pub fn land(
    conn: &Connection,
    paths: &Paths,
    workspace: Option<&Path>,
    config: &MemoryConfig,
    project_id: Option<&str>,
    raw: &str,
) -> Result<Accepted, String> {
    let mut records = extract::parse_candidates(raw);
    for record in &mut records {
        record.source = MemorySource::Inferred;
        // 反思没有对话可指。留空，而不是编一个 conversation_id 冒充"用户说过的某轮"
        record.origin = None;
    }
    let provenance = Provenance {
        actor: crate::audit::Actor::Reflection,
        origin: None,
        must_stay_candidate: true,
    };
    extract::accept(
        conn,
        paths,
        workspace,
        config,
        project_id,
        &records,
        &provenance,
    )
}

fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("读 {} 失败：{e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{
        append_record, ensure_layout, index, MemoryKind, MemoryRecord, MemoryScope, MemoryView,
        Stability,
    };
    use crate::test_support::{remove_tree, temp_dir};

    fn harness() -> (Paths, Connection) {
        let paths = Paths::new(temp_dir("reflect-root"));
        ensure_layout(&paths).unwrap();
        let conn = index::open(&paths.index_db()).unwrap();
        (paths, conn)
    }

    /// 一份"过得了门槛"的服务商返回。置信与重要性都刻意给满：这样下面那条断言才是在量来路，
    /// 而不是在量一个本来就拦得住它的阈值
    fn raw_about(content: &str) -> String {
        format!(
            "[{{\"type\":\"preference\",\"content\":\"{content}\",\"scope\":\"global\",\
             \"importance\":5,\"confidence\":0.95,\"stability\":\"stable\"}}]"
        )
    }

    fn stored<'a>(views: &'a [MemoryView], id: &str) -> &'a MemoryRecord {
        views
            .iter()
            .map(|view| &view.record)
            .find(|record| record.id == id)
            .expect("记录该在库里")
    }

    /// 用户自己说的一条，用来当"已经被记着的事实"
    fn user_safer(content: &str) -> MemoryRecord {
        let mut record = MemoryRecord::draft(MemoryScope::Global, content);
        record.kind = MemoryKind::Preference;
        record.source = MemorySource::User;
        record.confidence = 1.0;
        record.importance = 4;
        record.stability = Stability::Stable;
        record
    }

    #[test]
    fn a_reflection_stays_a_candidate_even_though_the_thresholds_would_promote_it() {
        let (paths, conn) = harness();
        let config = MemoryConfig::default();
        let report = land(
            &conn,
            &paths,
            None,
            &config,
            None,
            &raw_about("用户习惯在周五下午发布。"),
        )
        .unwrap();
        assert_eq!(report.stored.len(), 1);
        let views = list_all(&conn).unwrap();
        let reflected = stored(&views, &report.stored[0].record.id);
        assert_eq!(
            reflected.status,
            MemoryStatus::Candidate,
            "过得了门槛也不许自动转正"
        );
        assert_eq!(
            reflected.source,
            MemorySource::Inferred,
            "反思产物不许冒充用户说的"
        );
        assert!(
            reflected.origin.is_none(),
            "反思没有对话可指，不许编一个 conversation_id"
        );

        // 同一份形状换一条来路：提取那一路的门槛照常被跨过。这证明上面拦它的是来路，不是数字
        let promoted = extract::accept(
            &conn,
            &paths,
            None,
            &config,
            None,
            &extract::parse_candidates(&raw_about("用户习惯在周一上午开会。")),
            &Provenance {
                actor: crate::audit::Actor::Model,
                origin: None,
                must_stay_candidate: false,
            },
        )
        .unwrap();
        let views = list_all(&conn).unwrap();
        assert_eq!(
            stored(&views, &promoted.stored[0].record.id).status,
            MemoryStatus::Active,
            "对照臂没转正：那条 Candidate 断言就没在量它声称的东西"
        );

        remove_tree(&paths.root);
    }

    /// 这条是那个新字段的真正用处：`merge_into` 会换掉旧正文并把置信度抬上去，
    /// 让一次推断改写掉一条用户说过的话
    #[test]
    fn a_reflection_does_not_fold_itself_into_an_existing_fact() {
        let (paths, conn) = harness();
        let config = MemoryConfig::default();
        let seeded = user_safer("用户习惯在周五下午发布。");
        let seeded_id = seeded.id.clone();
        append_record(&conn, &paths, None, &seeded).unwrap();

        let report = land(
            &conn,
            &paths,
            None,
            &config,
            None,
            &raw_about("用户习惯在周五下午发布。"),
        )
        .unwrap();
        assert_eq!(report.merged, 0, "反思不许合并进那条用户说过的");
        let views = list_all(&conn).unwrap();
        assert_eq!(
            stored(&views, &seeded_id).content,
            "用户习惯在周五下午发布。",
            "原话一个字都不许动"
        );
        assert_eq!(stored(&views, &seeded_id).status, MemoryStatus::Active);
        assert_eq!(report.stored.len(), 1, "宁可多一条重复的候选让人裁决");
        assert_eq!(
            stored(&views, &report.stored[0].record.id).status,
            MemoryStatus::Candidate
        );

        // 对照：同样的两条东西走提取那一路，合并确实会发生——所以前面那个 0 不是"根本没撞上"
        let (other_paths, other_conn) = harness();
        append_record(
            &other_conn,
            &other_paths,
            None,
            &user_safer("用户习惯在周五下午发布。"),
        )
        .unwrap();
        let merged = extract::accept(
            &other_conn,
            &other_paths,
            None,
            &config,
            None,
            &extract::parse_candidates(&raw_about("用户习惯在周五下午发布。")),
            &Provenance {
                actor: crate::audit::Actor::Model,
                origin: None,
                must_stay_candidate: false,
            },
        )
        .unwrap();
        assert_eq!(merged.merged, 1, "对照臂没合并：这条测试什么都没否证");
        assert_eq!(list_all(&other_conn).unwrap().len(), 1);

        remove_tree(&paths.root);
        remove_tree(&other_paths.root);
    }

    #[test]
    fn reflection_is_asked_for_only_when_both_switches_are_up() {
        let config = MemoryConfig::default();
        assert!(!should_ask(&config), "默认关：一次服务商都不该发");

        let mut on = MemoryConfig {
            reflect_enabled: true,
            ..Default::default()
        };
        assert!(should_ask(&on));

        on.enabled = false;
        assert!(!should_ask(&on), "总开关关掉时，反思那一格开着也不该发");
    }

    #[test]
    fn material_shows_the_recent_logs_and_what_is_already_known() {
        let (paths, conn) = harness();
        let held = user_safer("部署脚本要幂等，跑两遍不许改结果。");
        append_record(&conn, &paths, None, &held).unwrap();

        let material = material_of(&paths, &conn, None).unwrap();
        assert!(
            material.contains("最近的日志"),
            "日志那一半要在：{material}"
        );
        assert!(
            material.contains("已经记着的"),
            "『已经记着』的清单要在：{material}"
        );
        assert!(
            material.contains("部署脚本要幂等"),
            "已记着的那条要列出来，否则模型会再生产一遍"
        );

        // 候选区的东西不算"已经记着"：它还没被谁点头。日志那一半里有它是正常的
        // （流水账记的就是"写下过什么"），所以要判的是清单那一段
        let mut pending = user_safer("这条还停在候选区。");
        pending.status = MemoryStatus::Candidate;
        append_record(&conn, &paths, None, &pending).unwrap();
        let material = material_of(&paths, &conn, None).unwrap();
        let known = material.split("已经记着的").nth(1).expect("清单那一段要在");
        assert!(
            !known.contains("这条还停在候选区"),
            "候选不该出现在『已经记着』的清单里：{known}"
        );
        remove_tree(&paths.root);

        let (bare_paths, bare_conn) = harness();
        let empty = material_of(&bare_paths, &bare_conn, None).unwrap();
        assert!(
            empty.contains("（还没有任何长期记忆）"),
            "空库要说清是空的，不是给一句假清单"
        );
        remove_tree(&bare_paths.root);
    }

    /// 提示词里那份 JSON 契约必须和解析器认的一致——否则模型照提示词产出的东西会被整条丢掉
    #[test]
    fn the_prompt_holds_the_same_json_contract_the_parser_reads() {
        let prompt = prompt_for("材料");
        assert!(
            prompt.contains("entities"),
            "反思的提示词要与提取同一份契约：{prompt}"
        );
        assert!(prompt.contains("只输出一个 JSON 数组"));
        assert!(prompt.contains("清单里已经有的"));
        assert!(prompt.contains("推不出来就返回 []"));
        let parsed = extract::parse_candidates(&raw_about("周五下午发布。"));
        assert_eq!(parsed.len(), 1, "提示词要求的形状解析器要读得出来");
    }
}
