import { JsonViewer, tryParseJson } from "@/components/common/JsonViewer";
import { CopyButton } from "@/components/common/CopyButton";
import { formatNumber } from "@/lib/utils";

/**
 * 一段报文的展示：顶上一行字数 + 复制，下面是内容。
 *
 * `rawMode` 为 true 时不折行（横向滚动）—— 排查报文时哪些空白是真有的
 * 必须看得分明，折行会把它们揉平。
 */
export function RawBody({
  text,
  rawMode,
  note,
  empty = "未捕获",
}: {
  text?: string | null;
  rawMode: boolean;
  note?: string;
  empty?: string;
}) {
  if (!text) {
    return (
      <p className="text-muted-foreground py-6 text-center text-xs">{empty}</p>
    );
  }

  return (
    <div className="space-y-1.5">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <span className="text-muted-foreground text-[11px] tabular-nums">
          {formatNumber(text.length)} 字符
          {note ? `（${note}）` : rawMode ? "（未加工）" : "（已格式化）"}
        </span>
        <CopyButton text={text} />
      </div>
      {rawMode ? (
        // 原始模式用 `pre`：不折行、横向滚动，否则分不清哪些空白是报文里真有的。
        <pre className="bg-muted/30 max-h-[45vh] min-h-[6rem] overflow-auto rounded-md border p-3 font-mono text-xs whitespace-pre">
          {text}
        </pre>
      ) : (
        <FormattedBody text={text} />
      )}
    </div>
  );
}

/**
 * 格式化视图。是 JSON 就给可折叠的树，不是就退回纯文本。
 *
 * SSE 事件流、上游返回的 HTML 错误页都不是 JSON —— 那种情况硬塞进 JSON 树
 * 只会报错，所以退回纯文本并说明原因，而不是显示一片空白。
 */
export function FormattedBody({ text }: { text: string }) {
  const parsed = tryParseJson(text);

  if (parsed === undefined) {
    return (
      <div className="space-y-1">
        <p className="text-muted-foreground text-[11px]">
          这段不是 JSON（可能是 SSE 事件流或纯文本），按原文显示。切到「原始」
          可以看未经折行的版本。
        </p>
        <pre className="bg-muted/30 max-h-[45vh] min-h-[6rem] overflow-auto rounded-md border p-3 font-mono text-xs whitespace-pre-wrap">
          {text}
        </pre>
      </div>
    );
  }

  return <JsonViewer value={parsed} />;
}
