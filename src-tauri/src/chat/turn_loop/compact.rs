//! 自动压缩闸（O1-6 拆分时外置）：microcompact 先行、整段压缩收尾，
//! 缓存热的推迟判定也在这里。返回本轮是否真的压缩过——输出预算钳制
//! 据此决定信字符估算还是信上一发的真实上报。

use super::{AppConfig, ChatEvent, EventSink, Send};
use crate::chat::{compaction_boundary, transcript::summarize_history_in};
use crate::session::layers::{self, KEEP_RECENT_CHARS};
use crate::session::send::{calibrated_baseline, sizing_of, tokens_of_chars};

pub(in crate::chat) fn run_auto_compact(
    send: &mut Send,
    config: &AppConfig,
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
    calibration: Option<&crate::usage::Calibration>,
    conversation_id: &str,
    on_event: &dyn EventSink,
    continuing_goal: bool,
) -> Result<bool, String> {
    let mut compacted = false;
    if config.auto_compact && send.history().len() >= 4 {
        let real_usage =
            crate::usage::last_usage_tokens_for_in(config_dir, conversation_id).unwrap_or(None);
        let real_baseline = real_usage.as_ref().map(|(input, _, _)| *input).unwrap_or(0);
        let tail_chars = send
            .history()
            .last()
            .and_then(|message| message["content"].as_str())
            .map(|text| text.chars().count())
            .unwrap_or(0);
        // 什么时候压：不再是"总量过了窗口的九成"，而是预算表点名要历史这一层付账。
        // 窗口先减掉本来就要留给输出的那截——旧的 `* 0.9` 想说的就是这个数，
        // 而它明写在配置里（config.max_tokens），不该用一个写死的比例去猜
        let sizing = sizing_of(config, calibration);
        let uses =
            layers::uses(&send.opened.log, send.standing()).map_err(|error| error.to_string())?;
        let table = layers::budget(&uses, sizing);
        let estimate = if real_baseline > 0 {
            layers::Estimate {
                // 服务商报的是 token，这张表量的是字符：不换算就等于把 3 万 token 当成 3 万字符
                chars: calibrated_baseline(real_baseline, calibration) + tail_chars,
                kind: layers::EstimateKind::Calibrated,
                // O6-1：缓存命中与写入随真实上报走——压缩闸的成本判定不再两眼一抹黑
                cache_read_tokens: real_usage.as_ref().map(|(_, read, _)| *read).unwrap_or(0),
                cache_write_tokens: real_usage.as_ref().map(|(_, _, write)| *write).unwrap_or(0),
            }
        } else {
            layers::estimate(&uses)
        };
        let plan = layers::plan(estimate, &table, None);
        // microcompact 先于整段压缩（O5-1/O5-2）：阶梯点了「清旧工具结果」就走读侧
        // 变换——不花摘要请求、不动日志。省下 ≥256 token **且**清完装得进预算，
        // 这一发就用清过的 wire 发；两项有一项不满足，照旧走下面的整段压缩
        let mut microcompacted = false;
        if plan
            .ladder
            .contains(&layers::Concession::ClearStaleToolResults)
        {
            let (_cleared, saved_chars) =
                crate::session::context::clear_stale_tool_results(send.history());
            let saved_tokens = tokens_of_chars(saved_chars, calibration) as usize;
            let post_plan = layers::plan(
                layers::Estimate {
                    chars: estimate.chars.saturating_sub(saved_chars),
                    kind: estimate.kind,
                    cache_read_tokens: estimate.cache_read_tokens,
                    cache_write_tokens: estimate.cache_write_tokens,
                },
                &table,
                None,
            );
            let fits = !matches!(post_plan.reason, layers::BreakReason::OverBudget { .. });
            if saved_tokens >= crate::session::context::MICROCOMPACT_MIN_SAVED_TOKENS && fits {
                send.microcompact = true;
                send.refresh()?;
                microcompacted = true;
                // 贴附提示不进正文：清了几条、省了多少，跟"压缩推迟"同一档的读数
                on_event.send(ChatEvent::Retry {
                    text: format!(
                        "microcompact：清掉旧工具结果，这一发省下约 {saved_tokens} token。"
                    ),
                    reason: "上下文让步阶梯：先清旧工具结果（读侧变换），日志一行不动。".into(),
                });
            }
        }
        let owed = !microcompacted
            && (plan
                .ladder
                .contains(&layers::Concession::CompactHistory)
            // 预防线（SoL-Pi 的步骤边界思想）：目标/计划续跑的轮次是语义干净的
            // 步骤边界——历史层用到硬顶七成就在这里提前压，别等逼近上限时
            // 在任务中间压
            || (continuing_goal
                && table
                    .row(layers::Layer::History)
                    .is_some_and(|row| row.chars * 10 >= row.max * 7)));
        if owed {
            // 缓存重写成本项（SoL-Pi 的 cacheWriteReadRatio 精神）：压缩必然作废
            // 粘住成员身上的热前缀缓存，下一发是全价重写。缓存还热、没有更深的
            // 让步点名、且总量仍在窗口容量内（晚一发压不会 400）时，推迟一次——
            // 缓存冷了或更逼近上限时，这里的判定自然放行
            let cache_hot = crate::pool::cache_hot_for(conversation_id);
            let deeper = plan.ladder.iter().any(|step| {
                matches!(
                    step,
                    layers::Concession::DropMemorySection | layers::Concession::TrimSkills
                )
            });
            let window_chars = (config.context_tokens.saturating_sub(config.max_tokens)) as f64
                * crate::usage::budget_ratio(calibration);
            let slack = (estimate.chars as f64) < window_chars * 0.98;
            if cache_hot && !deeper && slack {
                // 只弹贴附提示不进正文：推迟的压缩不是本轮的失败
                on_event.send(ChatEvent::Retry {
                    text: "压缩推迟：当前成员的缓存还热，压一次等于整段重写。".into(),
                    reason: "上下文逼近预算上限，缓存转冷或更逼近上限时会自动压缩。".into(),
                });
            } else {
                on_event.send(ChatEvent::Compaction {
                    phase: "start".into(),
                    summary: None,
                    kept: None,
                });
                let history = send.history().to_vec();
                match summarize_history_in(config_dir, data_dir, config, &history) {
                    Ok(summary) => {
                        // 压缩写成一条条目，而不是就地改写一个数组：改写的版本下一轮就没了，
                        // 界面上的条数和模型看到的条数还会各说各话（旧设计里 `kept` 口径不一致
                        // 就是这么来的）。条目进日志之后，"压过了"这个事实本身也是历史的一部分
                        compacted = true;
                        let boundary = send.provenance().ok().and_then(|origin| {
                            compaction_boundary(&history, &origin, KEEP_RECENT_CHARS)
                        });
                        match boundary {
                            Some((first_kept_entry_id, kept)) => {
                                send.append(crate::session::entry::EntryPayload::Compaction {
                                    summary: summary.clone(),
                                    first_kept_entry_id,
                                    tokens_before: layers::thread_chars(send.standing(), &history),
                                    usage: None,
                                    system_message: send.section_snapshot(),
                                })?;
                                on_event.send(ChatEvent::Compaction {
                                    phase: "done".into(),
                                    summary: Some(summary),
                                    kept: Some(kept),
                                });

                                // 压缩后复验走同一张预算表。真实 baseline 是压缩前的旧值，
                                // 不能再拿它判定，否则会误判"仍超窗"而连环压缩
                                let post_uses = layers::uses(&send.opened.log, send.standing())
                                    .unwrap_or_default();
                                let post_plan = layers::plan(
                                    layers::estimate(&post_uses),
                                    &layers::budget(&post_uses, sizing),
                                    // 这里不报"压缩授权过的那次断开"：本轮要看的是还装不装得下
                                    None,
                                );
                                if matches!(
                                    post_plan.reason,
                                    layers::BreakReason::OverBudget { .. }
                                ) {
                                    on_event.send(ChatEvent::Notice {
                                    text: "压缩后上下文仍接近窗口上限，建议调大「上下文窗口」配置或减少保留长度。"
                                        .into(),
                                });
                                }
                            }
                            None => {
                                on_event.send(ChatEvent::Notice {
                                    text:
                                        "上下文接近窗口上限，但可压缩的对话太少，本轮按原样发送。"
                                            .into(),
                                });
                            }
                        }
                    }
                    Err(error) => {
                        // 压缩失败不拦路：降级按原样发送，爆窗口是服务商的事，摘要挂了不该把整轮拖死
                        eprintln!("上下文压缩失败，按原样发送：{error}");
                        on_event.send(ChatEvent::Notice {
                            text: format!("上下文压缩失败（{error}），本轮按原样发送。"),
                        });
                    }
                }
            }
        }
    }
    Ok(compacted)
}
