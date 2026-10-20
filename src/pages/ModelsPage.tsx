import { useEffect, useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Boxes, RefreshCw, Server, SlidersHorizontal } from "lucide-react";
import { toast } from "sonner";

import { EmptyState } from "@/components/common/EmptyState";
import { ErrorState, TableSkeleton } from "@/components/common/StatCard";
import { PageShell } from "@/components/layout/PageShell";
import type { ViewKey } from "@/components/layout/nav";
import { ModelPolicyCard } from "@/components/models/ModelPolicyCard";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import { qk } from "@/hooks/queries";
import { api, type ModelCatalogEntry } from "@/lib/api";

/**
 * 上一次自动发现上游模型的时间（模块级，跨页面挂载保留）。
 *
 * `/v1/models` 是打到外部服务商的网络请求，每次进模型页都问一遍既慢又容易
 * 撞限流。五分钟一次足够跟上"上游加了新模型"这种低频变化；急着看结果可以点
 * 卡片上的「检测上游模型」。
 */
let lastDiscoveryAt = 0;
const DISCOVERY_INTERVAL_MS = 5 * 60_000;

export default function ModelsPage({
  onNavigate,
}: {
  onNavigate: (view: ViewKey) => void;
}) {
  const qc = useQueryClient();
  const catalog = useQuery({
    queryKey: qk.modelCatalog,
    queryFn: api.listModelCatalog,
    retry: 1,
  });

  const models = catalog.data ?? [];
  /** 上游实际返回的模型名。含「通吃」渠道那些不会写进声明的名字。 */
  const [upstreamModels, setUpstreamModels] = useState<string[]>([]);
  const [discovering, setDiscovering] = useState(false);

  /**
   * 向上游确认一遍模型清单。
   *
   * 不只是"刷新下拉"：后端会把新出现的名字补进**已经声明过模型**的渠道，
   * 否则下拉里虽然列得出来，路由阶段却找不到渠道（非通吃渠道只认声明名）。
   * 只有「一个模型都没声明」的通吃渠道保持原样。
   */
  const runDiscover = async (manual: boolean) => {
    if (discovering) return;
    setDiscovering(true);
    try {
      const r = await api.discoverUpstreamModels();
      setUpstreamModels(r.available);
      const added = r.added.reduce((n, g) => n + g.models.length, 0);
      if (added > 0) {
        // 声明变了：目录、渠道面板里的模型列表、价格都跟着变。
        qc.invalidateQueries({ queryKey: qk.modelCatalog });
        qc.invalidateQueries({ queryKey: qk.providerModelsAll });
        qc.invalidateQueries({ queryKey: qk.pricing });
        toast.success(`已自动发现 ${added} 个新模型`, {
          description: r.added
            .map((g) => `${g.provider_name}：${g.models.length} 个`)
            .join("；"),
        });
      } else if (manual) {
        toast.message("上游模型没有变化");
      }
      if (manual && r.failures.length > 0) {
        toast.warning(`${r.failures.length} 个渠道没拉到模型列表`, {
          description: r.failures
            .map((f) => `${f.provider_name}：${f.error}`)
            .join("\n"),
        });
      }
    } catch (e) {
      // 自动检测失败不该打断编辑；手动点的时候要告诉用户为什么没结果。
      if (manual) {
        toast.error(e instanceof Error ? e.message : "检测上游模型失败");
      }
    } finally {
      setDiscovering(false);
    }
  };

  useEffect(() => {
    if (Date.now() - lastDiscoveryAt < DISCOVERY_INTERVAL_MS) return;
    lastDiscoveryAt = Date.now();
    void runDiscover(false);
    // 只在挂载时跑一次。runDiscover 每次渲染都是新函数，放进依赖会变成每帧一问。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** 下拉的候选：声明的模型 + 上游实际有、但没进声明的（通吃渠道）。 */
  const modelNames = useMemo(() => {
    const set = new Set<string>();
    for (const m of models) set.add(m.model);
    for (const m of upstreamModels) set.add(m);
    return [...set].sort();
  }, [models, upstreamModels]);

  return (
    <PageShell
      title="模型"
      description="决定发往上游的请求里用哪个模型名"
      actions={
        <Button variant="outline" onClick={() => onNavigate("providers")}>
          <Server className="size-4" />
          渠道管理
        </Button>
      }
    >
      {/* 内容区只有内边距、没有纵向间距，卡片要靠这层撑开。 */}
      <div className="space-y-6">
        <ModelPolicyCard models={modelNames} />

        <Card className="py-0">
          <CardHeader className="gap-2 py-3">
            <div className="flex flex-wrap items-center gap-2">
              <span className="font-medium">模型列表</span>
              {models.length > 0 && (
                <Badge variant="outline">{models.length} 个</Badge>
              )}
              <Button
                variant="outline"
                size="sm"
                className="ml-auto h-7 px-2 text-[11px]"
                disabled={discovering}
                onClick={() => void runDiscover(true)}
                title="向每个启用渠道的 /v1/models 重新确认一次"
              >
                <RefreshCw className={discovering ? "size-3.5 animate-spin" : "size-3.5"} />
                {discovering ? "检测中…" : "检测上游模型"}
              </Button>
            </div>
            <p className="text-muted-foreground text-xs">
              各渠道声明的模型取并集；进入本页时会向上游确认一次，上游新增的模型
              会自动补进声明。
              某个模型走哪个渠道不在这页配 —— 去路由页。
            </p>
          </CardHeader>

          <CardContent className="pt-0">
            {catalog.isLoading ? (
              <TableSkeleton rows={4} cols={3} />
            ) : catalog.isError ? (
              <ErrorState onRetry={() => catalog.refetch()} />
            ) : models.length === 0 ? (
              <EmptyState
                icon={Boxes}
                title="还没有模型"
                description="模型挂在渠道下面。先到「渠道管理」添加上游渠道并声明模型，再回来配置。"
                action={
                  <Button size="sm" onClick={() => onNavigate("providers")}>
                    <Server className="size-4" />
                    去渠道管理
                  </Button>
                }
              />
            ) : (
              <div className="space-y-1.5">
                {models.map((entry) => (
                  <ModelListRow
                    key={entry.model}
                    entry={entry}
                    onNavigate={() => onNavigate("routing")}
                  />
                ))}
              </div>
            )}
          </CardContent>
        </Card>
      </div>
    </PageShell>
  );
}

/** 只读一行：这个模型有哪些渠道、当前会走哪个，改的地方在路由页。 */
function ModelListRow({
  entry,
  onNavigate,
}: {
  entry: ModelCatalogEntry;
  onNavigate: () => void;
}) {
  const active = entry.candidates.find((c) => c.provider_tag === entry.primary);

  return (
    <div className="flex items-center gap-3 rounded-md border px-3 py-2 text-xs">
      <span className="min-w-0 flex-1 truncate font-mono font-medium">
        {entry.model}
      </span>

      <Badge variant="outline" className="shrink-0">
        {entry.candidates.length} 个渠道
      </Badge>

      <span className="text-muted-foreground w-40 shrink-0 truncate text-right">
        {active ? (
          <>当前走 {active.provider_name}</>
        ) : entry.policy?.strategy === "weight" ? (
          <>每次随机</>
        ) : (
          <>沿用路由规则</>
        )}
      </span>

      <Button
        variant="ghost"
        size="sm"
        className="h-7 shrink-0 px-2 text-[11px]"
        onClick={onNavigate}
      >
        <SlidersHorizontal className="size-3.5" />
        路由页调整
      </Button>
    </div>
  );
}
