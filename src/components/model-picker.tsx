import { useState } from "react";
import { IconCheck as Check, IconChevronRight as ChevronRight, IconPlus as Plus, IconRefresh as RefreshCw, IconSearch as Search, IconTrash as Trash2 } from "@tabler/icons-react";
import { Slider as SliderPrimitive } from "radix-ui";

import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { type AppConfig, EFFORT_LEVELS, effortIndex, effortLabel, switchPinnedMember } from "@/types/chat";
import { useChatStore } from "@/store/chat-store";
import { ModelIcon } from "@/components/model-icon";
import { classifyModelCapabilities } from "@/lib/model-capabilities";
import { cn } from "@/lib/utils";

const MAX_EFFORT_INDEX = EFFORT_LEVELS.length - 1;

const MODE_LABELS: Record<string, string> = {
  auto: "自动调度",
  pinned: "手动指定",
  decision: "决策层调度",
};

/** 池感知的模型展示文案：选择器触发器与空对话 hero 共用这一份，
 *  两处说的永远是同一句话——池子开着时顶层 model 不再是"这一发用谁"的真相 */
export function modelDisplayLabel(config: AppConfig): string {
  const pool = config.modelPool;
  if (pool.mode !== "off") {
    if (pool.mode === "pinned" && pool.pinned) return pool.pinned.model;
    return `池 · ${MODE_LABELS[pool.mode] ?? pool.mode}`;
  }
  return config.model || "未选择模型";
}

/** 会话档当前的"模型行"：视频画布跟四类页签、音乐跟子模式、生图档固定 image 键。
 *  kindKey 为 null = 该会话没有档位模型行（对话会话），显示回退 modelDisplayLabel；
 *  model 为空串 = 该档还没选模型——调用方显示占位文案，别拿全局 model 顶替
 *  （那是上一个模式留下的，音乐会话显示视频模型是撒谎，真机踩过） */
export function useKindModelLine(): { kindKey: string | null; model: string } {
  const config = useChatStore((s) => s.config);
  const kind = useChatStore((s) => s.kind);
  const videoGenerationType = useChatStore((s) => s.videoGenerationType);
  const musicSubMode = useChatStore((s) => s.musicSubMode);
  const kindKey =
    kind === "video"
      ? videoGenerationType === "text"
        ? "chat"
        : videoGenerationType
      : kind === "music"
        ? musicSubMode === "write"
          ? "chat"
          : "music"
        : kind === "image"
          ? "image"
          : null;
  return { kindKey, model: (kindKey && config.kindModels?.[kindKey]) || "" };
}

/** 档位模型行空着时的占位文案：明说缺什么 */
export function kindModelPlaceholder(kindKey: string): string {
  return kindKey === "image"
    ? "选择生图模型"
    : kindKey === "video"
      ? "选择视频模型"
      : kindKey === "audio" || kindKey === "music"
        ? "选择音频模型"
        : "选择对话模型";
}

/** 模糊匹配：不区分大小写的子序列——"gptimg" 命中 gpt-image-2.5，"nb2" 命中 nano-banana-2。
 *  模型名里符号多（-、.、/），连续子串太苛刻，按序出现即可 */
function fuzzyMatch(needle: string, haystack: string): boolean {
  const n = needle.trim().toLowerCase();
  if (!n) return true;
  let cursor = 0;
  for (const ch of haystack.toLowerCase()) {
    if (ch === n[cursor]) cursor += 1;
    if (cursor === n.length) return true;
  }
  return false;
}

export function ModelPicker() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const models = useChatStore((s) => s.models);
  const loadingModels = useChatStore((s) => s.loadingModels);
  const loadModels = useChatStore((s) => s.loadModels);
  // 会话的能力档：生图/视频会话里，选择器把有对应能力的模型排成一组置顶
  const kind = useChatStore((s) => s.kind);
  // 视频画布当前的生成类型（四类页签）：候选过滤与选中写入都跟它走——
  // 文本页签看对话模型、图片页签看生图模型，而不是被会话档锁死在视频模型
  const videoGenerationType = useChatStore((s) => s.videoGenerationType);
  // 选择器内搜索：命中模型名或服务商名；关层即清，下次打开从空开始
  const [query, setQuery] = useState("");

  const pool = config.modelPool;
  // 池开着时，顶层 model 不再是"这一发用谁"的真相：选择器改说池子的话
  const poolOn = pool.mode !== "off";
  const enabledMembers = pool.members.filter((member) => member.enabled);
  const profileName = (profileId: string) => {
    if (profileId === "") return "当前连接";
    return config.profiles.find((profile) => profile.id === profileId)?.name ?? "已删除的档案";
  };

  // 当前服务商（生效档案）勾选的模型：它们是"这一发"的天然候选，该跟池成员一起
  // 出现在选择器里。去重键是（档案 id + 模型名）——同档案同名只显示一次，
  // 不同档案的同名模型是不同来源（密钥/地址不同），不并
  const activeProfileId = config.activeProfileId ?? "";
  const memberKeys = new Set(
    pool.members.map((member) => `${member.profileId}\u{0}${member.model}`),
  );

  // 能力会话：候选里只留有对应能力的模型（配置标过以配置为准，没标的按模型名
  // 启发式识别）。对话会话不筛——对话是兜底能力，所有模型都是候选。
  // 视频会话跟随四类页签：文本页签看对话模型、音频页签看音频模型
  const musicSubMode = useChatStore((s) => s.musicSubMode);
  const wanted: string =
    kind === "video"
      ? videoGenerationType === "text"
        ? "chat"
        : videoGenerationType
      : kind === "image"
        ? "image"
        : kind === "music"
          ? musicSubMode === "write"
            ? "chat"
            : "audio"
          : "chat";
  const sectionLabel =
    wanted === "image"
      ? "生图模型"
      : wanted === "video"
        ? "视频模型"
        : wanted === "audio"
          ? "音频模型"
          : "对话模型";
  // 能力判定按**副本**走：不同供应商的同名模型是不同来源（密钥/端点/标注都可能不同），
  // 各自读自己档案里那一行的标注；该行没标才退回按名字的启发式。
  // 不做按名字的合并判定——A 家标了生图不该替 B 家的同名模型说话
  const matchesSession = (profileId: string, modelName: string) => {
    const spec = config.profiles
      .find((profile) => profile.id === profileId)
      ?.models.find((item) => item.model === modelName);
    const caps = spec?.capabilities?.length
      ? spec.capabilities
      : classifyModelCapabilities(modelName);
    // 对话会话排除生成模型（生图/视频各归各的会话）；媒体会话只留对应能力。
    // 没标注的模型按名字启发式——GLM/grok 这类落在对话，nano-banana 落在生图
    if (wanted === "chat") {
      return (
        !caps.includes("image") &&
        !caps.includes("video") &&
        !caps.includes("audio")
      );
    }
    return caps.includes(wanted);
  };

  const profileModels = config.models
    .map((spec) => spec.model.trim())
    .filter((model) => model !== "" && !memberKeys.has(`${activeProfileId}\u{0}${model}`))
    .filter((model) => matchesSession(activeProfileId, model));

  // 其他档案勾选的模型：池下拉原本只给"当前服务商"入口，别的档案（比如带订阅
  // 凭据的那张）的模型就没有出现的地方——按档案列出所有不在池里的候选，
  // 点按同样收进池并固定。去重键同上（档案 id + 模型名）
  const otherProfileModels: { profileId: string; model: string; profileLabel: string }[] = [];
  for (const profile of config.profiles) {
    if (profile.id === activeProfileId) continue;
    for (const spec of profile.models) {
      const model = spec.model.trim();
      if (model === "" || memberKeys.has(`${profile.id}\u{0}${model}`)) continue;
      if (!matchesSession(profile.id, model)) continue;
      if (
        otherProfileModels.some(
          (existing) => existing.profileId === profile.id && existing.model === model,
        )
      ) {
        continue;
      }
      otherProfileModels.push({ profileId: profile.id, model, profileLabel: profile.name });
    }
  }

  // 搜索过滤：三段候选各滤各的，段落头随空段隐藏；全空时给一条"没有匹配"
  const activeProfileLabel = profileName(activeProfileId);
  const searchHit = (model: string, profileLabel: string) =>
    fuzzyMatch(query, model) || fuzzyMatch(query, profileLabel);
  const sessionMembers = enabledMembers.filter((member) =>
    matchesSession(member.profileId, member.model),
  );
  const visibleMembers = sessionMembers.filter((member) =>
    searchHit(member.model, profileName(member.profileId)),
  );
  const profileModelsFiltered = profileModels.filter((model) =>
    searchHit(model, activeProfileLabel),
  );
  const otherProfileModelsFiltered = otherProfileModels.filter((entry) =>
    searchHit(entry.model, entry.profileLabel),
  );
  const noSearchHit =
    query.trim() !== "" &&
    visibleMembers.length === 0 &&
    profileModelsFiltered.length === 0 &&
    otherProfileModelsFiltered.length === 0 &&
    (enabledMembers.length > 0 || profileModels.length > 0 || otherProfileModels.length > 0);

  // 池关闭时的候选：服务商拉到的列表打头，顶层手填的模型名不在列表里也要给一个入口
  const options = config.model && !models.includes(config.model)
    ? [config.model, ...models]
    : models.length > 0
      ? models
      : [config.model].filter(Boolean);
  const visibleOptions = options.filter((model) => fuzzyMatch(query, model));

  const effort = effortIndex(config.reasoningEffort);
  const atMax = effort === MAX_EFFORT_INDEX;

  // 触发器显示"下一发真正用的模型"。视频画布的生成走 media_generate（不吃池），
  // 分母是当前页签的模型行：文本页签显示对话模型行，与发送用的同一格读数——
  // 页签一切过去就实时跟着换，而不是上一类的残留。档位行空着且全局模型能力
  // 对得上才顶替，对不上就明说"选择××模型"（硬发本来也会失败）
  const { kindKey, model: kindModel } = useKindModelLine();
  const generationModel =
    kindKey == null
      ? null
      : kindModel ||
        (config.model && matchesSession(activeProfileId, config.model) ? config.model : "");
  const triggerLabel =
    kindKey == null
      ? modelDisplayLabel(config)
      : generationModel || kindModelPlaceholder(kindKey);

  // 在当前会话档里选模型：除了换 model 本身，还记进该档的 kindModels——
  // 切换会话档时 syncModelForKind 才有"这一档上次用谁"可换。
  // 池手动指定时 pinned 成对换（switchPinnedMember）：只换模型名会写出幽灵组合，
  // Rust 按对校验成员表，每发必报"不在池里"——本函数目前只在池关分支用到，
  // 防的是将来有人把它接回池开的路径
  const pickModel = (model: string) => {
    const nextPool = switchPinnedMember(pool, model);
    void updateConfig({
      model,
      kindModels: {
        ...config.kindModels,
        [kind === "video"
          ? videoGenerationType === "text"
            ? "chat"
            : videoGenerationType
          : kind === "music"
            ? musicSubMode === "write"
              ? "chat"
              : "music"
            : kind || "chat"]: model,
      },
      ...(nextPool ? { modelPool: nextPool } : {}),
    });
  };

  const pinMember = (profileId: string, model: string) =>
    void updateConfig({
      model,
      kindModels: {
        ...config.kindModels,
        [kind === "video"
          ? videoGenerationType === "text"
            ? "chat"
            : videoGenerationType
          : kind === "music"
            ? musicSubMode === "write"
              ? "chat"
              : "music"
            : kind || "chat"]: model,
      },
      modelPool: { ...pool, mode: "pinned", pinned: { profileId, model } },
    });

  // 档案勾选的模型未必在池里，而 Rust 的 pinned 只认池成员表——
  // 点它就先收进池子（默认启用、权重 1）再固定，一步到位不绕设置页。
  // 当前服务商与别的档案的模型走同一个入口
  const pinPoolCandidate = (profileId: string, model: string) => {
    const members = pool.members.some(
      (member) => member.profileId === profileId && member.model === model,
    )
      ? pool.members
      : [...pool.members, { profileId, model, weight: 1, enabled: true }];
    void updateConfig({
      model,
      kindModels: {
        ...config.kindModels,
        [kind === "video"
          ? videoGenerationType === "text"
            ? "chat"
            : videoGenerationType
          : kind === "music"
            ? musicSubMode === "write"
              ? "chat"
              : "music"
            : kind || "chat"]: model,
      },
      modelPool: { ...pool, members, mode: "pinned", pinned: { profileId, model } },
    });
  };

  // 选择器里直接移除池成员。被移除的若正是当前固定者，必须一并放手——
  // Rust 的 pinned 只认池成员表，留着会每发必报"手动指定的池成员已经不在池里了"，
  // 顺手恢复自动调度（与右上角"恢复自动调度"按钮同一语义）
  const removeMember = (profileId: string, model: string) => {
    const members = pool.members.filter(
      (member) => !(member.profileId === profileId && member.model === model),
    );
    const wasPinned =
      pool.mode === "pinned" &&
      pool.pinned != null &&
      pool.pinned.profileId === profileId &&
      pool.pinned.model === model;
    void updateConfig({
      modelPool: wasPinned ? { ...pool, members, mode: "auto" } : { ...pool, members },
    });
  };

  // 搜索框：池开/关两分支共用一份
  const searchBox = (
    <div className="relative mt-1.5">
      <Search className="pointer-events-none absolute left-2 top-1/2 size-3 -translate-y-1/2 text-muted-foreground/60" />
      <input
        type="text"
        value={query}
        aria-label="搜索模型或服务商"
        placeholder="搜索模型或服务商…"
        onChange={(event) => setQuery(event.target.value)}
        className="h-7 w-full rounded-lg border border-input bg-background pl-7 pr-2 text-xs text-foreground outline-none transition-colors placeholder:text-muted-foreground/60 focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35"
      />
    </div>
  );

  return (
    <Popover onOpenChange={(open) => { if (!open) setQuery(""); }}>
      <PopoverTrigger
        type="button"
        className="group flex h-8 items-center gap-1.5 rounded-lg border border-transparent px-2 text-sm text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45 data-[state=open]:bg-accent data-[state=open]:text-foreground"
      >
        {/* 占位状态（该档还没选模型）不显示任何模型的图标——那是上一档的残留 */}
        {generationModel ? <ModelIcon model={generationModel} size={13} /> : null}
        <span className="max-w-[180px] truncate text-foreground">{triggerLabel}</span>
        {/* 思考档是对话/推理模型的旋钮；生图/视频会话的触发器不再显示它。
            视频画布的文本页签走的是对话模型，思考档对它有意义——照对话会话显示 */}
        {kind === "chat" || (kind === "video" && videoGenerationType === "text") ? (
          <>
            <span className="opacity-45">·</span>
            <span className={cn(atMax && "text-brand-text")}>{effortLabel(config.reasoningEffort)}</span>
          </>
        ) : null}
        <ChevronRight className="size-3.5 transition-transform group-aria-expanded:rotate-90" />
      </PopoverTrigger>

      <PopoverContent align="end" className="w-80">
        {poolOn ? (
          // 不带 border-b：思考程度区自己有 border-t，两道叠一起就是双线
          <div>
            <div className="flex items-center justify-between">
              <p className="text-xs text-muted-foreground">
                模型池 · {MODE_LABELS[pool.mode] ?? pool.mode}
              </p>
              {pool.mode === "pinned" ? (
                <button
                  type="button"
                  onClick={() => void updateConfig({ modelPool: { ...pool, mode: "auto" } })}
                  className="rounded px-1.5 py-0.5 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
                >
                  恢复自动调度
                </button>
              ) : (
                <p className="text-2xs text-muted-foreground/70">点成员即固定用它</p>
              )}
            </div>
            {searchBox}
            <div className="mt-1.5 max-h-52 space-y-0.5 overflow-y-auto">
              {enabledMembers.length === 0 ? (
                <p className="px-1.5 py-1.5 text-xs text-muted-foreground">
                  池里还没有启用的成员，去「设置 → 模型池」添加。
                </p>
              ) : kind !== "chat" && sessionMembers.length === 0 ? (
                <p className="px-1.5 py-1.5 text-xs leading-5 text-muted-foreground">
                  池里没有{wanted === "image" ? "生图" : wanted === "audio" ? "音频" : "视频"}模型——去「设置 → 服务商档案」给模型勾上生成能力，或把名字带
                  {wanted === "image" ? "seedream / cogview" : wanted === "audio" ? "tts / speech / audio / music / mureka" : "kling / sora"} 等的模型收进池。
                </p>
              ) : visibleMembers.length > 0 ? (
                visibleMembers.map((member) => {
                  const active =
                    pool.mode === "pinned" &&
                    pool.pinned != null &&
                    pool.pinned.profileId === member.profileId &&
                    pool.pinned.model === member.model;
                  const key = `${member.profileId}\u{0}${member.model}`;
                  return (
                    // 行壳只管悬停高亮：主体点按=固定，右侧删除钮悬停显出——
                    // 按钮不能嵌按钮，一行两个动作就得拆成壳+两兄弟按钮
                    <div
                      key={key}
                      className="group flex w-full items-center gap-1 rounded-lg pl-2 pr-1 text-left text-base transition-colors hover:bg-accent"
                    >
                      <button
                        type="button"
                        onClick={() => pinMember(member.profileId, member.model)}
                        className={cn(
                          // button 不继承 text-align（UA 默认 center），左对齐要显式给
                          "min-w-0 flex-1 cursor-pointer truncate rounded-lg py-1.5 text-left outline-none focus-visible:ring-2 focus-visible:ring-ring/45",
                          active ? "text-foreground" : "text-muted-foreground",
                        )}
                      >
                        <span className="flex min-w-0 items-center gap-1.5">
                          <ModelIcon model={member.model} size={13} />
                          <span className="truncate">{member.model}</span>
                        </span>
                        <span className="ml-1.5 shrink-0 text-2xs text-muted-foreground/60">
                          {profileName(member.profileId)}
                        </span>
                      </button>
                      {active ? <Check className="size-3.5 shrink-0 text-brand-text" /> : null}
                      <button
                        type="button"
                        title={
                          active
                            ? "从模型池移除（它是当前固定成员，移除后恢复自动调度）"
                            : "从模型池移除"
                        }
                        aria-label={`从模型池移除 ${member.model}`}
                        onClick={() => removeMember(member.profileId, member.model)}
                        className="flex size-6 shrink-0 cursor-pointer items-center justify-center rounded-md text-muted-foreground/50 opacity-0 transition-[opacity,background-color,color] hover:bg-accent hover:text-foreground focus-visible:opacity-100 focus-visible:ring-2 focus-visible:ring-ring/45 group-hover:opacity-100"
                      >
                        <Trash2 className="size-3.5" />
                      </button>
                    </div>
                  );
                })
              ) : null}

              {profileModelsFiltered.length > 0 ? (
                <>
                  <p className="px-1.5 pb-1 pt-2.5 text-2xs font-medium tracking-[0.06em] text-muted-foreground/70 uppercase">
                    当前服务商的模型
                  </p>
                  {profileModelsFiltered.map((model) => (
                    <button
                      key={model}
                      type="button"
                      title="点按收进模型池并固定用它"
                      onClick={() => pinPoolCandidate(activeProfileId, model)}
                      className="flex w-full items-center justify-between gap-3 rounded-lg px-2 py-1.5 text-left text-base text-muted-foreground outline-none transition-colors hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45"
                    >
                      <span className="flex min-w-0 items-center gap-1.5">
                        <ModelIcon model={model} size={13} />
                        <span className="truncate">{model}</span>
                        {/* 与池成员/其他档案两段同款：行尾跟服务商名，三段口径一致 */}
                        <span className="ml-1.5 shrink-0 text-2xs text-muted-foreground/60">
                          {activeProfileLabel}
                        </span>
                      </span>
                      <Plus className="size-3.5 shrink-0 text-muted-foreground/60" />
                    </button>
                  ))}
                </>
              ) : null}

              {otherProfileModelsFiltered.length > 0 ? (
                <>
                  <p className="px-1.5 pb-1 pt-2.5 text-2xs font-medium tracking-[0.06em] text-muted-foreground/70 uppercase">
                    其他档案的模型
                  </p>
                  {otherProfileModelsFiltered.map(({ profileId, model, profileLabel }) => (
                    <button
                      key={`${profileId}\u{0}${model}`}
                      type="button"
                      title="点按收进模型池并固定用它"
                      onClick={() => pinPoolCandidate(profileId, model)}
                      className="flex w-full items-center justify-between gap-3 rounded-lg px-2 py-1.5 text-left text-base text-muted-foreground outline-none transition-colors hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45"
                    >
                      <span className="flex min-w-0 items-center gap-1.5">
                        <ModelIcon model={model} size={13} />
                        <span className="truncate">{model}</span>
                        <span className="ml-1.5 shrink-0 text-2xs text-muted-foreground/60">
                          {profileLabel}
                        </span>
                      </span>
                      <Plus className="size-3.5 shrink-0 text-muted-foreground/60" />
                    </button>
                  ))}
                </>
              ) : null}
              {noSearchHit ? (
                <p className="px-1.5 py-1.5 text-xs text-muted-foreground">
                  没有匹配「{query.trim()}」的模型或服务商。
                </p>
              ) : null}
            </div>
          </div>
        ) : (
          <>
            <div className="flex items-center justify-between">
              <p className="text-xs text-muted-foreground">选择模型</p>
              <button
                type="button"
                aria-label="拉取模型列表"
                onClick={() => void loadModels()}
                disabled={loadingModels}
                className="flex size-6 items-center justify-center rounded-lg text-muted-foreground transition-colors hover:bg-accent hover:text-foreground disabled:opacity-45"
              >
                <RefreshCw className={cn("size-3", loadingModels && "animate-spin")} />
              </button>
            </div>

            {searchBox}
            <div className="mt-1.5 max-h-44 space-y-0.5 overflow-y-auto">
              {options.length === 0 ? (
                <p className="px-1.5 py-2 text-xs text-muted-foreground">
                  还没有拉到模型列表，可点右上角刷新，或在「设置」里填写 Base URL。
                </p>
              ) : visibleOptions.length === 0 ? (
                <p className="px-1.5 py-2 text-xs text-muted-foreground">
                  没有匹配「{query.trim()}」的模型。
                </p>
              ) : kind !== "chat" ? (
                (() => {
                  const capable = visibleOptions.filter((model) => matchesSession(activeProfileId, model));
                  const modelButton = (model: string) => (
                    <button
                      key={model}
                      type="button"
                      onClick={() => pickModel(model)}
                      className={cn(
                        "flex w-full items-center justify-between gap-3 rounded-lg px-2 py-1.5 text-left text-base outline-none transition-colors hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45",
                        model === config.model ? "text-foreground" : "text-muted-foreground",
                      )}
                    >
                      <span className="truncate">{model}</span>
                      {model === config.model ? (
                        <Check className="size-3.5 shrink-0 text-brand-text" />
                      ) : null}
                    </button>
                  );
                  return capable.length > 0 ? (
                    <>
                      <p className="px-1.5 pb-1 pt-1 text-2xs font-medium tracking-[0.06em] text-muted-foreground/70 uppercase">
                        {sectionLabel}
                      </p>
                      {capable.map(modelButton)}
                    </>
                  ) : (
                    <p className="px-1.5 py-1 text-xs leading-5 text-muted-foreground">
                      当前列表里没有识别到{sectionLabel}——去「设置 → 服务商档案」给模型勾上生成能力，
                      或选用名字带 seedream / cogview / kling / sora 等的模型。
                    </p>
                  );
                })()
              ) : (
                visibleOptions.map((model) => (
                  <button
                    key={model}
                    type="button"
                    onClick={() => pickModel(model)}
                    className={cn(
                      "flex w-full items-center justify-between gap-3 rounded-lg px-2 py-1.5 text-left text-base outline-none transition-colors hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45",
                      model === config.model ? "text-foreground" : "text-muted-foreground",
                    )}
                  >
                    <span className="truncate">{model}</span>
                    {model === config.model ? (
                      <Check className="size-3.5 shrink-0 text-brand-text" />
                    ) : null}
                  </button>
                ))
              )}
            </div>
          </>
        )}

        {/* 思考程度是对话/推理模型的旋钮：生图/视频模型不吃这个参数
            （生成管线根本不发送它），媒体会话里整块不显示 */}
        {kind === "chat" ? (
          <div className="mt-3 border-t border-border pt-3">
            <div className="flex items-center justify-between">
              <p className="text-xs text-muted-foreground">思考程度</p>
              <p className={cn("text-xs", atMax ? "text-brand-text" : "text-muted-foreground")}>
                {effortLabel(config.reasoningEffort)}
              </p>
            </div>

            <EffortSlider index={effort} atMax={atMax} onChange={updateConfig} />

            <div className="mt-2 flex justify-between text-2xs text-muted-foreground">
              {EFFORT_LEVELS.map((level, index) => (
                <span
                  key={level.value || "default"}
                  className={cn(index === effort && "text-foreground")}
                >
                  {level.label}
                </span>
              ))}
            </div>
          </div>
        ) : null}
      </PopoverContent>
    </Popover>
  );
}

function EffortSlider({
  index,
  atMax,
  onChange,
}: {
  index: number;
  atMax: boolean;
  onChange: (patch: { reasoningEffort: string }) => Promise<unknown>;
}) {
  return (
    <div className="relative mt-3">
      <SliderPrimitive.Root
        value={[index]}
        min={0}
        max={MAX_EFFORT_INDEX}
        step={1}
        aria-label="思考程度"
        className="relative flex h-7 w-full items-center"
        onValueChange={([value]) => void onChange({ reasoningEffort: EFFORT_LEVELS[value].value })}
      >
        <SliderPrimitive.Track className="relative h-3 grow overflow-hidden rounded-lg bg-muted">
          {/* 填充交给 Radix 的 Range，宽度和滑块位置由它算，避免自己拼 calc 对不齐 */}
          <SliderPrimitive.Range
            className={cn("absolute h-full bg-brand", atMax && "bg-ripple animate-ripple")}
          />
        </SliderPrimitive.Track>
        <SliderPrimitive.Thumb className="z-10 block size-6 shrink-0 cursor-grab rounded-full border border-border bg-foreground outline-none transition-[color,box-shadow] focus-visible:ring-2 focus-visible:ring-ring/45 active:cursor-grabbing" />
      </SliderPrimitive.Root>

      {/* 内缩量 = 滑块半径：Thumb size-6 半径 12px，档位点要对在滑块真实行程上 */}
      <div className="pointer-events-none absolute inset-x-[12px] top-1/2 h-0">
        {EFFORT_LEVELS.map((level, stop) => (
          <span
            key={level.value || "default"}
            className="absolute size-1 -translate-x-1/2 -translate-y-1/2 rounded-lg bg-background/75"
            style={{ left: `${(stop / MAX_EFFORT_INDEX) * 100}%` }}
          />
        ))}
      </div>
    </div>
  );
}
