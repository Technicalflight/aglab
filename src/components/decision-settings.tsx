import { useEffect, useState, type ReactNode } from "react";
import { open as pickFolder } from "@tauri-apps/plugin-dialog";

import { CapabilityToggle } from "@/components/ui/capability-toggle";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  deleteJevKeyringKey,
  JevProvider,
  jevEndpointProblem,
  refreshJevKeyringState,
  setJevKeyringKey,
  type JevEndpointConfig,
} from "@/lib/decision";
import { useDecisionStore } from "@/store/decision-store";
import { cn } from "@/lib/utils";
import { FormColumn } from "@/components/ui/content-column";

const inputClass =
  "h-9 w-full rounded-lg border border-input bg-background px-3 text-base text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35";

function Row({
  title,
  description,
  note,
  children,
}: {
  title: string;
  description: string;
  note?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="flex items-center justify-between gap-6 border-b border-border px-1 py-4 last:border-b-0">
      <div className="min-w-0">
        <p className="text-base font-medium text-foreground">{title}</p>
        <p className="mt-0.5 text-xs leading-5 text-muted-foreground">{description}</p>
        {note}
      </div>
      <div className="w-[190px] shrink-0">{children}</div>
    </div>
  );
}

function Group({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="mt-8 first:mt-0">
      <h2 className="text-lg font-semibold tracking-tight text-foreground">{title}</h2>
      <div className="mt-2 rounded-lg border border-border bg-surface px-3">{children}</div>
    </div>
  );
}

/**
 * 一格数字。合并配置时对不合法的值是"整格退回默认"而不是钳到边界，
 * 所以输入侧必须先钳住——否则用户打进去的数会被静默换成别的数
 */
function NumField({
  value,
  min,
  max,
  step = 1,
  onCommit,
  label,
}: {
  value: number;
  min: number;
  max: number;
  step?: number;
  onCommit: (value: number) => void;
  /** 读屏名称：必填。这些控件复用多处，不写清楚用户不知道在改什么 */
  label: string;
}) {
  const [draft, setDraft] = useState(String(value));
  useEffect(() => setDraft(String(value)), [value]);
  return (
    <input
      type="number"
      aria-label={label}
      min={min}
      max={max}
      step={step}
      value={draft}
      onChange={(event) => {
        const next = Number(event.target.value);
        if (!Number.isFinite(next)) return;
        const clamped = Math.min(Math.max(next, min), max);
        setDraft(String(clamped));
        onCommit(clamped);
      }}
      className={inputClass}
    />
  );
}

/** 文本一格：逐键重建决策系统太贵（每次还要顺手探一遍 sidecar），所以失焦或回车才提交 */
function TextField({
  value,
  placeholder,
  mono = false,
  onCommit,
  label,
}: {
  value: string;
  placeholder?: string;
  mono?: boolean;
  onCommit: (value: string) => void;
  /** 读屏名称：必填。这些控件复用多处，不写清楚用户不知道在改什么 */
  label: string;
}) {
  const [draft, setDraft] = useState(value);
  useEffect(() => setDraft(value), [value]);
  function commit() {
    if (draft !== value) onCommit(draft);
  }
  return (
    <input
      type="text"
      aria-label={label}
      value={draft}
      placeholder={placeholder}
      spellCheck={false}
      onChange={(event) => setDraft(event.target.value)}
      onBlur={commit}
      onKeyDown={(event) => {
        if (event.key === "Enter") commit();
      }}
      className={cn(inputClass, "text-sm", mono && "font-mono")}
    />
  );
}

/** 密钥一格：同 TextField 的失焦提交纪律，只是不回显明文 */
function EndpointKeyField({
  value,
  placeholder,
  onCommit,
  label,
}: {
  value: string;
  placeholder: string;
  onCommit: (value: string) => void;
  /** 读屏名称：必填。这些控件复用多处，不写清楚用户不知道在改什么 */
  label: string;
}) {
  const [draft, setDraft] = useState(value);
  useEffect(() => setDraft(value), [value]);
  function commit() {
    if (draft !== value) onCommit(draft);
  }
  return (
    <input
      type="password"
      aria-label={label}
      value={draft}
      placeholder={placeholder}
      spellCheck={false}
      onChange={(event) => setDraft(event.target.value)}
      onBlur={commit}
      onKeyDown={(event) => {
        if (event.key === "Enter") commit();
      }}
      className={cn(inputClass, "h-8 text-sm")}
    />
  );
}

/**
 * 决策层的设置页。读数不住这里（在右栏 → 决策），这里只出旋钮。
 * 一格改完即写存储并重装系统实例——不留"改了要重启才认"的暗坑。
 */
export function DecisionSettings() {
  const system = useDecisionStore((s) => s.system);
  const patch = useDecisionStore((s) => s.patch);
  const config = system.config;
  /** 决策池逐条的形式校验。全员不合法（或池子空着）时它同时是"这一层为什么被跳过"的答案 */
  const poolProblems =
    config.jev.via === "custom"
      ? config.jev.endpoints.map((entry) => jevEndpointProblem(entry.baseUrl))
      : [];
  const anyEndpointValid =
    poolProblems.length > 0 && poolProblems.some((problem) => problem === null);
  const customProblem =
    config.jev.via === "custom" && !anyEndpointValid
      ? poolProblems.find((problem) => problem !== null) ?? "池子是空的，先加一条服务商"
      : null;
  const [secret, setSecret] = useState("");
  const [keyring, setKeyring] = useState<boolean | null>(null);
  const [keyNote, setKeyNote] = useState<string | null>(null);
  const [probe, setProbe] = useState<{ running: boolean; ok: boolean; text: string } | null>(null);

  useEffect(() => {
    void refreshJevKeyringState().then(setKeyring);
  }, [system]);

  /** 改决策池里的第 index 条：结构化克隆的 draft 上就地改，patch 负责落盘重装 */
  function patchEndpoint(index: number, next: Partial<JevEndpointConfig>) {
    patch((draft) => {
      const target = draft.jev.endpoints[index];
      if (target) Object.assign(target, next);
    });
  }

  async function writeKey() {
    const trimmed = secret.trim();
    if (!trimmed) {
      setKeyNote("要先写上密钥。");
      return;
    }
    try {
      await setJevKeyringKey(trimmed);
      setSecret("");
      setKeyNote(null);
      setKeyring(await refreshJevKeyringState());
    } catch (error) {
      setKeyNote(error instanceof Error ? error.message : String(error));
    }
  }

  async function eraseKey() {
    try {
      await deleteJevKeyringKey();
      setKeyNote(null);
      setKeyring(await refreshJevKeyringState());
    } catch (error) {
      setKeyNote(error instanceof Error ? error.message : String(error));
    }
  }

  /**
   * 按**存储里现在这份**配置现造一个 Provider 发一次最小判定。
   * 读 getState 而不是闭包里那份 config：从地址那格直接点到这颗按钮时，先触发的是
   * 失焦提交——拿旧快照会把"我刚填的那个地址"试成上一次的
   */
  async function probeJev() {
    const jev = useDecisionStore.getState().system.config.jev;
    setProbe({ running: true, ok: false, text: "发出去了……" });
    const provider = new JevProvider({ ...jev });
    // keyring 模式下显式密钥是空的，可用性要看那一个探测比特（原生侧请求时自己取密钥）
    provider.setKeyringAvailable(keyring === true);
    const started = Date.now();
    try {
      await provider.warmup();
      setProbe({ running: false, ok: true, text: `通了，${Date.now() - started} 毫秒。` });
    } catch (error) {
      setProbe({
        running: false,
        ok: false,
        text: `没通：${error instanceof Error ? error.message : String(error)}`,
      });
    }
  }

  async function pickDir() {
    try {
      const picked = await pickFolder({ directory: true, multiple: false, title: "选择 sidecar 目录" });
      if (typeof picked !== "string") return;
      patch((draft) => {
        draft.laya.sidecarDir = picked;
      });
    } catch {
      // 没有系统选择器：让上面那格手填
    }
  }

  return (
    <FormColumn>
      <h1 className="text-2xl font-semibold tracking-tight text-foreground">决策层</h1>
      <p className="mt-1 text-sm leading-6 text-muted-foreground">
        1. 打开「决策层」总开关，按嵌入点调阈值；
        2. 实时读数与 sidecar 启停在右栏「决策」看。
      </p>

      <Group title="总开关">
        <Row title="决策层" description="关掉后各嵌入点直接回到原来的路径，一次判定都不发">
          <div className="flex justify-end">
            <CapabilityToggle
              label="决策层"
              enabled={config.enabled}
              onToggle={() => patch((draft) => void (draft.enabled = !draft.enabled))}
            />
          </div>
        </Row>

        <Row
          title="敏感请求钉在本地"
          description="private / confidential 的判定只走本机 Laya，云端两层连被问的机会都没有"
          note={
            config.routing.sensitiveForceLocal ? null : (
              <p className="mt-1.5 text-xs leading-5 text-destructive">
                这一条现在是关的：敏感正文可能出本机。它是测试口子，不是给人用的档。
              </p>
            )
          }
        >
          <div className="flex justify-end">
            <span
              className={cn(
                "flex h-8 items-center rounded-lg border px-2.5 text-xs",
                config.routing.sensitiveForceLocal
                  ? "border-border text-muted-foreground"
                  : "border-destructive/45 text-destructive",
              )}
            >
              {config.routing.sensitiveForceLocal ? "常开" : "已被手改关闭"}
            </span>
          </div>
        </Row>
      </Group>

      <Group title="Laya · 本地层">
        <Row title="启用" description="没装 sidecar 时这一层每次都会失败，判定降级到云端或直接交白卷">
          <div className="flex justify-end">
            <CapabilityToggle
              label="启用 Laya"
              enabled={config.laya.enabled}
              onToggle={() =>
                patch((draft) => void (draft.laya.enabled = !draft.laya.enabled))
              }
            />
          </div>
        </Row>
        <Row title="服务商" description="sidecar 在听哪个地址。换端口要两边一致">
          <TextField
            label="Laya sidecar 地址"
            value={config.laya.sidecarEndpoint}
            mono
            onCommit={(value) => patch((draft) => void (draft.laya.sidecarEndpoint = value))}
          />
        </Row>
        <Row
          title="脚本目录"
          description="一键启动用它找 index.mjs：只存路径，不存脚本。选不对就起不来，aglab 不猜"
        >
          <div className="flex items-center gap-2">
            <div className="min-w-0 flex-1">
              <TextField
                label="Laya 脚本目录"
                value={config.laya.sidecarDir}
                placeholder="…/scripts/laya-sidecar"
                mono
                onCommit={(value) => patch((draft) => void (draft.laya.sidecarDir = value))}
              />
            </div>
            <Button variant="subtle" size="sm" onClick={() => void pickDir()}>
              选
            </Button>
          </div>
        </Row>
        <Row title="模型子目录" description="@receptron/laya 的 checkpoint 子目录">
          <TextField
            label="Laya 模型子目录" value={config.laya.subfolder} mono onCommit={(value) => patch((draft) => void (draft.laya.subfolder = value))} />
        </Row>
        <Row title="单层超时（毫秒）" description="超过就算这一层没答上，走漏斗下一格">
          <NumField
            label="Laya 单层超时毫秒"
            value={config.laya.timeoutMs}
            min={100}
            max={120_000}
            onCommit={(value) => patch((draft) => void (draft.laya.timeoutMs = value))}
          />
        </Row>
        <Row title="启动时预热" description="开应用时先探一次 /health，把 TCP 和模型都热上">
          <div className="flex justify-end">
            <CapabilityToggle
              label="启动时预热"
              enabled={config.laya.warmupOnStart}
              onToggle={() =>
                patch((draft) => void (draft.laya.warmupOnStart = !draft.laya.warmupOnStart))
              }
            />
          </div>
        </Row>
      </Group>

      <Group title="Jev · 云端层">
        <Row title="启用" description="出站判定。每一发都过出口域名名单">
          <div className="flex justify-end">
            <CapabilityToggle
              label="启用 Jev"
              enabled={config.jev.enabled}
              onToggle={() => patch((draft) => void (draft.jev.enabled = !draft.jev.enabled))}
            />
          </div>
        </Row>
        <Row
          title="厂商"
          description="内置两家的地址在原生侧；自定义服务商仍过出口名单"
        >
          <Select
            value={config.jev.via}
            onValueChange={(value) =>
              patch((draft) => {
                draft.jev.via =
                  value === "custom"
                    ? "custom"
                    : value === "openrouter"
                      ? "openrouter"
                      : "typesafe";
              })
            }
          >
            <SelectTrigger>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="typesafe">TypeSafe</SelectItem>
              <SelectItem value="openrouter">OpenRouter</SelectItem>
              <SelectItem value="custom">自定义服务商</SelectItem>
            </SelectContent>
          </Select>
        </Row>
        {config.jev.via === "custom" ? (
          <div className="border-b border-border px-1 py-4">
            <div className="flex items-center justify-between gap-6">
              <div className="min-w-0">
                <p className="text-base font-medium text-foreground">
                  决策池（{config.jev.endpoints.length}）
                </p>
                <p className="mt-0.5 text-xs leading-5 text-muted-foreground">
                  请求粘住上次成功的服务商，失败或超时按顺序换下一条，全挂才降级。
                  scheme + 主机 + 路径写全，http 只允许本机。每一发都过出口名单。
                </p>
                {customProblem ? (
                  <p className="mt-1.5 text-xs leading-5 text-destructive">
                    {customProblem}。云端那一层会被跳过。
                  </p>
                ) : null}
              </div>
              <Button
                variant="subtle"
                size="sm"
                onClick={() =>
                  patch((draft) => {
                    draft.jev.endpoints.push({
                      name: `服务商 ${draft.jev.endpoints.length + 1}`,
                      baseUrl: "",
                      apiKey: "",
                    });
                  })
                }
              >
                加一条
              </Button>
            </div>
            <div className="mt-2 space-y-2">
              {config.jev.endpoints.map((entry, index) => {
                const problem = jevEndpointProblem(entry.baseUrl);
                return (
                  <div
                    key={index}
                    className="rounded-lg border border-border bg-background px-3 py-2.5"
                  >
                    <div className="flex items-center gap-2">
                      <div className="w-28 shrink-0">
                        <TextField
                          label="端点名称"
                          value={entry.name}
                          placeholder={`服务商 ${index + 1}`}
                          onCommit={(value) => patchEndpoint(index, { name: value })}
                        />
                      </div>
                      <div className="min-w-0 flex-1">
                        <TextField
                          label="端点 Base URL"
                          value={entry.baseUrl}
                          placeholder="https://gw.example.com/v1/systemone"
                          mono
                          onCommit={(value) => patchEndpoint(index, { baseUrl: value })}
                        />
                      </div>
                      <Button
                        variant="ghost"
                        size="sm"
                        onClick={() =>
                          patch((draft) => void draft.jev.endpoints.splice(index, 1))
                        }
                      >
                        移除
                      </Button>
                    </div>
                    {problem ? (
                      <p className="mt-1.5 text-xs leading-5 text-destructive">
                        不合法：{problem}。尝试时会跳过这一条。
                      </p>
                    ) : null}
                    <div className="mt-2">
                      <EndpointKeyField
                        label="端点 API 密钥"
                        value={entry.apiKey}
                        placeholder="这条自己的密钥（留空用下面的全局密钥）"
                        onCommit={(value) => patchEndpoint(index, { apiKey: value })}
                      />
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        ) : null}
        <Row
          title="传输" description="rust 经原生转发（不看 CORS，但过出口名单）；direct 是 WebView 直连，留给联调">
          <Select
            value={config.jev.transport}
            onValueChange={(value) =>
              patch((draft) => void (draft.jev.transport = value === "direct" ? "direct" : "rust"))
            }
          >
            <SelectTrigger>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="rust">原生通道</SelectItem>
              <SelectItem value="direct">直连</SelectItem>
            </SelectContent>
          </Select>
        </Row>
        <Row
          title="试一发"
          description="按上面这几格发一次最小判定请求。会真出网，也真花一次额度"
          note={
            probe ? (
              <p
                className={cn(
                  "mt-1.5 text-xs",
                  probe.ok ? "text-muted-foreground" : "text-destructive",
                )}
              >
                {probe.text}
              </p>
            ) : null
          }
        >
          <div className="flex justify-end">
            <Button
              variant="subtle"
              size="sm"
              disabled={probe?.running}
              onClick={() => void probeJev()}
            >
              {probe?.running ? "发送中" : "试一发"}
            </Button>
          </div>
        </Row>
        <Row title="密钥走系统凭据库" description="开了它下面那格作废：密钥不过 IPC、不进界面内存">
          <div className="flex justify-end">
            <CapabilityToggle
              label="密钥走凭据库"
              enabled={config.jev.useKeyring}
              onToggle={() => patch((draft) => void (draft.jev.useKeyring = !draft.jev.useKeyring))}
            />
          </div>
        </Row>
        <Row
          title="凭据库里的密钥"
          description={
            keyring === null ? "还没探过" : keyring ? "已有一条，请求时由原生侧自取" : "空的"
          }
          note={keyNote ? <p className="mt-1.5 text-xs text-destructive">{keyNote}</p> : null}
        >
          <div className="flex items-center gap-2">
            <input
              type="password"
              value={secret}
              placeholder="sk-…"
                aria-label="API 密钥"
              onChange={(event) => setSecret(event.target.value)}
              className={`${inputClass} h-8 text-sm`}
            />
            <Button variant="subtle" size="sm" onClick={() => void writeKey()}>
              写入
            </Button>
            <Button variant="ghost" size="sm" disabled={!keyring} onClick={() => void eraseKey()}>
              清除
            </Button>
          </div>
        </Row>
        <Row
          title="显式密钥"
          description="写进本地存储的明文，只建议联调用"
          note={
            config.jev.apiKey ? (
              <p className="mt-1.5 text-xs text-warning">这一格现在有明文密钥。</p>
            ) : null
          }
        >
          <TextField
            label="Jev 显式密钥"
            value={config.jev.apiKey}
            placeholder="留空"
            mono
            onCommit={(value) => patch((draft) => void (draft.jev.apiKey = value))}
          />
        </Row>
        <Row title="超时（毫秒）" description="单次出站判定的上限">
          <NumField
            label="Jev 超时毫秒"
            value={config.jev.timeoutMs}
            min={100}
            max={120_000}
            onCommit={(value) => patch((draft) => void (draft.jev.timeoutMs = value))}
          />
        </Row>
      </Group>

      <Group title="漏斗">
        <Row title="升级阈值" description="全部问题里最不确信的那个达到它才算拍板">
          <NumField
            label="自动升级阈值"
            value={config.routing.autoUpgradeThreshold}
            min={0}
            max={1}
            step={0.01}
            onCommit={(value) => patch((draft) => void (draft.routing.autoUpgradeThreshold = value))}
          />
        </Row>
        <Row title="最多走几层" description="本地→云端→System 2 是 3 层">
          <NumField
            label="最多升级层数"
            value={config.routing.maxUpgradeChain}
            min={1}
            max={8}
            onCommit={(value) => patch((draft) => void (draft.routing.maxUpgradeChain = value))}
          />
        </Row>
        <Row title="决策缓存" description="同样的输入在 TTL 内不重问">
          <div className="flex justify-end">
            <CapabilityToggle
              label="决策缓存"
              enabled={config.cache.enabled}
              onToggle={() => patch((draft) => void (draft.cache.enabled = !draft.cache.enabled))}
            />
          </div>
        </Row>
        <Row title="缓存存活（毫秒）" description="判定不该被一份旧答案锁住太久">
          <NumField
            label="决策缓存存活毫秒"
            value={config.cache.ttlMs}
            min={0}
            max={3_600_000}
            step={1000}
            onCommit={(value) => patch((draft) => void (draft.cache.ttlMs = value))}
          />
        </Row>
        <Row title="决策审计" description="侧栏那块面板吃的是它">
          <div className="flex justify-end">
            <CapabilityToggle
              label="决策审计"
              enabled={config.audit.enabled}
              onToggle={() => patch((draft) => void (draft.audit.enabled = !draft.audit.enabled))}
            />
          </div>
        </Row>
        <Row title="审计保留条数" description="环形缓冲的上限">
          <NumField
            label="审计保留条数"
            value={config.audit.maxEntries}
            min={1}
            max={10_000}
            step={50}
            onCommit={(value) => patch((draft) => void (draft.audit.maxEntries = value))}
          />
        </Row>
        <Row title="private 预览字符" description="审计里给 private 级 state 留多少字。confidential 恒为哈希">
          <NumField
            label="private 预览字符数"
            value={config.audit.redactPrivatePreviewChars}
            min={0}
            max={10_000}
            step={50}
            onCommit={(value) =>
              patch((draft) => void (draft.audit.redactPrivatePreviewChars = value))
            }
          />
        </Row>
      </Group>

      <Group title="嵌入点">
        <Row title="记忆提取门控" description="每轮收尾先在本地判值不值得提取，不值得就省掉一次云端提取">
          <div className="flex justify-end">
            <CapabilityToggle
              label="记忆提取门控"
              enabled={config.integrations.memoryGate}
              onToggle={() =>
                patch((draft) => void (draft.integrations.memoryGate = !draft.integrations.memoryGate))
              }
            />
          </div>
        </Row>
        <Row title="门控阈值" description="P(值得记住) 低于它就不提取">
          <NumField
            label="记忆提取门控阈值"
            value={config.integrations.memoryGateThreshold}
            min={0}
            max={1}
            step={0.05}
            onCommit={(value) =>
              patch((draft) => void (draft.integrations.memoryGateThreshold = value))
            }
          />
        </Row>
        <Row title="记忆敏感分级" description="入库后判一次密，判高了当场降档，只降不升">
          <div className="flex justify-end">
            <CapabilityToggle
              label="记忆敏感分级"
              enabled={config.integrations.sensitivityScan}
              onToggle={() =>
                patch((draft) =>
                  void (draft.integrations.sensitivityScan = !draft.integrations.sensitivityScan),
                )
              }
            />
          </div>
        </Row>
        <Row title="逐消息模型路由" description="只把判定记进审计，不改当前档案的模型选择">
          <div className="flex justify-end">
            <CapabilityToggle
              label="逐消息模型路由"
              enabled={config.integrations.modelRouting}
              onToggle={() =>
                patch((draft) =>
                  void (draft.integrations.modelRouting = !draft.integrations.modelRouting),
                )
              }
            />
          </div>
        </Row>
        <Row
          title="助理分配"
          description="编排监督者补做节点时，问决策层该派谁（只读侦察/动手执行/只读复核）。答不上照旧动手执行"
        >
          <div className="flex justify-end">
            <CapabilityToggle
              label="助理分配"
              enabled={config.integrations.taskAssignment}
              onToggle={() =>
                patch((draft) =>
                  void (draft.integrations.taskAssignment = !draft.integrations.taskAssignment),
                )
              }
            />
          </div>
        </Row>
        <Row
          title="上下文相关性"
          description="记忆注入前给候选打一次相关性分、按分重排。分数钉在本地出（private），一条都没答齐就照原序"
        >
          <div className="flex justify-end">
            <CapabilityToggle
              label="上下文相关性"
              enabled={config.integrations.contextRelevance}
              onToggle={() =>
                patch((draft) =>
                  void (draft.integrations.contextRelevance = !draft.integrations.contextRelevance),
                )
              }
            />
          </div>
        </Row>
      </Group>

      <p className="mt-4 text-xs leading-5 text-muted-foreground">
        这一页写在 <span className="font-mono">localStorage</span> 的{" "}
        <span className="font-mono">aglab.decisionLayer.config</span>，不进 config.json。
      </p>
    </FormColumn>
  );
}
