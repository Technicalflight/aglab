//! 溯源：一条记忆是从哪次对话里长出来的。
//!
//! 只存标识，不存正文。`origin` 里多一句消息原文，就等于给敏感内容多开一个落盘的
//! 口子，而"这条是哪来的"这个问题根本不需要原文就能答完——与审计行同一套哲学。
//!
//! 它住在记录的 frontmatter 里，不是只住在索引里：索引删掉重建时要能原样问回来。

use serde::{Deserialize, Serialize};

/// 一次写入的出处。字段全都要有，因为"哪次对话"缺了就答不出问题了
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Origin {
    pub conversation_id: String,
    /// 促成这条记忆的那几条消息的 id。只有标识，正文一个字都不在这儿
    pub entries: Vec<String>,
    pub extracted_at: String,
}

impl Origin {
    /// frontmatter 里 `origin:` 那一行的写法：一行紧凑 JSON。
    /// 本格式不支持缩进，所以嵌套映射只能压平成一行
    pub fn encode(&self) -> String {
        // 三个字段全是字符串和字符串表，serde 在这里没有失败路径。真失败了就是把格式
        // 改坏了：写个空值会把出处静默抹掉，那比崩在这里更糟
        serde_json::to_string(self).expect("Origin 的字段都是字符串，编码不会失败")
    }

    /// 解一行 frontmatter 值。认不出来就报错，不能当"没有出处"——
    /// 静默降级会让一条来历不明的记忆看起来像用户自己写的
    pub fn decode(text: &str) -> Result<Self, String> {
        let trimmed = text.trim();
        if trimmed.is_empty() || trimmed == "null" {
            return Err("origin 这一行是空的。".into());
        }
        let origin: Origin = serde_json::from_str(trimmed)
            .map_err(|e| format!("出处读不了：{e}。它只该带标识，不带正文。"))?;
        origin.check()?;
        Ok(origin)
    }

    pub fn check(&self) -> Result<(), String> {
        if self.conversation_id.trim().is_empty() {
            return Err("origin 缺 conversation_id：一条说不出来自哪次对话的出处不算出处。".into());
        }
        if self.extracted_at.trim().is_empty() {
            return Err(format!("{} 的 origin 没写什么时候提取的。", self.conversation_id));
        }
        Ok(())
    }
}

/// `memory_source` 的返回：一条记忆的来历。
///
/// 这里**故意没有正文字段**——记录本来就在 `memory_list` 里，把正文再搬一份进
/// 来历视图，只是给敏感内容多开一条出口。`file` 用索引里那份相对记忆根目录的路径，
/// 所以它与真相源同源，不会各说一套
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceView {
    pub record_id: String,
    pub file: String,
    pub origin: Option<Origin>,
    /// 记录被写下的时间
    pub created_at: String,
    /// 事情发生的时间。没有就等于没人说过，不拿 created_at 冒充
    pub occurred_at: Option<String>,
    /// 最近一次真的被用上的时间
    pub reinforced_at: Option<String>,
    pub injections: i64,
    pub last_injected_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> Origin {
        Origin {
            conversation_id: "conv-42".into(),
            entries: vec!["entry-7".into(), "entry-8".into()],
            extracted_at: "2026-09-25T10:00:00+08:00".into(),
        }
    }

    #[test]
    fn origin_survives_one_frontmatter_line() {
        let line = origin().encode();
        assert!(!line.contains('\n'), "frontmatter 一行只能一行：{line}");
        assert_eq!(Origin::decode(&line).unwrap(), origin());
    }

    #[test]
    fn origin_refuses_to_carry_the_message_text() {
        // 出处只存标识。谁往 origin 里塞正文（哪怕是为了"看起来更方便"），
        // 读侧就该拒绝，而不是把它当成一条来历不明的记录放行
        let leaky = "{\"conversationId\":\"c1\",\"entries\":[],\"extractedAt\":\"2026-01-01T00:00:00+08:00\",\"content\":\"他的密码是 hunter2abcdefgh\"}";
        let error = Origin::decode(leaky).expect_err("多出来的字段必须报错");
        assert!(error.contains("不带正文"), "报错要说清为什么：{error}");
    }

    #[test]
    fn an_origin_without_a_conversation_is_not_an_origin() {
        // 漏传对话 id 是最可能的偷懒方式：静默写个空串，出处就变成一条永远
        // 答不出问题的字段。读侧必须报错，而不是把它当成一条没有出处的记录
        let mut blank = origin();
        blank.conversation_id = "   ".into();
        assert!(Origin::decode(&blank.encode()).is_err(), "漏了对话 id 不能静默通过");
        let mut undated = origin();
        undated.extracted_at = String::new();
        assert!(Origin::decode(&undated.encode()).is_err());
    }

    #[test]
    fn a_broken_origin_errors_instead_of_looking_unprovened() {
        // 读不懂就当"没有出处"，等于把一条来历不明的记录冒充成用户自己写的
        assert!(Origin::decode("不是 JSON").is_err());
        assert!(Origin::decode("").is_err());
        assert!(Origin::decode("null").is_err());
    }
}
