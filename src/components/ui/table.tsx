import { cn } from "@/lib/utils";

/**
 * 数据表。语义标签全部到位（table/thead/tbody/th/scope），
 * 读屏软件才能在朗读时念出"某某列，某某值"。
 *
 * 密度是这类表的主要矛盾：行高给到能看清又不过分稀疏，
 * 数字列一律 tabular-nums（否则多行数字上下跳，读起来像在数数）。
 */
function Table({ className, ...props }: React.ComponentProps<"table">) {
  return (
    <div data-slot="table-scroll" className="w-full overflow-x-auto">
      <table
        data-slot="table"
        className={cn("w-full caption-bottom border-collapse text-xs", className)}
        {...props}
      />
    </div>
  );
}

function TableHeader({ className, ...props }: React.ComponentProps<"thead">) {
  return <thead data-slot="table-header" className={cn("", className)} {...props} />;
}

function TableBody({ className, ...props }: React.ComponentProps<"tbody">) {
  return <tbody data-slot="table-body" className={cn("", className)} {...props} />;
}

function TableRow({ className, ...props }: React.ComponentProps<"tr">) {
  return (
    <tr
      data-slot="table-row"
      className={cn(
        "border-b border-border-subtle transition-colors last:border-0 hover:bg-fill-4",
        className,
      )}
      {...props}
    />
  );
}

/**
 * 表头单元格。数字列传 numeric 让它右对齐并用等宽数字——
 * 文字列右对齐会读着别扭，数字列左对齐则位数对不齐。
 */
function TableHead({
  className,
  numeric = false,
  ...props
}: React.ComponentProps<"th"> & { numeric?: boolean }) {
  return (
    <th
      data-slot="table-head"
      scope="col"
      className={cn(
        "h-8 whitespace-nowrap px-3 text-2xs font-medium tracking-wide text-foreground-tertiary uppercase",
        numeric ? "text-right tabular-nums" : "text-left",
        className,
      )}
      {...props}
    />
  );
}

function TableCell({
  className,
  numeric = false,
  ...props
}: React.ComponentProps<"td"> & { numeric?: boolean }) {
  return (
    <td
      data-slot="table-cell"
      className={cn(
        "px-3 py-2 align-middle text-foreground-secondary",
        numeric ? "text-right tabular-nums" : "text-left",
        className,
      )}
      {...props}
    />
  );
}

function TableCaption({ className, ...props }: React.ComponentProps<"caption">) {
  return (
    <caption
      data-slot="table-caption"
      className={cn("mt-2 text-2xs text-foreground-tertiary", className)}
      {...props}
    />
  );
}

export { Table, TableBody, TableCaption, TableCell, TableHead, TableHeader, TableRow };
