import { useEffect, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Loader2, Wand2 } from "lucide-react";
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
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { qk } from "@/hooks/queries";
import {
  ALL_PROTOCOLS,
  api,
  PROTOCOL_DEFAULT_PATH,
  PROTOCOL_LABEL,
  PROTOCOL_VERDICT_LABEL,
  type AuthStyle,
  type ChannelProxyMode,
  type Protocol,
  type ProtocolDetection,
  type ProtocolVerdict,
  type Provider,
  type ProviderInput,
  type ProviderKind,
} from "@/lib/api";
import { cn } from "@/lib/utils";
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
  proxy_mode: ChannelProxyMode;
  proxy_url: string;
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
  proxy_mode: "inherit",
  proxy_url: "",
};

const PROXY_MODE_LABEL: Record<ChannelProxyMode, string> = {
  inherit: "跟随全局设置",
  direct: "直连（不走代理）",
  manual: "使用指定代理",
};

/**
 * 检测结论的配色。
 *
 * 「未知」用琥珀而不是红：那多半是密钥/网络/限流的问题，与"服务商没有这个入口"
 * 是两回事，混成一个颜色会让人去改错东西（比如把一条好渠道的勾去掉）。
 */
const VERDICT_CLASS: Record<ProtocolVerdict, string> = {
  supported: "bg-emerald-500/15 text-emerald-500",
  unsupported: "bg-destructive/15 text-destructive",
  inconclusive: "bg-amber-500/15 text-amber-500",
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
    // 后端可能给 null（老行没迁移过），前端这一栏统一用 inherit 兜底。
    proxy_mode: p.proxy?.mode ?? "inherit",
    proxy_url: p.proxy?.url ?? "",
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
  const [detecting, setDetecting] = useState(false);
  /** 上一次自动检测的结果，按协议索引；没测过的不在这里。 */
  const [detections, setDetections] = useState<
    Partial<Record<Protocol, ProtocolDetection>> | null
  >(null);

  useEffect(() => {
    if (open) {
      setForm(provider ? toForm(provider) : EMPTY);
      setError(null);
      // 关了再打开时旧结论必须清掉：base_url / 密钥可能已经换了，
      // 留着上一轮的徽标会让人以为那就是当前这个地址的检测结果。
      setDetections(null);
    }
  }, [open, provider]);

  /**
   * 新建渠道后顺手把上游的模型列表拉回来声明上。
   *
   * 没这份声明，这个渠道就是「通吃」：模型页里看不到它的模型，
   * 路由也没法按模型把它选出来。拉不到（服务商没有 /v1/models）不阻断创建 ——
   * 退回到原来的通吃语义，用户仍可在渠道行展开后手填。
   */
  const autoDeclareModels = async (id: number) => {
    let models: string[] = [];
    try {
      models = await api.fetchProviderModels(id);
    } catch {
      toast.success("渠道已创建", {
        description: "没拉到模型列表，可在渠道行展开后手动添加。",
      });
      return;
    }

    if (models.length === 0) {
      toast.success("渠道已创建", {
        description: "上游没返回模型，可在渠道行展开后手动添加。",
      });
      return;
    }

    try {
      await api.setProviderModels(id, models);
      qc.invalidateQueries({ queryKey: qk.providerModels(id) });
      qc.invalidateQueries({ queryKey: qk.modelCatalog });
      toast.success(`渠道已创建，已声明 ${models.length} 个模型`, {
        // 声明之后这个渠道就不再"通吃"了：客户端发来的名字必须在列表里才会路由过来。
        // 不说清楚的话，用户会觉得"刚加的渠道怎么不接我的请求"。
        description: "客户端要用的模型名得在列表里才会走这个渠道；需要改名去路由页配。",
      });
    } catch {
      toast.success("渠道已创建", {
        description: "自动写入模型列表失败，可在渠道行展开后手动添加。",
      });
    }
  };

  /**
   * 自动检测这个地址支持哪些协议。
   *
   * 只**补**勾、不取消已勾的：一次网络探测会误判（中转对未知路径回 200 的兜底页、
   * 网关在路由之前就做鉴权），拿它去删用户的配置等于把一个可能错的结论当成用户的
   * 决定。否定结论只标在对应那一行，改不改由用户定。
   */
  const runDetect = async () => {
    if (!form.base_url.trim()) return setError("先填好 base_url 再检测");
    setError(null);
    setDetecting(true);
    try {
      const results = await api.detectProviderProtocols({
        // 带上 id：密钥不回显，留空时后端沿用它库里存的那把 ——
        // 否则编辑老渠道时这次检测会不带鉴权头，三种协议一律 401。
        id: provider?.id ?? null,
        base_url: form.base_url.trim(),
        api_key: form.api_key.trim() ? form.api_key.trim() : null,
        auth_style: form.auth_style,
        extra_headers: form.extra_headers,
        proxy: {
          mode: form.proxy_mode,
          url: form.proxy_url.trim() ? form.proxy_url.trim() : null,
        },
        // 与保存时同一套规则：等于默认值的路径回退成 null，走协议默认路径的拼接。
        paths: ALL_PROTOCOLS.map((p) => {
          const path = effectivePath(form.paths, p);
          return {
            protocol: p,
            path: path === PROTOCOL_DEFAULT_PATH[p] ? null : path,
          };
        }),
      });

      const byProtocol: Partial<Record<Protocol, ProtocolDetection>> = {};
      for (const r of results) byProtocol[r.protocol] = r;
      setDetections(byProtocol);

      const found = results
        .filter((r) => r.verdict === "supported")
        .map((r) => r.protocol);

      if (found.length === 0) {
        toast.warning("没检测到可用的协议入口", {
          description: results.some((r) => r.verdict === "inconclusive")
            ? "上游有响应但说明不了问题 —— 原因标在每一行右侧。"
            : "三种协议的入口都是 404：检查 base_url 是不是少了路径前缀。",
        });
        return;
      }

      setForm((f) => {
        const protocols = [...f.protocols];
        for (const p of found) if (!protocols.includes(p)) protocols.push(p);
        return { ...f, protocols };
      });
      toast.success(`检测到 ${found.length} 种协议`, {
        description: "已勾上还没勾的那几种；检测不会取消你已勾选的协议。",
      });
    } catch {
      /* 错误 toast 已在 api 层弹出 */
    } finally {
      setDetecting(false);
    }
  };

  const mutation = useMutation({
    mutationFn: (input: ProviderInput) => api.upsertProvider(input),
    onSuccess: (saved) => {
      qc.invalidateQueries({ queryKey: qk.providers });
      // 改渠道的启用状态 / 删渠道都会影响模型页的候选列表。
      qc.invalidateQueries({ queryKey: qk.modelCatalog });
      qc.invalidateQueries({ queryKey: qk.providerModels(saved.id) });
      onOpenChange(false);

      if (provider) {
        toast.success("渠道已更新");
        return;
      }
      void autoDeclareModels(saved.id);
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
      tag: form.tag.trim() || undefined,
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
      weight: Number(form.weight) || 0,
      priority: Number(form.priority) || 0,
      enabled: form.enabled,
      timeout_ms: Number(form.timeout_ms) || 60000,
      proxy: {
        mode: form.proxy_mode,
        url: form.proxy_url.trim() ? form.proxy_url.trim() : null,
      },
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
            上游 LLM 服务地址与鉴权方式。新建时会自动获取该渠道支持的模型。
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-4">
          {/*
           * 编辑时也保留预设：老渠道是在"协议声明"这个功能存在之前建的，
           * 它们清一色只有 kind 一种协议、路径也没填过。要把它改成"这家其实
           * 支持三种协议"，手工勾选 + 抄路径很容易抄错，一键套用可靠得多。
           */}
          <div className="space-y-2">
            <Label className="text-muted-foreground text-xs">
              {provider
                ? "套用预设（会覆盖名称、base url、鉴权与协议声明）"
                : "快速填充常用服务商"}
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

          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
            <div className="space-y-2">
              <Label>名称</Label>
              <Input
                value={form.name}
                onChange={(e) => set("name", e.target.value)}
                placeholder="例如：Anthropic 官方"
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
            <div className="flex items-center justify-between gap-3">
              <Label className="text-sm">支持的协议</Label>
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={runDetect}
                disabled={detecting}
              >
                {detecting ? (
                  <Loader2 className="size-3.5 animate-spin" />
                ) : (
                  <Wand2 className="size-3.5" />
                )}
                {detecting ? "检测中…" : "自动检测"}
              </Button>
            </div>
            <p className="text-muted-foreground text-xs">
              Apilot 会用这里勾选的协议直接对上游说话。客户端说什么协议，命中哪一项就
              用哪一项，<span className="text-foreground">不重编码</span>
              （直通）；都没命中才做协议转换。
            </p>
            <p className="text-muted-foreground text-xs">
              「自动检测」按当前的 base url / 密钥 / 路径各探一次（请求体是空的，
              不消耗 token）：探到的会帮你勾上，
              <span className="text-foreground">但不会取消</span>
              你已经勾好的。
            </p>
            <div className="mt-1 space-y-2">
              {ALL_PROTOCOLS.map((p) => {
                const checked = form.protocols.includes(p);
                const value = effectivePath(form.paths, p);
                const isDefault = value === PROTOCOL_DEFAULT_PATH[p];
                const det = detections?.[p];
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
                        {det && (
                          <Tooltip>
                            <TooltipTrigger asChild>
                              <span
                                className={cn(
                                  "ml-1.5 cursor-help rounded px-1 py-px text-[10px] whitespace-nowrap",
                                  VERDICT_CLASS[det.verdict],
                                )}
                              >
                                {PROTOCOL_VERDICT_LABEL[det.verdict]}
                                {det.latency_ms != null
                                  ? ` ${det.latency_ms}ms`
                                  : ""}
                              </span>
                            </TooltipTrigger>
                            <TooltipContent>
                              <div className="max-w-72 space-y-0.5 text-xs">
                                <div>上游返回 {det.status ?? "—"}</div>
                                <div className="font-mono break-all">
                                  {det.url}
                                </div>
                                {det.note && <div>{det.note}</div>}
                              </div>
                            </TooltipContent>
                          </Tooltip>
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
                    {/* 勾了、但探测说这个路径没有入口。只提醒，不替你取消勾选 ——
                        一次网络探测不该悄悄删掉配置。 */}
                    {checked && det?.verdict === "unsupported" && (
                      <p className="text-destructive pl-[3.75rem] text-[11px]">
                        检测到该路径没有入口（HTTP {det.status}
                        ）—— 用这个协议的客户端会拿到 404。取消勾选，或改上面的路径。
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

          <div className="space-y-2 rounded-md border p-3">
            <Label className="text-sm">代理</Label>
            <p className="text-muted-foreground text-xs">
              「直连」是给本地模型服务（ollama / LM Studio）用的：全局配了公司代理时，
              只有它能保证请求留在本机。回环与私有网段本来就会绕过代理。
            </p>
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <Select
                value={form.proxy_mode}
                onValueChange={(v) => set("proxy_mode", v as ChannelProxyMode)}
              >
                <SelectTrigger className="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {(Object.keys(PROXY_MODE_LABEL) as ChannelProxyMode[]).map(
                    (m) => (
                      <SelectItem key={m} value={m}>
                        {PROXY_MODE_LABEL[m]}
                      </SelectItem>
                    ),
                  )}
                </SelectContent>
              </Select>
              <Input
                value={form.proxy_url}
                disabled={form.proxy_mode !== "manual"}
                onChange={(e) => set("proxy_url", e.target.value)}
                placeholder="socks5://127.0.0.1:1080"
              />
            </div>
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
