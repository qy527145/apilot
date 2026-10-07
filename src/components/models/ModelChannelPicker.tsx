import { useState } from "react";
import { useMutation } from "@tanstack/react-query";
import { toast } from "sonner";
import { Check, Gauge, RotateCcw } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  api,
  type ModelCandidate,
  type ModelCatalogEntry,
  type ModelStrategy,
} from "@/lib/api";
import { cn, formatMs } from "@/lib/utils";

const STRATEGY_LABEL: Record<ModelStrategy, string> = {
  priority: "按优先级",
  latency: "按延迟",
  weight: "按权重随机",
};

/**
 * 「同一个模型在多个渠道都有时用哪个」。
 *
 * 界面上它属于**路由页**：模型页只决定模型名，渠道归谁由这里和 selector 决定。
 * 但运行时那条界线没变 —— 模型配过策略就按策略走，没配过才回落 selector。
 */
export function ModelChannelPicker({
  models,
  onInvalidate,
}: {
  models: ModelCatalogEntry[];
  onInvalidate: () => void;
}) {
  return (
    <Card className="py-0">
      <CardHeader className="gap-2 py-3">
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-medium">模型的渠道选择</span>
          <Badge variant="outline">{models.length} 个模型</Badge>
        </div>
        <p className="text-muted-foreground text-xs">
          同一个模型有多个渠道时用哪个。配过策略的模型按这里的选法走；
          没配过的仍交给上面的选择器与路由规则 —— 想统一交给规则管，就点「交回路由规则」。
        </p>
      </CardHeader>

      <CardContent className="space-y-3 pt-0">
        {models.length === 0 && (
          <p className="text-muted-foreground text-xs">
            还没有渠道声明任何模型。先去「渠道管理」添加。
          </p>
        )}
        {models.map((entry) => (
          <ModelRow key={entry.model} entry={entry} onInvalidate={onInvalidate} />
        ))}
      </CardContent>
    </Card>
  );
}

function ModelRow({
  entry,
  onInvalidate,
}: {
  entry: ModelCatalogEntry;
  onInvalidate: () => void;
}) {
  const [probing, setProbing] = useState(false);

  const resolve = useMutation({
    mutationFn: (tag: string) => api.switchModelChannel(entry.model, tag),
    onSuccess: (p) => {
      onInvalidate();
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
    onSuccess: onInvalidate,
  });

  const reset = useMutation({
    mutationFn: () => api.resetModelPolicy(entry.model),
    onSuccess: () => {
      onInvalidate();
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
      onInvalidate();
      const ok = results.filter((r) => r.ok).length;
      toast.success(`测速完成：${ok}/${results.length} 个渠道可用`);
    },
  });

  const managed = !!entry.policy;
  const strategy = entry.policy?.strategy ?? "priority";
  const hasLatency = entry.candidates.some((c) => c.latency_ms != null);

  return (
    <div className="space-y-2 rounded-md border p-3">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-mono text-sm font-medium">{entry.model}</span>
        <Badge variant="outline">{entry.candidates.length} 个渠道</Badge>
        {managed ? (
          <Badge variant="default">由这里决定</Badge>
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
    </div>
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
