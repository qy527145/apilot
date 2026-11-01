import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

import type { SpeedSample } from "@/lib/api";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/** 整数 quota 单位换算为美元字符串，1 USD = 500000 quota */
export const QUOTA_PER_USD = 500000;

export function quotaToUsd(quota: number | null | undefined): string {
  const v = (quota ?? 0) / QUOTA_PER_USD;
  return `$${v.toFixed(4)}`;
}

export function formatNumber(n: number | null | undefined): string {
  return (n ?? 0).toLocaleString("zh-CN");
}

export function formatPercent(n: number | null | undefined, digits = 1): string {
  return `${((n ?? 0) * 100).toFixed(digits)}%`;
}

export function formatMs(n: number | null | undefined): string {
  if (n === null || n === undefined) return "—";
  if (n < 1000) return `${Math.round(n)} ms`;
  return `${(n / 1000).toFixed(2)} s`;
}

/**
 * 输出速度。算不出来时给「—」而不是 0 —— 0 会被读成“慢到没有”，而不是“测不出”。
 */
export function formatTps(n: number | null | undefined): string {
  if (n === null || n === undefined || !Number.isFinite(n)) return "—";
  return `${n.toFixed(1)} tok/s`;
}

/**
 * 由一批观感样本算三个平均值：`(平均 TTFT, 平均 ITL, 平均 TPS)`。
 *
 * 与后端 `storage/aggregates.rs::speed_averages` **同一个口径**，改一处就得改另一处：
 *
 * - 先相加再相除，不逐条求平均 —— 平均速度该按 token 数加权，否则一个只吐 3 个
 *   token 的请求会跟一个吐 3000 个的等权，均值被前者拖垮。
 * - ITL 的分母是 Σ(tokens − 1)：每个请求的第一个 token 不是"间隔"；样本里可能有
 *   一个 token 都没吐的请求，这个差值会落到零或负数。
 * - 没有样本时三项都是 null，界面显示「—」而不是 0。
 */
export function speedAverages(samples: SpeedSample[]): {
  avgTtftMs: number | null;
  avgItlMs: number | null;
  avgTps: number | null;
} {
  if (samples.length === 0) return { avgTtftMs: null, avgItlMs: null, avgTps: null };
  let ttft = 0;
  let decodeMs = 0;
  let tokens = 0;
  for (const s of samples) {
    ttft += s.ttft_ms;
    decodeMs += s.decode_ms;
    tokens += s.output_tokens;
  }
  const intervals = tokens - samples.length;
  return {
    avgTtftMs: ttft / samples.length,
    avgItlMs: intervals > 0 ? decodeMs / intervals : null,
    avgTps: tokens > 0 ? (tokens * 1000) / decodeMs : null,
  };
}

export function formatTime(ts: number | null | undefined): string {
  if (!ts) return "—";
  const d = new Date(ts);
  const pad = (x: number) => String(x).padStart(2, "0");
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(
    d.getMinutes(),
  )}:${pad(d.getSeconds())}`;
}

export function timeAgo(ts: number | null | undefined): string {
  if (!ts) return "—";
  const diff = Date.now() - ts;
  const s = Math.floor(diff / 1000);
  if (s < 60) return `${s} 秒前`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} 分钟前`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h} 小时前`;
  return `${Math.floor(h / 24)} 天前`;
}

export function truncate(s: string | null | undefined, len = 40): string {
  if (!s) return "—";
  return s.length > len ? `${s.slice(0, len)}…` : s;
}

/**
 * 会话 ID 的短写。
 *
 * 取**末尾**而不是开头：Codex 的 ID 是 UUIDv7，开头是时间戳 —— 同一时刻开的两个
 * 对话前 8 位一模一样（实测撞过），截开头会让两行看着是同一个对话。
 */
export function shortSessionId(id: string): string {
  const tail = id.replace(/-/g, "").slice(-8);
  return tail || id;
}

/** 简易 JSON 美化，失败时原样返回 */
export function prettyJson(raw: string | null | undefined): string {
  if (!raw) return "";
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
}
