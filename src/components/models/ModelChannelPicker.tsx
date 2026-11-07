import { useEffect, useState } from "react";
import { useMutation } from "@tanstack/react-query";
import { ArrowDown, ArrowUp, Check, Gauge, RotateCcw } from "lucide-react";
import { toast } from "sonner";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import {
  api,
  type ModelCandidate,
  type ModelCatalogEntry,
} from "@/lib/api";
import { cn, formatMs } from "@/lib/utils";

/**
 * 「这个模型在多个渠道都有时走哪个」。
 *
 * 只有两件事：一张拖出来的顺序（写回 `provider_models.priority`，靠前的先走），
 * 以及点击某一行把它钉成「永远走这个渠道」。没有按延迟、按权重之类的自动策略 ——
 * 那些会让"现在到底走哪个"每请求变一次，界面上说不清，也没法复现。
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
          同一模型在多个渠道都有时，从上往下依次尝试，第一个成功的就是这次请求实际用的渠道。
          点某一行把它钉住 = 永远先走它；再点一次取消。
          配过的模型按这里的顺序走，没配过的仍交给路由规则与选择器。
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
  const [latency, setLatency] = useState<Record<string, number | null>>({});

  const pinned = entry.policy?.active_provider ?? null;

  /** 整份候选按顺序写回，优先级由位置决定（越靠前越大）。 */
  const writeOrder = useMutation({
    mutationFn: ({
      ordered,
      tag,
      upstream,
    }: {
      ordered: ModelCandidate[];
      tag?: string;
      upstream?: string | null;
    }) =>
      api.setModelCandidates(
        entry.model,
        ordered.map((c, i) => ({
          provider_tag: c.provider_tag,
          upstream_model:
            c.provider_tag === tag ? (upstream ?? null) : (c.upstream_model ?? null),
          priority: ordered.length - i,
          enabled: c.enabled,
        })),
      ),
    onSuccess: (_r, { tag, upstream }) => {
      onInvalidate();
      if (tag !== undefined) {
        toast.success(
          upstream ? `已改为发往上游的 ${upstream}` : "已恢复为同名",
        );
      }
    },
  });

  const pin = useMutation({
    mutationFn: (tag: string) => api.switchModelChannel(entry.model, tag),
    onSuccess: () => {
      onInvalidate();
      toast.success("已钉住这个渠道");
    },
  });

  const unpin = useMutation({
    mutationFn: () =>
      api.upsertModelPolicy({ model: entry.model, active_provider: null }),
    onSuccess: () => {
      onInvalidate();
      toast.success("已取消钉住的渠道，按上面的顺序走");
    },
  });

  const reset = useMutation({
    mutationFn: () => api.resetModelPolicy(entry.model),
    onSuccess: () => {
      onInvalidate();
      toast.success("已交回路由规则");
    },
  });

  const probe = useMutation({
    mutationFn: () => api.probeModelCandidates(entry.model),
    onMutate: () => setProbing(true),
    onSettled: () => setProbing(false),
    onSuccess: (results) => {
      // 测速结果只用于展示：选渠道不看延迟，这里不再写任何缓存。
      setLatency(
        Object.fromEntries(results.map((r) => [r.tag, r.ok ? r.latency_ms ?? 0 : null])),
      );
      const ok = results.filter((r) => r.ok).length;
      toast.success(`测速完成：${ok}/${results.length} 个渠道可用`);
    },
  });

  const ordered = [...entry.candidates].sort((a, b) => b.priority - a.priority);

  const move = (index: number, to: number) => {
    if (to < 0 || to >= ordered.length) return;
    const next = [...ordered];
    const [row] = next.splice(index, 1);
    next.splice(to, 0, row);
    writeOrder.mutate({ ordered: next });
  };

  const managed = !!entry.policy;

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

      {/* 顺序即优先级：从上往下依次尝试，第一个成功的就是实际用的渠道。 */}
      {ordered.map((c, i) => (
        <CandidateRow
          key={c.provider_tag}
          candidate={c}
          index={i}
          total={ordered.length}
          model={entry.model}
          pinned={pinned === c.provider_tag}
          latencyMs={latency[c.provider_tag]}
          saving={writeOrder.isPending}
          onMoveUp={() => move(i, i - 1)}
          onMoveDown={() => move(i, i + 1)}
          onTogglePin={() =>
            pinned === c.provider_tag ? unpin.mutate() : pin.mutate(c.provider_tag)
          }
          onSetUpstream={(upstream) =>
            writeOrder.mutate({ ordered, tag: c.provider_tag, upstream })
          }
        />
      ))}

      {ordered.length === 0 && (
        <p className="text-muted-foreground text-xs">
          还没有渠道声明这个模型。
        </p>
      )}
    </div>
  );
}

function CandidateRow({
  candidate,
  index,
  total,
  model,
  pinned,
  latencyMs,
  saving,
  onMoveUp,
  onMoveDown,
  onTogglePin,
  onSetUpstream,
}: {
  candidate: ModelCandidate;
  index: number;
  total: number;
  /** 入站模型名，用作上游名输入框的 placeholder（留空 = 同名）。 */
  model: string;
  pinned: boolean;
  latencyMs: number | null | undefined;
  saving: boolean;
  onMoveUp: () => void;
  onMoveDown: () => void;
  onTogglePin: () => void;
  onSetUpstream: (upstream: string | null) => void;
}) {
  const selectable = candidate.enabled;

  return (
    <div
      className={cn(
        "flex w-full items-center gap-3 rounded-md border px-3 py-2 text-xs",
        pinned && "border-emerald-500/50 bg-emerald-500/5",
        !selectable && "opacity-60",
      )}
    >
      <div className="flex shrink-0 flex-col">
        <Button
          variant="ghost"
          size="icon"
          className="size-5"
          disabled={index === 0}
          onClick={onMoveUp}
          title="上移"
        >
          <ArrowUp className="size-3" />
        </Button>
        <Button
          variant="ghost"
          size="icon"
          className="size-5"
          disabled={index === total - 1}
          onClick={onMoveDown}
          title="下移"
        >
          <ArrowDown className="size-3" />
        </Button>
      </div>

      {/* 点左半边即钉住 / 取消。右半边是输入框，不能包进这个 button 里 ——
          嵌套的表单控件会互相吃掉事件（回车、失焦都对不上）。 */}
      <button
        type="button"
        disabled={!selectable}
        onClick={onTogglePin}
        className={cn(
          "flex min-w-0 flex-1 items-center gap-3 text-left",
          selectable && "cursor-pointer hover:opacity-80",
        )}
      >
        <span
          className={cn(
            "flex size-4 shrink-0 items-center justify-center rounded-full border",
            pinned && "border-emerald-500 bg-emerald-500 text-white",
          )}
        >
          {pinned && <Check className="size-3" />}
        </span>

        <span className="text-muted-foreground w-5 shrink-0 text-right tabular-nums">
          {index + 1}
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

      <span className="w-16 shrink-0 text-right tabular-nums">
        {latencyMs === undefined ? (
          <span className="text-muted-foreground/50">—</span>
        ) : latencyMs === null ? (
          <span className="text-destructive">失败</span>
        ) : (
          <span className="text-muted-foreground">{formatMs(latencyMs)}</span>
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
