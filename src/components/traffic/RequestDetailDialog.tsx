import { useQuery } from "@tanstack/react-query";
import { ArrowRight, Loader2 } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { qk } from "@/hooks/queries";
import { api, PROTOCOL_LABEL, type Protocol, type RequestDetail } from "@/lib/api";
import {
  formatMs,
  formatNumber,
  formatTime,
  prettyJson,
  quotaToUsd,
} from "@/lib/utils";

interface Props {
  requestId: string | null;
  onOpenChange: (open: boolean) => void;
}

export function RequestDetailDialog({ requestId, onOpenChange }: Props) {
  const { data, isLoading, isError, refetch } = useQuery({
    queryKey: qk.requestDetail(requestId ?? ""),
    queryFn: () => api.getRequestDetail(requestId as string),
    enabled: !!requestId,
    retry: 1,
  });

  return (
    <Dialog open={!!requestId} onOpenChange={onOpenChange}>
      {/*
       * 弹窗必须有高度上限并把正文做成可滚动区。之前没有上限，
       * 徽章 + 指标网格 + 若干 tab 一叠就超出视口，底部内容被裁掉且无法滚动。
       * `min-h-0` 是关键：flex 子项默认 min-height:auto，不加就不会收缩，
       * 滚动条也不会出现。
       */}
      <DialogContent className="flex max-h-[85vh] flex-col gap-4 overflow-hidden sm:max-w-5xl">
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

            <Tabs defaultValue="inbound">
              <TabsList className="flex-wrap">
                <TabsTrigger value="inbound">① 客户端 → Apilot</TabsTrigger>
                <TabsTrigger value="outbound">② Apilot → 上游</TabsTrigger>
                <TabsTrigger value="upstream-response">③ 上游 → Apilot</TabsTrigger>
                <TabsTrigger value="client-response">④ Apilot → 客户端</TabsTrigger>
                <TabsTrigger value="stream">流式文本</TabsTrigger>
              </TabsList>

              <TabsContent value="inbound">
                <Exchange
                  headline={`${data.method || "POST"} ${data.path || "—"}`}
                  note="客户端原样发来的请求。协议与「协议(入)」一致。"
                  headers={data.request_headers}
                  body={data.request_body}
                  emptyBody="未捕获请求体"
                />
              </TabsContent>

              <TabsContent value="outbound">
                <Exchange
                  headline={data.upstream_url ?? "未请求上游"}
                  note={
                    data.upstream_url
                      ? `Apilot 实际发出的请求。协议 ${
                          PROTOCOL_LABEL[data.protocol_out as Protocol] ??
                          data.protocol_out
                        }，模型 ${
                          data.upstream_model ?? data.model
                        }。鉴权头已隐去。`
                      : data.cache_hit
                        ? "缓存命中，这次没有请求上游。"
                        : "这次请求没有走到上游。"
                  }
                  headers={data.upstream_headers}
                  body={data.upstream_body}
                  emptyBody={
                    data.upstream_url ? "未捕获请求体" : "没有发往上游的请求"
                  }
                />
              </TabsContent>

              <TabsContent value="upstream-response">
                <Exchange
                  headline={
                    data.upstream_status != null
                      ? `HTTP ${data.upstream_status}`
                      : "未请求上游"
                  }
                  note="上游返回的原始响应。与我们返回给客户端的那份可能不同：跨协议转换时两份内容并不一样，排查上游报错看这一份。"
                  headers={data.upstream_response_headers}
                  body={data.upstream_response_body}
                  emptyBody="未捕获上游响应体（流式响应不缓冲原文，见「流式文本」）"
                />
              </TabsContent>

              <TabsContent value="client-response">
                <Exchange
                  headline={`HTTP ${data.status_code}`}
                  note="Apilot 最终返回给客户端的内容。同协议直通时与上游响应的正文一致。"
                  headers={data.response_headers}
                  body={data.response_body}
                  emptyBody="未捕获响应体"
                />
              </TabsContent>

              <TabsContent value="stream">
                <BareBlock text={data.stream_text || "（无流式文本）"} />
              </TabsContent>
            </Tabs>
          </div>
        )}
      </DialogContent>
    </Dialog>
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
      {data.provider_tag && <Badge variant="outline">{data.provider_tag}</Badge>}
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
        <Field
          label="上游模型"
          value={data.upstream_model ?? "—"}
          highlight={
            !!data.upstream_model && data.upstream_model !== data.request_model
          }
        />
        <Field label="用量来源" value={data.usage_source} />
        <Field label="上游状态码" value={data.upstream_status?.toString() ?? "—"} />
        <Field label="流事件数" value={formatNumber(data.stream_events)} />
      </div>
    </div>
  );
}

/** 一个方向的请求/响应：标题行 + headers + body。 */
function Exchange({
  headline,
  note,
  headers,
  body,
  emptyBody,
}: {
  headline: string;
  note: string;
  headers: Record<string, string>;
  body?: string | null;
  emptyBody: string;
}) {
  const headerCount = Object.keys(headers ?? {}).length;

  return (
    <div className="space-y-2">
      <p className="font-mono text-xs break-all">{headline}</p>
      <p className="text-muted-foreground text-xs">{note}</p>
      <Tabs defaultValue="body">
        <TabsList>
          <TabsTrigger value="body">Body</TabsTrigger>
          <TabsTrigger value="headers">
            Headers{headerCount > 0 ? ` (${headerCount})` : ""}
          </TabsTrigger>
        </TabsList>
        <TabsContent value="body">
          <JsonBlock raw={body} empty={emptyBody} />
        </TabsContent>
        <TabsContent value="headers">
          <BareBlock text={JSON.stringify(headers ?? {}, null, 2)} />
        </TabsContent>
      </Tabs>
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

/**
 * 内容块统一用原生滚动而不是 Radix ScrollArea。
 * ScrollArea 的 viewport 需要一个确定高度才能滚动，而这里的高度由 flex 容器
 * 决定 —— 那正是详情弹窗溢出、滚不动的直接原因。
 */
function BareBlock({ text }: { text: string }) {
  return (
    <pre className="bg-muted/30 max-h-[45vh] min-h-[6rem] overflow-auto rounded-md border p-3 font-mono text-xs whitespace-pre-wrap">
      {text}
    </pre>
  );
}

function JsonBlock({ raw, empty }: { raw?: string | null; empty: string }) {
  if (!raw) {
    return (
      <p className="text-muted-foreground py-6 text-center text-xs">{empty}</p>
    );
  }
  return <BareBlock text={prettyJson(raw)} />;
}
