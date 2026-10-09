/**
 * 从落库的 SSE 正文里取回单个事件块。
 *
 * 只有一个用途：监控明细的「时间轴」点了某个事件之后，得知道**它发了什么**。
 * 时间轴本身只存了 (时间点, 名字)，正文不在里面 —— 但同一份捕获里存着完整的
 * 原始 SSE 正文，且两者是同一次遍历里一前一后写下的，顺序严格一致，所以按序号
 * 就能对齐。
 */

/**
 * 按 SSE 分隔符切开正文。
 *
 * 必须与后端 `gateway::sse::take_sse_block` 逐字一致：分隔符既有 `\r\n\r\n`
 * 也有 `\n\n`，要取**最早出现**的那个，不能固定优先级 —— 只认一种的话，混用
 * 两种分隔符的上游会把两个事件粘成一块，后面的内容整体错位。
 *
 * 末尾那个不完整的块照后端一样丢掉：它还没成事件，对不上任何时间点。
 */
function splitSseBlocks(raw: string): string[] {
  const blocks: string[] = [];
  let rest = raw;

  for (;;) {
    const crlf = rest.indexOf("\r\n\r\n");
    const lf = rest.indexOf("\n\n");
    // 同一个分隔符上 `\n\n` 一定在 `\r\n\r\n` 之后（它从第二个字节起），
    // 所以只比起点即可，不会出现"两个都命中、取错那个"。
    const useCrlf = crlf >= 0 && (lf < 0 || crlf < lf);
    const pos = useCrlf ? crlf : lf;
    if (pos < 0) break;

    blocks.push(rest.slice(0, pos));
    rest = rest.slice(pos + (useCrlf ? 4 : 2));
  }

  return blocks;
}

/** 去掉行尾 `\r`。后端用的是 `str::lines()`，它也做同一件事。 */
function stripCr(line: string): string {
  return line.endsWith("\r") ? line.slice(0, -1) : line;
}

/**
 * 这个块算不算「一个事件」。
 *
 * 与后端 `parse_event` 同一条判据：必须有 `data:` 行。注释行（`:` 开头）、
 * 只有 `event:` 的保活块都不算 —— 后端也没给它们记时间点，这里多算一个就会错位。
 */
function isEvent(block: string): boolean {
  return block.split("\n").some((line) => stripCr(line).startsWith("data:"));
}

/** 取 `field:` 的值，兼容冒号后有没有空格。 */
function fieldValue(line: string, field: string): string {
  const spaced = `${field}: `;
  return line.startsWith(spaced)
    ? line.slice(spaced.length)
    : line.slice(field.length + 1);
}

/** 事件块里的 `data:` 内容；多行按 `\n` 拼接（与 `parse_event` 一致）。 */
export function sseData(block: string): string | null {
  const parts: string[] = [];
  let seen = false;

  for (const rawLine of block.split("\n")) {
    const line = stripCr(rawLine);
    // 注释行按 SSE 规范忽略。
    if (line.startsWith(":")) continue;
    if (!line.startsWith("data:")) continue;
    if (seen) parts.push("\n");
    parts.push(fieldValue(line, "data"));
    seen = true;
  }

  return seen ? parts.join("") : null;
}

function tryParse(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

function asRecord(v: unknown): Record<string, unknown> | null {
  return typeof v === "object" && v !== null
    ? (v as Record<string, unknown>)
    : null;
}

function nonEmptyString(v: unknown): string | null {
  return typeof v === "string" && v.length > 0 ? v : null;
}

/**
 * 这一块里**真正新增的内容**，用于一眼看清每个 chunk 发了什么。
 *
 * 只认三种最常见的位置：Anthropic 的 `delta.text` / `delta.thinking` /
 * `delta.partial_json`、OpenAI Chat 的 `choices[0].delta.content`、Responses 的
 * 顶层 `delta` 字符串。认不出就返回 null —— 那时下面的 JSON 树照样能看到全部
 * 字段，不必在这儿穷举所有协议的所有形状（那正是 IR 该干的事）。
 */
export function sseTextFragment(block: string): string | null {
  const data = sseData(block);
  if (!data) return null;
  const parsed = asRecord(tryParse(data));
  if (!parsed) return null;

  const delta = asRecord(parsed.delta);
  const fromDelta =
    nonEmptyString(delta?.text) ??
    nonEmptyString(delta?.thinking) ??
    nonEmptyString(delta?.partial_json) ??
    nonEmptyString(parsed.delta);
  if (fromDelta) return fromDelta;

  const choices = Array.isArray(parsed.choices) ? parsed.choices : [];
  return nonEmptyString(asRecord(asRecord(choices[0])?.delta)?.content);
}

/**
 * 把时间轴的条目与原始 SSE 事件块按序号对齐。
 *
 * 两者是同一次遍历里写下的，序号天然一致；只有捕获被上限截断时才可能对不齐，
 * 那时**只对齐能对齐的前缀**，并把 `mismatch` 交给界面说清楚 —— 宁可少显示内容，
 * 也不能把 A 事件的正文安到 B 事件的名字下面。
 */
export function pairTimingsWithEvents(
  entryCount: number,
  raw: string | null | undefined,
): { blocks: (string | null)[]; mismatch: boolean } {
  if (!raw) {
    const empty: (string | null)[] = new Array(entryCount).fill(null);
    return { blocks: empty, mismatch: false };
  }

  const events = splitSseBlocks(raw).filter(isEvent);
  const blocks: (string | null)[] = [];
  for (let i = 0; i < entryCount; i++) blocks.push(events[i] ?? null);

  return { blocks, mismatch: events.length !== entryCount };
}
