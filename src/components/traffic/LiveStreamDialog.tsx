import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type UIEvent,
} from "react";
import { useQuery } from "@tanstack/react-query";
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
import { RawBody } from "@/components/common/RawBody";
import { RequestView } from "@/components/traffic/InspectViews";
import { StreamTimeline } from "@/components/traffic/StreamTimeline";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
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
  /**
   * 打开这条请求的落库明细。
   *
   * 流结束之后「请求」页签就空了 —— 报文跟着「进行中」条目一起撤掉，完整的那份
   * 在数据库里。不接这个回调，用户只能关掉弹窗、自己回列表里找那一行。
   */
  onOpenDetail?: (requestId: string) => void;
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
  onOpenDetail,
  onOpenChange,
}: Props) {
  // 用 null 表示"跟随最新"，用序号表示"固定在某帧"。
  // 不用 frames[frames.length-1] 作默认值，避免每帧都触发 raw 面板重渲染。
  const [selectedSeq, setSelectedSeq] = useState<number | null>(null);
  /**
   * 「响应」页签的两种看法：折叠后的内容块，还是某一帧的原始 SSE。
   *
   * 默认看内容块 —— 打开实时视图的人多半是想知道"它现在在说什么"。
   * 但点时间轴上的某一帧是明确的"我要看这一帧"，那时必须切到原文，
   * 否则点了没反应（内容页根本不随选中帧变化）。
   */
  const [responseMode, setResponseMode] = useState<"visual" | "raw">("visual");
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
      setResponseMode("visual");
      // 新请求的两个面板可能带着上一条的滚动位置，方向判据得同步归零。
      lastTop.current = { timeline: 0, content: 0 };
    }
  }, [live?.request_id]);

  // 跟随末尾：两个滚动区都跟，只看一边的话另一边照样会停在半路。
  // 直接设 `scrollTop` 而不是 `scrollIntoView` —— 后者会连带滚动外层弹窗，
  // 而且滚动消息面板时可能把整页拽走。
  //
  // 依赖里带 `responseMode`：「原文」视图里没有滚动区，切回内容块时要补一次
  // 跟滚，否则停在上次留下的位置、看起来像"新帧没进来"。
  useEffect(() => {
    if (!follow) return;
    for (const el of [timelineRef.current, contentRef.current]) {
      if (el) el.scrollTop = el.scrollHeight;
    }
  }, [frames.length, follow, live?.request_id, responseMode]);

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
    // 选中某一帧是有明确指向的动作，落到「响应 · 原文」上才看得到那一帧；
    // 停在内容块视图里点了等于没点。
    setResponseMode("raw");
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
      {/*
       * `DialogContent` 自带的是 `grid`，再叠一个 `flex` 不保险：两个类都设
       * `display`，谁赢取决于样式表的先后而不是 class 的顺序。布局就沿用它
       * 本身的 grid —— 头、正文、页脚各占一行。
       *
       * 高度写死而不是 `max-h`：请求与响应两个页签内容长短差得很远，随内容
       * 伸缩时一切页签整个弹窗就跳一下，页脚按钮跟着挪出去老远。固定下来，
       * 正文那一行吃掉剩余高度，长内容在各自的面板里滚。
       */}
      <DialogContent className="h-[85vh] grid-rows-[auto_minmax(0,1fr)_auto] overflow-hidden sm:max-w-5xl">
        <DialogHeader className="shrink-0">
          <DialogTitle className="flex flex-wrap items-center gap-2 pr-8">
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
          {/* 放在头里而不是单独一行：弹窗按「头 / 正文 / 页脚」三行排，多出一个
              时有时无的子元素会把正文挤到页脚那一行去。 */}
          {live?.error && (
            <p className="text-destructive text-xs">流未正常结束：{live.error}</p>
          )}
          {live?.done && !live.error && (
            <p className="text-muted-foreground text-xs">
              流已结束。完整报文已落库，点右下角「查看完整明细」。
            </p>
          )}
        </DialogHeader>

        {/* 窄屏是上下两块堆叠，没法各自撑满，整块一起滚；宽屏左右两栏各占满高、各自滚。 */}
        <div className="grid min-h-0 grid-cols-1 gap-3 overflow-y-auto md:grid-cols-[minmax(0,20rem)_minmax(0,1fr)] md:grid-rows-[minmax(0,1fr)] md:overflow-hidden">
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
              className="max-h-[52vh] min-h-32 overflow-y-auto pr-1 md:max-h-none md:flex-1"
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
                  {/* 等过一阵子才说这句话：刚开就弹一段解释，正常 2~3 秒的
                      请求会先闪一下这段字，反而像出了问题。 */}
                  {waitedMs > 15_000 && (
                    <p className="text-muted-foreground/80 max-w-md text-center text-[11px]">
                      上游迟迟没有回应。多数情况是上游在排队 —— 中转站被打满时连响应头
                      都要等几十秒，再等下去往往也不会更快。不想等就点右下角的
                      「中断并重试」。
                    </p>
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

          {/* ---- 右：请求 / 响应 ---- */}
          {/*
           * 一级只放方向（请求 / 响应），「可视化 / 原文」是响应的两种看法，降成
           * 页签里的切换。三个平级的「内容 / 原文 / 请求」把两个方向混在了一起 ——
           * 看响应的人得先排除掉请求那一页，而排查「发出去的到底是什么」时又得
           * 跨到另一侧。
           *
           * 页签顺序按时间先后：先有请求、后有响应。默认仍落在「响应」上 ——
           * 打开实时视图多半是想看它现在在说什么。
           *
           * 换一条请求就整块重建：页签停在「请求」上看的是上一条的报文，
           * 新的一条进来时会先闪一下旧的、再换掉，像是"这条请求发了别人的参数"。
           */}
          <Tabs key={live?.request_id} defaultValue="response" className="min-h-0 min-w-0">
            <TabsList className="shrink-0">
              <TabsTrigger value="request">请求</TabsTrigger>
              <TabsTrigger value="response">响应</TabsTrigger>
            </TabsList>

            <TabsContent value="response" className="flex min-h-0 flex-col gap-2 pt-3">
              <div className="flex shrink-0 flex-wrap items-center gap-2">
                <div className="flex items-center gap-1">
                  {(
                    [
                      ["visual", "可视化"],
                      ["raw", "原文"],
                    ] as const
                  ).map(([key, label]) => (
                    <Button
                      key={key}
                      variant={responseMode === key ? "default" : "outline"}
                      size="sm"
                      className="h-6 px-2 text-[11px]"
                      onClick={() => setResponseMode(key)}
                    >
                      {label}
                    </Button>
                  ))}
                </div>
                {responseMode === "raw" && selected && (
                  <span className="text-muted-foreground text-[11px]">
                    第 {selected.seq} 帧 · {frameName(selected)}
                    {selected.raw_truncated && "（原文已截断）"}
                  </span>
                )}
                {responseMode === "raw" && selected && (
                  <span className="ml-auto">
                    <CopyButton text={selected.raw} />
                  </span>
                )}
              </div>

              {responseMode === "visual" ? (
                <div
                  ref={contentRef}
                  onScroll={handleScroll}
                  className="max-h-[48vh] space-y-3 overflow-y-auto pr-1 md:max-h-none md:min-h-0 md:flex-1"
                >
                  {blocks.length === 0 ? (
                    <p className="text-muted-foreground py-8 text-center text-xs">
                      还没有可展示的内容增量。
                    </p>
                  ) : (
                    blocks.map((b) => <BlockView key={b.index} block={b} />)
                  )}
                </div>
              ) : selected ? (
                <div className="min-h-0 flex-1 space-y-2 overflow-y-auto pr-1">
                  <pre className="bg-muted/30 overflow-auto rounded-md border p-2 font-mono text-xs whitespace-pre-wrap">
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
                </div>
              ) : (
                <p className="text-muted-foreground py-8 text-center text-xs">
                  还没有收到事件。
                </p>
              )}
            </TabsContent>

            <TabsContent value="request" className="max-h-[52vh] overflow-y-auto pt-3 md:max-h-none md:min-h-0">
              {live && (
                <InflightRequestPane
                  requestId={live.request_id}
                  done={live.done}
                  onOpenDetail={
                    onOpenDetail
                      ? () => onOpenDetail(live.request_id)
                      : undefined
                  }
                />
              )}
            </TabsContent>
          </Tabs>
        </div>

        {/* 中断 / 重试放在页脚，不放标题栏。
            标题栏那个位置紧挨着右上角的关闭按钮（X），而关闭与"打断这次请求"是
            两件性质完全不同的事 —— 误触一次请求就没了。页脚单独一行，离任何
            危险控件都远，且留在这里不会随内容多少挪位置。 */}
        <DialogFooter className="shrink-0 items-center gap-2 sm:justify-between">
          <span className="text-muted-foreground hidden text-[11px] sm:block">
            {live?.done
              ? "流已结束，完整报文在明细里。"
              : semanticStarted
                ? "内容已经开始下发，现在中断会让客户端收到一条不完整的响应。"
                : "还没有内容发出去，重试对客户端来说只是慢了一点。"}
          </span>
          <div className="flex items-center gap-2">
            {live && !live.done && (
              <Button
                size="sm"
                variant={semanticStarted ? "outline" : "default"}
                disabled={pending !== null}
                title={
                  semanticStarted
                    ? "结束这条响应。内容已经发出去，重试会让客户端收到第二条 message_start"
                    : "放弃这次上游请求，换一条渠道重新来过；客户端那边感觉只是慢了一点"
                }
                onClick={() => interrupt(!semanticStarted)}
              >
                {pending !== null ? (
                  <Loader2 className="size-3.5 animate-spin" />
                ) : semanticStarted ? (
                  <Square className="size-3.5" />
                ) : (
                  <RotateCw className="size-3.5" />
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
                title="跳到下一条还在跑的请求"
                onClick={() => onGoToNext(nextRequestId)}
              >
                <SkipForward className="size-3.5" />
                下一条进行中
              </Button>
            )}
            {live?.done && onOpenDetail && (
              <Button
                size="sm"
                title="这条请求的完整报文已经在数据库里了"
                onClick={() => onOpenDetail(live.request_id)}
              >
                查看完整明细
              </Button>
            )}
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/**
 * 实时视图里的「请求」页签。
 *
 * 报文来自两处，按顺序取：请求还在跑时是内存里的那份（进行中的请求**还没落库**，
 * `get_request_detail` 查不到）；跑完之后后端接着读数据库里落库的捕获。用户在
 * 实时视图里打开「请求」，无论这条流是否结束，都该看到"这次请求发了什么"。
 *
 * 两处都没有才说明确实没有：捕获被关掉了，或者那行已经被裁掉。
 *
 * 只在切到这个页签时才发起查询（Radix 的 TabsContent 未激活时卸载内容），
 * 请求体动辄上百 KB，没必要为了一个可能没人看的页签先拉一遍。
 */
function InflightRequestPane({
  requestId,
  done,
  onOpenDetail,
}: {
  requestId: string;
  /** 流已经结束：落库通常比「结束」事件晚几百毫秒，可能得再问一次。 */
  done: boolean;
  onOpenDetail?: () => void;
}) {
  const [mode, setMode] = useState<"visual" | "formatted" | "raw">("visual");
  const [showHeaders, setShowHeaders] = useState(false);

  const { data, isLoading, isError, refetch } = useQuery({
    queryKey: ["inflight_request", requestId],
    queryFn: () => api.getInflightRequest(requestId),
    // 请求体在一条请求的生命周期内不会变；切走再切回来不该重拉。
    staleTime: Infinity,
    retry: 1,
  });

  /**
   * 落库比「流结束」事件晚一点，第一次问往往两处都没有。
   *
   * 与明细弹窗同一个补偿思路：短时间内问几次，拿不到再如实说没有。次数与间隔
   * 一起压在三秒内 —— 再多就成了"点了没反应"，不如直接告诉用户没有。
   */
  const MAX_MISSES = 6;
  const [misses, setMisses] = useState(0);
  useEffect(() => setMisses(0), [requestId]);
  useEffect(() => {
    if (!done || isLoading || isError || data || misses >= MAX_MISSES) return;
    const timer = window.setTimeout(() => {
      setMisses((n) => n + 1);
      void refetch();
    }, 500);
    return () => window.clearTimeout(timer);
  }, [done, isLoading, isError, data, misses, refetch]);

  if (isLoading) {
    return (
      <p className="text-muted-foreground flex items-center justify-center gap-2 py-8 text-xs">
        <Loader2 className="size-3 animate-spin" />
        读取请求报文…
      </p>
    );
  }

  if (isError) {
    return (
      <p className="text-muted-foreground py-8 text-center text-xs">
        读取请求报文失败。
      </p>
    );
  }

  if (!data) {
    // 两处都没有：没开捕获，或者那行已经被裁掉了。这不是"去别处看"能解决的，
    // 直接说清楚，免得用户以为是自己没找对地方。
    return (
      <p className="text-muted-foreground py-8 text-center text-xs">
        {done && misses < MAX_MISSES
          ? "这条请求刚结束，正在读取落库的请求报文…"
          : "没有这条请求的报文 —— 捕获可能被关掉了，或者这条记录已经被裁掉。"}
      </p>
    );
  }

  const headersText = JSON.stringify(data.headers ?? {}, null, 2);

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-mono text-xs break-all">
          {data.method} {data.path}
        </span>
        <Badge variant="outline" className="font-normal">
          {PROTOCOL_LABEL[data.protocol as Protocol] ?? data.protocol}
        </Badge>
        {/* 说清这份报文是哪来的：还在跑时是内存副本，跑完之后是落库那份。
            后者要提醒一句 —— 捕获关掉时落库的报文可能是空的。 */}
        <span className="text-muted-foreground text-[11px]">
          {data.source === "live" ? "进行中 · 内存副本" : "已落库"}
        </span>
        <button
          type="button"
          className="text-muted-foreground hover:text-foreground text-[11px] underline"
          onClick={() => setShowHeaders((v) => !v)}
        >
          Headers ({Object.keys(data.headers ?? {}).length})
        </button>
        {/* 落库之后这里只给入站一份；上游收到的那份、以及按协议解出来的响应
            都在明细里。给个直达，省得用户关掉弹窗再回列表里找。 */}
        {data.source === "detail" && onOpenDetail && (
          <button
            type="button"
            className="text-muted-foreground hover:text-foreground text-[11px] underline"
            onClick={onOpenDetail}
          >
            在明细里看完整报文
          </button>
        )}
        <div className="ml-auto flex items-center gap-1">
          {(
            [
              ["visual", "可视化"],
              ["formatted", "格式化"],
              ["raw", "原始"],
            ] as const
          ).map(([key, label]) => (
            <Button
              key={key}
              variant={mode === key ? "default" : "outline"}
              size="sm"
              className="h-6 px-2 text-[11px]"
              onClick={() => setMode(key)}
            >
              {label}
            </Button>
          ))}
        </div>
      </div>

      {showHeaders && (
        <div className="space-y-1">
          <div className="flex justify-end">
            <CopyButton text={headersText} />
          </div>
          <RawBody text={headersText} rawMode={mode === "raw"} empty="（无）" />
        </div>
      )}

      {mode === "visual" ? (
        data.view?.value ? (
          <RequestView req={data.view.value} />
        ) : (
          <div className="bg-muted/30 space-y-1 rounded-md border p-3 text-xs">
            <p className="font-medium">这份请求报文没法按语义解析</p>
            <p className="text-muted-foreground break-words">
              {data.view?.error ?? "没有可解析的请求体。"}
            </p>
            <p className="text-muted-foreground">
              切到「格式化」或「原始」看原文。
            </p>
          </div>
        )
      ) : (
        <RawBody
          text={data.body}
          rawMode={mode === "raw"}
          empty="这次请求没有请求体"
        />
      )}
    </div>
  );
}
