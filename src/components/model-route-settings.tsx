import { useEffect, useState } from "react";

import {
  IconArrowDown as ArrowDown,
  IconArrowUp as ArrowUp,
  IconPlus as Plus,
  IconTrash as Trash2,
} from "@tabler/icons-react";

import { PaginationBar, usePaged } from "@/components/pagination";
import { cn } from "@/lib/utils";

import { Button } from "@/components/ui/button";
import { CapabilityToggle } from "@/components/ui/capability-toggle";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import type { ModelRoute } from "@/types/chat";
import { useChatStore } from "@/store/chat-store";
import { FormColumn } from "@/components/ui/content-column";

const inputClass =
  "h-9 rounded-lg border border-input bg-background px-3 text-base text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35";

/** 规则缺失了什么。返回 null = 这条规则是完整的 */
function routeProblem(rule: ModelRoute): string | null {
  if (!rule.pattern.trim()) return "要写匹配的模型名";
  if (!rule.endpointProfileId.trim() && !rule.model.trim()) return "这条规则没写去哪儿";
  return null;
}

/**
 * 设置页的「模型路由」项：按模型名匹配的路由表（design-model-routing.md）。
 * 生效档位在链上排第三：点名 > 模型池 > 路由表 > 设置直连——池子接管与点名
 * 都不经过这里；规则按列表顺序匹配，第一条命中即停，所以行可以上下移动。
 */
export function ModelRouteSettings() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const routes = config.modelRoutes;
  const pagedRoutes = usePaged(routes);

  const updateRoutes = (next: ModelRoute[]) => {
    void updateConfig({ modelRoutes: next });
  };

  const addRoute = () => {
    // 日期戳 + 序号：连点「加一条」也在同一个毫秒里撞不出重名
    const id = `route-${Date.now().toString(36)}-${routes.length}`;
    updateRoutes([...routes, { id, pattern: "", endpointProfileId: "", model: "", enabled: true }]);
  };

  const patchRoute = (id: string, patch: Partial<ModelRoute>) => {
    updateRoutes(routes.map((rule) => (rule.id === id ? { ...rule, ...patch } : rule)));
  };

  const removeRoute = (id: string) => {
    updateRoutes(routes.filter((rule) => rule.id !== id));
  };

  const moveRoute = (index: number, step: -1 | 1) => {
    const target = index + step;
    if (target < 0 || target >= routes.length) return;
    const next = [...routes];
    [next[index], next[target]] = [next[target], next[index]];
    updateRoutes(next);
  };

  return (
    <FormColumn>
      <h1 className="text-2xl font-semibold tracking-tight text-foreground">模型路由</h1>
      <p className="mt-1 text-sm leading-6 text-muted-foreground">
        1. 新建规则：填模型名（支持 * 通配）→ 要改去的服务商档案与模型名； 2.
        保存后，聊天里选中的模型命中规则时，这一发就改发过去； 3.
        子助理、编排、任务点名过模型或服务商的，以及被模型池接管的那几发不走路由。
      </p>
      <div className="mt-8">
        <div className="flex items-center justify-between">
          <h2 className="text-lg font-semibold tracking-tight text-foreground">
            规则（{routes.length}）
          </h2>
          <Button size="sm" variant="subtle" onClick={addRoute}>
            <Plus className="size-3.5" />
            加一条规则
          </Button>
        </div>
        <p className="mt-1 text-xs leading-5 text-muted-foreground">
          匹配名精确相等，或以 * 结尾做前缀匹配（gpt-4o* 接住它的变体）；单独一个 * 接住一切。
          模型名大小写敏感。指向已删除档案的规则按不命中处理，滑到下一条或直连。
        </p>
        {routes.length === 0 ? (
          <p className="mt-3 rounded-lg border border-dashed border-border px-3 py-6 text-center text-sm text-muted-foreground">
            空表 = 不路由，每一发照旧走设置直连。
          </p>
        ) : (
          <div className="mt-3 rounded-xl border border-border p-4">
            <div className="space-y-3">
              {pagedRoutes.slice.map((rule, index) => (
                <RouteRow
                  key={rule.id}
                  rule={rule}
                  index={index}
                  total={routes.length}
                  profiles={config.profiles.map((profile) => ({
                    id: profile.id,
                    name: profile.name,
                  }))}
                  onPatch={(patch) => patchRoute(rule.id, patch)}
                  onRemove={() => removeRoute(rule.id)}
                  onMove={(step) => moveRoute(index, step)}
                />
              ))}
            </div>
            <PaginationBar
              page={pagedRoutes.page}
              pages={pagedRoutes.pages}
              total={pagedRoutes.total}
              onPage={pagedRoutes.setPage}
            />
          </div>
        )}
      </div>
    </FormColumn>
  );
}

function RouteRow({
  rule,
  index,
  total,
  profiles,
  onPatch,
  onRemove,
  onMove,
}: {
  rule: ModelRoute;
  index: number;
  total: number;
  profiles: Array<{ id: string; name: string }>;
  onPatch: (patch: Partial<ModelRoute>) => void;
  onRemove: () => void;
  onMove: (step: -1 | 1) => void;
}) {
  const [confirmDelete, setConfirmDelete] = useState(false);

  // 两步确认删除：第一下只进入待确认态，3 秒内再点一下才真删
  useEffect(() => {
    if (!confirmDelete) return;
    const timer = setTimeout(() => setConfirmDelete(false), 3000);
    return () => clearTimeout(timer);
  }, [confirmDelete]);

  const problem = routeProblem(rule);
  const profileMissing =
    rule.endpointProfileId.trim() !== "" &&
    !profiles.some((profile) => profile.id === rule.endpointProfileId);

  return (
    <div className="space-y-1">
      <div className="flex items-start gap-2">
        <span
          aria-hidden
          className="mt-2 w-5 shrink-0 text-center font-mono text-xs text-muted-foreground/70"
        >
          {index + 1}
        </span>
        <div className="flex min-w-0 flex-1 flex-wrap items-center gap-2">
          <input
            type="text"
            value={rule.pattern}
            aria-label="匹配的模型名"
            spellCheck={false}
            placeholder="模型名，如 gpt-4o*"
            className={cn(
              inputClass,
              "w-[190px] shrink-0 font-mono text-sm",
              !rule.pattern.trim() && "border-destructive/60",
            )}
            onChange={(event) => onPatch({ pattern: event.target.value })}
          />
          <span aria-hidden className="shrink-0 text-xs text-muted-foreground">
            →
          </span>
          <Select
            value={rule.endpointProfileId || "__current__"}
            onValueChange={(value) =>
              onPatch({ endpointProfileId: value === "__current__" ? "" : value })
            }
          >
            <SelectTrigger className="w-[180px] shrink-0 text-sm">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="__current__">当前连接（不改服务商）</SelectItem>
              {profiles.map((profile) => (
                <SelectItem key={profile.id} value={profile.id} className="text-sm">
                  {profile.name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <input
            type="text"
            value={rule.model}
            aria-label="改成的模型名"
            spellCheck={false}
            placeholder="模型名留空 = 不改名"
            className={cn(inputClass, "w-[190px] shrink-0 font-mono text-sm")}
            onChange={(event) => onPatch({ model: event.target.value })}
          />
        </div>
        <div className="flex shrink-0 items-center gap-1">
          <Button
            variant="subtle"
            size="icon"
            aria-label={`上移规则 ${index + 1}`}
            disabled={index === 0}
            onClick={() => onMove(-1)}
          >
            <ArrowUp className="size-3.5" />
          </Button>
          <Button
            variant="subtle"
            size="icon"
            aria-label={`下移规则 ${index + 1}`}
            disabled={index === total - 1}
            onClick={() => onMove(1)}
          >
            <ArrowDown className="size-3.5" />
          </Button>
          <CapabilityToggle
            label={`启用规则 ${index + 1}`}
            enabled={rule.enabled}
            onToggle={() => onPatch({ enabled: !rule.enabled })}
          />
          {confirmDelete ? (
            <Button
              variant="subtle"
              size="icon"
              aria-label={`再点一次确认删除规则 ${index + 1}`}
              className="text-destructive"
              onClick={onRemove}
            >
              <Trash2 className="size-3.5" />
            </Button>
          ) : (
            <Button
              variant="subtle"
              size="icon"
              aria-label={`删除规则 ${index + 1}`}
              onClick={() => setConfirmDelete(true)}
            >
              <Trash2 className="size-3.5" />
            </Button>
          )}
        </div>
      </div>
      {problem || profileMissing ? (
        <p className="pl-7 text-xs text-destructive">
          {problem ??
            "指向的档案已经不存在了：这条规则按不命中处理，去「服务商档案」重建或换一张。"}
        </p>
      ) : null}
    </div>
  );
}
