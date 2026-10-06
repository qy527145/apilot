import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Activity, Pause, Play, RefreshCw, Trash2 } from "lucide-react";
import { toast } from "sonner";

import { EmptyState } from "@/components/common/EmptyState";
import { ErrorState, TableSkeleton } from "@/components/common/StatCard";
import { PageShell } from "@/components/layout/PageShell";
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
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { qk } from "@/hooks/queries";
import { api } from "@/lib/api";
import { useApilotEvent, type TrafficSnapshot } from "@/lib/events";
import {
  cn,
  formatMs,
  formatNumber,
  formatTime,
  quotaToUsd,
  truncate,
} from "@/lib/utils";

const LOGS_QUERY_KEY = "traffic-live";

/** 去掉 URL 的 scheme，表格里一行放得下，完整值走 title。 */
const hostOf = (url: string) => url.replace(/^https?:\/\//, "");

export default function TrafficPage() {
  const qc = useQueryClient();
  const [selected, setSelected] = useState<string | null>(null);
  const [live, setLive] = useState(true);
  const [confirmClear, setConfirmClear] = useState(false);
  const [snapshot, setSnapshot] = useState<TrafficSnapshot | null>(null);

  const { data, isLoading, isError, refetch, isFetching } = useQuery({
    queryKey: qk.logs(LOGS_QUERY_KEY),
    queryFn: () => api.queryLogs({ limit: 100, offset: 0 }),
    refetchInterval: live ? 1000 : false,
    retry: 1,
  });

  useApilotEvent("apilot://traffic", setSnapshot);

  const clear = useMutation({
    mutationFn: api.clearLogs,
    onSuccess: (res) => {
      // 详情弹窗可能正开着一条已被删掉的记录，一起关掉免得看着像卡住。
      setSelected(null);
      qc.invalidateQueries({ queryKey: qk.logs(LOGS_QUERY_KEY) });
      toast.success(`已清空 ${res.logs} 条请求日志、${res.captures} 条原文捕获`);
      setConfirmClear(false);
    },
  });

  const items = data?.items ?? [];

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
      <Card className="py-0">
        <CardContent className="overflow-x-auto p-0">
          {isLoading ? (
            <div className="p-4">
              <TableSkeleton rows={8} cols={8} />
            </div>
          ) : isError ? (
            <div className="p-4">
              <ErrorState onRetry={() => refetch()} />
            </div>
          ) : items.length === 0 ? (
            <div className="p-6">
              <EmptyState
                icon={Activity}
                title="暂无实时请求"
                description="有请求经过网关时，会实时出现在这里。"
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
                  <TableHead className="text-center">状态码</TableHead>
                  <TableHead className="text-right">耗时</TableHead>
                  <TableHead className="text-right">TTFB</TableHead>
                  <TableHead className="text-right">Token</TableHead>
                  <TableHead className="text-right">费用</TableHead>
                  <TableHead className="text-center">缓存</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {items.map((r) => (
                  <TableRow
                    key={r.request_id}
                    className="cursor-pointer"
                    onClick={() => setSelected(r.request_id)}
                  >
                    <TableCell className="text-muted-foreground">
                      {formatTime(r.ts)}
                    </TableCell>
                    <TableCell>{truncate(r.client, 14)}</TableCell>
                    <TableCell className="max-w-[180px]">
                      <div className="truncate" title={r.model}>
                        {r.model}
                      </div>
                      {/* 映射改过名字时露出来 —— 上游说"模型不存在"多半是这里。 */}
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
                      {r.provider_tag || "—"}
                    </TableCell>
                    <TableCell className="text-center">
                      <Badge variant={r.status_code < 400 ? "success" : "destructive"}>
                        {r.status_code}
                      </Badge>
                      {/* 上游原始状态码与返回给客户端的不同时（如上游 404 → 客户端 502），
                          两个都摆出来，否则看着对不上会以为日志记错了。 */}
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

      <RequestDetailDialog
        requestId={selected}
        onOpenChange={(o) => !o && setSelected(null)}
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
