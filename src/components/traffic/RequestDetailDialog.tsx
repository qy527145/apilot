import { useQuery } from "@tanstack/react-query";
import { Loader2 } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { qk } from "@/hooks/queries";
import { api } from "@/lib/api";
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
      <DialogContent className="sm:max-w-4xl">
        <DialogHeader>
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
          <div className="space-y-4">
            <div className="flex flex-wrap items-center gap-2">
              <Badge variant={data.status_code < 400 ? "success" : "destructive"}>
                {data.status_code}
              </Badge>
              <Badge variant="outline">{data.client}</Badge>
              <Badge variant="outline">{data.model}</Badge>
              {data.provider_tag && (
                <Badge variant="outline">{data.provider_tag}</Badge>
              )}
              {data.is_stream && <Badge variant="secondary">流式</Badge>}
              {data.cache_hit && <Badge variant="success">缓存命中</Badge>}
            </div>

            <div className="grid grid-cols-2 gap-x-6 gap-y-2 rounded-md border p-3 text-xs sm:grid-cols-4">
              <Field label="时间" value={formatTime(data.ts)} />
              <Field label="耗时" value={formatMs(data.latency_ms)} />
              <Field label="TTFB" value={formatMs(data.ttfb_ms)} />
              <Field label="费用" value={quotaToUsd(data.quota)} />
              <Field label="输入 Token" value={formatNumber(data.input_tokens)} />
              <Field label="输出 Token" value={formatNumber(data.output_tokens)} />
              <Field
                label="缓存读 Token"
                value={formatNumber(data.cache_read_tokens)}
              />
              <Field
                label="缓存写 Token"
                value={formatNumber(data.cache_creation_tokens)}
              />
              <Field label="协议(入)" value={data.protocol_in} />
              <Field label="协议(出)" value={data.protocol_out} />
              <Field label="用量来源" value={data.usage_source} />
              <Field label="流事件数" value={formatNumber(data.stream_events)} />
            </div>

            {data.error_message && (
              <div className="border-destructive/40 bg-destructive/10 text-destructive rounded-md border p-3 text-xs">
                {data.error_message}
              </div>
            )}

            <Tabs defaultValue="request">
              <TabsList>
                <TabsTrigger value="request">请求体</TabsTrigger>
                <TabsTrigger value="response">响应体</TabsTrigger>
                <TabsTrigger value="stream">流式文本</TabsTrigger>
                <TabsTrigger value="headers">Headers</TabsTrigger>
              </TabsList>

              <TabsContent value="request">
                <JsonBlock raw={data.request_body} empty="未捕获请求体" />
              </TabsContent>
              <TabsContent value="response">
                <JsonBlock raw={data.response_body} empty="未捕获响应体" />
              </TabsContent>
              <TabsContent value="stream">
                <ScrollArea className="h-[40vh] rounded-md border bg-muted/30">
                  <pre className="p-3 font-mono text-xs whitespace-pre-wrap">
                    {data.stream_text || "（无流式文本）"}
                  </pre>
                </ScrollArea>
              </TabsContent>
              <TabsContent value="headers">
                <ScrollArea className="h-[40vh] rounded-md border bg-muted/30">
                  <pre className="p-3 font-mono text-xs whitespace-pre-wrap">
                    {JSON.stringify(
                      {
                        request: data.request_headers,
                        response: data.response_headers,
                      },
                      null,
                      2,
                    )}
                  </pre>
                </ScrollArea>
              </TabsContent>
            </Tabs>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}

function Field({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center justify-between gap-2">
      <span className="text-muted-foreground">{label}</span>
      <span className="truncate font-medium" title={value}>
        {value}
      </span>
    </div>
  );
}

function JsonBlock({ raw, empty }: { raw?: string | null; empty: string }) {
  if (!raw) {
    return (
      <p className="text-muted-foreground py-6 text-center text-xs">{empty}</p>
    );
  }
  return (
    <ScrollArea className="h-[40vh] rounded-md border bg-muted/30">
      <pre className="p-3 font-mono text-xs whitespace-pre-wrap">
        {prettyJson(raw)}
      </pre>
    </ScrollArea>
  );
}
