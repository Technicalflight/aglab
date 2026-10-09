import { describe, expect, it, vi } from "vitest";
import { DecisionCache } from "../cache";

describe("DecisionCache", () => {
  it("键序无关：同一状态换个写法仍命中", () => {
    const cache = new DecisionCache(60_000, 10);
    const written = {
      state: { b: 2, a: 1, nested: { y: 2, x: 1 } },
      questions: { q1: { type: "noul" } },
    };
    const read = {
      state: { a: 1, b: 2, nested: { x: 1, y: 2 } },
      questions: { q1: { type: "noul" } },
    };
    cache.set(written, { answers: { q1: 1 } });
    expect(cache.get(read)).toEqual({ answers: { q1: 1 } });
  });

  it("TTL 过期后失效", () => {
    vi.useFakeTimers();
    try {
      const cache = new DecisionCache(1000, 10);
      const request = { state: "s", questions: {} };
      cache.set(request, { ok: true });
      expect(cache.get(request)).toEqual({ ok: true });
      vi.setSystemTime(Date.now() + 1500);
      expect(cache.get(request)).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("LRU：容量满时先赶最久没读的，读过的续命", () => {
    const cache = new DecisionCache(60_000, 2);
    const a = { state: "a", questions: {} };
    const b = { state: "b", questions: {} };
    const c = { state: "c", questions: {} };
    cache.set(a, 1);
    cache.set(b, 2);
    expect(cache.get(a)).toBe(1); // A 续命，B 变成最老的
    cache.set(c, 3);
    expect(cache.get(b)).toBeNull();
    expect(cache.get(a)).toBe(1);
    expect(cache.get(c)).toBe(3);
  });

  it("sensitivity 参与键：public 的缓存不能给 confidential 用", () => {
    const cache = new DecisionCache(60_000, 10);
    const pub = { state: "same", questions: {}, sensitivity: "public" };
    const conf = { state: "same", questions: {}, sensitivity: "confidential" };
    cache.set(pub, { model: "jev" });
    expect(cache.get(conf)).toBeNull();
    expect(cache.get(pub)).toEqual({ model: "jev" });
  });
});
