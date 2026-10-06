import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { IconTerminal2 as SquareTerminal } from "@tabler/icons-react";

import { ConversationTimeline } from "@/components/conversation-timeline";
import { GoalDock } from "@/components/goal-panel";
import { groupTurns } from "@/lib/turns";
import { MessageItem } from "@/components/message-item";
import { modelDisplayLabel } from "@/components/model-picker";
import { ASSISTANT_NAME, useChatStore } from "@/store/chat-store";
import { effortLabel } from "@/types/chat";
import { cn } from "@/lib/utils";
import { ContentColumn } from "@/components/ui/content-column";

export function MessageList() {
  const messages = useChatStore((s) => s.messages);
  const edits = useChatStore((s) => s.edits);
  const config = useChatStore((s) => s.config);
  // 会话档：思考程度是对话轮的旋钮，生成会话（生图/视频）的空态不提它
  const kind = useChatStore((s) => s.kind);
  // 滚动容器存 state 而不是 ref：ref 不会让依赖它的 effect 重跑，容器一旦被换，
  // 监听就留在旧节点上——时间线就会卡在挂载时算出的那一格
  const [scroller, setScroller] = useState<HTMLDivElement | null>(null);
  const stuckRef = useRef(true);
  const [flash, setFlash] = useState<string | null>(null);
  const flashTimer = useRef<number | null>(null);

  const turns = useMemo(() => groupTurns(messages), [messages]);
  const byId = useMemo(() => new Map(messages.map((message) => [message.id, message])), [messages]);
  const lastId = messages[messages.length - 1]?.id;

  // 分支：切换器要的是"这一处有几支"，轨道角标要的是"哪一轮上有分叉"。
  // 节点全集 = 看得见的那条 + 切走的那些（offPath），父子关系是从后端日志抄来的
  const offPath = useChatStore((s) => s.offPath);
  const nodes = useMemo(() => [...messages, ...offPath], [messages, offPath]);
  // 兄弟表的键指纹：只有 id/父 id 集合变化才换表——流式 flush 换新数组时
  // 引用保持稳定，branch 数组不变，MessageItem 的 memo 才拦得住
  const nodesSignature = useMemo(
    () => nodes.map((node) => `${node.id}>${node.parentId ?? ""}`).join("\u0000"),
    [nodes],
  );
  const siblingsById = useMemo(() => {
    // 键是父 id；根（parentId 为空）用 null 做键，多条根互为兄弟
    const groups = new Map<string | null, string[]>();
    for (const node of nodes) {
      const parent = node.parentId ?? null;
      const bucket = groups.get(parent);
      if (bucket) bucket.push(node.id);
      else groups.set(parent, [node.id]);
    }
    return groups;
    // eslint-disable-next-line react-hooks/exhaustive-deps -- 依赖是指纹而非数组引用
  }, [nodesSignature]);
  const branchOf = (id: string) => {
    // 用 byId 查父（O(1)），别扫 nodes——这条在每次渲染对每条消息各跑一次
    const group = siblingsById.get(byId.get(id)?.parentId ?? null);
    return group && group.length > 1 ? group : undefined;
  };
  const forkedTurns = useMemo(() => {
    const counts = new Map<string, number>();
    for (const turn of turns) {
      const total = Math.max(
        ...turn.messageIds.map((id) => siblingsById.get(byId.get(id)?.parentId ?? null)?.length ?? 1),
        1,
      );
      if (total > 1) counts.set(turn.id, total);
    }
    return counts;
  }, [turns, siblingsById, byId]);

  const last = messages[messages.length - 1];
  // 跟随的信号是"最后那条消息被改写过没有"，不是"正文长了多少"：
  // 只看字数的话，流程条多一格、某个工具从执行中变成完成都不会让视图跟下去，
  // 用户看到的就是"它在跑，但屏幕不动"
  useEffect(() => {
    if (!scroller || !stuckRef.current) return;
    scroller.scrollTop = scroller.scrollHeight;
  }, [messages.length, last, scroller]);

  useEffect(
    () => () => {
      if (flashTimer.current) window.clearTimeout(flashTimer.current);
    },
    [],
  );

  const jump = useCallback(
    (turnId: string) => {
      if (!scroller) return;
      const el = scroller.querySelector<HTMLElement>(`[data-turn-id="${turnId}"]`);
      if (!el) return;
      const origin = scroller.getBoundingClientRect().top - scroller.scrollTop;
      scroller.scrollTop = Math.max(0, el.getBoundingClientRect().top - origin - 16);
      // 跳上去之后别再让自动跟随把人拽回底部
      stuckRef.current = false;
      setFlash(turnId);
      if (flashTimer.current) window.clearTimeout(flashTimer.current);
      flashTimer.current = window.setTimeout(() => setFlash(null), 1200);
    },
    [scroller],
  );

  if (messages.length === 0) {
    // 与底栏模型选择器共用同一份池感知文案：那边显示什么，这里就显示什么
    const label = modelDisplayLabel(config);
    const configured =
      Boolean(config.baseUrl) &&
      (config.modelPool.mode !== "off"
        ? config.modelPool.members.some((member) => member.enabled)
        : Boolean(config.model));

    return (
      <div className="relative flex min-h-0 flex-1 flex-col items-center justify-center gap-2 px-6 text-center">
        {/* 空话题也要能看见后台的目标与动静：浮层与消息态共用同一张卡组 */}
        <GoalDock />
        <SquareTerminal className="size-9 text-muted-foreground/45" strokeWidth={1.25} />
        <h2 className="mt-2 text-2xl font-medium tracking-tight text-foreground">
          我们要构建什么？
        </h2>
        <p className="text-sm text-muted-foreground">
          {configured
            ? (kind ?? "chat") === "chat"
              ? `${label} · 思考程度 ${effortLabel(config.reasoningEffort)}`
              : label
            : "先在「设置」里填 Base URL，并在 Windows 凭据管理器放好 API 密钥。"}
        </p>
      </div>
    );
  }

  return (
    <div className="relative flex min-h-0 flex-1">
      {/* 对话区左下角的浮层小卡片：后台目标 + 后台动静。它在滚动层之外，聊天滚它的 */}
      <GoalDock />
      <div
        ref={setScroller}
        onScroll={(event) => {
          const el = event.currentTarget;
          stuckRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
        }}
        className="min-h-0 flex-1 overflow-y-auto"
      >
        <ContentColumn className="py-6 sm:py-7">
          <div className="space-y-7">
            {turns.map((turn) => (
              <div
                key={turn.id}
                data-turn-id={turn.id}
                className={cn(
                  "space-y-7 rounded-lg scroll-mt-2",
                  flash === turn.id && "ring-2 ring-brand/45",
                )}
              >
                {turn.messageIds.map((id) => {
                  const message = byId.get(id);
                  if (!message) return null;
                  return (
                    <MessageItem
                      key={id}
                      message={message}
                      author={ASSISTANT_NAME}
                      isLast={id === lastId}
                      branch={branchOf(id)}
                    />
                  );
                })}
              </div>
            ))}
          </div>
        </ContentColumn>
      </div>

      <ConversationTimeline
        turns={turns}
        edits={edits}
        forks={forkedTurns}
        scroller={scroller}
        onJump={jump}
      />
    </div>
  );
}
