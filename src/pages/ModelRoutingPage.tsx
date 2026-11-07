import { useEffect, useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Boxes, RefreshCw, Server } from "lucide-react";
import { toast } from "sonner";

import { EmptyState } from "@/components/common/EmptyState";
import { ErrorState, TableSkeleton } from "@/components/common/StatCard";
import { PageShell } from "@/components/layout/PageShell";
import type { ViewKey } from "@/components/layout/nav";
import { ModelChannelPicker } from "@/components/models/ModelChannelPicker";
import { ModelPolicyCard } from "@/components/models/ModelPolicyCard";
import { RouteSimulator } from "@/components/routing/RouteSimulator";
import { RuleEditor } from "@/components/routing/RuleEditor";
import { RuleList } from "@/components/routing/RuleList";
import { SelectorPanel } from "@/components/routing/SelectorPanel";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { qk } from "@/hooks/queries";
import { api, type RouteRule } from "@/lib/api";
import { useApilotEvent } from "@/lib/events";

/**
 * 上一次自动发现上游模型的时间（模块级，跨页面挂载保留）。
 *
 * `/v1/models` 是打到外部服务商的网络请求，每次进这一页都问一遍既慢又容易
 * 撞限流。五分钟一次足够跟上"上游加了新模型"这种低频变化；急着看结果可以点
 * 卡片上的「检测上游模型」。
 */
let lastDiscoveryAt = 0;
const DISCOVERY_INTERVAL_MS = 5 * 60_000;

/**
 * 「模型路由」：一条请求最终落到哪个渠道的哪个模型。
 *
 * 分成三层，按请求经过它们的顺序排列：
 *
 * 1. **模型** —— 客户端发来的模型名要不要换成别的（全局 + 按客户端）；
 * 2. **渠道** —— 同一个模型在多个渠道都有时走哪个（按列表顺序 + 可钉住一个）；
 * 3. **规则** —— 前面的规则链、selector 热切换，以及用来验证的规则测试。
 *
 * 原来这是「模型」与「路由」两页，但用户要回答的问题只有一个："这条请求会打到哪"。
 * 拆成两页之后，改模型名要在这页、改渠道要去那页、验证又要回另一页。
 */
export default function ModelRoutingPage({
  onNavigate,
}: {
  onNavigate: (view: ViewKey) => void;
}) {
  const qc = useQueryClient();
  const [editorOpen, setEditorOpen] = useState(false);
  const [editing, setEditing] = useState<RouteRule | null>(null);
  const [discovering, setDiscovering] = useState(false);
  const [upstreamModels, setUpstreamModels] = useState<string[]>([]);

  const catalog = useQuery({
    queryKey: qk.modelCatalog,
    queryFn: api.listModelCatalog,
    retry: 1,
  });
  const providers = useQuery({
    queryKey: qk.providers,
    queryFn: api.listProviders,
    retry: 1,
  });
  const selectors = useQuery({
    queryKey: qk.selectors,
    queryFn: api.listSelectors,
    retry: 1,
  });
  const rules = useQuery({
    queryKey: qk.routeRules,
    queryFn: api.listRouteRules,
    retry: 1,
  });

  // 后端自动切换选择器时给出提示
  useApilotEvent("apilot://selector-changed", (payload) => {
    toast.message(`选择器 ${payload.selector} 已切换到 ${payload.provider_tag}`, {
      description: payload.reason,
    });
  });

  const models = catalog.data ?? [];

  /** 下拉的候选：声明的模型 + 上游实际有、但没进声明的（通吃渠道）。 */
  const modelNames = useMemo(() => {
    const set = new Set<string>();
    for (const m of models) set.add(m.model);
    for (const m of upstreamModels) set.add(m);
    return [...set].sort();
  }, [models, upstreamModels]);

  const invalidateModelViews = () => {
    qc.invalidateQueries({ queryKey: qk.modelCatalog });
    // 换渠道会直接影响选择器的热切换结果，一并刷新。
    qc.invalidateQueries({ queryKey: qk.selectors });
    // 上游名和渠道页展开的那份列表同源（都是 provider_models），
    // 改完不刷新的话切回渠道页看到的还是旧名字。
    qc.invalidateQueries({ queryKey: qk.providerModelsAll });
  };

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
        invalidateModelViews();
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

  return (
    <PageShell
      title="模型路由"
      description="一条请求最终落到哪个渠道的哪个模型"
      actions={
        <Button variant="outline" onClick={() => onNavigate("providers")}>
          <Server className="size-4" />
          渠道管理
        </Button>
      }
    >
      <Tabs defaultValue="model" className="gap-4">
        <TabsList>
          <TabsTrigger value="model">模型与渠道</TabsTrigger>
          <TabsTrigger value="rules">规则与选择器</TabsTrigger>
          <TabsTrigger value="test">规则测试</TabsTrigger>
        </TabsList>

        <TabsContent value="model" className="space-y-6">
          <ModelPolicyCard models={modelNames} />

          <Card className="py-0">
            <CardHeader className="gap-2 py-3">
              <div className="flex flex-wrap items-center gap-2">
                <span className="font-medium">模型的渠道选择</span>
                {models.length > 0 && (
                  <Badge variant="outline">{models.length} 个模型</Badge>
                )}
                <Button
                  variant="outline"
                  size="sm"
                  className="ml-auto h-7 px-2 text-[11px]"
                  disabled={discovering}
                  onClick={() => void runDiscover(true)}
                  title="向每个启用渠道的 /v1/models 重新确认一次"
                >
                  <RefreshCw
                    className={discovering ? "size-3.5 animate-spin" : "size-3.5"}
                  />
                  {discovering ? "检测中…" : "检测上游模型"}
                </Button>
              </div>
              <p className="text-muted-foreground text-xs">
                各渠道声明的模型取并集；进入本页时会向上游确认一次，上游新增的模型
                会自动补进声明。顺序即优先级，点行可钉住某个渠道。
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
                <ModelChannelPicker models={models} onInvalidate={invalidateModelViews} />
              )}
            </CardContent>
          </Card>
        </TabsContent>

        <TabsContent value="rules" className="space-y-6">
          <SelectorPanel
            selectors={selectors.data ?? []}
            providers={providers.data ?? []}
            isLoading={selectors.isLoading || providers.isLoading}
            isError={selectors.isError}
            onRetry={() => selectors.refetch()}
          />

          <RuleList
            rules={rules.data ?? []}
            isLoading={rules.isLoading}
            isError={rules.isError}
            onRetry={() => rules.refetch()}
            onCreate={() => {
              setEditing(null);
              setEditorOpen(true);
            }}
            onEdit={(rule) => {
              setEditing(rule);
              setEditorOpen(true);
            }}
          />
        </TabsContent>

        <TabsContent value="test">
          <RouteSimulator />
        </TabsContent>
      </Tabs>

      <RuleEditor
        open={editorOpen}
        onOpenChange={setEditorOpen}
        rule={editing}
        selectors={selectors.data ?? []}
      />
    </PageShell>
  );
}
