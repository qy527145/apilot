import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { CloudDownload, Plus, Trash2 } from "lucide-react";
import { toast } from "sonner";

import { ModelPickerDialog } from "@/components/providers/ModelPickerDialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { qk } from "@/hooks/queries";
import { api } from "@/lib/api";

interface Row {
  model: string;
  upstream_model: string;
}

/** 该行实际会发给上游的模型名：留空表示与入站名同名。 */
const effectiveUpstream = (r: Row) => (r.upstream_model || r.model).trim();

/** 渠道行展开后的模型映射编辑器：入站模型名 → 上游模型名 */
export function ModelMappingPanel({
  providerId,
  providerName,
}: {
  providerId: number;
  providerName: string;
}) {
  const qc = useQueryClient();
  const { data, isLoading, isError, refetch } = useQuery({
    queryKey: qk.providerModels(providerId),
    queryFn: () => api.listProviderModels(providerId),
    retry: 1,
  });

  const [rows, setRows] = useState<Row[]>([]);
  const [pickerOpen, setPickerOpen] = useState(false);

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
      // 保存会把映射同步进 providers.model_mapping，渠道对话框拿的是旧快照；
      // 不刷新的话，之后在渠道对话框点保存会用旧映射把它覆盖回去。
      qc.invalidateQueries({ queryKey: qk.providers });
      // 接管页的就绪判定取决于"有没有声明过模型"，改完得让它重新问一次后端。
      qc.invalidateQueries({ queryKey: qk.takeoverReadiness });
      toast.success("模型映射已保存");
    },
  });

  const update = (idx: number, key: keyof Row, value: string) => {
    setRows((rs) => rs.map((r, i) => (i === idx ? { ...r, [key]: value } : r)));
  };

  const selectedUpstreams = useMemo(
    () => rows.map(effectiveUpstream).filter(Boolean),
    [rows],
  );

  /**
   * 把下拉选择器的结果合并进当前行。
   *
   * `known` 是本次从上游拉到的模型全集 —— 只有落在这个集合里的行才归选择器管：
   * 手工敲进去的、或上游已经下架的模型名不在集合里，一律原样保留，
   * 免得"点一下应用选择"就把用户手填的行删掉。
   */
  const applyPicked = (picked: string[], known: string[]) => {
    const pickedSet = new Set(picked);
    const knownSet = new Set(known);

    setRows((rs) => {
      const kept = rs.filter((r) => {
        const eff = effectiveUpstream(r);
        if (!eff || !knownSet.has(eff)) return true;
        return pickedSet.has(eff);
      });

      const have = new Set(kept.map(effectiveUpstream).filter(Boolean));
      const added: Row[] = picked
        .filter((id) => !have.has(id))
        .map((id) => ({ model: id, upstream_model: "" }));

      const next = [
        ...kept.filter((r) => r.model.trim() || effectiveUpstream(r)),
        ...added,
      ];
      // 不能留下空列表：面板没有"零行"状态，用户会以为界面坏了。
      return next.length > 0 ? next : [{ model: "", upstream_model: "" }];
    });
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
        <div className="flex items-center gap-2">
          <Button
            variant="outline"
            size="sm"
            onClick={() => setPickerOpen(true)}
          >
            <CloudDownload className="size-4" />
            从上游获取
          </Button>
          <Button
            size="sm"
            onClick={() => save.mutate()}
            disabled={save.isPending}
          >
            {save.isPending ? "保存中…" : "保存映射"}
          </Button>
        </div>
      </div>

      <p className="text-muted-foreground text-xs">
        左列是客户端发来的模型名，右列是该渠道真正接受的模型名。
        右列留空表示同名。没声明任何模型的渠道会被视为「通吃」，任何模型名都会转发过去。
      </p>

      <div className="space-y-2">
        {rows.map((r, idx) => (
          <div key={idx} className="flex items-center gap-2">
            <Input
              value={r.model}
              placeholder="入站模型名，例如 claude-sonnet-4-5"
              onChange={(e) => update(idx, "model", e.target.value)}
              className="flex-1"
            />
            <span className="text-muted-foreground text-xs">→</span>
            <Input
              value={r.upstream_model}
              placeholder="上游模型名（留空表示同名）"
              onChange={(e) => update(idx, "upstream_model", e.target.value)}
              className="flex-1"
            />
            <Button
              variant="ghost"
              size="icon"
              onClick={() =>
                setRows((rs) => {
                  const next = rs.filter((_, i) => i !== idx);
                  return next.length > 0
                    ? next
                    : [{ model: "", upstream_model: "" }];
                })
              }
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

      <ModelPickerDialog
        open={pickerOpen}
        onOpenChange={setPickerOpen}
        providerId={providerId}
        providerName={providerName}
        selected={selectedUpstreams}
        onConfirm={applyPicked}
      />
    </div>
  );
}
