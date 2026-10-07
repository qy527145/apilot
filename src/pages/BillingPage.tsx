import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  Bar,
  BarChart,
  CartesianGrid,
  Cell,
  Legend,
  Pie,
  PieChart,
  ResponsiveContainer,
  Tooltip as ReTooltip,
  XAxis,
  YAxis,
} from "recharts";

import { EmptyState } from "@/components/common/EmptyState";
import { ErrorState, StatCard, TableSkeleton } from "@/components/common/StatCard";
import { PricingTable } from "@/components/billing/PricingTable";
import { PageShell } from "@/components/layout/PageShell";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
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
import { api, type GroupBy } from "@/lib/api";
import { RANGE_LABELS, rangeToTimeRange, type RangePreset } from "@/lib/ranges";
import { formatMs, formatNumber, formatPercent, quotaToUsd } from "@/lib/utils";

const CHART_COLORS = [
  "var(--chart-1)",
  "var(--chart-2)",
  "var(--chart-3)",
  "var(--chart-4)",
  "var(--chart-5)",
];

const GROUP_LABELS: Record<GroupBy, string> = {
  client: "按客户端",
  model: "按模型",
  provider: "按渠道",
};

export default function BillingPage() {
  const [preset, setPreset] = useState<RangePreset>("24h");
  const [groupBy, setGroupBy] = useState<GroupBy>("model");

  const range = useMemo(() => rangeToTimeRange(preset), [preset]);
  const rangeKey = `${preset}-${Math.floor(range.to / 60000)}`;

  const totals = useQuery({
    queryKey: qk.billingTotals(rangeKey),
    queryFn: () => api.billingTotals(range),
    retry: 1,
  });

  const buckets = useQuery({
    queryKey: qk.billingSummary(`${rangeKey}-${groupBy}`),
    queryFn: () => api.billingSummary(range, groupBy),
    retry: 1,
  });

  const bucketData = buckets.data ?? [];
  const pieData = bucketData
    .filter((b) => b.quota > 0)
    .map((b) => ({ name: b.key || "(未知)", value: b.quota / 500000 }));
  const barData = bucketData.map((b) => ({
    name: b.key || "(未知)",
    input: b.input_tokens,
    output: b.output_tokens,
    cache: b.cache_read_tokens,
  }));

  return (
    <PageShell
      title="统计"
      description="用量聚合与单价系数配置"
      actions={
        <Tabs value={preset} onValueChange={(v) => setPreset(v as RangePreset)}>
          <TabsList>
            {(Object.keys(RANGE_LABELS) as RangePreset[]).map((p) => (
              <TabsTrigger key={p} value={p}>
                {RANGE_LABELS[p]}
              </TabsTrigger>
            ))}
          </TabsList>
        </Tabs>
      }
    >
      <div className="space-y-6">
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-4">
          <StatCard
            label="请求数"
            value={formatNumber(totals.data?.requests)}
            loading={totals.isLoading}
          />
          <StatCard
            label="总费用"
            value={quotaToUsd(totals.data?.quota)}
            loading={totals.isLoading}
            hint={`缓存节省 ${quotaToUsd(totals.data?.cache_saved_quota)}`}
          />
          <StatCard
            label="Token 总量"
            value={formatNumber(
              (totals.data?.input_tokens ?? 0) + (totals.data?.output_tokens ?? 0),
            )}
            loading={totals.isLoading}
          />
          <StatCard
            label="缓存命中率 / P50 TTFB"
            value={`${formatPercent(totals.data?.prompt_cache_hit_rate)} · ${formatMs(
              totals.data?.p50_ttfb_ms,
            )}`}
            loading={totals.isLoading}
            hint="上游提示缓存（缓存读 / 全部输入）"
          />
        </div>

        <Card>
          <CardHeader className="flex-row flex-wrap items-center justify-between gap-2">
            <CardTitle className="text-sm">聚合维度</CardTitle>
            <Tabs
              value={groupBy}
              onValueChange={(v) => setGroupBy(v as GroupBy)}
            >
              <TabsList>
                {(Object.keys(GROUP_LABELS) as GroupBy[]).map((g) => (
                  <TabsTrigger key={g} value={g}>
                    {GROUP_LABELS[g]}
                  </TabsTrigger>
                ))}
              </TabsList>
            </Tabs>
          </CardHeader>
          <CardContent className="space-y-6">
            {buckets.isLoading ? (
              <TableSkeleton rows={5} cols={8} />
            ) : buckets.isError ? (
              <ErrorState onRetry={() => buckets.refetch()} />
            ) : bucketData.length === 0 ? (
              <EmptyState
                title="该时间范围内暂无用量"
                description="切换时间范围，或等待网关产生请求。"
              />
            ) : (
              <>
                <div className="grid grid-cols-1 gap-6 lg:grid-cols-2">
                  <div className="space-y-2">
                    <p className="text-muted-foreground text-xs">费用占比</p>
                    <div className="h-[260px]">
                      <ResponsiveContainer width="100%" height="100%">
                        <PieChart>
                          <Pie
                            data={pieData}
                            dataKey="value"
                            nameKey="name"
                            innerRadius={50}
                            outerRadius={90}
                            paddingAngle={2}
                          >
                            {pieData.map((_, i) => (
                              <Cell
                                key={i}
                                fill={CHART_COLORS[i % CHART_COLORS.length]}
                              />
                            ))}
                          </Pie>
                          <ReTooltip
                            formatter={(v: unknown) => `$${Number(v ?? 0).toFixed(4)}`}
                            contentStyle={{
                              background: "var(--popover)",
                              border: "1px solid var(--border)",
                              borderRadius: 8,
                              fontSize: 12,
                            }}
                          />
                          <Legend wrapperStyle={{ fontSize: 12 }} />
                        </PieChart>
                      </ResponsiveContainer>
                    </div>
                  </div>

                  <div className="space-y-2">
                    <p className="text-muted-foreground text-xs">Token 对比</p>
                    <div className="h-[260px]">
                      <ResponsiveContainer width="100%" height="100%">
                        <BarChart data={barData} margin={{ left: 4, right: 12 }}>
                          <CartesianGrid
                            strokeDasharray="3 3"
                            stroke="var(--border)"
                            vertical={false}
                          />
                          <XAxis
                            dataKey="name"
                            tick={{ fontSize: 11 }}
                            stroke="var(--muted-foreground)"
                          />
                          <YAxis
                            tick={{ fontSize: 11 }}
                            stroke="var(--muted-foreground)"
                            width={52}
                          />
                          <ReTooltip
                            contentStyle={{
                              background: "var(--popover)",
                              border: "1px solid var(--border)",
                              borderRadius: 8,
                              fontSize: 12,
                            }}
                          />
                          <Legend wrapperStyle={{ fontSize: 12 }} />
                          <Bar
                            dataKey="input"
                            name="输入"
                            stackId="t"
                            fill="var(--chart-1)"
                          />
                          <Bar
                            dataKey="output"
                            name="输出"
                            stackId="t"
                            fill="var(--chart-2)"
                          />
                          <Bar
                            dataKey="cache"
                            name="缓存读"
                            stackId="t"
                            fill="var(--chart-3)"
                          />
                        </BarChart>
                      </ResponsiveContainer>
                    </div>
                  </div>
                </div>

                <div className="overflow-x-auto rounded-md border">
                  <Table>
                    <TableHeader>
                      <TableRow>
                        <TableHead>{GROUP_LABELS[groupBy]}</TableHead>
                        <TableHead className="text-right">请求数</TableHead>
                        <TableHead className="text-right">输入 Token</TableHead>
                        <TableHead className="text-right">输出 Token</TableHead>
                        <TableHead className="text-right">缓存读</TableHead>
                        <TableHead className="text-right">缓存命中率</TableHead>
                        <TableHead className="text-right">本地缓存命中</TableHead>
                        <TableHead className="text-right">费用</TableHead>
                      </TableRow>
                    </TableHeader>
                    <TableBody>
                      {bucketData.map((b) => (
                        <TableRow key={b.key}>
                          <TableCell className="font-medium">
                            <div className="truncate">{b.key || "(未知)"}</div>
                            {/*
                              把被折叠进来的客户端模型名露出来。聚合按**生效模型**记
                              （计费口径不能动），所以只看这一列永远答不出"我发的
                              gpt-6-sol 怎么跑到 deepseek-flash 这行来了"。
                              与监控页的模型列用同一套写法（← / 小字 / 静音色）。
                            */}
                            {b.request_models.map((a) => (
                              <div
                                key={a.model}
                                className="text-muted-foreground truncate text-[11px] font-normal"
                                title={`客户端请求的模型：${a.model}`}
                              >
                                ← {a.model}（{formatNumber(a.requests)}）
                              </div>
                            ))}
                          </TableCell>
                          <TableCell className="text-right tabular-nums">
                            {formatNumber(b.requests)}
                          </TableCell>
                          <TableCell className="text-right tabular-nums">
                            {formatNumber(b.input_tokens)}
                          </TableCell>
                          <TableCell className="text-right tabular-nums">
                            {formatNumber(b.output_tokens)}
                          </TableCell>
                          <TableCell className="text-right tabular-nums">
                            {formatNumber(b.cache_read_tokens)}
                          </TableCell>
                          <TableCell className="text-right tabular-nums">
                            {formatPercent(b.prompt_cache_hit_rate)}
                          </TableCell>
                          <TableCell className="text-right tabular-nums">
                            {formatNumber(b.cache_hits)}
                          </TableCell>
                          <TableCell className="text-right tabular-nums">
                            {quotaToUsd(b.quota)}
                          </TableCell>
                        </TableRow>
                      ))}
                    </TableBody>
                  </Table>
                </div>
              </>
            )}
          </CardContent>
        </Card>

        <PricingTable />
      </div>
    </PageShell>
  );
}
