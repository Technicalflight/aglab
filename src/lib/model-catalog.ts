/**
 * 模型目录的共享取数层：`pool_catalog` 的结果按「服务商配置指纹」缓存进 localStorage。
 * 模型池页与子助理页共用这一份——同一个问题不许有两个问法，也不该有两个缓存。
 */

import { fetchPoolCatalog, type PoolCatalogEntry } from "@/lib/chat-transport";
import type { AppConfig } from "@/types/chat";

/** 目录缓存的 localStorage 键。v1：服务商配置指纹 + 上一次拉到的条目 */
const CATALOG_CACHE_KEY = "modelPool.catalog.v1";

interface CatalogCache {
  fingerprint: string;
  entries: PoolCatalogEntry[];
  savedAt: number;
}

/** pool_catalog 的内容只由这些连接要素决定：当前连接 + 每张档案的
 *  （地址、线协议、凭据服务、凭据用户、代理绑定）与展示名，外加全局代理
 *  （当前连接那一格经全局解析）。指纹一致就说明拉出来的目录不会变，
 *  不必重跑各服务商。池成员、权重、模式这些不影响目录的配置不进指纹——
 *  改它们不该触发重拉 */
export function catalogFingerprint(
  config: Pick<
    AppConfig,
    | "baseUrl"
    | "apiFormat"
    | "credentialService"
    | "credentialUser"
    | "profiles"
    | "proxy"
    | "proxyDefault"
    | "proxyBypass"
  >,
): string {
  return [
    config.baseUrl,
    config.apiFormat,
    config.credentialService,
    config.credentialUser,
    config.proxy,
    config.proxyDefault,
    ...config.proxyBypass,
    ...config.profiles.flatMap((profile) => [
      profile.id,
      profile.name,
      profile.baseUrl,
      profile.apiFormat,
      profile.credentialService,
      profile.credentialUser,
      profile.proxy,
    ]),
  ].join("\u{0}");
}

/** 缓存读出来可能是手改坏的、缺字段的旧结构：不合形就当没有 */
export function readCatalogCache(): CatalogCache | null {
  try {
    const parsed = JSON.parse(
      window.localStorage.getItem(CATALOG_CACHE_KEY) ?? "null",
    ) as CatalogCache | null;
    if (typeof parsed?.fingerprint !== "string" || !Array.isArray(parsed.entries)) return null;
    return parsed;
  } catch {
    return null;
  }
}

export function writeCatalogCache(cache: CatalogCache) {
  try {
    window.localStorage.setItem(CATALOG_CACHE_KEY, JSON.stringify(cache));
  } catch {
    // 存不下就算了：下次进页面顶多再拉一遍
  }
}

/**
 * 取目录：指纹一致直接用本地缓存（不碰任何服务商）；不一致才现拉并写回。
 * pool_catalog 本身是 async 命令（阻塞的网络请求在阻塞池里跑），
 * 这里的等待不占主线程——调用方拿 null 当"还在拉"画转圈就好
 */
export async function loadCatalog(config: AppConfig): Promise<PoolCatalogEntry[]> {
  const fingerprint = catalogFingerprint(config);
  const cached = readCatalogCache();
  if (cached && cached.fingerprint === fingerprint) return cached.entries;
  const entries = await fetchPoolCatalog();
  writeCatalogCache({ fingerprint, entries, savedAt: Date.now() });
  return entries;
}
