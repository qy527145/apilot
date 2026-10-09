/**
 * 监控页「自定义表达式」筛选 —— **前端这一半**。
 *
 * 历史日志的筛选在后端做（`src-tauri/src/traffic/log_filter.rs`）：日志分页、
 * 报文在另一张表里，只有后端能同时看到两者。但「进行中」的请求还没落库、
 * 没有 captures、也不分页，所以那几个（通常个位数）在前端求值就够了 ——
 * 让整个列表实时跟着表达式变，不用等一次后端往返。
 *
 * 两边必须**同语义**：同样的内置对象、同样「表达式优先、语句体兜底」的写法。
 * 改这边就要同步改那边，否则同一个表达式在「进行中」和「已完成」两处结果不一致。
 */

import type { LiveRequest } from "@/components/traffic/LiveStreamDialog";

/** 编译好的表达式。`null` 表示表达式为空或写错了 —— 两种都等于「不筛」。 */
export type CompiledExpr = (ctx: Record<string, unknown>) => boolean;

/**
 * 为一条进行中的请求组装内置对象。
 *
 * 字段与后端 `expr_context` 一一对应，只是**响应侧全为 `null`** ——
 * 请求还没结束，上游还没回话，这里如实留空而不是编一个假值。
 * 用可选链（`ctx.response?.body`）写的表达式因此不会在这里炸掉。
 */
export function buildInflightContext(r: LiveRequest): Record<string, unknown> {
  return {
    // --- 日志字段：进行中只有请求侧已知的那些 ---
    id: r.request_id,
    ts: r.ts,
    client: r.client,
    model: r.model,
    requestModel: r.request_model,
    upstreamModel: null,
    protocolIn: r.protocol_in,
    protocolOut: null,
    provider: r.provider_name || r.provider_tag || null,
    path: r.path,
    status: null,
    upstreamStatus: null,
    isStream: r.is_stream,
    error: r.error ?? null,
    latencyMs: null,
    ttfbMs: null,
    inputTokens: null,
    outputTokens: null,
    cacheReadTokens: null,
    cacheCreationTokens: null,
    quota: null,
    costUsd: null,
    cacheHit: null,

    // --- 客户端 → Apilot（method/headers/body 不落内存，留空）---
    request: { method: null, path: r.path, headers: null, body: null },
    // --- Apilot → 客户端：还没发生 ---
    response: { headers: null, body: null },
    // --- Apilot → 上游：目标已知，报文不落内存 ---
    upstreamRequest: { url: r.upstream_url ?? null, headers: null, body: null },
    // --- 上游 → Apilot：还没回话 ---
    upstreamResponse: { status: null, headers: null, body: null },
  };
}

/**
 * 编译表达式。两种写法都试：`ctx.status === 500`（表达式）与
 * `if (…) return true; return false;`（语句体）—— 与后端同一套兜底规则。
 *
 * 用 `new Function` 而不是 `eval`：前者在干净的作用域里求值，拿不到本模块的
 * 变量；用户写坏了也顶多抛异常，污染不到筛选逻辑本身。
 */
export function compileExpr(source: string): CompiledExpr | null {
  const src = source.trim();
  if (!src) return null;

  // 表达式形式：去掉结尾的分号再包进 `return (…)`，否则 `a === b;` 会变成
  // `return (a === b;);` —— 一个和用户意图无关的语法错。
  const exprForm = src.replace(/;\s*$/, "");
  const candidates = [`return (${exprForm});`, src];

  for (const body of candidates) {
    try {
      const fn = new Function("ctx", body) as CompiledExpr;
      return fn;
    } catch {
      // 语法错，试下一种写法。
    }
  }
  return null;
}

/**
 * 一条进行中的请求是否命中表达式。
 *
 * 编译不出来（用户还在打字）时返回 `true`：那时历史列表会由后端报出真正的
 * 语法错，进行中这几条不该被无声地藏起来，否则像是「请求消失了」。
 */
export function inflightMatches(
  compiled: CompiledExpr | null,
  ctx: Record<string, unknown>,
): boolean {
  if (!compiled) return true;
  try {
    return Boolean(compiled(ctx));
  } catch {
    // 脚本自己 throw（多半是读了不存在的字段）。按不命中处理，
    // 不能让它把整个列表的渲染带崩。
    return false;
  }
}