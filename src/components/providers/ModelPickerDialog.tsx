import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { RefreshCw, Search } from "lucide-react";

import { Button } from "@/components/ui/button";
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
import { ScrollArea } from "@/components/ui/scroll-area";
import { Skeleton } from "@/components/ui/skeleton";
import { api } from "@/lib/api";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  providerId: number;
  providerName: string;
  /** 当前映射里已经在用的上游模型名，用于预勾选。 */
  selected: string[];
  /** `picked` 是勾选的上游模型名，`known` 是本次拉到的全集（供调用方判断哪些行归选择器管）。 */
  onConfirm: (picked: string[], known: string[]) => void;
}

/**
 * 从上游 `GET /v1/models` 拉模型列表，让用户勾选要使用的模型。
 *
 * 这里只负责"选哪些上游模型"，不碰入站名 —— 入站名由面板左列决定，
 * 因为只有用户知道客户端会发什么名字（Claude Code 发的是 claude-*）。
 */
export function ModelPickerDialog({
  open,
  onOpenChange,
  providerId,
  providerName,
  selected,
  onConfirm,
}: Props) {
  const [search, setSearch] = useState("");
  const [checked, setChecked] = useState<Set<string>>(new Set());

  const { data, isLoading, isError, error, refetch, isFetching } = useQuery({
    queryKey: ["remote_models", providerId],
    queryFn: () => api.fetchProviderModels(providerId),
    enabled: open,
    // 模型列表变动很慢，同一渠道短时间内反复打开不必重打上游。
    staleTime: 5 * 60_000,
    retry: false,
  });

  // 只在「打开」这一刻用当时的已选集合初始化。刻意不把 selected 放进依赖：
  // 父组件每次重渲染都会给它一个新数组，那样会把用户在弹窗里的勾选冲掉。
  useEffect(() => {
    if (open) {
      setChecked(new Set(selected));
      setSearch("");
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const models = useMemo(() => data ?? [], [data]);
  const filtered = useMemo(() => {
    const q = search.trim().toLowerCase();
    return q ? models.filter((m) => m.toLowerCase().includes(q)) : models;
  }, [models, search]);

  const toggle = (id: string) => {
    setChecked((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const allFilteredChecked =
    filtered.length > 0 && filtered.every((m) => checked.has(m));

  const toggleAllFiltered = () => {
    setChecked((prev) => {
      const next = new Set(prev);
      if (allFilteredChecked) filtered.forEach((m) => next.delete(m));
      else filtered.forEach((m) => next.add(m));
      return next;
    });
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[85vh] sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>从上游获取模型</DialogTitle>
          <DialogDescription>
            读取「{providerName}」的 /v1/models。勾选后的模型会出现在映射列表里，
            再用左列指定客户端会发来的模型名。
          </DialogDescription>
        </DialogHeader>

        {isLoading ? (
          <div className="space-y-2">
            <Skeleton className="h-9 w-full" />
            <Skeleton className="h-40 w-full" />
          </div>
        ) : isError ? (
          <div className="space-y-3 rounded-md border border-destructive/40 bg-destructive/5 p-3">
            <p className="text-destructive text-xs">
              {error instanceof Error ? error.message : "拉取模型列表失败"}
            </p>
            <Button
              variant="outline"
              size="sm"
              onClick={() => refetch()}
              disabled={isFetching}
            >
              <RefreshCw className="size-4" />
              重试
            </Button>
          </div>
        ) : models.length === 0 ? (
          <p className="text-muted-foreground py-6 text-center text-sm">
            上游没有返回任何模型。可以在映射面板里手工添加模型名。
          </p>
        ) : (
          <>
            <div className="flex items-center gap-2">
              <div className="relative flex-1">
                <Search className="text-muted-foreground absolute left-2.5 top-2.5 size-4" />
                <Input
                  value={search}
                  onChange={(e) => setSearch(e.target.value)}
                  placeholder="搜索模型名"
                  className="pl-8"
                />
              </div>
              <Button variant="outline" size="sm" onClick={toggleAllFiltered}>
                {allFilteredChecked ? "取消全选" : "全选"}
              </Button>
              <Button
                variant="ghost"
                size="sm"
                onClick={() => setChecked(new Set())}
              >
                清空
              </Button>
            </div>

            <ScrollArea className="h-64 rounded-md border">
              <div className="p-1">
                {filtered.length === 0 ? (
                  <p className="text-muted-foreground p-4 text-center text-xs">
                    没有匹配「{search}」的模型
                  </p>
                ) : (
                  filtered.map((id) => (
                    <label
                      key={id}
                      className="hover:bg-muted/50 flex cursor-pointer items-center gap-2 rounded px-2 py-1.5"
                    >
                      <Checkbox
                        checked={checked.has(id)}
                        onCheckedChange={() => toggle(id)}
                      />
                      <span className="truncate font-mono text-xs">{id}</span>
                    </label>
                  ))
                )}
              </div>
            </ScrollArea>

            <p className="text-muted-foreground text-xs">
              已选 {checked.size} / 共 {models.length}
            </p>
          </>
        )}

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            取消
          </Button>
          <Button
            onClick={() => {
              // `checked` 里可能残留上一批模型（预勾选来自面板，面板不认识上游的下架情况），
              // 按本次拉到的全集过滤一次，避免把已经不存在的模型写成映射行。
              const known = new Set(models);
              onConfirm([...checked].filter((id) => known.has(id)), models);
              onOpenChange(false);
            }}
            disabled={isLoading || isError}
          >
            应用选择
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
