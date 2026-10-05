import { useState } from "react";
import { IconHelpCircle as HelpCircle } from "@tabler/icons-react";

import { Textarea } from "@/components/ui/textarea";
import { Button } from "@/components/ui/button";
import { useChatStore } from "@/store/chat-store";

/** 提问卡：模型 ask_user 挂起时的问题与选项，长在输入框上方。
 *  后端这一发在等人（没有超时），回答原文会作为工具结果回到模型。
 *  选项之外留一个自由输入的口——模型给的选项从来不覆盖用户想说的话 */
export function AskCard() {
  const activeId = useChatStore((s) => s.activeId);
  const question = useChatStore((s) =>
    activeId ? s.pendingQuestions[activeId] ?? null : null,
  );
  const answerQuestion = useChatStore((s) => s.answerQuestion);
  const [custom, setCustom] = useState("");

  if (!question) return null;

  const answer = (text: string) => {
    const trimmed = text.trim();
    if (!trimmed) return;
    setCustom("");
    void answerQuestion(question.id, trimmed);
  };

  return (
    <div className="mb-1.5 rounded-lg border border-brand/40 bg-elevated px-3 py-2.5">
      <div className="flex items-start gap-1.5">
        <HelpCircle className="mt-0.5 size-3.5 shrink-0 text-brand-text" />
        <p className="min-w-0 text-sm font-medium leading-5 text-foreground">
          {question.question}
        </p>
      </div>

      <div className="mt-2 flex flex-wrap gap-1.5">
        {question.options.map((option) => (
          <button
            key={option.label}
            type="button"
            title={option.description ?? undefined}
            onClick={() => answer(option.label)}
            className="max-w-full rounded-lg border border-border bg-surface px-2.5 py-1 text-left text-sm text-foreground outline-none transition-colors hover:border-brand/50 hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45"
          >
            {option.label}
          </button>
        ))}
      </div>

      <div className="mt-2 flex items-end gap-1.5">
        <Textarea
          value={custom}
          rows={1}
          aria-label="自定义回答"
          placeholder="都不合适？自己说一句"
          className="max-h-24 min-h-9 resize-none border-0 bg-transparent px-2 py-1.5 text-sm focus-visible:ring-0"
          onChange={(event) => setCustom(event.target.value)}
          onKeyDown={(event) => {
            if (event.nativeEvent.isComposing || event.keyCode === 229) return;
            if (event.key === "Enter" && !event.shiftKey) {
              event.preventDefault();
              answer(custom);
            }
          }}
        />
        <Button
          size="sm"
          variant="subtle"
          disabled={!custom.trim()}
          onClick={() => answer(custom)}
        >
          回答
        </Button>
      </div>
    </div>
  );
}
