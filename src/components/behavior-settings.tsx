import { useEffect, useState } from "react";
import { IconExternalLink as ExternalLink } from "@tabler/icons-react";
import { revealItemInDir } from "@tauri-apps/plugin-opener";

import { CapabilityToggle } from "@/components/ui/capability-toggle";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { CharLimit, Group, NumberField, Row, SettingsHeader } from "@/components/settings-ui";
import { fetchConfigPath } from "@/lib/chat-transport";
import { usePermissionSwitch } from "@/lib/use-permission-switch";
import { useChatStore } from "@/store/chat-store";
import { EFFORT_LEVELS, PERMISSION_LEVELS, type PermissionTier } from "@/types/chat";
import { FormColumn } from "@/components/ui/content-column";

/**
 * 设置页的「运行行为」项：模型怎么干活——推理档位、回合预算、上下文闸。
 * 执行边界（沙箱/审查/审计）在「安全与审计」，能力接入（搜索/SSH/LSP）在「扩展」。
 */
export function BehaviorSettings() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const { requestSwitch, confirmDialog } = usePermissionSwitch();

  const [configPath, setConfigPath] = useState("");
  useEffect(() => {
    fetchConfigPath()
      .then(setConfigPath)
      .catch(() => setConfigPath(""));
  }, []);

  function openConfigFile() {
    if (!configPath) return;
    void revealItemInDir(configPath).catch(() => undefined);
  }

  const effort = config.reasoningEffort;

  return (
    <FormColumn>
      <SettingsHeader
        title="运行行为"
        description="模型的推理行为与回合预算。改动即时生效并写入配置。"
        action={
          <button
            type="button"
            onClick={openConfigFile}
            disabled={!configPath}
            className="flex shrink-0 items-center gap-1.5 rounded-lg px-2.5 py-1.5 text-sm text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
          >
            <span>打开 config.json</span>
            <ExternalLink className="size-3.5" />
          </button>
        }
      />

      <Group title="推理行为">
        <Row
          title="工具权限档位"
          description="写入文件、执行命令时先问你，还是放行"
          note={
            config.fullAccessAcknowledged ? (
              <p className="mt-1.5 text-xs leading-5 text-muted-foreground">
                已跳过「完全访问」的风险确认，切换时不再弹窗。
                <button
                  type="button"
                  onClick={() => void updateConfig({ fullAccessAcknowledged: false })}
                  className="ml-1 text-brand-text outline-none hover:underline focus-visible:ring-2 focus-visible:ring-ring/45"
                >
                  恢复提示
                </button>
              </p>
            ) : null
          }
        >
          <Select
            value={config.permission}
            onValueChange={(value) => requestSwitch(value as PermissionTier)}
          >
            <SelectTrigger>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {PERMISSION_LEVELS.map((level) => (
                <SelectItem key={level.value} value={level.value}>
                  {level.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </Row>

        <Row title="思考程度" description="发给服务商的 reasoning_effort 档位">
          <Select
            value={effort}
            onValueChange={(value) => void updateConfig({ reasoningEffort: value })}
          >
            <SelectTrigger>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="">模型默认</SelectItem>
              {EFFORT_LEVELS.map((level) => (
                <SelectItem key={level.value} value={level.value}>
                  {level.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </Row>

        <Row
          title="命令 Shell"
          description="AI 跑命令（run_command）用的 shell。Git Bash 需要 Git for Windows；模型显式指定的 shell 优先于这里"
        >
          <Select
            value={config.commandShell}
            onValueChange={(value) => void updateConfig({ commandShell: value })}
          >
            <SelectTrigger>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="">默认（cmd）</SelectItem>
              <SelectItem value="powershell">Windows PowerShell</SelectItem>
              <SelectItem value="pwsh">PowerShell 7</SelectItem>
              <SelectItem value="cmd">Command Prompt</SelectItem>
              <SelectItem value="git-bash">Git Bash</SelectItem>
            </SelectContent>
          </Select>
        </Row>

        <Row title="显示思考过程" description="关闭后不渲染模型的思维链，只看正文">
          <div className="flex justify-end">
            <CapabilityToggle
              label="显示思考过程"
              enabled={config.showReasoning}
              onToggle={() => void updateConfig({ showReasoning: !config.showReasoning })}
            />
          </div>
        </Row>
      </Group>

      <Group title="回合与预算">
        <Row
          title="工具调用轮数上限"
          description="单回合内模型最多连续调用多少轮工具，到顶停止。0 = 不设上限"
        >
          <NumberField
            label="单回合最大工具轮数"
            value={config.maxToolRounds}
            onCommit={(value) => void updateConfig({ maxToolRounds: value })}
          />
        </Row>

        <Row
          title="多助理 全局并发上限"
          description="所有编排计划加起来同时跑几路。每份计划自己的那个上限管不到这件事：三份各 4 就是 12 路并发请求。0 = 不设上限"
        >
          <NumberField
            label="全局最大并发路数"
            value={config.totalParallel}
            onCommit={(value) => void updateConfig({ totalParallel: value })}
          />
        </Row>

        <Row
          title="服务商限流（429）时无限重试"
          description="限流说的是「稍后再来」，不是「此路不通」。开启后按指数退避一直试到成功：第 n 次等 2ⁿ 秒、封顶 60 秒，每一轮都在对话里说一句卡在哪儿。默认关：「无限」意味着这一发可能永远不结束，要你点头。等待期间按停止随时可退，代理与模型池不会因此被记成失败"
        >
          <div className="flex items-center justify-end gap-2">
            <span className="text-xs text-muted-foreground">
              {config.unlimitedRetry429 ? "已开启" : "已关闭"}
            </span>
            <CapabilityToggle
              label="服务商限流时无限重试"
              enabled={config.unlimitedRetry429}
              onToggle={() => void updateConfig({ unlimitedRetry429: !config.unlimitedRetry429 })}
            />
          </div>
        </Row>
      </Group>

      <Group title="上下文">
        <Row
          title="自动压缩上下文"
          description="上下文超过窗口 90% 时压缩成摘要再继续（细节在服务商档案页）"
        >
          <div className="flex items-center justify-end gap-2">
            <span className="text-xs text-muted-foreground">
              {config.autoCompact ? "已开启" : "已关闭"}
            </span>
            <CapabilityToggle
              label="自动压缩上下文"
              enabled={config.autoCompact}
              onToggle={() => void updateConfig({ autoCompact: !config.autoCompact })}
            />
          </div>
        </Row>

        <Row
          title="缓存保温"
          description="服务商缓存快到期时，用一次只回 1 个 token 的重放把它续上。默认关：它先花一笔小钱，赌省下一笔大钱，只有价表算得出至少省 $0.05 才会发"
        >
          <div className="flex items-center justify-end gap-2">
            <span className="text-xs text-muted-foreground">
              {config.cacheWarming ? "已开启" : "已关闭"}
            </span>
            <CapabilityToggle
              label="缓存保温"
              enabled={config.cacheWarming}
              onToggle={() => void updateConfig({ cacheWarming: !config.cacheWarming })}
            />
          </div>
        </Row>

        <Row
          title="重复循环护栏"
          description="模型有时会在短句上无限复读（解码退化），不掐会一直烧到 token 上限。流式检测到重复模式就自动截断：循环前的内容保留，后面的钱不花。默认开"
        >
          <div className="flex items-center justify-end gap-2">
            <span className="text-xs text-muted-foreground">
              {config.repetitionGuard ? "已开启" : "已关闭"}
            </span>
            <CapabilityToggle
              label="重复循环护栏"
              enabled={config.repetitionGuard}
              onToggle={() => void updateConfig({ repetitionGuard: !config.repetitionGuard })}
            />
          </div>
        </Row>

        <Row
          title="项目约定（字符）"
          description="AGENTS.md / CLAUDE.md 最多注入多少，超出只留开头并注明截断。0 = 不设上限"
        >
          <CharLimit
            label="项目约定注入字符上限"
            value={config.projectRulesMaxChars}
            onCommit={(value) => void updateConfig({ projectRulesMaxChars: value })}
          />
        </Row>

        <Row
          title="工具结果（字符）"
          description="一条结果超过这么多就只留头 3/4 与尾 1/8，中间注明省略了多少。0 = 不设上限"
        >
          <CharLimit
            label="工具结果字符上限"
            value={config.toolResultMaxChars}
            onCommit={(value) => void updateConfig({ toolResultMaxChars: value })}
          />
        </Row>

        <Row
          title="记忆段（字符）"
          description="本轮检索出的记忆超过这么多就整段不发——截一半等于伪造一条没存在过的记忆。0 = 不设上限"
        >
          <CharLimit
            label="记忆段字符上限"
            value={config.memorySectionMaxChars}
            onCommit={(value) => void updateConfig({ memorySectionMaxChars: value })}
          />
        </Row>

        <Row
          title="重启后自动继续挂着的目标"
          description="关掉时，程序重启会把还在推进的目标落成「已暂停」并说一声——你在目标带上按继续才往下跑。默认关：要不要继续花钱由你决定，不让一次重启替你重按播放键。开着时它照旧自己接下一轮，包括那些你没看着的话题"
        >
          <div className="flex items-center justify-end gap-2">
            <span className="text-xs text-muted-foreground">
              {config.goalResumeOnLaunch ? "已开启" : "已关闭"}
            </span>
            <CapabilityToggle
              label="重启后自动继续目标"
              enabled={config.goalResumeOnLaunch}
              onToggle={() => void updateConfig({ goalResumeOnLaunch: !config.goalResumeOnLaunch })}
            />
          </div>
        </Row>
      </Group>

      <p className="mt-4 text-xs leading-5 text-muted-foreground">
        以上全部写在 <span className="font-mono">config.json</span>（{configPath || "…"}
        ），也可以直接编辑文件后重启生效。
      </p>

      {confirmDialog}
    </FormColumn>
  );
}
