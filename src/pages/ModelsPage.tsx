import { useQuery } from "@tanstack/react-query";
import { AlertTriangle, Boxes, Server } from "lucide-react";

import { EmptyState } from "@/components/common/EmptyState";
import { ErrorState, TableSkeleton } from "@/components/common/StatCard";
import { PageShell } from "@/components/layout/PageShell";
import type { ViewKey } from "@/components/layout/nav";
import { ModelMappingPanel } from "@/components/providers/ModelMappingPanel";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import { qk } from "@/hooks/queries";
import {
  ALL_PROTOCOLS,
  api,
  PROTOCOL_LABEL,
  type Protocol,
  type Provider,
} from "@/lib/api";

/**
 * 该渠道能**直通**的协议集合。
 *
 * 库里存空数组表示"只支持 kind 那一种" —— 老数据与预设的语义，
 * 必须照后端 `Provider::endpoints()` 还原。
 */
const directProtocols = (p: Provider): Protocol[] =>
  p.protocols?.length ? p.protocols.map((e) => e.protocol) : [p.kind];

/** 三种协议里，这个渠道不直通（需要 Apilot 转换）的那些。 */
const convertedProtocols = (p: Provider): Protocol[] =>
  ALL_PROTOCOLS.filter((proto) => !directProtocols(p).includes(proto));

export default function ModelsPage({
  onNavigate,
}: {
  onNavigate: (view: ViewKey) => void;
}) {
  const { data, isLoading, isError, refetch } = useQuery({
    queryKey: qk.providers,
    queryFn: api.listProviders,
    retry: 1,
  });

  const providers = data ?? [];
  // 声明过的模型数。providers.model_mapping 是「入站名 → 上游名」的派生视图，
  // 保存映射时与 provider_models 同步写入，可以直接拿来做统计。
  const declared = providers.reduce(
    (n, p) => n + Object.keys(p.model_mapping ?? {}).length,
    0,
  );
  const wildcards = providers.filter(
    (p) => Object.keys(p.model_mapping ?? {}).length === 0,
  );

  return (
    <PageShell
      title="模型"
      description="各渠道声明了哪些模型、分别能用哪种协议访问"
      actions={
        <Button variant="outline" onClick={() => onNavigate("providers")}>
          <Server className="size-4" />
          渠道管理
        </Button>
      }
    >
      {isLoading ? (
        <Card className="py-0">
          <CardContent className="p-4">
            <TableSkeleton rows={4} cols={3} />
          </CardContent>
        </Card>
      ) : isError ? (
        <Card className="py-0">
          <CardContent className="p-4">
            <ErrorState onRetry={() => refetch()} />
          </CardContent>
        </Card>
      ) : providers.length === 0 ? (
        <Card className="py-0">
          <CardContent className="p-6">
            <EmptyState
              icon={Boxes}
              title="还没有渠道"
              description="模型挂在渠道下面。先到「渠道管理」添加上游渠道，再回来声明要使用的模型。"
              action={
                <Button size="sm" onClick={() => onNavigate("providers")}>
                  <Server className="size-4" />
                  去渠道管理
                </Button>
              }
            />
          </CardContent>
        </Card>
      ) : (
        <>
          <div className="text-muted-foreground flex flex-wrap items-center gap-x-4 gap-y-1 text-xs">
            <span>
              共 {providers.length} 个渠道，已声明 {declared} 个模型
            </span>
            {wildcards.length > 0 && (
              <span className="flex items-center gap-1 text-amber-500">
                <AlertTriangle className="size-3.5" />
                {wildcards.length} 个渠道未声明模型，会接受任何模型名
              </span>
            )}
          </div>

          {providers.map((p) => {
            const models = Object.entries(p.model_mapping ?? {});
            return (
              <Card key={p.id} className="py-0">
                <CardHeader className="gap-2 py-3">
                  <div className="flex flex-wrap items-center gap-2">
                    <span className="font-medium">{p.name}</span>
                    <code className="text-muted-foreground text-xs">
                      {p.tag}
                    </code>
                    <Badge variant="outline">
                      首选 {PROTOCOL_LABEL[p.kind]}
                    </Badge>
                    <Badge variant={p.enabled ? "success" : "secondary"}>
                      {p.enabled ? "启用" : "停用"}
                    </Badge>
                    <span className="text-muted-foreground text-xs">
                      {models.length} 个模型
                    </span>
                    {!p.enabled && (
                      <span className="text-muted-foreground text-xs">
                        停用后路由不会再选它
                      </span>
                    )}
                  </div>

                  <p className="text-muted-foreground flex flex-wrap items-center gap-1.5 text-xs">
                    <Badge
                      variant="success"
                      className="font-normal"
                    >
                      直通
                    </Badge>
                    {directProtocols(p)
                      .map((proto) => PROTOCOL_LABEL[proto])
                      .join(" / ")}
                    <span className="text-border">|</span>
                    <Badge variant="secondary" className="font-normal">
                      转换
                    </Badge>
                    {convertedProtocols(p).length > 0
                      ? `${convertedProtocols(p)
                          .map((proto) => PROTOCOL_LABEL[proto])
                          .join(" / ")} 请求时由 Apilot 转换`
                      : "该渠道已声明全部三种协议，任何客户端协议都不需要转换"}
                  </p>
                </CardHeader>

                <CardContent className="pt-0">
                  {models.length === 0 && (
                    <p className="text-muted-foreground mb-3 text-xs">
                      尚未声明模型。没声明任何模型的渠道被视为「通吃」，任何模型名
                      都会原样转发给上游；声明之后，只有列出的模型会走这个渠道。
                    </p>
                  )}
                  <ModelMappingPanel providerId={p.id} providerName={p.name} />
                </CardContent>
              </Card>
            );
          })}
        </>
      )}
    </PageShell>
  );
}
