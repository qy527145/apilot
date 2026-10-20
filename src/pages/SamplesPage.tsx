import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertTriangle,
  CloudDownload,
  Fingerprint,
  Loader2,
  RefreshCw,
  RotateCcw,
  Trash2,
} from "lucide-react";
import { toast } from "sonner";

import { CopyButton } from "@/components/common/CopyButton";
import { EmptyState } from "@/components/common/EmptyState";
import { PageShell } from "@/components/layout/PageShell";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { qk } from "@/hooks/queries";
import {
  api,
  FINGERPRINT_REMOTE_BASE,
  type FingerprintLibraryInfo,
  type FingerprintLocalLabel,
} from "@/lib/api";

function formatTime(value: number) {
  if (!value) return "—";
  return new Date(value).toLocaleString("zh-CN", { hour12: false });
}

function sourceLabel(source: string) {
  return source === "remote" ? "远端更新" : "内置快照";
}

export default function SamplesPage() {
  return (
    <PageShell
      title="指纹样本"
      description="远端参考库与本地样本：检测渠道背后实际调用的模型，并用自己的样本做交叉验证"
    >
      <Tabs defaultValue="remote" className="gap-4">
        <TabsList>
          <TabsTrigger value="remote">远端参考库</TabsTrigger>
          <TabsTrigger value="local">本地样本库</TabsTrigger>
        </TabsList>
        <TabsContent value="remote" className="mt-2">
          <RemoteLibrary />
        </TabsContent>
        <TabsContent value="local" className="mt-2">
          <LocalLibrary />
        </TabsContent>
      </Tabs>
    </PageShell>
  );
}

function RemoteLibrary() {
  const qc = useQueryClient();
  const [updateOpen, setUpdateOpen] = useState(false);
  const [baseUrl, setBaseUrl] = useState(FINGERPRINT_REMOTE_BASE);
  const { data, isLoading } = useQuery({
    queryKey: qk.fingerprintLibrary,
    queryFn: api.fingerprintLibraryInfo,
    retry: 1,
  });

  const update = useMutation({
    mutationFn: () => api.updateFingerprintLibrary(baseUrl.trim() || null),
    onSuccess: (info: FingerprintLibraryInfo) => {
      qc.setQueryData(qk.fingerprintLibrary, info);
      qc.invalidateQueries({ queryKey: qk.fingerprintLibrary });
      setUpdateOpen(false);
      toast.success(`参考库已更新：${info.model_count} 个模型`);
    },
  });

  const reset = useMutation({
    mutationFn: api.resetFingerprintLibrary,
    onSuccess: (info: FingerprintLibraryInfo) => {
      qc.setQueryData(qk.fingerprintLibrary, info);
      qc.invalidateQueries({ queryKey: qk.fingerprintLibrary });
      toast.success("已恢复内置参考库");
    },
  });

  if (isLoading) {
    return (
      <div className="space-y-3">
        <Skeleton className="h-32 w-full" />
        <Skeleton className="h-64 w-full" />
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-sm">
            <Fingerprint className="size-4" />
            当前参考库
            {data && <Badge variant="outline">{sourceLabel(data.source)}</Badge>}
          </CardTitle>
          <CardDescription className="text-xs">
            官方排名与置信度由这份冻结检测器计算；本地样本不会修改它。
            参考库是闭集：不在库里的模型也会得到“最像”的候选。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          {data?.error && (
            <div className="flex items-start gap-2 rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs text-amber-600">
              <AlertTriangle className="mt-0.5 size-4 shrink-0" />
              <span>{data.error}</span>
            </div>
          )}
          {data && !data.error && (
            <div className="grid gap-2 text-xs sm:grid-cols-2 lg:grid-cols-3">
              <div>
                <span className="text-muted-foreground">构建时间：</span>
                {data.built_at || "—"}
              </div>
              <div>
                <span className="text-muted-foreground">模型身份：</span>
                {data.model_count}
              </div>
              <div>
                <span className="text-muted-foreground">挑战数：</span>
                {data.challenges}
              </div>
              <div className="sm:col-span-2">
                <span className="text-muted-foreground">来源：</span>
                <span className="break-all">
                  {data.source_url || "安装包内置"}
                </span>
              </div>
              <div className="flex flex-wrap items-center gap-1 sm:col-span-2 lg:col-span-3">
                <span className="text-muted-foreground">SHA-256：</span>
                <code className="break-all font-mono text-[11px]">
                  {data.sha256 || "—"}
                </code>
                {data.sha256 && <CopyButton text={data.sha256} />}
              </div>
            </div>
          )}
          <div className="flex flex-wrap gap-2">
            <Button size="sm" onClick={() => setUpdateOpen(true)}>
              <CloudDownload className="size-4" />
              检查更新
            </Button>
            <Button
              size="sm"
              variant="outline"
              disabled={data?.source === "embedded" || reset.isPending}
              onClick={() => {
                if (confirm("恢复内置参考库？远端下载的文件会保留但不再使用。")) {
                  reset.mutate();
                }
              }}
            >
              {reset.isPending ? (
                <Loader2 className="size-4 animate-spin" />
              ) : (
                <RotateCcw className="size-4" />
              )}
              恢复内置
            </Button>
            <Button
              size="sm"
              variant="ghost"
              onClick={() => qc.invalidateQueries({ queryKey: qk.fingerprintLibrary })}
            >
              <RefreshCw className="size-4" />
              刷新
            </Button>
          </div>
        </CardContent>
      </Card>

      {data && data.models.length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle className="text-sm">参考库模型</CardTitle>
            <CardDescription className="text-xs">
              这些是检测器能区分的身份；显示名可能与实际请求名不同（合并类别）。
            </CardDescription>
          </CardHeader>
          <CardContent className="p-0">
            <ScrollArea className="max-h-[520px]">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>身份</TableHead>
                    <TableHead>显示名</TableHead>
                    <TableHead>家族</TableHead>
                    <TableHead className="text-right">参考回答</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {data.models.map((model) => (
                    <TableRow key={model.id}>
                      <TableCell className="font-mono text-xs">{model.id}</TableCell>
                      <TableCell className="text-xs">{model.display_name}</TableCell>
                      <TableCell className="text-xs">{model.family_name}</TableCell>
                      <TableCell className="text-right text-xs tabular-nums">
                        {model.response_count}
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </ScrollArea>
          </CardContent>
        </Card>
      )}

      <Dialog open={updateOpen} onOpenChange={(open) => !update.isPending && setUpdateOpen(open)}>
        <DialogContent className="sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>检查指纹参考库更新</DialogTitle>
            <DialogDescription className="text-xs">
              会从 lm-detector 下载检测器（约 4.4 MB）和完整元数据（约 13 MB），
              校验兼容性后原子替换。更新只改官方参考库，不影响本地样本。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2 text-xs">
            <label className="text-muted-foreground">下载地址（可填镜像）</label>
            <Input
              value={baseUrl}
              onChange={(event) => setBaseUrl(event.target.value)}
              className="font-mono text-xs"
              placeholder={FINGERPRINT_REMOTE_BASE}
            />
            <p className="text-muted-foreground">
              必须是 https；本机镜像可用 http://127.0.0.1 或 http://localhost。
            </p>
          </div>
          <DialogFooter>
            <Button
              variant="outline"
              disabled={update.isPending}
              onClick={() => setUpdateOpen(false)}
            >
              取消
            </Button>
            <Button
              disabled={update.isPending || !baseUrl.trim()}
              onClick={() => update.mutate()}
            >
              {update.isPending ? (
                <Loader2 className="size-4 animate-spin" />
              ) : (
                <CloudDownload className="size-4" />
              )}
              {update.isPending ? "下载中…" : "开始更新"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

function LocalLibrary() {
  const qc = useQueryClient();
  const [selected, setSelected] = useState<string | null>(null);
  const labelsQuery = useQuery({
    queryKey: qk.fingerprintLabels,
    queryFn: api.listFingerprintLabels,
    retry: 1,
  });
  const labels = labelsQuery.data ?? [];
  const current =
    labels.find((label) => label.label === selected) ?? labels[0] ?? null;
  const currentLabel = current?.label ?? null;

  const samplesQuery = useQuery({
    queryKey: qk.fingerprintSamples(currentLabel),
    queryFn: () => api.listFingerprintSamples(currentLabel),
    enabled: currentLabel !== null,
    retry: 1,
  });

  const invalidate = () => {
    qc.invalidateQueries({ queryKey: qk.fingerprintLabels });
    qc.invalidateQueries({ queryKey: qk.fingerprintSamplesAll });
  };

  const removeSample = useMutation({
    mutationFn: (id: number) => api.deleteFingerprintSample(id),
    onSuccess: () => {
      invalidate();
      toast.success("已删除该样本");
    },
  });

  const removeLabel = useMutation({
    mutationFn: (label: string) => api.deleteFingerprintLabel(label),
    onSuccess: () => {
      setSelected(null);
      invalidate();
      toast.success("已删除该标签下的全部样本");
    },
  });

  if (labelsQuery.isLoading) {
    return (
      <div className="grid gap-4 lg:grid-cols-[300px_1fr]">
        <Skeleton className="h-64 w-full" />
        <Skeleton className="h-64 w-full" />
      </div>
    );
  }

  if (labels.length === 0) {
    return (
      <EmptyState
        icon={Fingerprint}
        title="还没有本地样本"
        description="在「渠道管理」里展开一条渠道，对模型点指纹图标。检测完成后可以把有效回答加入本地样本库，并指定你确认的权威模型。每类至少 3 条才参与检测，6 条才会给出本地概率。"
      />
    );
  }

  return (
    <div className="grid gap-4 lg:grid-cols-[300px_1fr]">
      <Card className="py-0">
        <CardHeader className="border-b py-3">
          <CardTitle className="text-sm">本地标签</CardTitle>
          <CardDescription className="text-xs">
            权威模型由你在加入样本时确认。
          </CardDescription>
        </CardHeader>
        <CardContent className="max-h-[calc(100vh-260px)] overflow-y-auto p-0">
          {labels.map((label) => (
            <LabelRow
              key={label.label}
              label={label}
              active={label.label === currentLabel}
              onClick={() => setSelected(label.label)}
            />
          ))}
        </CardContent>
      </Card>

      {current && (
        <Card className="min-w-0">
          <CardHeader className="flex-row items-start justify-between gap-2">
            <div className="min-w-0">
              <CardTitle className="flex flex-wrap items-center gap-2 text-sm">
                <span className="truncate font-mono">{current.label}</span>
                {current.official ? (
                  <Badge variant="success">官方同名</Badge>
                ) : (
                  <Badge variant="outline">本地独有</Badge>
                )}
                <Badge variant="secondary">{current.samples} 条样本</Badge>
              </CardTitle>
              <CardDescription className="mt-1 text-xs">
                {current.samples >= 6
                  ? "已满足本地概率门槛（≥6 条）。"
                  : current.samples >= 3
                    ? `已参与本地排名；还差 ${6 - current.samples} 条可给出本地概率。`
                    : `还差 ${3 - current.samples} 条才会参与本地排名。`}
              </CardDescription>
            </div>
            <Button
              size="sm"
              variant="ghost"
              className="text-destructive shrink-0"
              disabled={removeLabel.isPending}
              onClick={() => {
                if (confirm(`删除「${current.label}」下的全部 ${current.samples} 条样本？`)) {
                  removeLabel.mutate(current.label);
                }
              }}
            >
              <Trash2 className="size-4" />
              删除标签
            </Button>
          </CardHeader>
          <CardContent className="p-0">
            {samplesQuery.isLoading ? (
              <div className="p-4">
                <Skeleton className="h-32 w-full" />
              </div>
            ) : (
              <ScrollArea className="max-h-[calc(100vh-300px)]">
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead>挑战</TableHead>
                      <TableHead className="text-right">期望数字</TableHead>
                      <TableHead className="text-right">有效数字</TableHead>
                      <TableHead>来源渠道 / 模型</TableHead>
                      <TableHead>加入时间</TableHead>
                      <TableHead className="w-10" />
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {(samplesQuery.data ?? []).map((sample) => (
                      <TableRow key={sample.id}>
                        <TableCell className="font-mono text-xs">
                          {sample.challenge_id}
                        </TableCell>
                        <TableCell className="text-right text-xs tabular-nums">
                          {sample.expected_count}
                        </TableCell>
                        <TableCell className="text-right text-xs tabular-nums">
                          {sample.numbers_count}
                        </TableCell>
                        <TableCell className="text-xs whitespace-normal">
                          <div>{sample.provider_tag || "—"}</div>
                          <div className="text-muted-foreground font-mono text-[11px]">
                            {sample.declared_model || "—"}
                          </div>
                        </TableCell>
                        <TableCell className="text-xs">
                          {formatTime(sample.created_at)}
                        </TableCell>
                        <TableCell>
                          <Button
                            size="icon"
                            variant="ghost"
                            disabled={removeSample.isPending}
                            onClick={() => removeSample.mutate(sample.id)}
                          >
                            <Trash2 className="size-4 text-destructive" />
                          </Button>
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </ScrollArea>
            )}
          </CardContent>
        </Card>
      )}
    </div>
  );
}

function LabelRow({
  label,
  active,
  onClick,
}: {
  label: FingerprintLocalLabel;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`hover:bg-accent/50 block w-full border-b px-3 py-2.5 text-left last:border-b-0 ${
        active ? "bg-accent" : ""
      }`}
    >
      <div className="flex items-center justify-between gap-2">
        <span className="truncate font-mono text-sm">{label.label}</span>
        <span className="text-muted-foreground shrink-0 text-xs tabular-nums">
          {label.samples} 条
        </span>
      </div>
      <div className="text-muted-foreground mt-1 flex items-center gap-2 text-[11px]">
        {label.official ? <Badge variant="success">官方</Badge> : null}
        <span>最后加入 {formatTime(label.last_added_at)}</span>
      </div>
    </button>
  );
}
