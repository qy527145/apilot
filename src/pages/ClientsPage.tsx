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
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
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
import {
  api,
  type AppSettings,
  type ClientDetect,
  type ClientModelMode,
  type ThinkingMode,
} from "@/lib/api";
import { cn } from "@/lib/utils";

/// 四种模式各自的说明。文案要能让人在不看源码的情况下选对 —— 尤其"有没有 apply_patch"
/// 和"依不依赖网关"这两条代价。
const MODES: { value: ClientModelMode; label: string; hint: string }[] = [
  {
    value: "off",
    label: "不碰客户端配置",
    hint: "接管只改网关地址。客户端用什么模型由它自己决定 —— 如果它是 GPT 系名字，Codex 会走 Responses Lite。",
  },
  {
    value: "rename",
    label: "只写模型名",
    hint: "把 Apilot 当前配置的模型写进客户端。不依赖网关、不用联网，但 Codex 会失去 apply_patch（改用 shell 写文件），且按模型名配的路由规则会跟着变。",
  },
  {
    value: "catalog",
    label: "只下发模型目录",
    hint: "让 Codex 来取网关的模型元数据，含 apply_patch。要多写两个 Codex 开关，且客户端启动时网关得可达 —— 取不到会静默退回 Lite。",
  },
  {
    value: "both",
    label: "两个都写（推荐）",
    hint: "目录取到 → 完整元数据（含 apply_patch）；取不到（网关没起 / 目录被撤）→ 模型名那条路兜底：经典工具集，没有 apply_patch，但至少不是 Lite。",
  },
];

/**
 * 设置还没读回来时按这个预选。
 *
 * **必须与后端 `AppSettings::default()` 一致**（那里是 `ClientModelMode::Both`）：
 * 不一致的话，首屏会先显示一个用户从没选过的模式，读回来再跳一下。
 */
const DEFAULT_MODE: ClientModelMode = "both";

/// 思考接管的档位。与上面那条**不同**：它不写客户端配置，而是在网关侧改写请求参数，
/// 所以每条请求都生效 —— 不用重启客户端，也不用重新接管一次。
///
/// 各家的参数名与取值并不通用，所以界面上只给档位，折算由 Apilot 按上游实际协议做。
const THINKING_MODES: { value: ThinkingMode; label: string; hint: string }[] = [
  {
    value: "off",
    label: "不接管思考",
    hint: "客户端发什么思考参数就原样转发。",
  },
  {
    value: "disabled",
    label: "关闭（不发思考参数）",
    hint: "把请求里的思考参数去掉，由上游按默认处理。注意这不等于上游一定不思考 —— 推理模型照样推理，只是不再由客户端指定档位。",
  },
  {
    value: "low",
    label: "低",
    hint: "Anthropic 线写 thinking.budget_tokens（4096，上限受 max_tokens 约束），OpenAI 系写 effort=low。",
  },
  {
    value: "medium",
    label: "中",
    hint: "Anthropic 线写 thinking.budget_tokens（8192），OpenAI 系写 effort=medium。",
  },
  {
    value: "high",
    label: "高",
    hint: "Anthropic 线写 thinking.budget_tokens（16384），OpenAI 系写 effort=high。",
  },
  {
    value: "xhigh",
    label: "极高",
    hint: "Anthropic 线写 thinking.budget_tokens（32768），OpenAI 系写 effort=xhigh。部分上游不认这个档，会直接报错。",
  },
];

/** 同 `DEFAULT_MODE` 的约定：必须与后端 `AppSettings::default()` 一致（那里是 `Off`）。 */
const DEFAULT_THINKING_MODE: ThinkingMode = "off";

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

  // 这个模式存在全局设置里（`AppSettings.client_model_mode`），但它的效果只在
  // 「接管」这一步发生，所以放在这一页而不是设置页。
  const { data: settings } = useSettings();
  const saveMode = useMutation({
    mutationFn: (mode: ClientModelMode) =>
      api.updateSettings({ ...(settings as AppSettings), client_model_mode: mode }),
    onSuccess: (s) => {
      qc.setQueryData(qk.settings, s);
      toast.success("已保存；已接管的客户端已按新策略重写", {
        description: "重启客户端后生效。",
      });
    },
  });

  // 思考接管同样是全局设置，但它的效果发生在**每一条请求**上（网关侧改写参数），
  // 所以保存完立刻生效 —— 没有"重启客户端""重新接管"这些后续动作。
  const saveThinking = useMutation({
    mutationFn: (mode: ThinkingMode) =>
      api.updateSettings({ ...(settings as AppSettings), thinking_mode: mode }),
    onSuccess: (s) => {
      qc.setQueryData(qk.settings, s);
      toast.success("已保存；下一条请求就按新档位发往上游");
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
          <CardContent className="space-y-3 py-4">
            <div className="space-y-1">
              <Label>接管时怎么处理模型配置</Label>
              <p className="text-muted-foreground text-xs">
                Codex 用不用 Responses Lite 只由它的模型元数据决定，而 GPT 系名字内置就是
                Lite —— 工具被塞进 input 里的 additional_tools。有些上游收下这种请求、返回
                200，工具却一个都不认，模型只能把工具调用当正文吐出来。
              </p>
            </div>
            <Select
              value={settings?.client_model_mode ?? DEFAULT_MODE}
              onValueChange={(v) => saveMode.mutate(v as ClientModelMode)}
              disabled={!settings || saveMode.isPending}
            >
              <SelectTrigger className="w-full sm:w-80">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {MODES.map((m) => (
                  <SelectItem key={m.value} value={m.value}>
                    {m.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <p className="text-muted-foreground text-xs">
              {MODES.find((m) => m.value === (settings?.client_model_mode ?? DEFAULT_MODE))?.hint}
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardContent className="space-y-3 py-4">
            <div className="space-y-1">
              <Label>接管思考</Label>
              <p className="text-muted-foreground text-xs">
                三种客户端的思考都是靠请求参数控制的，所以这里不改客户端配置，而是由 Apilot
                在转发前改写参数。也因此它每一条请求都生效 —— 不用重启客户端，也不用重新接管一次。
                各家协议对应的参数名与取值并不通用，折算按上游渠道实际用的协议做。
              </p>
            </div>
            <Select
              value={settings?.thinking_mode ?? DEFAULT_THINKING_MODE}
              onValueChange={(v) => saveThinking.mutate(v as ThinkingMode)}
              disabled={!settings || saveThinking.isPending}
            >
              <SelectTrigger className="w-full sm:w-80">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {THINKING_MODES.map((m) => (
                  <SelectItem key={m.value} value={m.value}>
                    {m.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <p className="text-muted-foreground text-xs">
              {
                THINKING_MODES.find(
                  (m) => m.value === (settings?.thinking_mode ?? DEFAULT_THINKING_MODE),
                )?.hint
              }
            </p>
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
