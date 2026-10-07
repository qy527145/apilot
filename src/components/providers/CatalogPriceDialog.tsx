import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Check, Loader2, RefreshCw, X } from "lucide-react";
import { toast } from "sonner";

import { Badge } from "@/components/ui/badge";
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
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { qk } from "@/hooks/queries";
import {
  api,
  CATALOG_SOURCE_LABEL,
  PRICE_ACTION_LABEL,
  type CatalogSource,
  type PriceAction,
} from "@/lib/api";

/** 每种处置的颜色。跳过用户手填的行要显眼 —— 那是这个功能最容易被误解的地方。 */
const ACTION_VARIANT: Record<
  PriceAction,
  "success" | "warning" | "destructive" | "secondary" | "outline"
> = {
  insert: "success",
  update: "warning",
  keep_user_owned: "secondary",
  unchanged: "outline",
};

/**
 * 从公开模型目录更新单价。
 *
 * **只覆盖「目录导进来的」那些行，用户手填的一律不动** —— 否则用户精心按自己
 * 的渠道折扣调完价，下次点一下更新就全白调了。预览里专门把"跳过（手填）"
 * 列出来，就是为了让这条规则看得见，而不是等用户发现价格被改了才去猜。
 *
 * 后端在真正写入时会**重新算一遍差异**，而不是信这里显示的行：预览和应用之间
 * 可能隔了几分钟，期间用户可能手改过某行，按旧差异写下去会把改动覆盖掉。
 */
export function CatalogPriceDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const qc = useQueryClient();
  const [source, setSource] = useState<CatalogSource>("models_dev");
  const [refresh, setRefresh] = useState(false);

  const preview = useQuery({
    queryKey: ["catalog_price_preview", source, refresh],
    queryFn: () => api.catalogPricePreview(source, refresh),
    enabled: open,
    // 目录本身按天更新，同一来源短时间内没必要反复下。
    staleTime: 5 * 60_000,
    retry: false,
  });

  const apply = useMutation({
    mutationFn: () => api.catalogPriceApply(source),
    onSuccess: (stats) => {
      qc.invalidateQueries({ queryKey: qk.pricing });
      toast.success(
        `已更新价格：新增 ${stats.inserted}、覆盖 ${stats.updated}、` +
          `跳过手填 ${stats.kept_user_owned}`,
      );
      onOpenChange(false);
    },
  });

  const stats = preview.data?.stats;
  /** 没变化的行不必占地方 —— 目录里几千个模型，绝大多数是没变的。 */
  const interesting = (preview.data?.rows ?? []).filter(
    (r) => r.action !== "unchanged",
  );
  const changedCount = interesting.filter(
    (r) => r.action === "insert" || r.action === "update",
  ).length;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[85vh] sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>从目录更新价格</DialogTitle>
          <DialogDescription className="text-xs">
            只更新本项目中<b>已经声明过模型</b>的渠道价格。手填的价格不会被覆盖。
          </DialogDescription>
        </DialogHeader>

        <div className="flex flex-wrap items-end gap-3">
          <div className="min-w-40 flex-1 space-y-2">
            <Label htmlFor="price-source">目录来源</Label>
            <Select
              value={source}
              onValueChange={(v) => setSource(v as CatalogSource)}
            >
              <SelectTrigger id="price-source">
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
          </div>
          <Button
            variant="outline"
            onClick={() => setRefresh((v) => !v)}
            disabled={preview.isFetching}
          >
            <RefreshCw
              className={`size-4 ${preview.isFetching ? "animate-spin" : ""}`}
            />
            重新抓取
          </Button>
        </div>

        {preview.isError ? (
          <div className="text-destructive rounded-md border p-3 text-xs">
            拉取目录失败。这个域名在部分网络下需要走代理 ——
            检查「设置」里的出站代理，或换个来源再试。
          </div>
        ) : preview.isLoading ? (
          <div className="flex items-center gap-2 p-6 text-xs">
            <Loader2 className="size-4 animate-spin" />
            正在下载并比对…
          </div>
        ) : (
          <>
            <div className="text-muted-foreground flex flex-wrap gap-x-4 gap-y-1 text-xs">
              <span>
                目录 <b className="text-foreground">{preview.data?.total}</b> 个模型
              </span>
              <span>
                本项目用得到且有价的{" "}
                <b className="text-foreground">{preview.data?.priced}</b> 个
              </span>
              <span>
                将新增 <b className="text-foreground">{stats?.inserted ?? 0}</b>
                {"、覆盖 "}
                <b className="text-foreground">{stats?.updated ?? 0}</b>
              </span>
              <span>
                跳过手填{" "}
                <b className="text-foreground">{stats?.kept_user_owned ?? 0}</b>
              </span>
            </div>

            <ScrollArea className="h-72 rounded-md border">
              {interesting.length === 0 ? (
                <div className="text-muted-foreground p-4 text-xs">
                  没有需要改动的价格 —— 目录里的价和你库里的已经一致。
                </div>
              ) : (
                <table className="w-full text-xs">
                  <thead className="bg-muted/50 sticky top-0">
                    <tr className="text-muted-foreground [&>th]:px-2 [&>th]:py-1.5 [&>th]:text-left [&>th]:font-medium">
                      <th>模型</th>
                      <th>处置</th>
                      <th className="text-right">现在</th>
                      <th className="text-right">目录</th>
                    </tr>
                  </thead>
                  <tbody>
                    {interesting.map((row) => (
                      <tr key={row.model} className="border-t [&>td]:px-2 [&>td]:py-1">
                        <td className="max-w-0 truncate font-mono" title={row.model}>
                          {row.model}
                        </td>
                        <td>
                          <Badge variant={ACTION_VARIANT[row.action]}>
                            {PRICE_ACTION_LABEL[row.action]}
                          </Badge>
                        </td>
                        <td className="text-muted-foreground text-right tabular-nums">
                          {row.current ? row.current.model_ratio.toFixed(3) : "—"}
                        </td>
                        <td className="text-right tabular-nums">
                          {row.incoming.model_ratio.toFixed(3)}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              )}
            </ScrollArea>

            <p className="text-muted-foreground text-[11px]">
              「倍率」是 Apilot 的计费单位：1.0 对应每 100 万输入 token 2 美元，
              所以 3 美元/100 万会折成 1.5。两家目录对个别模型（如 DeepSeek）
              报价并不一致，换来源会改变这些数字。
            </p>
          </>
        )}

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            <X className="size-4" />
            取消
          </Button>
          <Button
            onClick={() => apply.mutate()}
            disabled={apply.isPending || !preview.data || changedCount === 0}
          >
            {apply.isPending ? (
              <Loader2 className="size-4 animate-spin" />
            ) : (
              <Check className="size-4" />
            )}
            应用 {changedCount} 处改动
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
