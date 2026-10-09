import { useEffect, useRef } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { GatewayStatus, RequestLog, StreamDelta } from "@/lib/api";

/* ================================================================== */
/* 事件负载类型契约                                                    */
/* ================================================================== */

export interface TrafficSnapshot {
  rps: number;
  active: number;
  ttfb_p50_ms?: number | null;
  total_requests: number;
}

export interface SelectorChanged {
  selector: string;
  provider_tag: string;
  reason: string;
}

/** 请求开始。监控页据此把一条记录放进「进行中」。 */
export interface RequestStarted {
  request_id: string;
  ts: number;
  client: string;
  /** 生效模型（计费口径）。 */
  model: string;
  /** 客户端请求的原始名字；与 `model` 不同说明被模型策略改写过了。 */
  request_model: string;
  path: string;
  protocol_in: string;
  provider_tag: string;
  provider_name: string;
  upstream_url: string;
  is_stream: boolean;
}

/** 请求结束。与 `apilot://request` 分开：那个会丢负载，当不了结束信号。 */
export interface RequestFinished {
  request_id: string;
  status_code: number;
  error?: string | null;
}

/** 流式请求里的一帧。 */
export interface StreamFrame {
  seq: number;
  at_ms: number;
  /** 上游原始 SSE 块原文（可能已截断）。 */
  raw: string;
  raw_truncated: boolean;
  /** 该帧解码出的 IR 增量，解析失败时为空。 */
  deltas: StreamDelta[];
}

/** 一批流事件。`done` 为真表示这条流结束了。 */
export interface StreamBatch {
  request_id: string;
  frames: StreamFrame[];
  done: boolean;
  truncated: boolean;
  error?: string | null;
}

export interface ApilotEventMap {
  "apilot://gateway": GatewayStatus;
  "apilot://traffic": TrafficSnapshot;
  "apilot://selector-changed": SelectorChanged;
  "apilot://request": RequestLog;
  "apilot://request-start": RequestStarted;
  "apilot://request-end": RequestFinished;
  "apilot://stream": StreamBatch;
}

export type ApilotEventName = keyof ApilotEventMap;

/**
 * 订阅 apilot 后端事件。
 * handler 会在每次事件到达时以最新引用被调用（无需写进依赖数组）。
 */
export function useApilotEvent<K extends ApilotEventName>(
  name: K,
  handler: (payload: ApilotEventMap[K]) => void,
): void {
  const ref = useRef(handler);
  ref.current = handler;

  useEffect(() => {
    let disposed = false;
    let unlisten: UnlistenFn | undefined;

    listen<ApilotEventMap[K]>(name, (event) => {
      ref.current(event.payload);
    })
      .then((fn) => {
        if (disposed) fn();
        else unlisten = fn;
      })
      .catch(() => {
        /* 在浏览器（非 Tauri）环境下静默失败 */
      });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [name]);
}

/** 非 hook 版本，供组件外订阅使用 */
export async function listenApilot<K extends ApilotEventName>(
  name: K,
  handler: (payload: ApilotEventMap[K]) => void,
): Promise<UnlistenFn> {
  return listen<ApilotEventMap[K]>(name, (event) => handler(event.payload));
}
