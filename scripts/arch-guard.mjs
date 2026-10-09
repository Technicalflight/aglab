#!/usr/bin/env node
/**
 * 架构守卫（优化路线 O3-3）。三道钉：
 *
 * 1. chat.rs 行数棘轮——预算存在 scripts/arch-budget.json，max 是冻结值，
 *    增长即红；target（≤2500）是 O1 拆分完成后的终态，守卫只提示距离不拦人。
 *    为什么用棘轮而不是一步到位 2500：拆分在 O1，红线先卡死会把 P0 期间
 *    每一个提交都变红——红着的门禁拦不住人，只会教会大家无视它。
 * 2. ToolSpec 唯一表——工具名册只准有一份（tools.rs 的 REGISTRY）。
 *    第二份名册 = 工具清单各说各话，声明、审批、审计全部失真。
 * 3. capability 唯一收口——模型能力识别前后端各一个收口函数
 *    （Rust provider::capability::resolve / TS resolveCapabilities），
 *    收口之外再长出同义判定就是双份真相。
 *
 * 用法：node scripts/arch-guard.mjs（CI 的 desktop job 里跑，本地随手可跑）
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const read = (rel) => readFileSync(path.join(root, rel), "utf8");
const countMatches = (rel, re) => (read(rel).match(re) ?? []).length;

const failures = [];
const notes = [];

// --- 1. chat.rs 行数棘轮 ---
const budget = JSON.parse(read("scripts/arch-budget.json"));
const chatBudget = budget["src-tauri/src/chat.rs"];
if (!chatBudget) {
  failures.push("arch-budget.json 里没有 src-tauri/src/chat.rs 这一条——预算文件与守卫脚本必须成对");
} else {
  // 与 wc -l 同一口径（数换行符）：split 出来的空尾元素不算一行
  const raw = read("src-tauri/src/chat.rs").split("\n");
  if (raw[raw.length - 1] === "") raw.pop();
  const lines = raw.length;
  if (lines > chatBudget.max) {
    failures.push(
      `chat.rs 行数 ${lines} 超过预算 ${chatBudget.max}。预算是棘轮：要加行，先在 arch-budget.json 里显式调——` +
        `调大是欠债，调小（朝 ${chatBudget.target} 走）是还债，两样都得是有意识的决定`,
    );
  }
  notes.push(`chat.rs ${lines} 行（预算 ${chatBudget.max}，O1 终态目标 ${chatBudget.target}）`);
}

// --- 2. ToolSpec 唯一表 ---
const registryCount = countMatches(
  "src-tauri/src/tools.rs",
  /const\s+REGISTRY:\s*\[ToolSpec/g,
);
if (registryCount !== 1) {
  failures.push(
    `ToolSpec 名册出现了 ${registryCount} 份（应为 1 份，tools.rs 的 REGISTRY）。` +
      "第二份名册 = 工具清单各说各话，声明/审批/审计全部失真",
  );
} else {
  notes.push("ToolSpec 名册唯一（tools.rs REGISTRY）✓");
}
// 其它文件里不许冒出同形声明（防复制粘贴到别处另立门户）
for (const rel of ["src-tauri/src/mcp.rs"]) {
  const n = countMatches(rel, /:\s*\[ToolSpec\s*;/g);
  if (n > 0) failures.push(`${rel} 出现了 ${n} 处 ToolSpec 数组声明——名册只准住在 tools.rs`);
}

// --- 3. capability 唯一收口 ---
const rustResolve = countMatches(
  "src-tauri/src/provider/capability.rs",
  /pub\s+fn\s+resolve\s*\(/g,
);
if (rustResolve !== 1) {
  failures.push(
    `Rust 侧能力识别收口出现 ${rustResolve} 处 pub fn resolve（应为 1，provider/capability.rs）`,
  );
} else {
  notes.push("Rust 能力识别收口唯一（provider::capability::resolve）✓");
}
const tsResolve = countMatches(
  "src/lib/model-capabilities.ts",
  /export\s+function\s+resolveCapabilities\s*\(/g,
);
if (tsResolve !== 1) {
  failures.push(
    `前端能力识别收口出现 ${tsResolve} 处 export function resolveCapabilities（应为 1，src/lib/model-capabilities.ts）`,
  );
} else {
  notes.push("前端能力识别收口唯一（model-capabilities.ts）✓");
}

// --- 报告 ---
for (const note of notes) console.log(`  ✓ ${note}`);
if (failures.length > 0) {
  for (const failure of failures) console.error(`  ✗ ${failure}`);
  console.error(`架构守卫：${failures.length} 处违规`);
  process.exit(1);
}
console.log("架构守卫：全部通过");
