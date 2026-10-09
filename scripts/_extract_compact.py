import io

P = "src/chat/turn_loop.rs"

t = io.open(P, encoding="utf-8", newline="").read()
eol = "\r\n" if "\r\n" in t else "\n"
lines = t.split(eol)


def brace_close(i):
    depth = 0
    started = False
    for k in range(i, len(lines)):
        depth += lines[k].count("{") - lines[k].count("}")
        if depth > 0:
            started = True
        if started and depth == 0:
            return k
    raise SystemExit("unbalanced")


c_start = next(i for i, l in enumerate(lines) if "let mut compacted = false;" in l)
c_close = brace_close(c_start + 1)
cut_a = lines[c_start : c_close + 1]
joined = "".join(cut_a)
assert "auto_compact" in joined and "summarize_history_in" in joined
print(f"cut A: {c_start+1}..{c_close+1} ({len(cut_a)})")

sig_a = [
    "pub(in crate::chat) fn run_auto_compact(",
    "    send: &mut Send,",
    "    config: &AppConfig,",
    "    config_dir: &std::path::Path,",
    "    data_dir: &std::path::Path,",
    "    calibration: Option<&crate::usage::Calibration>,",
    "    conversation_id: &str,",
    "    on_event: &dyn EventSink,",
    "    continuing_goal: bool,",
    ") -> bool {",
]
header_a = [
    "//! 自动压缩闸（O1-6 拆分时外置）：microcompact 先行、整段压缩收尾，",
    "//! 缓存热的推迟判定也在这里。返回本轮是否真的压缩过——输出预算钳制",
    "//! 据此决定信字符估算还是信上一发的真实上报。",
    "",
    "use crate::chat::transcript::summarize_history_in;",
    "use crate::session::layers;",
    "use crate::usage;",
    "use super::{AppConfig, EventSink, Send};",
    "",
]
body_a = []
for l in cut_a:
    l = l.replace("crate::session::layers::", "layers::")
    body_a.append(l)
body_a.append("    compacted")

io.open(
    "src/chat/turn_loop/compact.rs",
    "w",
    encoding="utf-8",
    newline="",
).write(eol.join(header_a + sig_a + body_a) + eol)

lines[c_start : c_close + 1] = [
    "    let compacted = compact::run_auto_compact(",
    "        send,",
    "        config,",
    "        &config_dir,",
    "        &data_dir,",
    "        calibration.as_ref(),",
    "        conversation_id,",
    "        on_event,",
    "        continuing_goal,",
    "    );",
]
io.open(P, "w", encoding="utf-8", newline="").write(eol.join(lines))
print(f"turn_loop lines: {len(lines)}")
