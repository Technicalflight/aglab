/**
 * 输入框的链接识别：从草稿里抽出用户贴进来/敲进来的 http(s) 链接。
 * 纯函数——正则的边界条件（中文标点、行尾句号、重复链接）在这里钉死，
 * 组件只消费结果不关心怎么数。
 */

/**
 * URL 的右边界：空格、引号、英文配对符号收尾都停；中文标点同样当边界——
 * 「看 https://x.com/a。」「（https://x.com/b）」里的句号与括号不是 URL 的一部分
 */
const URL_PATTERN = /https?:\/\/[^\s<>"'`）)\]}，。；！？、：」』】]+/g;

/** 从草稿提取链接：去重保序、剥掉尾部残标点（句号/逗号不是 URL 的一部分） */
export function extractUrls(text: string): string[] {
  const matches = text.match(URL_PATTERN) ?? [];
  const cleaned = matches.map((url) => url.replace(/[.,;:!?…]+$/, ""));
  return [...new Set(cleaned)].filter((url) => {
    // 剥完标点后至少要有 host 的样子（x.yy），避免 "https://" 这种残根
    try {
      const parsed = new URL(url);
      return parsed.hostname.includes(".") || parsed.hostname === "localhost";
    } catch {
      return false;
    }
  });
}

/** 主机名展示用：一行 chip 放不下整个 URL */
export function hostOfUrl(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}
