import { useEffect, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { ChevronDown, Plus, Trash2 } from "lucide-react";
import { toast } from "sonner";

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
import { api, type Protocol, type RouteAction, type RouteRule, type RouteRuleInput, type RuleItem, type Selector } from "@/lib/api";
import {
  ACTION_TYPE_LABELS,
  ITEM_TYPE_LABELS,
  PROTOCOLS,
  emptyAction,
  emptyItem,
  summarizeAction,
} from "@/components/routing/ruleSummary";

/* ---------------------------- 字符串列表输入 ---------------------------- */

function StringListInput({
  value,
  onChange,
  placeholder,
}: {
  value: string[];
  onChange: (v: string[]) => void;
  placeholder?: string;
}) {
  const [text, setText] = useState(value.join(", "));
  useEffect(() => setText(value.join(", ")), [value]);
  return (
    <Input
      value={text}
      placeholder={placeholder}
      onChange={(e) => {
        setText(e.target.value);
        onChange(
          e.target.value
            .split(",")
            .map((s) => s.trim())
            .filter(Boolean),
        );
      }}
    />
  );
}

/* ------------------------------ 单条条件编辑 ------------------------------ */

function RuleItemEditor({
  item,
  onChange,
  onRemove,
  depth = 0,
}: {
  item: RuleItem;
  onChange: (next: RuleItem) => void;
  onRemove: () => void;
  depth?: number;
}) {
  return (
    <div className="bg-muted/30 space-y-2 rounded-md border p-3">
      <div className="flex items-center justify-between gap-2">
        <span className="text-xs font-medium">
          {ITEM_TYPE_LABELS[item.type]}
        </span>
        <Button variant="ghost" size="icon" className="size-6" onClick={onRemove}>
          <Trash2 className="size-3.5 text-destructive" />
        </Button>
      </div>

      {item.type === "client" && (
        <StringListInput
          value={item.any}
          onChange={(any) => onChange({ type: "client", any })}
          placeholder="客户端 id，逗号分隔：claude-code, codex"
        />
      )}

      {item.type === "model" && (
        <StringListInput
          value={item.patterns}
          onChange={(patterns) => onChange({ type: "model", patterns })}
          placeholder="glob，逗号分隔：claude-*, gpt-4o"
        />
      )}

      {item.type === "path" && (
        <StringListInput
          value={item.prefixes}
          onChange={(prefixes) => onChange({ type: "path", prefixes })}
          placeholder="路径前缀，逗号分隔：/v1/messages"
        />
      )}

      {item.type === "protocol" && (
        <div className="flex flex-wrap gap-3">
          {PROTOCOLS.map((p) => (
            <label key={p} className="flex items-center gap-1.5 text-xs">
              <Checkbox
                checked={item.any.includes(p)}
                onCheckedChange={(c) =>
                  onChange({
                    type: "protocol",
                    any: c
                      ? [...item.any, p]
                      : item.any.filter((x: Protocol) => x !== p),
                  })
                }
              />
              {p}
            </label>
          ))}
        </div>
      )}

      {item.type === "header" && (
        <div className="flex items-center gap-2">
          <Input
            value={item.name}
            placeholder="Header 名"
            onChange={(e) =>
              onChange({ type: "header", name: e.target.value, equals: item.equals })
            }
          />
          <span className="text-muted-foreground text-xs">=</span>
          <Input
            value={item.equals ?? ""}
            placeholder="值（留空表示只判断存在）"
            onChange={(e) =>
              onChange({
                type: "header",
                name: item.name,
                equals: e.target.value || null,
              })
            }
          />
        </div>
      )}

      {item.type === "token_estimate" && (
        <div className="flex items-center gap-2">
          <Input
            type="number"
            value={item.min ?? ""}
            placeholder="最小 token"
            onChange={(e) =>
              onChange({
                type: "token_estimate",
                min: e.target.value === "" ? null : Number(e.target.value),
                max: item.max ?? null,
              })
            }
          />
          <span className="text-muted-foreground text-xs">~</span>
          <Input
            type="number"
            value={item.max ?? ""}
            placeholder="最大 token"
            onChange={(e) =>
              onChange({
                type: "token_estimate",
                min: item.min ?? null,
                max: e.target.value === "" ? null : Number(e.target.value),
              })
            }
          />
        </div>
      )}

      {item.type === "logical" && (
        <div className="space-y-2">
          <div className="flex items-center gap-3">
            <Select
              value={item.mode}
              onValueChange={(v) =>
                onChange({ ...item, mode: v as "and" | "or" })
              }
            >
              <SelectTrigger size="sm" className="w-24">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="and">全部满足 (AND)</SelectItem>
                <SelectItem value="or">任一满足 (OR)</SelectItem>
              </SelectContent>
            </Select>
            <label className="flex items-center gap-1.5 text-xs">
              <Checkbox
                checked={item.invert}
                onCheckedChange={(c) => onChange({ ...item, invert: !!c })}
              />
              取反 (NOT)
            </label>
          </div>

          <div className="space-y-2 border-l-2 pl-3">
            {item.rules.map((r, i) => (
              <RuleItemEditor
                key={i}
                item={r}
                depth={depth + 1}
                onChange={(next) =>
                  onChange({
                    ...item,
                    rules: item.rules.map((x, xi) => (xi === i ? next : x)),
                  })
                }
                onRemove={() =>
                  onChange({
                    ...item,
                    rules: item.rules.filter((_, xi) => xi !== i),
                  })
                }
              />
            ))}
            <AddItemMenu
              onAdd={(t) => onChange({ ...item, rules: [...item.rules, emptyItem(t)] })}
            />
          </div>
        </div>
      )}
    </div>
  );
}

/* ------------------------------ 添加条件菜单 ------------------------------ */

function AddItemMenu({ onAdd }: { onAdd: (t: RuleItem["type"]) => void }) {
  const [open, setOpen] = useState(false);
  const types = Object.keys(ITEM_TYPE_LABELS) as RuleItem["type"][];
  return (
    <div className="relative">
      <Button
        type="button"
        variant="outline"
        size="sm"
        onClick={() => setOpen((o) => !o)}
      >
        <Plus className="size-4" />
        添加条件
        <ChevronDown className="size-3" />
      </Button>
      {open && (
        <div className="bg-popover absolute z-20 mt-1 w-44 rounded-md border p-1 shadow-md">
          {types.map((t) => (
            <button
              key={t}
              type="button"
              className="hover:bg-accent flex w-full items-center rounded-sm px-2 py-1.5 text-left text-xs"
              onClick={() => {
                onAdd(t);
                setOpen(false);
              }}
            >
              {ITEM_TYPE_LABELS[t]}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

/* ------------------------------ 动作编辑 ------------------------------ */

function ActionEditor({
  action,
  onChange,
  selectors,
}: {
  action: RouteAction;
  onChange: (a: RouteAction) => void;
  selectors: Selector[];
}) {
  const types = Object.keys(ACTION_TYPE_LABELS) as RouteAction["type"][];
  return (
    <div className="space-y-2">
      <Select
        value={action.type}
        onValueChange={(v) => onChange(emptyAction(v as RouteAction["type"]))}
      >
        <SelectTrigger className="w-full">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {types.map((t) => (
            <SelectItem key={t} value={t}>
              {ACTION_TYPE_LABELS[t]}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>

      {action.type === "final" && (
        <Select
          value={action.selector || "__none__"}
          onValueChange={(v) =>
            onChange({ type: "final", selector: v === "__none__" ? "" : v })
          }
        >
          <SelectTrigger className="w-full">
            <SelectValue placeholder="选择目标选择器" />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="__none__">（请选择）</SelectItem>
            {selectors.map((s) => (
              <SelectItem key={s.tag} value={s.tag}>
                {s.name} ({s.tag})
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      )}

      {action.type === "reject" && (
        <Input
          value={action.reason}
          placeholder="拒绝原因"
          onChange={(e) => onChange({ type: "reject", reason: e.target.value })}
        />
      )}

      {action.type === "model_override" && (
        <Input
          value={action.model}
          placeholder="改写的模型名"
          onChange={(e) =>
            onChange({ type: "model_override", model: e.target.value })
          }
        />
      )}

      {action.type === "route_options" && (
        <div className="flex items-center gap-2">
          <Select
            value={action.target_selector ?? "__none__"}
            onValueChange={(v) =>
              onChange({
                ...action,
                target_selector: v === "__none__" ? null : v,
              })
            }
          >
            <SelectTrigger className="flex-1">
              <SelectValue placeholder="目标选择器（可选）" />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="__none__">（不指定）</SelectItem>
              {selectors.map((s) => (
                <SelectItem key={s.tag} value={s.tag}>
                  {s.name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Select
            value={
              action.cache === null || action.cache === undefined
                ? "inherit"
                : action.cache
                  ? "on"
                  : "off"
            }
            onValueChange={(v) =>
              onChange({
                ...action,
                cache: v === "inherit" ? null : v === "on",
              })
            }
          >
            <SelectTrigger className="w-32">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="inherit">缓存继承</SelectItem>
              <SelectItem value="on">缓存开启</SelectItem>
              <SelectItem value="off">缓存关闭</SelectItem>
            </SelectContent>
          </Select>
        </div>
      )}

      {action.type === "sniff" && (
        <p className="text-muted-foreground text-xs">
          仅记录请求特征，不改变路由结果，继续匹配后续规则。
        </p>
      )}
    </div>
  );
}

/* ------------------------------ 规则编辑器 ------------------------------ */

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  rule?: RouteRule | null;
  selectors: Selector[];
}

export function RuleEditor({ open, onOpenChange, rule, selectors }: Props) {
  const qc = useQueryClient();
  const [name, setName] = useState("");
  const [enabled, setEnabled] = useState(true);
  const [items, setItems] = useState<RuleItem[]>([]);
  const [action, setAction] = useState<RouteAction>({ type: "final", selector: "" });
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (open) {
      setName(rule?.name ?? "");
      setEnabled(rule?.enabled ?? true);
      setItems(rule?.items ?? []);
      setAction(rule?.action ?? { type: "final", selector: "" });
      setError(null);
    }
  }, [open, rule]);

  const mutation = useMutation({
    mutationFn: (input: RouteRuleInput) => api.upsertRouteRule(input),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.routeRules });
      toast.success(rule ? "规则已更新" : "规则已创建");
      onOpenChange(false);
    },
  });

  const submit = () => {
    if (!name.trim()) return setError("请填写规则名称");
    const input: RouteRuleInput = {
      id: rule?.id ?? null,
      name: name.trim(),
      enabled,
      items,
      action,
    };
    setError(null);
    mutation.mutate(input);
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>{rule ? "编辑路由规则" : "新建路由规则"}</DialogTitle>
          <DialogDescription>
            规则按顺序自上而下匹配，命中后根据动作决定是否继续。
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4">
          <div className="flex items-end gap-3">
            <div className="flex-1 space-y-2">
              <Label>规则名称</Label>
              <Input
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="例如：Claude Code 走 anthropic 池"
              />
            </div>
            <label className="flex items-center gap-2 pb-2 text-sm">
              <Switch checked={enabled} onCheckedChange={setEnabled} />
              启用
            </label>
          </div>

          <div className="space-y-2">
            <Label>匹配条件（全部满足）</Label>
            {items.length === 0 && (
              <p className="text-muted-foreground text-xs">
                未设置条件时表示无条件匹配（可作为兜底规则）。
              </p>
            )}
            <div className="space-y-2">
              {items.map((item, i) => (
                <RuleItemEditor
                  key={i}
                  item={item}
                  onChange={(next) =>
                    setItems((arr) => arr.map((x, xi) => (xi === i ? next : x)))
                  }
                  onRemove={() =>
                    setItems((arr) => arr.filter((_, xi) => xi !== i))
                  }
                />
              ))}
            </div>
            <AddItemMenu onAdd={(t) => setItems((arr) => [...arr, emptyItem(t)])} />
          </div>

          <div className="space-y-2">
            <Label>动作</Label>
            <ActionEditor action={action} onChange={setAction} selectors={selectors} />
            <p className="text-muted-foreground text-xs">
              预览：{summarizeAction(action)}
            </p>
          </div>

          {error && <p className="text-destructive text-xs">{error}</p>}
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            取消
          </Button>
          <Button onClick={submit} disabled={mutation.isPending}>
            {mutation.isPending ? "保存中…" : "保存"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
