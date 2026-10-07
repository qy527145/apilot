import { useQuery } from "@tanstack/react-query";
import { Boxes, Server, SlidersHorizontal } from "lucide-react";

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

export default function ModelsPage({
  onNavigate,
}: {
  onNavigate: (view: ViewKey) => void;
}) {
  const catalog = useQuery({
    queryKey: qk.modelCatalog,
    queryFn: api.listModelCatalog,
    retry: 1,
  });

  const models = catalog.data ?? [];

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
        <ModelPolicyCard models={models.map((m) => m.model)} />

        <Card className="py-0">
          <CardHeader className="gap-2 py-3">
            <div className="flex flex-wrap items-center gap-2">
              <span className="font-medium">模型列表</span>
              {models.length > 0 && (
                <Badge variant="outline">{models.length} 个</Badge>
              )}
            </div>
            <p className="text-muted-foreground text-xs">
              各渠道声明的模型取并集，自动跟着渠道变。
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
