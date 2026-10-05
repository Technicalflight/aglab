import { useChatStore } from "@/store/chat-store";
import { ToastCard, ToastProvider, ToastViewport } from "@/components/ui/toast";

/** 失败要说得完，所以比一般提示留得久；挂起提示看完就够了 */
const DURATIONS = { error: 9000, info: 6000 } as const;

/**
 * 右上角告警层。挂在 App 根上，不随分区切换而消失——请求失败的正文标记会留在
 * 记录里，但"刚刚失败了"这件事得在用户还看着这屏的时候说一次
 */
export function ToastLayer() {
  const toasts = useChatStore((s) => s.toasts);
  const dismissToast = useChatStore((s) => s.dismissToast);

  return (
    <ToastProvider swipeDirection="right">
      {/* top-14 而不是 top-4：这是 frameless 窗口，标题栏右上角住着最小化/最大化/关闭，
          告警条压上去就等于在最需要点窗口按钮的时候把它盖住 */}
      <div className="pointer-events-none fixed right-4 top-14 z-[100] flex w-[360px] max-w-[calc(100vw-2rem)] flex-col gap-2">
        {toasts.map((item) => (
          <ToastCard
            key={item.id}
            tone={item.tone}
            title={item.title}
            detail={item.detail}
            duration={DURATIONS[item.tone]}
            onOpenChange={(open) => {
              if (!open) dismissToast(item.id);
            }}
          />
        ))}
        {/* 没有 label 的 Viewport 不会生成 aria-live，读屏就完全听不到告警。
            Radix 只在控制台提一句，很容易漏 */}
        <ToastViewport
          label="通知"
          className="fixed right-4 top-14 flex w-[360px] max-w-[calc(100vw-2rem)] flex-col gap-2 outline-none"
        />
      </div>
    </ToastProvider>
  );
}
