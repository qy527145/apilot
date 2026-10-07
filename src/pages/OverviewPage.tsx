import { useMemo } from "react";
import {
  Area,
  AreaChart,
  CartesianGrid,
  ResponsiveContainer,
  Tooltip as ReTooltip,
  XAxis,
  YAxis,
} from "recharts";
import { Activity, CircleDollarSign, Gauge, Timer } from "lucide-react";

import { PageShell } from "@/components/layout/PageShell";
import type { ViewKey } from "@/components/layout/nav";
import { StatCard, TableSkeleton, ErrorState } from "@/components/common/StatCard";
import { EmptyState } from "@/components/common/EmptyState";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { useGatewayStatus, qk } from "@/hooks/queries";
import { api } from "@/lib/api";
import { todayRange } from "@/lib/ranges";
import {
  formatMs,
  formatNumber,
  formatPercent,
  formatTime,
  quotaToUsd,
  truncate,
} from "@/lib/utils";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";

interface Props {
  onNavigate: (view: ViewKey) => void;
}

export default function OverviewPage({ onNavigate }: Props) {
  const qc = useQueryClient();
  const range = useMemo(() => todayRange(), []);
  const rangeKey = `today-${range.from}`;

  const { data: gateway, isLoading: gwLoading } = useGatewayStatus();
  const totals = useQuery({
    queryKey: qk.billingTotals(rangeKey),
    queryFn: () => api.billingTotals(range),
    retry: 1,
  });
  const series = useQuery({
    queryKey: qk.billingSeries("overview-24h"),
    queryFn: () => api.billingTimeseries({ from: Date.now() - 86_400_000, to: Date.now() }),
    refetchInterval: 30_000,
    retry: 1,
  });
  const recent = useQuery({
    queryKey: qk.logs("overview-recent"),
    queryFn: () => api.queryLogs({ limit: 10, offset: 0 }),
    refetchInterval: 5_000,
    retry: 1,
  });

  const toggle = useMutation({
    // 参数是开关**将要变成**的状态（`onCheckedChange` 给的是新值，不是旧值）。
    // 之前写成 `running ? stop() : start()` 就正好反了：开着拨下去调的是 start
    // （已在运行会原样返回），关着拨上去调的是 stop（没运行是空操作），
    // 结果两头都没反应。
    mutationFn: (nextRunning: boolean) =>
      nextRunning ? api.gatewayStart() : api.gatewayStop(),
    onSuccess: (status) => {
      qc.setQueryData(qk.gateway, status);
      toast.success(status.running ? "网关已启动" : "网关已停止");
    },
    // 失败不用在这里兜：`api.call` 已经统一 toast 了，再加一个会弹两条。
  });

  const chartData = useMemo(
    () =>
      (series.data ?? []).map((p) => ({
        // 后端给的 bucket_ts 是 unix **秒**（usage_hourly.bucket_ts），
        // `new Date` 要的是毫秒 —— 不乘这一千，横轴会显示成 1970 年的小时。
        label: formatTime(p.bucket_ts * 1000).slice(6),
        requests: p.requests,
        usd: p.quota / 500000,
      })),
    [series.data],
  );

  return (
    <PageShell
      title="概览"
      description="网关运行状态与今日关键指标"
      actions={
        <div className="flex items-center gap-3 rounded-md border px-3 py-1.5">
          <span className="text-muted-foreground text-xs">网关</span>
          {gwLoading ? (
            <Skeleton className="h-4 w-20" />
          ) : (
            <>
              <Badge variant={gateway?.running ? "success" : "secondary"}>
                {gateway?.running ? "运行中" : "已停止"}
              </Badge>
              <Switch
                checked={!!gateway?.running}
                disabled={toggle.isPending}
                onCheckedChange={(v) => toggle.mutate(v)}
              />
            </>
          )}
        </div>
      }
    >
      <div className="space-y-6">
        {gateway?.error && (
          <div className="border-destructive/40 bg-destructive/10 text-destructive rounded-md border px-4 py-2 text-xs">
            网关错误：{gateway.error}
          </div>
        )}

        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-4">
          <StatCard
            label="今日请求数"
            value={formatNumber(totals.data?.requests)}
            icon={<Activity className="size-4" />}
            loading={totals.isLoading}
            hint={
              // 停着的时候 `stop()` 会把端口清成 0，直接拼会显示成「监听 …:0」。
              gateway?.running
                ? `监听 ${gateway.host}:${gateway.port}`
                : undefined
            }
          />
          <StatCard
            label="今日费用"
            value={quotaToUsd(totals.data?.quota)}
            icon={<CircleDollarSign className="size-4" />}
            loading={totals.isLoading}
            hint={`缓存节省 ${quotaToUsd(totals.data?.cache_saved_quota)}`}
          />
          <StatCard
            label="缓存命中率"
            value={formatPercent(totals.data?.cache_hit_rate)}
            icon={<Gauge className="size-4" />}
            loading={totals.isLoading}
          />
          <StatCard
            label="P50 TTFB"
            value={formatMs(totals.data?.p50_ttfb_ms)}
            icon={<Timer className="size-4" />}
            loading={totals.isLoading}
          />
        </div>

        <Card>
          <CardHeader>
            <CardTitle className="text-sm">24 小时请求 / 费用趋势</CardTitle>
          </CardHeader>
          <CardContent>
            {series.isLoading ? (
              <Skeleton className="h-[260px] w-full" />
            ) : chartData.length === 0 ? (
              <EmptyState
                title="暂无趋势数据"
                description="网关收到请求后，这里会显示 24 小时的请求量与费用走势。"
              />
            ) : (
              <div className="h-[260px] w-full">
                <ResponsiveContainer width="100%" height="100%">
                  <AreaChart data={chartData} margin={{ left: 4, right: 12, top: 8 }}>
                    <defs>
                      <linearGradient id="gReq" x1="0" y1="0" x2="0" y2="1">
                        <stop offset="5%" stopColor="var(--chart-1)" stopOpacity={0.7} />
                        <stop offset="95%" stopColor="var(--chart-1)" stopOpacity={0.05} />
                      </linearGradient>
                      <linearGradient id="gUsd" x1="0" y1="0" x2="0" y2="1">
                        <stop offset="5%" stopColor="var(--chart-2)" stopOpacity={0.7} />
                        <stop offset="95%" stopColor="var(--chart-2)" stopOpacity={0.05} />
                      </linearGradient>
                    </defs>
                    <CartesianGrid strokeDasharray="3 3" stroke="var(--border)" vertical={false} />
                    <XAxis dataKey="label" tick={{ fontSize: 11 }} stroke="var(--muted-foreground)" />
                    <YAxis yAxisId="left" tick={{ fontSize: 11 }} stroke="var(--muted-foreground)" width={40} />
                    <YAxis yAxisId="right" orientation="right" tick={{ fontSize: 11 }} stroke="var(--muted-foreground)" width={52} />
                    <ReTooltip
                      contentStyle={{
                        background: "var(--popover)",
                        border: "1px solid var(--border)",
                        borderRadius: 8,
                        fontSize: 12,
                      }}
                    />
                    <Area
                      yAxisId="left"
                      type="monotone"
                      dataKey="requests"
                      name="请求数"
                      stroke="var(--chart-1)"
                      fill="url(#gReq)"
                      strokeWidth={2}
                    />
                    <Area
                      yAxisId="right"
                      type="monotone"
                      dataKey="usd"
                      name="费用(USD)"
                      stroke="var(--chart-2)"
                      fill="url(#gUsd)"
                      strokeWidth={2}
                    />
                  </AreaChart>
                </ResponsiveContainer>
              </div>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader className="flex-row items-center justify-between">
            <CardTitle className="text-sm">最近请求</CardTitle>
            <Button variant="ghost" size="sm" onClick={() => onNavigate("traffic")}>
              查看全部
            </Button>
          </CardHeader>
          <CardContent>
            {recent.isLoading ? (
              <TableSkeleton rows={5} cols={5} />
            ) : recent.isError ? (
              <ErrorState onRetry={() => recent.refetch()} />
            ) : (recent.data?.items.length ?? 0) === 0 ? (
              <EmptyState
                title="还没有请求记录"
                description="启动网关并把客户端指过来后，请求会实时出现在这里。"
              />
            ) : (
              <div className="overflow-x-auto">
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead>时间</TableHead>
                      <TableHead>客户端</TableHead>
                      <TableHead>模型</TableHead>
                      <TableHead>渠道</TableHead>
                      <TableHead>状态</TableHead>
                      <TableHead className="text-right">耗时</TableHead>
                      <TableHead className="text-right">费用</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {recent.data?.items.map((r) => (
                      <TableRow key={r.request_id}>
                        <TableCell className="text-muted-foreground">{formatTime(r.ts)}</TableCell>
                        <TableCell>{truncate(r.client, 16)}</TableCell>
                        <TableCell className="max-w-[180px] truncate">{r.model}</TableCell>
                        <TableCell>{truncate(r.provider_tag, 18)}</TableCell>
                        <TableCell>
                          <Badge variant={r.status_code < 400 ? "success" : "destructive"}>
                            {r.status_code}
                          </Badge>
                        </TableCell>
                        <TableCell className="text-right tabular-nums">
                          {formatMs(r.latency_ms)}
                        </TableCell>
                        <TableCell className="text-right tabular-nums">
                          {quotaToUsd(r.quota)}
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </div>
            )}
          </CardContent>
        </Card>
      </div>
    </PageShell>
  );
}
