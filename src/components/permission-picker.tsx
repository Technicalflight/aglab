import { useState } from "react";
import { IconCheck as Check, IconGauge as Gauge, IconShieldQuestion as ShieldQuestion, IconAlertTriangle as TriangleAlert } from "@tabler/icons-react";

import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { usePermissionSwitch } from "@/lib/use-permission-switch";
import { PERMISSION_LEVELS, type PermissionTier } from "@/types/chat";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";

const ICONS = {
  ask: ShieldQuestion,
  auto: Gauge,
  full: TriangleAlert,
} as const;

export function PermissionPicker() {
  const permission = useChatStore((s) => s.config.permission);
  const { requestSwitch, confirmDialog } = usePermissionSwitch();
  const [open, setOpen] = useState(false);
  const current = PERMISSION_LEVELS.find((level) => level.value === permission) ?? PERMISSION_LEVELS[0];
  const CurrentIcon = ICONS[current.value];

  return (
    <>
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger
          type="button"
          className={cn(
            // shrink-0 + nowrap：工具条空间再紧也不能把档位名挤成两行
            "flex h-8 shrink-0 items-center gap-1.5 whitespace-nowrap rounded-lg px-2 text-sm outline-none transition-colors hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45 data-[state=open]:bg-accent",
            current.value === "full" ? "text-destructive" : "text-muted-foreground",
          )}
        >
          <CurrentIcon className="size-3.5" />
          {current.label}
        </PopoverTrigger>

        <PopoverContent align="start" className="w-[300px] p-1.5">
          <p className="px-2 pt-1.5 pb-2 text-xs text-muted-foreground">工具执行前要不要先问你</p>

          {PERMISSION_LEVELS.map((level) => {
            const Icon = ICONS[level.value as PermissionTier];
            const active = level.value === permission;

            return (
              <button
                key={level.value}
                type="button"
                onClick={() => {
                  setOpen(false);
                  requestSwitch(level.value);
                }}
                className="flex w-full cursor-pointer items-start gap-2.5 rounded-lg px-2 py-2 text-left outline-none transition-colors hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45"
              >
                <Icon
                  className={cn(
                    "mt-0.5 size-4 shrink-0",
                    level.value === "full" ? "text-destructive" : "text-muted-foreground",
                  )}
                />
                <span className="min-w-0 flex-1">
                  <span className="block text-base text-foreground">{level.label}</span>
                  <span className="mt-0.5 block text-xs leading-5 text-muted-foreground">
                    {level.description}
                  </span>
                </span>
                {active ? <Check className="mt-1 size-3.5 shrink-0 text-brand-text" /> : null}
              </button>
            );
          })}
        </PopoverContent>
      </Popover>

      {confirmDialog}
    </>
  );
}
