import { useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Gauge, Plus, Trash2, Zap } from "lucide-react";
import { toast } from "sonner";

import { EmptyState } from "@/components/common/EmptyState";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { qk } from "@/hooks/queries";
import {
  api,
  type ProbeResult,
  type Provider,
  type Selector,
  type SelectorInput,
} from "@/lib/api";
import { cn } from "@/lib/utils";

interface Props {
  selectors: Selector[];
  providers: Provider[];
  isLoading: boolean;
  isError: boolean;
  onRetry: () => void;
}

export function SelectorPanel({
  selectors,
  providers,
  isLoading,
  isError,
  onRetry,
}: Props) {
  const qc = useQueryClient();
  const [dialogOpen, setDialogOpen] = useState(false);
  const [draft, setDraft] = useState<SelectorInput>({
    tag: "",
    name: "",
    mode: "selector",
    members: [],
    tolerance_ms: 50,
  });
  const [testing, setTesting] = useState<string | null>(null);
  const [probes, setProbes] = useState<Record<string, ProbeResult[]>>({});

  const nameOf = (tag: string) =>
    providers.find((p) => p.tag === tag)?.name ?? tag;

  const switchTo = useMutation({
    mutationFn: ({ selector, providerTag }: { selector: string; providerTag: string }) =>
      api.switchSelector(selector, providerTag),
    onSuccess: (sel) => {
      qc.invalidateQueries({ queryKey: qk.selectors });
      toast.success(`已切到 ${nameOf(sel.current_provider ?? "")}，无需重启`);
    },
  });

  const create = useMutation({
    mutationFn: (input: SelectorInput) => api.upsertSelector(input),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.selectors });
      toast.success("选择器已创建");
      setDialogOpen(false);
    },
  });

  const remove = useMutation({
    mutationFn: (tag: string) => api.deleteSelector(tag),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.selectors });
      toast.success("选择器已删除");
    },
  });

  const runTest = async (tag: string) => {
    setTesting(tag);
    try {
      const res = await api.runUrltest(tag);
      setProbes((p) => ({ ...p, [tag]: res }));
    } catch {
      /* toast 已处理 */
    } finally {
      setTesting(null);
    }
  };

  return (
    <Card>
      <CardHeader className="flex-row items-center justify-between">
        <CardTitle className="text-sm">选择器（一键热切换）</CardTitle>
        <Button
          size="sm"
          variant="outline"
          onClick={() => {
            setDraft({
              tag: "",
              name: "",
              mode: "selector",
              members: [],
              tolerance_ms: 50,
            });
            setDialogOpen(true);
          }}
        >
          <Plus className="size-4" />
          新建选择器
        </Button>
      </CardHeader>
      <CardContent className="space-y-3">
        {isLoading ? (
          <>
            <Skeleton className="h-20 w-full" />
            <Skeleton className="h-20 w-full" />
          </>
        ) : isError ? (
          <div className="py-6 text-center text-xs">
            <span className="text-destructive">加载选择器失败。 </span>
            <button className="underline" onClick={onRetry}>
              重试
            </button>
          </div>
        ) : selectors.length === 0 ? (
          <EmptyState
            icon={Zap}
            title="还没有选择器"
            description="选择器把多个渠道聚合成一个可热切换的池，路由规则最终指向它。"
          />
        ) : (
          selectors.map((s) => {
            const current = s.current_provider ?? "";
            const probeList = probes[s.tag];
            return (
              <div
                key={s.tag}
                className="flex flex-col gap-3 rounded-lg border p-3 lg:flex-row lg:items-center lg:justify-between"
              >
                <div className="min-w-0 space-y-1">
                  <div className="flex flex-wrap items-center gap-2">
                    <span className="text-sm font-medium">{s.name}</span>
                    <code className="text-muted-foreground text-xs">{s.tag}</code>
                    <Badge variant="outline">
                      {s.mode === "urltest" ? "自动测速" : "手动"}
                    </Badge>
                    <span className="text-muted-foreground text-xs">
                      当前：
                      <span className="text-foreground font-medium">
                        {current ? nameOf(current) : "未选择"}
                      </span>
                    </span>
                  </div>
                  <div className="flex flex-wrap gap-1">
                    {s.members.length === 0 && (
                      <span className="text-muted-foreground text-xs">
                        暂无成员
                      </span>
                    )}
                    {s.members.map((m) => (
                      <Badge
                        key={m}
                        variant={m === current ? "default" : "secondary"}
                      >
                        {nameOf(m)}
                      </Badge>
                    ))}
                  </div>
                  {probeList && (
                    <div className="flex flex-wrap gap-2 pt-1">
                      {probeList.map((p) => (
                        <span
                          key={p.tag}
                          className={cn(
                            "text-xs",
                            p.ok ? "text-emerald-500" : "text-destructive",
                          )}
                        >
                          {nameOf(p.tag)}:{" "}
                          {p.ok ? `${p.latency_ms ?? 0} ms` : p.error ?? "失败"}
                        </span>
                      ))}
                    </div>
                  )}
                </div>

                <div className="flex shrink-0 items-center gap-2">
                  <Button
                    variant="ghost"
                    size="sm"
                    disabled={testing === s.tag}
                    onClick={() => runTest(s.tag)}
                  >
                    <Gauge className="size-4" />
                    {testing === s.tag ? "测速中…" : "测速"}
                  </Button>
                  <Select
                    value={current || undefined}
                    onValueChange={(v) =>
                      switchTo.mutate({ selector: s.tag, providerTag: v })
                    }
                    disabled={s.members.length === 0}
                  >
                    <SelectTrigger size="sm" className="w-40">
                      <SelectValue placeholder="切换到…" />
                    </SelectTrigger>
                    <SelectContent>
                      {s.members.map((m) => (
                        <SelectItem key={m} value={m}>
                          {nameOf(m)}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                  <Button
                    variant="ghost"
                    size="icon"
                    onClick={() => {
                      if (confirm(`确认删除选择器「${s.name}」？`))
                        remove.mutate(s.tag);
                    }}
                  >
                    <Trash2 className="size-4 text-destructive" />
                  </Button>
                </div>
              </div>
            );
          })
        )}
      </CardContent>

      <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>新建选择器</DialogTitle>
            <DialogDescription>
              从已启用的渠道中挑选成员，之后可一键热切换。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-4">
            <div className="grid grid-cols-2 gap-3">
              <div className="space-y-2">
                <Label>名称</Label>
                <Input
                  value={draft.name}
                  onChange={(e) => setDraft({ ...draft, name: e.target.value })}
                  placeholder="例如：主力池"
                />
              </div>
              <div className="space-y-2">
                <Label>Tag</Label>
                <Input
                  value={draft.tag}
                  onChange={(e) => setDraft({ ...draft, tag: e.target.value })}
                  placeholder="main-pool"
                />
              </div>
            </div>
            <div className="grid grid-cols-2 gap-3">
              <div className="space-y-2">
                <Label>模式</Label>
                <Select
                  value={draft.mode}
                  onValueChange={(v) =>
                    setDraft({ ...draft, mode: v as SelectorInput["mode"] })
                  }
                >
                  <SelectTrigger className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="selector">手动切换</SelectItem>
                    <SelectItem value="urltest">自动测速</SelectItem>
                  </SelectContent>
                </Select>
              </div>
              <div className="space-y-2">
                <Label>容差 (ms)</Label>
                <Input
                  type="number"
                  value={draft.tolerance_ms}
                  onChange={(e) =>
                    setDraft({ ...draft, tolerance_ms: Number(e.target.value) || 0 })
                  }
                />
              </div>
            </div>
            <div className="space-y-2">
              <Label>成员渠道</Label>
              {providers.length === 0 ? (
                <p className="text-muted-foreground text-xs">
                  还没有渠道，请先到「渠道管理」创建。
                </p>
              ) : (
                <div className="grid grid-cols-2 gap-2">
                  {providers.map((p) => (
                    <label
                      key={p.tag}
                      className="flex items-center gap-2 text-xs"
                    >
                      <Checkbox
                        checked={draft.members.includes(p.tag)}
                        onCheckedChange={(c) =>
                          setDraft({
                            ...draft,
                            members: c
                              ? [...draft.members, p.tag]
                              : draft.members.filter((m) => m !== p.tag),
                          })
                        }
                      />
                      {p.name} ({p.tag})
                    </label>
                  ))}
                </div>
              )}
            </div>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDialogOpen(false)}>
              取消
            </Button>
            <Button
              disabled={create.isPending}
              onClick={() => {
                if (!draft.tag.trim() || !draft.name.trim()) {
                  toast.error("请填写名称与 tag");
                  return;
                }
                create.mutate({ ...draft, tag: draft.tag.trim(), name: draft.name.trim() });
              }}
            >
              创建
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </Card>
  );
}
