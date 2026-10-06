import { useCallback, useEffect, useState } from "react";
import { IconDownload as Download, IconRefresh as RefreshCw, IconUpload as Upload } from "@tabler/icons-react";
import { Group, Row, SettingsHeader, NumberField, inputClass } from "@/components/settings-ui";
import { Button } from "@/components/ui/button";
import { FormColumn } from "@/components/ui/content-column";
import { useChatStore } from "@/store/chat-store";
import {
  kbEmbedStatus,
  kbReembed,
  embeddingModels,
  embeddingCredentialSet,
  embeddingCredentialClear,
  embeddingCredentialProbe,
  ocrEngineStatus,
  ocrEngineStart,
  ocrEngineDownload,
  type KbEmbedStatus,
  type OcrEngineStatus,
} from "@/lib/knowledge";

/**
 * 设置页的「资料库检索」项：资料库语义索引的 embedding 端点。
 * 关键词检索永远可用；这里配了端点才会在入库时切块建向量索引，
 * 检索时关键词与向量两路 RRF 融合。密钥沿用当前服务商档案的主密钥
 * （中转站同一把钥匙通常同时代理 /chat/completions 与 /embeddings）。
 */
export function EmbeddingSettings() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const pushToast = useChatStore((s) => s.pushToast);
  const embedding = config.embedding;
  const [status, setStatus] = useState<KbEmbedStatus | null>(null);
  const [rebuilding, setRebuilding] = useState(false);

  // 端点模型目录：null = 还没拉过；换端点后作废重拉
  const [endpointModels, setEndpointModels] = useState<string[] | null>(null);
  const [fetchingModels, setFetchingModels] = useState(false);
  const [modelListOpen, setModelListOpen] = useState(false);
  // 模型名草稿：打字不过库，失焦才落——每键一次 config_patch 太重
  const [modelDraft, setModelDraft] = useState(embedding.model);
  useEffect(() => setModelDraft(embedding.model), [embedding.model]);
  const [rerankDraft, setRerankDraft] = useState(embedding.rerankModel ?? "");
  useEffect(() => setRerankDraft(embedding.rerankModel ?? ""), [embedding.rerankModel]);

  // 专用密钥：只写不读回。probe 只答在不在，输入框永远空着
  const [hasDedicatedKey, setHasDedicatedKey] = useState(false);
  const [keySaving, setKeySaving] = useState(false);
  useEffect(() => {
    void embeddingCredentialProbe()
      .then(setHasDedicatedKey)
      .catch(() => setHasDedicatedKey(false));
  }, []);

  // OCR 引擎状态：进页探一次，下载/启动后再探
  const [engine, setEngine] = useState<OcrEngineStatus | null>(null);
  const [engineBusy, setEngineBusy] = useState(false);
  const loadEngine = useCallback(() => {
    void ocrEngineStatus()
      .then(setEngine)
      .catch(() => setEngine(null));
  }, []);
  useEffect(() => {
    loadEngine();
  }, [loadEngine]);

  async function engineAction(action: "download" | "start") {
    if (engineBusy) return;
    setEngineBusy(true);
    try {
      if (action === "download") {
        await ocrEngineDownload();
        pushToast({ tone: "info", title: "引擎装好了", detail: "Umi-OCR 已解压到应用数据目录。" });
      } else {
        await ocrEngineStart();
        pushToast({ tone: "info", title: "引擎已启动", detail: "Umi-OCR 在后台托盘运行，服务端口 1224。" });
      }
    } catch (error) {
      pushToast({ tone: "error", title: action === "download" ? "下载没完成" : "启动失败", detail: String(error) });
    } finally {
      setEngineBusy(false);
      loadEngine();
    }
  }

  async function saveKey(event: React.FocusEvent<HTMLInputElement>) {
    const secret = event.target.value.trim();
    if (!secret || keySaving) return;
    setKeySaving(true);
    try {
      await embeddingCredentialSet(secret);
      setHasDedicatedKey(true);
      pushToast({ tone: "info", title: "专用密钥已保存", detail: "语义检索的请求从此带这把钥匙。" });
    } catch (error) {
      pushToast({ tone: "error", title: "密钥没存上", detail: String(error) });
    } finally {
      event.target.value = "";
      setKeySaving(false);
    }
  }

  async function clearKey() {
    try {
      await embeddingCredentialClear();
      setHasDedicatedKey(false);
      pushToast({ tone: "info", title: "已清除专用密钥", detail: "语义检索回到沿用当前服务商档案的主密钥。" });
    } catch (error) {
      pushToast({ tone: "error", title: "清除失败", detail: String(error) });
    }
  }

  const loadStatus = useCallback(() => {
    void kbEmbedStatus()
      .then(setStatus)
      .catch(() => setStatus(null));
  }, []);

  // 配置一变就重读状态：索引模型的比对对象就是刚保存的这份档
  useEffect(() => {
    loadStatus();
  }, [loadStatus, embedding.baseUrl, embedding.model, embedding.dimensions]);

  const commit = (next: Partial<typeof embedding>) => void updateConfig({ embedding: { ...embedding, ...next } });

  async function fetchModels() {
    if (fetchingModels) return;
    const base = embedding.baseUrl.trim();
    if (!base) {
      pushToast({ tone: "error", title: "先填端点基址", detail: "模型列表从端点的 /models 接口拉取。" });
      return;
    }
    setFetchingModels(true);
    try {
      const names = await embeddingModels(base);
      setEndpointModels(names);
      setModelListOpen(true);
      pushToast({
        tone: "info",
        title: names.length > 0 ? `端点返回 ${names.length} 个模型` : "端点没返回任何模型",
        detail: names.length > 0 ? "在模型一栏的下拉里挑 embedding 系列。" : undefined,
      });
    } catch (error) {
      setEndpointModels(null);
      pushToast({ tone: "error", title: "模型列表没拉到", detail: String(error) });
    } finally {
      setFetchingModels(false);
    }
  }

  async function reembed() {
    if (rebuilding) return;
    setRebuilding(true);
    try {
      await kbReembed();
      pushToast({ tone: "info", title: "重建索引已开始", detail: "全部文档正在后台重新切块嵌入。" });
    } catch (error) {
      pushToast({ tone: "error", title: "重建没跑起来", detail: String(error) });
    } finally {
      setRebuilding(false);
      loadStatus();
    }
  }

  // 建议列表：拉到的目录按输入过滤（大小写不敏感），已精确输入的不再提示
  const suggestions = (endpointModels ?? [])
    .filter((name) => name.toLowerCase() !== modelDraft.trim().toLowerCase())
    .filter((name) => modelDraft.trim() === "" || name.toLowerCase().includes(modelDraft.trim().toLowerCase()));

  return (
    <FormColumn>
      <SettingsHeader
        title="资料库检索"
        description="资料库默认按关键词检索；配一个 OpenAI 兼容的 /embeddings 端点后，入库文档会自动切块建向量索引，检索时两路结果融合排序。"
      />

      <Group title="语义索引（可选）">
        <Row
          wide
          title="端点基址"
          description="OpenAI 兼容 /embeddings 端点的基址（如 https://relay.example.com/v1）。留空 = 未启用，资料库保持纯关键词检索。"
        >
          <input
            type="text"
            spellCheck={false}
            defaultValue={embedding.baseUrl}
            placeholder="https://relay.example.com/v1"
            aria-label="Embedding 端点基址"
            className={`${inputClass} w-72 font-mono`}
            onBlur={(event) => {
              const next = event.target.value.trim();
              if (next !== embedding.baseUrl) {
                setEndpointModels(null);
                commit({ baseUrl: next });
              }
            }}
            onKeyDown={(event) => {
              if (event.key === "Enter") event.currentTarget.blur();
            }}
          />
        </Row>
        {embedding.baseUrl ? (
          <>
            <Row
              wide
              title="Embedding 模型"
              description="如 text-embedding-3-small / bge-m3。换了模型必须点下面的「重建索引」——不同模型的向量不能混在一个库里比余弦。"
              note={
                endpointModels ? (
                  <p className="mt-1 text-xs leading-5 text-muted-foreground">
                    已拉到 {endpointModels.length} 个模型（端点 /models 的全量目录，含对话模型——挑 embedding 系列的用）。
                  </p>
                ) : null
              }
            >
              <div className="relative flex items-center gap-2">
                <input
                  type="text"
                  spellCheck={false}
                  value={modelDraft}
                  placeholder="text-embedding-3-small"
                  aria-label="Embedding 模型名"
                  className={`${inputClass} w-60 font-mono`}
                  onChange={(event) => {
                    setModelDraft(event.target.value);
                    setModelListOpen(true);
                  }}
                  onFocus={() => setModelListOpen(true)}
                  onBlur={() => {
                    setModelListOpen(false);
                    const next = modelDraft.trim();
                    if (next !== embedding.model) commit({ model: next });
                  }}
                  onKeyDown={(event) => {
                    if (event.key === "Escape") setModelListOpen(false);
                  }}
                />
                <Button
                  variant="subtle"
                  size="sm"
                  disabled={fetchingModels}
                  onClick={() => void fetchModels()}
                >
                  <Download className="size-3.5" />
                  {fetchingModels ? "拉取中…" : "拉取模型列表"}
                </Button>
                {modelListOpen && suggestions.length > 0 ? (
                  <ul
                    className="absolute top-full right-0 left-0 z-30 mt-1 max-h-56 overflow-y-auto rounded-lg border border-border bg-background py-1 shadow-lg"
                    role="listbox"
                  >
                    {suggestions.map((name) => (
                      <li key={name}>
                        <button
                          type="button"
                          role="option"
                          aria-selected={name === modelDraft}
                          // mousedown 先于 input 的 blur：按下时先把值填上，blur 再关面板
                          onMouseDown={(event) => {
                            event.preventDefault();
                            commit({ model: name });
                            setModelListOpen(false);
                          }}
                          className="w-full px-3 py-1.5 text-left font-mono text-sm text-foreground transition-colors hover:bg-accent"
                        >
                          {name}
                        </button>
                      </li>
                    ))}
                  </ul>
                ) : null}
              </div>
            </Row>
            <Row
              title="向量维度"
              description="0 = 首次嵌入时从响应自动探测并记下；之后必须一致，换模型后请清零让它重新探测。"
            >
              <NumberField
                label="向量维度"
                value={embedding.dimensions}
                min={0}
                max={65_536}
                onCommit={(value) => {
                  if (value !== embedding.dimensions) {
                    commit({ dimensions: value });
                  }
                }}
              />
            </Row>
            <Row
              wide
              title="Rerank 精排模型（可选）"
              description="如 bge-reranker-v2-m3。配了之后，检索先融合出 4 倍候选，再让端点的 /rerank 精排取回最优的几条；端点不通时自动回落融合序。与 embedding 同端点同密钥。"
            >
              <input
                type="text"
                spellCheck={false}
                value={rerankDraft}
                placeholder="留空 = 不精排"
                aria-label="Rerank 精排模型名"
                className={`${inputClass} w-72 font-mono`}
                onChange={(event) => setRerankDraft(event.target.value)}
                onBlur={() => {
                  const next = rerankDraft.trim();
                  if (next !== (embedding.rerankModel ?? "")) commit({ rerankModel: next });
                }}
                onKeyDown={(event) => {
                  if (event.key === "Enter") event.currentTarget.blur();
                }}
              />
            </Row>
            <Row
              wide
              title="API 密钥（专用）"
              description="存进独立凭据条目（default.aglab/embedding），与档案密钥互不牵连——换档案、换主密钥都不影响语义检索。留空不动已保存的专用密钥。"
              note={
                hasDedicatedKey ? (
                  <div className="mt-2 flex items-center gap-2">
                    <span className="text-xs text-emerald-600 dark:text-emerald-500">
                      已设置专用密钥（内容不回显）
                    </span>
                    <Button variant="subtle" size="sm" onClick={() => void clearKey()}>
                      改用主密钥
                    </Button>
                  </div>
                ) : (
                  <p className="mt-1 text-xs leading-5 text-muted-foreground">
                    没设专用密钥时，请求沿用<span className="text-foreground">当前服务商档案的主密钥</span>
                    ——中转站一把钥匙开 chat 与 embeddings 两个端点是常态。密钥只进 Windows 凭据管理器，不进配置文件。
                  </p>
                )
              }
            >
              <input
                type="password"
                spellCheck={false}
                autoComplete="off"
                placeholder={hasDedicatedKey ? "输入新值覆盖" : "sk-…"}
                aria-label="Embedding 专用 API 密钥"
                className={`${inputClass} w-72 font-mono`}
                onBlur={(event) => void saveKey(event)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") event.currentTarget.blur();
                }}
              />
            </Row>
          </>
        ) : null}
      </Group>

      <Group title="OCR 引擎（Umi-OCR）">
        <Row
          wide
          title="OCR 服务地址"
          description="Umi-OCR 的 HTTP 服务地址，默认本机 127.0.0.1:1224（软件默认开启，仅本地环回）。资料库导入 PDF 与图片时用它提取文字。"
        >
          <input
            type="text"
            spellCheck={false}
            defaultValue={config.ocr.baseUrl}
            placeholder="http://127.0.0.1:1224"
            aria-label="OCR 服务地址"
            className={`${inputClass} w-72 font-mono`}
            onBlur={(event) => {
              const next = event.target.value.trim();
              if (next !== config.ocr.baseUrl) {
                void updateConfig({ ocr: { baseUrl: next } });
              }
            }}
            onKeyDown={(event) => {
              if (event.key === "Enter") event.currentTarget.blur();
            }}
          />
        </Row>
        <Row
          wide
          title="内置引擎"
          description={
            engine?.running
              ? `运行中 · ${engine.version}。导入 PDF/图片会自动调它。`
              : engine?.installed
                ? "已安装未运行。启动后它在后台托盘待命，导入时自动调用。"
                : "未安装。下载官方 Paddle 引擎整合包（约 134MB）到应用数据目录，导入 PDF/图片前启动一次即可。"
          }
          note={
            <p className="mt-1 text-xs leading-5 text-muted-foreground">
              引擎包来自 hiroi-sora/Umi-OCR 官方发布（AGPL-3.0，与本项目同协议），下载走 gh-proxy 镜像、不需要代理。
            </p>
          }
        >
          <div className="flex items-center gap-2">
            {!engine?.installed ? (
              <Button variant="outline" size="sm" disabled={engineBusy || engine?.downloading} onClick={() => void engineAction("download")}>
                <Download className={engineBusy ? "size-3.5 animate-spin" : "size-3.5"} />
                {engineBusy || engine?.downloading ? "下载安装中…" : "下载内置引擎"}
              </Button>
            ) : null}
            {engine?.installed && !engine.running ? (
              <Button variant="outline" size="sm" disabled={engineBusy} onClick={() => void engineAction("start")}>
                <Upload className="size-3.5" />
                启动引擎
              </Button>
            ) : null}
            {engine?.running ? <span className="text-sm text-emerald-600 dark:text-emerald-500">就绪</span> : null}
          </div>
        </Row>
      </Group>

      <Group title="索引状态">
        {!status || !status.enabled ? (
          <div className="px-1 py-3 text-sm leading-6 text-muted-foreground">
            {status && status.chunks > 0
              ? `已有 ${status.chunks} 段历史向量（旧模型：${status.indexedModel || "未知"}）。填好上面的端点与模型后点重建即可启用。`
              : "未启用。填好端点与模型后，新入库的文档会自动切块嵌入，无需手动操作。"}
          </div>
        ) : (
          <>
            <Row
              title="已索引段数"
              description={`向量库里的切块总数（索引模型：${status.indexedModel || "还没有任何向量"}）。`}
            >
              <span className="text-base tabular-nums text-foreground">{status.chunks}</span>
            </Row>
            <Row
              title="与当前模型一致"
              description={
                status.stale
                  ? "向量库是空的，或里面的向量出自别的模型——检索会自动退回纯关键词，重建后恢复语义路。"
                  : "向量库与上面配置的模型一致，语义检索随每次入库自动更新。"
              }
            >
              {status.stale ? (
                <Button variant="outline" size="sm" disabled={rebuilding} onClick={() => void reembed()}>
                  <RefreshCw className={rebuilding ? "size-3.5 animate-spin" : "size-3.5"} />
                  {rebuilding ? "重建中…" : "重建索引"}
                </Button>
              ) : (
                <span className="text-sm text-emerald-600 dark:text-emerald-500">一致</span>
              )}
            </Row>
          </>
        )}
      </Group>
    </FormColumn>
  );
}
