import { useState } from "react";

import { FullAccessConfirm } from "@/components/full-access-confirm";
import type { PermissionTier } from "@/types/chat";
import { useChatStore } from "@/store/chat-store";

/**
 * 档位切换的唯一入口：切到「完全访问」且用户没确认过风险时先弹确认，其余档位直接落盘。
 * 选择器和设置页共用它——两处都能改档位，只拦一处等于留了个绕过去的洞
 */
export function usePermissionSwitch() {
  const acknowledged = useChatStore((s) => s.config.fullAccessAcknowledged);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const [confirming, setConfirming] = useState(false);

  const requestSwitch = (tier: PermissionTier) => {
    if (tier === "full" && !acknowledged) {
      setConfirming(true);
      return;
    }
    void updateConfig({ permission: tier });
  };

  const confirmDialog = (
    <FullAccessConfirm
      open={confirming}
      onClose={() => setConfirming(false)}
      onConfirm={(stopAsking) => {
        void updateConfig(
          stopAsking
            ? { permission: "full", fullAccessAcknowledged: true }
            : { permission: "full" },
        );
      }}
    />
  );

  return { requestSwitch, confirmDialog };
}
