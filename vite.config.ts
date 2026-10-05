import { createLogger, defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { fileURLToPath } from "node:url";
import process from "node:process";

const host = process.env.TAURI_DEV_HOST;

// 刷屏折叠：Tailwind v4 对每次保存都会重发 index.css 的 HMR，一次保存常打两行，
// 批量保存时一屏全是同一句话。只折叠"仅 index.css"的重复行，别的模块更新原样过。
// 走 customLogger（文档保证的挂法），不用插件的 configureServer 里偷换
function createCollapsedLogger() {
  const logger = createLogger();
  const baseInfo = logger.info.bind(logger);
  let swallowing = false;
  let folded = 0;
  logger.info = (msg, options) => {
    // Vite 的 logger 拿到的 msg 不带时间戳（写入时才加），但带 [vite] 与
    // (client) 前缀，颜色码在前后——全部剥掉再比对
    const strip = (text: string) =>
      text
        .replace(/\u001b[[0-9;]*m/g, "")
        .replace(/^\[vite\]\s*/, "")
        .replace(/^\(client\)\s*/, "");
    const plain = strip(msg);
    if (/^hmr update \S*\/src\/index\.css$/.test(plain)) {
      if (swallowing) {
        folded += 1;
        return;
      }
      swallowing = true;
      folded = 0;
      baseInfo(msg, options);
      return;
    }
    if (swallowing && folded > 0) {
      baseInfo(`  └ 同上的 index.css 更新又发了 ${folded} 次（已折叠）`);
    }
    swallowing = false;
    folded = 0;
    baseInfo(msg, options);
  };
  return logger;
}

// https://vite.dev/config/
export default defineConfig(() => ({
  customLogger: createCollapsedLogger(),
  plugins: [react(), tailwindcss()],

  resolve: {
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    // 默认的 `localhost` 在 Vite 8 + 新 Node 上只绑 ::1（IPv6 环回），而 tauri-cli
    // 轮询 devUrl 走 IPv4——于是 "Waiting for your frontend dev server" 永远等不到
    // （实测 netstat 只有 [::1]:1420 在听）。钉死 IPv4 环回，两边与 devUrl 同一句话；
    // TAURI_DEV_HOST（手机/局域网调试）仍然优先
    host: host || "127.0.0.1",
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },

  // Temp/ 是参照克隆与草稿区（pi / dsh / resin / 别家仓库的 tarball），里面那些
  // *.test.js 是**别人项目**的测试：vitest 默认的收集规则会把它们当本项目的用例跑，
  // 于是满屏红而 aglab 一行代码都没坏。写死在这里，下一次往 Temp 放仓库不会再咬到
  test: {
    exclude: ["**/node_modules/**", "**/.git/**", "**/Temp/**"],
  },

  build: {
    // 桌面端不追求"最小单包"，追求**可缓存**：拆成 vendor 后，
    // 业务代码改动只会让 app chunk 失效，react/radix/markdown 这些几乎不动的
    // 依赖留在缓存里。原先 1.4 MB 单包，用户每改一行都要重下整包。
    //
    // Vite 8 底层是 Rolldown：对象式 manualChunks 已不支持，
    // 分组改用 output.codeSplitting.groups。
    rollupOptions: {
      output: {
        codeSplitting: {
          groups: [
            // React 与其调度器必须同组——版本必须严格配对
            { name: "react", test: /node_modules[\\/](react|react-dom|scheduler)[\\/]/ },
            // 浮层与无障碍原语。radix-ui 是聚合包，真正的模块落在 @radix-ui/* 下，
            // 正则只写 radix-ui 会一个都匹配不到（第一版就漏了）
            { name: "radix", test: /node_modules[\\/](@radix-ui|radix-ui)[\\/]/ },
            // Markdown 渲染链：体积大、变动少
            {
              name: "markdown",
              test: /node_modules[\\/](react-markdown|remark-gfm|remark-math|rehype-katex|micromark|mdast|unist|unified|vfile|hast|property-information|space-separated-tokens|comma-separated-tokens|html-url-attributes|zwitch|longest-streak|ccount|markdown-table|escape-string-regexp|character-entities|decode-named-character-reference|trim-lines|devlop|extend)[\\/]/,
            },
            // shiki **引擎**与主题。必须显式排除 @shikijs/langs：
            // 那 15 个语言包靠 dynamic import 按需加载（见 src/lib/highlight.ts），
            // 一旦被这个正则捞进来，首屏就得吞掉约 1 MB 的语法数据，
            // 懒加载优化当场作废。正则只认 @shikijs/core 与 @shikijs/types 等，
            // 不写宽泛的 @shikijs。
            {
              name: "shiki",
              test: /node_modules[\\/](shiki[\\/]|@shikijs[\\/](core|types|engine|themes|vfs|regex|utils|dual-classes)[\\/])/,
            },
          ],
        },
      },
    },
    // 单 chunk 超过 500kB 才告警：拆完之后 markdown 一类仍会偏大，
    // 那是刻意的缓存权衡，不该每次构建都刷一屏黄字
    chunkSizeWarningLimit: 900,
  },
}));
