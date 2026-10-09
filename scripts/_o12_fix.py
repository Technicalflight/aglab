import io
import re

# 1) wire/mod.rs：合并两组再导出（去掉旧组里的重复项）
p = "src/chat/wire/mod.rs"
t = io.open(p, encoding="utf-8").read()
old_group1 = """pub(in crate::chat) use read::{
    apply_anthropic_event, apply_chat_event, apply_gemini_event, apply_responses_event,
    partial_of, read_anthropic_round, read_chat_round, read_gemini_round, read_responses_round,
    responses_partial,
};"""
if old_group1 in t:
    t = t.replace(old_group1 + "\n", "")
old_group2 = """pub(in crate::chat) use read::is_pool_swappable_error;"""
t = t.replace(
    old_group2,
    """pub(in crate::chat) use read::is_pool_swappable_error;
pub(in crate::chat) use read::{
    affinity_headers, apply_anthropic_event, apply_chat_event, apply_gemini_event,
    apply_responses_event, partial_of, read_anthropic_round, read_chat_round, read_gemini_round,
    read_responses_round, responses_partial, retry_429_delay, ChatState, ResponsesState,
    StreamItem,
};""",
)
io.open(p, "w", encoding="utf-8", newline="").write(t)
print("ok mod.rs consolidated")

# 2) read.rs：状态机结构体的字段放开（ChatState / ResponsesState / StreamItem）
p = "src/chat/wire/read.rs"
lines = io.open(p, encoding="utf-8").read().split("\n")
targets = {"ChatState", "ResponsesState", "StreamItem", "DeltaCoalescer"}
out = []
in_struct = None
depth = 0
for i, line in enumerate(lines):
    m = re.match(r"pub\(in crate::chat\) (struct (\w+) \{)", line)
    if m and m.group(2) in targets:
        in_struct = m.group(2)
        depth = 1
        out.append(line)
        continue
    if in_struct:
        depth += line.count("{") - line.count("}")
        fm = re.match(r"^    ([a-z_][a-z_0-9]*\s*:)", line)
        if fm and depth > 0 and not line.strip().startswith("//"):
            line = "    pub(in crate::chat) " + line[4:]
        if depth <= 0:
            in_struct = None
    out.append(line)
io.open(p, "w", encoding="utf-8", newline="").write("\n".join(out))
print("ok read.rs fields opened")
