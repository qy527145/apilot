import { useQuery, useQueryClient } from "@tanstack/react-query";

import { api } from "@/lib/api";
import { useApilotEvent } from "@/lib/events";

export const qk = {
  appInfo: ["app_info"] as const,
  settings: ["settings"] as const,
  gateway: ["gateway"] as const,
  providers: ["providers"] as const,
  providerModels: (id: number) => ["provider_models", id] as const,
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
  });

  return query;
}
