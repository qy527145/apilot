import { useState } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn, formatNumber } from "@/lib/utils";

/**
 * JSON 树视图。
 *
 * 只是把 JSON 排整齐铺成一大段文本，对监控详情这种动辄几百 KB 的报文毫无用处 ——
 * 想找 `tools[3].parameters` 还是得自己数括号。所以这里做的是真正可折叠的树：
 * 每个对象/数组能收起成一行摘要，长字符串单独折叠。
 *
 * 默认只展开两层：一屏能看个大概，又不会因为展开整棵大树卡住渲染。
 *
 * 没有引第三方 JSON 查看器 —— 这类库（react-json-view 一族）维护状况普遍不好，
 * 而需要的功能就这么点，按仓库风格手写更可控。
 */

/** 长字符串（base64 图片、整段文件内容）折起来，否则一行能把页面撑爆。 */
const STRING_PREVIEW = 160;

/** 解析 JSON；失败返回 `undefined` 由调用方回退到纯文本。 */
export function tryParseJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

const isPlainObject = (v: unknown): v is Record<string, unknown> =>
  typeof v === "object" && v !== null && !Array.isArray(v);

function Primitive({ value }: { value: unknown }) {
  const [expanded, setExpanded] = useState(false);

  if (typeof value === "string") {
    const long = value.length > STRING_PREVIEW;
    return (
      <span className="break-all">
        <span className="text-emerald-600 dark:text-emerald-400">
          "{expanded || !long ? value : `${value.slice(0, STRING_PREVIEW)}…`}"
        </span>
        {long && (
          <button
            type="button"
            className="text-muted-foreground hover:text-foreground ml-1 text-[11px] underline"
            onClick={() => setExpanded((v) => !v)}
          >
            {expanded ? "收起" : `展开（${formatNumber(value.length)} 字符）`}
          </button>
        )}
      </span>
    );
  }

  if (typeof value === "number") {
    return (
      <span className="text-amber-600 dark:text-amber-400">{String(value)}</span>
    );
  }
  if (typeof value === "boolean") {
    return (
      <span className="text-purple-600 dark:text-purple-400">
        {String(value)}
      </span>
    );
  }
  if (value === null) {
    return <span className="text-muted-foreground">null</span>;
  }
  // undefined 不是合法 JSON，但值可能来自手写的结构，兜一下。
  return <span className="text-muted-foreground">{String(value)}</span>;
}

/** 收起状态下的预览内容。括号由调用方补，这里只给中间那截。 */
function summarize(
  value: Record<string, unknown> | unknown[],
  keys: string[],
): string {
  if (Array.isArray(value)) {
    return `${value.length} 项`;
  }
  const shown = keys.slice(0, 3).join(", ");
  return keys.length > 3 ? `${shown}, … ${keys.length} 个键` : shown;
}

/**
 * 缩进直接由深度算出来，而不是靠嵌套的 padding 与负 margin 相互抵消 ——
 * 后者要精确知道每一层的盒模型，改一处样式就会让闭合括号和它对应的开括号对不齐。
 *
 * 布局规则与标准 JSON 排版一致：开括号行与它的闭括号对齐在同一缩进，
 * 子项各多一级。
 */
const INDENT_EM = 1.1;
const indentOf = (depth: number) => `${depth * INDENT_EM}em`;

/**
 * 叶子节点没有折叠箭头，但要留出与箭头同宽的空位 —— 否则同一层里
 * `"a": {` 和 `"b": 2` 的起始位置会差一个箭头的宽度，整棵树看起来是歪的。
 */
function ChevronSpacer() {
  return <span className="mr-0.5 inline-block size-3 shrink-0" />;
}

function Node({
  name,
  value,
  depth,
  defaultDepth,
}: {
  name?: string;
  value: unknown;
  depth: number;
  defaultDepth: number;
}) {
  const isContainer = isPlainObject(value) || Array.isArray(value);
  const keys = isContainer ? Object.keys(value as object) : [];
  const [open, setOpen] = useState(depth < defaultDepth);

  const label = name !== undefined && (
    <>
      <span className="text-sky-600 dark:text-sky-400">"{name}"</span>
      <span className="text-muted-foreground">: </span>
    </>
  );

  if (!isContainer) {
    return (
      <div style={{ paddingLeft: indentOf(depth) }}>
        <ChevronSpacer />
        {label}
        <Primitive value={value} />
      </div>
    );
  }

  const bracket = Array.isArray(value) ? ["[", "]"] : ["{", "}"];

  if (keys.length === 0) {
    return (
      <div style={{ paddingLeft: indentOf(depth) }}>
        <ChevronSpacer />
        {label}
        <span className="text-muted-foreground">
          {bracket[0]}
          {bracket[1]}
        </span>
      </div>
    );
  }

  return (
    <div>
      <button
        type="button"
        style={{ paddingLeft: indentOf(depth) }}
        className="hover:bg-muted/50 flex w-full items-start rounded text-left"
        onClick={() => setOpen((v) => !v)}
      >
        {open ? (
          <ChevronDown className="text-muted-foreground mr-0.5 mt-[3px] size-3 shrink-0" />
        ) : (
          <ChevronRight className="text-muted-foreground mr-0.5 mt-[3px] size-3 shrink-0" />
        )}
        <span>
          {label}
          <span className="text-muted-foreground">{bracket[0]}</span>
          {!open && (
            <span className="text-muted-foreground">
              {summarize(value as Record<string, unknown> | unknown[], keys)}{" "}
              {bracket[1]}
            </span>
          )}
        </span>
      </button>

      {open && (
        <>
          {keys.map((k) => (
            <Node
              key={k}
              // 数组的下标不值得占一格引号，看起来和对象一模一样反而更难分辨。
              name={Array.isArray(value) ? undefined : k}
              value={(value as Record<string, unknown>)[k]}
              depth={depth + 1}
              defaultDepth={defaultDepth}
            />
          ))}
          <div
            className="text-muted-foreground"
            style={{ paddingLeft: indentOf(depth) }}
          >
            {bracket[1]}
          </div>
        </>
      )}
    </div>
  );
}

/**
 * 展开/收起由「默认深度」加一个重挂载令牌控制：每个节点的展开状态是它自己的
 * 局部 state，比起把路径集合提到父级再逐层下发，重挂载整棵树简单得多，
 * 也不会因为少同步一层而出现两个地方的状态打架。
 *
 * 令牌必须单独计数而不是拿 depth 当 key —— 连点两次同一个按钮时 depth 不变，
 * 那样就重置不了用户手工展开/收起的节点。
 */
export function JsonViewer({
  value,
  defaultDepth = 2,
}: {
  value: unknown;
  defaultDepth?: number;
}) {
  const [depth, setDepth] = useState(defaultDepth);
  const [resetToken, setResetToken] = useState(0);

  const apply = (d: number) => {
    setDepth(d);
    setResetToken((t) => t + 1);
  };

  // 顶层是裸值（数字、字符串）时没有可折叠的东西，不必给按钮。
  const collapsible = isPlainObject(value) || Array.isArray(value);

  return (
    <div className="space-y-1.5">
      {collapsible && (
        <div className="flex gap-2">
          {(
            [
              [1, "展开一层"],
              [2, "展开两层"],
              [Infinity, "全部展开"],
            ] as const
          ).map(([d, label]) => (
            <Button
              key={label}
              variant="ghost"
              size="sm"
              className="h-6 px-2 text-[11px]"
              onClick={() => apply(d)}
            >
              {label}
            </Button>
          ))}
        </div>
      )}
      <div
        key={resetToken}
        className={cn(
          "bg-muted/30 max-h-[45vh] min-h-[6rem] overflow-auto rounded-md border p-3",
          "font-mono text-xs leading-relaxed",
        )}
      >
        <Node value={value} depth={0} defaultDepth={depth} />
      </div>
    </div>
  );
}
