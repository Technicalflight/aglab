import { useState } from "react";

import { Button } from "@/components/ui/button";
import { Group, Row, SettingsHeader, inputClass } from "@/components/settings-ui";
import { PaginationBar, usePaged } from "@/components/pagination";
import {
  auditClear,
  auditExport,
  auditRotate,
  auditView,
  AUDIT_ACTOR_LABELS,
  AUDIT_OUTCOME_LABELS,
  type AuditPage,
} from "@/lib/chat-transport";
import { useChatStore } from "@/store/chat-store";
import { FormColumn } from "@/components/ui/content-column";

/**
 * 设置页的「审计中心」（design-security-center.md D9）：保留策略、归档、导出与清空、账本。
 * 拦截/放行记录都在这里读；日志内容在写入时就按类别遮过敏感项，这里只读不写。
 */
export function AuditSettings() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const [auditDate, setAuditDate] = useState("");
  const [auditPage, setAuditPage] = useState<AuditPage | null>(null);
  // 两句状态共用一格：导出/清空的结果与账本的报错不会同时有话可说
  const [auditNote, setAuditNote] = useState("");
  const [archived, setArchived] = useState<string | null>(null);
  // 账目一页 5 条；换日期读账回到第 1 页
  const pagedAudit = usePaged(auditPage?.entries ?? [], 5, auditPage?.date);

  return (
    <FormColumn>
      <SettingsHeader
        title="审计中心"
        description="拦截/放行记录与日志导出。账本只增不减：保留策略是归档不是删除，清空也是先归档再清——原文件永远能在 audit/archive/ 里找到。"
      />

      <Group title="审计">
        <Row
          title="审计保留（天）"
          description="过期的整天日志整片搬进 audit/archive/，不删除。0 = 一直留着"
        >
          <input
            aria-label="审计日志保留天数"
            type="number"
            min={0}
            max={3650}
            value={config.auditKeepDays}
            className={inputClass}
            onChange={(event) => {
              const value = Math.round(Number(event.target.value));
              if (Number.isFinite(value)) {
                void updateConfig({ auditKeepDays: Math.min(Math.max(value, 0), 3650) });
              }
            }}
          />
        </Row>

        <Row
          title="现在归档一次"
          description={
            archived === null
              ? "不必等重启：按上面的天数立刻搬一次旧分片"
              : /^\d+$/.test(archived)
                ? archived === "0"
                  ? "没有到期的分片，什么都没动"
                  : `搬走了 ${archived} 个旧分片，没有删除任何内容`
                : archived
          }
        >
          <div className="flex justify-end">
            <Button
              variant="subtle"
              size="sm"
              onClick={() => {
                void auditRotate()
                  .then((moved) => setArchived(String(moved)))
                  .catch((cause) =>
                    setArchived(
                      `没搬动：${cause instanceof Error ? cause.message : String(cause)}`,
                    ),
                  );
              }}
            >
              归档
            </Button>
          </div>
        </Row>

        <Row
          wide
          title="导出与清空"
          description={
            auditNote === null
              ? "导出把选定日期段（含两端）的活动分片拼成一份 JSONL；清空先把现有分片整批归档、再清空活动区——归档还在磁盘上，清空不销毁"
              : auditNote
          }
        >
          <div className="flex justify-end gap-2">
            <Button
              variant="subtle"
              size="sm"
              onClick={() => {
                const today = new Date().toISOString().slice(0, 10);
                const from = auditDate || today;
                void auditExport(from, today)
                  .then((dest) => setAuditNote(`已导出到 ${dest}`))
                  .catch((cause) =>
                    setAuditNote(
                      `没导出成：${cause instanceof Error ? cause.message : String(cause)}`,
                    ),
                  );
              }}
            >
              导出日志
            </Button>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => {
                void auditClear()
                  .then((moved) =>
                    setAuditNote(
                      moved.length > 0
                        ? `已清空：${moved.length} 个分片整批归档，原文件都在 audit/archive/ 里`
                        : "活动区本来是空的",
                    ),
                  )
                  .catch((cause) =>
                    setAuditNote(
                      `没清成：${cause instanceof Error ? cause.message : String(cause)}`,
                    ),
                  );
              }}
            >
              清空记录
            </Button>
          </div>
        </Row>

        <Row
          title="审计账本"
          description={
            auditNote ||
            (auditPage
              ? `${auditPage.date}：${auditPage.entries.length} 条${
                  auditPage.truncated ? "（只显示最新一截，更早的没送来）" : ""
                }${auditPage.skipped ? ` · ${auditPage.skipped} 行读不懂，已跳过` : ""}`
              : "谁在什么时候对什么做了什么、结果是放行还是拦下。这里只读不写：日志里的内容早就按类别遮过敏感项")
          }
        >
          <div className="flex items-center justify-end gap-2">
            <input
              type="text"
              value={auditDate}
              placeholder="2026-09-26，留空=今天"
              aria-label="审计归档起始日期"
              onChange={(event) => setAuditDate(event.target.value)}
              className={`${inputClass} h-8 w-44 text-xs`}
            />
            <Button
              variant="subtle"
              size="sm"
              onClick={() => {
                setAuditNote("");
                void auditView(auditDate)
                  .then((page) => setAuditPage(page))
                  .catch((cause) => {
                    // 读不懂的日期要说清是日期不对，不是"这一天没有记录"
                    setAuditPage(null);
                    setAuditNote(cause instanceof Error ? cause.message : String(cause));
                  });
              }}
            >
              读出来
            </Button>
          </div>
        </Row>

        {auditPage && auditPage.entries.length > 0 ? (
          <>
            <div className="max-h-64 overflow-y-auto rounded-lg border border-border bg-background">
              {pagedAudit.slice.map((entry, index) => (
                <div
                  key={`${entry.at}-${entry.action}-${index}`}
                  className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5 border-b border-border/60 px-3 py-1.5 text-xs last:border-b-0"
                >
                  <span className="text-muted-foreground">{entry.at}</span>
                  <span>{AUDIT_ACTOR_LABELS[entry.actor] ?? entry.actor}</span>
                  <span className="text-foreground">{entry.action}</span>
                  <span className="text-muted-foreground">{entry.target}</span>
                  <span>{AUDIT_OUTCOME_LABELS[entry.outcome] ?? entry.outcome}</span>
                  {entry.detail ? (
                    <span className="text-muted-foreground">{entry.detail}</span>
                  ) : null}
                </div>
              ))}
            </div>
            <PaginationBar
              page={pagedAudit.page}
              pages={pagedAudit.pages}
              total={pagedAudit.total}
              onPage={pagedAudit.setPage}
            />
          </>
        ) : null}
      </Group>
    </FormColumn>
  );
}
