import { useMemo } from "react";

import { cn, formatMs } from "@/lib/utils";

/** 时间轴上的一个事件。 */
export interface TimingEntry {
  /** 相对流开始的毫秒数。 */
  at_ms: number;
  /** 事件名。 */
  name: string;
}

/** 后端落库的形态：平行数组 + 名字表，见 `gateway::stream::StreamTimings`。 */
interface StoredTimings {
  at_ms?: number[];
  names?: string[];
  name_idx?: number[];
  truncated?: boolean;
}

/**
 * 把落库的平行数组还原成逐条事件。解不出来就返回 `null`。
 *
 * 宁愿少一个时间轴，也不要在明细弹窗里抛异常 —— 那会把整页的报文一起带走。
 */
export function parseTimings(
  raw: string | null | undefined,
): { entries: TimingEntry[]; truncated: boolean } | null {
  if (!raw) return null;
  try {
    const t = JSON.parse(raw) as StoredTimings;
    if (!Array.isArray(t?.at_ms) || !Array.isArray(t?.name_idx)) return null;
    const names = Array.isArray(t.names) ? t.names : [];
    return {
      entries: t.at_ms.map((at, i) => ({
        at_ms: at,
        name: names[t.name_idx![i]] ?? "—",
      })),
      truncated: !!t.truncated,
    };
  } catch {
    return null;
  }
}

/**
 * 每个事件**自己**花了多久 = 与上一个事件的时间差。
 *
 * 注意第一个事件那一项是从流开始算起的等待 —— 也就是首字节耗时。它和后面那些
 * "两个事件之间的间隔"不是一回事，但对"我到底在等谁"是同一个问题，所以一并画出来。
 */
function withGaps(entries: TimingEntry[]): Array<TimingEntry & { gap: number }> {
  return entries.map((e, i) => ({
    ...e,
    gap: Math.max(0, e.at_ms - (i === 0 ? 0 : entries[i - 1].at_ms)),
  }));
}

/**
 * 耗时条的配色。
 *
 * 用**绝对**阈值而不是"占最大值的比例"：条长负责比较，颜色负责告诉你这一下
 * 是不是真慢 —— 全按比例上色的话，一条整体都慢的流会整屏通红，反而看不出哪儿不对。
 */
function tier(ms: number): string {
  if (ms >= 1000) return "bg-destructive/60";
  if (ms >= 300) return "bg-amber-500/60";
  return "bg-foreground/15";
}

export function StreamTimeline({
  entries,
  truncated,
  selected,
  onSelect,
  barClassName = "w-20",
  showAbsolute = true,
  className,
}: {
  entries: TimingEntry[];
  truncated?: boolean;
  /** 当前选中的序号（从 1 起）。给了它才可点。 */
  selected?: number | null;
  onSelect?: (seq: number) => void;
  /** 条的宽度档位。侧边栏里窄，整页里可以宽些。 */
  barClassName?: string;
  /**
   * 是否显示"相对流开始的位置"那一列。
   *
   * 窄侧边栏里关掉：实时看图的是"每个事件多久"，绝对位置在长回答里意义有限，
   * 而少一列能让事件名多出一倍的可读宽度。
   */
  showAbsolute?: boolean;
  className?: string;
}) {
  const rows = useMemo(() => withGaps(entries), [entries]);
  // 分母取最慢的那个：这样"最慢的一根"总是满格，扫一眼就知道尖在哪里。
  const slowest = rows.reduce((a, b) => (b.gap > a.gap ? b : a), rows[0]);

  if (rows.length === 0) {
    return (
      <p className={cn("text-muted-foreground py-6 text-center text-xs", className)}>
        还没有事件。
      </p>
    );
  }

  const max = Math.max(1, slowest.gap);

  return (
    <div className={cn("space-y-2", className)}>
      <div className="text-muted-foreground flex flex-wrap items-baseline gap-x-3 gap-y-1 text-[11px]">
        <span>共 {rows.length} 个事件</span>
        <span>
          最慢 {formatMs(slowest.gap)}（第 {rows.indexOf(slowest) + 1} 个 ·{" "}
          <span className="font-mono">{slowest.name}</span>）
        </span>
        {truncated && <span className="text-amber-600 dark:text-amber-400">只记了前 {rows.length} 个</span>}
      </div>

      <div className="overflow-hidden rounded-md border">
        {rows.map((e, i) => {
          const seq = i + 1;
          const active = selected === seq;
          return (
            <button
              key={seq}
              type="button"
              onClick={onSelect ? () => onSelect(seq) : undefined}
              className={cn(
                "flex w-full items-center gap-2 border-b px-2 py-1 text-left text-[11px] last:border-b-0",
                onSelect && "cursor-pointer",
                active && "bg-accent",
              )}
            >
              <span className="text-muted-foreground w-8 shrink-0 tabular-nums">
                {seq}
              </span>

              <span
                className={cn(
                  "bg-muted h-1.5 shrink-0 overflow-hidden rounded-full",
                  barClassName,
                )}
              >
                <span
                  className={cn("block h-full rounded-full", tier(e.gap))}
                  style={{ width: `${(e.gap / max) * 100}%` }}
                />
              </span>

              <span className="min-w-0 flex-1 truncate font-mono">{e.name}</span>

              <span className="w-16 shrink-0 text-right tabular-nums">
                {formatMs(e.gap)}
              </span>

              {showAbsolute && (
                <span className="text-muted-foreground w-16 shrink-0 text-right tabular-nums">
                  +{formatMs(e.at_ms)}
                </span>
              )}
            </button>
          );
        })}
      </div>

      <p className="text-muted-foreground text-[11px]">
        左边是每个事件自己的耗时（与上一个事件的时间差，第一个是等到首字节的时间），
        右边是它相对流开始的位置。
      </p>
    </div>
  );
}
