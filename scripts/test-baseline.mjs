/**
 * 测试基线守卫（优化路线 O3-4）：把 `cargo test --lib` 的
 * passed / failed / ignored 与 scripts/test-baseline.json 对账。
 *
 * 用法：cargo test --lib 2>&1 | node scripts/test-baseline.mjs
 * （CI 里把测试输出落盘再喂进来；本地随手可跑）
 *
 * 对账规则（棘轮）：
 * - passed  ≥ 基线（只许涨，跌了就是有人删了测试或测试没跑）
 * - failed  ≤ 基线（基线里那 1 个是回收站的本机环境失败；CI 上多一个失败就是坏了）
 * - ignored ≤ 基线（跳过要显式调基线）
 * 基线数字变了必须出现在提交里——和 arch-budget.json 同一个纪律。
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const baseline = JSON.parse(readFileSync(path.join(root, "scripts", "test-baseline.json"), "utf8"));
const input = readFileSync(0, "utf8");

// 匹配 cargo test 的收尾行：test result: ok. 1266 passed; 0 failed; 5 ignored; ...
const m = input.match(/test result:\s*\w+\.\s*(\d+) passed;\s*(\d+) failed;\s*(\d+) ignored/);
if (!m) {
  console.error("test-baseline：没在输入里找到 test result 行——先跑 cargo test --lib");
  process.exit(2);
}
const passed = Number(m[1]);
const failed = Number(m[2]);
const ignored = Number(m[3]);

const problems = [];
if (passed < baseline.passed) problems.push(`passed ${passed} < 基线 ${baseline.passed}（测试变少了）`);
if (failed > baseline.failed) problems.push(`failed ${failed} > 基线 ${baseline.failed}（新失败）`);
if (ignored > baseline.ignored) problems.push(`ignored ${ignored} > 基线 ${baseline.ignored}（又跳过了）`);

const summary = `| passed | failed | ignored |\n|---|---|---|\n| ${passed} ${passed >= baseline.passed ? "✓" : "✗"} | ${failed} ${failed <= baseline.failed ? "✓" : "✗"} | ${ignored} ${ignored <= baseline.ignored ? "✓" : "✗"} |`;
console.log(`测试基线对账：${passed}/${failed}/${ignored}（基线 ${baseline.passed}/${baseline.failed}/${baseline.ignored}）`);
console.log(summary);
// GitHub Actions：写进 job 摘要页
if (process.env.GITHUB_STEP_SUMMARY) {
  const { appendFileSync } = await import("node:fs");
  appendFileSync(process.env.GITHUB_STEP_SUMMARY, `## 测试基线\n\n${summary}\n`);
}
if (problems.length > 0) {
  for (const problem of problems) console.error(`  ✗ ${problem}`);
  process.exit(1);
}
console.log("  ✓ 基线之内");
