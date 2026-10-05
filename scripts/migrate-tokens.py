#!/usr/bin/env python3
"""
把散落的 Tailwind 任意值换成设计令牌刻度。

只做"值完全等价"的替换——12px -> text-sm 换成 12px，视觉零变化；
凡是会改变实际渲染值的（15/17/19/12.5px）一律不碰，留在报告里人工裁定。
派生字号（text-[length:calc(...)]）按前缀保护，绝不替换。
"""
import re
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "src"

# 精确等价的字号映射：key=原字面量, value=刻度类
FONT_EXACT = {
    "10": "text-2xs",
    "11": "text-xs",
    "12": "text-sm",
    "13": "text-base",
    "14": "text-md",
    "16": "text-lg",
    "18": "text-xl",
    "20": "text-2xl",
    "24": "text-3xl",
}
# 会改变渲染值的：只报告不改
FONT_REVIEW = {"15": "text-lg?", "17": "text-lg?", "19": "text-2xl?", "12.5": "text-sm?"}

RADIUS_EXACT = {"4": "rounded-xs", "6": "rounded-sm", "8": "rounded-md", "16": "rounded-xl"}

# 派生字号保护：text-[length:calc(...)] 与 text-[clamp(...)]
DERIVED = re.compile(r"text-\[(?!length:calc|clamp:)(?:(\d+(?:\.\d+)?)px)\]")

report = Counter()
review = Counter()
touched: list[tuple[str, int, int]] = []


def convert(path: Path) -> None:
    src = path.read_text(encoding="utf-8")
    original = src
    lines = src.split("\n")

    for idx, line in enumerate(lines, start=1):
        def sub_font(m: re.Match) -> str:
            px = m.group(1)
            if px in FONT_EXACT:
                report[f"text-[{px}px] -> {FONT_EXACT[px]}"] += 1
                return FONT_EXACT[px]
            if px in FONT_REVIEW:
                review[f"text-[{px}px] (待人工裁定)"] += 1
            return m.group(0)

        new_line = DERIVED.sub(sub_font, line)

        # 圆角
        def sub_radius(m: re.Match) -> str:
            px = m.group(1)
            if px in RADIUS_EXACT:
                report[f"rounded-[{px}px] -> {RADIUS_EXACT[px]}"] += 1
                return RADIUS_EXACT[px]
            review[f"rounded-[{px}px] (待人工裁定)"] += 1
            return m.group(0)

        new_line = re.sub(r"rounded-\[(\d+(?:\.\d+)?)px\]", sub_radius, new_line)

        if new_line != line:
            lines[idx - 1] = new_line
            touched.append((str(path.relative_to(ROOT)), idx, 0))

    out = "\n".join(lines)
    if out != original:
        path.write_text(out, encoding="utf-8")


for p in sorted(SRC.rglob("*.tsx")) + sorted(SRC.rglob("*.ts")):
    convert(p)

print("=" * 60)
print("已替换（视觉零变化）")
print("=" * 60)
for k, v in sorted(report.items(), key=lambda kv: -kv[1]):
    print(f"  {v:5d}  {k}")

print()
print("=" * 60)
print("待人工裁定（会改变渲染值，未动）")
print("=" * 60)
if review:
    for k, v in sorted(review.items(), key=lambda kv: -kv[1]):
        print(f"  {v:5d}  {k}")
else:
    print("  （无）")

print()
print(f"改动文件数: {len({t[0] for t in touched})}，改动行数: {len(touched)}")
print(f"替换总数: {sum(report.values())}")
