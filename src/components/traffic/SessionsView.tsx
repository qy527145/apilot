import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { FoldVertical, ListFilter, MessagesSquare } from "lucide-react";

import { EmptyState } from "@/components/common/EmptyState";
import { ErrorState, TableSkeleton } from "@/components/common/StatCard";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  api,
  type SessionSummary,
  type ThreadKind,
  type ThreadSummary,
  type TimelineEntry,
} from "@/lib/api";
import { cn, formatMs, formatNumber, formatTime, quotaToUsd, shortSessionId } from "@/lib/utils";

const KIND_LABEL: Record<ThreadKind, string> = {
  main: "主线",
  agent: "子代理",
  side: "旁路",
};

const KIND_HINT: Record<ThreadKind, string> = {
  main: "你在对话的那条线：带工具、请求最多",
  agent: "带工具但不是主线：主线派出去干活的子代理",
  side: "不带工具的一次性任务：起标题、hook 求值、摘要",
};

/** 一条线的名字：类别 + 同类里的序号（主线只有一条，不编号）。 */
function threadLabels(threads: ThreadSummary[]): Map<string, string> {
  const seen: Record<ThreadKind, number> = { main: 0, agent: 0, side: 0 };
  const out = new Map<string, string>();
  for (const t of threads) {
    seen[t.kind] += 1;
    out.set(t.thread_key, t.kind === "main" ? KIND_LABEL.main : `${KIND_LABEL[t.kind]} ${seen[t.kind]}`);
  }
  return out;
}

interface Props {
  client: string | null;
  from: number | null;
  to: number | null;
  /** 打开某一条请求的详情（复用监控页的详情弹窗）。 */
  onOpenRequest: (requestId: string) => void;
  /** 切回请求列表，只看这个对话。 */
  onFilterRequests: (sessionId: string) => void;
}

export function SessionsView({ client, from, to, onOpenRequest, onFilterRequests }: Props) {
  const [selected, setSelected] = useState<string | null>(null);

  const sessions = useQuery({
    queryKey: ["sessions", client, from, to],
    queryFn: () => api.listSessions({ client, from, to }),
    refetchInterval: 3000,
    retry: 1,
  });

  // 没选时默认看最近活跃的那个 —— 进来就能看到东西，而不是一块空白。
  const current = selected ?? sessions.data?.[0]?.session_id ?? null;

  if (sessions.isLoading) {
    return (
      <Card className="py-0">
        <CardContent className="p-4">
          <TableSkeleton rows={6} cols={4} />
        </CardContent>
      </Card>
    );
  }
  if (sessions.isError) {
    return <ErrorState onRetry={() => sessions.refetch()} />;
  }
  if (!sessions.data?.length) {
    return (
      <Card className="py-0">
        <CardContent className="p-6">
          <EmptyState
            icon={MessagesSquare}
            title="还没有可追溯的对话"
            description="对话靠客户端在请求里带的会话 ID 归组（Claude Code / Codex 都带）。升级前的历史请求没有记录这个 ID，新请求进来后就会出现在这里。"
          />
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="grid gap-4 lg:grid-cols-[300px_1fr]">
      <Card className="py-0">
        <CardContent className="max-h-[calc(100vh-260px)] overflow-y-auto p-0">
          {sessions.data.map((s) => (
            <SessionRow
              key={s.session_id}
              s={s}
              active={s.session_id === current}
              onClick={() => setSelected(s.session_id)}
            />
          ))}
        </CardContent>
      </Card>

      {current && (
        <Timeline
          sessionId={current}
          onOpenRequest={onOpenRequest}
          onFilterRequests={onFilterRequests}
        />
      )}
    </div>
  );
}

function SessionRow({ s, active, onClick }: { s: SessionSummary; active: boolean; onClick: () => void }) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        "hover:bg-accent/50 block w-full border-b px-3 py-2.5 text-left last:border-b-0",
        active && "bg-accent",
      )}
    >
      <div className="flex items-center justify-between gap-2">
        <span className="truncate text-sm font-medium">
          {s.client}
          <span className="text-muted-foreground ml-1.5 font-mono text-xs" title={s.session_id}>
            #{shortSessionId(s.session_id)}
          </span>
        </span>
        <span className="text-muted-foreground shrink-0 text-xs tabular-nums">{formatTime(s.last_ts)}</span>
      </div>
      <div className="text-muted-foreground mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
        <span className="tabular-nums">{formatNumber(s.requests)} 次请求</span>
        {s.errors > 0 && <span className="text-destructive tabular-nums">{s.errors} 失败</span>}
        <span className="tabular-nums">{quotaToUsd(s.quota)}</span>
      </div>
      <div className="mt-1 flex flex-wrap gap-1">
        {s.models.map((m) => (
          <Badge key={m} variant="secondary" className="font-mono text-[11px]">
            {m}
          </Badge>
        ))}
        {s.models.length > 1 && (
          <Badge variant="warning" className="text-[11px]" title="这个对话中途换过模型">
            换过模型
          </Badge>
        )}
      </div>
    </button>
  );
}

function Timeline({
  sessionId,
  onOpenRequest,
  onFilterRequests,
}: {
  sessionId: string;
  onOpenRequest: (id: string) => void;
  onFilterRequests: (sessionId: string) => void;
}) {
  const [thread, setThread] = useState<string | "all">("all");
  const q = useQuery({
    queryKey: ["session-timeline", sessionId],
    queryFn: () => api.getSessionTimeline(sessionId),
    refetchInterval: 3000,
    retry: 1,
  });

  const labels = useMemo(() => threadLabels(q.data?.threads ?? []), [q.data]);
  const kindOf = useMemo(
    () => new Map((q.data?.threads ?? []).map((t) => [t.thread_key, t.kind] as const)),
    [q.data],
  );

  if (q.isLoading) {
    return (
      <Card className="py-0">
        <CardContent className="p-4">
          <TableSkeleton rows={8} cols={6} />
        </CardContent>
      </Card>
    );
  }
  if (q.isError || !q.data) return <ErrorState onRetry={() => q.refetch()} />;

  const entries = q.data.entries.filter((e) =>
    thread === "all" ? true : e.thread_key === thread,
  );
  const otherCount = q.data.entries.filter((e) => e.thread_key === null).length;

  return (
    <Card className="py-0">
      <CardContent className="space-y-3 p-3">
        <div className="flex flex-wrap items-center gap-2">
          <Button
            size="sm"
            variant={thread === "all" ? "secondary" : "ghost"}
            className="h-7"
            onClick={() => setThread("all")}
          >
            全部
            <span className="text-muted-foreground tabular-nums">· {q.data.entries.length}</span>
          </Button>
          {q.data.threads.map((t) => (
            <Button
              key={t.thread_key}
              size="sm"
              variant={thread === t.thread_key ? "secondary" : "ghost"}
              className="h-7"
              title={KIND_HINT[t.kind]}
              onClick={() => setThread(t.thread_key)}
            >
              {labels.get(t.thread_key)}
              <span className="text-muted-foreground tabular-nums">· {t.requests}</span>
              {t.compactions > 0 && (
                <span className="text-amber-500" title={`压缩过 ${t.compactions} 次`}>
                  · 压缩 {t.compactions}
                </span>
              )}
            </Button>
          ))}
          {otherCount > 0 && (
            <span
              className="text-muted-foreground text-xs"
              title="count_tokens 这类不是对话一轮的请求：在「全部」里按时间出现，但不属于任何一条线"
            >
              + {otherCount} 次计数等请求
            </span>
          )}
          <Button
            size="sm"
            variant="outline"
            className="ml-auto h-7"
            onClick={() => onFilterRequests(sessionId)}
            title="切到请求列表，只看这个对话的请求（可再叠加其它筛选与表达式）"
          >
            <ListFilter className="size-3.5" />
            在请求列表中查看
          </Button>
        </div>

        <div className="divide-y rounded-md border">
          {entries.map((e, i) => (
            <TimelineRow
              key={e.request_id}
              e={e}
              prevModel={i > 0 ? entries[i - 1].model : null}
              label={e.thread_key ? labels.get(e.thread_key) ?? "" : "计数"}
              kind={e.thread_key ? kindOf.get(e.thread_key) ?? null : null}
              onClick={() => onOpenRequest(e.request_id)}
            />
          ))}
        </div>
      </CardContent>
    </Card>
  );
}

function TimelineRow({
  e,
  prevModel,
  label,
  kind,
  onClick,
}: {
  e: TimelineEntry;
  prevModel: string | null;
  label: string;
  kind: ThreadKind | null;
  onClick: () => void;
}) {
  const modelChanged = prevModel !== null && prevModel !== e.model && e.model !== "";
  return (
    <>
      {e.compacted && (
        <div className="flex items-center gap-2 bg-amber-500/10 px-3 py-1.5 text-xs text-amber-600 dark:text-amber-500">
          <FoldVertical className="size-3.5" />
          上下文压缩：{label}从 {formatNumber(e.compacted_from)} 条消息降到{" "}
          {formatNumber(e.message_count)} 条
        </div>
      )}
      <button
        type="button"
        onClick={onClick}
        className={cn(
          "hover:bg-accent/50 grid w-full grid-cols-[96px_64px_minmax(150px,1fr)_52px_auto] items-center gap-3 px-3 py-2 text-left text-xs",
          kind !== "main" && "text-muted-foreground",
        )}
      >
        {/* 同一个对话里日期几乎不变，只留时分秒，把宽度让给模型名；完整时间在悬停里。 */}
        <span className="tabular-nums" title={formatTime(e.ts)}>
          {formatTime(e.ts).slice(6)}
        </span>
        <span>
          <Badge variant={kind === "main" ? "default" : "secondary"} className="text-[11px]">
            {label}
          </Badge>
        </span>
        <span className="min-w-0 truncate">
          <span className={cn("font-mono", modelChanged && "text-amber-600 dark:text-amber-500")}>
            {modelChanged && "→ "}
            {e.model || "—"}
          </span>
          {e.thread_key === null && (
            <span className="text-muted-foreground ml-2 font-mono">{e.path}</span>
          )}
          {e.error_message && (
            <span className="text-destructive ml-2" title={e.error_message}>
              {e.error_message}
            </span>
          )}
        </span>
        {/* 单独一列：和模型名挤在一起时会被截成「1 …」，而它恰恰是看压缩要盯的数。 */}
        <span className="text-muted-foreground text-right tabular-nums" title="这次请求带的消息条数">
          {e.message_count != null ? `${formatNumber(e.message_count)} 条` : ""}
        </span>
        <span className="flex items-center gap-3 tabular-nums">
          <span className="whitespace-nowrap" title="输入 / 缓存读 / 输出">
            {formatNumber(e.input_tokens)}
            {e.cache_read_tokens > 0 && (
              <span className="text-muted-foreground"> +{formatNumber(e.cache_read_tokens)}</span>
            )}{" "}
            / {formatNumber(e.output_tokens)}
          </span>
          <span className="w-16 text-right">{formatMs(e.latency_ms)}</span>
          <span className="w-16 text-right">{quotaToUsd(e.quota)}</span>
          <Badge variant={e.status_code < 400 ? "success" : "destructive"}>{e.status_code}</Badge>
        </span>
      </button>
    </>
  );
}
