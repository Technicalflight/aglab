import { useId, useState } from "react";
import { IconAlertTriangle as TriangleAlert } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";

const GRANTS = [
  "内置工具全部免确认：浏览目录、读取文件、搜索内容、写入文件、精确编辑、删除文件、执行命令、打开文件或网址、读后台命令输出、停后台命令、读网页、联网搜索、资料库检索、取回观察原文、取用技能、操作内置浏览器、列出窗口、读窗口控件、操作别的程序、派出子助理、子助理控制、声明交付物、运行脚本、SSH 执行、LSP 语义查询。",
  "「操作内置浏览器」驱动的是 aglab 用独立配置目录拉起的 Chrome/Edge：它能打开网页、按编号点击与输入。导航过出口名单与内网地址两道闸，但网页上的动作（下单、发帖、登录后的操作）免确认。",
  "「操作别的程序」是合成鼠标与键盘输入：它点得到任何窗口，也往任何拿到焦点的输入框敲字——做的是你本人在这台机器上能做的事。",
  "「派出子助理」同样免确认：子助理开自己的话题去执行任务，它内部的写文件与命令按各自的白名单与权限走。",
  "MCP 连接器带来的工具，以及模型自己编出来的工具名，一并免确认。",
  "命令走 cmd /C（Windows）或 sh -c，以你本机账户的权限运行，没有任何沙箱隔离。",
  "项目目录内的读写不再问。项目外：**读**仍单独问你一句，**写与删**仍然一律拒绝（这一条 full 也放不开）。「删除文件」默认移入回收站，设置里的「删除保护」关掉后才是系统删除。",
];

const CONSEQUENCES = [
  "覆盖写不可撤销：应用内没有回收站也没有撤销栈，写坏的文件只能靠你自己的备份。",
  "命令同样不可撤销：删目录、改系统配置、对外发请求都在能力范围内，误判就是既成事实。",
  "合成输入同样不可撤销：点错一个按钮可能就是发出去一封邮件、清掉一个字段、或在你自己的账户里做完一笔操作。",
  "少一道人工闸门：模型被外部内容（网页、文件、报错信息）诱导时，没有第二次确认拦它。",
  "立即影响正在进行的这场对话——档位每轮重读，不是只对新对话生效。",
  "写进 config.json 且重启保留，对所有项目全局生效。",
];

const REMAINING = [
  "往项目外写文件仍然一律拒绝；读项目外仍然要单独点头。这两条不随档位放开。",
  "自我毁灭形的命令（rm -rf /、mkfs、format c:、fork 炸弹、删系统目录…）仍然直接拒，full 也不问。",
  "标题里带口令 / 支付 / 凭据 / 钱包这类词的窗口，读和动都一律拒——但窗口标题是应用自己写的，这一条挡的是误点与被诱导的一次点击，挡不住铁了心要绕的人。",
  "命令 60 秒超时强杀、输出 32KB 截断、单文件读取 128KB 截断。",
  "「读网页」只到得了公网：出口域名名单与内网地址两道闸不随档位放开。",
  "设置里逐工具关掉的那一项仍然不会执行——这是仍然有效的围栏。",
  "插件声明的 PreToolUse 钩子仍会先跑，它拒绝就执行不了。",
  "留痕只有话题历史里的工具调用记录，删掉话题就没有了。",
];

function Section({ title, items }: { title: string; items: string[] }) {
  return (
    <div className="mt-3.5">
      <p className="text-xs font-medium tracking-[0.08em] text-foreground-tertiary uppercase">
        {title}
      </p>
      <ul className="mt-1.5 space-y-1">
        {items.map((item) => (
          <li key={item} className="flex gap-2 text-sm leading-5 text-foreground/90">
            <span className="mt-1.5 size-1 shrink-0 rounded-full bg-muted-foreground/60" />
            {item}
          </li>
        ))}
      </ul>
    </div>
  );
}

/**
 * 切进「完全访问」前的风险确认。三节内容逐条对着后端实现写，不是通用免责套话——
 * 一个往轻了说的弹窗比没有弹窗更危险
 */
export function FullAccessConfirm({
  open,
  onClose,
  onConfirm,
}: {
  open: boolean;
  onClose: () => void;
  onConfirm: (stopAsking: boolean) => void;
}) {
  const descId = useId();
  const [acknowledged, setAcknowledged] = useState(false);
  const [stopAsking, setStopAsking] = useState(false);

  const close = () => {
    setAcknowledged(false);
    setStopAsking(false);
    onClose();
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next) close();
      }}
    >
      <DialogContent aria-describedby={descId} className="max-h-[85vh] w-[520px] overflow-y-auto">
        <DialogTitle className="flex items-center gap-2 text-destructive">
          <TriangleAlert className="size-4" />
          开启完全访问？
        </DialogTitle>
        <p id={descId} className="mt-1 text-xs leading-5 text-muted-foreground">
          此后工具调用不再逐条问你。下面这些是这个档位在实现里真正放开的东西。
        </p>

        <Section title="你将放开的权限" items={GRANTS} />
        <Section title="可能造成的后果" items={CONSEQUENCES} />
        <Section title="仍然剩下的边界" items={REMAINING} />

        <p className="mt-4 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2.5 text-sm leading-5 text-destructive">
          这是按你本人意愿，把本机账户的执行权交给模型。由此产生的文件损坏、数据丢失、凭据外泄、超额费用或任何其他后果，由你自行承担——aglab
          不提供撤销、恢复或赔偿。
        </p>

        <label className="mt-4 flex cursor-pointer items-start gap-2 text-sm leading-5 text-foreground">
          <input
            type="checkbox"
            checked={acknowledged}
            onChange={(event) => setAcknowledged(event.target.checked)}
            className="mt-0.5 size-3.5 shrink-0 accent-brand"
          />
          我已逐条读过上述风险，确认要自行承担后果
        </label>
        <label className="mt-2 flex cursor-pointer items-start gap-2 text-sm leading-5 text-muted-foreground">
          <input
            type="checkbox"
            checked={stopAsking}
            onChange={(event) => setStopAsking(event.target.checked)}
            className="mt-0.5 size-3.5 shrink-0 accent-brand"
          />
          以后不再弹出此确认（可在设置页「工具权限档位」下恢复）
        </label>

        <div className="mt-5 flex justify-end gap-2">
          <Button variant="subtle" onClick={close}>
            取消
          </Button>
          <Button
            disabled={!acknowledged}
            className="bg-destructive text-destructive-foreground hover:bg-destructive/90"
            onClick={() => {
              close();
              onConfirm(stopAsking);
            }}
          >
            开启完全访问
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
