import * as React from "react";

import { cn } from "@/lib/utils";

/** 多行输入。补 hover / disabled 底色，与 Input 的四态对齐 */
function Textarea({ className, ...props }: React.ComponentProps<"textarea">) {
  return (
    <textarea
      data-slot="textarea"
      className={cn(
        "flex field-sizing-content w-full resize-none rounded-md border border-input bg-background px-3.5 py-3 text-base text-foreground shadow-none outline-none transition-[color,background-color,border-color,box-shadow] duration-[var(--dur-fast)] placeholder:text-foreground-tertiary hover:border-brand/35 focus-visible:border-brand/60 focus-visible:ring-2 focus-visible:ring-ring/35 disabled:cursor-not-allowed disabled:bg-fill-4 disabled:text-foreground-quaternary",
        className,
      )}
      {...props}
    />
  );
}

export { Textarea };
