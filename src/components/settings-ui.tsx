import { type ReactNode } from "react";

/** 设置页共用的行与分组框。原来只有 behavior-settings 一页在用（各页各抄一份），
 *  拆页之后这就是它们的公共件：行距、字级、边框只认这一份 */
export const inputClass =
  "h-9 w-full rounded-lg border border-input bg-background px-3 text-base text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35";

export function Row({
  title,
  description,
  note,
  wide,
  children,
}: {
  title: string;
  description: string;
  note?: ReactNode;
  /** 双控件行用：默认槽只有 150px，塞两个控件时内容会从 justify-end 的左侧
   * 溢出、正好盖在描述文字上（遮挡不是挤压，看起来像布局坏了）。
   * wide 行不再左右并排——标题行右侧放控件、说明独占整行：
   * 无论设置列多窄，文字与控件在结构上就摆不到一起去 */
  wide?: boolean;
  children: ReactNode;
}) {
  if (wide) {
    return (
      <div className="border-b border-border px-1 py-4 last:border-b-0">
        <div className="flex items-center justify-between gap-4">
          <p className="min-w-0 text-base font-medium text-foreground">{title}</p>
          <div className="shrink-0">{children}</div>
        </div>
        <p className="mt-1 text-xs leading-5 text-muted-foreground">{description}</p>
        {note}
      </div>
    );
  }
  return (
    <div className="flex items-center justify-between gap-6 border-b border-border px-1 py-4 last:border-b-0">
      <div className="min-w-0">
        <p className="text-base font-medium text-foreground">{title}</p>
        <p className="mt-0.5 text-xs leading-5 text-muted-foreground">{description}</p>
        {note}
      </div>
      <div className="w-[150px] shrink-0">{children}</div>
    </div>
  );
}

export function Group({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="mt-8 first:mt-0">
      <h2 className="text-lg font-semibold tracking-tight text-foreground">{title}</h2>
      <div className="mt-2 rounded-lg border border-border bg-surface px-5 py-4">{children}</div>
    </div>
  );
}

/**
 * 一格"0 = 不设上限"的字符数。长度闸们共用这一条夹取规则。
 *
 * label 必填且不设默认值：这个控件被 4 处复用，缺了名称读屏只会念
 * "编辑框"——用户根本不知道改的是请求数还是记忆条数。
 * 强制调用方写，比在这里猜一个"上限"要有用得多。
 */
export function CharLimit({
  value,
  onCommit,
  label,
}: {
  value: number;
  onCommit: (value: number) => void;
  label: string;
}) {
  return (
    <input
      type="number"
      aria-label={label}
      min={0}
      max={1_000_000}
      value={value}
      className={inputClass}
      onChange={(event) => {
        const next = Math.round(Number(event.target.value));
        if (Number.isFinite(next)) {
          onCommit(Math.min(Math.max(next, 0), 1_000_000));
        }
      }}
    />
  );
}

/** 设置页的标准页头：标题 + 一句它是干什么的 */
export function SettingsHeader({
  title,
  description,
  action,
}: {
  title: string;
  description: string;
  action?: ReactNode;
}) {
  return (
    <div className="flex items-start justify-between gap-4">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight text-foreground">{title}</h1>
        <p className="mt-1 text-sm leading-6 text-muted-foreground">{description}</p>
      </div>
      {action}
    </div>
  );
}
