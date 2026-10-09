import { describe, expect, it } from "vitest";
import { groupTurns, isCompactionDone } from "@/lib/turns";
import type { Message } from "@/types/chat";

const msg = (over: Partial<Message> & { id: string }): Message =>
  ({
    role: "assistant",
    content: "",
    createdAt: 1,
    ...over,
  }) as Message;

describe("isCompactionDone", () => {
  it("认 summary 标记与投影前缀两条路", () => {
    expect(isCompactionDone(msg({ id: "a", summary: true, content: "摘要" }))).toBe(true);
    expect(
      isCompactionDone(
        msg({
          id: "b",
          content:
            "【上下文压缩完成】更早的对话已压缩成摘要，任务上下文已衔接，继续处理中。\n\n摘要正文",
        }),
      ),
    ).toBe(true);
    // 压缩中那一条不算"完成"
    expect(
      isCompactionDone(msg({ id: "c", content: "【上下文压缩中】正在把更早的对话压缩成摘要" })),
    ).toBe(false);
    expect(isCompactionDone(msg({ id: "d", content: "普通回答" }))).toBe(false);
  });

  it("重开话题后的前缀行归位为压缩锚点，不再退化成普通气泡", () => {
    const turns = groupTurns([
      msg({
        id: "a",
        content: "【上下文压缩完成】更早的对话已压缩成摘要，任务上下文已衔接，继续处理中。\n\n摘要",
      }),
      msg({ id: "b", role: "user", content: "接着问" }),
    ]);
    expect(turns[0].kind).toBe("compaction");
    expect(turns[1].kind).toBe("turn");
  });
});
