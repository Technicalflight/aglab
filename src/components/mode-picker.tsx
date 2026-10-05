import { useEffect, useState } from "react";
import {
  IconCheck as Check,
  IconClipboardList as ClipboardList,
  IconMessage as MessageSquare,
  IconSparkles as Sparkles,
  IconTarget as Target,
  IconTrash as Trash,
} from "@tabler/icons-react";

import { commandRisk, goalCriteriaDraft, type CriterionInput } from "@/lib/chat-transport";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { useChatStore } from "@/store/chat-store";
import { MODE_LEVELS, type WorkingMode } from "@/types/chat";
import { cn } from "@/lib/utils";

const ICONS: Record<WorkingMode, typeof Target> = {
  chat: MessageSquare,
  plan: ClipboardList,
  goal: Target,
};

const fieldClass =
  "h-9 w-full rounded-lg border border-input bg-background px-3 text-base text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35";

/** 档案下拉里那一格"不点名"。Select 不许空串当 value，所以给一个哨兵值 */
const FOLLOW = "__follow__";

/** 弹框里一条判据的本地形状。id 由后端补铸——人在弹框里写的是内容，不是名字 */
type CriterionRow = { kind: "check" | "judgment"; text: string; command: string };

/** 判据条数与字数的限（design-goal-mode.md §3.1）。超限在写入口拒、读侧不裁 */
const MAX_CRITERIA = 12;
const MAX_CONSTRAINTS = 8;

const RISK_LABEL: Record<string, string> = {
  safe: "复验",
  elevated: "仅上报",
  high: "仅上报",
};

/**
 * 作业模式选择器：对话 / 规划 / 目标。
 *
 * 它与旁边的权限档位是两个独立的轴——那一档问"这一下要不要点头"，这一档问"这一支准不准
 * 动手"。读数由后端从话题日志算出来，这里只显示，不在前端另存一份
 *
 * 它只管切档。目标那一行读数与它的动作都在输入框上方的 `GoalStrip` 上
 */
export function ModePicker() {
  const mode = useChatStore((s) => s.mode);
  const modeError = useChatStore((s) => s.modeError);
  const setMode = useChatStore((s) => s.setMode);
  const setGoalDialog = useChatStore((s) => s.setGoalDialog);

  const [open, setOpen] = useState(false);

  const current = mode?.mode ?? "chat";
  const level = MODE_LEVELS.find((item) => item.value === current) ?? MODE_LEVELS[0];
  const Icon = ICONS[current];
  // 挂着目标就把这一格染成品牌色：不管现在在哪一档、也不管它是不是暂停中，
  // 收进颜色里会让人以为它没了——而它其实正在一轮一轮地跑。
  // 读数与动作不在这格上，它们在输入框上方那条 `GoalStrip`
  const hasGoal = Boolean(mode?.objective);

  function pick(value: WorkingMode) {
    setOpen(false);
    if (value === current) return;
    if (value === "goal") {
      setGoalDialog(true);
      return;
    }
    void setMode({ mode: value });
  }

  return (
    <>
      <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger
        type="button"
        className={cn(
          "flex h-8 w-max shrink-0 items-center gap-1.5 rounded-lg px-2 text-sm outline-none transition-colors hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45 data-[state=open]:bg-accent",
          current === "chat" && !hasGoal ? "text-muted-foreground" : "text-brand-text",
        )}
      >
        <Icon className="size-3.5 shrink-0" />
        <span className="shrink-0">{level.label}</span>
        {/* 这里只摆档位名。轮数与花费不进这一行：它们是**这一支目标**的账，
            长在输入框上方那条 `GoalStrip` 上，摊到工具条里只会把档位名挤短。
            挂着目标这件事由颜色说（品牌紫），不占字的位置 */}
      </PopoverTrigger>

      <PopoverContent align="start" className="w-[320px] p-1.5">
        <p className="px-2 pt-1.5 pb-2 text-xs text-muted-foreground">
          这一支现在怎么干活。它与左边的权限档位是两件事：档位管"要不要点头"，这里管"准不准动手"。
        </p>

        {MODE_LEVELS.map((item) => {
          const OptionIcon = ICONS[item.value];
          const active = item.value === current;
          return (
            <button
              key={item.value}
              type="button"
              onClick={() => pick(item.value)}
              className="flex w-full cursor-pointer items-start gap-2.5 rounded-lg px-2 py-2 text-left outline-none transition-colors hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45"
            >
              <OptionIcon className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
              <span className="min-w-0 flex-1">
                <span className="block text-base text-foreground">{item.label}</span>
                <span className="mt-0.5 block text-xs leading-5 text-muted-foreground">
                  {item.description}
                </span>
              </span>
              {active ? <Check className="mt-1 size-3.5 shrink-0 text-brand-text" /> : null}
            </button>
          );
        })}

        {/* 这里只剩报错。目标读数与它的动作都在输入框上方那条 `GoalStrip` 上——
            同一个数在屏幕上印两遍，就是等着漂 */}
        {modeError ? (
          <div className="mt-1 space-y-1 border-t border-border/70 px-2 pt-2">
            <p className="text-xs text-destructive">{modeError}</p>
          </div>
        ) : null}
      </PopoverContent>
      </Popover>
      {/* 弹框挂在这一格：三个入口（选择器 / 目标带 / 规划接力）都经 store 的
          `goalDialogOpen` 来开，它自己不占工具条的位置 */}
      <GoalDialog />
    </>
  );
}

/**
 * 目标弹框：两列契约编辑器（design-goal-mode.md §5.3）。
 *
 * 左列是契约（终态 + 判据 + 约束），右列是预算与执行加契约自检——竖排到第 6 条判据时
 * 「开始推进」已经在屏外，按档案弹窗那条规矩重画成两列。
 *
 * 它有三个入口共用：模式选择器的「定一个目标」、目标带的「编辑」（同一支换文字）、
 * 规划档的「按这份方案立目标」（打开后自动走一次草案）。开合由 store 的
 * `goalDialogOpen` 管，因为入口在组件外面
 */
export function GoalDialog() {
  const modeBusy = useChatStore((s) => s.modeBusy);
  const activeId = useChatStore((s) => s.activeId);
  const profiles = useChatStore((s) => s.config.profiles);
  const goalDialogOpen = useChatStore((s) => s.goalDialogOpen);
  const goalDialogSource = useChatStore((s) => s.goalDialogSource);
  const setGoalDialog = useChatStore((s) => s.setGoalDialog);
  const goalSet = useChatStore((s) => s.goalSet);
  const goalEdit = useChatStore((s) => s.goalEdit);

  const [objective, setObjective] = useState("");
  const [criteria, setCriteria] = useState<CriterionRow[]>([{ kind: "check", text: "", command: "" }]);
  const [constraints, setConstraints] = useState<string[]>([]);
  const [capUsd, setCapUsd] = useState("");
  const [profileId, setProfileId] = useState<string>(FOLLOW);
  const [formError, setFormError] = useState<string | null>(null);
  const [needsConfirm, setNeedsConfirm] = useState(false);
  const [isDraft, setIsDraft] = useState(false);
  const [draftBusy, setDraftBusy] = useState(false);
  // 每条 check 命令的风险档，问的是后端那把 classify——前端不另算
  const [risks, setRisks] = useState<Record<number, string>>({});

  // 打开那一刻按现状预填一次：编辑预填同一支的契约；替换确认在提交那一刻由后端的
  // "确认替换"报错触发，这里不替它猜。依赖只有开合这一格——弹框开着时后台的
  // Mode 事件不许把人正在写的表单冲掉
  const [editAtOpen, setEditAtOpen] = useState(false);
  useEffect(() => {
    if (!goalDialogOpen) return;
    const current = useChatStore.getState().mode;
    setEditAtOpen(Boolean(current?.objective));
    setFormError(null);
    setNeedsConfirm(false);
    setIsDraft(false);
    setRisks({});
    setObjective(current?.objective ?? "");
    const held = current?.contract;
    setCriteria(
      held && held.criteria.length > 0
        ? held.criteria.map((criterion) => ({
            kind: criterion.kind,
            text: criterion.text,
            command: criterion.command ?? "",
          }))
        : [{ kind: "check", text: "", command: "" }],
    );
    setConstraints(held?.constraints ?? []);
    setCapUsd(current && current.maxCostUsdE8 > 0 ? String(current.maxCostUsdE8 / 1e8) : "");
    setProfileId(current?.profile ?? FOLLOW);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [goalDialogOpen]);

  // 规划接力的材料在打开时就地进草案：方案的"每一步用什么验证"正是判据的原料
  useEffect(() => {
    if (!goalDialogOpen || !goalDialogSource) return;
    void runDraft(goalDialogSource);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [goalDialogOpen, goalDialogSource]);

  const checkCount = criteria.filter((row) => row.kind === "check").length;
  const judgmentCount = criteria.length - checkCount;
  const missingCommand = criteria.filter((row) => row.kind === "check" && !row.command.trim()).length;
  const runtimeCount = checkCount - missingCommand;

  /** 让模型补全判据与约束。产出是草案：虚线边 + 徽标，采纳才立目标 */
  async function runDraft(source: string) {
    setDraftBusy(true);
    setFormError(null);
    try {
      const draft = await goalCriteriaDraft(source);
      setCriteria(
        draft.criteria.length > 0
          ? draft.criteria.map((criterion) => ({
              kind: criterion.kind,
              text: criterion.text,
              command: criterion.command ?? "",
            }))
          : [{ kind: "check", text: "", command: "" }],
      );
      setConstraints(draft.constraints);
      setIsDraft(true);
    } catch (error) {
      setFormError(error instanceof Error ? error.message : String(error));
    } finally {
      setDraftBusy(false);
    }
  }

  // check 命令的风险档：就地问后端的 classify（无 IO，便宜），防抖 300ms
  useEffect(() => {
    if (!goalDialogOpen) return;
    const timer = window.setTimeout(() => {
      for (const [index, row] of criteria.entries()) {
        if (row.kind !== "check" || !row.command.trim()) continue;
        void commandRisk(activeId, row.command.trim())
          .then((risk) => setRisks((held) => ({ ...held, [index]: risk })))
          .catch(() => {});
      }
    }, 300);
    return () => window.clearTimeout(timer);
  }, [criteria, goalDialogOpen, activeId]);

  function patchRow(index: number, patch: Partial<CriterionRow>) {
    setCriteria((held) => held.map((row, i) => (i === index ? { ...row, ...patch } : row)));
    setIsDraft(false);
  }

  async function submit(force: boolean) {
    const wanted = objective.trim();
    if (!wanted) {
      setFormError("把目标写下来——没有目标，续跑就没有方向。");
      return;
    }
    if (criteria.every((row) => !row.text.trim())) {
      setFormError("还差一条判据：写它怎么验证。没有判据，'做完了'就没有门可过。");
      return;
    }
    const cap = capUsd.trim();
    if (cap !== "" && (!Number.isFinite(Number(cap)) || Number(cap) < 0)) {
      setFormError("花费上限要么留空（不设上限），要么填一个不小于 0 的美元数字。");
      return;
    }
    const payload: CriterionInput[] = criteria
      .filter((row) => row.text.trim())
      .map((row) =>
        row.kind === "check" && row.command.trim()
          ? { text: row.text.trim(), kind: "check" as const, command: row.command.trim() }
          : { text: row.text.trim(), kind: "judgment" as const },
      );
    const error = editAtOpen
      ? await goalEdit({
          objective: wanted,
          criteria: payload,
          constraints: constraints.filter((c) => c.trim()),
        })
      : await goalSet({
          objective: wanted,
          criteria: payload,
          constraints: constraints.filter((c) => c.trim()),
          maxCostUsd: cap === "" ? null : cap,
          profile: profileId === FOLLOW ? null : profileId,
          force,
        });
    if (error) {
      setFormError(error);
      // 后端那道替换确认闸的暗号：它说了算，界面只负责把确认亮出来
      if (error.includes("确认替换")) setNeedsConfirm(true);
      return;
    }
    setFormError(null);
    setGoalDialog(false);
  }

  return (
    <Dialog
      open={goalDialogOpen}
      onOpenChange={(next) => {
        setGoalDialog(next);
        if (!next) setFormError(null);
      }}
    >
      {/* DialogContent 默认写死 w-[420px]：不覆盖宽度，两列契约编辑器就塌成一条
          （max-width 对固定 width 无效）。这里用 w 覆盖——twMerge 会把默认那格挤掉。
          判据加到十几条时内容会超过一屏，而弹框是固定居中定位、不会自己滚：
          限高 + 内部滚动，标题与底部按钮保持可见 */}
      <DialogContent className="flex max-h-[92vh] w-[min(1120px,94vw)] flex-col overflow-hidden">
        <DialogTitle className="shrink-0">给这一支定一个目标</DialogTitle>
        {/* 窗口窄于 md 时退化为单列竖排——两列是宽窗的形状，不是窄窗的。
            min-h-0 是承重的：没有它，flex 子元素不许自己缩，滚动区就撑不开 */}
        <div className="mt-2 grid min-h-0 flex-1 grid-cols-1 gap-4 overflow-y-auto pr-1 md:grid-cols-[1fr_320px]">
          <div className="min-w-0 space-y-3">
            <p className="text-xs leading-5 text-muted-foreground">
              按下<span className="text-foreground">开始推进就立刻开跑</span>，一轮一轮做到完——
              <span className="text-foreground">没有轮次上限</span>。要拦它有花费上限与目标带上的暂停/结束；
              输入框上的<span className="text-foreground">停止只管眼前这一轮</span>。
            </p>
            <button
              type="button"
              disabled={draftBusy || !objective.trim()}
              className={cn(
                "flex items-center gap-1.5 rounded-lg border border-dashed border-brand/40 px-2.5 py-1.5 text-xs text-brand-text outline-none transition-colors hover:bg-brand/5 focus-visible:ring-2 focus-visible:ring-ring/45",
                draftBusy || !objective.trim() ? "cursor-not-allowed opacity-50" : "cursor-pointer",
              )}
              title={objective.trim() ? "让模型从目标（或方案）里提炼判据与约束，产出是草案，可改可弃" : "先写下目标，才有东西可提炼"}
              onClick={() => void runDraft(objective.trim())}
            >
              <Sparkles className="size-3.5" />
              {draftBusy ? "正在提炼…" : "让模型补全判据与约束"}
            </button>
            <label className="block">
              <span className="mb-1.5 block text-xs text-muted-foreground">终态（一句话，给人看）</span>
              <textarea
                rows={3}
                value={objective}
                placeholder="例：把台账与价格的三处名单对账补齐，并跑通全量测试"
                className={cn(fieldClass, "h-auto resize-y py-2 leading-6")}
                onChange={(event) => setObjective(event.target.value)}
              />
            </label>
            <div>
              <div className="mb-1.5 flex items-baseline justify-between">
                <span className="text-xs text-muted-foreground">
                  判据（怎么才算真做到）· 至少 1 条
                </span>
                {isDraft ? (
                  <span className="rounded-full bg-brand/15 px-1.5 text-2xs text-brand-text">由 AI 生成</span>
                ) : null}
              </div>
              <div className={cn("space-y-1.5 rounded-xl p-1", isDraft && "border border-dashed border-brand/40")}>
                {criteria.map((row, index) => (
                  <div key={index} className="flex items-center gap-1.5">
                    <div className="flex shrink-0 overflow-hidden rounded-lg border border-input">
                      {(["check", "judgment"] as const).map((kind) => (
                        <button
                          key={kind}
                          type="button"
                          title={kind === "check" ? "跑命令：收尾时运行时会复跑它" : "要人看：只接受模型上报"}
                          className={cn(
                            "rounded-sm px-2 py-1.5 text-xs outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/55",
                            row.kind === kind
                              ? "bg-brand/15 text-brand-text"
                              : "text-muted-foreground hover:bg-accent",
                          )}
                          onClick={() => patchRow(index, { kind })}
                        >
                          {kind === "check" ? "跑命令" : "要人看"}
                        </button>
                      ))}
                    </div>
                    <input aria-label="判据说明"
                      value={row.text}
                      placeholder="这条判据说什么才算过"
                      className={cn(fieldClass, "h-8 flex-1 text-sm")}
                      onChange={(event) => patchRow(index, { text: event.target.value })}
                    />
                    {row.kind === "check" ? (
                      <div className="relative flex-1">
                        <input aria-label="判据命令"
                          value={row.command}
                          placeholder="命令待填"
                          className={cn(fieldClass, "h-8 pr-16 font-mono text-xs")}
                          onChange={(event) => patchRow(index, { command: event.target.value })}
                        />
                        {row.command.trim() && risks[index] ? (
                          <span
                            className={cn(
                              "absolute right-2 top-1/2 -translate-y-1/2 rounded-full px-1.5 text-2xs",
                              risks[index] === "safe"
                                ? "bg-brand/15 text-brand-text"
                                : "bg-destructive/10 text-destructive",
                            )}
                            title={
                              risks[index] === "safe"
                                ? "只读命令：收尾时运行时会自己复跑"
                                : "会动东西的命令：收尾时不自动跑，只接上报"
                            }
                          >
                            {RISK_LABEL[risks[index]] ?? risks[index]}
                          </span>
                        ) : null}
                      </div>
                    ) : null}
                    <button
                      type="button"
                      title="删掉这一条"
                      className="shrink-0 rounded-md p-1.5 text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-destructive focus-visible:ring-2 focus-visible:ring-ring/45"
                      onClick={() =>
                        setCriteria((held) =>
                          held.length > 1 ? held.filter((_, i) => i !== index) : held,
                        )
                      }
                    >
                      <Trash className="size-3.5" />
                    </button>
                  </div>
                ))}
                {criteria.length < MAX_CRITERIA ? (
                  <button
                    type="button"
                    className="rounded-lg px-2 py-1 text-xs text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
                    onClick={() =>
                      setCriteria((held) => [...held, { kind: "check", text: "", command: "" }])
                    }
                  >
                    ＋ 加一条判据
                  </button>
                ) : (
                  <p className="px-2 text-xs text-destructive">判据到顶了（{MAX_CRITERIA} 条）——把同一件事的几条并成一条。</p>
                )}
              </div>
            </div>
            <div>
              <span className="mb-1.5 block text-xs text-muted-foreground">
                约束（推进期间不许动什么）
              </span>
              <div className="space-y-1.5">
                {constraints.map((constraint, index) => (
                  <div key={index} className="flex items-center gap-1.5">
                    <span className="shrink-0 text-xs text-muted-foreground">·</span>
                    <input aria-label="约束"
                      value={constraint}
                      className={cn(fieldClass, "h-8 flex-1 text-sm")}
                      onChange={(event) =>
                        setConstraints((held) =>
                          held.map((item, i) => (i === index ? event.target.value : item)),
                        )
                      }
                    />
                    <button
                      type="button"
                      title="删掉这一条"
                      className="shrink-0 rounded-md p-1.5 text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-destructive focus-visible:ring-2 focus-visible:ring-ring/45"
                      onClick={() => setConstraints((held) => held.filter((_, i) => i !== index))}
                    >
                      <Trash className="size-3.5" />
                    </button>
                  </div>
                ))}
                {constraints.length < MAX_CONSTRAINTS ? (
                  <button
                    type="button"
                    className="rounded-lg px-2 py-1 text-xs text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
                    onClick={() => setConstraints((held) => [...held, ""])}
                  >
                    ＋ 加一条约束
                  </button>
                ) : (
                  <p className="px-2 text-xs text-destructive">约束到顶了（{MAX_CONSTRAINTS} 条）。</p>
                )}
              </div>
            </div>
          </div>

          <div className="space-y-3">
            <label className="block">
              <span className="mb-1.5 block text-xs text-muted-foreground">
                花费上限（美元，留空 = 不设）
              </span>
              <input
                type="text"
                inputMode="decimal"
                value={capUsd}
                placeholder="不设"
                className={fieldClass}
                onChange={(event) => setCapUsd(event.target.value)}
              />
            </label>
            {/* 这里不能用 label 包住 Select：Radix 的触发器自带 role，
                套进 label 会把整块控件的点选抢给文字 */}
            <div className="block">
              <span className="mb-1.5 block text-xs text-muted-foreground">由谁执行</span>
              <Select value={profileId} onValueChange={setProfileId}>
                <SelectTrigger className="w-full text-base">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value={FOLLOW}>跟随当前配置</SelectItem>
                  {profiles.map((profile) => (
                    <SelectItem key={profile.id} value={profile.id}>
                      {profile.name}
                      {profile.model ? ` · ${profile.model}` : ""}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <p className="mt-1.5 text-xs leading-5 text-muted-foreground">
                点名之后，这个目标<span className="text-foreground">每一轮都走那张档案</span>
                的连接与模型。引用的是那张卡片本身：以后改它的连接，目标跟着新连接走。
              </p>
            </div>

            {/* 契约自检：纯读数不做旋钮。把"哪几条会被自动复验、哪几条只接上报"
                数给人看——完成门那道闸在人写下的这一刻就看得见 */}
            <div className="rounded-xl border border-border/70 bg-surface/60 p-2.5">
              <p className="text-xs font-medium text-foreground">契约自检</p>
              <p className="mt-1 text-xs leading-5 text-muted-foreground">
                {criteria.filter((row) => row.text.trim()).length} 条判据 ·{" "}
                {missingCommand > 0 ? `${missingCommand} 条还没定命令` : "命令都齐了"}
              </p>
              <p className="mt-0.5 text-xs leading-5 text-muted-foreground">
                {runtimeCount} 条收尾时自动复验 ·{" "}
                {judgmentCount + missingCommand} 条只接上报·界面上会标
              </p>
            </div>

            {needsConfirm ? (
              <p className="rounded-lg border border-destructive/40 bg-destructive/5 px-2.5 py-2 text-xs leading-5 text-destructive">
                已有一支推进中的目标。再按一次就是<span className="text-foreground">替换</span>
                ——旧目标的账与结论一起收档，新目标从零起算。
              </p>
            ) : null}
            {formError ? (
              <p className="text-xs leading-5 text-destructive">{formError}</p>
            ) : null}
          </div>
        </div>
        <div className="mt-3 flex justify-end gap-2">
          <Button variant="ghost" size="sm" onClick={() => setGoalDialog(false)}>
            取消
          </Button>
          <Button
            variant="brand"
            size="sm"
            disabled={modeBusy || draftBusy}
            onClick={() => void submit(needsConfirm)}
          >
            {editAtOpen ? "保存并继续" : needsConfirm ? "替换并开始" : "开始推进"}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

/**
 * 规划模式的那一行批准入口。它只在 `planReady` 为真时存在——也就是模型这一支已经把
 * 话说完了（后端从日志派生：最新一条用户发言之后有一条以文本收尾的回答）。
 * 刚切进规划模式、或用户又追问了一句的时候不该有这颗按钮：那时屏上没有方案，
 * 点亮"批准"就是替模型把"方案已交"这件事先说了
 */
export function PlanApprovalBar() {
  const mode = useChatStore((s) => s.mode);
  const pending = useChatStore((s) => s.pending);
  const modeBusy = useChatStore((s) => s.modeBusy);
  const setMode = useChatStore((s) => s.setMode);
  const setGoalDialog = useChatStore((s) => s.setGoalDialog);
  const send = useChatStore((s) => s.send);
  const messages = useChatStore((s) => s.messages);
  const [busy, setBusy] = useState(false);

  if (!mode || !mode.planReady || pending) return null;

  async function approve() {
    setBusy(true);
    // 先切档再说话：切档失败就停在这儿，别让"批准"变成一句模型收到、护栏却还在的空话
    const error = await setMode({ mode: "chat" });
    if (error) {
      setBusy(false);
      return;
    }
    await send("方案我看了，按这份开工。做每一步之后说一句实际结果，别一次报完整份计划。");
    setBusy(false);
  }

  /** 规划接力（§5.6）：方案全文就是草案的材料——
   *  `PLAN_BODY` 本来就要求方案写清"每一步用什么验证它真的成了" */
  function planToGoal() {
    const planText = [...messages]
      .reverse()
      .find((message) => message.role === "assistant")?.content ?? "";
    setGoalDialog(true, planText);
  }

  return (
    <div className="mx-2.5 mt-2.5 flex items-center gap-2 rounded-lg border border-brand/30 bg-brand/5 px-3 py-2">
      <span className="min-w-0 flex-1 text-sm leading-5 text-muted-foreground">
        上面那份就是方案。批准后它开始动手，之前它只读不改。
      </span>
      <Button
        size="sm"
        variant="ghost"
        disabled={busy || modeBusy}
        title="把方案里的验证步骤提炼成判据，立成一支目标——不切档、不开工，草案可改"
        onClick={() => planToGoal()}
      >
        按这份方案立目标
      </Button>
      <Button size="sm" variant="brand" disabled={busy || modeBusy} onClick={() => void approve()}>
        批准并执行
      </Button>
    </div>
  );
}
