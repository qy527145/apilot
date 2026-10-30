import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { FoldVertical, ListFilter, MessagesSquare, Pin, PinOff, Search, SortAsc, SortDesc } from "lucide-react";
import { toast } from "sonner";

import { EmptyState } from "@/components/common/EmptyState";
import { ErrorState, TableSkeleton } from "@/components/common/StatCard";
import type { LiveRequest } from "@/components/traffic/LiveStreamDialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import {
  api,
  type PinScope,
  type SessionPin,
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
  /**
   * 正在进行（还没落库）的请求。与请求列表同一份底账。
   *
   * 少了它，一条对话最要紧的那一轮 —— 正在跑的那一轮 —— 恰恰不在链路里：
   * 流式请求在整条流结束前根本不落库，用户只能盯着一个"最后一条是五分钟前"的
   * 列表猜。这里把它们按会话 ID 挑出来，挂在对应线上。
   */
  live: LiveRequest[];
  /** 打开正在进行的流式请求的实时视图。 */
  onOpenLive: (requestId: string) => void;
  /**
   * 打开某一条请求的详情（复用监控页的详情弹窗）。`order` 是时间线此刻在屏幕上的
   * 顺序（只含已落库的），详情弹窗按它翻上一条 / 下一条。
   */
  onOpenRequest: (requestId: string, order: string[]) => void;
  /** 切回请求列表，只看这个对话。 */
  onFilterRequests: (sessionId: string) => void;
}

export function SessionsView({
  client,
  from,
  to,
  live,
  onOpenLive,
  onOpenRequest,
  onFilterRequests,
}: Props) {
  const [selected, setSelected] = useState<string | null>(null);
  const [keyword, setKeyword] = useState("");

  const sessions = useQuery({
    queryKey: ["sessions", client, from, to],
    queryFn: () => api.listSessions({ client, from, to }),
    refetchInterval: 3000,
    retry: 1,
  });

  // 过滤放在前端：一次最多 100 条、3 秒一刷，为这点事再跑一趟后端不值得。
  // 会话 ID 也参与匹配 —— 标题取不到的对话在列表上显示的就是它。
  const visible = useMemo(() => {
    const kw = keyword.trim().toLowerCase();
    const all = sessions.data ?? [];
    if (!kw) return all;
    return all.filter((s) =>
      [s.title, s.session_id, s.client].some((f) => f?.toLowerCase().includes(kw)),
    );
  }, [sessions.data, keyword]);

  // 有请求正在跑的对话，在列表上一眼看得见 —— 否则"我现在这个对话"跟别的长得一样。
  const liveSessions = useMemo(
    () => new Set(live.map((r) => r.session_id).filter((id): id is string => !!id)),
    [live],
  );

  // 没选时默认看最近活跃的那个 —— 进来就能看到东西，而不是一块空白。
  // 选中的那条被搜掉时也回落：右边挂着一个列表里看不见的对话更让人发懵。
  const current = visible.some((s) => s.session_id === selected)
    ? selected
    : visible[0]?.session_id ?? null;

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
    // 第二列用 `minmax(0,1fr)` 而不是 `1fr`：grid 子项的默认最小宽度是
    // `auto`（= 内容宽度），时间线里那几列不换行的数字会把整页撑宽。窄窗口下
    // 页面横向溢出之后，左侧 padding 会被滚出视野（见 PageShell 的 onScroll）。
    <div className="grid gap-4 lg:grid-cols-[300px_minmax(0,1fr)]">
      <Card className="py-0">
        <div className="border-b p-2">
          <div className="relative">
            <Search className="text-muted-foreground pointer-events-none absolute top-1/2 left-2 size-3.5 -translate-y-1/2" />
            <Input
              value={keyword}
              onChange={(e) => setKeyword(e.target.value)}
              placeholder="搜标题 / 会话 ID"
              className="h-8 pl-7 text-xs"
            />
          </div>
        </div>
        <CardContent className="max-h-[calc(100vh-300px)] overflow-y-auto p-0">
          {visible.map((s) => (
            <SessionRow
              key={s.session_id}
              s={s}
              active={s.session_id === current}
              running={liveSessions.has(s.session_id)}
              onClick={() => setSelected(s.session_id)}
            />
          ))}
          {visible.length === 0 && (
            <p className="text-muted-foreground p-4 text-xs">没有匹配的对话。</p>
          )}
        </CardContent>
      </Card>

      {current && (
        <Timeline
          sessionId={current}
          live={live}
          onOpenLive={onOpenLive}
          onOpenRequest={onOpenRequest}
          onFilterRequests={onFilterRequests}
        />
      )}
    </div>
  );
}

function SessionRow({
  s,
  active,
  running,
  onClick,
}: {
  s: SessionSummary;
  active: boolean;
  /** 这个对话此刻有请求在跑（还没落库，所以列表里的数字还没算上它）。 */
  running: boolean;
  onClick: () => void;
}) {
  // 标题是主角：几个对话同时开着时，"claude-code #668fff86" 谁也认不出来。
  // 取不到标题（升级前的老对话、开场只有一张图）才回落到它 —— 那一行本来就是这个。
  const fallback = (
    <>
      {s.client}
      <span className="text-muted-foreground ml-1.5 font-mono text-xs">#{shortSessionId(s.session_id)}</span>
    </>
  );
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
        <span className="truncate text-sm font-medium" title={s.title ?? s.session_id}>
          {s.title ?? fallback}
        </span>
        <span className="text-muted-foreground flex shrink-0 items-center gap-1.5 text-xs tabular-nums">
          {running && (
            <span className="inline-block size-1.5 animate-pulse rounded-full bg-emerald-500" title="有请求正在跑" />
          )}
          {formatTime(s.last_ts)}
        </span>
      </div>
      <div className="text-muted-foreground mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
        {s.title && (
          <span title={s.session_id}>
            {s.client}
            <span className="ml-1 font-mono">#{shortSessionId(s.session_id)}</span>
          </span>
        )}
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
        {s.pin && (
          <Badge variant="default" className="gap-1 text-[11px]" title={`已固定到 ${s.pin.model}`}>
            <Pin className="size-3" />
            {s.pin.model}
          </Badge>
        )}
      </div>
    </button>
  );
}

/** 时间线上的一行：已经落库的，或正在跑的。 */
type Row = { kind: "entry"; e: TimelineEntry } | { kind: "live"; r: LiveRequest };

const rowTs = (row: Row) => (row.kind === "entry" ? row.e.ts : row.r.ts);

/**
 * 正在进行的请求在链路里的样子。
 *
 * 只点得开**流式**的：非流式的进行中请求还没有可看的东西 —— 报文要等它落库，
 * 而它在请求结束前根本不存在于数据库里。和请求列表里那条一样，点了不响应，
 * 但这里给出为什么（悬停说明），免得用户以为是坏了。
 */
function LiveTimelineRow({
  r,
  label,
  kind,
  onOpen,
}: {
  r: LiveRequest;
  label: string;
  kind: ThreadKind | null;
  onOpen: () => void;
}) {
  // 线还没分类（这条线的请求一条都没落库）时不当它是旁路 —— 那会把它压暗，
  // 而它恰恰是列表里唯一一行。
  const dim = kind !== null && kind !== "main";
  return (
    <button
      type="button"
      onClick={onOpen}
      disabled={!r.is_stream}
      title={
        r.is_stream
          ? "流式请求：点开看实时事件流（还没落库，明细要等它结束）"
          : "非流式请求要跑完才落库，在那之前没有可看的报文"
      }
      className={cn(
        "grid w-full min-w-[720px] grid-cols-[96px_64px_minmax(150px,1fr)_52px_auto] items-center gap-3 px-3 py-2 text-left text-xs",
        r.is_stream ? "hover:bg-emerald-500/5" : "cursor-default",
        dim && "text-muted-foreground",
      )}
    >
      {/* 绿点与请求列表里那个同一套语言：进行中。 */}
      <span className="flex items-center gap-1.5 tabular-nums" title={formatTime(r.ts)}>
        <span className="inline-block size-1.5 shrink-0 animate-pulse rounded-full bg-emerald-500" />
        {formatTime(r.ts).slice(6)}
      </span>
      <span>
        {label ? (
          <Badge variant={kind === "main" ? "default" : "secondary"} className="text-[11px]">
            {label}
          </Badge>
        ) : (
          <Badge variant="success" className="text-[11px]">
            进行中
          </Badge>
        )}
      </span>
      <span className="flex min-w-0 items-baseline gap-2">
        <span className="shrink-0 font-mono">{r.model || "—"}</span>
        {r.request_model && r.request_model !== r.model && (
          <span
            className="text-muted-foreground min-w-0 truncate font-mono text-[11px]"
            title={`客户端请求的模型：${r.request_model}`}
          >
            ← {r.request_model}
          </span>
        )}
        {!r.is_stream && <span className="text-muted-foreground shrink-0">非流式</span>}
      </span>
      <span className="text-muted-foreground text-right" title="还没结束，轮数要等它落库才有">
        —
      </span>
      <span className="flex items-center gap-3 tabular-nums">
        <span className="text-muted-foreground whitespace-nowrap">
          {r.is_stream ? `已收 ${formatNumber(r.frames.length)} 帧` : "等上游"}
        </span>
        <span className="w-16 text-right">—</span>
        <span className="w-16 text-right">—</span>
        <Badge variant="success">进行中</Badge>
      </span>
    </button>
  );
}

function Timeline({
  sessionId,
  live,
  onOpenLive,
  onOpenRequest,
  onFilterRequests,
}: {
  sessionId: string;
  live: LiveRequest[];
  onOpenLive: (id: string) => void;
  onOpenRequest: (id: string, order: string[]) => void;
  onFilterRequests: (sessionId: string) => void;
}) {
  const [thread, setThread] = useState<string | "all">("all");
  // 默认最新在前：一条对话几百轮，想看的几乎总是"刚刚发生了什么"。
  const [newestFirst, setNewestFirst] = useState(true);
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
  // 「换了模型」只在同一条线里比：主线后面插一条起标题的旁路，模型不同是常态，
  // 拿相邻两行比会满屏都是假的「换模型」。不属于线的请求（count_tokens）不参与。
  const prevModelOf = new Map<string, string | null>();
  {
    const last = new Map<string, string>();
    for (const e of q.data.entries) {
      if (!e.thread_key) continue;
      prevModelOf.set(e.request_id, last.get(e.thread_key) ?? null);
      if (e.model) last.set(e.thread_key, e.model);
    }
  }
  const otherCount = q.data.entries.filter((e) => e.thread_key === null).length;
  // 正在进行的请求按线的归属筛（和上面一样：不属于任何线的只在「全部」里出现）。
  // 它们的起点天然晚于已落库的那些，所以并进来之后自己就排在最新那一头。
  const liveHere = live.filter(
    (r) => r.session_id === sessionId && (thread === "all" || r.thread_key === thread),
  );
  const rows: Row[] = [
    ...entries.map((e) => ({ kind: "entry" as const, e })),
    ...liveHere.map((r) => ({ kind: "live" as const, r })),
  ].sort((a, b) => rowTs(a) - rowTs(b));

  // 只反转**展示**用的这一份：上面 `prevModelOf` 的"上一条"必须按时间正序算，
  // 「压缩横幅」也要跟在自己那条请求身上，倒着渲染不该改变它们的含义。
  const shown = newestFirst ? [...rows].reverse() : rows;
  // 进行中的那几条走实时视图，不进详情弹窗的翻页序列。
  const entryOrder = shown.flatMap((row) => (row.kind === "entry" ? [row.e.request_id] : []));

  return (
    // `min-w-0`：作为外层 grid 的第二列，它必须能缩到比内容更窄，
    // 否则那几列数字会把整页撑出横向溢出。
    <Card className="min-w-0 py-0">
      <CardContent className="min-w-0 space-y-3 p-3">
        <div className="flex min-w-0 items-baseline gap-2">
          {q.data.title && <h2 className="truncate text-sm font-medium">{q.data.title}</h2>}
          <span className="text-muted-foreground shrink-0 font-mono text-xs" title={sessionId}>
            #{shortSessionId(sessionId)}
          </span>
        </div>
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
          <div className="ml-auto flex min-w-0 items-center gap-2">
          <Button
            size="sm"
            variant="ghost"
            className="h-7 shrink-0"
            onClick={() => setNewestFirst((v) => !v)}
            title={newestFirst ? "现在按时间逆序（最新在前），点击改成正序" : "现在按时间正序（最早在前），点击改成逆序"}
          >
            {newestFirst ? <SortDesc className="size-3.5" /> : <SortAsc className="size-3.5" />}
            {newestFirst ? "最新在前" : "最早在前"}
          </Button>
          <SessionPinControl sessionId={sessionId} pin={q.data.pin ?? null} />
          <Button
            size="sm"
            variant="outline"
            className="h-7 shrink-0"
            onClick={() => onFilterRequests(sessionId)}
            title="切到请求列表，只看这个对话的请求（可再叠加其它筛选与表达式）"
          >
            <ListFilter className="size-3.5" />
            在请求列表中查看
          </Button>
          </div>
        </div>

        {/* 行本身有最小宽度（五列数字都不换行）。窗口窄时让**这张卡片内部**
            横向滚动，而不是把整个页面撑宽 —— 后者会把左侧 padding 滚没。 */}
        <div className="divide-y overflow-x-auto rounded-md border">
          {shown.map((row) =>
            row.kind === "live" ? (
              <LiveTimelineRow
                key={row.r.request_id}
                r={row.r}
                label={row.r.thread_key ? labels.get(row.r.thread_key) ?? "" : ""}
                kind={row.r.thread_key ? kindOf.get(row.r.thread_key) ?? null : null}
                onOpen={() => onOpenLive(row.r.request_id)}
              />
            ) : (
              <TimelineRow
                key={row.e.request_id}
                e={row.e}
                prevModel={prevModelOf.get(row.e.request_id) ?? null}
                label={row.e.thread_key ? labels.get(row.e.thread_key) ?? "" : "计数"}
                kind={row.e.thread_key ? kindOf.get(row.e.thread_key) ?? null : null}
                onClick={() => onOpenRequest(row.e.request_id, entryOrder)}
              />
            ),
          )}
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
          "hover:bg-accent/50 grid w-full min-w-[720px] grid-cols-[96px_64px_minmax(150px,1fr)_52px_auto] items-center gap-3 px-3 py-2 text-left text-xs",
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
        <span className="min-w-0">
          <span className="flex min-w-0 items-baseline gap-2">
            <span
              className={cn("shrink-0 font-mono", modelChanged && "text-amber-600 dark:text-amber-500")}
              title={modelChanged ? `这条线上一次用的是 ${prevModel}` : e.model}
            >
              {modelChanged && "→ "}
              {e.model || "—"}
            </span>
            {/* 与进行中那行同一套写法：生效模型在前，客户端原本要的模型在后。
                两者相同的请求（绝大多数）不显示，免得每行都挂一截噪音。 */}
            {e.request_model && e.request_model !== e.model && (
              <span
                className="text-muted-foreground min-w-0 truncate font-mono text-[11px]"
                title={`客户端请求的模型：${e.request_model}`}
              >
                ← {e.request_model}
              </span>
            )}
            {e.thread_key === null && (
              // 模型名优先：路径在这一行只是辅助说明（标签已经写了「计数」），挤不下就截它。
              <span className="text-muted-foreground min-w-0 truncate font-mono" title={e.path}>
                {e.path}
              </span>
            )}
            {e.error_message && (
              <span className="text-destructive truncate" title={e.error_message}>
                {e.error_message}
              </span>
            )}
          </span>
          {/* 单独一行：和模型名挤在一起时总是先被截掉，而它恰恰说明了"为什么是这个模型"。 */}
          {e.pinned_from && (
            <span
              className="text-muted-foreground flex items-center gap-0.5 truncate text-[11px]"
              title={`客户端发的是 ${e.pinned_from}，被对话固定改成了 ${e.model}`}
            >
              <Pin className="size-3 shrink-0" />
              <span className="truncate font-mono">{e.pinned_from}</span>
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

const SCOPE_LABEL: Record<PinScope, string> = {
  tools: "主线与子代理",
  all: "全部请求",
};

const SCOPE_HINT: Record<PinScope, string> = {
  tools: "只改带工具的请求。起标题、hook 求值这类旁路是客户端特意用小模型跑的，保持原样",
  all: "会话里的请求全部改用这个模型，旁路也算",
};

/**
 * 「把这个对话固定到某个模型」。
 *
 * 候选来自渠道声明过的模型，但也允许手填：通吃的渠道（没声明模型列表）照样能接
 * 一个列表里没有的名字，只给选不给填会把这类用法挡掉。
 */
function SessionPinControl({ sessionId, pin }: { sessionId: string; pin: SessionPin | null }) {
  const qc = useQueryClient();
  const [open, setOpen] = useState(false);
  const [model, setModel] = useState(pin?.model ?? "");
  const [scope, setScope] = useState<PinScope>(pin?.scope ?? "tools");

  const options = useQuery({
    queryKey: ["model_options"],
    queryFn: api.listModelOptions,
    enabled: open,
    staleTime: 30_000,
  });

  const refresh = () => {
    qc.invalidateQueries({ queryKey: ["session-timeline", sessionId] });
    qc.invalidateQueries({ queryKey: ["sessions"] });
  };
  const onError = (e: unknown) =>
    toast.error((e as { message?: string })?.message ?? String(e));

  const save = useMutation({
    mutationFn: () => api.pinSessionModel(sessionId, model.trim(), scope),
    onSuccess: () => {
      toast.success(`已固定到 ${model.trim()}，这个对话的下一个请求起生效`);
      setOpen(false);
      refresh();
    },
    onError,
  });
  const clear = useMutation({
    mutationFn: () => api.unpinSessionModel(sessionId),
    onSuccess: () => {
      toast.success("已取消固定，恢复按模型策略选模型");
      setOpen(false);
      refresh();
    },
    onError,
  });

  const filtered = (options.data ?? []).filter((o) =>
    o.model.toLowerCase().includes(model.trim().toLowerCase()),
  );

  return (
    <Popover
      open={open}
      onOpenChange={(o) => {
        setOpen(o);
        // 每次打开都从当前生效的那份起步，别留着上次没保存的草稿。
        if (o) {
          setModel(pin?.model ?? "");
          setScope(pin?.scope ?? "tools");
        }
      }}
    >
      <PopoverTrigger asChild>
        <Button
          size="sm"
          variant={pin ? "default" : "outline"}
          className="h-7 max-w-56 min-w-0"
          title={pin ? `已固定到 ${pin.model}（${SCOPE_LABEL[pin.scope]}）` : "让这个对话改用指定模型"}
        >
          <Pin className="size-3.5 shrink-0" />
          <span className="truncate font-mono">{pin ? pin.model : "固定模型"}</span>
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-80 space-y-3">
        <div className="space-y-1">
          <p className="text-sm font-medium">这个对话用哪个模型</p>
          <p className="text-muted-foreground text-xs">
            只影响这一个对话，优先于全局与客户端的模型策略。客户端界面上仍显示它自己选的模型 ——
            换成上下文窗口更小的模型时，可能还没等客户端自动压缩就先超长报错。
          </p>
        </div>

        <Input
          value={model}
          onChange={(e) => setModel(e.target.value)}
          placeholder="模型名，可从下面选或直接填"
          className="h-8 font-mono text-xs"
          autoFocus
        />
        <div className="max-h-40 overflow-y-auto rounded-md border">
          {options.isLoading ? (
            <p className="text-muted-foreground p-2 text-xs">加载中…</p>
          ) : filtered.length === 0 ? (
            <p className="text-muted-foreground p-2 text-xs">
              渠道里没有声明匹配的模型；照样可以固定，前提是有渠道接受这个名字。
            </p>
          ) : (
            filtered.map((o) => (
              <button
                key={o.model}
                type="button"
                onClick={() => setModel(o.model)}
                className={cn(
                  "hover:bg-accent flex w-full items-center justify-between px-2 py-1 text-left text-xs",
                  o.model === model.trim() && "bg-accent",
                )}
              >
                <span className="truncate font-mono">{o.model}</span>
                <span className="text-muted-foreground shrink-0">{o.provider_count} 个渠道</span>
              </button>
            ))
          )}
        </div>

        <div className="space-y-1.5">
          <p className="text-xs font-medium">作用范围</p>
          <div className="flex gap-1.5">
            {(["tools", "all"] as const).map((s) => (
              <Button
                key={s}
                size="sm"
                variant={scope === s ? "secondary" : "ghost"}
                className="h-7 flex-1"
                title={SCOPE_HINT[s]}
                onClick={() => setScope(s)}
              >
                {SCOPE_LABEL[s]}
              </Button>
            ))}
          </div>
          <p className="text-muted-foreground text-[11px]">{SCOPE_HINT[scope]}</p>
        </div>

        <div className="flex justify-between gap-2">
          {pin ? (
            <Button
              size="sm"
              variant="ghost"
              className="text-muted-foreground h-8"
              disabled={clear.isPending}
              onClick={() => clear.mutate()}
            >
              <PinOff className="size-3.5" />
              取消固定
            </Button>
          ) : (
            <span />
          )}
          <Button
            size="sm"
            className="h-8"
            disabled={!model.trim() || save.isPending}
            onClick={() => save.mutate()}
          >
            {pin ? "更新" : "固定"}
          </Button>
        </div>
      </PopoverContent>
    </Popover>
  );
}
