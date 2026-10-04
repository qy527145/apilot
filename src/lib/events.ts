import { useEffect, useRef } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { GatewayStatus, RequestLog } from "@/lib/api";

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

export interface ApilotEventMap {
  "apilot://gateway": GatewayStatus;
  "apilot://traffic": TrafficSnapshot;
  "apilot://selector-changed": SelectorChanged;
  "apilot://request": RequestLog;
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
