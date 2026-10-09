"""clippy JSON 批量修复器：rustfix 在 Windows 上 panic，自己按 JSON 建议落盘。

只应用 MachineApplicable 的建议；按字节区间操作（源码含中文，避免解码问题），
按文件分组后从后往前替换，重叠的建议跳过（宁可留给人修，也不赌一把）。
"""
import json
import sys
from collections import defaultdict

ROOT = r"C:\Users\s1mple\Desktop\aglab\src-tauri"
LOG = r"C:\Users\s1mple\Desktop\aglab\clippy-o31.json"


def collect():
    fixes = defaultdict(list)  # file -> [(start, end, replacement, lint)]
    stats = defaultdict(int)
    with open(LOG, encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                diag = json.loads(line)
            except json.JSONDecodeError:
                continue
            reason = diag.get("reason")
            if reason not in ("compiler-message", "diagnostic"):
                continue
            msg = diag.get("message") or diag
            if not isinstance(msg, dict) or "spans" not in msg:
                continue
            code = (msg.get("code") or {}).get("code", "?")

            def walk(d):
                for span in d.get("spans", []):
                    repl = span.get("suggested_replacement")
                    appl = span.get("suggestion_applicability")
                    if repl is None or appl != "MachineApplicable":
                        continue
                    if span.get("is_primary") is False and span.get("byte_start") is None:
                        continue
                    if span.get("byte_start") is None or span.get("byte_end") is None:
                        continue
                    fname = span.get("file_name", "")
                    if not fname.endswith(".rs"):
                        continue
                    fixes[fname].append(
                        (span["byte_start"], span["byte_end"], repl, code)
                    )
                for child in d.get("children", []):
                    walk(child)

            walk(msg)
            stats[code] += 1
    return fixes, stats


def apply(fixes):
    applied = skipped = 0
    for fname, spans in fixes.items():
        spans.sort(key=lambda s: (s[0], s[1]))
        # 去重 + 去重叠
        chosen = []
        last_end = -1
        for start, end, repl, code in spans:
            if start < last_end or end <= start:
                skipped += 1
                continue
            if chosen and chosen[-1][:2] == (start, end):
                continue
            chosen.append((start, end, repl, code))
            last_end = end
        path = f"{ROOT}\\{fname.replace('/', chr(92))}"
        with open(path, "rb") as f:
            data = f.read()
        for start, end, repl, _code in sorted(chosen, key=lambda s: -s[0]):
            data = data[:start] + repl.encode("utf-8") + data[end:]
        applied += len(chosen)
        with open(path, "wb") as f:
            f.write(data)
    return applied, skipped


fixes, stats = collect()
print("diagnostics by lint (top):")
for code, n in sorted(stats.items(), key=lambda kv: -kv[1])[:12]:
    print(f"  {n:4} {code}")
applied, skipped = apply(fixes)
print(f"applied: {applied} | skipped(overlap/empty): {skipped}")
