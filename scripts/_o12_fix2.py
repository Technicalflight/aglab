import io

# 1) wire/mod.rs：四个 reader 移回 lib 组
p = "src/chat/wire/mod.rs"
t = io.open(p, encoding="utf-8", newline="").read()
eol = "\r\n" if "\r\n" in t else "\n"
old = eol.join([
    "pub(crate) use read::{DeltaCoalescer, describe_status, EgressFail};",
    "pub(crate) use read::{ENHANCE_SYSTEM, is_pool_swappable_error, RETRYABLE_STATUS_WORD};",
])
new = eol.join([
    "pub(crate) use read::{",
    "    DeltaCoalescer, describe_status, EgressFail, read_anthropic_round, read_chat_round,",
    "    read_gemini_round, read_responses_round, ENHANCE_SYSTEM, is_pool_swappable_error,",
    "    RETRYABLE_STATUS_WORD,",
    "];",
])
assert t.count(old) == 1, f"lib group: {t.count(old)}"
t = t.replace(old, new)
# cfg(test) 组确认无 reader（本来就没加进去）
assert "affinity_headers, apply_anthropic_event" in t
io.open(p, "w", encoding="utf-8", newline="").write(t)
print("ok readers moved to lib group")

# 2) chat.rs：测试模块去重 with_timeouts（root 已有）
p = "src/chat.rs"
t = io.open(p, encoding="utf-8", newline="").read()
eol = "\r\n" if "\r\n" in t else "\n"
old3 = eol.join([
    "mod wire_format_tests {",
    "    use super::*;",
    "    use crate::net::with_timeouts;",
    "    use wire::{",
])
new3 = eol.join([
    "mod wire_format_tests {",
    "    use super::*;",
    "    use wire::{",
])
assert t.count(old3) == 1, f"chat.rs dedup: {t.count(old3)}"
t = t.replace(old3, new3)
io.open(p, "w", encoding="utf-8", newline="").write(t)
print("ok with_timeouts dedup")
