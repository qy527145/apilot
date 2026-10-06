import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { Boxes, Check, Gauge, RotateCcw, Server } from "lucide-react";

import { EmptyState } from "@/components/common/EmptyState";
import { ErrorState, TableSkeleton } from "@/components/common/StatCard";
import { PageShell } from "@/components/layout/PageShell";
import type { ViewKey } from "@/components/layout/nav";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
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
  type ModelCatalogEntry,
  type ModelCandidate,
  type ModelPolicy,
  type ModelPolicyMode,
  type ModelStrategy,
} from "@/lib/api";
import { cn, formatMs } from "@/lib/utils";

const POLICY_MODE_LABEL: Record<ModelPolicyMode, string> = {
  off: "关闭（不改模型）",
  always: "无条件替换",
  fallback: "仅兜底（请求的模型没有渠道时才换）",
  per_client: "按客户端分别指定",
};

const STRATEGY_LABEL: Record<ModelStrategy, string> = {
  priority: "按优先级",
  latency: "按延迟",
  weight: "按权重随机",
};

/** 需要挑一个模型来替换的模式。 */
const NEEDS_ACTIVE_MODEL: ModelPolicyMode[] = ["always", "fallback", "per_client"];

/** 常见的三种客户端，用于「按客户端」那一栏的默认行。 */
const KNOWN_CLIENTS: Array<[string, string]> = [
  ["claude-code", "Claude Code"],
  ["codex", "Codex"],
  ["gemini-cli", "Gemini CLI"],
];

export default function ModelsPage({
  onNavigate,
}: {
  onNavigate: (view: ViewKey) => void;
}) {
  const catalog = useQuery({
    queryKey: qk.modelCatalog,
    queryFn: api.listModelCatalog,
    retry: 1,
  });

  const models = catalog.data ?? [];

  return (
    <PageShell
      title="模型"
      description="选一个模型全局生效，并决定它在多个渠道之间怎么排"
      actions={
        <Button variant="outline" onClick={() => onNavigate("providers")}>
          <Server className="size-4" />
          渠道管理
        </Button>
      }
    >
      <ModelPolicyCard models={models.map((m) => m.model)} />

      {catalog.isLoading ? (
        <Card className="py-0">
          <CardContent className="p-4">
            <TableSkeleton rows={4} cols={3} />
          </CardContent>
        </Card>
      ) : catalog.isError ? (
        <Card className="py-0">
          <CardContent className="p-4">
            <ErrorState onRetry={() => catalog.refetch()} />
          </CardContent>
        </Card>
      ) : models.length === 0 ? (
        <Card className="py-0">
          <CardContent className="p-6">
            <EmptyState
              icon={Boxes}
              title="还没有模型"
              description="模型挂在渠道下面。先到「渠道管理」添加上游渠道并声明模型，再回来配置。"
              action={
                <Button size="sm" onClick={() => onNavigate("providers")}>
                  <Server className="size-4" />
                  去渠道管理
                </Button>
              }
            />
          </CardContent>
        </Card>
      ) : (
        models.map((entry) => (
          <ModelCard key={entry.model} entry={entry} />
        ))
      )}
    </PageShell>
  );
}

// ---------------------------------------------------------------------------
// 全局模型替换
// ---------------------------------------------------------------------------

function ModelPolicyCard({ models }: { models: string[] }) {
  const qc = useQueryClient();
  const { data } = useQuery({ queryKey: qk.settings, queryFn: api.getSettings });

  const save = useMutation({
    mutationFn: api.setModelPolicy,
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.settings });
      toast.success("已保存");
    },
  });

  const policy = data?.model_policy;
  if (!policy) return null;

  const patch = (next: Partial<ModelPolicy>) =>
    save.mutate({ ...policy, ...next });

  const modelOptions = useMemo(() => {
    // 全局替换的目标可能是个还没配渠道的模型，所以除了目录里的，
    // 也要把当前值本身列进去，否则下拉会显示成空。
    const set = new Set(models);
    if (policy.active_model) set.add(policy.active_model);
    return [...set].sort();
  }, [models, policy.active_model]);

  return (
    <Card className="py-0">
      <CardHeader className="gap-2 py-3">
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-medium">全局模型替换</span>
          <Badge variant={policy.mode === "off" ? "secondary" : "default"}>
            {POLICY_MODE_LABEL[policy.mode]}
          </Badge>
        </div>
        <p className="text-muted-foreground text-xs">
          客户端请求里写的模型名会被换成这里选的 —— Claude Code 发
          <code className="mx-1">claude-sonnet-5</code>、Codex 发
          <code className="mx-1">gpt-5</code>，都落到你选的那个模型上。
          计费与缓存按替换后的模型算。
        </p>
      </CardHeader>

      <CardContent className="space-y-3 pt-0">
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
          <div className="space-y-1.5">
            <Label className="text-xs">模式</Label>
            <Select
              value={policy.mode}
              onValueChange={(v) => patch({ mode: v as ModelPolicyMode })}
            >
              <SelectTrigger className="w-full">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {(Object.keys(POLICY_MODE_LABEL) as ModelPolicyMode[]).map((m) => (
                  <SelectItem key={m} value={m}>
                    {POLICY_MODE_LABEL[m]}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>

          {NEEDS_ACTIVE_MODEL.includes(policy.mode) && (
            <div className="space-y-1.5">
              <Label className="text-xs">
                模型
                {policy.mode === "per_client" && (
                  <span className="text-muted-foreground">
                    （没单独指定的客户端用它）
                  </span>
                )}
              </Label>
              <Select
                value={policy.active_model ?? ""}
                onValueChange={(v) => patch({ active_model: v })}
              >
                <SelectTrigger className="w-full">
                  <SelectValue placeholder="选一个模型" />
                </SelectTrigger>
                <SelectContent>
                  {modelOptions.map((m) => (
                    <SelectItem key={m} value={m}>
                      {m}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          )}
        </div>

        {policy.mode === "per_client" && (
          <div className="space-y-2 rounded-md border p-3">
            <Label className="text-xs">按客户端</Label>
            {KNOWN_CLIENTS.map(([id, label]) => (
              <div key={id} className="flex items-center gap-3">
                <span className="w-32 shrink-0 text-xs">{label}</span>
                <Input
                  className="h-8 flex-1 font-mono text-xs"
                  value={policy.per_client[id] ?? ""}
                  placeholder="留空 = 跟随上面的全局模型"
                  onChange={(e) =>
                    patch({
                      per_client: { ...policy.per_client, [id]: e.target.value },
                    })
                  }
                />
              </div>
            ))}
            <p className="text-muted-foreground text-[11px]">
              留空的客户端落到上面那个「模型」。
            </p>
          </div>
        )}

        {policy.mode !== "off" && !policy.active_model && (
          <p className="text-amber-600 text-xs dark:text-amber-400">
            还没选模型，替换不会生效。
          </p>
        )}
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// 每个模型
// ---------------------------------------------------------------------------

function ModelCard({ entry }: { entry: ModelCatalogEntry }) {
  const qc = useQueryClient();
  const [probing, setProbing] = useState(false);

  const invalidate = () => {
    qc.invalidateQueries({ queryKey: qk.modelCatalog });
  };

  const resolve = useMutation({
    mutationFn: (tag: string) =>
      api.switchModelChannel(entry.model, tag),
    onSuccess: (p) => {
      invalidate();
      toast.success(`已切到 ${p.active_provider}`);
    },
  });

  const setStrategy = useMutation({
    mutationFn: (strategy: ModelStrategy) =>
      api.upsertModelPolicy({
        model: entry.model,
        strategy,
        active_provider: entry.policy?.active_provider ?? null,
      }),
    onSuccess: invalidate,
  });

  const reset = useMutation({
    mutationFn: () => api.resetModelPolicy(entry.model),
    onSuccess: () => {
      invalidate();
      toast.success("已交回路由规则");
    },
  });

  const probe = useMutation({
    mutationFn: () => {
      setProbing(true);
      return api.probeModelCandidates(entry.model);
    },
    onSettled: () => setProbing(false),
    onSuccess: (results) => {
      invalidate();
      const ok = results.filter((r) => r.ok).length;
      toast.success(`测速完成：${ok}/${results.length} 个渠道可用`);
    },
  });

  const managed = !!entry.policy;
  const strategy = entry.policy?.strategy ?? "priority";
  const hasLatency = entry.candidates.some((c) => c.latency_ms != null);

  return (
    <Card className="py-0">
      <CardHeader className="gap-2 py-3">
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-mono text-sm font-medium">{entry.model}</span>
          <Badge variant="outline">
            {entry.candidates.length} 个渠道
          </Badge>
          {managed ? (
            <Badge variant="default">由模型页决定</Badge>
          ) : (
            <Badge variant="secondary">沿用路由规则</Badge>
          )}
        </div>

        <div className="flex flex-wrap items-center gap-3 text-xs">
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">选法</span>
            <Select
              value={strategy}
              onValueChange={(v) => setStrategy.mutate(v as ModelStrategy)}
            >
              <SelectTrigger size="sm" className="h-7 w-36">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {(Object.keys(STRATEGY_LABEL) as ModelStrategy[]).map((s) => (
                  <SelectItem key={s} value={s}>
                    {STRATEGY_LABEL[s]}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>

          {strategy === "latency" && !hasLatency && (
            <span className="text-amber-600 dark:text-amber-400">
              还没测过速，先点右边的「测速」
            </span>
          )}

          <div className="ml-auto flex items-center gap-1.5">
            <Button
              variant="ghost"
              size="sm"
              className="h-7 px-2 text-[11px]"
              disabled={probing}
              onClick={() => probe.mutate()}
            >
              <Gauge className="size-3.5" />
              {probing ? "测速中…" : "测速"}
            </Button>
            {managed && (
              <Button
                variant="ghost"
                size="sm"
                className="h-7 px-2 text-[11px]"
                onClick={() => reset.mutate()}
              >
                <RotateCcw className="size-3.5" />
                交回路由规则
              </Button>
            )}
          </div>
        </div>
      </CardHeader>

      <CardContent className="space-y-2 pt-0">
        {/* 点一行即切换主渠道 —— 与代理软件选节点是同一个交互。 */}
        {entry.candidates.map((c) => (
          <CandidateRow
            key={c.provider_tag}
            candidate={c}
            primary={entry.primary === c.provider_tag}
            // 加权随机每次请求都重抽，没有"当前"可点。
            canSwitch={strategy !== "weight"}
            onSwitch={() => resolve.mutate(c.provider_tag)}
          />
        ))}

        {entry.candidates.length === 0 && (
          <p className="text-muted-foreground text-xs">
            还没有渠道声明这个模型。
          </p>
        )}
      </CardContent>
    </Card>
  );
}

function CandidateRow({
  candidate,
  primary,
  canSwitch,
  onSwitch,
}: {
  candidate: ModelCandidate;
  primary: boolean;
  canSwitch: boolean;
  onSwitch: () => void;
}) {
  const upstream = candidate.upstream_model;

  return (
    <button
      type="button"
      disabled={!canSwitch || !candidate.enabled}
      onClick={onSwitch}
      className={cn(
        "flex w-full items-center gap-3 rounded-md border px-3 py-2 text-left text-xs",
        primary && "border-emerald-500/50 bg-emerald-500/5",
        (!candidate.enabled || !canSwitch) && "opacity-60",
        canSwitch && candidate.enabled && "hover:bg-muted/50",
      )}
    >
      <span
        className={cn(
          "flex size-4 shrink-0 items-center justify-center rounded-full border",
          primary && "border-emerald-500 bg-emerald-500 text-white",
        )}
      >
        {primary && <Check className="size-3" />}
      </span>

      <span className="min-w-0 flex-1">
        <span className="block truncate font-medium">
          {candidate.provider_name}
          {!candidate.enabled && (
            <span className="text-muted-foreground ml-1.5">（已停用）</span>
          )}
        </span>
        <span className="text-muted-foreground block truncate font-mono text-[11px]">
          {candidate.provider_tag}
          {upstream ? ` · 上游 ${upstream}` : ""}
        </span>
      </span>

      <span className="text-muted-foreground shrink-0 tabular-nums">
        优先级 {candidate.priority}
        {candidate.weight !== 1 && ` · 权重 ${candidate.weight}`}
      </span>

      <span className="w-16 shrink-0 text-right tabular-nums">
        {candidate.latency_ms != null ? (
          <span className="text-muted-foreground">
            {formatMs(candidate.latency_ms)}
          </span>
        ) : (
          <span className="text-muted-foreground/50">—</span>
        )}
      </span>
    </button>
  );
}
