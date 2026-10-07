import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { CloudDownload, FlaskConical, ListChecks, Loader2, Plus, Trash2 } from "lucide-react";
import { toast } from "sonner";

import { CatalogCapabilitiesDialog } from "@/components/providers/CatalogCapabilitiesDialog";
import { ModelPickerDialog } from "@/components/providers/ModelPickerDialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { qk } from "@/hooks/queries";
import {
  api,
  ALL_CAPABILITIES,
  CAPABILITY_LABEL,
  CAPABILITY_VERDICT_LABEL,
  type Capability,
  type CapabilityRecord,
  type CapabilityVerdict,
  type OneshotOutcome,
} from "@/lib/api";

interface Row {
  model: string;
  /** 上游名。这里只读展示 —— 改它去路由页的「模型的渠道选择」。 */
  upstream_model: string;
}

/** 该行实际会发给上游的模型名：留空表示与声明名同名。 */
const effectiveUpstream = (r: Row) => (r.upstream_model || r.model).trim();

type ProbeState = Record<string, Capability | "loading" | "done">;

/**
 * 渠道行展开后的模型声明编辑器：**这个渠道支持哪些模型**。
 *
 * 只管"有哪些"，不管"叫什么"——上游名归路由页的「模型的渠道选择」管（那边按
 * 「模型 × 渠道」列候选，是同一张 provider_models 表的另一个视角）。
 * 注意 ModelChannelPicker 这个组件虽然在 components/models/ 目录下，但渲染它的是
 * RoutingPage —— 别按目录名猜成模型页，界面上它属于路由页。
 * 两处都能全量替换自己那一维，
 * 所以这里的保存必须按模型名保留已有的上游名，见 `providers::set_models`。
 */
export function ProviderModelsPanel({
  providerId,
  providerName,
}: {
  providerId: number;
  providerName: string;
}) {
  const qc = useQueryClient();
  const { data, isLoading, isError, refetch } = useQuery({
    queryKey: qk.providerModels(providerId),
    queryFn: () => api.listProviderModels(providerId),
    retry: 1,
  });

  const { data: capabilities } = useQuery({
    queryKey: qk.capabilities(providerId),
    queryFn: () => api.listCapabilities(providerId),
    retry: 1,
  });

  const [rows, setRows] = useState<Row[]>([]);
  const [pickerOpen, setPickerOpen] = useState(false);
  const [capsOpen, setCapsOpen] = useState(false);
  /** 正在探测的 (模型, 能力)。值是 `"loading"` 或刚探完的能力名。 */
  const [probing, setProbing] = useState<ProbeState>({});
  /** 每个模型最近一次连通测试的结果。 */
  const [tests, setTests] = useState<Record<string, OneshotOutcome | "loading">>({});

  useEffect(() => {
    if (data) {
      setRows(
        data.length > 0
          ? data.map((m) => ({ model: m.model, upstream_model: m.upstream_model ?? "" }))
          : [{ model: "", upstream_model: "" }],
      );
    }
  }, [data]);

  /** `模型名 → 能力 → 记录`，供每行渲染三枚徽标。 */
  const capIndex = useMemo(() => {
    const idx: Record<string, Partial<Record<Capability, CapabilityRecord>>> = {};
    for (const c of capabilities ?? []) {
      (idx[c.model] ??= {})[c.capability] = c;
    }
    return idx;
  }, [capabilities]);

  const save = useMutation({
    mutationFn: () =>
      api.setProviderModels(
        providerId,
        rows.map((r) => r.model.trim()).filter(Boolean),
      ),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.providerModels(providerId) });
      // 模型页的目录是从 provider_models 派生的，声明变了它也得变。
      qc.invalidateQueries({ queryKey: qk.modelCatalog });
      // 接管页的就绪判定取决于"有没有声明过模型"，改完得让它重新问一次后端。
      qc.invalidateQueries({ queryKey: qk.takeoverReadiness });
      // 能力是按「渠道 × 模型」存的，删掉某个模型声明后它的能力行也该跟着消失。
      qc.invalidateQueries({ queryKey: qk.capabilities(providerId) });
      toast.success("已保存该渠道支持的模型");
    },
  });

  const update = (idx: number, value: string) => {
    setRows((rs) =>
      rs.map((r, i) => (i === idx ? { ...r, model: value } : r)),
    );
  };

  const selectedUpstreams = useMemo(
    () => rows.map(effectiveUpstream).filter(Boolean),
    [rows],
  );

  /** 探测一项能力。一次只发一项 —— 每项都要花一次请求的 token。 */
  const runProbe = async (model: string, capability: Capability) => {
    const key = `${model}:${capability}`;
    setProbing((p) => ({ ...p, [key]: "loading" }));
    try {
      await api.probeCapability(providerId, model, capability);
      qc.invalidateQueries({ queryKey: qk.capabilities(providerId) });
    } catch {
      /* toast 已在 api 层弹出 */
    } finally {
      setProbing((p) => {
        const next = { ...p };
        delete next[key];
        return next;
      });
    }
  };

  const runTest = async (model: string) => {
    setTests((t) => ({ ...t, [model]: "loading" }));
    try {
      const res = await api.testModel(providerId, model);
      setTests((t) => ({ ...t, [model]: res }));
    } catch {
      /* toast 已在 api 层弹出 */
    }
  };

  /**
   * 把下拉选择器的结果合并进当前行。
   *
   * `known` 是本次从上游拉到的模型全集 —— 只有落在这个集合里的行才归选择器管：
   * 手工敲进去的、或上游已经下架的模型名不在集合里，一律原样保留，
   * 免得"点一下应用选择"就把用户手填的行删掉。
   */
  const applyPicked = (picked: string[], known: string[]) => {
    const pickedSet = new Set(picked);
    const knownSet = new Set(known);

    setRows((rs) => {
      const kept = rs.filter((r) => {
        const eff = effectiveUpstream(r);
        if (!eff || !knownSet.has(eff)) return true;
        return pickedSet.has(eff);
      });

      const have = new Set(kept.map(effectiveUpstream).filter(Boolean));
      const added: Row[] = picked
        .filter((id) => !have.has(id))
        .map((id) => ({ model: id, upstream_model: "" }));

      const next = [
        ...kept.filter((r) => r.model.trim() || effectiveUpstream(r)),
        ...added,
      ];
      // 不能留下空列表：面板没有"零行"状态，用户会以为界面坏了。
      return next.length > 0 ? next : [{ model: "", upstream_model: "" }];
    });
  };

  if (isLoading) {
    return (
      <div className="space-y-2 p-4">
        <Skeleton className="h-8 w-full" />
        <Skeleton className="h-8 w-full" />
      </div>
    );
  }

  if (isError) {
    return (
      <div className="p-4 text-xs">
        <span className="text-destructive">加载模型声明失败。 </span>
        <button className="underline" onClick={() => refetch()}>
          重试
        </button>
      </div>
    );
  }

  return (
    // whitespace-normal 是必须的:这个面板挂在 TableCell 里,而 TableCell 基类带
    // whitespace-nowrap（表格式的默认）。不显式盖掉，下面那段说明文字会变成一整行
    // 不换行 —— 实测宽 1516px，把整张表从 1006 撑到 1548，所有列被推出屏幕。
    // 放在组件自己身上而不是调用处的 td 上：这是组件自身的排版责任，
    // 换个地方挂（比如以后的抽屉）也不该重蹈覆辙。
    <div className="bg-muted/30 space-y-3 rounded-md p-4 whitespace-normal">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-xs font-medium">该渠道支持的模型</p>
        <div className="flex items-center gap-2">
          <Button
            variant="outline"
            size="sm"
            onClick={() => setCapsOpen(true)}
          >
            <ListChecks className="size-4" />
            从目录更新能力
          </Button>
          <Button
            variant="outline"
            size="sm"
            onClick={() => setPickerOpen(true)}
          >
            <CloudDownload className="size-4" />
            从上游获取
          </Button>
          <Button
            size="sm"
            onClick={() => save.mutate()}
            disabled={save.isPending}
          >
            {save.isPending ? "保存中…" : "保存"}
          </Button>
        </div>
      </div>

      <p className="text-muted-foreground text-xs">
        这份清单决定模型页里出现哪些模型、路由时哪些模型名会选中这条渠道。
        新建渠道时会自动从上游拉一份填进来；拉不到（服务商没有{" "}
        <code>/v1/models</code>）或想增删，在这里改就好。
        每行右边的「→ 上游名」是这个渠道实际发给上游的模型名，只读 —— 要改去
        <b>路由页</b>的「模型的渠道选择」。
        一个模型都不声明的渠道视为「通吃」，任何模型名都会转发过去。
      </p>

      <div className="space-y-2">
        {rows.map((r, idx) => {
          const upstream = r.upstream_model.trim();
          const declared = r.model.trim();
          return (
            <div
              key={idx}
              className="bg-background/40 space-y-2 rounded-md border px-2 py-2"
            >
              <div className="flex items-center gap-2">
                <Input
                  value={r.model}
                  placeholder="客户端发来的模型名，例如 claude-sonnet-4-5"
                  onChange={(e) => update(idx, e.target.value)}
                  className="flex-1"
                />
                {upstream && upstream !== declared && (
                  <span
                    className="text-muted-foreground shrink-0 truncate font-mono text-[11px]"
                    title={`实际发给上游：${upstream}`}
                  >
                    → {upstream}
                  </span>
                )}
                {declared && (
                  <TestButton
                    model={declared}
                    result={tests[declared]}
                    onTest={() => runTest(declared)}
                  />
                )}
                <Button
                  variant="ghost"
                  size="icon"
                  onClick={() =>
                    setRows((rs) => {
                      const next = rs.filter((_, i) => i !== idx);
                      return next.length > 0
                        ? next
                        : [{ model: "", upstream_model: "" }];
                    })
                  }
                >
                  <Trash2 className="size-4 text-destructive" />
                </Button>
              </div>

              {declared && (
                <div className="flex flex-wrap items-center gap-1.5 pl-0.5">
                  {ALL_CAPABILITIES.map((cap) => (
                    <CapabilityChip
                      key={cap}
                      capability={cap}
                      record={capIndex[declared]?.[cap]}
                      busy={probing[`${declared}:${cap}`] === "loading"}
                      onProbe={() => runProbe(declared, cap)}
                    />
                  ))}
                </div>
              )}
            </div>
          );
        })}
      </div>

      <Button
        variant="outline"
        size="sm"
        onClick={() => setRows((rs) => [...rs, { model: "", upstream_model: "" }])}
      >
        <Plus className="size-4" />
        添加模型
      </Button>

      <ModelPickerDialog
        open={pickerOpen}
        onOpenChange={setPickerOpen}
        providerId={providerId}
        providerName={providerName}
        selected={selectedUpstreams}
        onConfirm={applyPicked}
      />

      <CatalogCapabilitiesDialog
        open={capsOpen}
        onOpenChange={setCapsOpen}
        providerId={providerId}
        providerName={providerName}
      />
    </div>
  );
}

/**
 * 一枚能力徽标。点一下就实测一次。
 *
 * 三态各有各的样子，**「未知」必须和「不支持」长得不一样** —— 把两者都画成
 * 否定色，用户就会把"测了但看不出来"当成"确认不支持"，然后白白弃用一条好渠道。
 */
function CapabilityChip({
  capability,
  record,
  busy,
  onProbe,
}: {
  capability: Capability;
  record?: CapabilityRecord;
  busy: boolean;
  onProbe: () => void;
}) {
  const verdict: CapabilityVerdict | undefined = record?.verdict;
  const variant =
    verdict === "supported"
      ? "success"
      : verdict === "unsupported"
        ? "destructive"
        : verdict === "inconclusive"
          ? "warning"
          : "outline";

  const body = (
    <button
      type="button"
      onClick={onProbe}
      disabled={busy}
      className="cursor-pointer disabled:cursor-wait"
    >
      <Badge
        variant={variant}
        className={busy ? "opacity-60" : undefined}
      >
        {busy && <Loader2 className="size-3 animate-spin" />}
        {CAPABILITY_LABEL[capability]}
        {verdict ? ` · ${CAPABILITY_VERDICT_LABEL[verdict]}` : " · 未测"}
      </Badge>
    </button>
  );

  const detail = record?.evidence
    ? `${record.source === "probe" ? "实测" : "目录"}：${record.evidence}`
    : `点一下实测这项能力（会向上游发一次真实请求）`;

  return (
    <Tooltip>
      <TooltipTrigger asChild>{body}</TooltipTrigger>
      <TooltipContent className="max-w-xs text-xs">{detail}</TooltipContent>
    </Tooltip>
  );
}

/** 单次模型测试：往这个渠道真发一条最短的对话请求。 */
function TestButton({
  model,
  result,
  onTest,
}: {
  model: string;
  result: OneshotOutcome | "loading" | undefined;
  onTest: () => void;
}) {
  if (result === "loading") {
    return (
      <Button variant="ghost" size="icon" disabled>
        <Loader2 className="size-4 animate-spin" />
      </Button>
    );
  }

  const label = !result
    ? null
    : result.ok
      ? `${result.latency_ms} ms`
      : "失败";

  const trigger = (
    <Button
      variant="ghost"
      size={label ? "sm" : "icon"}
      className={
        result ? (result.ok ? "text-emerald-500" : "text-destructive") : undefined
      }
      onClick={onTest}
    >
      {label ?? <FlaskConical className="size-4" />}
    </Button>
  );

  return (
    <Tooltip>
      <TooltipTrigger asChild>{trigger}</TooltipTrigger>
      <TooltipContent className="max-w-sm text-xs">
        <TestDetail model={model} result={result} />
      </TooltipContent>
    </Tooltip>
  );
}

function TestDetail({
  model,
  result,
}: {
  model: string;
  result: OneshotOutcome | undefined;
}) {
  if (!result) {
    return (
      <div>
        向 <code>{model}</code> 发一条最短的真实请求
        <div className="text-muted-foreground">会消耗少量 token，并计入账单</div>
      </div>
    );
  }
  return (
    <div className="space-y-1">
      <div>
        {result.ok
          ? `HTTP ${result.status} · ${result.latency_ms} ms`
          : (result.error ?? "请求失败")}
      </div>
      {result.text && (
        <div className="text-muted-foreground line-clamp-3">回答：{result.text}</div>
      )}
      {!result.text && result.preview && (
        <div className="text-muted-foreground line-clamp-3">{result.preview}</div>
      )}
      {result.url && (
        <div className="text-muted-foreground font-mono text-[10px] break-all">
          {result.url}
        </div>
      )}
    </div>
  );
}
