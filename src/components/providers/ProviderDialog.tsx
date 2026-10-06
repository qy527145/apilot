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
import { Checkbox } from "@/components/ui/checkbox";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { qk } from "@/hooks/queries";
import {
  ALL_PROTOCOLS,
  api,
  PROTOCOL_DEFAULT_PATH,
  PROTOCOL_LABEL,
  type AuthStyle,
  type Protocol,
  type Provider,
  type ProviderInput,
  type ProviderKind,
} from "@/lib/api";
import { PROVIDER_PRESETS, type ProviderPreset } from "./presets";

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
  /** 勾了哪些协议。没勾过的渠道用不到这里 —— `kind` 会自动兜底。 */
  protocols: Protocol[];
  /** 各协议的路径覆盖，留空表示用默认路径。 */
  paths: Partial<Record<Protocol, string>>;
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
  // 新建时默认勾上首选协议，否则用户不碰这一栏就存不下去 ——
  // 而"没声明过协议"在后端本来就等于"只有首选那一种"。
  protocols: ["anthropic"],
  paths: {},
  weight: "1",
  priority: "0",
  timeout_ms: "60000",
  enabled: true,
  extra_headers: {},
  param_override: "",
};

/** 某协议当前生效的路径：覆盖过就用覆盖值，否则用协议默认值。 */
const effectivePath = (
  paths: Partial<Record<Protocol, string>>,
  p: Protocol,
): string => paths[p]?.trim() || PROTOCOL_DEFAULT_PATH[p];

/**
 * 预览出站 URL。**必须与后端 `Provider::endpoint` / `endpoint_verbatim` 同规则**，
 * 否则这个预览会比没有更糟 —— 它会让用户以为路径是对的。
 *
 * 规则：只去掉「base 与 path 都带 /v1」时重复的那一次，绝不替你补 /v1。
 * 后端分两个函数（一个补、一个不补）是因为它自己生成的默认路径需要补，
 * 而用户手写的路径不需要；这里一律按"用户写的"处理，因为输入框里就是用户看到的值。
 */
const joinUrl = (baseUrl: string, path: string): string => {
  const base = baseUrl.trim().replace(/\/+$/, "");
  const p = path.trim().replace(/^\/+/, "");
  if (!base) return p;
  if (base.endsWith("/v1") && p.startsWith("v1/")) {
    return `${base}/${p.slice(3)}`;
  }
  return p ? `${base}/${p}` : base;
};

function toForm(p: Provider): FormState {
  let protocols = (p.protocols ?? []).map((e) => e.protocol);
  const paths: Partial<Record<Protocol, string>> = {};

  // 老渠道（或任何没声明过协议的渠道）在库里存的是空数组，
  // 语义是"只支持 kind 那一种"。补上它，用户改个超时时间不必先手动勾一次协议。
  //
  // 反过来，**不能**因为 kind 不在集合里就自动补进去：那会把
  // "只声明了 chat" 的渠道悄悄变成 "chat + anthropic"，之后 Anthropic 请求就不再
  // 转换、直接打 /v1/messages 了 —— 用户没这么配，服务商也未必有那个路径。
  if (protocols.length === 0) {
    protocols = [p.kind];
  }

  // 路径一律填上**当前生效值**，不留空。留空只显示 placeholder 的话，
  // 用户看不出这条协议到底会打到哪个 URL 上，而这正是最需要一眼看清的东西。
  for (const proto of ALL_PROTOCOLS) {
    const override = (p.protocols ?? []).find((e) => e.protocol === proto)?.path;
    paths[proto] = override ?? PROTOCOL_DEFAULT_PATH[proto];
  }

  return {
    tag: p.tag,
    name: p.name,
    kind: p.kind,
    base_url: p.base_url,
    api_key: "",
    auth_style: p.auth_style,
    protocols,
    paths,
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

  // 预设只覆盖"服务商身份"相关的字段；api_key 留空由用户自己填，
  // 免得切预设时把已经敲好的密钥冲掉。
  //
  // 支持的协议与路径一并带上 —— 预设里写的是核实过的端点。少了它们，
  // 像 DeepSeek 这种三种协议都支持的服务商会被当成只说一种，客户端每次
  // 请求都要经 Apilot 转换；而它的 Anthropic 入口挂在 /anthropic 子路径下，
  // 用错路径就是 404。
  const applyPreset = (p: ProviderPreset) =>
    setForm((f) => {
      const paths: Partial<Record<Protocol, string>> = {};
      for (const proto of ALL_PROTOCOLS) {
        const override = p.protocols.find((e) => e.protocol === proto)?.path;
        paths[proto] = override ?? PROTOCOL_DEFAULT_PATH[proto];
      }
      return {
        ...f,
        name: p.name,
        tag: p.tag,
        kind: p.kind,
        base_url: p.base_url,
        auth_style: p.auth_style,
        protocols: p.protocols.map((e) => e.protocol),
        paths,
      };
    });

  const submit = () => {
    if (!form.tag.trim()) return setError("请填写渠道 tag");
    if (!form.name.trim()) return setError("请填写渠道名称");
    if (!form.base_url.trim()) return setError("请填写 base_url");
    if (form.protocols.length === 0)
      return setError("请至少勾选一种支持的协议");

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
      // 顺序无所谓，后端按协议查找。
      // 路径等于默认值（或留空）时回退成 null，让后端用协议默认路径 ——
      // 存一份和默认值一模一样的字符串只会让"到底覆盖过没有"变得看不出来。
      protocols: form.protocols.map((p) => {
        const path = effectivePath(form.paths, p);
        return {
          protocol: p,
          path: path === PROTOCOL_DEFAULT_PATH[p] ? null : path,
        };
      }),
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
          {!provider && (
            <div className="space-y-2">
              <Label className="text-muted-foreground text-xs">
                快速填充常用服务商
              </Label>
              <div className="flex flex-wrap gap-2">
                {PROVIDER_PRESETS.map((p) => (
                  <Button
                    key={p.id}
                    type="button"
                    variant="outline"
                    size="sm"
                    onClick={() => applyPreset(p)}
                  >
                    {p.label}
                  </Button>
                ))}
              </div>
            </div>
          )}

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
              <Label>首选协议</Label>
              <Select
                value={form.kind}
                onValueChange={(v) => {
                  const kind = v as ProviderKind;
                  // 首选协议必须在勾选集合里，否则保存后会被后端当成
                  // "不在声明里"而落到兜底分支，与用户看到的不一致。
                  setForm((f) => ({
                    ...f,
                    kind,
                    protocols: f.protocols.includes(kind)
                      ? f.protocols
                      : [...f.protocols, kind],
                  }));
                }}
              >
                <SelectTrigger className="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {ALL_PROTOCOLS.map((k) => (
                    <SelectItem key={k} value={k}>
                      {PROTOCOL_LABEL[k]}
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

          {/* 协议声明：勾上的就是 Apilot 会直接对上游说的协议。
              入站协议命中勾选集合时原样转发（直通，不重编码）；没命中才转换。 */}
          <div className="space-y-2 rounded-md border p-3">
            <Label className="text-sm">支持的协议</Label>
            <p className="text-muted-foreground text-xs">
              Apilot 会用这里勾选的协议直接对上游说话。客户端说什么协议，命中哪一项就
              用哪一项，<span className="text-foreground">不重编码</span>
              （直通）；都没命中才做协议转换。
            </p>
            <div className="mt-1 space-y-2">
              {ALL_PROTOCOLS.map((p) => {
                const checked = form.protocols.includes(p);
                const value = effectivePath(form.paths, p);
                const isDefault = value === PROTOCOL_DEFAULT_PATH[p];
                return (
                  <div key={p} className="space-y-1">
                    <div className="flex items-center gap-3">
                      <Checkbox
                        checked={checked}
                        onCheckedChange={(c) =>
                          setForm((f) => {
                            const protocols = c
                              ? [...f.protocols, p]
                              : f.protocols.filter((x) => x !== p);
                            return {
                              ...f,
                              protocols,
                              // 首选协议跟着勾选走：取消勾选首选时，换成还留着的第一个，
                              // 免得留下"首选不在集合里"的非法状态。
                              kind:
                                !c && f.kind === p && protocols.length > 0
                                  ? protocols[0]
                                  : f.kind,
                            };
                          })
                        }
                      />
                      <span className="w-44 shrink-0 text-xs">
                        {PROTOCOL_LABEL[p]}
                        {form.kind === p && (
                          <span className="text-muted-foreground">（首选）</span>
                        )}
                      </span>
                      <Input
                        className="h-8 flex-1 font-mono text-xs"
                        value={value}
                        disabled={!checked}
                        onChange={(e) =>
                          setForm((f) => ({
                            ...f,
                            paths: { ...f.paths, [p]: e.target.value },
                          }))
                        }
                      />
                    </div>
                    {/* 直接把拼出来的完整地址摆出来。上游报 404 时第一个要看的就是它，
                        藏在一个 placeholder 里没人看得见。 */}
                    {checked && form.base_url.trim() && (
                      <p className="text-muted-foreground pl-[3.75rem] font-mono text-[11px] break-all">
                        → {joinUrl(form.base_url, value)}
                        {isDefault && (
                          <span className="font-sans">（默认路径）</span>
                        )}
                      </p>
                    )}
                  </div>
                );
              })}
            </div>
            {form.protocols.length === 0 && (
              <p className="text-destructive text-xs">至少勾选一种协议。</p>
            )}
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
