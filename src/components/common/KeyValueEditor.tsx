import { Plus, Trash2 } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

interface KeyValueEditorProps {
  value: Record<string, string>;
  onChange: (next: Record<string, string>) => void;
  keyPlaceholder?: string;
  valuePlaceholder?: string;
  addLabel?: string;
  emptyLabel?: string;
  disabled?: boolean;
}

/** 通用的键值对编辑器（模型映射 / 额外请求头等） */
export function KeyValueEditor({
  value,
  onChange,
  keyPlaceholder = "键",
  valuePlaceholder = "值",
  addLabel = "添加一项",
  emptyLabel = "暂无条目",
  disabled,
}: KeyValueEditorProps) {
  const rows = Object.entries(value);

  const update = (idx: number, k: string, v: string) => {
    const entries = [...rows];
    entries[idx] = [k, v];
    onChange(Object.fromEntries(entries));
  };

  const remove = (idx: number) => {
    const entries = rows.filter((_, i) => i !== idx);
    onChange(Object.fromEntries(entries));
  };

  const add = () => onChange({ ...value, "": "" });

  return (
    <div className="space-y-2">
      {rows.length === 0 && (
        <p className="text-muted-foreground text-xs">{emptyLabel}</p>
      )}
      {rows.map(([k, v], idx) => (
        <div key={idx} className="flex items-center gap-2">
          <Input
            value={k}
            disabled={disabled}
            placeholder={keyPlaceholder}
            onChange={(e) => update(idx, e.target.value, v)}
            className="flex-1"
          />
          <span className="text-muted-foreground text-xs">→</span>
          <Input
            value={v}
            disabled={disabled}
            placeholder={valuePlaceholder}
            onChange={(e) => update(idx, k, e.target.value)}
            className="flex-1"
          />
          <Button
            type="button"
            variant="ghost"
            size="icon"
            disabled={disabled}
            onClick={() => remove(idx)}
          >
            <Trash2 className="size-4 text-destructive" />
          </Button>
        </div>
      ))}
      <Button
        type="button"
        variant="outline"
        size="sm"
        disabled={disabled}
        onClick={add}
      >
        <Plus className="size-4" />
        {addLabel}
      </Button>
    </div>
  );
}
