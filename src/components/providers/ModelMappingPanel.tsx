import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, Trash2 } from "lucide-react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { qk } from "@/hooks/queries";
import { api, type ProviderModel } from "@/lib/api";

/** 渠道行展开后的模型映射编辑器：入站模型名 → 上游模型名 */
export function ModelMappingPanel({ providerId }: { providerId: number }) {
  const qc = useQueryClient();
  const { data, isLoading, isError, refetch } = useQuery({
    queryKey: qk.providerModels(providerId),
    queryFn: () => api.listProviderModels(providerId),
    retry: 1,
  });

  const [rows, setRows] = useState<ProviderModel[]>([]);

  useEffect(() => {
    if (data) {
      setRows(
        data.length > 0
          ? data.map((m) => ({ model: m.model, upstream_model: m.upstream_model ?? "" }))
          : [{ model: "", upstream_model: "" }],
      );
    }
  }, [data]);

  const save = useMutation({
    mutationFn: () => {
      const cleaned = rows
        .filter((r) => r.model.trim())
        .map((r) => ({
          model: r.model.trim(),
          upstream_model: r.upstream_model?.trim() ? r.upstream_model.trim() : null,
        }));
      return api.setProviderModels(providerId, cleaned);
    },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.providerModels(providerId) });
      toast.success("模型映射已保存");
    },
  });

  const update = (idx: number, key: keyof ProviderModel, value: string) => {
    setRows((rs) => rs.map((r, i) => (i === idx ? { ...r, [key]: value } : r)));
  };

  if (isLoading) {
    return (
      <div className="space-y-2 p-4">
        <Skeleton className="h-8 w-full" />
        <Skeleton className="h-8 w-full" />
      </div>
    );
  }

  if (isError) {
    return (
      <div className="p-4 text-xs">
        <span className="text-destructive">加载模型映射失败。 </span>
        <button className="underline" onClick={() => refetch()}>
          重试
        </button>
      </div>
    );
  }

  return (
    <div className="bg-muted/30 space-y-3 rounded-md p-4">
      <div className="flex items-center justify-between">
        <p className="text-xs font-medium">模型映射（入站 → 上游）</p>
        <Button
          size="sm"
          onClick={() => save.mutate()}
          disabled={save.isPending}
        >
          {save.isPending ? "保存中…" : "保存映射"}
        </Button>
      </div>

      <div className="space-y-2">
        {rows.map((r, idx) => (
          <div key={idx} className="flex items-center gap-2">
            <Input
              value={r.model}
              placeholder="入站模型名，例如 claude-3-5-sonnet"
              onChange={(e) => update(idx, "model", e.target.value)}
              className="flex-1"
            />
            <span className="text-muted-foreground text-xs">→</span>
            <Input
              value={r.upstream_model ?? ""}
              placeholder="上游模型名（留空表示同名）"
              onChange={(e) => update(idx, "upstream_model", e.target.value)}
              className="flex-1"
            />
            <Button
              variant="ghost"
              size="icon"
              onClick={() => setRows((rs) => rs.filter((_, i) => i !== idx))}
            >
              <Trash2 className="size-4 text-destructive" />
            </Button>
          </div>
        ))}
      </div>

      <Button
        variant="outline"
        size="sm"
        onClick={() => setRows((rs) => [...rs, { model: "", upstream_model: "" }])}
      >
        <Plus className="size-4" />
        添加映射
      </Button>
    </div>
  );
}
