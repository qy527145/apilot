import { Fragment, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ChevronDown,
  ChevronRight,
  Loader2,
  Pencil,
  Plus,
  PlugZap,
  Trash2,
} from "lucide-react";
import { toast } from "sonner";

import { EmptyState } from "@/components/common/EmptyState";
import { ErrorState, TableSkeleton } from "@/components/common/StatCard";
import { PageShell } from "@/components/layout/PageShell";
import { ProviderDialog } from "@/components/providers/ProviderDialog";
import { ProviderModelsPanel } from "@/components/providers/ProviderModelsPanel";
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
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { qk } from "@/hooks/queries";
import {
  api,
  PROTOCOL_LABEL,
  type ProbeResult,
  type Protocol,
  type Provider,
} from "@/lib/api";
import { cn, formatNumber } from "@/lib/utils";

const KIND_LABEL: Record<string, string> = {
  anthropic: "Anthropic",
  openai_chat: "OpenAI Chat",
  openai_responses: "OpenAI Responses",
};

/**
 * 该渠道支持的协议集合。
 *
 * 库里存空数组表示"只支持 kind 那一种"——这是老数据的语义，
 * 界面必须照后端 `Provider::endpoints()` 的规则还原，否则会显示成"一种都不支持"。
 */
const supportedProtocols = (p: Provider): Protocol[] =>
  p.protocols?.length ? p.protocols.map((e) => e.protocol) : [p.kind];

export default function ProvidersPage() {
  const qc = useQueryClient();
  const [dialogOpen, setDialogOpen] = useState(false);
  const [editing, setEditing] = useState<Provider | null>(null);
  const [expanded, setExpanded] = useState<number | null>(null);
  const [probes, setProbes] = useState<Record<number, ProbeResult | "loading">>({});

  const { data, isLoading, isError, refetch } = useQuery({
    queryKey: qk.providers,
    queryFn: api.listProviders,
    retry: 1,
  });

  const del = useMutation({
    mutationFn: (id: number) => api.deleteProvider(id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.providers });
      qc.invalidateQueries({ queryKey: qk.modelCatalog });
      toast.success("渠道已删除");
    },
  });

  /**
   * 就地启用 / 停用。
   *
   * 乐观更新：拨动开关要有立刻的反馈，而后端还要写库 + 重载注册表。
   * 失败时回滚到拨动前的快照 —— 不回滚的话开关会停在用户以为的状态上，
   * 而路由那边其实没变。
   */
  const toggle = useMutation({
    mutationFn: ({ id, enabled }: { id: number; enabled: boolean }) =>
      api.setProviderEnabled(id, enabled),
    onMutate: async ({ id, enabled }) => {
      await qc.cancelQueries({ queryKey: qk.providers });
      const prev = qc.getQueryData<Provider[]>(qk.providers);
      qc.setQueryData<Provider[]>(qk.providers, (old) =>
        old?.map((p) => (p.id === id ? { ...p, enabled } : p)),
      );
      return { prev };
    },
    onError: (_e, _vars, ctx) => {
      if (ctx?.prev) qc.setQueryData(qk.providers, ctx.prev);
    },
    onSuccess: (_d, { enabled }) => {
      toast.success(enabled ? "渠道已启用" : "渠道已停用", {
        description: enabled ? undefined : "路由不会再选择它。",
      });
    },
    onSettled: () => {
      qc.invalidateQueries({ queryKey: qk.providers });
      // 渠道的启用状态会改变模型页的候选列表。
      qc.invalidateQueries({ queryKey: qk.modelCatalog });
    },
  });

  const runTest = async (p: Provider) => {
    setProbes((prev) => ({ ...prev, [p.id]: "loading" }));
    try {
      const res = await api.testProvider(p.id);
      setProbes((prev) => ({ ...prev, [p.id]: res }));
    } catch {
      /* toast 已在 api 层弹出 */
      setProbes((prev) => ({
        ...prev,
        [p.id]: { tag: p.tag, ok: false, error: "测试失败" },
      }));
    }
  };

  const openCreate = () => {
    setEditing(null);
    setDialogOpen(true);
  };

  const openEdit = (p: Provider) => {
    setEditing(p);
    setDialogOpen(true);
  };

  const providers = data ?? [];

  return (
    <PageShell
      title="渠道管理"
      description="配置上游 LLM 渠道、鉴权与可用模型"
      actions={
        <Button onClick={openCreate}>
          <Plus className="size-4" />
          新建渠道
        </Button>
      }
    >
      <Card className="py-0">
        <CardContent className="p-0">
          {isLoading ? (
            <div className="p-4">
              <TableSkeleton rows={5} cols={6} />
            </div>
          ) : isError ? (
            <div className="p-4">
              <ErrorState onRetry={() => refetch()} />
            </div>
          ) : providers.length === 0 ? (
            <div className="p-6">
              <EmptyState
                icon={PlugZap}
                title="还没有渠道"
                description="点击右上角「新建渠道」，添加 Anthropic / OpenAI 等上游服务。"
                action={
                  <Button size="sm" onClick={openCreate}>
                    <Plus className="size-4" />
                    新建渠道
                  </Button>
                }
              />
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="w-8" />
                  <TableHead>名称</TableHead>
                  <TableHead>Tag</TableHead>
                  <TableHead>类型</TableHead>
                  <TableHead className="w-full min-w-[112px] max-w-0">
                    Base URL
                  </TableHead>
                  <TableHead className="text-center">状态</TableHead>
                  <TableHead className="text-right">优先级</TableHead>
                  <TableHead className="text-right">权重</TableHead>
                  <TableHead className="text-right">操作</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {providers.map((p) => {
                  const probe = probes[p.id];
                  const isOpen = expanded === p.id;
                  return (
                    <Fragment key={p.id}>
                      <TableRow>
                        <TableCell>
                          <Button
                            variant="ghost"
                            size="icon"
                            className="size-6"
                            onClick={() => setExpanded(isOpen ? null : p.id)}
                          >
                            {isOpen ? (
                              <ChevronDown className="size-4" />
                            ) : (
                              <ChevronRight className="size-4" />
                            )}
                          </Button>
                        </TableCell>
                        <TableCell className="font-medium">{p.name}</TableCell>
                        <TableCell>
                          <code className="text-muted-foreground text-xs">
                            {p.tag}
                          </code>
                        </TableCell>
                        <TableCell>
                          <div className="space-y-1">
                            <Badge variant="outline">
                              {KIND_LABEL[p.kind] ?? p.kind}
                            </Badge>
                            {supportedProtocols(p).length > 1 && (
                              <Tooltip>
                                <TooltipTrigger asChild>
                                  <div className="text-muted-foreground cursor-help text-[11px]">
                                    支持 {supportedProtocols(p).length} 种协议
                                  </div>
                                </TooltipTrigger>
                                <TooltipContent>
                                  <div className="space-y-0.5 text-xs">
                                    {supportedProtocols(p).map((proto) => {
                                      const override = p.protocols?.find(
                                        (e) => e.protocol === proto,
                                      )?.path;
                                      return (
                                        <div key={proto}>
                                          {PROTOCOL_LABEL[proto]}
                                          {proto === p.kind ? "（首选）" : ""}
                                          <span className="text-muted-foreground">
                                            {" "}
                                            {override ?? "默认路径"}
                                          </span>
                                        </div>
                                      );
                                    })}
                                  </div>
                                </TooltipContent>
                              </Tooltip>
                            )}
                          </div>
                        </TableCell>
                        {/*
                          宽度靠 CSS 自适应，不用 truncate(s, 34) 那种按字符数硬砍：
                          窗口宽时白占位、窄时又砍不出足够空间。表头与本格同用
                          w-full + max-w-0，这列就吃掉整张表的剩余宽度，由内层
                          truncate 按实际空间截断。

                          min-w 是刻意的下限：不给下限时这列在窄窗口会被压到
                          60px 出头，只剩「https:...」，读不出是哪家上游。宁可
                          让表格在窗口窄于约 1050px 时横向滚动，也别把 URL 压废。
                        */}
                        <TableCell className="text-muted-foreground w-full min-w-[112px] max-w-0 text-xs">
                          <div className="truncate" title={p.base_url}>
                            {p.base_url}
                          </div>
                        </TableCell>
                        <TableCell className="text-center">
                          <StatusToggle
                            provider={p}
                            pending={
                              toggle.isPending && toggle.variables?.id === p.id
                            }
                            onToggle={(enabled) =>
                              toggle.mutate({ id: p.id, enabled })
                            }
                          />
                        </TableCell>
                        <TableCell className="text-right tabular-nums">
                          {formatNumber(p.priority)}
                        </TableCell>
                        <TableCell className="text-right tabular-nums">
                          {formatNumber(p.weight)}
                        </TableCell>
                        <TableCell>
                          <div className="flex items-center justify-end gap-1">
                            <ProbeButton
                              probe={probe}
                              onTest={() => runTest(p)}
                            />
                            <Tooltip>
                              <TooltipTrigger asChild>
                                <Button
                                  variant="ghost"
                                  size="icon"
                                  onClick={() => openEdit(p)}
                                >
                                  <Pencil className="size-4" />
                                </Button>
                              </TooltipTrigger>
                              <TooltipContent>编辑</TooltipContent>
                            </Tooltip>
                            <Tooltip>
                              <TooltipTrigger asChild>
                                <Button
                                  variant="ghost"
                                  size="icon"
                                  disabled={del.isPending}
                                  onClick={() => {
                                    if (confirm(`确认删除渠道「${p.name}」？`))
                                      del.mutate(p.id);
                                  }}
                                >
                                  <Trash2 className="size-4 text-destructive" />
                                </Button>
                              </TooltipTrigger>
                              <TooltipContent>删除</TooltipContent>
                            </Tooltip>
                          </div>
                        </TableCell>
                      </TableRow>
                      {isOpen && (
                        <TableRow>
                          <TableCell colSpan={9} className="bg-muted/20 p-0">
                            <ProviderModelsPanel
                              providerId={p.id}
                              providerName={p.name}
                            />
                          </TableCell>
                        </TableRow>
                      )}
                    </Fragment>
                  );
                })}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>

      <ProviderDialog
        open={dialogOpen}
        onOpenChange={setDialogOpen}
        provider={editing}
      />
    </PageShell>
  );
}

function ProbeButton({
  probe,
  onTest,
}: {
  probe: ProbeResult | "loading" | undefined;
  onTest: () => void;
}) {
  if (probe === "loading") {
    return (
      <Button variant="ghost" size="icon" disabled>
        <Loader2 className="size-4 animate-spin" />
      </Button>
    );
  }
  if (probe) {
    return (
      <Tooltip>
        <TooltipTrigger asChild>
          <Button
            variant="ghost"
            size="sm"
            className={probe.ok ? "text-emerald-500" : "text-destructive"}
            onClick={onTest}
          >
            {probe.ok
              ? `${probe.latency_ms ?? 0} ms`
              : "失败"}
          </Button>
        </TooltipTrigger>
        <TooltipContent>
          {probe.ok ? "连通正常，点击重测" : probe.error ?? "连通失败"}
        </TooltipContent>
      </Tooltip>
    );
  }
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button variant="ghost" size="icon" onClick={onTest}>
          <PlugZap className="size-4" />
        </Button>
      </TooltipTrigger>
      <TooltipContent>测试连通</TooltipContent>
    </Tooltip>
  );
}

/**
 * 「状态」那一列的药丸开关。
 *
 * 用按钮而不是 `Switch`：这一列原先是个只读徽标，换成开关轨之后宽度、点击热区
 * 都变了，而同桌还有优先级 / 权重两列数字挤在右边。药丸保持原来的视觉分量
 * （绿=启用、灰=停用），点一下就切，且带明确的 hover 反馈。
 */
function StatusToggle({
  provider,
  pending,
  onToggle,
}: {
  provider: Provider;
  pending: boolean;
  onToggle: (enabled: boolean) => void;
}) {
  const on = provider.enabled;
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          disabled={pending}
          aria-pressed={on}
          aria-label={on ? "停用该渠道" : "启用该渠道"}
          onClick={() => onToggle(!on)}
          className={cn(
            "inline-flex cursor-pointer items-center gap-1.5 rounded-full border px-2.5 py-0.5 text-xs font-medium whitespace-nowrap transition-colors disabled:opacity-50",
            on
              ? "border-emerald-500/30 bg-emerald-500/15 text-emerald-500 hover:bg-emerald-500/25"
              : "border-transparent bg-secondary text-secondary-foreground hover:bg-secondary/80",
          )}
        >
          <span
            className={cn(
              "size-1.5 rounded-full",
              on ? "bg-emerald-500" : "bg-muted-foreground",
            )}
          />
          {on ? "启用" : "停用"}
        </button>
      </TooltipTrigger>
      <TooltipContent>
        {on ? "点击停用 —— 路由不再选择它" : "点击启用"}
      </TooltipContent>
    </Tooltip>
  );
}
