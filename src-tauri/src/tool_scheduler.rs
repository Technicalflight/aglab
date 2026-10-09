//! 一轮多调用的拓扑调度：**相邻的安全读并排跑，动东西的一律独占栅栏**。
//!
//! 模型在一发回复里常常连发七八个互不相干的读（搜三处、读五个文件），串行执行
//! 是纯等待；而写文件与提问的顺序就是语义，绝不能并排。拓扑规则因此极简且可测：
//! 按声明顺序扫一遍，`read_only && concurrent_safe` 的连续段收进一个并行批
//! （批内上限 [`MAX_CONCURRENCY`]，超了拆成多批——批与批之间仍是顺序边界），
//! 其余每个调用各占一个串行槽。批内并行由调用方开线程，这里只给拓扑。

/// 并行批的上限。十个只读调用并排已经把 IO 等待填满，再多只是线程调度噪声
pub const MAX_CONCURRENCY: usize = 10;

/// 一个调度槽。`index` 是调用在原始顺序里的下标——结果必须按这个顺序回填，
/// 乱序的 tool 结果会让模型把第 3 个调用的输出当第 2 个的
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    /// 槽内下标并行执行（保证 ≤ [`MAX_CONCURRENCY`]）
    Parallel(Vec<usize>),
    /// 独占执行（写/提问/命令这类，或并行批里落单的一个）
    Serial(usize),
}

/// 一轮调用的拓扑计划。`contracts[i]` 对应第 i 个调用。
/// 全串行的轮（全是写/提问）得到的计划与"逐个跑"等价，调用方无感
pub fn plan_round(contracts: &[crate::tool_contract::Contract]) -> Vec<Slot> {
    let mut slots: Vec<Slot> = Vec::new();
    let mut batch: Vec<usize> = Vec::new();

    for (index, contract) in contracts.iter().enumerate() {
        let parallelable = contract.read_only && contract.concurrent_safe;
        if parallelable {
            batch.push(index);
            if batch.len() == MAX_CONCURRENCY {
                slots.push(Slot::Parallel(std::mem::take(&mut batch)));
            }
            continue;
        }
        // 栅栏：先把攒下的安全读结算成一个并行批（落单的退化成串行槽），
        // 再给这个动东西的调用一个独占槽
        match batch.len() {
            0 => {}
            1 => {
                let only = batch.remove(0);
                slots.push(Slot::Serial(only));
            }
            _ => slots.push(Slot::Parallel(std::mem::take(&mut batch))),
        }
        slots.push(Slot::Serial(index));
    }

    match batch.len() {
        0 => {}
        1 => {
            let only = batch.remove(0);
            slots.push(Slot::Serial(only));
        }
        _ => slots.push(Slot::Parallel(batch)),
    }
    slots
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_contract::contract_for;

    fn contracts_of(names: &[&str]) -> Vec<crate::tool_contract::Contract> {
        names
            .iter()
            .map(|name| contract_for(name, &serde_json::json!({})))
            .collect()
    }

    #[test]
    fn adjacent_reads_batch_into_one_parallel_slot() {
        let slots = plan_round(&contracts_of(&[
            "read_file",
            "search_text",
            "web_search",
            "knowledge_search",
            "lsp_query",
            "obs_recall",
        ]));
        assert_eq!(slots.len(), 1, "{slots:?}");
        match &slots[0] {
            Slot::Parallel(indexes) => {
                assert_eq!(indexes, &[0, 1, 2, 3, 4, 5], "顺序保留：结果回填按原下标");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_write_is_a_barrier_that_splits_batches() {
        let slots = plan_round(&contracts_of(&[
            "read_file",
            "web_search",
            "write_file",
            "read_file",
            "knowledge_search",
        ]));
        assert_eq!(slots.len(), 3, "{slots:?}");
        assert_eq!(slots[0], Slot::Parallel(vec![0, 1]));
        assert_eq!(slots[1], Slot::Serial(2), "写独占一个槽");
        assert_eq!(slots[2], Slot::Parallel(vec![3, 4]));
    }

    #[test]
    fn questions_and_controls_are_serial_even_between_reads() {
        let slots = plan_round(&contracts_of(&["read_file", "ask_user", "read_file"]));
        assert_eq!(
            slots,
            vec![Slot::Serial(0), Slot::Serial(1), Slot::Serial(2)],
            "提问要等人，前后都不并"
        );
    }

    #[test]
    fn batch_is_capped_at_max_concurrency() {
        let names: Vec<&str> = (0..13).map(|_| "read_file").collect();
        let slots = plan_round(&contracts_of(&names));
        assert_eq!(slots.len(), 2, "{slots:?}");
        match (&slots[0], &slots[1]) {
            (Slot::Parallel(a), Slot::Parallel(b)) => {
                assert_eq!(a.len(), MAX_CONCURRENCY);
                assert_eq!(b.len(), 3);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn all_writes_degenerate_to_plain_serial_order() {
        let slots = plan_round(&contracts_of(&["write_file", "edit_file", "delete_file"]));
        assert_eq!(
            slots,
            vec![Slot::Serial(0), Slot::Serial(1), Slot::Serial(2)]
        );
    }

    #[test]
    fn a_lone_read_is_a_serial_slot_not_a_one_member_batch() {
        let slots = plan_round(&contracts_of(&["read_file"]));
        assert_eq!(slots, vec![Slot::Serial(0)]);
    }
}
