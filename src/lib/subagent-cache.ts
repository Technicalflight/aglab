import { builtinSubagentsList, type PoolCatalogEntry } from "@/lib/chat-transport";
import { catalogFingerprint, loadCatalog } from "@/lib/model-catalog";
import type { AppConfig, BuiltinSubagentView } from "@/types/chat";

/** 出厂名册的会话级缓存：定义住后端代码，一次拉到手就不必每进一次页面问一遍 */
let builtinRosterCache: BuiltinSubagentView[] | null = null;
/** 上一次拉名册时的覆盖指纹：覆盖没变（多半是没变），重进页面连 IPC 都不发 */
let rosterFetchedOverridesKey: string | null = null;
/** 模型目录的会话级缓存：指纹一致时进页面连 localStorage 都不用再解析 */
let catalogMemo: { fingerprint: string; entries: PoolCatalogEntry[] } | null = null;

export function getBuiltinRosterCache(): BuiltinSubagentView[] | null {
  return builtinRosterCache;
}

export function getRosterFetchedOverridesKey(): string | null {
  return rosterFetchedOverridesKey;
}

export function setBuiltinRosterCache(list: BuiltinSubagentView[], overridesKey: string) {
  builtinRosterCache = list;
  rosterFetchedOverridesKey = overridesKey;
}

export function getCatalogMemo() {
  return catalogMemo;
}

export function setCatalogMemo(fingerprint: string, entries: PoolCatalogEntry[]) {
  catalogMemo = { fingerprint, entries };
}

/**
 * 预热：设置壳一打开就在后台把名册与模型目录备好（对齐「子助理」页进页即画）。
 * 名册那次顺手记下覆盖指纹——预热成功的话，第一次进页也是零 IPC；
 * 目录照指纹缓存，指纹变了才真的轮服务商。
 * 住在独立小模块里：设置壳引用预热时不会把整个子助理页拖进设置壳的包。
 */
export function warmSubagentCaches(config: AppConfig) {
  if (builtinRosterCache === null) {
    const overridesKey = JSON.stringify(config.subagentOverrides ?? []);
    builtinSubagentsList()
      .then((list) => {
        builtinRosterCache = list;
        rosterFetchedOverridesKey = overridesKey;
      })
      .catch(() => undefined);
  }
  const fingerprint = catalogFingerprint(config);
  if (catalogMemo === null || catalogMemo.fingerprint !== fingerprint) {
    loadCatalog(config)
      .then((entries) => {
        catalogMemo = { fingerprint, entries };
      })
      .catch(() => undefined);
  }
}
