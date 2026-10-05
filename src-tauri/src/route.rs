//! 模型路由表（design-model-routing.md）：按模型名把「跟着设置走」的那一发
//! 改写到规则指定的服务商档案与模型名上。
//!
//! 生效档位在连接解析链上排第三：**点名**（子助理/编排/任务，`with_connection`）
//! **> 模型池**（`crate::pool`）**> 路由表（本模块）> 设置直连**。调用方只有两处，
//! 都是「池子没接管、也没点名」的分支（`chat_send` 与 `run_turn_into`）——
//! 点名是一发明确的指定，池子不该抢，路由表同理；池成员本身就是一对
//! （服务商 × 模型）的明确指定，再叠一层映射只会制造没人能解释的改道。

use crate::config::AppConfig;

/// 一条规则的模式名接不接得住这个模型名。
/// 精确相等，或 `*` 结尾的前缀匹配（`gpt-4o*` 接住 `gpt-4o` 与它的变体）；
/// 单独一个 `*` 前缀为空，接住一切——兜底规则的写法。中段的 `*` 不展开，
/// 按字面匹配：模型名里没有它，正则引擎是 search_text 的工具，不是这里的。
/// 模型名大小写敏感，逐字节比较——`GPT-4o` 与 `gpt-4o` 在服务商那里就是两个名字
pub fn matches(pattern: &str, model: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        // 空模式是坏行（界面写不出，手改 config.json 才会出现），谁的键都不接
        return false;
    }
    match pattern.strip_suffix('*') {
        Some(prefix) => model.starts_with(prefix),
        None => model == pattern,
    }
}

/// 查表并改写。第一条命中的启用规则生效；返回是否真的改写了什么。
/// 指向已删档案的规则**按不命中处理**，滑到下一条或直连：路由是常驻配置，
/// 档案删了不该让所有聊天报错——破绽由设置页的「已删除的档案」徽章露出
pub fn apply(config: &mut AppConfig) -> bool {
    let requested = config.model.clone();
    // 先整体抄一份：命中分支要改 config，挂着规则的不可变借用改不动
    let rules = config.model_routes.clone();
    for rule in &rules {
        if !rule.enabled || !matches(&rule.pattern, &requested) {
            continue;
        }
        // 先服务商后模型（与 `with_connection` 同序，模型名是更具体的那一档）。
        // 档案抄写会把档案自带的默认模型一并带进来（13 个连接域一个不落），
        // 但规则的语义是「空 = 不改名」：没写名字就把请求原名的还给请求
        let profile_id = rule.endpoint_profile_id.trim();
        if !profile_id.is_empty() {
            let profile = match config.profiles.iter().find(|p| p.id == profile_id) {
                Some(profile) => profile.clone(),
                None => continue,
            };
            crate::config::apply_profile_connection(config, &profile);
        }
        let name = rule.model.trim();
        if name.is_empty() {
            config.model = requested.clone();
        } else {
            config.model = name.to_string();
        }
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EndpointProfile, ModelRoute};

    fn profile(id: &str, base_url: &str, model: &str) -> EndpointProfile {
        EndpointProfile {
            id: id.to_string(),
            name: id.to_string(),
            base_url: base_url.to_string(),
            model: model.to_string(),
            ..EndpointProfile::default()
        }
    }

    fn route(pattern: &str, endpoint: &str, model: &str) -> ModelRoute {
        ModelRoute {
            id: format!("route-{pattern}"),
            pattern: pattern.to_string(),
            endpoint_profile_id: endpoint.to_string(),
            model: model.to_string(),
            enabled: true,
        }
    }

    #[test]
    fn exact_prefix_and_catchall_patterns_match() {
        assert!(matches("gpt-4o", "gpt-4o"));
        assert!(!matches("gpt-4o", "gpt-4o-2024"), "精确匹配不多接一个字");
        assert!(matches("gpt-4o*", "gpt-4o"));
        assert!(matches("gpt-4o*", "gpt-4o-2024-08-06"));
        assert!(!matches("gpt-4o*", "gpt-4"), "前缀是前缀，不是相近就算");
        assert!(matches("*", "anything-at-all"), "单独的星号接住一切");
        assert!(!matches("   ", "gpt-4o"), "空模式是坏行，不接");
        assert!(!matches("gpt*4o", "gpt-4o"), "中段的星号按字面，不展开");
        assert!(!matches("GPT-4o", "gpt-4o"), "模型名大小写敏感");
    }

    #[test]
    fn first_matching_enabled_rule_wins() {
        let mut config = AppConfig::default();
        config.model = "gpt-4o".into();
        config.profiles.push(profile("p-a", "https://a.test/v1", "fallback-a"));
        config.model_routes.push(route("gpt-4o*", "p-a", "cheap-a"));
        let mut disabled = route("gpt-4o", "p-a", "wrong");
        disabled.enabled = false;
        config.model_routes.push(disabled);
        config.model_routes.push(route("*", "p-a", "catch-all"));

        assert!(apply(&mut config));
        assert_eq!(config.model, "cheap-a", "先写的规则先命中，停用与兜底轮不到");
    }

    #[test]
    fn a_rule_may_change_endpoint_or_name_or_both() {
        // 只改名：连接域原地不动
        let mut config = AppConfig::default();
        config.model = "claude-sonnet".into();
        config.base_url = "https://keep.test/v1".into();
        config.model_routes.push(route("claude-*", "", "glm-4.7"));
        assert!(apply(&mut config));
        assert_eq!(config.model, "glm-4.7");
        assert_eq!(config.base_url, "https://keep.test/v1", "没写服务商就不动连接域");

        // 只换服务商：模型名保持请求原名，档案自带的默认模型不趁乱塞进来
        let mut config = AppConfig::default();
        config.model = "claude-sonnet".into();
        config.profiles.push(profile("p-b", "https://b.test/v1", "b-default-model"));
        config.model_routes.push(route("claude-*", "p-b", ""));
        assert!(apply(&mut config));
        assert_eq!(config.base_url, "https://b.test/v1");
        assert_eq!(config.model, "claude-sonnet", "空模型名 = 不改名");
    }

    #[test]
    fn a_route_pointing_at_a_missing_profile_is_skipped_not_fatal() {
        let mut config = AppConfig::default();
        config.model = "gpt-4o".into();
        config.profiles.push(profile("p-real", "https://real.test/v1", "x"));
        config.model_routes.push(route("gpt-4o", "p-gone", "never"));
        config.model_routes.push(route("gpt-4o", "p-real", "survivor"));

        assert!(apply(&mut config));
        assert_eq!(config.model, "survivor", "死规则让位，下一条照常接住");
        assert_eq!(config.base_url, "https://real.test/v1");
    }

    #[test]
    fn no_match_or_empty_table_leaves_the_connection_alone() {
        let mut config = AppConfig::default();
        config.model = "gpt-4o".into();
        config.base_url = "https://plain.test/v1".into();
        config.model_routes.push(route("claude-*", "", "glm-4.7"));
        assert!(!apply(&mut config), "没命中就没动作");
        assert_eq!(config.model, "gpt-4o");
        assert_eq!(config.base_url, "https://plain.test/v1");

        config.model_routes.clear();
        assert!(!apply(&mut config), "空表 = 不路由");
        assert_eq!(config.model, "gpt-4o");
    }
}
