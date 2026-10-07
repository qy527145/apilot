import { useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { CloudDownload, Loader2 } from "lucide-react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { qk } from "@/hooks/queries";
import {
  api,
  CATALOG_SOURCE_LABEL,
  type CapabilityImportStats,
  type CatalogSource,
} from "@/lib/api";

/**
 * 从上游模型目录导入能力标志。
 *
 * 和「实测」的分工：目录零成本、覆盖广，但只反映**模型宣称**的能力；
 * 实测花 token，反映**这条渠道实际**的行为（中转商可能把 tools 剥掉，
 * 自建模型可能根本不在目录里）。所以这个按钮解决"批量铺一遍底"，
 * 每行那三枚徽标解决"这一条准不准"。
 */
export function CatalogCapabilitiesDialog({
  open,
  onOpenChange,
  providerId,
  providerName,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  providerId: number;
  providerName: string;
}) {
  const qc = useQueryClient();
  const [source, setSource] = useState<CatalogSource>("models_dev");
  const [allProviders, setAllProviders] = useState(false);
  const [result, setResult] = useState<CapabilityImportStats | null>(null);

  const run = useMutation({
    mutationFn: () =>
      api.catalogCapabilitiesImport(source, allProviders ? undefined : providerId),
    onSuccess: (stats) => {
      setResult(stats);
      // 导入可能覆盖多个渠道，按前缀整片失效。
      qc.invalidateQueries({ queryKey: qk.capabilitiesAll });
      toast.success(`已导入 ${stats.written} 条能力判定`);
    },
  });

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        if (!o) setResult(null);
        onOpenChange(o);
      }}
    >
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>从目录导入能力</DialogTitle>
          <DialogDescription className="text-xs">
            读取公开模型目录里「支不支持思考 / 工具 / 多模态」的断言。
            零成本、不向你的渠道发任何请求。
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4">
          <div className="space-y-2">
            <Label htmlFor="cap-source">目录来源</Label>
            <Select
              value={source}
              onValueChange={(v) => setSource(v as CatalogSource)}
            >
              <SelectTrigger id="cap-source">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {(Object.keys(CATALOG_SOURCE_LABEL) as CatalogSource[]).map((s) => (
                  <SelectItem key={s} value={s}>
                    {CATALOG_SOURCE_LABEL[s]}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <p className="text-muted-foreground text-[11px]">
              两家覆盖不同：models.dev 收录更全更新（kimi / glm / qwen3-max 只有它有），
              LiteLLM 偏一手大厂。选中一个即可，不必两个都导 —— 后导的会覆盖前一个。
            </p>
          </div>

          <div className="flex items-start justify-between gap-3 rounded-md border p-3">
            <div className="space-y-0.5">
              <Label htmlFor="cap-scope">套用到所有渠道</Label>
              <p className="text-muted-foreground text-[11px]">
                关掉则只处理「{providerName}」。同一个模型名在不同中转后面表现可能
                不同，所以能力是按渠道各存一份的。
              </p>
            </div>
            <Switch
              id="cap-scope"
              checked={allProviders}
              onCheckedChange={setAllProviders}
            />
          </div>

          {result && (
            <div className="bg-muted/40 rounded-md p-3 text-xs">
              <div>
                处理了 {result.providers} 个渠道，写入 <b>{result.written}</b> 条判定。
              </div>
              {result.missing > 0 && (
                <div className="text-muted-foreground mt-1">
                  有 {result.missing} 个模型在目录里找不到（自建、中转或新发布），
                  它们的能力得靠每行的徽标实测。
                </div>
              )}
            </div>
          )}
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            关闭
          </Button>
          <Button onClick={() => run.mutate()} disabled={run.isPending}>
            {run.isPending ? (
              <Loader2 className="size-4 animate-spin" />
            ) : (
              <CloudDownload className="size-4" />
            )}
            {run.isPending ? "下载中…" : "开始导入"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
