import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Activity, Clock, Pause, Play, RefreshCw, Trash2, X } from "lucide-react";
import { toast } from "sonner";

import { EmptyState } from "@/components/common/EmptyState";
import { ErrorState, TableSkeleton } from "@/components/common/StatCard";
import { PageShell } from "@/components/layout/PageShell";
import { FilterExprPanel } from "@/components/traffic/FilterExprPanel";
import { LiveStreamDialog, MAX_LIVE_FRAMES, type LiveRequest } from "@/components/traffic/LiveStreamDialog";
import { RequestDetailDialog } from "@/components/traffic/RequestDetailDialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Separator } from "@/components/ui/separator";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { qk } from "@/hooks/queries";
import {
  ALL_PROTOCOLS,
  api,
  PROTOCOL_LABEL,
  type LogFilter,
  type LogStatus,
  type Protocol,
} from "@/lib/api";
import { useApilotEvent, type TrafficSnapshot } from "@/lib/events";
import { buildInflightContext, compileExpr, inflightMatches } from "@/lib/logExpr";
import {
  cn,
  formatMs,
  formatNumber,
  formatTime,
  quotaToUsd,
  truncate,
} from "@/lib/utils";

const LOGS_QUERY_KEY = "traffic-live";

/** 下拉里的「不限」。Radix 的 Select 不接受空串。 */
const ANY = "__any__";

const PAGE_SIZE = 100;
const MAX_LIMIT = 1000;

/** 流结束后，实时条目在内存里再留多久。够用户看完最后几帧，又不至于一直占着。 */
const DONE_TTL_MS = 120_000;

type RangePreset = "all" | "5m" | "1h" | "24h" | "today";

const RANGE_LABEL: Record<RangePreset, string> = {
  all: "不限",
  "5m": "5 分钟",
  "1h": "1 小时",
  "24h": "24 小时",
  today: "今天",
};

function rangeFrom(preset: RangePreset): number | null {
  const now = Date.now();
  switch (preset) {
    case "all":
      return null;
    case "5m":
      return now - 5 * 60_000;
    case "1h":
      return now - 60 * 60_000;
    case "24h":
      return now - 24 * 60 * 60_000;
    case "today": {
      const d = new Date(now);
      d.setHours(0, 0, 0, 0);
      return d.getTime();
    }
  }
}

const hostOf = (url: string) => url.replace(/^https?:\/\//, "");

export default function TrafficPage() {
  const qc = useQueryClient();
  const [selected, setSelected] = useState<string | null>(null);
  const [liveStream, setLiveStream] = useState<string | null>(null);
  const [live, setLive] = useState(true);
  const [confirmClear, setConfirmClear] = useState(false);
  const [snapshot, setSnapshot] = useState<TrafficSnapshot | null>(null);

  // --- 筛选条件 ---
  // 时间单独控（见下方的时间行）；其余是高频条件，平铺在筛选卡第一行。
  const [range, setRange] = useState<RangePreset>("all");
  const [client, setClient] = useState<string>(ANY);
  /** 客户端请求时写的模型名（`request_model`）。 */
  const [requestModel, setRequestModel] = useState<string>(ANY);
  /** 实际路由到的模型名（`model`）。与上面是两个轴，别合并。 */
  const [routedModel, setRoutedModel] = useState<string>(ANY);
  const [protocol, setProtocol] = useState<string>(ANY);
  const [status, setStatus] = useState<string>(ANY);
  const [onlyStream, setOnlyStream] = useState(false);
  /** 自定义 JS 表达式。空 = 不筛。 */
  const [expr, setExpr] = useState("");
  const [limit, setLimit] = useState(PAGE_SIZE);

  const from = useMemo(() => rangeFrom(range), [range]);

  const filter: LogFilter = useMemo(
    () => ({
      limit,
      offset: 0,
      from,
      client: client === ANY ? null : client,
      request_model: requestModel === ANY ? null : requestModel,
      model: routedModel === ANY ? null : routedModel,
      protocol: protocol === ANY ? null : (protocol as Protocol),
      status: status === ANY ? null : (status as LogStatus),
      is_stream: onlyStream ? true : null,
      expr: expr.trim() || null,
    }),
    [limit, from, client, requestModel, routedModel, protocol, status, onlyStream, expr],
  );

  const { data, isLoading, isError, error, refetch, isFetching } = useQuery({
    queryKey: qk.logs(`${LOGS_QUERY_KEY}-${JSON.stringify(filter)}`),
    queryFn: () => api.queryLogs(filter),
    refetchInterval: live ? 1000 : false,
    retry: 1,
  });

  const facets = useQuery({
    queryKey: qk.logs("facets"),
    queryFn: api.listLogFacets,
    staleTime: 30_000,
    retry: 1,
  });

  useApilotEvent("apilot://traffic", setSnapshot);

  // --- 进行中的请求 ---
  const [inflight, setInflight] = useState<Map<string, LiveRequest>>(new Map());

  // 进入页面时查询当前正在进行的请求，补齐用户进入前已开始的请求。
  // 这些请求不会触发 apilot://request-start（那个已经在进页面前发出去了）。
  const inflightQuery = useQuery({
    queryKey: ["inflight_seed"],
    queryFn: api.getInflightRequests,
    // 只在挂载时取一次；之后靠事件驱动维护。
    staleTime: Infinity,
    retry: 1,
  });

  useEffect(() => {
    if (!inflightQuery.data) return;
    setInflight((prev) => {
      const next = new Map(prev);
      for (const r of inflightQuery.data) {
        // 只补齐之前没有的；事件已经处理过的不覆盖（那些可能有帧数据了）。
        if (!next.has(r.request_id)) {
          next.set(r.request_id, {
            request_id: r.request_id,
            ts: r.ts,
            client: r.client,
            model: r.model,
            request_model: r.request_model,
            path: r.path,
            protocol_in: r.protocol_in,
            provider_tag: r.provider_tag,
            provider_name: r.provider_name,
            upstream_url: r.upstream_url,
            is_stream: r.is_stream,
            frames: [],
            done: false,
            truncated: false,
          });
        }
      }
      return next;
    });
  }, [inflightQuery.data]);

  const markDone = (cur: LiveRequest, error?: string | null): LiveRequest => ({
    ...cur,
    done: true,
    done_at: Date.now(),
    error: error ?? cur.error,
  });

  const pruneDone = (m: Map<string, LiveRequest>, now: number) => {
    for (const [id, r] of m) {
      if (r.done && now - (r.done_at ?? now) > DONE_TTL_MS) m.delete(id);
    }
  };

  useApilotEvent("apilot://request-start", (p) => {
    setInflight((prev) => {
      const next = new Map(prev);
      pruneDone(next, Date.now());
      next.set(p.request_id, {
        request_id: p.request_id,
        ts: p.ts,
        client: p.client,
        model: p.model,
        request_model: p.request_model,
        path: p.path,
        protocol_in: p.protocol_in,
        provider_tag: p.provider_tag,
        provider_name: p.provider_name,
        upstream_url: p.upstream_url,
        is_stream: p.is_stream,
        frames: [],
        done: false,
        truncated: false,
      });
      return next;
    });
  });

  useApilotEvent("apilot://stream", (batch) => {
    setInflight((prev) => {
      const cur = prev.get(batch.request_id);
      if (!cur) return prev;

      const next = new Map(prev);
      if (batch.done) {
        next.set(batch.request_id, markDone(cur, batch.error));
        return next;
      }

      next.set(batch.request_id, {
        ...cur,
        frames: [...cur.frames, ...batch.frames].slice(-MAX_LIVE_FRAMES),
        truncated: cur.truncated || batch.truncated,
      });
      return next;
    });
  });

  useApilotEvent("apilot://request-end", (p) => {
    setInflight((prev) => {
      const cur = prev.get(p.request_id);
      if (!cur) return prev;
      const next = new Map(prev);
      if (cur.is_stream) next.set(p.request_id, markDone(cur, p.error));
      else next.delete(p.request_id);
      return next;
    });
  });

  // 表达式编译一次复用。空表达式编译为 null，`inflightMatches` 对 null 一律放行。
  const compiledExpr = useMemo(() => compileExpr(expr), [expr]);

  // 进行中且未结束的请求，按时间倒序（最新的在最上方）。
  // 同时应用与历史日志相同的筛选条件 —— 筛选器对进行中请求同样生效。
  //
  // 表达式这边走的是前端求值（`lib/logExpr.ts`）：进行中的请求没有捕获报文、
  // 也不分页，本地算就够了。响应侧字段一律为 null，所以只读请求侧的表达式
  // 才能在这几行上命中。
  const inflightList = useMemo(
    () =>
      [...inflight.values()]
        .filter((r) => {
          if (r.done) return false;
          if (from !== null && r.ts < from) return false;
          if (client !== ANY && r.client !== client) return false;
          if (requestModel !== ANY && r.request_model !== requestModel) return false;
          if (routedModel !== ANY && r.model !== routedModel) return false;
          if (protocol !== ANY && r.protocol_in !== protocol) return false;
          if (onlyStream && !r.is_stream) return false;
          return inflightMatches(compiledExpr, buildInflightContext(r));
        })
        .sort((a, b) => b.ts - a.ts),
    [
      inflight,
      from,
      client,
      requestModel,
      routedModel,
      protocol,
      onlyStream,
      compiledExpr,
    ],
  );

  const clear = useMutation({
    mutationFn: api.clearLogs,
    onSuccess: (res) => {
      setSelected(null);
      qc.invalidateQueries({ queryKey: qk.logs(LOGS_QUERY_KEY) });
      toast.success(`已清空 ${res.logs} 条请求日志、${res.captures} 条原文捕获`);
      setConfirmClear(false);
    },
  });

  const items = data?.items ?? [];
  const total = data?.total ?? 0;

  const hasFilter =
    range !== "all" ||
    client !== ANY ||
    requestModel !== ANY ||
    routedModel !== ANY ||
    protocol !== ANY ||
    status !== ANY ||
    onlyStream ||
    expr.trim() !== "";

  const resetFilters = () => {
    setRange("all");
    setClient(ANY);
    setRequestModel(ANY);
    setRoutedModel(ANY);
    setProtocol(ANY);
    setStatus(ANY);
    setOnlyStream(false);
    setExpr("");
    setLimit(PAGE_SIZE);
  };

  const openLive = liveStream ? inflight.get(liveStream) ?? null : null;

  // 表达式在后端执行，其错因（语法错、执行超时）只有查询失败时才拿得到，
  // 顺手显示出来 —— 否则用户只能看到一个笼统的「加载失败」。
  const errorMessage = error instanceof Error ? error.message : null;

  return (
    <PageShell
      title="监控"
      description="实时请求流，点击任意一行查看请求 / 响应明细"
      actions={
        <div className="flex items-center gap-4">
          <div className="hidden items-center gap-3 text-xs sm:flex">
            <span className="text-muted-foreground">
              RPS <span className="text-foreground font-medium tabular-nums">{snapshot?.rps?.toFixed(1) ?? "0.0"}</span>
            </span>
            <span className="text-muted-foreground">
              并发 <span className="text-foreground font-medium tabular-nums">{snapshot?.active ?? 0}</span>
            </span>
            <span className="text-muted-foreground">
              P50 TTFB <span className="text-foreground font-medium tabular-nums">{formatMs(snapshot?.ttfb_p50_ms)}</span>
            </span>
          </div>
          <Button variant="outline" size="sm" onClick={() => refetch()}>
            <RefreshCw className={cn("size-4", isFetching && "animate-spin")} />
            刷新
          </Button>
          <Button
            variant={live ? "default" : "outline"}
            size="sm"
            onClick={() => setLive((v) => !v)}
          >
            {live ? <Pause className="size-4" /> : <Play className="size-4" />}
            {live ? "暂停" : "继续"}
          </Button>
          <Button
            variant="outline"
            size="sm"
            onClick={() => setConfirmClear(true)}
          >
            <Trash2 className="size-4" />
            清空
          </Button>
        </div>
      }
    >
      <div className="space-y-6">
        <Card className="py-0">
          <CardContent className="space-y-3 p-3">
            {/* 高频筛选条件 */}
            <div className="flex flex-wrap items-center gap-3">
              <Select value={client} onValueChange={setClient}>
                <SelectTrigger size="sm" className="h-8 w-36">
                  <SelectValue placeholder="客户端" />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value={ANY}>全部客户端</SelectItem>
                  {(facets.data?.clients ?? []).map((c) => (
                    <SelectItem key={c} value={c}>
                      {c}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>

              <Select value={requestModel} onValueChange={setRequestModel}>
                <SelectTrigger size="sm" className="h-8 w-44">
                  <SelectValue placeholder="请求模型" />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value={ANY}>全部请求模型</SelectItem>
                  {(facets.data?.models ?? []).map((m) => (
                    <SelectItem key={m} value={m}>
                      {m}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>

              <Select value={routedModel} onValueChange={setRoutedModel}>
                <SelectTrigger size="sm" className="h-8 w-44">
                  <SelectValue placeholder="实际模型" />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value={ANY}>全部实际模型</SelectItem>
                  {(facets.data?.routed_models ?? []).map((m) => (
                    <SelectItem key={m} value={m}>
                      {m}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>

              <Select value={protocol} onValueChange={setProtocol}>
                <SelectTrigger size="sm" className="h-8 w-40">
                  <SelectValue placeholder="协议" />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value={ANY}>全部协议</SelectItem>
                  {ALL_PROTOCOLS.map((p) => (
                    <SelectItem key={p} value={p}>
                      {PROTOCOL_LABEL[p]}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>

              <Select value={status} onValueChange={setStatus}>
                <SelectTrigger size="sm" className="h-8 w-32">
                  <SelectValue placeholder="状态" />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value={ANY}>全部状态</SelectItem>
                  <SelectItem value="ok">只看成功</SelectItem>
                  <SelectItem value="error">只看失败</SelectItem>
                </SelectContent>
              </Select>

              <Button
                variant={onlyStream ? "default" : "outline"}
                size="sm"
                onClick={() => setOnlyStream((v) => !v)}
              >
                只看流式
              </Button>

              <FilterExprPanel value={expr} onChange={setExpr} />

              {hasFilter && (
                <Button variant="ghost" size="sm" onClick={resetFilters}>
                  <X className="size-4" />
                  清除筛选
                </Button>
              )}

              <span className="text-muted-foreground ml-auto text-xs tabular-nums">
                共 {formatNumber(total)} 条
              </span>
            </div>

            <Separator />

            {/* 时间：单独一组，不与高频条件混在一行。 */}
            <div className="flex flex-wrap items-center gap-3">
              <span className="text-muted-foreground flex items-center gap-1.5 text-xs">
                <Clock className="size-3.5" />
                时间范围
              </span>
              <Tabs value={range} onValueChange={(v) => setRange(v as RangePreset)}>
                <TabsList>
                  {(Object.keys(RANGE_LABEL) as RangePreset[]).map((r) => (
                    <TabsTrigger key={r} value={r}>
                      {RANGE_LABEL[r]}
                    </TabsTrigger>
                  ))}
                </TabsList>
              </Tabs>
            </div>
          </CardContent>
        </Card>

        <Card className="py-0">
          <CardContent className="overflow-x-auto p-0">
            {isLoading ? (
              <div className="p-4">
                <TableSkeleton rows={8} cols={8} />
              </div>
            ) : isError ? (
              <div className="space-y-2 p-4">
                <ErrorState onRetry={() => refetch()} />
                {errorMessage && (
                  <p className="text-muted-foreground text-center text-xs">
                    {errorMessage}
                  </p>
                )}
              </div>
            ) : inflightList.length === 0 && items.length === 0 ? (
              <div className="p-6">
                <EmptyState
                  icon={Activity}
                  title={hasFilter ? "没有符合条件的请求" : "暂无实时请求"}
                  description={
                    hasFilter
                      ? "换一下筛选条件，或点「清除筛选」看看全部。"
                      : "有请求经过网关时，会实时出现在这里。"
                  }
                />
              </div>
            ) : (
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>时间</TableHead>
                    <TableHead>客户端</TableHead>
                    <TableHead>模型</TableHead>
                    <TableHead>入站路径</TableHead>
                    <TableHead>上游</TableHead>
                    <TableHead>协议</TableHead>
                    <TableHead>渠道</TableHead>
                    <TableHead className="text-center">状态</TableHead>
                    <TableHead className="text-right">耗时</TableHead>
                    <TableHead className="text-right">TTFB</TableHead>
                    <TableHead className="text-right">Token</TableHead>
                    <TableHead className="text-right">费用</TableHead>
                    <TableHead className="text-center">缓存</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {/* 进行中的请求：置顶显示，带脉冲指示器。 */}
                  {inflightList.map((r) => (
                    <TableRow
                      key={r.request_id}
                      className="cursor-pointer hover:bg-emerald-500/5"
                      onClick={() => {
                        if (r.is_stream) setLiveStream(r.request_id);
                      }}
                    >
                      <TableCell className="text-muted-foreground text-xs tabular-nums whitespace-nowrap">
                        <span className="mr-1.5 inline-block size-1.5 rounded-full bg-emerald-500 align-middle animate-pulse" />
                        {formatTime(r.ts)}
                      </TableCell>
                      <TableCell className="max-w-[120px] truncate text-xs">
                        {truncate(r.client, 14)}
                      </TableCell>
                      <TableCell className="max-w-[160px] text-xs">
                        <div className="truncate font-mono">{r.model}</div>
                        {r.request_model && r.request_model !== r.model && (
                          <div
                            className="text-muted-foreground truncate text-[11px]"
                            title={`客户端请求的模型：${r.request_model}`}
                          >
                            ← {r.request_model}
                          </div>
                        )}
                      </TableCell>
                      <TableCell
                        className="text-muted-foreground max-w-[150px] truncate font-mono text-xs"
                        title={r.path}
                      >
                        {r.path || "—"}
                      </TableCell>
                      <TableCell className="text-muted-foreground text-xs">
                        {r.upstream_url ? hostOf(r.upstream_url) : "—"}
                      </TableCell>
                      <TableCell className="text-xs whitespace-nowrap">
                        {r.is_stream ? (
                          <Badge variant="success">流式</Badge>
                        ) : (
                          <Badge variant="secondary">非流式</Badge>
                        )}
                      </TableCell>
                      <TableCell
                        className="max-w-[140px] truncate text-xs"
                        title={r.provider_tag ?? ""}
                      >
                        {r.provider_name || r.provider_tag || "—"}
                      </TableCell>
                      {/* 进行中的请求没有最终状态，用「进行中」占位。 */}
                      <TableCell className="text-center">
                        <Badge variant="secondary">进行中</Badge>
                      </TableCell>
                      <TableCell className="text-right text-xs">—</TableCell>
                      <TableCell className="text-right text-xs">—</TableCell>
                      <TableCell className="text-right text-xs">—</TableCell>
                      <TableCell className="text-right text-xs">—</TableCell>
                      <TableCell className="text-center text-xs">—</TableCell>
                    </TableRow>
                  ))}

                  {/* 已完成的请求：来自数据库。 */}
                  {items.map((r) => (
                    <TableRow
                      key={r.request_id}
                      className="cursor-pointer"
                      onClick={() => setSelected(r.request_id)}
                    >
                      <TableCell className="text-muted-foreground text-xs tabular-nums whitespace-nowrap">
                        {formatTime(r.ts)}
                      </TableCell>
                      <TableCell className="max-w-[120px] truncate text-xs">
                        {truncate(r.client, 14)}
                      </TableCell>
                      <TableCell className="max-w-[160px] text-xs">
                        <div className="truncate font-mono">{r.model}</div>
                        {r.request_model && r.request_model !== r.model && (
                          <div
                            className="text-muted-foreground truncate text-[11px]"
                            title={`客户端请求的模型：${r.request_model}`}
                          >
                            ← {r.request_model}
                          </div>
                        )}
                        {r.upstream_model && r.upstream_model !== r.model && (
                          <div
                            className="text-muted-foreground truncate text-[11px]"
                            title={`实际发给上游：${r.upstream_model}`}
                          >
                            ↑ {r.upstream_model}
                          </div>
                        )}
                      </TableCell>
                      <TableCell
                        className="text-muted-foreground max-w-[150px] truncate font-mono text-xs"
                        title={r.path}
                      >
                        {r.path || "—"}
                      </TableCell>
                      <TableCell
                        className="text-muted-foreground max-w-[220px] truncate font-mono text-xs"
                        title={r.upstream_url ?? undefined}
                      >
                        {r.upstream_url ? hostOf(r.upstream_url) : "—"}
                      </TableCell>
                      <TableCell className="text-xs whitespace-nowrap">
                        {r.protocol_in === r.protocol_out ? (
                          <Badge variant="success">直通</Badge>
                        ) : (
                          <Badge variant="secondary" title={`${r.protocol_in} → ${r.protocol_out}`}>
                            转换
                          </Badge>
                        )}
                      </TableCell>
                      <TableCell
                        className="max-w-[140px] truncate"
                        title={r.provider_tag ?? ""}
                      >
                        {r.provider_name || r.provider_tag || "—"}
                      </TableCell>
                      <TableCell className="text-center">
                        <Badge variant={r.status_code < 400 ? "success" : "destructive"}>
                          {r.status_code}
                        </Badge>
                        {r.upstream_status != null &&
                          r.upstream_status !== r.status_code && (
                            <div
                              className="text-muted-foreground mt-0.5 text-[11px]"
                              title="上游返回的原始状态码"
                            >
                              上游 {r.upstream_status}
                            </div>
                          )}
                      </TableCell>
                      <TableCell className="text-right tabular-nums">
                        {formatMs(r.latency_ms)}
                      </TableCell>
                      <TableCell className="text-right tabular-nums">
                        {formatMs(r.ttfb_ms)}
                      </TableCell>
                      <TableCell className="text-right tabular-nums">
                        {formatNumber(r.input_tokens + r.output_tokens)}
                      </TableCell>
                      <TableCell className="text-right tabular-nums">
                        {quotaToUsd(r.quota)}
                      </TableCell>
                      <TableCell className="text-center">
                        {r.cache_hit ? (
                          <Badge variant="success">命中</Badge>
                        ) : (
                          <span className="text-muted-foreground text-xs">—</span>
                        )}
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            )}
          </CardContent>
        </Card>

        {items.length > 0 && limit < MAX_LIMIT && items.length >= limit && (
          <div className="flex justify-center">
            <Button
              variant="outline"
              size="sm"
              onClick={() => setLimit((v) => Math.min(v + PAGE_SIZE, MAX_LIMIT))}
            >
              加载更多
            </Button>
          </div>
        )}
      </div>

      <RequestDetailDialog
        requestId={selected}
        onOpenChange={(o) => !o && setSelected(null)}
      />

      <LiveStreamDialog
        live={openLive}
        onOpenChange={(o) => !o && setLiveStream(null)}
      />

      <Dialog open={confirmClear} onOpenChange={setConfirmClear}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>清空请求日志？</DialogTitle>
            <DialogDescription className="text-xs">
              会删除全部请求明细与捕获的请求 / 响应原文，无法撤销。
              <br />
              计费统计（用量聚合与账单）不受影响，也不会被清零。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setConfirmClear(false)}>
              取消
            </Button>
            <Button
              variant="destructive"
              onClick={() => clear.mutate()}
              disabled={clear.isPending}
            >
              {clear.isPending ? "清空中…" : "确认清空"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </PageShell>
  );
}