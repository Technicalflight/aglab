/**
 * 链接提取的行为钉：中文标点边界、尾残标点、去重、残根过滤。
 * 这是 composer「识别链接」功能的判定源——边界错了抓错正文比不抓更糟。
 */
import { describe, expect, it } from "vitest";
import { extractUrls, hostOfUrl } from "../links";

describe("extractUrls", () => {
  it("基本提取与去重保序", () => {
    expect(extractUrls("看下 https://a.com/x 和 https://b.com/y，再看看 https://a.com/x")).toEqual([
      "https://a.com/x",
      "https://b.com/y",
    ]);
  });

  it("中文标点是右边界：句号、括号、顿号不进 URL", () => {
    expect(extractUrls("https://a.com/1。再看")).toEqual(["https://a.com/1"]);
    expect(extractUrls("（https://a.com/2）")).toEqual(["https://a.com/2"]);
    expect(extractUrls("https://a.com/3、https://b.com/4")).toEqual(["https://a.com/3", "https://b.com/4"]);
  });

  it("行尾英文残标点剥掉：句号逗号不是 URL 的一部分", () => {
    expect(extractUrls("see https://a.com/x.")).toEqual(["https://a.com/x"]);
    expect(extractUrls("see https://a.com/x, and this")).toEqual(["https://a.com/x"]);
  });

  it("路径内的合法标点保留：查询串与波浪线不受伤", () => {
    expect(extractUrls("https://a.com/x?q=1&r=2")).toEqual(["https://a.com/x?q=1&r=2"]);
    expect(extractUrls("https://a.com/~user/page")).toEqual(["https://a.com/~user/page"]);
  });

  it("残根过滤：没有 host 的不算链接", () => {
    expect(extractUrls("https:// 就是个词头")).toEqual([]);
    expect(extractUrls("http://localhost:3000/api")).toEqual(["http://localhost:3000/api"]);
  });

  it("空串与无链接文本返回空集", () => {
    expect(extractUrls("")).toEqual([]);
    expect(extractUrls("普通中文消息，没有链接")).toEqual([]);
  });
});

describe("hostOfUrl", () => {
  it("取 host，坏 URL 原样返回", () => {
    expect(hostOfUrl("https://docs.example.com/a/b?q=1")).toBe("docs.example.com");
    expect(hostOfUrl("not a url")).toBe("not a url");
  });
});
