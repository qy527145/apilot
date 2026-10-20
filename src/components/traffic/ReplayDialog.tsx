import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { Loader2, Play } from "lucide-react";

import { EmptyState } from "@/components/common/EmptyState";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
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
import { qk } from "@/hooks/queries";
import {
  api,
  PROTOCOL_LABEL,
  type ContentBlock,
  type Protocol,
  type RequestDetail,
  type UnifiedUsage,
} from "@/lib/api";
import { formatMs, formatNumber } from "@/lib/utils";

interface Props {
  detail: RequestDetail;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/**
 * 「自动」在 Select 里的哨兵值。
 *
 * Radix 不接受空字符串作为 `SelectItem` 的值，而 `null` 又表达不了"这个选项
 * 本身是有效的"。后端把某个值当"未指定"处理即可 —— 真正的判定在
 * `replay.rs::resolve_provider`。
 */
const AUTO_PROVIDER = "__auto__";

const AUTO_HINT = "自动：按模型策略与渠道声明选择；只改模型时会自动换到支持它的渠道。";

/** 内容块里只取能直接读的文字；工具调用给一行占位。 */
function textOf(content: ContentBlock[] | undefined): string {
  return (content ?? [])
    .map((b) => {
      if (b.type === "text") return b.text;
      if (b.type === "thinking") return b.text ? `[思考] ${b.text}` : "[思考]";
      if (b.type === "tool_use") return `\n[工具调用 ${b.name}]\n`;
      return "";
    })
    .join("");
}

/** 原始响应：优先用语义化视图，解不出来再退回拼接文本 / 原始体。 */
function originalText(d: RequestDetail): string {
  const decoded =
    d.views.streamed_response?.value ?? d.views.client_response?.value;
  if (decoded) return textOf(decoded.content);
  return d.stream_text || d.response_body || "";
}

/** 协议名可能是老日志里的自由文本，认不出就原样显示。 */
function protocolLabel(p: string): string {
  return PROTOCOL_LABEL[p as Protocol] ?? p;
}

function usageLine(
  u:
    | Pick<
        UnifiedUsage,
        "input_tokens" | "output_tokens" | "cache_read_tokens"
      >
    | null
    | undefined,
): string {
  if (!u) return "上游未返回用量";
  return `输入 ${formatNumber(u.input_tokens)} · 输出 ${formatNumber(
    u.output_tokens,
  )} · 缓存读 ${formatNumber(u.cache_read_tokens)}`;
}

/** 一侧的指标行。 */
function Metrics({ items }: { items: [string, string][] }) {
  return (
    <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-xs">
      {items.map(([k, v]) => (
        <div key={k} className="contents">
          <dt className="text-muted-foreground">{k}</dt>
          <dd className="truncate font-mono" title={v}>
            {v}
          </dd>
        </div>
      ))}
    </dl>
  );
}

/**
 * 回放 / A-B 对比。
 *
 * 独立一个弹窗而不是塞进详情页：一次回放是**真实出站请求**（会把请求内容发给
 * 另一个渠道），需要用户先明确选择目标；而详情页是只读视图，不该混进副作用。
 */
export function ReplayDialog({ detail, open, onOpenChange }: Props) {
  // 默认「自动」而不是原渠道：用户最常见的操作是改目标模型，此时原渠道未必
  // 服务新模型。显式选渠道仍然保留，只是不再是默认。
  const [providerTag, setProviderTag] = useState(AUTO_PROVIDER);
  const [model, setModel] = useState(detail.model);

  // 换一条请求就把目标恢复成默认值，别把上一条的选择带过来。
  useEffect(() => {
    setProviderTag(AUTO_PROVIDER);
    setModel(detail.model);
  }, [detail.request_id, detail.model]);

  const providers = useQuery({
    queryKey: qk.providers,
    queryFn: api.listProviders,
    enabled: open,
    retry: 1,
  });

  const modelOptions = useQuery({
    queryKey: ["model_options"] as const,
    queryFn: api.listModelOptions,
    enabled: open,
    retry: 1,
  });

  const run = useMutation({
    mutationFn: () =>
      api.replayRequest(
        detail.request_id,
        providerTag === AUTO_PROVIDER ? null : providerTag || null,
        model || null,
      ),
  });

  const original = useMemo(() => originalText(detail), [detail]);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="flex max-h-[88vh] flex-col gap-4 overflow-hidden sm:max-w-5xl">
        <DialogHeader className="shrink-0">
          <DialogTitle>回放对比</DialogTitle>
          <DialogDescription>
            把这条请求重发到指定渠道 / 模型，与原始回答并排比较。
            <span className="text-amber-600 dark:text-amber-400">
              {" "}
              这是一次真实请求，请求内容会发送到所选渠道；不写日志、不计费。
            </span>
          </DialogDescription>
        </DialogHeader>

        <div className="grid shrink-0 gap-3 sm:grid-cols-2">
          <div className="space-y-2">
            <Label>目标渠道</Label>
            <Select value={providerTag} onValueChange={setProviderTag}>
              <SelectTrigger>
                <SelectValue placeholder="选择渠道" />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value={AUTO_PROVIDER}>自动（按模型选择）</SelectItem>
                {/* 停用的渠道不在注册表里，选了也只会得到"渠道不存在" ——
                    直接在列表里排除，别让用户先踩一脚。 */}
                {(providers.data ?? [])
                  .filter((p) => p.enabled)
                  .map((p) => (
                    <SelectItem key={p.tag} value={p.tag}>
                      {p.name}（{p.tag}）
                    </SelectItem>
                  ))}
              </SelectContent>
            </Select>
            <p className="text-muted-foreground text-xs">
              {providerTag === AUTO_PROVIDER
                ? AUTO_HINT
                : "已手动指定渠道；这里不会替你检查它是否声明了目标模型。"}
            </p>
          </div>
          <div className="space-y-2">
            <Label>目标模型</Label>
            <Input
              value={model}
              onChange={(e) => setModel(e.target.value)}
              list="apilot-replay-models"
              placeholder="客户端可见的模型名"
            />
            <datalist id="apilot-replay-models">
              {(modelOptions.data ?? []).map((m) => (
                <option key={m.model} value={m.model} />
              ))}
            </datalist>
          </div>
        </div>

        <div className="flex shrink-0 items-center gap-3">
          <Button
            size="sm"
            onClick={() => run.mutate()}
            disabled={run.isPending || !model.trim()}
          >
            {run.isPending ? (
              <Loader2 className="size-4 animate-spin" />
            ) : (
              <Play className="size-4" />
            )}
            {run.isPending ? "请求中…" : "开始回放"}
          </Button>
          <span className="text-muted-foreground text-xs">
            回放一律按非流式发送，才能拿到完整回答比较。
          </span>
        </div>

        {run.error ? (
          <p className="text-destructive shrink-0 text-xs">
            {(run.error as Error).message}
          </p>
        ) : null}

        <div className="min-h-0 flex-1 overflow-y-auto">
          {!run.data ? (
            <EmptyState
              title="还没有回放结果"
              description="选好目标渠道与模型后点「开始回放」。"
            />
          ) : (
            <div className="grid gap-4 lg:grid-cols-2">
              <section className="space-y-3 rounded-md border p-3">
                <h4 className="text-sm font-medium">原始响应</h4>
                <Metrics
                  items={[
                    ["渠道", detail.provider_tag ?? "—"],
                    ["模型", detail.model],
                    ["耗时", formatMs(detail.latency_ms)],
                    ["用量", usageLine(detail)],
                    [
                      "协议",
                      `${protocolLabel(detail.protocol_in)} → ${protocolLabel(
                        detail.protocol_out,
                      )}`,
                    ],
                  ]}
                />
                <pre className="max-h-80 overflow-y-auto whitespace-pre-wrap break-words rounded bg-muted/40 p-2 font-mono text-xs">
                  {original || "（没有捕获到可读的响应正文）"}
                </pre>
              </section>

              <section className="space-y-3 rounded-md border p-3">
                <h4 className="flex items-center gap-2 text-sm font-medium">
                  回放结果
                  <span
                    className={
                      run.data.ok
                        ? "text-emerald-600 text-xs dark:text-emerald-400"
                        : "text-destructive text-xs"
                    }
                  >
                    {run.data.ok ? "成功" : "失败"}
                  </span>
                </h4>
                <Metrics
                  items={[
                    ["渠道", `${run.data.provider_name}（${run.data.provider_tag}）`],
                    ["模型", `${run.data.model} → ${run.data.upstream_model}`],
                    ["耗时", formatMs(run.data.latency_ms)],
                    ["状态", run.data.status ? String(run.data.status) : "—"],
                    ["协议", protocolLabel(run.data.wire)],
                    ["用量", usageLine(run.data.usage)],
                    ["URL", run.data.url || "—"],
                  ]}
                />
                {run.data.error ? (
                  <p className="text-destructive whitespace-pre-wrap text-xs">
                    {run.data.error}
                  </p>
                ) : null}
                <pre className="max-h-80 overflow-y-auto whitespace-pre-wrap break-words rounded bg-muted/40 p-2 font-mono text-xs">
                  {run.data.text ||
                    textOf(run.data.content) ||
                    "（没有可读的响应正文）"}
                </pre>
              </section>
            </div>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
