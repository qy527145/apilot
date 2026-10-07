import { useState, type ReactNode } from "react";
import {
  ChevronDown,
  ChevronRight,
  Image as ImageIcon,
  Lock,
  Wrench,
} from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { cn, formatNumber } from "@/lib/utils";
import type {
  ContentBlock,
  FinishReason,
  Role,
  ToolChoice,
  UnifiedMessage,
  UnifiedRequest,
  UnifiedResponse,
  UnifiedUsage,
} from "@/lib/api";

/**
 * 捕获报文的语义化渲染。
 *
 * 输入是后端解码出的 IR —— 三个协议解出来是同一套结构，所以这里只有一份逻辑。
 * 目的不是"把 JSON 排整齐"，而是让人一眼看到：问了什么、答了什么、
 * 模型调了哪些工具、耗了多少 token。原始 JSON 仍然在「格式化 / 原始」里可取。
 */

// ---------------------------------------------------------------------------
// 基础件
// ---------------------------------------------------------------------------

/** 可折叠分区。仓库里没有 Collapsible 依赖，按 ProvidersPage 的展开模式手写。 */
function Section({
  title,
  count,
  defaultOpen = true,
  children,
}: {
  title: string;
  count?: number | string;
  defaultOpen?: boolean;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);

  return (
    <div className="rounded-md border">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="hover:bg-muted/40 flex w-full items-center gap-2 rounded-md px-3 py-2 text-left text-xs font-medium"
      >
        {open ? (
          <ChevronDown className="size-3.5 shrink-0" />
        ) : (
          <ChevronRight className="size-3.5 shrink-0" />
        )}
        <span>{title}</span>
        {count !== undefined && (
          <span className="text-muted-foreground font-normal">{count}</span>
        )}
      </button>
      {open && <div className="space-y-2 px-3 pb-3">{children}</div>}
    </div>
  );
}

/**
 * 长内容的截断展示。
 *
 * 一段系统提示词或一次工具输出动辄几千字，全铺开会把别的信息挤到屏幕外；
 * 但截断又不该让人以为"就这么多"，所以始终给出真实字数与展开入口。
 */
function LongText({
  text,
  limit = 800,
  className,
}: {
  text: string;
  limit?: number;
  className?: string;
}) {
  const [expanded, setExpanded] = useState(false);
  const tooLong = text.length > limit;

  return (
    <div className="space-y-1">
      <pre
        className={cn(
          "bg-muted/30 overflow-auto rounded-md border p-2 font-mono text-xs whitespace-pre-wrap",
          !expanded && tooLong && "max-h-40",
          className,
        )}
      >
        {expanded || !tooLong ? text : `${text.slice(0, limit)}…`}
      </pre>
      {tooLong && (
        <button
          type="button"
          className="text-muted-foreground hover:text-foreground text-[11px] underline"
          onClick={() => setExpanded((v) => !v)}
        >
          {expanded ? "收起" : `展开全部（共 ${formatNumber(text.length)} 字）`}
        </button>
      )}
    </div>
  );
}

/** 一行小徽章，用于参数这类"键少值短"的信息。 */
function Chips({
  items,
}: {
  items: Array<[string, string] | null>;
}) {
  const shown = items.filter(Boolean) as Array<[string, string]>;
  if (shown.length === 0) {
    return <p className="text-muted-foreground text-xs">（无）</p>;
  }
  return (
    <div className="flex flex-wrap gap-1.5">
      {shown.map(([k, v]) => (
        <span
          key={k}
          className="bg-muted/40 rounded border px-1.5 py-0.5 font-mono text-[11px]"
        >
          <span className="text-muted-foreground">{k}=</span>
          {v}
        </span>
      ))}
    </div>
  );
}

const ROLE_LABEL: Record<Role, string> = {
  system: "system",
  user: "user",
  assistant: "assistant",
  tool: "tool",
};

const ROLE_STYLE: Record<Role, string> = {
  system: "text-muted-foreground",
  user: "",
  assistant: "text-emerald-600 dark:text-emerald-400",
  tool: "text-amber-600 dark:text-amber-400",
};

// ---------------------------------------------------------------------------
// 内容块
// ---------------------------------------------------------------------------

function ToolCallCard({ name, input }: { name: string; input: unknown }) {
  return (
    <div className="bg-background rounded-md border p-2">
      <div className="mb-1 flex items-center gap-1.5">
        <Wrench className="size-3.5 shrink-0" />
        <span className="font-mono text-xs font-medium">{name}</span>
        <span className="text-muted-foreground text-[11px]">调用参数</span>
      </div>
      <pre className="bg-muted/30 max-h-60 overflow-auto rounded border p-2 font-mono text-[11px] whitespace-pre-wrap">
        {JSON.stringify(input ?? {}, null, 2)}
      </pre>
    </div>
  );
}

/** 取出纯文本内容（**不含**思考块）。
 *
 * 收敛成一个函数是有原因的：`thinking` 块的形状是 `{ type: "thinking", text }`，
 * 它同样带 `text` 属性，所以 `"text" in block` 或 `block.text` 这类写法会把
 * 思考内容混进回答里。判断一律走 `type`，且只在这一个地方判断。
 */
function joinText(blocks: ContentBlock[]): string {
  return blocks
    .filter((b) => b.type === "text")
    .map((b) => b.text)
    .join("");
}

/**
 * 按 `block.type` 渲染。`tool_result` 的内容是块数组，所以这里是递归的。
 *
 * ⚠️ 判断块类型一律用 `block.type === "..."`，**不要**用 `"text" in block`：
 * `thinking` 块的形状是 `{ type: "thinking", text: "..." }`，它同样有 `text`
 * 属性，用 `in` 判断会把思考内容当成回答拼进去。取文本用 [`joinText`]。
 *
 * `depth` 只用于给嵌套块加缩进 —— 工具结果里再套工具结果虽然罕见，
 * 但协议允许，不加缩进会看不出层级。
 *
 * `labels` 控制是否自带类型标签。响应视图会给每个块套一层带标题的卡片，
 * 那里再带上标签就重复了。
 */
function BlockRenderer({
  block,
  depth = 0,
  labels = true,
}: {
  block: ContentBlock;
  depth?: number;
  labels?: boolean;
}) {
  switch (block.type) {
    case "text":
      return <LongText text={block.text} />;

    case "thinking":
      return (
        <div className={labels ? "border-l-2 border-dashed pl-2" : undefined}>
          {labels && (
            <div className="text-muted-foreground mb-1 text-[11px]">
              思考内容
              {block.signature ? "（带签名）" : ""}
            </div>
          )}
          <LongText text={block.text} limit={400} className="italic" />
        </div>
      );

    case "redacted_thinking":
      return (
        <div className="text-muted-foreground flex items-center gap-1.5 text-xs">
          <Lock className="size-3.5" />
          加密的思考内容（{formatNumber(block.data.length)} 字符，只能原样回传）
        </div>
      );

    case "tool_use":
      return <ToolCallCard name={block.name} input={block.input} />;

    case "tool_result":
      return (
        <div
          className={cn(
            "rounded-md border p-2",
            block.is_error && "border-destructive/40 bg-destructive/10",
          )}
        >
          {labels && (
            <div className="mb-1 flex items-center gap-1.5 text-[11px]">
              <span className="text-muted-foreground font-mono">
                结果 → {block.tool_use_id}
              </span>
              {block.is_error && (
                <Badge variant="destructive" className="h-4 px-1 text-[10px]">
                  出错
                </Badge>
              )}
            </div>
          )}
          <div className={cn("space-y-2", depth > 0 && "pl-3")}>
            {block.content.map((b, i) => (
              <BlockRenderer key={i} block={b} depth={depth + 1} labels={labels} />
            ))}
          </div>
        </div>
      );

    case "image":
      return (
        <div className="text-muted-foreground flex items-center gap-1.5 text-xs">
          <ImageIcon className="size-3.5" />
          图片（{block.media_type}，约 {formatNumber(block.data.length)} 字节）
        </div>
      );

    default:
      return (
        <pre className="bg-muted/30 overflow-auto rounded border p-2 font-mono text-[11px]">
          {JSON.stringify(block, null, 2)}
        </pre>
      );
  }
}

function MessageCard({
  msg,
  index,
  open,
  onToggle,
}: {
  msg: UnifiedMessage;
  index: number;
  open: boolean;
  onToggle: () => void;
}) {
  // 一屏里塞十条带工具结果的工具消息会很难读，所以每条默认收起由调用方决定。
  const preview = previewOf(msg);

  return (
    <div className="rounded-md border">
      <button
        type="button"
        onClick={onToggle}
        className="hover:bg-muted/40 flex w-full items-start gap-2 rounded-md px-2 py-1.5 text-left"
      >
        {open ? (
          <ChevronDown className="mt-0.5 size-3.5 shrink-0" />
        ) : (
          <ChevronRight className="mt-0.5 size-3.5 shrink-0" />
        )}
        <span className="text-muted-foreground w-6 shrink-0 font-mono text-[11px]">
          #{index}
        </span>
        <span
          className={cn(
            "w-20 shrink-0 font-mono text-xs",
            ROLE_STYLE[msg.role],
          )}
        >
          {ROLE_LABEL[msg.role]}
        </span>
        {!open && (
          <span className="text-muted-foreground min-w-0 flex-1 truncate font-mono text-[11px]">
            {preview}
          </span>
        )}
      </button>
      {open && (
        <div className="space-y-2 px-2 pb-2 pl-14">
          {msg.content.length === 0 ? (
            <p className="text-muted-foreground text-xs">（空消息）</p>
          ) : (
            msg.content.map((b, i) => <BlockRenderer key={i} block={b} />)
          )}
        </div>
      )}
    </div>
  );
}

/** 收起状态下的一行摘要：先给文本，没有文本就说明有几个什么块。 */
function previewOf(msg: UnifiedMessage): string {
  const text = joinText(msg.content).trim();
  if (text) return text.replace(/\s+/g, " ").slice(0, 120);

  const counts: string[] = [];
  const n = (t: ContentBlock["type"], label: string) => {
    const c = msg.content.filter((b) => b.type === t).length;
    if (c > 0) counts.push(`${c} ${label}`);
  };
  n("thinking", "思考");
  n("tool_use", "工具调用");
  n("tool_result", "工具结果");
  n("image", "图片");
  n("redacted_thinking", "加密思考");
  return counts.join("、") || "（空消息）";
}

// ---------------------------------------------------------------------------
// 请求
// ---------------------------------------------------------------------------

const SYSTEM_KEYS = new Set([
  "model",
  "stream",
  "system",
  "messages",
  "tools",
  "tool_choice",
  "temperature",
  "top_p",
  "max_tokens",
  "stop",
  "reasoning",
]);

/** 把 tool_choice 说成人话，比展示 `{"type":"tool","name":"Bash"}` 直观。 */
function describeToolChoice(tc: ToolChoice | null | undefined): string | null {
  if (!tc) return null;
  switch (tc.type) {
    case "auto":
      return "自动";
    case "none":
      return "禁止调用";
    case "required":
      return "必须调用";
    case "tool":
      return `必须调用 ${tc.name}`;
  }
}

export function RequestView({ req }: { req: UnifiedRequest }) {
  // 全部展开/收起：展开状态提到父级，否则按钮没法一次影响所有消息。
  const [openMap, setOpenMap] = useState<Record<number, boolean>>({});
  const isOpen = (i: number) => openMap[i] !== false;

  // 后端对空的集合用了 skip_serializing_if，空时字段会整个消失，所以都要兜底。
  const tools = req.tools ?? [];
  const stop = req.stop ?? [];
  const system = req.system ?? [];
  const messages = req.messages ?? [];

  // `extra` 是后端 flatten 上来的未建模字段，要按已知键名排除掉。
  const extras = Object.entries(req).filter(([k]) => !SYSTEM_KEYS.has(k));

  // 按 type 判断，不用 `"text" in b` —— 思考块同样带 text 属性（见 BlockRenderer 的说明）。
  const systemText = system
    .filter((b) => b.type === "text")
    .map((b) => b.text)
    .join("\n")
    .trim();

  return (
    <div className="space-y-2">
      <Section title="参数">
        <Chips
          items={[
            ["model", req.model],
            ["stream", String(req.stream)],
            req.temperature != null ? ["temperature", String(req.temperature)] : null,
            req.top_p != null ? ["top_p", String(req.top_p)] : null,
            req.max_tokens != null ? ["max_tokens", String(req.max_tokens)] : null,
            stop.length > 0 ? ["stop", stop.join(" | ")] : null,
            req.tool_choice
              ? ["tool_choice", describeToolChoice(req.tool_choice) ?? ""]
              : null,
            req.reasoning?.budget_tokens != null
              ? ["thinking budget", String(req.reasoning.budget_tokens)]
              : null,
            req.reasoning?.effort ? ["effort", req.reasoning.effort] : null,
          ]}
        />
      </Section>

      {systemText && (
        <Section title="系统提示词" count={`${formatNumber(systemText.length)} 字`}>
          <LongText text={systemText} />
        </Section>
      )}

      <Section
        title="工具"
        count={tools.length ? `${tools.length} 个` : undefined}
        defaultOpen={tools.length > 0 && tools.length <= 8}
      >
        {tools.length === 0 ? (
          <p className="text-muted-foreground text-xs">
            这次请求没有带工具（模型无法调用任何工具）
          </p>
        ) : (
          tools.map((t) => (
            <div key={`${t.namespace ?? ""}\u0000${t.name}`} className="rounded-md border p-2">
              <div className="mb-1 flex items-center gap-1.5">
                <Wrench className="size-3.5 shrink-0" />
                {/* Codex 的 namespace 工具（functions / clock / collaboration …）由后端展平成
                    一个个裸名工具，组名单独标出来，免得看着像一堆平级工具。 */}
                {t.namespace && (
                  <span className="bg-muted text-muted-foreground rounded px-1 py-0.5 font-mono text-[10px]">
                    {t.namespace}
                  </span>
                )}
                <span className="font-mono text-xs font-medium">{t.name}</span>
              </div>
              {t.description && (
                <p className="text-muted-foreground mb-1 text-[11px]">
                  {t.description}
                </p>
              )}
              <LongText
                text={JSON.stringify(t.input_schema ?? {}, null, 2)}
                limit={400}
              />
            </div>
          ))
        )}
      </Section>

      <Section
        title="对话上下文"
        count={`${messages.length} 条`}
        defaultOpen={messages.length > 0}
      >
        {messages.length > 0 && (
          <div className="flex gap-2">
            <Button
              variant="ghost"
              size="sm"
              className="h-6 px-2 text-[11px]"
              onClick={() => {
                const next: Record<number, boolean> = {};
                messages.forEach((_, i) => (next[i] = false));
                setOpenMap(next);
              }}
            >
              全部收起
            </Button>
            <Button
              variant="ghost"
              size="sm"
              className="h-6 px-2 text-[11px]"
              onClick={() => setOpenMap({})}
            >
              全部展开
            </Button>
          </div>
        )}
        {messages.map((m, i) => (
          <MessageCard
            key={i}
            msg={m}
            index={i}
            open={isOpen(i)}
            onToggle={() => setOpenMap((s) => ({ ...s, [i]: !isOpen(i) }))}
          />
        ))}
      </Section>

      {extras.length > 0 && (
        <Section
          title="其它字段"
          count={`${extras.length} 个（协议原生、未建模）`}
          defaultOpen={false}
        >
          <pre className="bg-muted/30 max-h-60 overflow-auto rounded border p-2 font-mono text-[11px] whitespace-pre-wrap">
            {JSON.stringify(Object.fromEntries(extras), null, 2)}
          </pre>
        </Section>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// 响应
// ---------------------------------------------------------------------------

const FINISH_LABEL: Record<FinishReason["type"], string> = {
  stop: "正常结束",
  length: "达到长度上限被截断",
  tool_use: "要调用工具",
  content_filter: "被内容策略拦截",
  other: "其它",
};

function finishText(r: FinishReason): string {
  if (r.type === "other") return `其它（${r.value}）`;
  return FINISH_LABEL[r.type];
}

/** token 明细。缓存部分单独列出来，因为它是省钱与否的关键。 */
function UsagePanel({ usage }: { usage: UnifiedUsage }) {
  const cached = usage.cache_read_tokens;
  const fresh = usage.input_tokens;
  const totalIn = fresh + cached;
  const hitRate = totalIn > 0 ? cached / totalIn : 0;

  const rows: Array<[string, number, string?]> = [
    ["输入（未命中缓存）", fresh],
    ["输入（缓存命中）", cached, "按更低的倍率计费"],
    ["缓存写入", usage.cache_creation_tokens],
    ["输出", usage.output_tokens],
    ["推理", usage.reasoning_tokens, "含在输出里"],
  ];

  return (
    <div className="space-y-2">
      <div className="grid grid-cols-2 gap-x-4 gap-y-1 text-xs sm:grid-cols-3">
        {rows.map(([label, value, hint]) => (
          <div key={label} className="flex items-center justify-between gap-2">
            <span className="text-muted-foreground" title={hint}>
              {label}
            </span>
            <span className="font-medium tabular-nums">
              {formatNumber(value)}
            </span>
          </div>
        ))}
      </div>

      {totalIn > 0 && (
        <div className="space-y-1">
          <div className="text-muted-foreground flex justify-between text-[11px]">
            <span>输入里命中缓存的比例</span>
            <span className="tabular-nums">
              {cached} / {totalIn}（{(hitRate * 100).toFixed(1)}%）
            </span>
          </div>
          <div className="bg-muted h-1.5 overflow-hidden rounded-full">
            <div
              className="h-full bg-emerald-500"
              style={{ width: `${Math.min(100, hitRate * 100)}%` }}
            />
          </div>
        </div>
      )}

      <p className="text-muted-foreground text-[11px]">
        输入的两种口径已折算过：Anthropic 本就不含缓存，OpenAI / Responses 的
        prompt_tokens 含缓存，由各协议 codec 扣减。合计{" "}
        {formatNumber(usage.input_tokens + usage.output_tokens)} token。
      </p>
    </div>
  );
}

/** 响应里每种内容块的标题。用于把交错的块区分开。 */
const RESPONSE_BLOCK_LABEL: Record<ContentBlock["type"], string> = {
  text: "回答",
  thinking: "思考",
  redacted_thinking: "加密思考",
  tool_use: "工具调用",
  tool_result: "工具结果",
  image: "图片",
};

/**
 * 响应里的一个内容块。
 *
 * 文字块与思考块套一层带标题的卡片 —— 它们在事件流里是交替出现的，不标出来
 * 就看不出哪段是模型想给自己看的、哪段是给用户看的。
 * 工具调用/图片这类块自带标题，直接用 `BlockRenderer` 的成品，不再套一层。
 */
function ResponseBlock({ block }: { block: ContentBlock }) {
  if (block.type !== "text" && block.type !== "thinking") {
    return <BlockRenderer block={block} />;
  }

  const thinking = block.type === "thinking";
  const len = block.text.length;

  return (
    <div
      className={cn(
        "rounded-md border p-2",
        thinking && "bg-muted/20 border-dashed",
      )}
    >
      <div className="mb-1 flex items-center gap-2 text-[11px]">
        <span
          className={cn(
            "font-medium",
            thinking ? "text-muted-foreground" : "text-foreground",
          )}
        >
          {RESPONSE_BLOCK_LABEL[block.type]}
        </span>
        <span className="text-muted-foreground tabular-nums">
          {formatNumber(len)} 字
        </span>
        {block.type === "thinking" && block.signature && (
          <span className="text-muted-foreground">带签名</span>
        )}
      </div>
      {/* 思考默认限得更短：它是过程，不该把回答挤到屏幕外。 */}
      <BlockRenderer block={block} labels={false} />
    </div>
  );
}

export function ResponseView({
  resp,
  usage,
}: {
  resp: UnifiedResponse;
  usage?: UnifiedUsage | null;
}) {
  const content = resp.content ?? [];

  const hasText = content.some((b) => b.type === "text");
  const toolCalls = content.filter((b) => b.type === "tool_use").length;

  return (
    <div className="space-y-2">
      {/*
       * 顺序就是事件流里的顺序：模型先想、再答、最后决定调工具。
       * 按类型分桶重排（先"要调用的工具"、再"回答"、再"思考内容"）会把这个
       * 因果顺序抹掉，读起来像"思考和回答是并列的两件事"。
       */}
      <div className="space-y-2">
        {content.map((b, i) => (
          <ResponseBlock key={i} block={b} />
        ))}
      </div>

      {content.length === 0 && (
        <p className="text-muted-foreground py-4 text-center text-xs">
          （这一轮没有输出任何内容块）
        </p>
      )}

      {content.length > 0 && !hasText && (
        <p className="text-muted-foreground text-xs">
          {toolCalls > 0
            ? "模型这一轮没有输出文本，只发起了工具调用。"
            : "（没有文本内容）"}
        </p>
      )}

      {usage && (
        <Section title="Token 消耗" defaultOpen={false}>
          <UsagePanel usage={usage} />
        </Section>
      )}

      <Section title="收尾" defaultOpen={false}>
        <div className="flex flex-wrap items-center gap-2 text-xs">
          <Badge
            variant={resp.finish_reason.type === "stop" ? "success" : "warning"}
          >
            {finishText(resp.finish_reason)}
          </Badge>
          <span className="text-muted-foreground font-mono">
            id={resp.id}
          </span>
          <span className="text-muted-foreground font-mono">
            model={resp.model}
          </span>
        </div>
      </Section>
    </div>
  );
}
