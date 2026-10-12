import { describe, expect, it } from "vitest";

import { filterCommands, filterTopics, mergeRows } from "@/lib/command-palette";

const commands = [
  { key: "new-chat", label: "新聊天", hint: "对话模式 · 开新会话", run: () => {} },
  { key: "mode-image", label: "新建生图会话", hint: "生图模式", run: () => {} },
  { key: "section-settings", label: "前往设置", hint: "分区", run: () => {} },
  { key: "panel-terminal", label: "打开终端面板", hint: "工具栏", run: () => {} },
  { key: "theme", label: "切换到深色主题", hint: "外观", run: () => {} },
];

const topics = [
  { id: "a", title: "压缩闸排查", preview: "O12", project: "aglab" },
  { id: "b", title: "生图测试", preview: "画一张封面", project: "" },
];

describe("命令中心的数据层", () => {
  it("空关键词给全量，非空按 label 与 hint 大小写不敏感过滤", () => {
    expect(filterCommands(commands, "")).toHaveLength(commands.length);
    expect(filterCommands(commands, "  ")).toHaveLength(commands.length);
    const byLabel = filterCommands(commands, "生图");
    expect(byLabel.map((item) => item.key)).toEqual(["mode-image"]);
    // hint 也参与匹配：「工具栏」命中终端面板那条
    const byHint = filterCommands(commands, "工具栏");
    expect(byHint.map((item) => item.key)).toEqual(["panel-terminal"]);
    // 大小写不敏感
    expect(filterCommands(commands, "CTRL")).toEqual([]);
    expect(
      filterCommands([{ key: "x", label: "Open Terminal", hint: "", run: () => {} }], "terminal"),
    ).toHaveLength(1);
  });

  it("话题按标题、预览与项目名过滤", () => {
    expect(filterTopics(topics, "")).toEqual(topics);
    expect(filterTopics(topics, "压缩").map((item) => item.id)).toEqual(["a"]);
    expect(filterTopics(topics, "封面").map((item) => item.id)).toEqual(["b"]);
    expect(filterTopics(topics, "aglab").map((item) => item.id)).toEqual(["a"]);
  });

  it("命令与话题排进同一条导航序列：命令在前，前 9 条话题带 Ctrl+1..9 徽标", () => {
    const rows = mergeRows(commands, topics);
    const first = rows[0];
    if (first.kind !== "command") throw new Error("命令该排在话题前面");
    expect(first.command.key).toBe("new-chat");
    const topicRows = rows.filter((row) => row.kind === "topic");
    expect(topicRows[0]).toMatchObject({ kind: "topic", id: "a", badge: "Ctrl+1" });
    expect(topicRows[1]).toMatchObject({ kind: "topic", id: "b", badge: "Ctrl+2" });

    // 第 10 条起没有徽标：Ctrl+1..9 只指话题区的前九个
    const many = Array.from({ length: 12 }, (_, index) => ({
      id: `t${index}`,
      title: `话题${index}`,
      preview: "",
      project: "",
    }));
    const badges = mergeRows([], many)
      .map((row) => (row.kind === "topic" ? row.badge : undefined))
      .filter(Boolean);
    expect(badges).toHaveLength(9);
    expect(badges[8]).toBe("Ctrl+9");
  });
});
