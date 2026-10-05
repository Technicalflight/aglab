import { useState } from "react";
import { IconRefresh as RefreshCw } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Pager } from "@/components/ui/pager";
import { formatUsd } from "@/lib/format";
import { pageSlice } from "@/lib/pagination";
import { useChatStore } from "@/store/chat-store";
import type { ModelPrice } from "@/types/chat";

const inputClass =
  "h-9 w-full rounded-lg border border-input bg-background px-3 text-base text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35";

/** 一屏多少条单价。cc-switch 那份价表一键就是上百条，全渲染要把这一页拉成长龙 */
const PRICE_PAGE_SIZE = 20;

/** 四类单价的输入框标签。顺序就是弹层里填写的顺序 */
const PRICE_FIELDS: Array<{ key: keyof Pick<ModelPrice, "inputUsdPerM" | "outputUsdPerM" | "cacheReadUsdPerM" | "cacheCreationUsdPerM">; label: string }> = [
  { key: "inputUsdPerM", label: "输入 / 百万 tokens" },
  { key: "outputUsdPerM", label: "输出 / 百万 tokens" },
  { key: "cacheReadUsdPerM", label: "缓存读 / 百万 tokens" },
  { key: "cacheCreationUsdPerM", label: "缓存写 / 百万 tokens" },
];

function emptyPrice(): ModelPrice {
  return {
    modelId: "",
    displayName: "",
    inputUsdPerM: "0",
    outputUsdPerM: "0",
    cacheReadUsdPerM: "0",
    cacheCreationUsdPerM: "0",
  };
}

function toUsdText(raw: string): string {
  const value = Number(raw);
  return Number.isFinite(value) && value > 0 ? formatUsd(value) : "—";
}

export function PricingBlock() {
  const prices = useChatStore((s) => s.prices);
  const pricesError = useChatStore((s) => s.pricesError);
  const pricesBusy = useChatStore((s) => s.pricesBusy);
  const pricesNote = useChatStore((s) => s.pricesNote);
  const loadPrices = useChatStore((s) => s.loadPrices);
  const upsertPrice = useChatStore((s) => s.upsertPrice);
  const removePrice = useChatStore((s) => s.removePrice);
  const importPricing = useChatStore((s) => s.importPricing);

  const [draft, setDraft] = useState<ModelPrice | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [confirmRemove, setConfirmRemove] = useState<string | null>(null);
  const [page, setPage] = useState(0);
  // 分页只是这一格的显示方式：价表的权威在后端，翻来翻去不改它一个字节
  const visible = pageSlice(prices, page, PRICE_PAGE_SIZE);

  function openDraft(price: ModelPrice | null) {
    setFormError(null);
    setDraft(price ? { ...price } : emptyPrice());
  }

  async function submitDraft() {
    if (!draft) return;
    const modelId = draft.modelId.trim();
    if (!modelId) {
      setFormError("模型名不能为空。");
      return;
    }
    // 前端先拦一道：负数和非数字都是填错，别等后端退回来
    for (const field of PRICE_FIELDS) {
      const raw = draft[field.key].trim();
      if (raw === "" || !Number.isFinite(Number(raw)) || Number(raw) < 0) {
        setFormError(`${field.label}的单价得是不小于 0 的数字。`);
        return;
      }
    }
    const error = await upsertPrice({
      ...draft,
      modelId,
      displayName: draft.displayName.trim() || modelId,
    });
    if (error) {
      setFormError(error);
      return;
    }
    setDraft(null);
  }

  return (
    <div>
      <div className="flex items-center gap-2 border-t border-border pt-5">
        <h2 className="min-w-0 flex-1 truncate text-base font-semibold tracking-tight text-foreground">
          价格表
        </h2>
        <Button
          variant="subtle"
          size="sm"
          disabled={pricesBusy}
          onClick={() => void importPricing()}
        >
          从 cc-switch 导入价表
        </Button>
        <Button variant="subtle" size="sm" disabled={pricesBusy} onClick={() => openDraft(null)}>
          新增单价
        </Button>
        <Button
          variant="ghost"
          size="icon"
          aria-label="重新读取价格表"
          disabled={pricesBusy}
          onClick={() => void loadPrices()}
        >
          <RefreshCw className="size-3.5" />
        </Button>
      </div>

      <p className="mt-1 text-sm leading-6 text-muted-foreground">
        每百万 token 多少美元。台账按这张表把 token 折成钱——
        模型没进表就按 $0 记账并在上方单独报出来。装了 cc-switch 的话，一键导入它攒好的价表最省事。
      </p>

      {pricesError ? (
        <p className="mt-2 text-xs text-destructive">{pricesError}</p>
      ) : null}
      {pricesNote ? <p className="mt-2 text-xs text-brand-text">{pricesNote}</p> : null}

      {prices.length === 0 ? (
        <p className="mt-3 text-sm text-muted-foreground">
          价格表还是空的。点「从 cc-switch 导入价表」，或者自己填一条。
        </p>
      ) : (
        <div className="mt-3 overflow-hidden rounded-lg border border-border bg-surface">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-border text-left text-xs text-muted-foreground">
                <th className="px-3 py-2 font-normal">模型</th>
                <th className="px-3 py-2 text-right font-normal">输入</th>
                <th className="px-3 py-2 text-right font-normal">输出</th>
                <th className="px-3 py-2 text-right font-normal">缓存读</th>
                <th className="px-3 py-2 text-right font-normal">缓存写</th>
                <th className="px-3 py-2 text-right font-normal">操作</th>
              </tr>
            </thead>
            <tbody>
              {visible.map((price) => (
                <tr key={price.modelId} className="border-b border-border/60 last:border-b-0">
                  <td className="max-w-[200px] truncate px-3 py-2.5" title={price.modelId}>
                    {price.displayName || price.modelId}
                  </td>
                  <td className="px-3 py-2.5 text-right">{toUsdText(price.inputUsdPerM)}</td>
                  <td className="px-3 py-2.5 text-right">{toUsdText(price.outputUsdPerM)}</td>
                  <td className="px-3 py-2.5 text-right">{toUsdText(price.cacheReadUsdPerM)}</td>
                  <td className="px-3 py-2.5 text-right">
                    {toUsdText(price.cacheCreationUsdPerM)}
                  </td>
                  <td className="whitespace-nowrap px-3 py-2.5 text-right">
                    <Button
                      variant="ghost"
                      size="sm"
                      className="h-7 text-xs"
                      onClick={() => openDraft(price)}
                    >
                      编辑
                    </Button>
                    {confirmRemove === price.modelId ? (
                      <>
                        <Button
                          variant="ghost"
                          size="sm"
                          className="h-7 text-xs text-destructive hover:bg-destructive/15 hover:text-destructive"
                          onClick={() => {
                            setConfirmRemove(null);
                            void removePrice(price.modelId);
                          }}
                        >
                          确认删
                        </Button>
                        <Button
                          variant="ghost"
                          size="sm"
                          className="h-7 text-xs"
                          onClick={() => setConfirmRemove(null)}
                        >
                          算了
                        </Button>
                      </>
                    ) : (
                      <Button
                        variant="ghost"
                        size="sm"
                        className="h-7 text-xs"
                        onClick={() => setConfirmRemove(price.modelId)}
                      >
                        删除
                      </Button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {/* 装得下时 Pager 自己不出场，这里就不另摆一个条件 */}
      <Pager page={page} total={prices.length} size={PRICE_PAGE_SIZE} onPage={setPage} />

      <Dialog
        open={draft !== null}
        onOpenChange={(open) => {
          if (!open) setDraft(null);
        }}
      >
        <DialogContent className="max-w-md">
          <DialogTitle>{draft?.modelId ? "编辑单价" : "新增单价"}</DialogTitle>
          {draft ? (
            <div className="mt-3 space-y-3">
              <label className="block">
                <span className="mb-1.5 block text-xs text-muted-foreground">
                  模型名（按台账里的写法匹配，忽略大小写）
                </span>
                <input
                  type="text"
                  value={draft.modelId}
                  placeholder="glm-5.3-flash"
                  spellCheck={false}
                  className={inputClass}
                  onChange={(event) => setDraft({ ...draft, modelId: event.target.value })}
                />
              </label>
              <label className="block">
                <span className="mb-1.5 block text-xs text-muted-foreground">
                  显示名（可留空）
                </span>
                <input
                  type="text"
                  value={draft.displayName}
                  spellCheck={false}
                  className={inputClass}
                  onChange={(event) => setDraft({ ...draft, displayName: event.target.value })}
                />
              </label>
              {PRICE_FIELDS.map((field) => (
                <label key={field.key} className="block">
                  <span className="mb-1.5 block text-xs text-muted-foreground">
                    {field.label}（美元）
                  </span>
                  <input
                    type="text"
                    inputMode="decimal"
                    value={draft[field.key]}
                    spellCheck={false}
                    className={inputClass}
                    onChange={(event) =>
                      setDraft({ ...draft, [field.key]: event.target.value } as ModelPrice)
                    }
                  />
                </label>
              ))}
              {formError ? (
                <p className="text-xs leading-5 text-destructive">{formError}</p>
              ) : null}
              <div className="flex justify-end gap-2 pt-1">
                <Button variant="ghost" size="sm" onClick={() => setDraft(null)}>
                  取消
                </Button>
                <Button variant="brand" size="sm" disabled={pricesBusy} onClick={() => void submitDraft()}>
                  保存
                </Button>
              </div>
            </div>
          ) : null}
        </DialogContent>
      </Dialog>
    </div>
  );
}
