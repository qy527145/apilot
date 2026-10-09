import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { ArrowRight, Loader2 } from "lucide-react";

import { CopyButton } from "@/components/common/CopyButton";
import { JsonViewer, tryParseJson } from "@/components/common/JsonViewer";
import { RawBody } from "@/components/common/RawBody";
import { ResponseView, RequestView } from "@/components/traffic/InspectViews";
import {
  StreamTimeline,
  parseTimings,
  type TimingEntry,
} from "@/components/traffic/StreamTimeline";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { qk } from "@/hooks/queries";
import { pairTimingsWithEvents, sseData, sseTextFragment } from "@/lib/sse";
import {
  api,
  PROTOCOL_LABEL,
  type DecodedResponse,
  type Protocol,
  type RequestDetail,
  type UnifiedRequest,
  type UnifiedResponse,
  type UnifiedUsage,
} from "@/lib/api";
import {
  formatMs,
  formatNumber,
  formatTime,
  quotaToUsd,
} from "@/lib/utils";

interface Props {
  requestId: string | null;
  onOpenChange: (open: boolean) => void;
}

/** 三种查看方式。表达的是"我在核对报文"还是"我在看语义"，与具体是哪一段无关。 */
type ViewMode = "visual" | "formatted" | "raw";

/** 顶层页签。时间轴单独一个 —— 它不看报文，看的是每个事件花了多久。 */
type DetailTab = "request" | "response" | "timeline";

/**
 * 响应侧「原始」视图取哪份数据。
 *
 * 流式要呈现的是**完整的 SSE 事件流**，不是拼接后的模型回答 —— 后者丢了事件
 * 结构，排查时看不出上游到底发了哪些事件、终止帧长什么样。
 *
 * 直通时客户端收到的就是上游字节，所以客户端侧直接复用上游帧；后端正是因此
 * 不重复存第二份。
 *
 * 按优先级取，而不是按"是不是流式"二选一：客户端要了流、上游却回了一整个 JSON
 * 的情况很常见（有些中转会无视 `stream`），那时 `is_stream` 为真但没有 SSE 帧，
 * 该显示的就是那份完整响应体。
 */
function rawOf(
  d: RequestDetail,
  side: "client" | "upstream",
): { raw: string | null; rawNote?: string } {
  const own = side === "client" ? d.client_stream_raw : d.upstream_stream_raw;
  if (own) {
    return {
      raw: own,
      rawNote: side === "client" ? "重编码后发给客户端的 SSE 帧" : "上游发出的 SSE 帧",
    };
  }

  if (side === "client" && d.upstream_stream_raw) {
    return { raw: d.upstream_stream_raw, rawNote: "直通：与上游 SSE 帧逐字节相同" };
  }

  const body = side === "client" ? d.response_body : d.upstream_response_body;
  if (body) {
    return {
      raw: body,
      rawNote: d.is_stream ? "上游无视 stream 参数，回了完整响应" : undefined,
    };
  }

  // 改动之前的旧日志没有原始帧，只有拼接后的文本 —— 给出来但要说清它是什么。
  if (d.stream_text) {
    return {
      raw: d.stream_text,
      rawNote: "该请求未保存原始 SSE 帧（改动前的旧日志），以下是拼接后的文本",
    };
  }

  return { raw: null };
}

/**
 * 响应侧「格式化」视图取哪份数据。
 *
 * 流式没有完整响应体，但落库了由增量重建出的 IR（`response_content`）——
 * 那才是这次响应的完整形态，比整块显示"未捕获"有用得多。
 */
function formattedOf(d: RequestDetail, side: "client" | "upstream"): string | null {
  return (
    (side === "client"
      ? d.response_body
      : d.upstream_response_body) ?? d.response_content ?? null
  );
}


export function RequestDetailDialog({ requestId, onOpenChange }: Props) {
  const [view, setView] = useState<ViewMode>("visual");
  const [tab, setTab] = useState<DetailTab>("request");

  const { data, isLoading, isError, refetch } = useQuery({
    queryKey: qk.requestDetail(requestId ?? ""),
    queryFn: () => api.getRequestDetail(requestId as string),
    enabled: !!requestId,
    retry: 1,
  });

  // 时间轴的数据是**后加的**：改动之前落库的请求没有它。没有就不显示那个页签，
  // 而不是给一份空轴让人以为这个请求一个事件都没发。
  const timeline = parseTimings(data?.stream_timings);
  // 换了一条没有时间轴的请求时，别停在那个已经消失的页签上 —— 那样正文会是空白。
  const activeTab: DetailTab =
    tab === "timeline" && !timeline ? "request" : tab;

  // 选中的是时间轴上第几个事件（从 1 起，与 `StreamTimeline` 给出的序号一致）。
  const [selectedSeq, setSelectedSeq] = useState<number | null>(null);

  // 换一条请求就清掉选中：序号是"第几个"，留着它会正好落在另一条请求的轴上别处。
  useEffect(() => {
    setSelectedSeq(null);
  }, [requestId]);

  // 时间轴只知道"第几个事件什么时候到"，不知道它发了什么。正文得从同一份捕获里的
  // 原始 SSE 帧按序号取回来 —— 两者是同一次遍历里写下的，序号天然对齐。
  const events = useMemo(
    () =>
      pairTimingsWithEvents(
        timeline?.entries.length ?? 0,
        data?.upstream_stream_raw,
      ),
    [timeline, data?.upstream_stream_raw],
  );

  return (
    <Dialog open={!!requestId} onOpenChange={onOpenChange}>
      {/*
       * 弹窗必须有高度上限并把正文做成可滚动区。`min-h-0` 是关键：
       * flex 子项默认 min-height:auto，不加就不会收缩，滚动条也不会出现。
       */}
      <DialogContent className="flex max-h-[88vh] flex-col gap-4 overflow-hidden sm:max-w-5xl">
        <DialogHeader className="shrink-0">
          <DialogTitle>请求详情</DialogTitle>
          <DialogDescription className="font-mono text-xs">
            {requestId}
          </DialogDescription>
        </DialogHeader>

        {isLoading ? (
          <div className="flex items-center justify-center gap-2 py-12 text-sm text-muted-foreground">
            <Loader2 className="size-4 animate-spin" />
            加载中…
          </div>
        ) : isError || !data ? (
          <div className="py-10 text-center text-xs">
            <span className="text-destructive">加载失败。 </span>
            <button className="underline" onClick={() => refetch()}>
              重试
            </button>
          </div>
        ) : (
          <div className="min-h-0 flex-1 space-y-4 overflow-y-auto pr-1">
            <Summary data={data} />
            <Metrics data={data} />

            {data.error_message && (
              <div className="border-destructive/40 bg-destructive/10 text-destructive rounded-md border p-3 text-xs break-words">
                {data.error_message}
              </div>
            )}

            <Tabs value={activeTab} onValueChange={(v) => setTab(v as DetailTab)}>
              <div className="flex flex-wrap items-center justify-between gap-2">
                <TabsList>
                  <TabsTrigger value="request">请求</TabsTrigger>
                  <TabsTrigger value="response">响应</TabsTrigger>
                  {timeline && <TabsTrigger value="timeline">时间轴</TabsTrigger>}
                </TabsList>
                {/* 「可视化 / 格式化 / 原始」说的是报文怎么看，时间轴上没有报文。 */}
                {activeTab !== "timeline" && (
                  <ViewToggle value={view} onChange={setView} />
                )}
              </div>

              <TabsContent value="request">
                <DirectionPane
                  view={view}
                  directions={[
                    {
                      key: "inbound",
                      label: "客户端 → Apilot",
                      headline: `${data.method || "POST"} ${data.path || "—"}`,
                      protocol: data.protocol_in,
                      headers: data.request_headers,
                      formatted: data.request_body,
                      raw: data.request_body,
                      error: data.views.inbound_request?.error,
                      decoded: data.views.inbound_request?.value,
                      kind: "request",
                    },
                    {
                      key: "upstream",
                      label: "Apilot → 上游",
                      headline: data.upstream_url ?? "未请求上游",
                      protocol: data.protocol_out,
                      headers: data.upstream_headers,
                      formatted: data.upstream_body,
                      raw: data.upstream_body,
                      error: data.views.upstream_request?.error,
                      decoded: data.views.upstream_request?.value,
                      kind: "request",
                    },
                  ]}
                  unavailable="这次没有请求上游，没有出站报文"
                />
              </TabsContent>

              <TabsContent value="response">
                <DirectionPane
                  view={view}
                  directions={[
                    {
                      key: "client",
                      label: "Apilot → 客户端",
                      headline: `HTTP ${data.status_code}`,
                      protocol: data.protocol_in,
                      headers: data.response_headers,
                      formatted: formattedOf(data, "client"),
                      ...rawOf(data, "client"),
                      error: data.views.client_response?.error,
                      decoded: data.views.client_response?.value,
                      usage: data.views.client_response?.usage,
                      streamed: data.views.streamed_response,
                      kind: "response",
                    },
                    {
                      key: "upstream",
                      label: "上游 → Apilot",
                      headline:
                        data.upstream_status != null
                          ? `HTTP ${data.upstream_status}`
                          : "未请求上游",
                      protocol: data.protocol_out,
                      headers: data.upstream_response_headers,
                      formatted: formattedOf(data, "upstream"),
                      ...rawOf(data, "upstream"),
                      error: data.views.upstream_response?.error,
                      decoded: data.views.upstream_response?.value,
                      usage: data.views.upstream_response?.usage,
                      streamed: data.views.streamed_response,
                      kind: "response",
                    },
                  ]}
                  unavailable="这次没有请求上游，没有上游响应"
                />
              </TabsContent>

              {timeline && (
                <TabsContent value="timeline" className="pt-3">
                  {/* 左耗时、右正文放在同一屏：看到"这一下慢"的时候，
                      下一件想知道的事就是"它到底发了什么"，不该再切一次页签。
                      两列各自独立滚动：左边事件多时滚左边，右边内容长时滚右边，互不影响。 */}
                  <div className="grid h-[56vh] min-h-32 gap-3 md:grid-cols-[minmax(0,22rem)_minmax(0,1fr)]">
                    <div className="min-h-0 overflow-y-auto pr-1">
                      <StreamTimeline
                        entries={timeline.entries}
                        truncated={timeline.truncated}
                        barClassName="w-12"
                        showAbsolute={false}
                        selected={selectedSeq}
                        onSelect={setSelectedSeq}
                      />
                    </div>
                    <div className="min-h-0 overflow-y-auto">
                      <ChunkView
                        seq={selectedSeq}
                        entry={
                          selectedSeq === null
                            ? null
                            : (timeline.entries[selectedSeq - 1] ?? null)
                        }
                        block={
                          selectedSeq === null
                            ? null
                            : (events.blocks[selectedSeq - 1] ?? null)
                        }
                        mismatch={events.mismatch}
                        rawTruncated={data.stream_raw_truncated}
                      />
                    </div>
                  </div>
                </TabsContent>
              )}
            </Tabs>

            {data.is_stream && data.stream_raw_truncated && (
              <p className="text-amber-600 text-xs dark:text-amber-400">
                原始 SSE 帧超过存储上限，已截断 —— 下面的「原始」不是全部内容。
              </p>
            )}
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}

/**
 * 时间轴上选中事件的正文。
 *
 * 历史请求没有逐帧的 IR 增量（那个只存在于实时视图里），所以这里给两样：
 * 这一块**原样发出的 SSE 正文**，以及它 `data` 里的 JSON 树。前者用来核对上游
 * 到底怎么写的（字段顺序、有没有多包一层），后者用来读字段。
 */
function ChunkView({
  seq,
  entry,
  block,
  mismatch,
  rawTruncated,
}: {
  seq: number | null;
  entry: TimingEntry | null;
  block: string | null;
  mismatch: boolean;
  rawTruncated: boolean;
}) {
  if (seq === null || !entry) {
    return (
      <div className="rounded-md border border-dashed p-4">
        <p className="text-muted-foreground text-xs">
          点左边任意一个事件，这里显示它在流里发的内容。
        </p>
      </div>
    );
  }

  const data = block ? sseData(block) : null;
  const fragment = block ? sseTextFragment(block) : null;
  const parsed = data === null ? undefined : tryParseJson(data);

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-xs font-medium tabular-nums">第 {seq} 个事件</span>
        <Badge variant="outline" className="font-mono">
          {entry.name}
        </Badge>
        <span className="text-muted-foreground text-[11px] tabular-nums">
          +{formatMs(entry.at_ms)}
        </span>
      </div>

      {!block && (
        <p className="text-muted-foreground text-xs">
          {rawTruncated
            ? "原始 SSE 帧超过存储上限，这个事件的正文没有被保存下来。"
            : mismatch
              ? "原始 SSE 帧与时间轴对不上（多半是捕获被截断），因此不显示正文 —— 宁可少看一块，也不能把别的事件的内容安到这个名下。"
              : "这次请求没有保存原始 SSE 帧（改动前的旧日志），只能看到事件名与时间。"}
        </p>
      )}

      {fragment !== null && (
        <div className="space-y-1">
          <p className="text-muted-foreground text-[11px]">这一次新增的内容</p>
          <pre className="bg-muted/30 max-h-64 overflow-auto rounded-md border p-2 font-mono text-xs whitespace-pre-wrap">
            {fragment}
          </pre>
        </div>
      )}

      {parsed !== undefined && (
        <div className="space-y-1">
          <p className="text-muted-foreground text-[11px]">data 解出来的 JSON</p>
          <JsonViewer value={parsed} defaultDepth={3} />
        </div>
      )}

      {block && (
        <div className="space-y-1">
          <div className="flex items-center justify-between gap-2">
            <p className="text-muted-foreground text-[11px]">原始 SSE 事件块</p>
            <CopyButton text={block} />
          </div>
          <pre className="bg-muted/30 max-h-64 overflow-auto rounded-md border p-2 font-mono text-[11px] whitespace-pre-wrap">
            {block}
          </pre>
        </div>
      )}
    </div>
  );
}

/** 一个方向的报文：标题 + Headers + 三态视图。 */
interface Direction {
  key: string;
  label: string;
  headline: string;
  protocol: string;
  headers: Record<string, string>;
  /** 格式化视图的源串 */
  formatted?: string | null;
  /** 原始视图的源串 */
  raw?: string | null;
  rawNote?: string;
  error?: string | null;
  /** 解码出的 IR。请求方向是 UnifiedRequest，响应方向是 UnifiedResponse。 */
  decoded?: UnifiedRequest | UnifiedResponse | null;
  usage?: UnifiedUsage | null;
  /** 流式响应：可视化与用量都取自增量重建的结果 */
  streamed?: DecodedResponse | null;
  kind: "request" | "response";
}

function DirectionPane({
  directions,
  view,
  unavailable,
}: {
  directions: Direction[];
  view: ViewMode;
  unavailable: string;
}) {
  const [activeKey, setActiveKey] = useState(directions[0].key);
  const active = directions.find((d) => d.key === activeKey) ?? directions[0];

  // 两侧都没有报文时不必让人做无意义的选择。
  const hasAny = directions.some((d) => d.formatted || d.raw || d.decoded);
  if (!hasAny) {
    return (
      <p className="text-muted-foreground py-6 text-center text-xs">
        {unavailable}
      </p>
    );
  }

  // 流式响应没有完整响应体，可视化与用量都来自增量重建的那份。
  const decoded = active.streamed?.value ?? active.decoded;
  const usage = active.streamed?.usage ?? active.usage;
  const decodeError = active.streamed?.error ?? active.error;

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-2">
        <DirectionToggle
          directions={directions}
          activeKey={active.key}
          onChange={setActiveKey}
        />
        {directions.every((d) => d.protocol === directions[0].protocol) &&
          directions.length > 1 && (
            <span className="text-muted-foreground text-[11px]">
              直通，两侧协议一致
            </span>
          )}
      </div>

      <Headline
        headline={active.headline}
        protocol={active.protocol}
        headers={active.headers}
        rawMode={view === "raw"}
      />

      {view === "visual" ? (
        decoded ? (
          active.kind === "request" ? (
            <RequestView req={decoded as UnifiedRequest} />
          ) : (
            <ResponseView
              resp={decoded as UnifiedResponse}
              usage={usage ?? null}
            />
          )
        ) : (
          <DecodeFailure error={decodeError} />
        )
      ) : (
        <RawBody
          text={view === "raw" ? active.raw : active.formatted}
          rawMode={view === "raw"}
          note={view === "raw" ? active.rawNote : undefined}
          empty="未捕获报文（可能是缓存命中、路由未走到上游，或未开启捕获）"
        />
      )}
    </div>
  );
}

function DirectionToggle({
  directions,
  activeKey,
  onChange,
}: {
  directions: Direction[];
  activeKey: string;
  onChange: (k: string) => void;
}) {
  const usable = directions.filter((d) => d.formatted || d.raw || d.decoded);
  if (usable.length <= 1) return null;

  return (
    <div className="flex items-center gap-1">
      {usable.map((d) => (
        <Button
          key={d.key}
          variant={d.key === activeKey ? "default" : "outline"}
          size="sm"
          className="h-6 px-2 text-[11px]"
          onClick={() => onChange(d.key)}
        >
          {d.label}
        </Button>
      ))}
    </div>
  );
}

function ViewToggle({
  value,
  onChange,
}: {
  value: ViewMode;
  onChange: (v: ViewMode) => void;
}) {
  const modes: Array<[ViewMode, string, string]> = [
    ["visual", "可视化", "按语义展示：对话、系统提示词、工具、回答、思考、token"],
    ["formatted", "格式化", "把 JSON 排整齐"],
    ["raw", "原始", "未经任何加工的报文原文"],
  ];

  return (
    <div className="flex items-center gap-1">
      {modes.map(([key, label, hint]) => (
        <Button
          key={key}
          variant={value === key ? "default" : "outline"}
          size="sm"
          className="h-6 px-2 text-[11px]"
          title={hint}
          onClick={() => onChange(key)}
        >
          {label}
        </Button>
      ))}
    </div>
  );
}

function Headline({
  headline,
  protocol,
  headers,
  rawMode,
}: {
  headline: string;
  protocol: string;
  headers: Record<string, string>;
  rawMode: boolean;
}) {
  const [showHeaders, setShowHeaders] = useState(false);
  const count = Object.keys(headers ?? {}).length;
  // Headers 本身就是个 JSON 对象，格式化视图直接用树，与正文一致。
  const headersText = JSON.stringify(headers ?? {}, null, 2);

  return (
    <div className="space-y-1">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-mono text-xs break-all">{headline}</span>
        <Badge variant="outline" className="font-normal">
          {PROTOCOL_LABEL[protocol as Protocol] ?? protocol}
        </Badge>
        {count > 0 && (
          <button
            type="button"
            className="text-muted-foreground hover:text-foreground text-[11px] underline"
            onClick={() => setShowHeaders((v) => !v)}
          >
            Headers ({count})
          </button>
        )}
      </div>
      {showHeaders && (
        <div className="space-y-1">
          <div className="flex justify-end">
            <CopyButton text={headersText} />
          </div>
          <RawBody text={headersText} rawMode={rawMode} empty="（无）" />
        </div>
      )}
    </div>
  );
}

/** 解不出可视化时的说明。不默默留白 —— 用户需要知道为什么，以及还能看什么。 */
function DecodeFailure({ error }: { error?: string | null }) {
  return (
    <div className="bg-muted/30 space-y-1 rounded-md border p-3 text-xs">
      <p className="font-medium">这份报文没法按语义解析</p>
      <p className="text-muted-foreground break-words">
        {error ?? "没有可解析的报文。"}
      </p>
      <p className="text-muted-foreground">
        切到「格式化」或「原始」看原文。常见原因：报文是上游返回的错误体、
        流被中途截断，或它本来就不是这个协议的格式。
      </p>
    </div>
  );
}

/** 顶部徽章：状态、客户端、模型、渠道、是否流式、是否缓存命中。 */
function Summary({ data }: { data: RequestDetail }) {
  return (
    <div className="flex flex-wrap items-center gap-2">
      <Badge variant={data.status_code < 400 ? "success" : "destructive"}>
        {data.status_code}
      </Badge>
      <Badge variant="outline">{data.client}</Badge>
      <Badge variant="outline">{data.model}</Badge>
      {data.provider_tag && <Badge variant="outline">{data.provider_name ?? data.provider_tag}</Badge>}
      {data.is_stream && <Badge variant="secondary">流式</Badge>}
      {data.cache_hit && <Badge variant="success">缓存命中</Badge>}
    </div>
  );
}

/** 指标网格：用量、耗时、两个方向的模型名与状态码。 */
function Metrics({ data }: { data: RequestDetail }) {
  const converted = data.protocol_in !== data.protocol_out;

  return (
    <div className="space-y-3">
      <div className="bg-muted/30 flex flex-wrap items-center gap-2 rounded-md border p-3 text-xs">
        <span className="font-mono">
          {data.method || "POST"} {data.path || "—"}
        </span>
        <span className="text-muted-foreground">
          {PROTOCOL_LABEL[data.protocol_in as Protocol] ?? data.protocol_in}
        </span>
        <ArrowRight className="text-muted-foreground size-3.5 shrink-0" />
        <Badge variant={converted ? "secondary" : "success"}>
          {converted ? "协议转换" : "直通"}
        </Badge>
        <ArrowRight className="text-muted-foreground size-3.5 shrink-0" />
        <span
          className="max-w-[24rem] truncate font-mono"
          title={data.upstream_url ?? undefined}
        >
          {data.upstream_url ?? "未请求上游"}
        </span>
        {data.upstream_url && (
          <span className="text-muted-foreground">
            {PROTOCOL_LABEL[data.protocol_out as Protocol] ?? data.protocol_out}
          </span>
        )}
      </div>

      <div className="grid grid-cols-2 gap-x-6 gap-y-2 rounded-md border p-3 text-xs sm:grid-cols-3 lg:grid-cols-4">
        <Field label="时间" value={formatTime(data.ts)} />
        <Field label="耗时" value={formatMs(data.latency_ms)} />
        <Field label="TTFB" value={formatMs(data.ttfb_ms)} />
        <Field label="费用" value={quotaToUsd(data.quota)} />
        <Field label="输入 Token" value={formatNumber(data.input_tokens)} />
        <Field label="输出 Token" value={formatNumber(data.output_tokens)} />
        <Field label="缓存读 Token" value={formatNumber(data.cache_read_tokens)} />
        <Field label="缓存写 Token" value={formatNumber(data.cache_creation_tokens)} />
        <Field label="协议(入)" value={data.protocol_in} />
        <Field label="协议(出)" value={data.protocol_out} />
        <Field label="请求模型" value={data.request_model} />
        {/*
          三个模型名是一条链，缺一个就断在中间：客户端发的 → Apilot 决定的（计费与
          缓存按它算）→ 渠道映射后真发出去的。只显示首尾的话，"我发的 A 怎么按 B
          计费"这种问题还是查不出来。
        */}
        <Field
          label="生效模型"
          value={data.model}
          highlight={data.model !== data.request_model}
        />
        <Field
          label="上游模型"
          value={data.upstream_model ?? "—"}
          highlight={
            !!data.upstream_model && data.upstream_model !== data.model
          }
        />
        <Field label="用量来源" value={data.usage_source} />
        <Field label="上游状态码" value={data.upstream_status?.toString() ?? "—"} />
        <Field label="流事件数" value={formatNumber(data.stream_events)} />
      </div>
    </div>
  );
}

function Field({
  label,
  value,
  highlight,
}: {
  label: string;
  value: string;
  highlight?: boolean;
}) {
  return (
    <div className="flex min-w-0 items-center justify-between gap-2">
      <span className="text-muted-foreground shrink-0">{label}</span>
      <span
        className={
          highlight
            ? "truncate font-medium text-amber-600 dark:text-amber-400"
            : "truncate font-medium"
        }
        title={value}
      >
        {value}
      </span>
    </div>
  );
}
