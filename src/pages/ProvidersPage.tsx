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
import { ModelMappingPanel } from "@/components/providers/ModelMappingPanel";
import { ProviderDialog } from "@/components/providers/ProviderDialog";
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
import { api, type ProbeResult, type Provider } from "@/lib/api";
import { formatNumber, truncate } from "@/lib/utils";

const KIND_LABEL: Record<string, string> = {
  anthropic: "Anthropic",
  openai_chat: "OpenAI Chat",
  openai_responses: "OpenAI Responses",
};

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
      toast.success("渠道已删除");
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
      description="配置上游 LLM 渠道、鉴权与模型映射"
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
                  <TableHead>Base URL</TableHead>
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
                          <Badge variant="outline">
                            {KIND_LABEL[p.kind] ?? p.kind}
                          </Badge>
                        </TableCell>
                        <TableCell
                          className="text-muted-foreground max-w-[240px] truncate text-xs"
                          title={p.base_url}
                        >
                          {truncate(p.base_url, 34)}
                        </TableCell>
                        <TableCell className="text-center">
                          <Badge variant={p.enabled ? "success" : "secondary"}>
                            {p.enabled ? "启用" : "停用"}
                          </Badge>
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
                            <ModelMappingPanel providerId={p.id} />
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
