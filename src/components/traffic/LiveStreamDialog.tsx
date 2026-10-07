import { useEffect, useMemo, useRef, useState } from "react";
import { Loader2, Radio } from "lucide-react";

import { CopyButton } from "@/components/common/CopyButton";
import { StreamTimeline } from "@/components/traffic/StreamTimeline";
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
import { PROTOCOL_LABEL, type Protocol } from "@/lib/api";
import type { StreamFrame } from "@/lib/events";
import { cn, formatTime } from "@/lib/utils";

/** 一条正在跑（或刚跑完）的流式请求在内存里的样子。 */
export interface LiveRequest {
  request_id: string;
  ts: number;
  client: string;
  model: string;
  path: string;
  protocol_in: string;
  provider_tag: string;
  is_stream: boolean;
  frames: StreamFrame[];
  /** 流已结束（正常结束、出错或客户端断连）。 */
  done: boolean;
  /** 结束的墙钟时间。结束后的条目靠它算保留期 —— 用开始时间会把长回答立刻清掉。 */
  done_at?: number | null;
  truncated: boolean;
  error?: string | null;
}

/**
 * 单条请求保留多少帧。
 *
 * 后端已经按每请求 2000 帧封顶了，这里再收一道：前端是长驻进程，用户开着
 * 监控页过夜的话，几百条请求各留 2000 帧会把内存吃光。实时视图看的是"当下"，
 * 完整内容在请求结束后本来就能从明细里看到。
 */
export const MAX_LIVE_FRAMES = 500;

/** 把增量折叠成可读的内容块。 */
interface Block {
  index: number;
  kind: "text" | "thinking" | "tool";
  text: string;
  /** 工具调用才有。 */
  name?: string;
}

function accumulate(frames: StreamFrame[]): Block[] {
  const byIndex = new Map<number, Block>();
  const order: number[] = [];

  const ensure = (index: number, kind: Block["kind"]): Block => {
    let b = byIndex.get(index);
    if (!b) {
      b = { index, kind, text: "" };
      byIndex.set(index, b);
      order.push(index);
    }
    return b;
  };

  for (const f of frames) {
    for (const d of f.deltas) {
      switch (d.kind) {
        case "block_start":
          if (d.block.type === "tool_use") {
            const b = ensure(d.index, "tool");
            b.name = d.block.name;
          } else if (d.block.type === "thinking") {
            ensure(d.index, "thinking");
          }
          break;
        case "text":
          ensure(d.index, "text").text += d.text;
          break;
        case "thinking":
          ensure(d.index, "thinking").text += d.text;
          break;
        // 工具入参是**分片 JSON，收齐前不可解析** —— 原样拼起来展示，
        // 别在这里 JSON.parse，那会在流的中间态抛异常。
        case "tool_input":
          ensure(d.index, "tool").text += d.partial_json;
          break;
      }
    }
  }

  return order.map((i) => byIndex.get(i)!);
}

function BlockView({ block }: { block: Block }) {
  const label =
    block.kind === "thinking" ? "思考" : block.kind === "tool" ? "工具调用" : "文本";
  return (
    <div className="space-y-1">
      <div className="flex items-center gap-2">
        <Badge variant="outline">{label}</Badge>
        {block.kind === "tool" && block.name && (
          <span className="font-mono text-[11px]">{block.name}</span>
        )}
      </div>
      <pre className="bg-muted/30 max-h-64 overflow-auto rounded-md border p-2 font-mono text-xs whitespace-pre-wrap">
        {block.text || "（等待内容…）"}
      </pre>
    </div>
  );
}

/** 一帧在时间线里的名字：优先 `event:` 行，没有就回落到 data 里的 `type`。 */
function frameName(frame: StreamFrame): string {
  // 后端推的是原始 SSE 块，`event:` 与 data 里的 `type` 都要认 ——
  // OpenAI Responses 只用 data 里的 type 区分事件。
  const m = /^event:\s*(.+)$/m.exec(frame.raw);
  if (m) return m[1].trim();
  return frame.deltas[0]?.kind ?? "—";
}

interface Props {
  live: LiveRequest | null;
  onOpenChange: (open: boolean) => void;
}

/**
 * 正在进行的流式请求的实时视图。
 *
 * 数据来自 `apilot://stream` 事件，不查数据库 —— 流式请求在整条流结束前
 * **根本没有落库**，查也查不到。
 */
export function LiveStreamDialog({ live, onOpenChange }: Props) {
  const [selectedSeq, setSelectedSeq] = useState<number | null>(null);
  const [follow, setFollow] = useState(true);
  const bottomRef = useRef<HTMLDivElement>(null);

  const frames = live?.frames ?? [];
  const blocks = useMemo(() => accumulate(frames), [frames]);
  // 时间轴吃的是 (时间点, 名字) 两样，与明细那边从库里读出来的形态一致 ——
  // 同一个组件因此能给两种数据源画同一张图。
  const entries = useMemo(
    () => frames.map((f) => ({ at_ms: f.at_ms, name: frameName(f) })),
    [frames],
  );

  // 跟随最新：不这么做的话，长回答滚上去之后就再也看不到新内容了。
  useEffect(() => {
    if (follow) bottomRef.current?.scrollIntoView({ block: "end" });
  }, [frames.length, follow]);

  const selected =
    frames.find((f) => f.seq === selectedSeq) ?? frames[frames.length - 1];

  return (
    <Dialog open={!!live} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[90vh] overflow-hidden sm:max-w-5xl">
        <DialogHeader>
          <DialogTitle className="flex flex-wrap items-center gap-2">
            <Radio
              className={cn("size-4", !live?.done && "animate-pulse text-emerald-500")}
            />
            实时事件流
            {live && (
              <span className="text-muted-foreground font-mono text-xs">
                {live.request_id.slice(0, 8)}
              </span>
            )}
            {live && !live.done && <Badge variant="success">进行中</Badge>}
            {live?.truncated && <Badge variant="secondary">已截断</Badge>}
          </DialogTitle>
          <DialogDescription className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
            {live && (
              <>
                <span>{formatTime(live.ts)}</span>
                <span>{live.client}</span>
                <span className="font-mono">{live.model}</span>
                <span>{live.provider_tag}</span>
                <span>
                  {PROTOCOL_LABEL[live.protocol_in as Protocol] ?? live.protocol_in}
                </span>
                <span>已收 {frames.length} 帧</span>
              </>
            )}
          </DialogDescription>
        </DialogHeader>

        {live?.error && (
          <p className="text-destructive text-xs">流未正常结束：{live.error}</p>
        )}
        {live?.done && !live.error && (
          <p className="text-muted-foreground text-xs">
            流已结束。这里只保留最近若干帧，完整报文在列表里点这一行看明细。
          </p>
        )}

        <div className="grid min-h-0 grid-cols-1 gap-3 md:grid-cols-[minmax(0,20rem)_minmax(0,1fr)]">
          {/* ---- 左：事件时间线 ---- */}
          <div className="flex min-h-0 flex-col gap-2">
            <div className="flex items-center justify-between">
              <span className="text-xs font-medium">事件时间轴</span>
              <Button
                variant="ghost"
                size="sm"
                className="h-6 px-2 text-[11px]"
                onClick={() => setFollow((v) => !v)}
              >
                {follow ? "停止跟随" : "跟随最新"}
              </Button>
            </div>
            <div className="max-h-[52vh] min-h-32 overflow-y-auto pr-1">
              {frames.length === 0 ? (
                <p className="text-muted-foreground flex items-center justify-center gap-2 py-8 text-xs">
                  <Loader2 className="size-3 animate-spin" />
                  等待上游第一个事件…
                </p>
              ) : (
                <StreamTimeline
                  entries={entries}
                  truncated={live?.truncated}
                  selected={selected?.seq ?? null}
                  onSelect={(seq) => {
                    setSelectedSeq(seq);
                    setFollow(false);
                  }}
                  barClassName="w-12"
                  showAbsolute={false}
                />
              )}
              <div ref={bottomRef} />
            </div>
          </div>

          {/* ---- 右：内容 / 原文 ---- */}
          <Tabs defaultValue="content" className="min-w-0">
            <TabsList>
              <TabsTrigger value="content">内容</TabsTrigger>
              <TabsTrigger value="raw">原文</TabsTrigger>
            </TabsList>

            <TabsContent value="content" className="max-h-[52vh] space-y-3 overflow-y-auto pt-3">
              {blocks.length === 0 ? (
                <p className="text-muted-foreground py-8 text-center text-xs">
                  还没有可展示的内容增量。
                </p>
              ) : (
                blocks.map((b) => <BlockView key={b.index} block={b} />)
              )}
            </TabsContent>

            <TabsContent value="raw" className="space-y-2 pt-3">
              {selected ? (
                <>
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <span className="text-muted-foreground text-[11px]">
                      第 {selected.seq} 帧 · {frameName(selected)}
                      {selected.raw_truncated && "（原文已截断）"}
                    </span>
                    <CopyButton text={selected.raw} />
                  </div>
                  <pre className="bg-muted/30 max-h-[46vh] overflow-auto rounded-md border p-2 font-mono text-xs whitespace-pre-wrap">
                    {selected.raw}
                  </pre>
                  {selected.deltas.length > 0 && (
                    <>
                      <p className="text-muted-foreground text-[11px]">
                        该帧解码出的增量
                      </p>
                      <pre className="bg-muted/30 max-h-40 overflow-auto rounded-md border p-2 font-mono text-[11px]">
                        {JSON.stringify(selected.deltas, null, 2)}
                      </pre>
                    </>
                  )}
                </>
              ) : (
                <p className="text-muted-foreground py-8 text-center text-xs">
                  还没有收到事件。
                </p>
              )}
            </TabsContent>
          </Tabs>
        </div>
      </DialogContent>
    </Dialog>
  );
}
