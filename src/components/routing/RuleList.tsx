import { useEffect, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import {
  DndContext,
  PointerSensor,
  closestCenter,
  useSensor,
  useSensors,
  type DragEndEvent,
} from "@dnd-kit/core";
import {
  SortableContext,
  arrayMove,
  useSortable,
  verticalListSortingStrategy,
} from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { GripVertical, Pencil, Plus, Trash2 } from "lucide-react";
import { toast } from "sonner";

import { EmptyState } from "@/components/common/EmptyState";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import { qk } from "@/hooks/queries";
import { api, type RouteRule } from "@/lib/api";
import { cn } from "@/lib/utils";
import { summarizeAction, summarizeItem } from "@/components/routing/ruleSummary";

interface Props {
  rules: RouteRule[];
  isLoading: boolean;
  isError: boolean;
  onRetry: () => void;
  onCreate: () => void;
  onEdit: (rule: RouteRule) => void;
}

export function RuleList({
  rules,
  isLoading,
  isError,
  onRetry,
  onCreate,
  onEdit,
}: Props) {
  const qc = useQueryClient();
  const [items, setItems] = useState<RouteRule[]>(rules);
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 6 } }),
  );

  useEffect(() => setItems(rules), [rules]);

  const reorder = useMutation({
    mutationFn: (ids: number[]) => api.reorderRouteRules(ids),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.routeRules });
      toast.success("规则顺序已保存");
    },
    onError: () => qc.invalidateQueries({ queryKey: qk.routeRules }),
  });

  const toggle = useMutation({
    mutationFn: (rule: RouteRule) =>
      api.upsertRouteRule({
        id: rule.id,
        name: rule.name,
        enabled: !rule.enabled,
        items: rule.items,
        action: rule.action,
      }),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.routeRules }),
  });

  const remove = useMutation({
    mutationFn: (id: number) => api.deleteRouteRule(id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.routeRules });
      toast.success("规则已删除");
    },
  });

  const onDragEnd = (e: DragEndEvent) => {
    const { active, over } = e;
    if (!over || active.id === over.id) return;
    const oldIndex = items.findIndex((r) => r.id === active.id);
    const newIndex = items.findIndex((r) => r.id === over.id);
    if (oldIndex < 0 || newIndex < 0) return;
    const next = arrayMove(items, oldIndex, newIndex);
    setItems(next);
    reorder.mutate(next.map((r) => r.id));
  };

  return (
    <Card>
      <CardHeader className="flex-row items-center justify-between">
        <CardTitle className="text-sm">路由规则链（自上而下匹配）</CardTitle>
        <Button size="sm" variant="outline" onClick={onCreate}>
          <Plus className="size-4" />
          新建规则
        </Button>
      </CardHeader>
      <CardContent className="space-y-2">
        {isLoading ? (
          <>
            <Skeleton className="h-14 w-full" />
            <Skeleton className="h-14 w-full" />
            <Skeleton className="h-14 w-full" />
          </>
        ) : isError ? (
          <div className="py-6 text-center text-xs">
            <span className="text-destructive">加载规则失败。 </span>
            <button className="underline" onClick={onRetry}>
              重试
            </button>
          </div>
        ) : items.length === 0 ? (
          <EmptyState
            title="还没有路由规则"
            description="创建规则把请求按客户端、模型或协议分发到不同的选择器。"
            action={
              <Button size="sm" onClick={onCreate}>
                <Plus className="size-4" />
                新建规则
              </Button>
            }
          />
        ) : (
          <DndContext
            sensors={sensors}
            collisionDetection={closestCenter}
            onDragEnd={onDragEnd}
          >
            <SortableContext
              items={items.map((r) => r.id)}
              strategy={verticalListSortingStrategy}
            >
              {items.map((rule) => (
                <SortableRule
                  key={rule.id}
                  rule={rule}
                  onToggle={() => toggle.mutate(rule)}
                  onEdit={() => onEdit(rule)}
                  onDelete={() => {
                    if (confirm(`确认删除规则「${rule.name}」？`))
                      remove.mutate(rule.id);
                  }}
                />
              ))}
            </SortableContext>
          </DndContext>
        )}
      </CardContent>
    </Card>
  );
}

function SortableRule({
  rule,
  onToggle,
  onEdit,
  onDelete,
}: {
  rule: RouteRule;
  onToggle: () => void;
  onEdit: () => void;
  onDelete: () => void;
}) {
  const { attributes, listeners, setNodeRef, transform, transition, isDragging } =
    useSortable({ id: rule.id });

  const style = {
    transform: CSS.Transform.toString(transform),
    transition,
  };

  const condition =
    rule.items.length === 0
      ? "匹配全部请求（兜底）"
      : rule.items.map(summarizeItem).join(" 且 ");

  return (
    <div
      ref={setNodeRef}
      style={style}
      className={cn(
        "flex items-center gap-3 rounded-lg border bg-card p-3",
        isDragging && "opacity-70 shadow-lg",
      )}
    >
      <button
        className="text-muted-foreground cursor-grab touch-none"
        {...attributes}
        {...listeners}
      >
        <GripVertical className="size-4" />
      </button>

      <div className="min-w-0 flex-1 space-y-1">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-sm font-medium">{rule.name}</span>
          <Badge variant={rule.enabled ? "success" : "secondary"}>
            {rule.enabled ? "启用" : "停用"}
          </Badge>
          <Badge variant="outline">{summarizeAction(rule.action)}</Badge>
        </div>
        <p className="text-muted-foreground truncate text-xs" title={condition}>
          {condition}
        </p>
      </div>

      <div className="flex shrink-0 items-center gap-1">
        <Switch checked={rule.enabled} onCheckedChange={onToggle} />
        <Button variant="ghost" size="icon" onClick={onEdit}>
          <Pencil className="size-4" />
        </Button>
        <Button variant="ghost" size="icon" onClick={onDelete}>
          <Trash2 className="size-4 text-destructive" />
        </Button>
      </div>
    </div>
  );
}
