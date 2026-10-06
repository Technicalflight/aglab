import { useCallback, useEffect, useState } from "react";
import { IconRefresh as RefreshCw } from "@tabler/icons-react";
import { Group, Row, SettingsHeader, NumberField, inputClass } from "@/components/settings-ui";
import { Button } from "@/components/ui/button";
import { useChatStore } from "@/store/chat-store";
import { kbEmbedStatus, kbReembed, type KbEmbedStatus } from "@/lib/knowledge";

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

  const loadStatus = useCallback(() => {
    void kbEmbedStatus()
      .then(setStatus)
      .catch(() => setStatus(null));
  }, []);

  // 配置一变就重读状态：索引模型的比对对象就是刚保存的这份档
  useEffect(() => {
    loadStatus();
  }, [loadStatus, embedding.baseUrl, embedding.model, embedding.dimensions]);

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

  return (
    <>
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
                void updateConfig({ embedding: { ...embedding, baseUrl: next } });
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
            >
              <input
                type="text"
                spellCheck={false}
                defaultValue={embedding.model}
                placeholder="text-embedding-3-small"
                aria-label="Embedding 模型名"
                className={`${inputClass} w-72 font-mono`}
                onBlur={(event) => {
                  const next = event.target.value.trim();
                  if (next !== embedding.model) {
                    void updateConfig({ embedding: { ...embedding, model: next } });
                  }
                }}
                onKeyDown={(event) => {
                  if (event.key === "Enter") event.currentTarget.blur();
                }}
              />
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
                    void updateConfig({ embedding: { ...embedding, dimensions: value } });
                  }
                }}
              />
            </Row>
            <div className="border-b-0 px-1 pt-3 text-xs leading-5 text-muted-foreground">
              <p className="mb-1 font-medium text-amber-600 dark:text-amber-500">密钥从哪来</p>
              <p>
                请求带的是<span className="text-foreground">当前服务商档案的主密钥</span>
                ，不另设一格——中转站通常同一把钥匙同时代理对话与 embeddings 两个端点。走的是模型请求同一套代理与出口判定。
              </p>
            </div>
          </>
        ) : null}
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
    </>
  );
}
