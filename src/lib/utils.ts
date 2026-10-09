import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

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

/** 简易 JSON 美化，失败时原样返回 */
export function prettyJson(raw: string | null | undefined): string {
  if (!raw) return "";
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
}
