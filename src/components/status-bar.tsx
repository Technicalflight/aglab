import { useEffect, useRef, useState } from "react";
import { IconChevronDown as ChevronDown, IconGitBranch as GitBranch } from "@tabler/icons-react";

import { ContextUsageRing } from "@/components/context-usage-ring";
import { ProjectPicker } from "@/components/project-picker";
import { Menu, MenuContent, MenuItem, MenuLabel, MenuTrigger } from "@/components/ui/menu";
import { useChatStore } from "@/store/chat-store";
import { formatTokensCompact } from "@/lib/format";
import { cn } from "@/lib/utils";

/** base_url 只露主机名：一行里放不下整个 URL，完整地址悬停可见 */
function hostOf(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

/**
 * 输入框下方的详情摘要（原右栏「详情」格的精选，与工作目录选择同一行）。
 * 挑的标准是"别处看不见的"：模型与思考程度在底栏选择器上、权限在权限选择器上、
 * 工作目录在左边——这里放的是服务商、密钥状态、消息数和上一轮的账（tokens 与耗时）。
 */
function DetailStrip() {
  const config = useChatStore((s) => s.config);
  const hasKey = useChatStore((s) => s.hasKey);
  // 只用得到条数：订阅长度而不是数组——流式期间数组引用每 60ms 换一次，
  // 挂大数组等于给底栏加一笔白拿的重渲税
  const messageCount = useChatStore((s) => s.messages.length);
  const usage = useChatStore((s) => s.usage);

  const items: Array<{ label: string; value: string; warn?: boolean; title?: string }> = [
    {
      label: "服务商",
      value: hostOf(config.baseUrl) || "未配置",
      warn: !config.baseUrl,
      title: config.baseUrl || undefined,
    },
    { label: "密钥", value: hasKey ? "已就绪" : "未找到", warn: !hasKey },
    { label: "消息", value: String(messageCount) },
    {
      // 输入与输出分开报：input 才是"上下文"，output 是这一轮的生成量——
      // 相加的那个数会把两者混在一起，看起来就像和缓存命中对不上
      label: "上轮",
      value: usage
        ? `${formatTokensCompact(usage.inputTokens)} → ${formatTokensCompact(usage.outputTokens)}`
        : "—",
      title: !usage
        ? undefined
        : usage.cachedTokens === null
          ? "服务商没回缓存字段，命中量不可知（不是 0）"
          : `输入 ${usage.inputTokens}（其中命中缓存 ${usage.cachedTokens}）· 输出 ${usage.outputTokens} tokens`,
    },
    { label: "耗时", value: usage ? `${(usage.durationMs / 1000).toFixed(1)}s` : "—" },
  ];

  return (
    <div className="flex min-w-0 items-center text-xs">
      {items.map((item, index) => (
        <span
          key={item.label}
          className={cn(
            "flex min-w-0 items-center gap-1",
            index > 0 && "ml-3 border-l border-border pl-3",
          )}
        >
          <span className="shrink-0 text-foreground-tertiary">{item.label}</span>
          <span
            title={item.title}
            className={cn(
              "truncate tabular-nums",
              item.warn ? "text-destructive" : "text-foreground-secondary",
            )}
          >
            {item.value}
          </span>
        </span>
      ))}
    </div>
  );
}

/**
 * Worktree 分支选择与开关（从输入框工具条迁来）。
 *
 * 这两件是"这一场话题的文件落在哪棵树"的**场地信息**，与状态栏是同一类读数——
 * 长在这里比挤在输入框工具条合适；尤其沉浸式下工具条只剩会话动作，分支与
 * 开关留在原地反而打断"悬浮输入卡"的形态。
 *
 * 绑定后这一场话题的文件工具全落在独立工作树上，原工作目录一个字节不动；
 * 摘树时分支与提交保留，脏树要点两次（第二次强制）。
 */
function WorktreeControl() {
  const activeId = useChatStore((s) => s.activeId);
  const activeProjectId = useChatStore((s) => s.config.activeProjectId);
  const gitBranches = useChatStore((s) => s.gitBranches);
  const bound = useChatStore((s) => s.worktrees[activeId] ?? null);
  const attachWorktree = useChatStore((s) => s.attachWorktree);
  const detachWorktree = useChatStore((s) => s.detachWorktree);
  const refreshWorktreeBranches = useChatStore((s) => s.refreshWorktreeBranches);
  const pushToast = useChatStore((s) => s.pushToast);
  const [picked, setPicked] = useState("");
  const forceRef = useRef(false);

  // 话题切换或工作目录切换后分支清单都要重取：清单跟"这场话题生效的仓库"走
  // （话题绑定项目优先，散对话回落激活项目）；绑定态由 refreshWorktree 在切话题时读
  useEffect(() => {
    void refreshWorktreeBranches();
  }, [activeId, activeProjectId, refreshWorktreeBranches]);

  // 没绑 git 工作目录（或压根没绑工作目录）时这两个控件没有意义：整组隐藏，别摆死按钮
  if (!activeId || !gitBranches?.isRepo) return null;

  // unborn 分支：选择器上显示的名字只是 HEAD 的意向（init 后没提交过），
  // 如实标注"未提交"，不然勾上 Worktree 只会吃到一句莫名的报错。
  // 后缀刻意短：这条栏上每个字都在挤同一行
  const branchLabel = bound
    ? bound.branch
    : gitBranches.unborn
      ? `${gitBranches.current || "HEAD"}（未提交）`
      : picked || gitBranches.current || "选分支";

  async function toggle() {
    if (!activeId) return;
    if (bound) {
      const error = await detachWorktree(activeId, forceRef.current);
      if (error) {
        // 第一次点通常是因为树上有未提交的改动：把"再点一次 = 强制"告知到位
        forceRef.current = true;
        pushToast({ tone: "error", title: "Worktree 没有摘掉", detail: error });
      } else {
        forceRef.current = false;
      }
      return;
    }
    const error = await attachWorktree(activeId, picked || undefined);
    if (error) pushToast({ tone: "error", title: "Worktree 没有挂上", detail: error });
  }

  return (
    // 与左侧 ProjectPicker 之间一道细分隔线：两件事（在哪干活 / 落在哪棵树）并排读
    <div className="ml-1 flex items-center gap-0.5 border-l border-border pl-2">
      {/* 打开菜单的瞬间重取一次分支读数：label 可能已经过期（比如 AI 刚提交完），
          但菜单里的选项必须是真的——过期账本不能当操作依据 */}
      <Menu onOpenChange={(open) => open && void refreshWorktreeBranches()}>
        <MenuTrigger
          type="button"
          aria-label="选择 Worktree 基于的分支"
          disabled={!!bound}
          title={
            bound
              ? `分支已由 Worktree 定为「${bound.branch}」（基于「${bound.baseBranch}」）`
              : "选择 Worktree 基于哪个分支"
          }
          className="flex h-7 w-max max-w-44 shrink-0 items-center gap-1 rounded-lg px-1.5 text-xs text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45 disabled:pointer-events-none"
        >
          <GitBranch className="size-3.5 shrink-0" />
          <span className="min-w-0 truncate">{branchLabel}</span>
          {bound ? null : <ChevronDown className="size-3 shrink-0" />}
        </MenuTrigger>
        <MenuContent className="max-h-64 overflow-y-auto">
          <MenuLabel>基于哪个分支</MenuLabel>
          {gitBranches.branches.length === 0 ? (
            <MenuItem disabled>
              {gitBranches.unborn
                ? "仓库还没有任何提交——先提交一次，再来开 Worktree"
                : "这个仓库还没有分支"}
            </MenuItem>
          ) : (
            gitBranches.branches.map((name) => (
              <MenuItem key={name} onSelect={() => setPicked(name)}>
                {name === gitBranches.current ? `${name}（当前）` : name}
              </MenuItem>
            ))
          )}
        </MenuContent>
      </Menu>

      <button
        type="button"
        aria-pressed={!!bound}
        onClick={() => void toggle()}
        title={
          bound
            ? `本话题运行在独立工作树上：\n${bound.dir}\n分支 ${bound.branch}（未提交改动 ${bound.changedFiles} 个文件）。点击摘除工作树，分支与提交都保留。`
            : "启用后，aglab 会基于所选分支为本次话题创建独立工作目录，避免直接修改原工作目录中的文件。"
        }
        className={cn(
          "flex h-7 shrink-0 items-center gap-1.5 whitespace-nowrap rounded-lg px-1.5 text-xs outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
          bound ? "text-foreground" : "text-muted-foreground hover:bg-accent hover:text-foreground",
        )}
      >
        {/* 开关（设计基准）：轨道亮起即绑定，旋钮滑到右端；与状态栏的小字同一密度 */}
        <span
          aria-hidden
          className={cn(
            "relative h-[16px] w-7 shrink-0 rounded-full transition-colors",
            bound ? "bg-brand" : "bg-sunken ring-1 ring-inset ring-border",
          )}
        >
          <span
            className={cn(
              "absolute top-1/2 size-3 -translate-y-1/2 rounded-full bg-elevated shadow-sm transition-[left] duration-[var(--dur-fast)]",
              bound ? "left-[11px]" : "left-[2px]",
            )}
          />
        </span>
        <span>Worktree</span>
      </button>
    </div>
  );
}

/**
 * 全窗口底部状态栏（设计基准）：左侧工作目录 + Worktree 分支/开关，
 * 右侧上下文用量 + 服务商/密钥/消息/上轮/耗时。
 * Worktree 是对话轮机制：只在对话分区、对话档会话出现；非 git 仓库时控件自隐藏。
 */
export function StatusBar() {
  const section = useChatStore((s) => s.section);
  const kind = useChatStore((s) => s.kind);
  const showWorktree = section === "chats" && (kind ?? "chat") === "chat";

  return (
    <footer className="flex h-9 shrink-0 items-center justify-between gap-3 px-3 pb-0.5">
      <div className="flex min-w-0 items-center">
        <ProjectPicker />
        {showWorktree ? <WorktreeControl /> : null}
      </div>
      <div className="flex min-w-0 items-center gap-1.5">
        <ContextUsageRing />
        <DetailStrip />
      </div>
    </footer>
  );
}
