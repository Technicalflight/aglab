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
 * 4. 模块依赖环（O19-1 第一条新钉）——按**模块目录**建依赖图，互相依赖的组（强连通分量）即环。
 *    按目录而不是按文件：Rust 里文件级互相 use 完全合法，按文件建图满屏噪音，
 *    而目录级互相依赖才是"两个子系统缠在一起"这个真问题。
 *    判据只算生产代码：注释与 #[cfg(test)] 之后的测试模块都不算——测试里的
 *    `crate::X` 会造出生产里不存在的边（实测：不排测试时那组会从 31 个涨到 38 个）。
 *    现状不是"很干净"，而是有两组存量环（一组 4 个、一组 31 个，中心是 chat），
 *    挂在 arch-budget.json 的基线里。棘轮朝终态走：新增环组直接红，
 *    修好的环组若不从基线里删掉同样红——这条钉子的作用是**冻结已知缠结、只许往外拆**。
 * 5. 文件行数上限（O19-1 第二条新钉）——任意源文件（Rust + 前端，测试文件除外）≤1500 行。
 *    长文件是拆分债的温度计：超限文件逐个记进 arch-budget.json 的「文件行数基线」，
 *    冻结的是**动手那一刻的行数**——豁免文件只许变短，涨一行也红；
 *    降到上限以内的要当场从基线删掉，修好了不记账基线就烂在原地。
 *    测试文件不计行数红线：测试长是覆盖的影子，不是结构债。
 * 6. 公开方法数上限（O19-1 第三条新钉）——单个 Rust 文件的生产面 pub fn ≤20 个
 *    （pub / pub(crate) / pub(super) / pub(in ...) 都算：无论限定得多窄，
 *    都是这个文件往外暴露的名字，暴露面就是要管的东西）。
 *    只算生产代码（注释与 #[cfg(test)] 之后剔除）——测试模块里的 pub fn 是测试基建，
 *    不是 API 面，混进来基线数字会随测试增长失真（实测 config.rs 全量 61 / 生产面 1）。
 *    超限文件逐个冻结进「公开方法数基线」，棘轮语义与第 5 条相同。
 *
 * 用法：node scripts/arch-guard.mjs（CI 的 desktop job 里跑，本地随手可跑）
 */
import { readdirSync, readFileSync, statSync } from "node:fs";
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

// --- 4. 模块依赖环（按目录建图） ---
const rustRoot = "src-tauri/src";
const walkRust = (dir, out = []) => {
  for (const name of readdirSync(path.join(root, dir))) {
    const rel = path.posix.join(dir, name);
    if (statSync(path.join(root, rel)).isDirectory()) walkRust(rel, out);
    else if (name.endsWith(".rs")) out.push(rel);
  }
  return out;
};
// 一个文件属于哪个节点：顶层目录算一个节点，顶层文件各自算一个
const nodeOf = (rel) => {
  const parts = rel.slice(rustRoot.length + 1).split("/");
  return parts.length === 1 ? parts[0].replace(/\.rs$/, "") : parts[0];
};
// 只留生产代码：去注释 + 砍掉 #[cfg(test)] 之后的部分
const productionOnly = (src) => {
  const withoutComments = src
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/\/\/[^\n]*/g, "");
  const tests = withoutComments.indexOf("#[cfg(test)]");
  return tests === -1 ? withoutComments : withoutComments.slice(0, tests);
};
const rustFiles = walkRust(rustRoot);
const moduleNodes = [...new Set(rustFiles.map(nodeOf))].sort();
const moduleEdges = new Map(moduleNodes.map((name) => [name, new Set()]));
for (const rel of rustFiles) {
  const from = nodeOf(rel);
  for (const hit of productionOnly(read(rel)).matchAll(/crate::([A-Za-z_][A-Za-z0-9_]*)/g)) {
    if (moduleNodes.includes(hit[1]) && hit[1] !== from) moduleEdges.get(from).add(hit[1]);
  }
}
// Tarjan 求强连通分量：>1 个成员的组就是环。用 SCC 而不是枚举具体路径——
// SCC 的成员集合与遍历起点无关，基线才稳定（枚举路径会随文件顺序飘）
const sccs = [];
{
  let clock = 0;
  const index = new Map();
  const low = new Map();
  const onStack = new Set();
  const stack = [];
  const strong = (node) => {
    index.set(node, clock);
    low.set(node, clock);
    clock += 1;
    stack.push(node);
    onStack.add(node);
    for (const next of moduleEdges.get(node)) {
      if (!index.has(next)) {
        strong(next);
        low.set(node, Math.min(low.get(node), low.get(next)));
      } else if (onStack.has(next)) {
        low.set(node, Math.min(low.get(node), index.get(next)));
      }
    }
    if (low.get(node) === index.get(node)) {
      const group = [];
      let popped;
      do {
        popped = stack.pop();
        onStack.delete(popped);
        group.push(popped);
      } while (popped !== node);
      if (group.length > 1) sccs.push(group.sort());
    }
  };
  for (const node of moduleNodes) if (!index.has(node)) strong(node);
}
const cycleBaseline = budget["模块依赖环基线"];
if (!cycleBaseline || !Array.isArray(cycleBaseline["现有"])) {
  failures.push("arch-budget.json 里没有「模块依赖环基线」——新钉与基线必须成对落地");
} else {
  const key = (group) => [...group].sort().join("+");
  const known = new Set(cycleBaseline["现有"].map(key));
  const found = new Set(sccs.map(key));
  // 组可能很大（现存那组 31 个模块），报的时候截断，别把一行刷成几百字
  const show = (group) =>
    group.length <= 8
      ? group.join(" ↔ ")
      : `${group.slice(0, 8).join(" ↔ ")} …（共 ${group.length} 个）`;
  for (const group of sccs) {
    if (!known.has(key(group))) {
      failures.push(
        `新增模块依赖环：${show(group)}。目录级互相依赖 = 两个子系统缠在一起，` +
          "要么把共用件下沉到第三处，要么让单向的那一半掉头；" +
          "确认无解就往 arch-budget.json 的「模块依赖环基线」里显式记一笔（那是欠债）",
      );
    }
  }
  for (const group of cycleBaseline["现有"]) {
    if (found.has(key(group))) continue;
    // 精确匹配不上有两种：组变大了（有新模块缠进来，坏消息）与组变小/没了
    // （往外拆有成效，好消息）。两种都得红——棘轮只许变短，但话得说准
    const grew = sccs.find((candidate) => group.every((name) => candidate.includes(name)));
    if (grew) {
      failures.push(
        `模块依赖环变大了：基线里那组 ${group.length} 个模块，现在 ${grew.length} 个——` +
          `新缠进来的是 ${grew.filter((name) => !group.includes(name)).join("、")}。` +
          "要么让它单向依赖，要么把这一笔显式记进 arch-budget.json 的基线（那是欠债）",
      );
    } else {
      failures.push(
        `基线里的环组（${group.length} 个模块）已经变小或不存在了——把 arch-budget.json 的` +
          "「模块依赖环基线」改成现在的样子。棘轮只许变短：拆好了不记账，基线就烂在原地",
      );
    }
  }
  if (sccs.length === 0) notes.push("模块依赖环 0 组（基线已清空）✓");
  else
    notes.push(
      `模块依赖环 ${sccs.length} 组（均在基线内：${sccs.map((g) => `${g.length} 个模块`).join("、")}）`,
    );
}

// --- 5. 文件行数上限（任意源文件 ≤1500） ---
const FILE_LINE_LIMIT = 1500;
// 与钉 1 同口径（数换行符）
const lineCount = (src) => {
  const lines = src.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  return lines.length;
};
// 前端源文件：测试不计行数红线（测试长是覆盖的影子，不是结构债）
const walkFront = (dir, out = []) => {
  for (const name of readdirSync(path.join(root, dir))) {
    const rel = path.posix.join(dir, name);
    if (statSync(path.join(root, rel)).isDirectory()) {
      if (name !== "node_modules" && name !== "__tests__") walkFront(rel, out);
    } else if (/\.(ts|tsx)$/.test(name) && !/\.test\.tsx?$/.test(name)) out.push(rel);
  }
  return out;
};
const lineBaseline = budget["文件行数基线"];
if (!lineBaseline || typeof lineBaseline["超限"] !== "object") {
  failures.push("arch-budget.json 里没有「文件行数基线」——新钉与基线必须成对落地");
} else {
  const sourceFiles = [...walkRust(rustRoot), ...walkFront("src")];
  const counts = new Map(sourceFiles.map((rel) => [rel, lineCount(read(rel))]));
  const exempt = lineBaseline["超限"];
  for (const [rel, lines] of counts) {
    if (lines > FILE_LINE_LIMIT && !(rel in exempt)) {
      failures.push(
        `${rel} 行数 ${lines} 超过单文件上限 ${FILE_LINE_LIMIT}。先拆再交——` +
          "长文件是拆分债；确要缓一手，往 arch-budget.json 的「文件行数基线」显式记一笔（那是欠债）",
      );
    }
  }
  let worst = null;
  for (const [rel, frozen] of Object.entries(exempt)) {
    const lines = counts.get(rel);
    if (lines === undefined) {
      failures.push(`文件行数基线里的 ${rel} 已经不在源码树里——把这一条从基线里删掉`);
    } else if (lines > frozen) {
      failures.push(
        `${rel} 行数 ${lines} 超过基线冻结的 ${frozen}。豁免不是放大器：基线记的是动手那一刻的数，` +
          "增长必须拆掉，或显式调基线（欠债）",
      );
    } else if (lines <= FILE_LINE_LIMIT) {
      failures.push(
        `${rel} 已经降到 ${FILE_LINE_LIMIT} 行以内——把它从 arch-budget.json 的「文件行数基线」里删掉。` +
          "棘轮只许变短：修好了不记账，基线就烂在原地",
      );
    } else if (!worst || lines > worst.lines) {
      worst = { rel, lines, frozen };
    }
  }
  if (worst) {
    notes.push(
      `文件行数：${Object.keys(exempt).length} 个超限文件均在基线内（最高 ${worst.rel} ${worst.lines} 行，` +
        `上限 ${FILE_LINE_LIMIT}）✓`,
    );
  }
}

// --- 6. 公开方法数上限（Rust 生产面 pub fn ≤20） ---
const PUB_FN_LIMIT = 20;
// 行首锚定；pub / pub(crate) / pub(super) / pub(in crate::x) 全算——无论限定得多窄，
// 都是这个文件往外暴露的名字。async / unsafe 修饰都要认。
const PUB_FN_RE =
  /^[ \t]*pub(?:\([a-z0-9_: ]+\))?[ \t]+(?:async[ \t]+)?(?:unsafe[ \t]+)?fn\b/gm;
const pubFnBaseline = budget["公开方法数基线"];
if (!pubFnBaseline || typeof pubFnBaseline["超限"] !== "object") {
  failures.push("arch-budget.json 里没有「公开方法数基线」——新钉与基线必须成对落地");
} else {
  // 只算生产代码：测试模块里的 pub fn 是测试基建不是 API 面（实测 config.rs 全量 61 / 生产面 1）
  const counts = new Map(
    rustFiles.map((rel) => [rel, (productionOnly(read(rel)).match(PUB_FN_RE) ?? []).length]),
  );
  const exempt = pubFnBaseline["超限"];
  for (const [rel, count] of counts) {
    if (count > PUB_FN_LIMIT && !(rel in exempt)) {
      failures.push(
        `${rel} 生产面 pub fn ${count} 个，超过上限 ${PUB_FN_LIMIT}。公开面是这个文件对外的承诺，` +
          "承诺越少越好——先收拢职责再交；确要缓一手，往 arch-budget.json 的「公开方法数基线」显式记一笔（那是欠债）",
      );
    }
  }
  let worst = null;
  for (const [rel, frozen] of Object.entries(exempt)) {
    const count = counts.get(rel);
    if (count === undefined) {
      failures.push(`公开方法数基线里的 ${rel} 已经不在源码树里——把这一条从基线里删掉`);
    } else if (count > frozen) {
      failures.push(
        `${rel} 生产面 pub fn ${count} 个，超过基线冻结的 ${frozen}。豁免不是放大器：` +
          "基线记的是动手那一刻的数，增长必须收拢，或显式调基线（欠债）",
      );
    } else if (count <= PUB_FN_LIMIT) {
      failures.push(
        `${rel} 的公开面已经降到 ${PUB_FN_LIMIT} 个以内——把它从 arch-budget.json 的` +
          "「公开方法数基线」里删掉。棘轮只许变短：修好了不记账，基线就烂在原地",
      );
    } else if (!worst || count > worst.count) {
      worst = { rel, count, frozen };
    }
  }
  if (worst) {
    notes.push(
      `公开方法数：${Object.keys(exempt).length} 个超限文件均在基线内（最高 ${worst.rel} ${worst.count} 个，` +
        `上限 ${PUB_FN_LIMIT}）✓`,
    );
  }
}

// --- 报告 ---
for (const note of notes) console.log(`  ✓ ${note}`);
if (failures.length > 0) {
  for (const failure of failures) console.error(`  ✗ ${failure}`);
  console.error(`架构守卫：${failures.length} 处违规`);
  process.exit(1);
}
console.log("架构守卫：全部通过");
