import { useEffect, useState } from "react";
import { useMutation } from "@tanstack/react-query";
import { toast } from "sonner";
import { Check, Gauge, RotateCcw } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
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
          <br />
          「上游名」是这个渠道真正接受的模型名：留空就同名；填上它就实现了
          <b>客户端不用改配置</b>
          —— 客户端发
          <code className="mx-1">claude-sonnet-4-5</code>
          ，照样能打到只认
          <code className="mx-1">deepseek-chat</code>
          的渠道上。
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

  // `set_model_candidates` 是按模型全量替换，所以每次都要把整份候选列表发回去，
  // 只改目标渠道那一行的上游名。
  const setUpstream = useMutation({
    mutationFn: ({ tag, upstream }: { tag: string; upstream: string | null }) =>
      api.setModelCandidates(
        entry.model,
        entry.candidates.map((c) => ({
          provider_tag: c.provider_tag,
          upstream_model:
            c.provider_tag === tag ? upstream : (c.upstream_model ?? null),
          priority: c.priority,
          weight: c.weight,
          enabled: c.enabled,
        })),
      ),
    onSuccess: (_r, { upstream }) => {
      onInvalidate();
      toast.success(
        upstream ? `已改为发往上游的 ${upstream}` : "已恢复为同名",
      );
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
          model={entry.model}
          primary={entry.primary === c.provider_tag}
          // 加权随机每次请求都重抽，没有"当前"可点。
          canSwitch={strategy !== "weight"}
          saving={setUpstream.isPending}
          onSwitch={() => resolve.mutate(c.provider_tag)}
          onSetUpstream={(upstream) =>
            setUpstream.mutate({ tag: c.provider_tag, upstream })
          }
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
  model,
  primary,
  canSwitch,
  saving,
  onSwitch,
  onSetUpstream,
}: {
  candidate: ModelCandidate;
  /** 入站模型名，用作上游名输入框的 placeholder（留空 = 同名）。 */
  model: string;
  primary: boolean;
  canSwitch: boolean;
  /** 有上游名正在落库，期间锁住输入框。 */
  saving: boolean;
  onSwitch: () => void;
  onSetUpstream: (upstream: string | null) => void;
}) {
  const switching = canSwitch && candidate.enabled;

  return (
    <div
      className={cn(
        "flex w-full items-center gap-3 rounded-md border px-3 py-2 text-xs",
        primary && "border-emerald-500/50 bg-emerald-500/5",
        (!candidate.enabled || !canSwitch) && "opacity-60",
      )}
    >
      {/* 点左半边即切换主渠道。右半边是输入框，不能包进这个 button 里 ——
          嵌套的表单控件会互相吃掉事件（回车、失焦都对不上）。 */}
      <button
        type="button"
        disabled={!switching}
        onClick={onSwitch}
        className={cn(
          "flex min-w-0 flex-1 items-center gap-3 text-left",
          switching && "cursor-pointer hover:opacity-80",
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
          </span>
        </span>
      </button>

      <UpstreamNameInput
        model={model}
        value={candidate.upstream_model ?? null}
        // 一次提交是全量替换该模型的候选列表，用提交那一刻的快照拼出来。
        // 前一次还没回来就允许改第二行，第二次会把第一次的结果覆盖回去 ——
        // 干脆在落库期间锁住，代价是几百毫秒不能输入。
        disabled={saving}
        onCommit={onSetUpstream}
      />

      <span className="text-muted-foreground w-28 shrink-0 text-right tabular-nums">
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
    </div>
  );
}

/**
 * 这个渠道真正接受的模型名。留空 = 与入站模型名同名。
 *
 * 提交交给 blur / 回车，不做成受控后立刻写库 —— 每敲一个字符发一次请求，
 * 既会打满后端，又会让"上游名"在用户还没敲完时就被别的渠道的快照覆盖。
 */
function UpstreamNameInput({
  model,
  value,
  disabled,
  onCommit,
}: {
  model: string;
  value: string | null;
  disabled?: boolean;
  onCommit: (upstream: string | null) => void;
}) {
  const [draft, setDraft] = useState(value ?? "");

  // 保存成功后保存的那份会被重新拉回来，跟着它走即可。正在编辑时不会被打断：
  // 提交之前 value 一直是旧值，draft 与它不同只可能是用户自己敲的。
  useEffect(() => {
    setDraft(value ?? "");
  }, [value]);

  const commit = () => {
    const next = draft.trim();
    if (next === (value ?? "").trim()) return;
    // 空串是"同名"，落到库里是 NULL —— 存一个空字符串会让调用方
    // 分不清"没配"和"配成了空"。
    onCommit(next || null);
  };

  return (
    <Input
      value={draft}
      disabled={disabled}
      placeholder={model}
      title="该渠道真正接受的模型名；留空表示同名"
      className="h-7 w-44 shrink-0 font-mono text-[11px]"
      onChange={(e) => setDraft(e.target.value)}
      onBlur={commit}
      onKeyDown={(e) => {
        if (e.key === "Enter") {
          e.currentTarget.blur();
        } else if (e.key === "Escape") {
          setDraft(value ?? "");
          e.currentTarget.blur();
        }
      }}
    />
  );
}
