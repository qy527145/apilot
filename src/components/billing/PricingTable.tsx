import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, Save, Trash2 } from "lucide-react";
import { toast } from "sonner";

import { EmptyState } from "@/components/common/EmptyState";
import { TableSkeleton } from "@/components/common/StatCard";
import { Button } from "@/components/ui/button";
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
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { qk } from "@/hooks/queries";
import { api, type Pricing, type PricingInput } from "@/lib/api";

const FIELDS: { key: keyof PricingInput; label: string }[] = [
  { key: "model_ratio", label: "模型倍率" },
  { key: "completion_ratio", label: "输出倍率" },
  { key: "cache_ratio", label: "缓存读倍率" },
  { key: "cache_create_ratio", label: "缓存写倍率" },
  { key: "group_ratio", label: "分组倍率" },
  { key: "tool_call_surcharge", label: "工具调用附加费" },
];

type EditableRow = Record<keyof PricingInput, string>;

function toEditable(p: Pricing): EditableRow {
  return {
    model: p.model,
    model_ratio: String(p.model_ratio),
    completion_ratio: String(p.completion_ratio),
    cache_ratio: String(p.cache_ratio),
    cache_create_ratio: String(p.cache_create_ratio),
    group_ratio: String(p.group_ratio),
    image_ratio: String(p.image_ratio),
    audio_ratio: String(p.audio_ratio),
    tool_call_surcharge: String(p.tool_call_surcharge),
  };
}

export function PricingTable() {
  const qc = useQueryClient();
  const { data, isLoading } = useQuery({
    queryKey: qk.pricing,
    queryFn: api.listPricing,
    retry: 1,
  });

  const [rows, setRows] = useState<EditableRow[]>([]);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [draft, setDraft] = useState({ model: "", model_ratio: "1" });

  useEffect(() => {
    if (data) setRows(data.map(toEditable));
  }, [data]);

  const save = useMutation({
    mutationFn: (input: PricingInput) => api.upsertPricing(input),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.pricing });
      toast.success("单价系数已保存");
    },
  });

  const remove = useMutation({
    mutationFn: (model: string) => api.deletePricing(model),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.pricing });
      toast.success("单价已删除");
    },
  });

  const update = (idx: number, key: keyof PricingInput, value: string) => {
    setRows((rs) => rs.map((r, i) => (i === idx ? { ...r, [key]: value } : r)));
  };

  const commit = (row: EditableRow) => {
    if (!row.model.trim()) {
      toast.error("模型名不能为空");
      return;
    }
    const input: PricingInput = {
      model: row.model.trim(),
      model_ratio: Number(row.model_ratio) || 0,
      completion_ratio: Number(row.completion_ratio) || 0,
      cache_ratio: Number(row.cache_ratio) || 0,
      cache_create_ratio: Number(row.cache_create_ratio) || 0,
      group_ratio: Number(row.group_ratio) || 0,
      image_ratio: Number(row.image_ratio) || 0,
      audio_ratio: Number(row.audio_ratio) || 0,
      tool_call_surcharge: Number(row.tool_call_surcharge) || 0,
    };
    save.mutate(input);
  };

  const addNew = () => {
    if (!draft.model.trim()) {
      toast.error("请填写模型名");
      return;
    }
    save.mutate(
      {
        model: draft.model.trim(),
        model_ratio: Number(draft.model_ratio) || 1,
        completion_ratio: 1,
        cache_ratio: 1,
        cache_create_ratio: 1,
        group_ratio: 1,
        image_ratio: 1,
        audio_ratio: 1,
        tool_call_surcharge: 0,
      },
      {
        onSuccess: () => {
          setDialogOpen(false);
          setDraft({ model: "", model_ratio: "1" });
        },
      },
    );
  };

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold">单价系数配置</h2>
        <Button size="sm" variant="outline" onClick={() => setDialogOpen(true)}>
          <Plus className="size-4" />
          新增单价
        </Button>
      </div>

      {isLoading ? (
        <TableSkeleton rows={4} cols={7} />
      ) : rows.length === 0 ? (
        <EmptyState
          title="还没有单价配置"
          description="添加模型单价系数后，统计页的配额与费用才会按此计算。"
          action={
            <Button size="sm" onClick={() => setDialogOpen(true)}>
              <Plus className="size-4" />
              新增单价
            </Button>
          }
        />
      ) : (
        <div className="overflow-x-auto rounded-md border">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="min-w-[180px]">模型</TableHead>
                {FIELDS.map((f) => (
                  <TableHead key={f.key} className="min-w-[110px]">
                    {f.label}
                  </TableHead>
                ))}
                <TableHead className="text-right">操作</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {rows.map((row, idx) => (
                <TableRow key={row.model || idx}>
                  <TableCell>
                    <Input
                      value={row.model}
                      onChange={(e) => update(idx, "model", e.target.value)}
                    />
                  </TableCell>
                  {FIELDS.map((f) => (
                    <TableCell key={f.key}>
                      <Input
                        type="number"
                        step="0.01"
                        value={row[f.key]}
                        onChange={(e) => update(idx, f.key, e.target.value)}
                      />
                    </TableCell>
                  ))}
                  <TableCell>
                    <div className="flex items-center justify-end gap-1">
                      <Button
                        variant="ghost"
                        size="icon"
                        disabled={save.isPending}
                        onClick={() => commit(row)}
                      >
                        <Save className="size-4" />
                      </Button>
                      <Button
                        variant="ghost"
                        size="icon"
                        onClick={() => {
                          if (confirm(`确认删除「${row.model}」的单价？`))
                            remove.mutate(row.model);
                        }}
                      >
                        <Trash2 className="size-4 text-destructive" />
                      </Button>
                    </div>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      )}

      <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>新增单价</DialogTitle>
            <DialogDescription>
              其余倍率将默认设为 1，创建后可在表格内继续修改。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-3">
            <div className="space-y-2">
              <Label>模型名</Label>
              <Input
                value={draft.model}
                onChange={(e) => setDraft({ ...draft, model: e.target.value })}
                placeholder="例如：claude-3-5-sonnet"
              />
            </div>
            <div className="space-y-2">
              <Label>模型倍率</Label>
              <Input
                type="number"
                step="0.01"
                value={draft.model_ratio}
                onChange={(e) =>
                  setDraft({ ...draft, model_ratio: e.target.value })
                }
              />
            </div>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDialogOpen(false)}>
              取消
            </Button>
            <Button onClick={addNew} disabled={save.isPending}>
              创建
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
