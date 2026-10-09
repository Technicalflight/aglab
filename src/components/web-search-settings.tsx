import { Group, Row, SettingsHeader, CharLimit, inputClass } from "@/components/settings-ui";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { useChatStore } from "@/store/chat-store";
import { FormColumn } from "@/components/ui/content-column";

/**
 * 设置页的「联网搜索」项：web_search 工具的供应商与凭据。
 * 从运行行为页拆出来——它配置的是一个扩展进来的能力，不是推理行为。
 */
export function WebSearchSettings() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);

  return (
    <FormColumn>
      <SettingsHeader
        title="联网搜索"
        description="1. 选一个搜索服务；2. 填它的凭据或实例地址（SearXNG 免费、无需 key）。配好后 web_search 工具自动声明给模型。"
      />

      <Group title="搜索服务">
        <Row
          title="搜索接口"
          description="给模型的 web_search 工具选搜索服务。没选或凭据/实例地址缺失时这个工具不会声明给模型"
        >
          <Select
            value={config.webSearch.provider}
            onValueChange={(value) =>
              void updateConfig({ webSearch: { ...config.webSearch, provider: value } })
            }
          >
            <SelectTrigger>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="">关闭</SelectItem>
              <SelectItem value="tavily">Tavily</SelectItem>
              <SelectItem value="searxng">SearXNG（免费）</SelectItem>
            </SelectContent>
          </Select>
        </Row>

        {config.webSearch.provider === "searxng" ? (
          <>
            <Row
              wide
              title="SearXNG 实例地址"
              description="自建实例（如 http://localhost:8080）或选定的公共实例。实例需要在 settings.yml 的 search.formats 里开启 json 输出。"
            >
              <input
                type="text"
                spellCheck={false}
                defaultValue={config.webSearch.searxngUrl}
                placeholder="https://searx.example.com"
                aria-label="SearXNG 实例地址"
                className={`${inputClass} w-64 font-mono`}
                onBlur={(event) => {
                  const next = event.target.value.trim();
                  if (next !== config.webSearch.searxngUrl) {
                    void updateConfig({ webSearch: { ...config.webSearch, searxngUrl: next } });
                  }
                }}
                onKeyDown={(event) => {
                  if (event.key === "Enter") event.currentTarget.blur();
                }}
              />
            </Row>
            <Row
              title="访问令牌（可选）"
              description="实例开了反代鉴权（basic/Bearer）才需要填；留空则不带任何令牌。按现状存进 config.json 明文——别把这台机器的配置文件发给别人"
            >
              <input
                type="text"
                spellCheck={false}
                defaultValue={config.webSearch.apiKey}
                placeholder="留空 = 不带令牌"
                aria-label="SearXNG 访问令牌"
                className={`${inputClass} w-64 font-mono`}
                onBlur={(event) => {
                  const next = event.target.value.trim();
                  if (next !== config.webSearch.apiKey) {
                    void updateConfig({ webSearch: { ...config.webSearch, apiKey: next } });
                  }
                }}
                onKeyDown={(event) => {
                  if (event.key === "Enter") event.currentTarget.blur();
                }}
              />
            </Row>
            <div className="border-b border-border px-1 py-4 text-xs leading-5 text-muted-foreground">
              <p className="mb-1 font-medium text-amber-600 dark:text-amber-500">
                使用公共实例前必读
              </p>
              <p>
                SearXNG 本身免费开源；但 <span className="font-mono">searx.space</span>{" "}
                上的公共实例由社区志愿者提供，
                <span className="text-foreground">无法保证安全与隐私</span>
                ——你的搜索词会明文发给该实例的运营者，可能被记录或注入结果，公共实例还常限流或未开
                JSON 输出。
              </p>
              <p className="mt-1">
                自建实例最稳妥（Docker 一条命令，文档见 docs.searxng.org），实例列表见 searx.space。
                自建在局域网或本机的地址可以直接填——它是你配置的可信端点，与模型服务商地址同级。
              </p>
            </div>
          </>
        ) : null}

        {config.webSearch.provider === "tavily" ? (
          <Row
            title="API Key"
            description="Tavily 的搜索 key（tavily.com 免费注册）。按现状存进 config.json 明文——与插件 MCP 的服务器头部同一处境，别把这台机器的配置文件发给别人"
          >
            <input
              type="text"
              spellCheck={false}
              defaultValue={config.webSearch.apiKey}
              placeholder="tvly-…"
              aria-label="Tavily 搜索密钥"
              className={`${inputClass} w-64 font-mono`}
              onBlur={(event) => {
                const next = event.target.value.trim();
                if (next !== config.webSearch.apiKey) {
                  void updateConfig({ webSearch: { ...config.webSearch, apiKey: next } });
                }
              }}
              onKeyDown={(event) => {
                if (event.key === "Enter") event.currentTarget.blur();
              }}
            />
          </Row>
        ) : null}
        {config.webSearch.provider ? (
          <Row title="结果条数" description="一次搜索最多回几条（1–10）。摘要每条最多 400 字">
            <CharLimit
              label="搜索结果条数上限"
              value={config.webSearch.maxResults}
              onCommit={(value) =>
                void updateConfig({
                  webSearch: {
                    ...config.webSearch,
                    maxResults: Math.min(10, Math.max(1, value)),
                  },
                })
              }
            />
          </Row>
        ) : null}
      </Group>
    </FormColumn>
  );
}
