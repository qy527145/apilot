import { useMemo, useState, type ReactNode } from "react";
import {
  ChevronDown,
  ChevronRight,
  Image as ImageIcon,
  Lock,
  Search,
  Wrench,
  X,
} from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { cn, formatNumber } from "@/lib/utils";
import type {
  ContentBlock,
  FinishReason,
  Role,
  ToolChoice,
  ToolDef,
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
 * 把命中的片段标出来。
 *
 * 只做大小写不敏感的**原文切片**，不做正则 —— 用户搜的是字面量，把
 * `*` / `(` 当元字符解释会让人搜不到本来明明存在的内容。
 */
function Highlight({ text, query }: { text: string; query?: string }) {
  const q = (query ?? "").trim();
  if (!q) return <>{text}</>;

  const lower = text.toLowerCase();
  const needle = q.toLowerCase();
  const out: ReactNode[] = [];
  let from = 0;
  let at = lower.indexOf(needle);
  let key = 0;
  while (at !== -1) {
    if (at > from) out.push(text.slice(from, at));
    out.push(
      <mark
        key={key++}
        className="rounded-sm bg-amber-200/70 text-inherit dark:bg-amber-500/30"
      >
        {text.slice(at, at + needle.length)}
      </mark>,
    );
    from = at + needle.length;
    at = lower.indexOf(needle, from);
  }
  if (from < text.length) out.push(text.slice(from));
  return <>{out}</>;
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
  highlight,
}: {
  text: string;
  limit?: number;
  className?: string;
  /** 搜索词。命中的片段会被标出来。 */
  highlight?: string;
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
        <Highlight
          text={expanded || !tooLong ? text : `${text.slice(0, limit)}…`}
          query={highlight}
        />
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

function ToolCallCard({
  name,
  input,
  highlight,
}: {
  name: string;
  input: unknown;
  highlight?: string;
}) {
  return (
    <div className="bg-background rounded-md border p-2">
      <div className="mb-1 flex items-center gap-1.5">
        <Wrench className="size-3.5 shrink-0" />
        <span className="font-mono text-xs font-medium">
          <Highlight text={name} query={highlight} />
        </span>
        <span className="text-muted-foreground text-[11px]">调用参数</span>
      </div>
      <pre className="bg-muted/30 max-h-60 overflow-auto rounded border p-2 font-mono text-[11px] whitespace-pre-wrap">
        <Highlight text={JSON.stringify(input ?? {}, null, 2)} query={highlight} />
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
  highlight,
}: {
  block: ContentBlock;
  depth?: number;
  labels?: boolean;
  /** 搜索词，命中处高亮。响应视图不传。 */
  highlight?: string;
}) {
  switch (block.type) {
    case "text":
      return <LongText text={block.text} highlight={highlight} />;

    case "thinking":
      return (
        <div className={labels ? "border-l-2 border-dashed pl-2" : undefined}>
          {labels && (
            <div className="text-muted-foreground mb-1 text-[11px]">
              思考内容
              {block.signature ? "（带签名）" : ""}
            </div>
          )}
          <LongText
            text={block.text}
            limit={400}
            className="italic"
            highlight={highlight}
          />
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
      return (
        <ToolCallCard
          name={block.name}
          input={block.input}
          highlight={highlight}
        />
      );

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
              <BlockRenderer
                key={i}
                block={b}
                depth={depth + 1}
                labels={labels}
                highlight={highlight}
              />
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

    // 后端没建模的块（附件的 `document`、服务端工具的产物……）：
    // 只摊开原文，不猜它是什么 —— 硬猜一个语义才是真的会骗人。
    case "unmodeled":
      return (
        <div className="space-y-1">
          <div className="text-muted-foreground text-[11px]">
            未识别的内容块（原样展示，不做语义化渲染）
          </div>
          <pre className="bg-muted/30 overflow-auto rounded border p-2 font-mono text-[11px]">
            <Highlight text={JSON.stringify(block.raw, null, 2)} query={highlight} />
          </pre>
        </div>
      );

    // 兜底：IR 以后再加块类型时，至少把原文摊开，别让详情页白屏。
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
  query,
  hits,
}: {
  msg: UnifiedMessage;
  index: number;
  open: boolean;
  onToggle: () => void;
  /** 搜索词，命中处高亮。 */
  query?: string;
  /** 这条消息里的命中次数。为 0 时不显示徽章。 */
  hits?: number;
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
            <Highlight text={preview} query={query} />
          </span>
        )}
        {/* 命中可能落在收起摘要看不到的地方（工具结果、未建模块），
            给个次数，免得用户不知道为什么这条被留下了。 */}
        {!!hits && hits > 0 && (
          <Badge variant="secondary" className="h-4 shrink-0 px-1 text-[10px]">
            {hits} 处命中
          </Badge>
        )}
      </button>
      {open && (
        <div className="space-y-2 px-2 pb-2 pl-14">
          {msg.content.length === 0 ? (
            <p className="text-muted-foreground text-xs">（空消息）</p>
          ) : (
            msg.content.map((b, i) => (
              <BlockRenderer key={i} block={b} highlight={query} />
            ))
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

/** 消息里出现过的块类型，`tool_result` 会递归进去 —— 搜索结果里的文本块
 *  其实常常是工具输出，只看外层会把它们排除在"文本"筛选之外。 */
function blockTypesOf(blocks: ContentBlock[]): Set<ContentBlock["type"]> {
  const out = new Set<ContentBlock["type"]>();
  const walk = (bs: ContentBlock[]) => {
    for (const b of bs) {
      out.add(b.type);
      if (b.type === "tool_result") walk(b.content);
    }
  };
  walk(blocks);
  return out;
}

/**
 * 搜索用的纯文本。
 *
 * 图片与加密思考的 base64 / 密文不参与：搜它没有意义，还会让"命中 N 处"
 * 虚高。未建模的块摊成 JSON —— 用户想搜的是里面的字，不是它的结构。
 */
function searchTextOf(blocks: ContentBlock[]): string {
  const parts: string[] = [];
  const walk = (bs: ContentBlock[]) => {
    for (const b of bs) {
      switch (b.type) {
        case "text":
        case "thinking":
          parts.push(b.text);
          break;
        case "tool_use":
          parts.push(b.name, JSON.stringify(b.input ?? {}));
          break;
        case "tool_result":
          walk(b.content);
          break;
        case "unmodeled":
          parts.push(JSON.stringify(b.raw ?? null));
          break;
      }
    }
  };
  walk(blocks);
  return parts.join("\n");
}

/** 命中次数。用于给消息挂一个"N 处命中"，不关心重叠。 */
function countHits(lowerHay: string, needle: string): number {
  if (!needle) return 0;
  let n = 0;
  let at = lowerHay.indexOf(needle);
  while (at !== -1) {
    n += 1;
    at = lowerHay.indexOf(needle, at + needle.length);
  }
  return n;
}

const ROLE_ORDER: Role[] = ["system", "user", "assistant", "tool"];

const BLOCK_LABEL: Record<ContentBlock["type"], string> = {
  text: "文本",
  thinking: "思考",
  tool_use: "工具调用",
  tool_result: "工具结果",
  image: "图片",
  redacted_thinking: "加密思考",
  unmodeled: "未识别",
};

const BLOCK_ORDER: ContentBlock["type"][] = [
  "text",
  "thinking",
  "tool_use",
  "tool_result",
  "image",
  "redacted_thinking",
  "unmodeled",
];

/** 展开状态的默认值：条数少就全摊开，多了先收起 —— 一百多条消息全展开
 *  既读不动也渲染不动，而「全部展开」随时可以点。 */
const AUTO_EXPAND_LIMIT = 12;

function FilterChip({
  active,
  onClick,
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <Button
      type="button"
      size="sm"
      variant={active ? "secondary" : "outline"}
      className="h-6 px-2 text-[11px]"
      onClick={onClick}
    >
      {children}
    </Button>
  );
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

/**
 * 一个工具定义，收成一行。
 *
 * 工具定义动辄带着几百行的 `input_schema`，Claude Code 一类客户端一次发十几个，
 * 全铺开就把「对话上下文」整段挤到屏幕外。收起时只留名字与一行说明，点开才给
 * 完整描述与 schema —— 想找某个工具靠扫名字，比在一堆 JSON 里翻快得多。
 */
function ToolRow({ tool }: { tool: ToolDef }) {
  const [open, setOpen] = useState(false);
  const schema = JSON.stringify(tool.input_schema ?? {}, null, 2);

  return (
    <div className="rounded-md border">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="hover:bg-muted/40 flex w-full items-center gap-1.5 rounded-md px-2 py-1.5 text-left"
      >
        {open ? (
          <ChevronDown className="size-3.5 shrink-0" />
        ) : (
          <ChevronRight className="size-3.5 shrink-0" />
        )}
        <Wrench className="size-3.5 shrink-0" />
        {/* Codex 的 namespace 工具（functions / clock / collaboration …）由后端展平成
            一个个裸名工具，组名单独标出来，免得看着像一堆平级工具。 */}
        {tool.namespace && (
          <span className="bg-muted text-muted-foreground shrink-0 rounded px-1 py-0.5 font-mono text-[10px]">
            {tool.namespace}
          </span>
        )}
        <span className="shrink-0 font-mono text-xs font-medium">{tool.name}</span>
        {tool.description && (
          <span className="text-muted-foreground min-w-0 truncate text-[11px]">
            {tool.description}
          </span>
        )}
      </button>
      {open && (
        <div className="space-y-1 px-2 pb-2">
          {tool.description && (
            <p className="text-muted-foreground text-[11px] whitespace-pre-wrap">
              {tool.description}
            </p>
          )}
          <pre className="bg-muted/30 max-h-60 overflow-auto rounded border p-2 font-mono text-[11px] whitespace-pre-wrap">
            {schema}
          </pre>
        </div>
      )}
    </div>
  );
}

export function RequestView({ req }: { req: UnifiedRequest }) {
  // 全部展开/收起：展开状态提到父级，否则按钮没法一次影响所有消息。
  const [openMap, setOpenMap] = useState<Record<number, boolean>>({});

  // 对话的筛选条件。三个条件之间是「与」，同一组里多选是「或」。
  const [query, setQuery] = useState("");
  const [roleFilter, setRoleFilter] = useState<Role[]>([]);
  const [typeFilter, setTypeFilter] = useState<ContentBlock["type"][]>([]);

  // 后端对空的集合用了 skip_serializing_if，空时字段会整个消失，所以都要兜底。
  const tools = req.tools ?? [];
  const stop = req.stop ?? [];
  const system = req.system ?? [];
  const messages = req.messages ?? [];

  // 逐条预算：搜索文本与小写副本。这些内容动辄几 MB，不能在每次按键时
  // 重新摊一遍 —— 输入框的每次 onChange 都会重渲染。
  const rows = useMemo(
    () =>
      messages.map((m, index) => {
        const text = searchTextOf(m.content);
        return {
          index,
          msg: m,
          types: blockTypesOf(m.content),
          lower: text.toLowerCase(),
        };
      }),
    [messages],
  );

  const needle = query.trim().toLowerCase();
  const filtering =
    needle !== "" || roleFilter.length > 0 || typeFilter.length > 0;

  const visible = rows.filter(
    (r) =>
      (roleFilter.length === 0 || roleFilter.includes(r.msg.role)) &&
      (typeFilter.length === 0 || typeFilter.some((t) => r.types.has(t))) &&
      (needle === "" || r.lower.includes(needle)),
  );

  // 条数少就默认摊开，多了先收起 —— 见 AUTO_EXPAND_LIMIT 的说明。
  const defaultOpen = visible.length <= AUTO_EXPAND_LIMIT;
  const isOpen = (i: number) => openMap[i] ?? defaultOpen;

  const toggle = <T,>(list: T[], v: T): T[] =>
    list.includes(v) ? list.filter((x) => x !== v) : [...list, v];

  const presentRoles = ROLE_ORDER.filter((r) =>
    messages.some((m) => m.role === r),
  );
  const presentTypes = BLOCK_ORDER.filter((t) =>
    rows.some((r) => r.types.has(t)),
  );

  const setAllVisible = (open: boolean) => {
    const next = { ...openMap };
    visible.forEach((r) => (next[r.index] = open));
    setOpenMap(next);
  };

  const clearFilters = () => {
    setQuery("");
    setRoleFilter([]);
    setTypeFilter([]);
  };

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
        // 每个工具收起后只有一行，摊开也不占地方，所以按「有没有工具」决定，
        // 不再按个数阈值整个折叠 —— 那样会因为少看一眼就以为请求没带工具。
        defaultOpen={tools.length > 0}
      >
        {tools.length === 0 ? (
          <p className="text-muted-foreground text-xs">
            这次请求没有带工具（模型无法调用任何工具）
          </p>
        ) : (
          tools.map((t) => (
            <ToolRow key={`${t.namespace ?? ""}\u0000${t.name}`} tool={t} />
          ))
        )}
      </Section>

      <Section
        title="对话上下文"
        count={
          filtering
            ? `${visible.length} / ${messages.length} 条`
            : `${messages.length} 条`
        }
        defaultOpen={messages.length > 0}
      >
        {messages.length > 0 && (
          <div className="space-y-2">
            <div className="flex flex-wrap items-center gap-2">
              <div className="relative min-w-56 flex-1">
                <Search className="text-muted-foreground pointer-events-none absolute top-1/2 left-2 size-3.5 -translate-y-1/2" />
                <Input
                  value={query}
                  onChange={(e) => setQuery(e.target.value)}
                  placeholder="搜索内容（文本 / 思考 / 工具参数与结果）"
                  className="h-7 pr-7 pl-7 text-xs"
                />
                {query && (
                  <button
                    type="button"
                    aria-label="清除搜索"
                    className="text-muted-foreground hover:text-foreground absolute top-1/2 right-1.5 -translate-y-1/2"
                    onClick={() => setQuery("")}
                  >
                    <X className="size-3.5" />
                  </button>
                )}
              </div>
              <span className="text-muted-foreground text-[11px]">
                {filtering
                  ? `显示 ${visible.length} / ${messages.length} 条`
                  : `共 ${messages.length} 条`}
              </span>
            </div>

            <div className="flex flex-wrap items-center gap-1.5">
              <span className="text-muted-foreground w-7 text-[11px]">角色</span>
              {presentRoles.map((r) => (
                <FilterChip
                  key={r}
                  active={roleFilter.includes(r)}
                  onClick={() => setRoleFilter((prev) => toggle(prev, r))}
                >
                  {ROLE_LABEL[r]}
                </FilterChip>
              ))}
            </div>

            <div className="flex flex-wrap items-center gap-1.5">
              <span className="text-muted-foreground w-7 text-[11px]">类型</span>
              {presentTypes.map((t) => (
                <FilterChip
                  key={t}
                  active={typeFilter.includes(t)}
                  onClick={() => setTypeFilter((prev) => toggle(prev, t))}
                >
                  {BLOCK_LABEL[t]}
                </FilterChip>
              ))}
              {filtering && (
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-6 px-2 text-[11px]"
                  onClick={clearFilters}
                >
                  清除筛选
                </Button>
              )}
            </div>

            <div className="flex gap-2">
              <Button
                variant="ghost"
                size="sm"
                className="h-6 px-2 text-[11px]"
                onClick={() => setAllVisible(false)}
              >
                全部收起
              </Button>
              <Button
                variant="ghost"
                size="sm"
                className="h-6 px-2 text-[11px]"
                onClick={() => setAllVisible(true)}
              >
                全部展开
              </Button>
            </div>
          </div>
        )}

        {visible.length === 0 ? (
          <p className="text-muted-foreground text-xs">
            {messages.length === 0
              ? "（没有消息）"
              : "没有符合条件的消息。换个关键词，或点「清除筛选」。"}
          </p>
        ) : (
          visible.map((r) => (
            <MessageCard
              key={r.index}
              msg={r.msg}
              index={r.index}
              open={isOpen(r.index)}
              onToggle={() =>
                setOpenMap((s) => ({ ...s, [r.index]: !isOpen(r.index) }))
              }
              query={needle}
              hits={needle ? countHits(r.lower, needle) : 0}
            />
          ))
        )}
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
  unmodeled: "未识别的内容块",
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
