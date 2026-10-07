import { useQuery, useQueryClient } from "@tanstack/react-query";

import { api } from "@/lib/api";
import { useApilotEvent } from "@/lib/events";

export const qk = {
  appInfo: ["app_info"] as const,
  settings: ["settings"] as const,
  gateway: ["gateway"] as const,
  providers: ["providers"] as const,
  providerModels: (id: number) => ["provider_models", id] as const,
  /** 前缀键，一次失效所有渠道的模型声明（改上游名时不知道会碰到哪几个渠道）。 */
  providerModelsAll: ["provider_models"] as const,
  /** 模型目录（模型视角）。渠道侧改过模型声明后也要让它失效。 */
  modelCatalog: ["model_catalog"] as const,
  routeRules: ["route_rules"] as const,
  selectors: ["selectors"] as const,
  pricing: ["pricing"] as const,
  cacheStats: ["cache_stats"] as const,
  cachePolicy: ["cache_policy"] as const,
  clients: ["clients"] as const,
  takeoverReadiness: ["takeover_readiness"] as const,
  logs: (key: string) => ["logs", key] as const,
  requestDetail: (id: string) => ["request_detail", id] as const,
  billingTotals: (key: string) => ["billing_totals", key] as const,
  billingSummary: (key: string) => ["billing_summary", key] as const,
  billingSeries: (key: string) => ["billing_series", key] as const,
};

export function useAppInfo() {
  return useQuery({ queryKey: qk.appInfo, queryFn: api.appInfo, retry: 1 });
}

export function useSettings() {
  return useQuery({ queryKey: qk.settings, queryFn: api.getSettings, retry: 1 });
}

/** 网关状态：轮询 + 订阅 apilot://gateway 事件实时刷新 */
export function useGatewayStatus() {
  const qc = useQueryClient();
  const query = useQuery({
    queryKey: qk.gateway,
    queryFn: api.gatewayStatus,
    refetchInterval: 5000,
    retry: 1,
  });

  useApilotEvent("apilot://gateway", (status) => {
    qc.setQueryData(qk.gateway, status);
    // 网关换地址时后端会把已接管的客户端一并改指到新地址，客户端页那张表
    // 里的「当前 Base URL」跟着过期了 —— 不刷新的话，用户会以为改端口没生效。
    qc.invalidateQueries({ queryKey: qk.clients });
  });

  return query;
}
