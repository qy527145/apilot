import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Activity, Pause, Play, RefreshCw } from "lucide-react";

import { EmptyState } from "@/components/common/EmptyState";
import { ErrorState, TableSkeleton } from "@/components/common/StatCard";
import { PageShell } from "@/components/layout/PageShell";
import { RequestDetailDialog } from "@/components/traffic/RequestDetailDialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
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

export default function TrafficPage() {
  const [selected, setSelected] = useState<string | null>(null);
  const [live, setLive] = useState(true);
  const [snapshot, setSnapshot] = useState<TrafficSnapshot | null>(null);

  const { data, isLoading, isError, refetch, isFetching } = useQuery({
    queryKey: qk.logs("traffic-live"),
    queryFn: () => api.queryLogs({ limit: 100, offset: 0 }),
    refetchInterval: live ? 1000 : false,
    retry: 1,
  });

  useApilotEvent("apilot://traffic", setSnapshot);

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
        </div>
      }
    >
      <Card className="py-0">
        <CardContent className="p-0">
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
                    <TableCell className="max-w-[200px] truncate" title={r.model}>
                      {r.model}
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
    </PageShell>
  );
}
