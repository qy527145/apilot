import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  Bar,
  BarChart,
  CartesianGrid,
  Legend,
  ResponsiveContainer,
  Tooltip as ReTooltip,
  XAxis,
  YAxis,
} from "recharts";
import { Wrench } from "lucide-react";

import { EmptyState } from "@/components/common/EmptyState";
import { ErrorState, StatCard, TableSkeleton } from "@/components/common/StatCard";
import { PageShell } from "@/components/layout/PageShell";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
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
import { api, type ToolUsageCell } from "@/lib/api";
import { RANGE_LABELS, rangeToTimeRange, type RangePreset } from "@/lib/ranges";
import { formatNumber, formatPercent } from "@/lib/utils";

const ANY = "__any__";

/**
 * 客户端的固定配色顺序。
 *
 * **按客户端名的字典序分配、不按出现次数**：排名一变就重新上色的话，用户切个
 * 时间范围之后同一个客户端换了颜色，图表就再也没法横向对比了。颜色跟着实体走。
 *
 * 只有前五个客户端有独立颜色，更多的折进「其他」—— 再生成的色相在色盲视角下
 * 和一已有的分不开，那比并成一档更糟。
 */
const CLIENT_COLORS = [
  "var(--tool-1)",
  "var(--tool-2)",
  "var(--tool-3)",
  "var(--tool-4)",
  "var(--tool-5)",
];
const MAX_CLIENTS = CLIENT_COLORS.length;
const OTHER_LABEL = "其他";

function colorFor(index: number): string {
  return CLIENT_COLORS[index % CLIENT_COLORS.length];
}

/** 「其他」那一档的颜色：中性灰，不与任何分类色抢识别。 */
const OTHER_COLOR = "var(--muted-foreground)";

/** 排名图上显示多少个工具；其余折叠成「其他」并靠表格看全。 */
const TOP_N = 12;

const TOOLTIP_STYLE = {
  background: "var(--popover)",
  border: "1px solid var(--border)",
  borderRadius: 8,
  fontSize: 12,
};

/** 数字的空值语义：算不出来给「—」，不要用 0 冒充（0 次调用与没有数据不是一回事）。 */
function perResponse(calls: number, responses: number): string {
  if (responses <= 0) return "—";
  return (calls / responses).toFixed(2);
}

/**
 * 客户端 → 颜色。按 `available_clients` 的字典序下标分配，**不按出现次数**：
 * 排名一变就重新上色的话，用户切个时间范围之后同一个客户端换了颜色，
 * 图表就再也没法横向对比了 —— 颜色跟着实体走。
 */
function buildClientColors(available: string[]): Map<string, ClientInfo> {
  const map = new Map<string, ClientInfo>();
  available.forEach((name, i) => {
    map.set(
      name,
      i < MAX_CLIENTS ? { name, color: colorFor(i) } : { name, color: OTHER_COLOR },
    );
  });
  return map;
}

/** 图的堆叠键：第 6 个及以后的客户端一律折进「其他」。 */
function makeBucketKey(available: string[]): (name: string) => string {
  return (name: string) => {
    const idx = available.indexOf(name);
    return idx >= 0 && idx < MAX_CLIENTS ? name : OTHER_LABEL;
  };
}

interface ClientInfo {
  name: string;
  color: string;
}

export default function ToolsPage() {
  const [preset, setPreset] = useState<RangePreset>("24h");
  const [client, setClient] = useState<string>(ANY);

  const range = useMemo(() => rangeToTimeRange(preset), [preset]);
  const rangeKey = `${preset}-${Math.floor(range.to / 60000)}-${client}`;

  const report = useQuery({
    queryKey: qk.toolUsage(rangeKey),
    queryFn: () => api.toolUsageReport(range, client === ANY ? null : client),
    retry: 1,
  });

  const data = report.data;

  /** 客户端 → 颜色。用**候选列表**（不受筛选影响）算下标，切筛选时颜色不变。 */
  const clientInfo = useMemo(
    () => buildClientColors(data?.available_clients ?? []),
    [data?.available_clients],
  );

  const bucketKey = useMemo(
    () => makeBucketKey(data?.available_clients ?? []),
    [data?.available_clients],
  );

  const multiClient = (data?.available_clients.length ?? 0) > 1;

  /**
   * 堆叠键 → 颜色。
   *
   * 「其他」是一个合成档，不在 `clientInfo` 里 —— 不单独处理的话它会落进
   * 兜底的 `--tool-1`，和图例里第一个真客户端撞色。
   */
  const colorOf = (key: string): string =>
    key === OTHER_LABEL
      ? OTHER_COLOR
      : (clientInfo.get(key)?.color ?? OTHER_COLOR);

  // ---- 工具排名：每个工具一根横条，按客户端堆叠 ----
  const ranking = useMemo(() => {
    const cells = data?.cells ?? [];
    const byTool = new Map<string, Record<string, number | string>>();
    for (const c of cells) {
      const key = bucketKey(c.client);
      const row = byTool.get(c.tool) ?? { tool: c.tool, total: 0 };
      row[key] = ((row[key] as number) ?? 0) + c.calls;
      row.total = (row.total as number) + c.calls;
      byTool.set(c.tool, row);
    }
    return [...byTool.values()]
      .sort((a, b) => (b.total as number) - (a.total as number))
      .slice(0, TOP_N);
  }, [bucketKey, data?.cells]);

  // ---- 趋势：每小时一根柱，按客户端堆叠 ----
  const trend = useMemo(() => {
    const series = data?.series ?? [];
    const byHour = new Map<number, Record<string, number | string>>();
    for (const p of series) {
      const key = bucketKey(p.client);
      const row = byHour.get(p.bucket_ts) ?? {
        ts: p.bucket_ts,
        label: new Date(p.bucket_ts * 1000).toLocaleTimeString("zh-CN", {
          hour: "2-digit",
          minute: "2-digit",
        }),
      };
      row[key] = ((row[key] as number) ?? 0) + p.calls;
      byHour.set(p.bucket_ts, row);
    }
    return [...byHour.entries()].sort((a, b) => a[0] - b[0]).map(([, v]) => v);
  }, [bucketKey, data?.series]);

  /** 图例与堆叠键要用到的客户端（只含真正画出来的那几个）。 */
  const drawnClients = useMemo(() => {
    const names = new Set<string>();
    for (const c of data?.cells ?? []) names.add(bucketKey(c.client));
    return [...names].sort();
  }, [bucketKey, data?.cells]);

  const empty = !report.isLoading && (data?.total_responses ?? 0) === 0;

  return (
    <PageShell
      title="工具分布"
      description="各 harness 实际调用了哪些工具。口径是响应里的调用，不是请求里的历史。"
      actions={
        <div className="flex flex-wrap items-center gap-2">
          <Select value={client} onValueChange={setClient}>
            <SelectTrigger size="sm" className="h-8 w-40">
              <SelectValue placeholder="客户端" />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ANY}>全部客户端</SelectItem>
              {(data?.available_clients ?? []).map((c) => (
                <SelectItem key={c} value={c}>
                  {c}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Tabs value={preset} onValueChange={(v) => setPreset(v as RangePreset)}>
            <TabsList>
              {(Object.keys(RANGE_LABELS) as RangePreset[]).map((p) => (
                <TabsTrigger key={p} value={p}>
                  {RANGE_LABELS[p]}
                </TabsTrigger>
              ))}
            </TabsList>
          </Tabs>
        </div>
      }
    >
      <div className="space-y-6">
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-4">
          <StatCard
            label="工具调用总数"
            value={formatNumber(data?.total_calls)}
            loading={report.isLoading}
          />
          <StatCard
            label="用到的工具种类"
            value={formatNumber(data?.tool_count)}
            loading={report.isLoading}
            hint="不同工具名去重，按客户端分别计"
          />
          <StatCard
            label="每次响应平均调用"
            value={perResponse(data?.total_calls ?? 0, data?.total_responses ?? 0)}
            loading={report.isLoading}
            hint="分母是解出过内容的响应条数"
          />
          <StatCard
            label="动手的响应占比"
            value={formatPercent(
              data && data.total_responses > 0
                ? data.tool_responses / data.total_responses
                : null,
            )}
            loading={report.isLoading}
            hint="带工具调用的响应 / 全部响应"
          />
        </div>

        {report.isError ? (
          <ErrorState onRetry={() => report.refetch()} />
        ) : report.isLoading ? (
          <TableSkeleton rows={6} cols={4} />
        ) : empty ? (
          <EmptyState
            icon={Wrench}
            title="该时间范围内没有可统计的响应"
            description="工具统计只数得出内容的响应。改时间范围，或者等网关产生一次新的请求 —— 上线前的历史只能从还留着的捕获里回填一部分。"
          />
        ) : (
          <>
            <div className="grid grid-cols-1 gap-6 xl:grid-cols-2">
              <Card>
                <CardHeader>
                  <CardTitle className="text-sm">调用次数排名</CardTitle>
                  <p className="text-muted-foreground text-xs">
                    取前 {TOP_N} 个工具。同名工具在不同 harness 下不合并。
                  </p>
                </CardHeader>
                <CardContent>
                  <div style={{ height: Math.max(200, ranking.length * 28 + 40) }}>
                    <ResponsiveContainer width="100%" height="100%">
                      <BarChart
                        data={ranking}
                        layout="vertical"
                        margin={{ left: 8, right: 24, top: 4, bottom: 4 }}
                        barCategoryGap={6}
                      >
                        <CartesianGrid
                          strokeDasharray="0"
                          stroke="var(--border)"
                          horizontal={false}
                        />
                        <XAxis
                          type="number"
                          tick={{ fontSize: 11 }}
                          stroke="var(--muted-foreground)"
                          allowDecimals={false}
                        />
                        <YAxis
                          type="category"
                          dataKey="tool"
                          width={130}
                          tick={{ fontSize: 11 }}
                          stroke="var(--muted-foreground)"
                        />
                        <ReTooltip
                          cursor={{ fill: "var(--muted)", opacity: 0.4 }}
                          contentStyle={TOOLTIP_STYLE}
                        />
                        {multiClient && <Legend wrapperStyle={{ fontSize: 12 }} />}
                        {multiClient ? (
                          drawnClients.map((name) => (
                            <Bar
                              key={name}
                              dataKey={name}
                              name={name}
                              stackId="calls"
                              fill={colorOf(name)}
                              radius={[0, 4, 4, 0]}
                              maxBarSize={18}
                            />
                          ))
                        ) : (
                          // 只有一个客户端时按排名上色就够了 —— 单系列不需要图例，
                          // 标题已经说明了画的是什么。
                          <Bar
                            dataKey="total"
                            name="调用次数"
                            fill="var(--tool-1)"
                            radius={[0, 4, 4, 0]}
                            maxBarSize={18}
                          />
                        )}
                      </BarChart>
                    </ResponsiveContainer>
                  </div>
                </CardContent>
              </Card>

              <Card>
                <CardHeader>
                  <CardTitle className="text-sm">按小时趋势</CardTitle>
                  <p className="text-muted-foreground text-xs">
                    每小时的工具调用次数，按客户端堆叠。
                  </p>
                </CardHeader>
                <CardContent>
                  <div className="h-[260px]">
                    <ResponsiveContainer width="100%" height="100%">
                      <BarChart
                        data={trend}
                        margin={{ left: 4, right: 12, top: 4, bottom: 4 }}
                        barCategoryGap={2}
                      >
                        <CartesianGrid
                          strokeDasharray="0"
                          stroke="var(--border)"
                          vertical={false}
                        />
                        <XAxis
                          dataKey="label"
                          tick={{ fontSize: 11 }}
                          stroke="var(--muted-foreground)"
                          interval="preserveStartEnd"
                          minTickGap={24}
                        />
                        <YAxis
                          tick={{ fontSize: 11 }}
                          stroke="var(--muted-foreground)"
                          width={44}
                          allowDecimals={false}
                        />
                        <ReTooltip
                          cursor={{ fill: "var(--muted)", opacity: 0.4 }}
                          contentStyle={TOOLTIP_STYLE}
                        />
                        {multiClient && <Legend wrapperStyle={{ fontSize: 12 }} />}
                        {drawnClients.map((name) => (
                          // 2px 表面色描边就是规范里的「间隙」：堆叠段之间靠底色
                          // 分开，不靠给每段描一圈有颜色的边。
                          <Bar
                            key={name}
                            dataKey={name}
                            name={name}
                            stackId="calls"
                            fill={colorOf(name)}
                            stroke="var(--card)"
                            strokeWidth={1}
                            maxBarSize={24}
                          />
                        ))}
                      </BarChart>
                    </ResponsiveContainer>
                  </div>
                </CardContent>
              </Card>
            </div>

            <Card>
              <CardHeader>
                <CardTitle className="text-sm">全部工具</CardTitle>
                <p className="text-muted-foreground text-xs">
                  「用过它的响应」与「调用次数」分开看：前者低而后者高，说明这个工具
                  一用就连着用好几次（并行读文件就是这样）。
                </p>
              </CardHeader>
              <CardContent>
                <div className="overflow-x-auto rounded-md border">
                  <Table>
                    <TableHeader>
                      <TableRow>
                        <TableHead>工具</TableHead>
                        <TableHead>客户端</TableHead>
                        <TableHead className="text-right">调用次数</TableHead>
                        <TableHead
                          className="text-right"
                          title="至少用过这个工具一次的响应条数。"
                        >
                          用过它的响应
                        </TableHead>
                        <TableHead
                          className="text-right"
                          title="调用次数 / 用过它的响应数。大于 1 说明一次响应里会连续调用多次。"
                        >
                          单条响应内平均
                        </TableHead>
                        <TableHead className="text-right">占全部调用</TableHead>
                      </TableRow>
                    </TableHeader>
                    <TableBody>
                      {(data?.cells ?? []).map((c) => (
                        <TableRow key={`${c.client}::${c.tool}`}>
                          <TableCell className="font-medium">
                            {c.tool}
                          </TableCell>
                          <TableCell>
                            <span className="inline-flex items-center gap-1.5">
                              <span
                                className="size-2 rounded-full"
                                style={{
                                  background:
                                    clientInfo.get(c.client)?.color ??
                                    "var(--muted-foreground)",
                                }}
                              />
                              {c.client}
                            </span>
                          </TableCell>
                          <TableCell className="text-right tabular-nums">
                            {formatNumber(c.calls)}
                          </TableCell>
                          <TableCell className="text-right tabular-nums">
                            {formatNumber(c.requests)}
                          </TableCell>
                          <TableCell className="text-right tabular-nums">
                            {perResponse(c.calls, c.requests)}
                          </TableCell>
                          <TableCell className="text-right tabular-nums">
                            {share(c, data?.total_calls ?? 0)}
                          </TableCell>
                        </TableRow>
                      ))}
                    </TableBody>
                  </Table>
                </div>
              </CardContent>
            </Card>

            {(data?.clients ?? []).length > 0 && (
              <Card>
                <CardHeader>
                  <CardTitle className="text-sm">按客户端</CardTitle>
                </CardHeader>
                <CardContent>
                  <div className="overflow-x-auto rounded-md border">
                    <Table>
                      <TableHeader>
                        <TableRow>
                          <TableHead>客户端</TableHead>
                          <TableHead className="text-right">调用次数</TableHead>
                          <TableHead className="text-right">响应条数</TableHead>
                          <TableHead className="text-right">
                            带工具的响应
                          </TableHead>
                          <TableHead className="text-right">
                            每次响应平均调用
                          </TableHead>
                        </TableRow>
                      </TableHeader>
                      <TableBody>
                        {(data?.clients ?? []).map((c) => (
                          <TableRow key={c.client}>
                            <TableCell className="font-medium">
                              <span className="inline-flex items-center gap-1.5">
                                <span
                                  className="size-2 rounded-full"
                                  style={{
                                    background:
                                      clientInfo.get(c.client)?.color ??
                                      "var(--muted-foreground)",
                                  }}
                                />
                                {c.client}
                              </span>
                            </TableCell>
                            <TableCell className="text-right tabular-nums">
                              {formatNumber(c.calls)}
                            </TableCell>
                            <TableCell className="text-right tabular-nums">
                              {formatNumber(c.responses)}
                            </TableCell>
                            <TableCell className="text-right tabular-nums">
                              {formatNumber(c.tool_responses)}
                            </TableCell>
                            <TableCell className="text-right tabular-nums">
                              {perResponse(c.calls, c.responses)}
                            </TableCell>
                          </TableRow>
                        ))}
                      </TableBody>
                    </Table>
                  </div>
                </CardContent>
              </Card>
            )}
          </>
        )}
      </div>
    </PageShell>
  );
}

function share(c: ToolUsageCell, total: number): string {
  if (total <= 0) return "—";
  return formatPercent(c.calls / total);
}