//! 资料库的关键词检索：切词、打分、摘要。
//!
//! 为什么不学 memory 那样上 SQLite FTS5：memory 按条注入、有实体图、要"哪几条记录"
//! 这种集合语义；资料库只回答"哪些文档的哪些段落跟这次提问相关"。桌面量级
//! （千级文档、MB 级正文）直接扫描是毫秒级的事，却少一个"索引与真相源同步"
//! 的失败模式。等语义向量检索进来时，把 `score_document` 换成向量相似度即可，
//! 调用方的形状（查询 → 分数 + 摘要）不变。
//!
//! 大小写折叠只动 ASCII（逐字符 1:1 映射）：`str::to_lowercase` 对个别字符会
//! 变长（İ → i̇），字节位一漂移，"命中位置"就不再是原文里的位置了。

/// 一段文字里的一个词元：ASCII 连续字母数字（已折叠小写）或 CJK 汉字二元组。
pub fn tokenize(query: &str) -> Vec<String> {
    fn is_han(c: char) -> bool {
        matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
    }

    let folded = fold(query);
    let chars: Vec<char> = folded.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_alphanumeric() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_alphanumeric() {
                i += 1;
            }
            out.push(chars[start..i].iter().collect());
        } else if is_han(c) {
            let start = i;
            while i < chars.len() && is_han(chars[i]) {
                i += 1;
            }
            let segment = &chars[start..i];
            if segment.len() == 1 {
                // 单字成段没有邻居可拼，保留单字：查「鱼」就该命中「鱼」
                out.push(segment.iter().collect());
            } else {
                for window in segment.windows(2) {
                    out.push(window.iter().collect());
                }
            }
        } else {
            i += 1;
        }
    }
    // 同一个词元在 query 里说两遍不算两份权重
    out.sort();
    out.dedup();
    out
}

/// ASCII 逐字符折叠。**必须保持字符数不变**——摘要的字符坐标全靠这一条
fn fold(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .collect()
}

/// 词元在已折叠文本里出现的次数
fn count_in(hay: &str, token: &str) -> usize {
    if token.is_empty() {
        return 0;
    }
    hay.match_indices(token).count()
}

/// 一篇文档的相关度。标题命中按 4 倍记（标题是文档自己声明的主题），
/// 再乘覆盖加成（1 + 命中词数/总词数）——命中的词越多越靠前。
/// 返回 `None` = 一个词元都没命中。第二个返回值是正文里第一处命中的字符位（摘要坐标）。
pub fn score_document(
    title: &str,
    content: &str,
    tokens: &[String],
) -> Option<(f64, Option<usize>)> {
    let folded_title = fold(title);
    let folded_content = fold(content);

    let mut total = 0.0f64;
    let mut matched = 0usize;
    for token in tokens {
        let in_title = count_in(&folded_title, token);
        let in_body = count_in(&folded_content, token);
        if in_title + in_body > 0 {
            matched += 1;
        }
        total += 4.0 * in_title as f64 + in_body as f64;
    }
    if total <= 0.0 {
        return None;
    }
    let coverage = matched as f64 / tokens.len().max(1) as f64;
    let first = tokens
        .iter()
        .filter_map(|token| folded_content.find(token.as_str()))
        .min()
        .map(|byte_at| folded_content[..byte_at].chars().count());
    Some((total * (1.0 + coverage), first))
}

/// 命中处为心的摘要窗口。换行折叠成空格——它要进的是一行检索结果，不是排版
pub fn snippet(content: &str, center: Option<usize>, width: usize) -> String {
    let chars: Vec<char> = content
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    if chars.is_empty() {
        return String::new();
    }
    let total = chars.len();
    let center = center.unwrap_or(0).min(total.saturating_sub(1));
    let half = width / 2;
    let start = center.saturating_sub(half);
    let end = (start + width).min(total);
    let mut text: String = chars[start..end].iter().collect();
    if start > 0 {
        text.insert(0, '…');
    }
    if end < total {
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizing_splits_words_and_han_bigrams() {
        let tokens = tokenize("Rust 所有权 Tokenize");
        assert!(tokens.contains(&"rust".to_string()), "{tokens:?}");
        assert!(tokens.contains(&"tokenize".to_string()), "{tokens:?}");
        assert!(tokens.contains(&"所有".to_string()), "{tokens:?}");
        assert!(tokens.contains(&"有权".to_string()), "{tokens:?}");
        // 标点与大写不进词元：折叠后 "Rust" 就是 "rust" 一份
        assert_eq!(tokens.iter().filter(|t| t.as_str() == "rust").count(), 1);

        // 单字成段保留单字
        assert!(tokenize("鱼").contains(&"鱼".to_string()));
        // 纯标点没有词元
        assert!(tokenize("，。！").is_empty());
    }

    #[test]
    fn title_hits_outweigh_body_hits() {
        let tokens = tokenize("预算");
        let titled = score_document("预算规则", "这里不谈预算以外的事", &tokens)
            .unwrap()
            .0;
        let body_only = score_document("别的题目", "预算只在正文里出现一次", &tokens)
            .unwrap()
            .0;
        assert!(
            titled > body_only,
            "标题命中该更贵：{titled} vs {body_only}"
        );
    }

    #[test]
    fn coverage_breaks_ties_toward_more_matched_tokens() {
        let tokens = tokenize("预算 审核");
        let both = score_document("", "预算和审核都出现", &tokens).unwrap().0;
        let one = score_document("", "预算出现但另一个词没有", &tokens)
            .unwrap()
            .0;
        assert!(both > one, "两词齐中该赢：{both} vs {one}");
    }

    #[test]
    fn no_hit_is_none() {
        assert!(score_document("标题", "正文", &tokenize("完全无关")).is_none());
    }

    #[test]
    fn match_position_survives_case_folding() {
        let tokens = tokenize("Hello");
        let (_, at) =
            score_document("t", "前面垫十个小写词 then Hello appears", &tokens).expect("该有命中");
        let at = at.expect("该有命中位置");
        let chars: Vec<char> = "前面垫十个小写词 then Hello appears".chars().collect();
        // 命中中心落在原文的 "Hello" 那一段
        assert_eq!(chars[at..at + 5].iter().collect::<String>(), "Hello");
    }

    #[test]
    fn snippet_centers_on_the_hit() {
        let filler = "字".repeat(200);
        let text = format!("{filler}needle{}", "字".repeat(200));
        let (_, at) = score_document("", &text, &tokenize("needle")).expect("命中");
        let view = snippet(&text, at, 40);
        assert!(view.contains("needle"), "{view}");
        assert!(
            view.starts_with('…') && view.ends_with('…'),
            "两端都在省略号外：{view}"
        );
        assert!(view.chars().count() <= 42, "窗口宽度含省略号封顶：{view}");
    }
}
