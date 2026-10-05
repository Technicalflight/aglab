// 生成 THIRD-PARTY-NOTICES.md：随安装包分发的第三方许可清单。
// 重跑：npm run licenses
//
// 范围口径：
// - JS 只扫 production 依赖（devDependencies 不进安装包，无分发义务）
// - Rust 扫 Cargo.lock 全量（按精确版本，license 从本地 cargo registry 缓存读，
//   缓存没有的走 crates.io API 补）
//
// 处理规则（写进产物头部，让读的人也知道）：
// - `A OR B`：按下面的偏好序选一个，整份清单里只出现被选中的那个许可证
// - `A AND B`：两个都得带
// - 许可证全文每种只附一份（附录），逐依赖只写 name@version + 许可证 + 出处

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const ROOT = path.resolve(import.meta.dirname, "..");
const OUT = path.join(ROOT, "THIRD-PARTY-NOTICES.md");

// OR 选项里的偏好序：能选 MIT 就选 MIT，否则选第一个命中的宽松许可证
const PREFERRED = [
  "MIT",
  "ISC",
  "BSD-2-Clause",
  "BSD-3-Clause",
  "0BSD",
  "Zlib",
  "Apache-2.0",
  "Unicode-3.0",
  "CDLA-Permissive-2.0",
  "MPL-2.0",
];

// 这些全文进附录；带 GPL 之类的选项只在 OR 里出现且从不被选中，不进
const TEXT_DENY = /GPL|AGPL|LGPL/;

function splitLicense(expr) {
  // "MIT OR Apache-2.0" / "Apache-2.0 AND ISC" / "A OR B OR C"
  return expr
    .split(/\s+(?:OR|AND)\s+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

function isOr(expr) {
  return /\sOR\s/.test(expr);
}

// 返回 { chosen: string[], note?: string } —— 这条依赖实际要附的许可证 id
function resolveLicense(expr) {
  if (isOr(expr)) {
    const options = splitLicense(expr);
    const pick = PREFERRED.find((p) => options.includes(p));
    if (pick) return { chosen: [pick] };
    const firstNonCopyleft = options.find((o) => !TEXT_DENY.test(o));
    if (firstNonCopyleft) return { chosen: [firstNonCopyleft] };
    return { chosen: options, note: "无可选 MIT 项" };
  }
  // AND 或单项：全部保留
  return { chosen: splitLicense(expr) };
}

// ---------- JS 侧 ----------
function scanNpm() {
  const raw = execFileSync(
    process.execPath,
    [path.join(ROOT, "node_modules", "license-checker", "bin", "license-checker"), "--production", "--json"],
    {
      cwd: ROOT,
      encoding: "utf8",
      maxBuffer: 64 * 1024 * 1024,
    },
  );
  const json = JSON.parse(raw);
  const rows = [];
  for (const [key, info] of Object.entries(json)) {
    // key 形如 "name@version"；根项目自己标 UNLICENSED，跳过
    if (info.licenses === "UNLICENSED" || info.path === ROOT) continue;
    const at = key.lastIndexOf("@");
    rows.push({
      name: key.slice(0, at),
      version: key.slice(at + 1),
      license: info.licenses,
      from: info.repository || info.homepage || "",
      side: "js",
    });
  }
  return rows;
}

// ---------- Rust 侧 ----------
async function scanCargo() {
  const lock = fs.readFileSync(path.join(ROOT, "src-tauri", "Cargo.lock"), "utf8");
  const pkgs = [...lock.matchAll(/name = "([^"]+)"\nversion = "([^"]+)"/g)].map((m) => ({
    name: m[1],
    version: m[2],
  }));
  const regDir = path.join(os.homedir(), ".cargo", "registry", "src");
  const caches = fs.existsSync(regDir) ? fs.readdirSync(regDir) : [];
  const known = new Set();
  for (const c of caches) {
    for (const d of fs.readdirSync(path.join(regDir, c))) known.add(d);
  }

  const rows = [];
  const missing = [];
  for (const { name, version } of pkgs) {
    if (name === "aglab") continue;
    let license = null;
    const dir = `${name}-${version}`;
    if (known.has(dir)) {
      for (const c of caches) {
        const toml = path.join(regDir, c, dir, "Cargo.toml");
        if (!fs.existsSync(toml)) continue;
        const m = fs.readFileSync(toml, "utf8").match(/^license\s*=\s*"([^"]+)"/m);
        if (m) license = m[1];
        break;
      }
    }
    const row = { name, version, license, from: `https://crates.io/crates/${name}/${version}`, side: "rust" };
    rows.push(row);
    if (!license) missing.push(row);
  }

  // 本地缓存没有的（常见于未编译的平台 target 依赖），走 crates.io 补
  for (const row of missing) {
    try {
      const res = await fetch(`https://crates.io/api/v1/crates/${row.name}/${row.version}`, {
        headers: { "User-Agent": "aglab-license-audit (local dev)" },
      });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const json = await res.json();
      row.license = json?.version?.license ?? null;
      if (!row.license) throw new Error("响应里没有 license 字段");
    } catch (e) {
      console.error(`  ⚠️ crates.io 回退失败 ${row.name}@${row.version}: ${e.message}`);
      row.license = null;
    }
  }
  return rows;
}

// ---------- 许可证全文（SPDX） ----------
async function fetchLicenseTexts(ids) {
  const texts = {};
  for (const id of [...ids].sort()) {
    if (TEXT_DENY.test(id)) continue;
    const res = await fetch(`https://spdx.org/licenses/${id}.json`);
    if (!res.ok) {
      texts[id] = `> SPDX 拉取失败（HTTP ${res.status}），发布前需人工补全文。`;
      continue;
    }
    const json = await res.json();
    texts[id] = json.licenseText ?? `> SPDX 无 ${id} 全文。`;
  }
  return texts;
}

// ---------- 组装 ----------
function renderRows(rows) {
  return rows
    .slice()
    .sort((a, b) => a.name.localeCompare(b.name))
    .map((r) => {
      const lic = r.license ? resolveLicense(r.license) : { chosen: [], note: "许可证未知，发布前必须人工核实" };
      const text = lic.chosen.join(" AND ") + (lic.note ? `（${lic.note}）` : "");
      return `- ${r.name}@${r.version} — ${text}${r.from ? ` — ${r.from}` : ""}`;
    })
    .join("\n");
}

const npm = scanNpm();
const cargo = await scanCargo();

const usedIds = new Set();
for (const r of [...npm, ...cargo]) {
  if (!r.license) continue;
  const { chosen } = resolveLicense(r.license);
  chosen.forEach((id) => usedIds.add(id));
}
const texts = await fetchLicenseTexts(usedIds);

const unknown = [...npm, ...cargo].filter((r) => !r.license);

const md = `# 第三方许可清单（THIRD-PARTY NOTICES）

本文件由 \`npm run licenses\` 自动生成，覆盖 JS production 依赖（${npm.length} 项）与 Rust 全量依赖（${cargo.length} 项）。

处理口径：
- 双许可 \`A OR B\` 一律按宽松许可证择一（优先 MIT），每条依赖只按被选中的许可证引用。
- \`A AND B\` 的组件（当前只有 \`ring\`）需同时满足两个许可证的义务，全文均附于附录。
- 许可证全文每种附一份（附录），对使用该许可证的全部组件生效。
- Apache-2.0 组件如有 NOTICE 文件，分发时需一并保留——发布前逐个核对。

## JavaScript（${npm.length}）

${renderRows(npm)}

## Rust（${cargo.length}）

${renderRows(cargo)}
${unknown.length ? `\n## ⚠️ 许可证未知的依赖（${unknown.length}，发布前必须人工核实）\n\n${unknown.map((r) => `- ${r.name}@${r.version}`).join("\n")}\n` : ""}
## 附录：许可证全文

${Object.entries(texts)
  .map(([id, text]) => `### ${id}\n\n\`\`\`\n${text.trim()}\n\`\`\``)
  .join("\n\n")}
`;

fs.writeFileSync(OUT, md);
console.log(
  `已生成 ${OUT}：JS ${npm.length} 项 / Rust ${cargo.length} 项 / 许可证全文 ${Object.keys(texts).length} 种` +
    (unknown.length ? ` / ⚠️ ${unknown.length} 项未知` : ""),
);
