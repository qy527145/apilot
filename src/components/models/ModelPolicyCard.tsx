import { useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { RotateCcw, Save } from "lucide-react";
import { toast } from "sonner";

import { CustomRuleEditor } from "@/components/models/CustomRuleEditor";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
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
  emptyCustomRules,
  type ClientMode,
  type ClientRule,
  type ModelPolicy,
  type ModelPolicyMode,
} from "@/lib/api";
import { useUnsavedChanges } from "@/lib/unsaved";

const MODE_LABEL: Record<ModelPolicyMode, string> = {
  passthrough: "用客户端请求的模型",
  always: "强制覆盖成指定模型",
  fallback: "没有可用渠道时兜底",
  custom: "自定义规则",
};

const MODE_HINT: Record<ModelPolicyMode, string> = {
  passthrough: "不改写，客户端发什么就是什么。",
  always: "不管客户端发什么，一律换成选定的模型。",
  fallback:
    "客户端要的模型在 Apilot 里配了渠道就用它；一个渠道都没有时才换成选定的模型。",
  custom: "自己决定用哪个模型：一张映射表，或者一段 JavaScript。",
};

/** 客户端那一栏的模式：在上面四种之外多一个「跟随全局」。 */
const CLIENT_MODE_LABEL: Record<ClientMode, string> = {
  inherit: "跟随全局",
  ...MODE_LABEL,
};

/** 需要挑一个模型来替换的模式。 */
const NEEDS_MODEL: ModelPolicyMode[] = ["always", "fallback"];

/** 同上，但客户端那边用的是 `ClientMode`（多一个 inherit），单独判断更清楚。 */
function clientNeedsModel(mode: ClientMode): boolean {
  return mode === "always" || mode === "fallback";
}

/** Radix 的 Select 不接受空串，用哨兵表示"没选/沿用全局"。 */
const NO_MODEL = "__none__";

export function ModelPolicyCard({ models }: { models: string[] }) {
  const qc = useQueryClient();
  const { data } = useQuery({ queryKey: qk.settings, queryFn: api.getSettings });

  const clients = useQuery({
    queryKey: qk.clients,
    queryFn: api.detectClients,
    retry: 1,
  });

  const server = data?.model_policy;
  const [draft, setDraft] = useState<ModelPolicy | null>(null);

  const dirty = !!server && !!draft && !samePolicy(draft, server);

  // 用 ref 让下面那个 effect 读到最新的 dirty，而不必把它放进依赖数组 ——
  // 放进去的话，每次编辑都会重跑一遍「从服务端同步草稿」，等于白做。
  const dirtyRef = useRef(dirty);
  dirtyRef.current = dirty;

  // 只在**没有未保存改动**时才用服务端的值覆盖草稿。
  // 无条件覆盖会把用户正在编辑的内容冲掉：设置查询会在保存后、别的页面
  // 改动设置后重新取数，而这时候草稿正是用户还没提交的那一份。
  useEffect(() => {
    if (server && !dirtyRef.current) setDraft(server);
  }, [server]);

  const save = useMutation({
    mutationFn: (policy: ModelPolicy) => api.setModelPolicy(policy),
    onSuccess: (saved) => {
      // 把服务端**规范化之后**的结果直接写回缓存，而不是原地重取：
      // 它才是真源（空白、非法正则行都是那边清掉的）。
      setDraft(saved);
      qc.setQueryData(qk.settings, (old) =>
        old ? { ...old, model_policy: saved } : old,
      );
      toast.success("已保存");
    },
  });

  // 页头和「保存并离开」共用一个入口，返回是否成功 —— 切页拦截那边要靠它
  // 决定到底走不走。失败时不吞掉，返回 false 让调用方停下来。
  //
  // 直接闭包捕获 draft 即可：useUnsavedChanges 把传入的回调存进 ref，
  // 每次渲染都会刷新，所以它拿到的永远是最新那份草稿。
  const doSave = async (): Promise<boolean> => {
    if (!draft) return false;
    try {
      await save.mutateAsync(draft);
      return true;
    } catch {
      return false; // api 层已经 toast 过具体错误
    }
  };

  useUnsavedChanges(dirty, doSave);

  const policy = draft;

  // hook 必须在下面那个提前 return **之前**调用 —— 顺序变了 React 会把整棵
  // 组件树卸载，表现为白/黑屏。
  const modelOptions = useMemo(() => {
    // 目标可能是个还没配渠道的模型，所以除了目录里的，也要把当前值本身列进去，
    // 否则下拉会显示成空。
    const set = new Set(models);
    if (policy?.active_model) set.add(policy.active_model);
    for (const rule of Object.values(policy?.per_client ?? {})) {
      if (rule.model) set.add(rule.model);
      for (const row of rule.custom?.table ?? []) {
        if (row.target) set.add(row.target);
      }
    }
    for (const row of policy?.custom?.table ?? []) {
      if (row.target) set.add(row.target);
    }
    return [...set].sort();
  }, [models, policy]);

  const clientOptions = (clients.data ?? []).map((c) => ({
    id: c.id,
    name: c.name,
  }));

  if (!policy) return null;

  const patch = (next: Partial<ModelPolicy>) =>
    setDraft((d) => (d ? { ...d, ...next } : d));

  const patchClient = (id: string, rule: ClientRule) =>
    patch({ per_client: { ...policy.per_client, [id]: rule } });

  return (
    <Card className="py-0">
      <CardHeader className="gap-2 py-3">
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-medium">模型替换</span>
          <Badge variant={policy.mode === "passthrough" ? "secondary" : "default"}>
            {MODE_LABEL[policy.mode]}
          </Badge>
          {dirty && (
            <span className="text-amber-600 text-xs dark:text-amber-400">
              有未保存的更改
            </span>
          )}
          <div className="ml-auto flex items-center gap-2">
            <Button
              variant="outline"
              size="sm"
              disabled={!dirty || save.isPending}
              onClick={() => setDraft(server ?? null)}
            >
              <RotateCcw className="size-4" />
              撤销
            </Button>
            <Button
              size="sm"
              disabled={!dirty || save.isPending}
              onClick={() => void doSave()}
            >
              <Save className="size-4" />
              {save.isPending ? "保存中…" : "保存"}
            </Button>
          </div>
        </div>
        <p className="text-muted-foreground text-xs">
          决定发往上游的请求体里 <code>model</code> 写什么。全局设一条，
          再按需要给某个客户端单独覆盖 —— Claude Code 发
          <code className="mx-1">claude-sonnet-5</code>、Codex 发
          <code className="mx-1">gpt-5</code>，都可以落到你选的模型上。
          计费与缓存按替换后的模型算。
        </p>
      </CardHeader>

      <CardContent className="space-y-4 pt-0">
        {/* ---- 全局 ---- */}
        <section className="space-y-3 rounded-md border p-3">
          <Label className="text-xs">全局</Label>

          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <div className="space-y-1.5">
              <Label className="text-muted-foreground text-[11px]">模式</Label>
              <Select
                value={policy.mode}
                onValueChange={(v) => patch({ mode: v as ModelPolicyMode })}
              >
                <SelectTrigger className="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {(Object.keys(MODE_LABEL) as ModelPolicyMode[]).map((m) => (
                    <SelectItem key={m} value={m}>
                      {MODE_LABEL[m]}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>

            {NEEDS_MODEL.includes(policy.mode) && (
              <ModelPicker
                label="模型"
                value={policy.active_model}
                options={modelOptions}
                onChange={(model) => patch({ active_model: model })}
              />
            )}
          </div>

          {policy.mode === "custom" && (
            <CustomRuleEditor
              value={policy.custom}
              onChange={(custom) => patch({ custom })}
              models={modelOptions}
              clients={clientOptions}
            />
          )}

          {policy.mode !== "passthrough" && (
            <p className="text-muted-foreground text-[11px]">
              {MODE_HINT[policy.mode]}
            </p>
          )}

          {policy.mode !== "passthrough" &&
            NEEDS_MODEL.includes(policy.mode) &&
            !policy.active_model && (
              <p className="text-amber-600 text-xs dark:text-amber-400">
                还没选模型，替换不会生效。
              </p>
            )}
        </section>

        {/* ---- 按客户端覆盖 ---- */}
        <section className="space-y-3 rounded-md border p-3">
          <div className="flex flex-wrap items-baseline gap-2">
            <Label className="text-xs">按客户端</Label>
            <span className="text-muted-foreground text-[11px]">
              只覆盖你改过的字段，其余沿用全局。
            </span>
          </div>

          {clientOptions.length === 0 ? (
            <p className="text-muted-foreground text-xs">正在读取客户端…</p>
          ) : (
            <div className="space-y-3">
              {clientOptions.map((c) => (
                <ClientRow
                  key={c.id}
                  name={c.name}
                  rule={policy.per_client[c.id]}
                  globalModel={policy.active_model ?? null}
                  models={modelOptions}
                  clients={clientOptions}
                  onChange={(rule) => patchClient(c.id, rule)}
                />
              ))}
            </div>
          )}
        </section>
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------

/**
 * 按键名递归排序后序列化。
 *
 * 用来判断草稿与已保存值是否一致。不能直接 `JSON.stringify`：`per_client`
 * 在 Rust 侧是 map，往返一次键序未必与草稿相同，那会让「没改过」被判成
 * 「改过」，保存按钮一直亮着、切页时也一直弹拦截。
 *
 * 也不用逐字段比较 —— 那个写法会随字段增减而漏，而且漏了不会报错。
 */
function stableStringify(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(stableStringify).join(",")}]`;
  if (value && typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>).sort(
      ([a], [b]) => (a < b ? -1 : a > b ? 1 : 0),
    );
    return `{${entries
      .map(([k, v]) => `${JSON.stringify(k)}:${stableStringify(v)}`)
      .join(",")}}`;
  }
  return JSON.stringify(value) ?? "null";
}

const samePolicy = (a: ModelPolicy, b: ModelPolicy) =>
  stableStringify(a) === stableStringify(b);

function ClientRow({
  name,
  rule,
  globalModel,
  models,
  clients,
  onChange,
}: {
  name: string;
  rule?: ClientRule;
  globalModel: string | null;
  models: string[];
  clients: Array<{ id: string; name: string }>;
  onChange: (rule: ClientRule) => void;
}) {
  const mode = rule?.mode ?? "inherit";

  const setMode = (next: ClientMode) =>
    onChange({
      // 切回「跟随全局」时把其余字段一并清掉：留着既不生效，
      // 又会让后端那条"整条都是跟随全局就丢掉"的判据失灵。
      mode: next,
      model: next === "inherit" ? null : (rule?.model ?? null),
      custom: next === "inherit" ? emptyCustomRules() : (rule?.custom ?? emptyCustomRules()),
    });

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-2">
        <span className="w-24 shrink-0 text-xs">{name}</span>

        <Select value={mode} onValueChange={(v) => setMode(v as ClientMode)}>
          <SelectTrigger size="sm" className="h-8 w-44">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {(Object.keys(CLIENT_MODE_LABEL) as ClientMode[]).map((m) => (
              <SelectItem key={m} value={m}>
                {CLIENT_MODE_LABEL[m]}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>

        {clientNeedsModel(mode) && (
          <ModelPicker
            inline
            label=""
            value={rule?.model ?? null}
            options={models}
            followGlobal={globalModel}
            onChange={(model) =>
              onChange({
                mode,
                model,
                custom: rule?.custom ?? emptyCustomRules(),
              })
            }
          />
        )}
      </div>

      {mode === "custom" && (
        <div className="pl-24">
          <CustomRuleEditor
            value={rule?.custom ?? emptyCustomRules()}
            onChange={(custom) => onChange({ mode, model: rule?.model ?? null, custom })}
            models={models}
            clients={clients}
            // 整张表本来就是写给这个客户端的，再选一次客户端是重复的。
            showClientFilter={false}
          />
        </div>
      )}
    </div>
  );
}

function ModelPicker({
  label,
  value,
  options,
  followGlobal,
  inline,
  onChange,
}: {
  label: string;
  value: string | null | undefined;
  options: string[];
  /** 有它时多一个「跟全局模型」选项（客户端那一栏用）。 */
  followGlobal?: string | null;
  inline?: boolean;
  onChange: (model: string | null) => void;
}) {
  const canFollow = followGlobal !== undefined;

  return (
    <div className={inline ? "" : "space-y-1.5"}>
      {label && (
        <Label className="text-muted-foreground text-[11px]">{label}</Label>
      )}
      <Select
        value={value ?? (canFollow ? NO_MODEL : "")}
        onValueChange={(v) => onChange(v === NO_MODEL ? null : v)}
      >
        <SelectTrigger size={inline ? "sm" : "default"} className={inline ? "h-8 w-48" : "w-full"}>
          <SelectValue placeholder="选一个模型" />
        </SelectTrigger>
        <SelectContent>
          {canFollow && (
            <SelectItem value={NO_MODEL}>
              {followGlobal ? `跟全局模型（${followGlobal}）` : "跟全局（当前为空）"}
            </SelectItem>
          )}
          {options.map((m) => (
            <SelectItem key={m} value={m}>
              {m}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </div>
  );
}
