import { useCallback, useEffect, useState } from "react";
import {
  IconExternalLink as ExternalLink,
  IconFolderOpen as FolderOpen,
  IconRefresh as RefreshCw,
  IconSearch as Search,
  IconStar as Star,
} from "@tabler/icons-react";
import { openUrl, revealItemInDir } from "@tauri-apps/plugin-opener";

import { CapabilityToggle } from "@/components/ui/capability-toggle";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { SectionFrame } from "@/components/section-frame";
import {
  githubSkillInstall,
  githubSkillProbe,
  skillhubInstall,
  skillhubSearch,
  type GithubProbe,
  type SkillhubEntry,
  type SkillhubSortBy,
} from "@/lib/chat-transport";
import { useChatStore } from "@/store/chat-store";
import { PaginationBar, usePaged } from "@/components/pagination";
import { cn } from "@/lib/utils";
import { ListSkeleton } from "@/components/ui/loading-skeleton";

export function SkillsView() {
  const skills = useChatStore((s) => s.skills);
  // 同 plugins-view：加载中与真的空目录在数据上无法区分，得显式分开
  const configLoaded = useChatStore((s) => s.configLoaded);
  const pagedSkills = usePaged(skills);
  const skillsDir = useChatStore((s) => s.skillsDir);
  const skillsError = useChatStore((s) => s.skillsError);
  const refreshSkills = useChatStore((s) => s.refreshSkills);
  const toggleSkill = useChatStore((s) => s.toggleSkill);

  const skillCandidates = useChatStore((s) => s.skillCandidates);
  const skillCandidatesLoading = useChatStore((s) => s.skillCandidatesLoading);
  const skillCandidatesError = useChatStore((s) => s.skillCandidatesError);
  const skillImportNote = useChatStore((s) => s.skillImportNote);
  const refreshSkillCandidates = useChatStore((s) => s.refreshSkillCandidates);
  const importSkills = useChatStore((s) => s.importSkills);

  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [importing, setImporting] = useState(false);

  // ---- SkillHub 市场：浏览、搜索，安装走详情页（内容不在公开接口里）----
  const [marketOpen, setMarketOpen] = useState(false);
  const [marketKeyword, setMarketKeyword] = useState("");
  const [marketSort, setMarketSort] = useState<SkillhubSortBy>("score");
  const [marketEntries, setMarketEntries] = useState<SkillhubEntry[]>([]);
  const [marketTotal, setMarketTotal] = useState(0);
  const [marketPage, setMarketPage] = useState(1);
  const [marketLoading, setMarketLoading] = useState(false);
  const [marketError, setMarketError] = useState<string | null>(null);
  const [installingSlug, setInstallingSlug] = useState<string | null>(null);

  // ---- GitHub 仓库安装：粘贴仓库地址 → 识别候选 → 挑一个装进个人目录 ----
  const [ghOpen, setGhOpen] = useState(false);
  const [ghUrl, setGhUrl] = useState("");
  const [ghProbe, setGhProbe] = useState<GithubProbe | null>(null);
  const [ghPicked, setGhPicked] = useState<number | null>(null);
  const [ghName, setGhName] = useState("");
  const [ghProbing, setGhProbing] = useState(false);
  const [ghInstalling, setGhInstalling] = useState(false);
  const [ghError, setGhError] = useState<string | null>(null);

  const SORTS: Array<{ value: SkillhubSortBy; label: string }> = [
    { value: "score", label: "评分" },
    { value: "downloads", label: "下载" },
    { value: "trending", label: "趋势" },
    { value: "updated", label: "最近更新" },
  ];

  async function loadMarket(page: number, sortBy: SkillhubSortBy) {
    setMarketLoading(true);
    setMarketError(null);
    try {
      const result = await skillhubSearch(marketKeyword, sortBy, page);
      // 换排序/关键词回到第 1 页时整表替换；翻页时追加
      setMarketEntries((previous) => (page === 1 ? result.entries : [...previous, ...result.entries]));
      setMarketTotal(result.total);
      setMarketPage(page);
    } catch (cause) {
      setMarketError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setMarketLoading(false);
    }
  }

  function openMarket() {
    setMarketOpen(true);
    if (marketEntries.length === 0) void loadMarket(1, marketSort);
  }

  // 已装判定：安装目录名取 slug 的技能段，技能清单里 id = 个人/<目录名>
  const installedSlugs = new Set(
    skills
      .filter((skill) => skill.id.startsWith("个人/"))
      .map((skill) => skill.id.slice("个人/".length)),
  );

  async function installEntry(entry: SkillhubEntry) {
    setInstallingSlug(entry.handle);
    setMarketError(null);
    try {
      // 下载服务商要的是完整规范名（@作者/技能），不是短 slug
      const report = await skillhubInstall(entry.handle);
      await refreshSkills();
      setNote(`已装好「${entry.name}」：${report.files} 个文件进 ${report.dir}`);
    } catch (cause) {
      setMarketError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setInstallingSlug(null);
    }
  }

  async function probeGithub() {
    setGhProbing(true);
    setGhError(null);
    setGhProbe(null);
    setGhPicked(null);
    try {
      const probe = await githubSkillProbe(ghUrl.trim());
      setGhProbe(probe);
      setGhPicked(probe.candidates.length === 1 ? 0 : null);
      if (probe.candidates.length === 1) {
        setGhName(probe.candidates[0].path.split("/").pop() || probe.repo);
      }
    } catch (cause) {
      setGhError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setGhProbing(false);
    }
  }

  async function installGithub() {
    if (!ghProbe || ghPicked === null) return;
    const candidate = ghProbe.candidates[ghPicked];
    setGhInstalling(true);
    setGhError(null);
    try {
      const report = await githubSkillInstall({
        url: ghUrl.trim(),
        path: candidate.path,
        branch: ghProbe.branch,
        name: ghName,
      });
      await refreshSkills();
      setGhOpen(false);
      setNote(`已装好「${candidate.name}」：${report.files} 个文件进 ${report.dir}`);
    } catch (cause) {
      setGhError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setGhInstalling(false);
    }
  }

  const reload = useCallback(async () => {
    setBusy(true);
    await refreshSkills();
    setBusy(false);
  }, [refreshSkills]);

  useEffect(() => {
    void reload();
  }, [reload]);

  // 候选到位后默认勾上"能导的"：重名或缺正文的没有意义，勾了也是白勾
  useEffect(() => {
    if (dialogOpen) {
      setSelected(
        new Set(
          skillCandidates
            .filter((candidate) => candidate.hasSkillMd && !candidate.exists)
            .map((candidate) => candidate.sourceId),
        ),
      );
    }
  }, [dialogOpen, skillCandidates]);

  async function openDialog() {
    setDialogOpen(true);
    await refreshSkillCandidates();
  }

  function flip(sourceId: string) {
    setSelected((previous) => {
      const next = new Set(previous);
      if (next.has(sourceId)) next.delete(sourceId);
      else next.add(sourceId);
      return next;
    });
  }

  async function submit() {
    setImporting(true);
    try {
      await importSkills([...selected]);
    } finally {
      setImporting(false);
    }
  }

  const importable = skillCandidates.filter(
    (candidate) => candidate.hasSkillMd && !candidate.exists,
  );

  const enabledCount = skills.filter((skill) => skill.enabled).length;
  const injected = skills
    .filter((skill) => skill.enabled)
    .reduce((total, skill) => total + skill.chars, 0);

  return (
    <SectionFrame
      title="技能"
      note={`${enabledCount} / ${skills.length} 项启用`}
      actions={
        <div className="flex items-center gap-1">
          <Button variant="ghost" size="sm" onClick={() => setGhOpen(true)}>
            <span>GitHub 安装</span>
          </Button>
          <Button variant="ghost" size="sm" onClick={() => openMarket()}>
            <span>市场</span>
          </Button>
          <Button variant="ghost" size="sm" onClick={() => void openDialog()}>
            <span>从 cc-switch 导入</span>
          </Button>
          <Button
            variant="ghost"
            size="sm"
            aria-label="打开技能目录"
            disabled={!skillsDir}
            onClick={() =>
              void revealItemInDir(skillsDir).catch((cause) =>
                setNote(`打不开目录：${cause instanceof Error ? cause.message : String(cause)}`),
              )
            }
          >
            <FolderOpen className="size-3.5" />
            <span>打开目录</span>
          </Button>
          <Button variant="ghost" size="sm" disabled={busy} onClick={() => void reload()}>
            <RefreshCw className={busy ? "size-3.5 animate-spin" : "size-3.5"} />
            <span>重新扫描</span>
          </Button>
        </div>
      }
    >
      <p className="text-sm leading-6 text-muted-foreground">
        技能是钉在墙上的操作清单：把重复的要求写成一份 SKILL.md，之后只说"写日报"就行。
        启用的技能只把<span className="text-foreground">名字和描述</span>列给模型，
        模型自己判断这次要不要用，决定用了才去取正文 —— 所以描述里要写清"什么时候用"。
        技能<span className="text-foreground">只给做法，不给执行权</span>，能碰文件和命令的是「工具」。
      </p>

      <div className="mt-3 rounded-lg border border-border bg-background px-3 py-2.5">
        <p className="text-xs text-muted-foreground">
          每个技能是一个子目录，里面放 SKILL.md：
        </p>
        <p className="mt-0.5 break-all font-mono text-xs text-muted-foreground">
          {skillsDir || "…"}/&lt;技能名&gt;/SKILL.md
        </p>
        <p className="mt-1 text-xs text-muted-foreground">放好回来点「重新扫描」。</p>
        <p className="mt-1 break-all font-mono text-xs text-foreground">{skillsDir || "…"}</p>
        <p className="mt-1.5 text-xs leading-5 text-muted-foreground">
          文件开头可以用 <span className="font-mono">---</span> 围一段 frontmatter：{" "}
          <span className="font-mono">name</span>（小写字母、数字和连字符）、
          <span className="font-mono">description</span>（说清功能和什么时候用，第三人称）、
          可选 <span className="font-mono">allowed-tools</span>（限定这个技能只用哪些工具）。
          值可以写成一行，也可以用 YAML 的 <span className="font-mono">&gt;</span> 折叠或{" "}
          <span className="font-mono">|</span> 保留换行。没写就用目录名和正文第一段当描述。
        </p>
      </div>

      {!configLoaded ? (
        <ListSkeleton rows={3} className="mt-3" label="正在加载技能" />
      ) : skills.length === 0 ? (
        <p className="mt-4 text-sm text-muted-foreground">这个目录里还没有技能。</p>
      ) : (
        <>
        <ul className="mt-4 divide-y divide-border overflow-hidden rounded-lg border border-border bg-surface">
          {pagedSkills.slice.map((skill) => (
            <li key={skill.id} className="px-3 py-3">
              <div className="flex items-start gap-3">
                <div className="min-w-0 flex-1">
                  <p className="flex items-baseline gap-2 text-base">
                    <span className="font-medium text-foreground">{skill.name}</span>
                    <span className="text-xs text-muted-foreground">
                      {skill.source} · {skill.chars} 字
                    </span>
                    {skill.allowedTools.length > 0 ? (
                      <>
                      <span className="shrink-0 text-xs text-muted-foreground">
                        限用 <span className="font-mono">{skill.allowedTools.join(", ")}</span>
                      </span>
                      </>
                    ) : null}
                  </p>
                  <p className="mt-1 text-xs leading-5 text-muted-foreground">
                    {skill.description}
                  </p>
                </div>
                <CapabilityToggle
                  label={skill.name}
                  enabled={skill.enabled}
                  onToggle={() => void toggleSkill(skill.id, !skill.enabled)}
                />
              </div>
              <p className="mt-2 line-clamp-2 text-xs leading-5 text-muted-foreground/80">
                {skill.preview}
              </p>
            </li>
          ))}
        </ul>
          <PaginationBar page={pagedSkills.page} pages={pagedSkills.pages} total={pagedSkills.total} onPage={pagedSkills.setPage} />
      </>
      )}

      <p className="mt-3 text-xs leading-5 text-muted-foreground">
        一次最多给模型列 60 个技能{injected > 0 ? `，当前启用正文合计 ${injected} 字` : ""}
        —— 这些字不预先占上下文，模型取用哪个才读哪个。
      </p>

      {skillsError ? <p className="mt-2 text-xs text-destructive">{skillsError}</p> : null}
      {note ? <p className="mt-2 text-xs text-muted-foreground">{note}</p> : null}

      <Dialog
        open={ghOpen}
        onOpenChange={(open) => {
          if (!open) setGhOpen(false);
        }}
      >
        <DialogContent className="max-w-lg">
          <DialogTitle>从 GitHub 仓库装技能</DialogTitle>
          <p className="mt-1 text-xs leading-5 text-muted-foreground">
            粘贴 github.com 的仓库地址（也认 /tree/分支/子路径 的深层链接）。
            aglab 会找出里面所有带 SKILL.md 的目录，挑一个装进个人技能目录。
            公开仓库免凭据；GitHub 匿名限速每小时 60 次。
          </p>

          <div className="mt-3 flex items-center gap-1.5">
            <input
              type="text"
              value={ghUrl}
              placeholder="https://github.com/所有者/仓库"
                aria-label="GitHub 仓库地址"
              spellCheck={false}
              className="h-8 min-w-0 flex-1 rounded-lg border border-input bg-background px-2.5 font-mono text-sm outline-none transition-colors focus-visible:border-brand/50"
              onChange={(event) => setGhUrl(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  void probeGithub();
                }
              }}
            />
            <Button
              variant="subtle"
              size="sm"
              className="shrink-0"
              disabled={ghProbing || ghUrl.trim() === ""}
              onClick={() => void probeGithub()}
            >
              {ghProbing ? "识别中…" : "识别"}
            </Button>
          </div>

          {ghError ? <p className="mt-2 text-xs text-destructive">{ghError}</p> : null}

          {ghProbe ? (
            <>
              <ul className="mt-3 max-h-64 space-y-1.5 overflow-y-auto">
                {ghProbe.candidates.map((candidate, index) => {
                  const picked = ghPicked === index;
                  const displayPath = candidate.path === "" ? `${ghProbe.repo}（仓库根）` : candidate.path;
                  return (
                    <li key={candidate.path || "root"}>
                      <button
                        type="button"
                        aria-pressed={picked}
                        onClick={() => {
                          setGhPicked(index);
                          setGhName(candidate.path.split("/").pop() || ghProbe.repo);
                        }}
                        className={cn(
                          "w-full rounded-lg border px-2.5 py-2 text-left outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                          picked
                            ? "border-brand/50 bg-brand/10"
                            : "border-border bg-background hover:bg-accent",
                        )}
                      >
                        <p className="truncate text-sm text-foreground">{candidate.name}</p>
                        {candidate.description ? (
                          <p className="mt-0.5 line-clamp-2 text-xs leading-5 text-muted-foreground">
                            {candidate.description}
                          </p>
                        ) : null}
                        <p className="mt-0.5 truncate font-mono text-2xs text-muted-foreground/60">
                          {displayPath}
                        </p>
                      </button>
                    </li>
                  );
                })}
              </ul>

              <div className="mt-3">
                <label className="block">
                  <span className="mb-1.5 block text-xs text-muted-foreground">
                    技能目录名（装进个人技能目录下这个名字）
                  </span>
                  <input
                    type="text"
                    value={ghName}
                    placeholder="如 daily-report"
                    spellCheck={false}
                    className="h-8 w-full rounded-lg border border-input bg-background px-2.5 font-mono text-sm outline-none transition-colors focus-visible:border-brand/50"
                    onChange={(event) => setGhName(event.target.value)}
                  />
                </label>
              </div>

              <div className="mt-4 flex items-center justify-end gap-2">
                <Button variant="ghost" size="sm" onClick={() => setGhOpen(false)}>
                  取消
                </Button>
                <Button
                  variant="brand"
                  size="sm"
                  disabled={ghInstalling || ghPicked === null || ghName.trim() === ""}
                  onClick={() => void installGithub()}
                >
                  {ghInstalling ? "安装中…" : "安装"}
                </Button>
              </div>
            </>
          ) : null}
        </DialogContent>
      </Dialog>

      <Dialog
        open={marketOpen}
        onOpenChange={(open) => {
          if (!open) setMarketOpen(false);
        }}
      >
        <DialogContent className="max-w-2xl">
          <DialogTitle>SkillHub 技能商店</DialogTitle>
          <p className="mt-1 text-xs leading-5 text-muted-foreground">
            来自 SkillHub（skillhub.cn）的社区技能目录。SKILL.md 本体不在公开接口里，
            所以这里<span className="text-foreground">不提供一键安装</span>：
            点「打开详情页」去 SkillHub 安装或下载，
            把技能文件夹放进上面的技能目录、点「重新扫描」即可用。
          </p>

          <div className="mt-3 flex items-center gap-1.5">
            <input
              type="search"
              value={marketKeyword}
              placeholder="搜技能名或用途"
                aria-label="搜索技能市场"
              className="h-8 min-w-0 flex-1 rounded-lg border border-input bg-background px-2.5 text-sm outline-none transition-colors focus-visible:border-brand/50"
              onChange={(event) => setMarketKeyword(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  void loadMarket(1, marketSort);
                }
              }}
            />
            <Button
              variant="subtle"
              size="sm"
              className="shrink-0"
              disabled={marketLoading}
              onClick={() => void loadMarket(1, marketSort)}
            >
              <Search className="size-3.5" />
              <span>搜索</span>
            </Button>
          </div>
          <div className="mt-2 flex items-center gap-1">
            {SORTS.map((option) => (
              <button
                key={option.value}
                type="button"
                aria-pressed={marketSort === option.value}
                onClick={() => {
                  setMarketSort(option.value);
                  void loadMarket(1, option.value);
                }}
                className={cn(
                  "rounded-lg border px-2 py-0.5 text-xs outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                  marketSort === option.value
                    ? "border-brand/45 bg-brand/10 text-brand-text"
                    : "border-border text-muted-foreground hover:bg-accent hover:text-foreground",
                )}
              >
                {option.label}
              </button>
            ))}
            <span className="ml-auto text-xs tabular-nums text-muted-foreground">
              共 {marketTotal.toLocaleString("en-US")} 个
            </span>
          </div>

          {marketError ? (
            <p className="mt-2 text-xs text-destructive">{marketError}</p>
          ) : null}

          <ul className="mt-2 max-h-80 space-y-1.5 overflow-y-auto">
            {marketEntries.map((entry) => (
              <li key={entry.handle} className="rounded-lg border border-border bg-background px-2.5 py-2">
                <div className="flex items-start gap-2">
                  <div className="min-w-0 flex-1">
                    <p className="flex items-baseline gap-1.5 text-sm">
                      <span className="min-w-0 truncate font-medium text-foreground">{entry.name}</span>
                      {entry.verified ? (
                        <span className="shrink-0 rounded border border-brand/40 px-1 text-2xs text-brand-text">
                          已认证
                        </span>
                      ) : null}
                      <span className="shrink-0 font-mono text-2xs text-muted-foreground/60">
                        v{entry.version}
                      </span>
                    </p>
                    <p className="mt-0.5 line-clamp-2 text-xs leading-5 text-muted-foreground">
                      {entry.description}
                    </p>
                    <p className="mt-1 flex items-center gap-2 text-2xs tabular-nums text-muted-foreground/70">
                      <span>{entry.handle}</span>
                      <span className="flex items-center gap-0.5">
                        <Star className="size-3" />
                        {entry.stars}
                      </span>
                      <span>{entry.downloads.toLocaleString("en-US")} 次下载</span>
                    </p>
                  </div>
                  <div className="flex shrink-0 items-center gap-1.5">
                    <Button
                      variant="brand"
                      size="sm"
                      disabled={installingSlug !== null || installedSlugs.has(entry.slug)}
                      title={
                        installedSlugs.has(entry.slug)
                          ? "已经装在个人技能目录里了"
                          : `下载 zip 并装进个人技能目录：${entry.slug}`
                      }
                      onClick={() => void installEntry(entry)}
                    >
                      {installingSlug === entry.handle
                        ? "安装中…"
                        : installedSlugs.has(entry.slug)
                          ? "已安装"
                          : "一键安装"}
                    </Button>
                    <Button
                      variant="ghost"
                      size="sm"
                      className="shrink-0"
                      onClick={() =>
                        void openUrl(entry.pageUrl).catch((cause) =>
                          setMarketError(
                            `打不开详情页：${cause instanceof Error ? cause.message : String(cause)}`,
                          ),
                        )
                      }
                    >
                      <ExternalLink className="size-3.5" />
                    </Button>
                  </div>
                </div>
              </li>
            ))}
            {marketEntries.length === 0 && !marketLoading && !marketError ? (
              <li className="py-3 text-center text-xs text-muted-foreground">
                还没有条目。输入关键词搜一下。
              </li>
            ) : null}
          </ul>

          <div className="mt-3 flex items-center justify-between gap-2">
            <span className="text-xs text-muted-foreground">
              {marketLoading ? "正在拉取…" : `第 ${marketPage} 页`}
            </span>
            {marketEntries.length > 0 && marketPage * 20 < marketTotal ? (
              <Button
                variant="ghost"
                size="sm"
                disabled={marketLoading}
                onClick={() => void loadMarket(marketPage + 1, marketSort)}
              >
                加载更多
              </Button>
            ) : null}
          </div>
        </DialogContent>
      </Dialog>

      <Dialog
        open={dialogOpen}
        onOpenChange={(open) => {
          if (!open) setDialogOpen(false);
        }}
      >
        <DialogContent className="max-w-lg">
          <DialogTitle>从 cc-switch 导入技能</DialogTitle>
          <p className="mt-1 text-xs leading-5 text-muted-foreground">
            清单读自 cc-switch 的技能库，导入就是把它的技能目录整个复制进 aglab 的技能目录——
            cc-switch 那边不受影响。重名的不会覆盖，缺 SKILL.md 的没有正文可导。
          </p>

          {skillCandidatesError ? (
            <p className="mt-3 text-sm text-destructive">{skillCandidatesError}</p>
          ) : null}
          {skillImportNote ? (
            <p className="mt-3 text-xs leading-5 text-brand-text">{skillImportNote}</p>
          ) : null}

          {!skillCandidatesError && !skillCandidatesLoading && skillCandidates.length === 0 ? (
            <p className="mt-3 text-sm text-muted-foreground">
              cc-switch 里没有已安装的技能。
            </p>
          ) : null}

          <ul className="mt-3 max-h-72 space-y-1.5 overflow-y-auto">
            {skillCandidates.map((candidate) => {
              const blocked = !candidate.hasSkillMd || candidate.exists;
              const checked = selected.has(candidate.sourceId);
              const reason = !candidate.hasSkillMd
                ? "没有 SKILL.md"
                : candidate.exists
                  ? "aglab 里已有同名技能"
                  : "";
              return (
                <li
                  key={candidate.sourceId}
                  className={cn(
                    "flex items-start gap-2.5 rounded-lg border px-2.5 py-2",
                    blocked ? "border-border bg-surface opacity-60" : "border-border bg-background",
                  )}
                >
                  <button
                    type="button"
                    role="checkbox"
                    aria-checked={checked}
                    aria-label={`选择 ${candidate.name}`}
                    disabled={blocked}
                    onClick={() => flip(candidate.sourceId)}
                    className={cn(
                      "mt-0.5 size-3.5 shrink-0 rounded border transition-colors",
                      checked
                        ? "border-brand bg-brand text-2xs leading-none text-brand-foreground"
                        : "border-input bg-background",
                      blocked && "cursor-not-allowed",
                    )}
                  >
                    {checked ? "✓" : ""}
                  </button>
                  <div className="min-w-0 flex-1">
                    <p className="text-sm font-medium text-foreground">{candidate.name}</p>
                    {candidate.description ? (
                      <p className="mt-0.5 line-clamp-2 text-xs leading-5 text-muted-foreground">
                        {candidate.description}
                      </p>
                    ) : null}
                    {reason ? (
                      <p className="mt-0.5 text-xs text-warning">跳过：{reason}</p>
                    ) : null}
                  </div>
                </li>
              );
            })}
          </ul>

          <div className="mt-4 flex items-center justify-end gap-2">
            <span className="flex-1 text-xs text-muted-foreground">
              {importable.length > 0 ? `可导入 ${importable.length} 个` : "没有可导入的技能"}
            </span>
            <Button variant="ghost" size="sm" onClick={() => setDialogOpen(false)}>
              关闭
            </Button>
            <Button
              variant="brand"
              size="sm"
              disabled={importing || selected.size === 0}
              onClick={() => void submit()}
            >
              {importing ? "导入中…" : `导入所选 (${selected.size})`}
            </Button>
          </div>
        </DialogContent>
      </Dialog>
    </SectionFrame>
  );
}
