import { useEffect, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";

import { KeyValueEditor } from "@/components/common/KeyValueEditor";
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
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { qk } from "@/hooks/queries";
import {
  api,
  type AuthStyle,
  type Provider,
  type ProviderInput,
  type ProviderKind,
} from "@/lib/api";

const KIND_LABELS: Record<ProviderKind, string> = {
  anthropic: "Anthropic",
  openai_chat: "OpenAI Chat",
  openai_responses: "OpenAI Responses",
};

const AUTH_LABELS: Record<AuthStyle, string> = {
  bearer: "Bearer Token",
  "x-api-key": "x-api-key",
  none: "无鉴权",
};

interface FormState {
  tag: string;
  name: string;
  kind: ProviderKind;
  base_url: string;
  api_key: string;
  auth_style: AuthStyle;
  weight: string;
  priority: string;
  timeout_ms: string;
  enabled: boolean;
  extra_headers: Record<string, string>;
  param_override: string;
}

const EMPTY: FormState = {
  tag: "",
  name: "",
  kind: "anthropic",
  base_url: "",
  api_key: "",
  auth_style: "bearer",
  weight: "1",
  priority: "0",
  timeout_ms: "60000",
  enabled: true,
  extra_headers: {},
  param_override: "",
};

function toForm(p: Provider): FormState {
  return {
    tag: p.tag,
    name: p.name,
    kind: p.kind,
    base_url: p.base_url,
    api_key: "",
    auth_style: p.auth_style,
    weight: String(p.weight),
    priority: String(p.priority),
    timeout_ms: String(p.timeout_ms),
    enabled: p.enabled,
    extra_headers: p.extra_headers ?? {},
    param_override: p.param_override
      ? JSON.stringify(p.param_override, null, 2)
      : "",
  };
}

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  provider?: Provider | null;
}

export function ProviderDialog({ open, onOpenChange, provider }: Props) {
  const qc = useQueryClient();
  const [form, setForm] = useState<FormState>(EMPTY);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (open) {
      setForm(provider ? toForm(provider) : EMPTY);
      setError(null);
    }
  }, [open, provider]);

  const mutation = useMutation({
    mutationFn: (input: ProviderInput) => api.upsertProvider(input),
    onSuccess: (saved) => {
      qc.invalidateQueries({ queryKey: qk.providers });
      qc.invalidateQueries({ queryKey: qk.providerModels(saved.id) });
      toast.success(provider ? "渠道已更新" : "渠道已创建");
      onOpenChange(false);
    },
  });

  const set = <K extends keyof FormState>(key: K, value: FormState[K]) =>
    setForm((f) => ({ ...f, [key]: value }));

  const submit = () => {
    if (!form.tag.trim()) return setError("请填写渠道 tag");
    if (!form.name.trim()) return setError("请填写渠道名称");
    if (!form.base_url.trim()) return setError("请填写 base_url");

    let paramOverride: unknown | null = null;
    if (form.param_override.trim()) {
      try {
        paramOverride = JSON.parse(form.param_override);
      } catch {
        return setError("param_override 不是合法 JSON");
      }
    }

    const input: ProviderInput = {
      id: provider?.id ?? null,
      tag: form.tag.trim(),
      name: form.name.trim(),
      kind: form.kind,
      base_url: form.base_url.trim(),
      api_key: form.api_key.trim() ? form.api_key.trim() : null,
      auth_style: form.auth_style,
      extra_headers: form.extra_headers,
      param_override: paramOverride,
      model_mapping: provider?.model_mapping ?? {},
      weight: Number(form.weight) || 0,
      priority: Number(form.priority) || 0,
      enabled: form.enabled,
      timeout_ms: Number(form.timeout_ms) || 60000,
    };
    setError(null);
    mutation.mutate(input);
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>{provider ? "编辑渠道" : "新建渠道"}</DialogTitle>
          <DialogDescription>
            上游 LLM 服务地址与鉴权方式。模型映射可在渠道列表中展开编辑。
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-4">
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
            <div className="space-y-2">
              <Label>名称</Label>
              <Input
                value={form.name}
                onChange={(e) => set("name", e.target.value)}
                placeholder="例如：Anthropic 官方"
              />
            </div>
            <div className="space-y-2">
              <Label>Tag（唯一标识）</Label>
              <Input
                value={form.tag}
                onChange={(e) => set("tag", e.target.value)}
                placeholder="例如：anthropic-official"
              />
            </div>
          </div>

          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
            <div className="space-y-2">
              <Label>协议类型</Label>
              <Select
                value={form.kind}
                onValueChange={(v) => set("kind", v as ProviderKind)}
              >
                <SelectTrigger className="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {(Object.keys(KIND_LABELS) as ProviderKind[]).map((k) => (
                    <SelectItem key={k} value={k}>
                      {KIND_LABELS[k]}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="space-y-2">
              <Label>鉴权方式</Label>
              <Select
                value={form.auth_style}
                onValueChange={(v) => set("auth_style", v as AuthStyle)}
              >
                <SelectTrigger className="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {(Object.keys(AUTH_LABELS) as AuthStyle[]).map((k) => (
                    <SelectItem key={k} value={k}>
                      {AUTH_LABELS[k]}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          </div>

          <div className="space-y-2">
            <Label>Base URL</Label>
            <Input
              value={form.base_url}
              onChange={(e) => set("base_url", e.target.value)}
              placeholder="https://api.anthropic.com"
            />
          </div>

          <div className="space-y-2">
            <Label>API Key</Label>
            <Input
              type="password"
              value={form.api_key}
              onChange={(e) => set("api_key", e.target.value)}
              placeholder={provider ? "留空表示不修改" : "sk-..."}
              autoComplete="off"
            />
          </div>

          <div className="grid grid-cols-3 gap-4">
            <div className="space-y-2">
              <Label>权重</Label>
              <Input
                type="number"
                value={form.weight}
                onChange={(e) => set("weight", e.target.value)}
              />
            </div>
            <div className="space-y-2">
              <Label>优先级</Label>
              <Input
                type="number"
                value={form.priority}
                onChange={(e) => set("priority", e.target.value)}
              />
            </div>
            <div className="space-y-2">
              <Label>超时 (ms)</Label>
              <Input
                type="number"
                value={form.timeout_ms}
                onChange={(e) => set("timeout_ms", e.target.value)}
              />
            </div>
          </div>

          <div className="space-y-2">
            <Label>额外请求头</Label>
            <KeyValueEditor
              value={form.extra_headers}
              onChange={(v) => set("extra_headers", v)}
              keyPlaceholder="Header 名"
              valuePlaceholder="Header 值"
            />
          </div>

          <div className="space-y-2">
            <Label>参数覆盖 (param_override, JSON)</Label>
            <Textarea
              value={form.param_override}
              onChange={(e) => set("param_override", e.target.value)}
              placeholder='{"temperature": 0.7}'
              className="font-mono text-xs"
              rows={3}
            />
          </div>

          <div className="flex items-center justify-between rounded-md border px-3 py-2">
            <div>
              <Label>启用该渠道</Label>
              <p className="text-muted-foreground text-xs">
                停用后路由不会再选择此渠道。
              </p>
            </div>
            <Switch
              checked={form.enabled}
              onCheckedChange={(v) => set("enabled", v)}
            />
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
