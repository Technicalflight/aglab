import { createHighlighterCore } from "shiki/core";
import { createJavaScriptRegexEngine } from "shiki/engine/javascript";
import type { ThemeInput } from "shiki/types";

const THEME = {
  name: "aglab-dark",
  type: "dark",
  colors: {
    "editor.background": "#191a1f",
    "editor.foreground": "#dfe0e5",
  },
  tokenColors: [
    { scope: ["comment"], settings: { foreground: "#5e616b", fontStyle: "italic" } },
    { scope: ["keyword", "storage.type", "storage.modifier"], settings: { foreground: "#a48ae6" } },
    {
      scope: ["string", "punctuation.definition.string.begin"],
      settings: { foreground: "#8dba77" },
    },
    {
      scope: ["constant.numeric", "constant.language", "constant.character"],
      settings: { foreground: "#c99a5e" },
    },
    {
      scope: ["entity.name.function", "support.function", "meta.function-call"],
      settings: { foreground: "#7d9fd6" },
    },
    {
      scope: [
        "entity.name.type",
        "entity.name.class",
        "support.type",
        "entity.other.attribute-name",
      ],
      settings: { foreground: "#5fa89d" },
    },
    { scope: ["entity.name.tag"], settings: { foreground: "#cd7f8b" } },
    {
      scope: ["variable", "variable.other.property", "variable.other.object"],
      settings: { foreground: "#c6c8d2" },
    },
    {
      scope: ["punctuation", "meta.brace", "punctuation.separator"],
      settings: { foreground: "#787b86" },
    },
  ],
} satisfies ThemeInput;

const LANGUAGE_LOADERS = {
  typescript: () => import("@shikijs/langs/typescript"),
  tsx: () => import("@shikijs/langs/tsx"),
  javascript: () => import("@shikijs/langs/javascript"),
  jsx: () => import("@shikijs/langs/jsx"),
  rust: () => import("@shikijs/langs/rust"),
  python: () => import("@shikijs/langs/python"),
  json: () => import("@shikijs/langs/json"),
  yaml: () => import("@shikijs/langs/yaml"),
  toml: () => import("@shikijs/langs/toml"),
  bash: () => import("@shikijs/langs/bash"),
  shellscript: () => import("@shikijs/langs/shellscript"),
  html: () => import("@shikijs/langs/html"),
  css: () => import("@shikijs/langs/css"),
  markdown: () => import("@shikijs/langs/markdown"),
  go: () => import("@shikijs/langs/go"),
} satisfies Record<string, () => Promise<unknown>>;

const ALIAS: Record<string, keyof typeof LANGUAGE_LOADERS> = {
  ts: "typescript",
  js: "javascript",
  py: "python",
  rs: "rust",
  sh: "bash",
  zsh: "bash",
  console: "bash",
  jsonc: "json",
  md: "markdown",
};

type Highlighter = Awaited<ReturnType<typeof createHighlighterCore>>;

let highlighterPromise: Promise<Highlighter> | null = null;

/**
 * 语法高亮器只建一次，但**语言包交给 Shiki 自己按需加载**。
 *
 * 这里传的是 loader **函数**而不是它们的返回值（原写法是
 * `langs: Object.values(LANGUAGE_LOADERS).map(load => load())`，
 * 一次性并发拉起全部 15 个语言 chunk，约 1 MB）。传函数后 Shiki 只在
 * 真正遇到某种语言时才去解析那一个——首次高亮从"等 1 MB"变成"等 100 KB"。
 *
 * 注意别改成运行时 loadLanguage()：那样 getLoadedLanguages() 确实会列出该语言，
 * 但 codeToTokens 查的是建壳时冻结的注册表，会抛 "Language `rust` not found"。
 * 已用 vitest 钉住这条链路。
 */
function createHighlighter(): Promise<Highlighter> {
  if (!highlighterPromise) {
    highlighterPromise = createHighlighterCore({
      themes: [THEME],
      langs: Object.values(LANGUAGE_LOADERS),
      engine: createJavaScriptRegexEngine(),
    }).catch((error: unknown) => {
      highlighterPromise = null;
      throw error;
    });
  }
  return highlighterPromise;
}

export interface HighlightToken {
  content: string;
  color?: string;
}

export function resolveLanguage(input?: string): keyof typeof LANGUAGE_LOADERS | "text" {
  const lang = input?.toLowerCase().trim() ?? "";
  if (lang in LANGUAGE_LOADERS) return lang as keyof typeof LANGUAGE_LOADERS;
  const aliased = ALIAS[lang];
  if (aliased) return aliased;
  return "text";
}

export async function highlightCode(
  code: string,
  lang: keyof typeof LANGUAGE_LOADERS | "text",
): Promise<HighlightToken[][]> {
  const highlighter = await createHighlighter();
  const result = highlighter.codeToTokens(code, {
    lang,
    theme: THEME.name,
  });

  return result.tokens.map((line) =>
    line.map((token) => ({
      content: token.content,
      color: token.color ?? undefined,
    })),
  );
}
