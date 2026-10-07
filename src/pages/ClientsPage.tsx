import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, Eye, FilePenLine, RefreshCw, RotateCcw, ShieldCheck } from "lucide-react";
import { toast } from "sonner";

import { EmptyState } from "@/components/common/EmptyState";
import { ErrorState, TableSkeleton } from "@/components/common/StatCard";
import { ManualOnboarding } from "@/components/clients/ManualOnboarding";
import { PageShell } from "@/components/layout/PageShell";
import type { ViewKey } from "@/components/layout/nav";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { qk, useSettings } from "@/hooks/queries";
import { api, type AppSettings, type ClientDetect } from "@/lib/api";
import { cn } from "@/lib/utils";

export default function ClientsPage({
  onNavigate,
}: {
  onNavigate: (view: ViewKey) => void;
}) {
  const qc = useQueryClient();
  const [preview, setPreview] = useState<{ client: ClientDetect; diff: string } | null>(
    null,
  );
  const [previewLoading, setPreviewLoading] = useState<string | null>(null);

  const { data, isLoading, isError, refetch, isFetching } = useQuery({
    queryKey: qk.clients,
    queryFn: api.detectClients,
    retry: 1,
  });

  // 就绪判定在后端：前端只负责把「缺哪一步」说清楚，不自己复刻一套规则。
  const { data: readiness } = useQuery({
    queryKey: qk.takeoverReadiness,
    queryFn: api.takeoverReadiness,
    retry: 1,
  });
  const blocked = readiness ? !readiness.ready : false;

  // 这个开关存在全局设置里（`AppSettings.inject_client_model`），但它的效果只在
  // 「接管」这一步发生，所以放在这一页而不是设置页。
  const { data: settings } = useSettings();
  const saveInject = useMutation({
    mutationFn: (inject: boolean) =>
      api.updateSettings({ ...(settings as AppSettings), inject_client_model: inject }),
    onSuccess: (s) => {
      qc.setQueryData(qk.settings, s);
      toast.success(
        s.inject_client_model
          ? "已开启：下次接管会把当前模型写进客户端配置"
          : "已关闭：接管不再改动客户端选的模型",
      );
    },
  });

  const invalidate = () => {
    qc.invalidateQueries({ queryKey: qk.clients });
    qc.invalidateQueries({ queryKey: qk.logs("") });
  };
  const apply = useMutation({
    mutationFn: (client: string) => api.applyTakeover(client),
    onSuccess: (res) => {
      toast.success(res.message || "已接管");
      invalidate();
    },
  });

  const restore = useMutation({
    mutationFn: (client: string) => api.restoreClient(client),
    onSuccess: (res) => {
      toast.success(res.message || "已还原");
      invalidate();
    },
  });

  // 打开失败基本只有一种情况：文件还没生成。后端已把话说清楚了，这里不再兜底文案。
  const openConfig = useMutation({ mutationFn: api.openClientConfig });

  const openPreview = async (client: ClientDetect) => {
    setPreviewLoading(client.id);
    try {
      const diff = await api.previewTakeover(client.id);
      setPreview({ client, diff });
    } catch {
      /* api 层已 toast */
    } finally {
      setPreviewLoading(null);
    }
  };

  const clients = data ?? [];

  return (
    <PageShell
      title="客户端接管"
      description="自动检测并改写本机 CLI 客户端的 API 配置，使其指向 Apilot"
      actions={
        <Button
          variant="outline"
          onClick={() => refetch()}
          disabled={isFetching}
        >
          <RefreshCw className={cn("size-4", isFetching && "animate-spin")} />
          重新检测
        </Button>
      }
    >
      <div className="space-y-6">
        {blocked && (
          <div className="border-amber-500/40 bg-amber-500/10 flex items-start gap-3 rounded-md border p-3">
            <AlertTriangle className="mt-0.5 size-4 shrink-0 text-amber-500" />
            <div className="flex-1 space-y-1">
              <p className="text-sm font-medium">暂时不能接管</p>
              <p className="text-muted-foreground text-xs">
                {readiness?.reason}
                。接管前先备好渠道和模型，客户端才不会一上来就报错。
              </p>
            </div>
            <Button size="sm" variant="outline" onClick={() => onNavigate("providers")}>
              去渠道管理
            </Button>
          </div>
        )}

        <Card>
          <CardContent className="flex items-start justify-between gap-4 py-4">
            <div className="space-y-1">
              <Label>接管时写入当前模型</Label>
              <p className="text-muted-foreground text-xs">
                把「当前配置的模型」写进客户端配置。对 Codex 尤其有用：GPT 系模型名会让
                它走 Responses Lite，工具被塞进 input 里的 additional_tools；而有些上游
                收下这种请求、返回 200，工具却一个都不认，模型只能把工具调用当正文吐出来。
                换成一个它不认识的名字，元数据就退回经典工具集。
              </p>
              <p className="text-muted-foreground text-xs">
                代价：Codex 会失去 apply_patch（改用 shell 写文件），按模型名配的路由规则
                也会跟着变。改完这个开关要重新接管一次才生效。
              </p>
            </div>
            <Switch
              checked={settings?.inject_client_model ?? false}
              onCheckedChange={(v) => saveInject.mutate(v)}
              disabled={!settings || saveInject.isPending}
            />
          </CardContent>
        </Card>

        <Card className="py-0">
          <CardContent className="p-0">
            {isLoading ? (
              <div className="p-4">
                <TableSkeleton rows={3} cols={4} />
              </div>
            ) : isError ? (
              <div className="p-4">
                <ErrorState onRetry={() => refetch()} />
              </div>
            ) : clients.length === 0 ? (
              <div className="p-6">
                <EmptyState
                  icon={ShieldCheck}
                  title="未检测到支持的客户端"
                  description="Apilot 支持接管 Claude Code / Codex / Gemini CLI。安装其中任意一个后点击「重新检测」。"
                />
              </div>
            ) : (
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>客户端</TableHead>
                    <TableHead className="text-center">检测</TableHead>
                    <TableHead className="text-center">接管</TableHead>
                    <TableHead>配置文件</TableHead>
                    <TableHead>当前 Base URL</TableHead>
                    <TableHead className="text-right">操作</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {clients.map((c) => (
                    <TableRow key={c.id}>
                      <TableCell className="font-medium">{c.name}</TableCell>
                      <TableCell className="text-center">
                        <Badge variant={c.detected ? "success" : "secondary"}>
                          {c.detected ? "已安装" : "未安装"}
                        </Badge>
                      </TableCell>
                      <TableCell className="text-center">
                        <Badge variant={c.taken_over ? "success" : "outline"}>
                          {c.taken_over ? "已接管" : "未接管"}
                        </Badge>
                      </TableCell>
                      <TableCell className="max-w-[260px] text-xs">
                        <div className="flex items-center gap-1">
                          <span
                            className="text-muted-foreground truncate"
                            title={c.config_path}
                          >
                            {c.config_path}
                          </span>
                          <Tooltip>
                            <TooltipTrigger asChild>
                              {/* 禁用的按钮吃不到 hover（pointer-events-none），
                                  套一层 span 才能让「为什么不能开」的提示浮出来 */}
                              <span className="inline-flex shrink-0">
                                <Button
                                  variant="ghost"
                                  size="icon"
                                  className="size-6"
                                  disabled={!c.detected || openConfig.isPending}
                                  onClick={() => openConfig.mutate(c.id)}
                                >
                                  <FilePenLine className="size-3.5" />
                                </Button>
                              </span>
                            </TooltipTrigger>
                            <TooltipContent>
                              {c.detected
                                ? "用默认程序打开配置文件"
                                : "配置文件还没生成，先接管"}
                            </TooltipContent>
                          </Tooltip>
                        </div>
                      </TableCell>
                      <TableCell
                        className="text-muted-foreground max-w-[220px] truncate text-xs"
                        title={c.current_base_url ?? ""}
                      >
                        {c.current_base_url || "—"}
                      </TableCell>
                      <TableCell>
                        <div className="flex items-center justify-end gap-1">
                          <Button
                            variant="ghost"
                            size="sm"
                            disabled={previewLoading === c.id}
                            onClick={() => openPreview(c)}
                          >
                            <Eye className="size-4" />
                            预览变更
                          </Button>
                          {c.taken_over ? (
                            <Button
                              variant="outline"
                              size="sm"
                              disabled={restore.isPending}
                              onClick={() => restore.mutate(c.id)}
                            >
                              <RotateCcw className="size-4" />
                              还原
                            </Button>
                          ) : (
                            <Button
                              size="sm"
                              disabled={!c.detected || blocked || apply.isPending}
                              title={blocked ? readiness?.reason ?? undefined : undefined}
                              onClick={() => apply.mutate(c.id)}
                            >
                              <ShieldCheck className="size-4" />
                              接管
                            </Button>
                          )}
                        </div>
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            )}
          </CardContent>
        </Card>

        <ManualOnboarding />

        <Dialog open={!!preview} onOpenChange={(o) => !o && setPreview(null)}>
          <DialogContent className="sm:max-w-3xl">
            <DialogHeader>
              <DialogTitle>预览变更 · {preview?.client.name}</DialogTitle>
              <DialogDescription>
                即将写入 <code>{preview?.client.config_path}</code> 的内容差异。
              </DialogDescription>
            </DialogHeader>
            <DiffView text={preview?.diff ?? ""} />
          </DialogContent>
        </Dialog>
      </div>
    </PageShell>
  );
}

/** unified diff 风格渲染：+ 绿、- 红、@@ 蓝色 */
function DiffView({ text }: { text: string }) {
  if (!text.trim()) {
    return (
      <p className="text-muted-foreground py-8 text-center text-sm">
        没有需要变更的内容（可能已经指向 Apilot）。
      </p>
    );
  }
  return (
    <div className="max-h-[60vh] overflow-auto rounded-md border bg-muted/30 p-3">
      <pre className="font-mono text-xs leading-relaxed">
        {text.split("\n").map((line, i) => (
          <div
            key={i}
            className={cn(
              "px-1",
              line.startsWith("+") &&
                !line.startsWith("+++") &&
                "bg-emerald-500/10 text-emerald-500",
              line.startsWith("-") &&
                !line.startsWith("---") &&
                "bg-red-500/10 text-red-500",
              line.startsWith("@@") && "text-blue-400",
            )}
          >
            {line || " "}
          </div>
        ))}
      </pre>
    </div>
  );
}
