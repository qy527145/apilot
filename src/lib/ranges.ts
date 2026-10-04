import type { TimeRange } from "@/lib/api";

export type RangePreset = "1h" | "24h" | "7d" | "30d";

export const RANGE_LABELS: Record<RangePreset, string> = {
  "1h": "1 小时",
  "24h": "24 小时",
  "7d": "7 天",
  "30d": "30 天",
};

const HOUR = 3600_000;
const DAY = 24 * HOUR;

export function rangeToTimeRange(preset: RangePreset, now = Date.now()): TimeRange {
  const map: Record<RangePreset, number> = {
    "1h": HOUR,
    "24h": DAY,
    "7d": 7 * DAY,
    "30d": 30 * DAY,
  };
  return { from: now - map[preset], to: now };
}

/** 今日 0 点 -> 现在 */
export function todayRange(now = Date.now()): TimeRange {
  const d = new Date(now);
  d.setHours(0, 0, 0, 0);
  return { from: d.getTime(), to: now };
}
