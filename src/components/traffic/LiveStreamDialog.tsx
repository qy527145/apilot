import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type UIEvent,
} from "react";
import {
  ArrowDownToLine,
  Loader2,
  Radio,
  RotateCw,
  SkipForward,
  Square,
} from "lucide-react";
import { toast } from "sonner";

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
import { api, PROTOCOL_LABEL, type Protocol } from "@/lib/api";
import type { StreamFrame } from "@/lib/events";
import { cn, formatMs, formatTime } from "@/lib/utils";

/** 一条正在跑（或刚跑完）的流式请求在内存里的样子。 */
export interface LiveRequest {
  request_id: string;
  ts: number;
  client: string;
  /** 生效模型（计费口径）。 */
  model: string;
  /** 客户端请求的原始名字；与 `model` 不同时说明被模型策略改写过了。 */
  request_model: string;
  path: string;
  protocol_in: string;
  provider_tag: string;
  /** 渠道当前名称（改名后自动更新）。 */
  provider_name?: string | null;
  upstream_url?: string | null;
  is_stream: boolean;
  session_id?: string | null;
  /** 对话里的哪条线。对话视图靠它把"正在进行"的那条挂到对应线上。 */
  thread_key?: string | null;
  frames: StreamFrame[];
  /** 流已结束（正常结束、出错或客户端断连）。 */
  done: boolean;
  /** 结束的墙钟时间。结束后的条目靠它算保留期 —— 用开始时间会把长回答立刻清掉。 */
  done_at?: number | null;
  truncated: boolean;
  error?: string | null;
}

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
  /**
   * 当前这条已结束、且还有别的请求在跑时，可供跳转的下一条。
   * 没有（还在跑，或者全都跑完了）就不显示那个按钮。
   */
  nextRequestId?: string | null;
  onGoToNext?: (requestId: string) => void;
  onOpenChange: (open: boolean) => void;
}

/**
 * 正在进行的流式请求的实时视图。
 *
 * 数据来自 `apilot://stream` 事件，不查数据库 —— 流式请求在整条流结束前
 * **根本没有落库**，查也查不到。
 */
export function LiveStreamDialog({
  live,
  nextRequestId,
  onGoToNext,
  onOpenChange,
}: Props) {
  // 用 null 表示"跟随最新"，用序号表示"固定在某帧"。
  // 不用 frames[frames.length-1] 作默认值，避免每帧都触发 raw 面板重渲染。
  const [selectedSeq, setSelectedSeq] = useState<number | null>(null);
  const [follow, setFollow] = useState(true);
  const timelineRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  /**
   * 两个滚动区上一次的 `scrollTop`。
   *
   * 判断"用户往上翻了"必须看**方向**，不能只看"离底部多远"：程序化跟滚
   * （`scrollTop = scrollHeight`）触发 scroll 事件时，新帧可能已经让
   * `scrollHeight` 又长了一截 —— 按距离判断会把这次跟滚误判成用户上翻，
   * 于是刚点「回到末尾」就立刻又停止跟随。方向判据不会：跟滚只会让
   * scrollTop 变大。
   */
  const lastTop = useRef({ timeline: 0, content: 0 });

  const frames = live?.frames ?? [];
  const blocks = useMemo(() => accumulate(frames), [frames]);

  /**
   * 下游是否已经拿到了有语义的东西（`message_start` 或内容增量）。
   *
   * 这是「能不能重试」的判据，与后端的 `saw_delta` 是同一条：重来一遍会在
   * 客户端那边出现第二条 `message_start`。拿 "帧数" 当判据不行 —— 上游的
   * `event: ping` 也带 `data:`，会算成一帧却解不出任何增量，于是一条**只发
   * 心跳**的卡死流恰好会被误判成"已经在出内容"，按钮反而不给。
   */
  const semanticStarted = useMemo(
    () => frames.some((f) => f.deltas.length > 0),
    [frames],
  );

  // 点下去到界面上看清结果之间隔着一个来回，没有它按钮会看起来"点了没反应"。
  const [pending, setPending] = useState<"retry" | "cancel" | null>(null);
  useEffect(() => {
    setPending(null);
  }, [live?.request_id, live?.done]);

  const interrupt = async (retry: boolean) => {
    if (!live) return;
    setPending(retry ? "retry" : "cancel");
    try {
      const reached = await api.interruptInflightRequest(live.request_id, retry);
      if (!reached) {
        // 点的时候它其实已经跑完了 —— 如实说，别让按钮看起来像生效了。
        toast.error("这条请求已经结束了");
        setPending(null);
      }
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
      setPending(null);
    }
  };

  /**
   * 首帧到达前的等待计时。
   *
   * 这段时间界面只有一个转圈，用户无从判断已经等了 3 秒还是 30 秒 ——
   * 事后看明细里的 TTFT 只有几秒时，就会怀疑"是不是少算了一段"。
   * 把等待时长显式写出来就能对上：起点与 TTFT 相同（请求到达网关），
   * 首帧到达即停，两者应当一致。
   */
  const waiting = !!live && !live.done && frames.length === 0;
  const [waitTick, setWaitTick] = useState(0);
  useEffect(() => {
    if (!waiting) return;
    setWaitTick(Date.now());
    const timer = window.setInterval(() => setWaitTick(Date.now()), 200);
    return () => window.clearInterval(timer);
  }, [waiting, live?.request_id]);
  const waitedMs = live && waitTick > 0 ? Math.max(0, waitTick - live.ts) : 0;

  // 时间轴吃的是 (时间点, 名字) 两样，与明细那边从库里读出来的形态一致 ——
  // 同一个组件因此能给两种数据源画同一张图。
  const entries = useMemo(
    () => frames.map((f) => ({ at_ms: f.at_ms, name: frameName(f) })),
    [frames],
  );

  // 当弹窗打开时重置选择状态，避免上一条请求的选中状态残留。
  useEffect(() => {
    if (live) {
      setSelectedSeq(null);
      setFollow(true);
      // 新请求的两个面板可能带着上一条的滚动位置，方向判据得同步归零。
      lastTop.current = { timeline: 0, content: 0 };
    }
  }, [live?.request_id]);

  // 跟随末尾：两个滚动区都跟，只看一边的话另一边照样会停在半路。
  // 直接设 `scrollTop` 而不是 `scrollIntoView` —— 后者会连带滚动外层弹窗，
  // 而且滚动消息面板时可能把整页拽走。
  useEffect(() => {
    if (!follow) return;
    for (const el of [timelineRef.current, contentRef.current]) {
      if (el) el.scrollTop = el.scrollHeight;
    }
  }, [frames.length, follow, live?.request_id]);

  /**
   * 用户自己往上滚了就停止跟随，别让下一批帧把他拽回底部；
   * 滚回底部则自动恢复跟随。
   *
   * 判据是**方向**（scrollTop 变小）而不是"离底部多远"。后者有个很难查的
   * 竞态：点「回到末尾」后程序化跟滚触发 scroll 事件时，新帧可能已经让
   * scrollHeight 又长了一截，距离判断会把这次跟滚当成用户上翻，于是刚恢复
   * 的跟随立刻又被关掉 —— 表现就是"点了回到末尾，跳一下就不跟了"。
   * 程序化跟滚只会让 scrollTop 变大，方向判据不会误伤。
   */
  const handleScroll = (e: UIEvent<HTMLDivElement>) => {
    const el = e.currentTarget;
    const panel = el === timelineRef.current ? "timeline" : "content";
    const prev = lastTop.current[panel];
    const next = el.scrollTop;
    lastTop.current[panel] = next;

    // 变大 = 跟滚或用户往下翻；变小才是用户往上翻。
    if (next < prev - 1) {
      setFollow(false);
      return;
    }
    if (
      !follow &&
      el.scrollHeight - el.scrollTop - el.clientHeight <= 16
    ) {
      setFollow(true);
    }
  };

  // 稳定引用：时间轴的行按 props 记忆，回调每次渲染都换新的话，
  // 整条流（可能几千行）会在每批帧到达时全部重渲染。
  const selectFrame = useCallback((seq: number) => {
    setSelectedSeq(seq);
    setFollow(false);
  }, []);

  // 把「选中帧」的计算收进 memo：selectedSeq 为 null 时取最后一帧，
  // 有值时找匹配，找不到也回落到最后一帧。
  // 不直接放在渲染里 —— 那样每次父组件 re-render（每帧一次）都会重新找一遍。
  const selected = useMemo(() => {
    if (frames.length === 0) return null;
    if (selectedSeq === null) return frames[frames.length - 1];
    return frames.find((f) => f.seq === selectedSeq) ?? frames[frames.length - 1];
  }, [frames, selectedSeq]);

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
            {live && !live.done && (
              <Button
                size="sm"
                variant={semanticStarted ? "outline" : "default"}
                className="ml-auto h-6 px-2 text-[11px]"
                disabled={pending !== null}
                title={
                  semanticStarted
                    ? "结束这条响应。内容已经发出去，重试会让客户端收到第二条 message_start"
                    : "放弃这次上游请求，换一条渠道重新来过；客户端那边感觉只是慢了一点"
                }
                onClick={() => interrupt(!semanticStarted)}
              >
                {pending !== null ? (
                  <Loader2 className="size-3 animate-spin" />
                ) : semanticStarted ? (
                  <Square className="size-3" />
                ) : (
                  <RotateCw className="size-3" />
                )}
                {pending === "retry"
                  ? "正在重试…"
                  : pending === "cancel"
                    ? "正在中断…"
                    : semanticStarted
                      ? "中断"
                      : "中断并重试"}
              </Button>
            )}
            {live?.done && nextRequestId && onGoToNext && (
              <Button
                size="sm"
                variant="outline"
                className="ml-auto h-6 px-2 text-[11px]"
                title="跳到下一条还在跑的请求"
                onClick={() => onGoToNext(nextRequestId)}
              >
                <SkipForward className="size-3" />
                下一条进行中
              </Button>
            )}
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
            流已结束。完整报文在列表里点这一行看明细。
          </p>
        )}

        <div className="grid min-h-0 grid-cols-1 gap-3 md:grid-cols-[minmax(0,20rem)_minmax(0,1fr)]">
          {/* ---- 左：事件时间线 ---- */}
          <div className="flex min-h-0 flex-col gap-2">
            <div className="flex items-center justify-between">
              <span className="text-xs font-medium">事件时间轴</span>
              <Button
                variant={follow ? "secondary" : "ghost"}
                size="sm"
                className="h-6 px-2 text-[11px]"
                title={
                  follow
                    ? "正在跟随末尾；手动向上滚动会停止跟随"
                    : "回到末尾并继续跟随"
                }
                onClick={() => {
                  if (follow) {
                    setFollow(false);
                    return;
                  }
                  // 切回跟随时也清掉固定选中，让视图跟着走。
                  setSelectedSeq(null);
                  setFollow(true);
                }}
              >
                <ArrowDownToLine className="size-3" />
                {follow ? "跟随末尾" : "回到末尾"}
              </Button>
            </div>
            <div
              ref={timelineRef}
              onScroll={handleScroll}
              className="max-h-[52vh] min-h-32 overflow-y-auto pr-1"
            >
              {frames.length === 0 ? (
                <div className="flex flex-col items-center justify-center gap-2 py-8 text-xs">
                  <p className="text-muted-foreground flex items-center gap-2">
                    <Loader2 className="size-3 animate-spin" />
                    等待上游第一个事件…
                    {!live?.done && (
                      <span className="tabular-nums">
                        已等待 {formatMs(waitedMs)}
                      </span>
                    )}
                  </p>
                  {!live?.done && (
                    <p className="text-muted-foreground/70 text-[11px]">
                      首字从请求到达网关起算，结束后明细里的 TTFT 就是这段等待。
                    </p>
                  )}
                  {/* 用户盯着的就是这一块，按钮也放在这儿 —— 只在标题栏里，
                      等得不耐烦的人还得先把视线挪上去。 */}
                  {live && !live.done && !semanticStarted && (
                    <Button
                      size="sm"
                      className="mt-1 h-7"
                      disabled={pending !== null}
                      onClick={() => interrupt(true)}
                    >
                      {pending === "retry" ? (
                        <Loader2 className="size-3 animate-spin" />
                      ) : (
                        <RotateCw className="size-3" />
                      )}
                      {pending === "retry" ? "正在重试…" : "中断并重试"}
                    </Button>
                  )}
                </div>
              ) : (
                <StreamTimeline
                  entries={entries}
                  truncated={live?.truncated}
                  selected={selected?.seq ?? null}
                  onSelect={selectFrame}
                  barClassName="w-12"
                  showAbsolute={false}
                />
              )}
            </div>
          </div>

          {/* ---- 右：内容 / 原文 ---- */}
          <Tabs defaultValue="content" className="min-w-0">
            <TabsList>
              <TabsTrigger value="content">内容</TabsTrigger>
              <TabsTrigger value="raw">原文</TabsTrigger>
            </TabsList>

            <TabsContent
              ref={contentRef}
              onScroll={handleScroll}
              value="content"
              className="max-h-[52vh] space-y-3 overflow-y-auto pt-3"
            >
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
