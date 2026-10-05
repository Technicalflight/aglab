# 插件创建器

教你怎么打一个 aglab 插件包。插件是**目录容器**：一个目录里可以同时带技能、MCP 服务器、钩子，以及命令与代理（后两者列出但不加载）。

## 目录结构

插件目录路径在「管理 › 插件」页顶显示。放进去后点右上角刷新。

```
my-plugin/
  .claude-plugin/
    plugin.json          # 元数据（可缺：缺了就用目录名当插件名）
  skills/
    排查构建/
      SKILL.md           # 技能，格式同个人技能
  .mcp.json              # MCP 服务器（可选）
  hooks/
    hooks.json           # 钩子（可选）
  commands/*.md          # 列出数量，不加载
  agents/*.md            # 列出数量，不加载
```

## plugin.json

```json
{
  "name": "my-plugin",
  "description": "一句话说清这个插件带来什么",
  "version": "1.0.0",
  "author": "张三",
  "category": "效率工具"
}
```

author 也可以是 `{"name":"张三","email":"…@example.com"}`，界面取里面的 name。

## .mcp.json

```json
{
  "mcpServers": {
    "postgres": { "command": "npx", "args": ["-y", "pg-mcp"], "env": {} }
  }
}
```

- 服务器 id 会带上插件前缀（`插件id::key`），两个插件用同名服务器不互相顶掉。
- MCP 工具一律按高风险处理：逐项确认与自动放行两档下都会先问用户。
- 插件带来的服务只能停整个插件，不能单独删；要单独管理就独立配置。
- 不是合法 JSON 会被跳过，插件详情里看得到。

## hooks.json

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "write_file",
        "hooks": [{ "type": "command", "command": "check.py" }]
      }
    ]
  }
}
```

- 只支持 `type: "command"` 处理器；prompt 型会被跳过，并在插件详情的 hook_notes 里说明原因——不静默消失。
- 钩子拿到 aglab 本进程的全部权限（读文件、联网、删东西都行），所以每条都要用户**逐条看过内容确认**才执行；确认记的是脚本内容指纹，改一个字节就作废重来。
- matcher 按工具 id 筛；不写 = 对所有工具生效。

## 验证清单

1. 刷新插件页，插件出现在列表，描述/版本/作者对得上。
2. 进详情：技能逐条可见（字符数合理）、MCP 服务列出、钩子状态是「待确认」或已信任。
3. 未经确认的钩子 runs = false——这是设计，不是故障。

## 常见错误

- 技能直接放 `skills/SKILL.md`（少一层目录）→ 扫不到；必须 `skills/<技能目录>/SKILL.md`。
- manifest 写在插件根目录而不是 `.claude-plugin/plugin.json` → 当作无名插件，用目录名。
- 钩子脚本用相对路径写命令 → 相对什么目录没有保证，写成绝对路径或让用户自己配。
