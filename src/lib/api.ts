import { invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";

/* ================================================================== */
/* 类型契约 —— 严格对齐后端 Rust 结构体（字段 snake_case）            */
/* ================================================================== */

export type Protocol = "anthropic" | "openai_chat" | "openai_responses";
export type ProviderKind = "anthropic" | "openai_chat" | "openai_responses";
export type AuthStyle = "bearer" | "x-api-key" | "none";

/** 协议名 → 中文标签。多处复用，避免同一个名字在三个页面里写法不一致。 */
export const PROTOCOL_LABEL: Record<Protocol, string> = {
  anthropic: "Anthropic Messages",
  openai_chat: "OpenAI Chat",
  openai_responses: "OpenAI Responses",
};

/** 各协议的标准请求路径。与后端 `Protocol::default_path()` 一一对应。 */
export const PROTOCOL_DEFAULT_PATH: Record<Protocol, string> = {
  anthropic: "/v1/messages",
  openai_chat: "/v1/chat/completions",
  openai_responses: "/v1/responses",
};

export const ALL_PROTOCOLS: Protocol[] = [
  "anthropic",
  "openai_chat",
  "openai_responses",
];

/**
 * 服务商在某种协议下的接入点。
 *
 * `path` 留空表示用该协议的默认路径；填了就**原样**拼在 base_url 后面
 * （不会替你补 /v1）—— 服务商把接口挂在子路径下时靠它避免 404。
 */
export interface ProtocolEndpoint {
  protocol: Protocol;
  path?: string | null;
}

export interface ProviderInput {
  id?: number | null;
  tag: string;
  name: string;
  kind: ProviderKind;
  base_url: string;
  api_key?: string | null;
  auth_style: AuthStyle;
  /** 该服务商支持哪些协议。留空表示"只支持 kind 那一种"。 */
  protocols: ProtocolEndpoint[];
  extra_headers: Record<string, string>;
  param_override?: unknown | null;
  model_mapping: Record<string, string>;
  weight: number;
  priority: number;
  enabled: boolean;
  timeout_ms: number;
}

export interface Provider {
  id: number;
  tag: string;
  name: string;
  kind: ProviderKind;
  base_url: string;
  auth_style: AuthStyle;
  protocols: ProtocolEndpoint[];
  extra_headers: Record<string, string>;
  param_override?: unknown | null;
  model_mapping: Record<string, string>;
  weight: number;
  priority: number;
  enabled: boolean;
  timeout_ms: number;
  created_at: number;
  updated_at: number;
}

export interface GatewayStatus {
  running: boolean;
  host: string;
  port: number;
  error?: string | null;
}

export interface AppSettings {
  listen_host: string;
  listen_port: number;
  autostart_gateway: boolean;
  first_byte_timeout_ms: number;
  idle_timeout_ms: number;
  request_timeout_ms: number;
  capture_enabled: boolean;
  capture_max_entries: number;
  cache_enabled: boolean;
  cache_ttl_secs: number;
  cache_max_entries: number;
}

/* ------------------------------ 路由规则 ------------------------------ */

export type RuleItem =
  | { type: "client"; any: string[] }
  | { type: "model"; patterns: string[] }
  | { type: "protocol"; any: Protocol[] }
  | { type: "path"; prefixes: string[] }
  | { type: "header"; name: string; equals?: string | null }
  | { type: "token_estimate"; min?: number | null; max?: number | null }
  | { type: "logical"; mode: "and" | "or"; invert: boolean; rules: RuleItem[] };

export type RouteAction =
  | { type: "final"; selector: string }
  | { type: "reject"; reason: string }
  | { type: "model_override"; model: string }
  | { type: "route_options"; target_selector?: string | null; cache?: boolean | null }
  | { type: "sniff" };

export interface RouteRuleInput {
  id?: number | null;
  name: string;
  enabled: boolean;
  items: RuleItem[];
  action: RouteAction;
}

export interface RouteRule {
  id: number;
  sort_index: number;
  name: string;
  enabled: boolean;
  items: RuleItem[];
  action: RouteAction;
  updated_at: number;
}

export interface SelectorInput {
  tag: string;
  name: string;
  mode: "selector" | "urltest";
  members: string[];
  tolerance_ms: number;
}

export interface Selector {
  tag: string;
  name: string;
  mode: "selector" | "urltest";
  members: string[];
  current_provider?: string | null;
  tolerance_ms: number;
}

export interface ProbeResult {
  tag: string;
  ok: boolean;
  latency_ms?: number | null;
  error?: string | null;
}

/* ------------------------------ 计费 ------------------------------ */

export interface PricingInput {
  model: string;
  model_ratio: number;
  completion_ratio: number;
  cache_ratio: number;
  cache_create_ratio: number;
  group_ratio: number;
  image_ratio: number;
  audio_ratio: number;
  tool_call_surcharge: number;
}

export interface Pricing extends PricingInput {
  other_ratios: Record<string, number>;
  currency: string;
  updated_at: number;
}

export interface TimeRange {
  from: number;
  to: number;
}

export type GroupBy = "client" | "model" | "provider";

export interface BillingBucket {
  key: string;
  requests: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  quota: number;
  cache_hits: number;
  saved_quota: number;
}

export interface HourlyPoint {
  bucket_ts: number;
  requests: number;
  quota: number;
  input_tokens: number;
  output_tokens: number;
}

export interface BillingSummary {
  requests: number;
  quota: number;
  cost_usd: number;
  input_tokens: number;
  output_tokens: number;
  cache_saved_quota: number;
  cache_hit_rate: number;
  p50_ttfb_ms?: number | null;
}

/* ------------------------------ 缓存 ------------------------------ */

export interface CacheStats {
  entries: number;
  hits: number;
  misses: number;
  hit_rate: number;
  saved_quota: number;
  total_bytes: number;
}

export interface CachePolicy {
  enabled: boolean;
  ttl_secs: number;
  max_entries: number;
}

export interface CacheScope {
  kind: "all" | "expired" | "model";
  model?: string | null;
}

/* ------------------------------ 日志 ------------------------------ */

export interface LogFilter {
  client?: string | null;
  model?: string | null;
  provider_tag?: string | null;
  from?: number | null;
  to?: number | null;
  only_cache_hit?: boolean | null;
  limit: number;
  offset: number;
}

export interface RequestLog {
  request_id: string;
  ts: number;
  client: string;
  protocol_in: string;
  protocol_out: string;
  provider_tag?: string | null;
  model: string;
  request_model: string;
  /** 入站请求路径（客户端打给 Apilot 的）。 */
  path: string;
  /** Apilot 实际请求的上游 URL。缓存命中 / 路由失败时为 null。 */
  upstream_url?: string | null;
  /** 映射后实际发给上游的模型名。与 model 不同时说明渠道做了映射。 */
  upstream_model?: string | null;
  /** 上游返回的原始状态码，可能与我们返回给客户端的 status_code 不同。 */
  upstream_status?: number | null;
  is_stream: boolean;
  status_code: number;
  error_message?: string | null;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
  usage_source: "upstream" | "local";
  quota: number;
  cost_usd: number;
  latency_ms: number;
  ttfb_ms?: number | null;
  cache_hit: boolean;
  saved_quota: number;
}

export interface Page<T> {
  items: T[];
  total: number;
}

/** 清空日志的结果：各删了多少条。 */
export interface ClearResult {
  logs: number;
  captures: number;
}

export interface RequestDetail extends RequestLog {
  /* --- 入站：客户端 → Apilot --- */
  method: string;
  request_headers: Record<string, string>;
  request_body?: string | null;

  /* --- 出站：Apilot → 上游 --- */
  upstream_headers: Record<string, string>;
  upstream_body?: string | null;
  upstream_response_headers: Record<string, string>;
  upstream_response_body?: string | null;

  /* --- 返回给客户端 --- */
  response_headers: Record<string, string>;
  response_body?: string | null;
  stream_text?: string | null;
  stream_events: number;
}

/* --------------------------- 客户端接管 --------------------------- */

export interface ClientDetect {
  id: string;
  name: string;
  config_path: string;
  detected: boolean;
  taken_over: boolean;
  current_base_url?: string | null;
}

export interface TakeoverResult {
  client: string;
  applied: boolean;
  backup_path?: string | null;
  message: string;
}

export interface AppInfo {
  name: string;
  version: string;
  apilot_home: string;
  db_path: string;
  started_at: number;
}

export interface ProviderModel {
  model: string;
  upstream_model?: string | null;
}

/** 接管前置条件。判定在后端，前端只负责展示缺了哪一步。 */
export interface TakeoverReadiness {
  gateway_running: boolean;
  has_models: boolean;
  ready: boolean;
  reason?: string | null;
}

/* ================================================================== */
/* 错误处理                                                            */
/* ================================================================== */

interface BackendError {
  code: string;
  message: string;
}

export class ApiError extends Error {
  code: string;
  constructor(code: string, message: string) {
    super(message);
    this.name = "ApiError";
    this.code = code;
  }
}

function normalizeError(err: unknown): ApiError {
  if (typeof err === "object" && err !== null) {
    const e = err as Partial<BackendError> & { message?: string };
    if (typeof e.message === "string") {
      return new ApiError(typeof e.code === "string" ? e.code : "unknown", e.message);
    }
  }
  if (typeof err === "string") return new ApiError("unknown", err);
  return new ApiError("unknown", "未知错误，请重试");
}

/**
 * 统一 invoke 封装：失败时弹出 toast 并抛出 ApiError，方便 react-query 处理。
 */
async function call<T>(
  cmd: string,
  args?: Record<string, unknown>,
): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (err) {
    const e = normalizeError(err);
    toast.error(e.message);
    throw e;
  }
}

/* ================================================================== */
/* 命令封装（名称与后端清单完全一致）                                 */
/* ================================================================== */

export const api = {
  /* ---- app / settings ---- */
  appInfo: () => call<AppInfo>("app_info"),
  getSettings: () => call<AppSettings>("get_settings"),
  updateSettings: (settings: AppSettings) =>
    call<AppSettings>("update_settings", { settings }),

  /* ---- gateway ---- */
  gatewayStart: () => call<GatewayStatus>("gateway_start"),
  gatewayStop: () => call<GatewayStatus>("gateway_stop"),
  gatewayStatus: () => call<GatewayStatus>("gateway_status"),

  /* ---- providers ---- */
  listProviders: () => call<Provider[]>("list_providers"),
  upsertProvider: (input: ProviderInput) =>
    call<Provider>("upsert_provider", { input }),
  deleteProvider: (id: number) => call<null>("delete_provider", { id }),
  testProvider: (id: number) => call<ProbeResult>("test_provider", { id }),
  listProviderModels: (providerId: number) =>
    call<ProviderModel[]>("list_provider_models", { providerId }),
  setProviderModels: (providerId: number, models: ProviderModel[]) =>
    call<null>("set_provider_models", { providerId, models }),
  /** 拉取上游 `GET {base_url}/v1/models`，返回模型 id 列表。 */
  fetchProviderModels: (id: number) =>
    call<string[]>("fetch_provider_models", { id }),

  /* ---- routing: rules ---- */
  listRouteRules: () => call<RouteRule[]>("list_route_rules"),
  upsertRouteRule: (input: RouteRuleInput) =>
    call<RouteRule>("upsert_route_rule", { input }),
  deleteRouteRule: (id: number) => call<null>("delete_route_rule", { id }),
  reorderRouteRules: (ids: number[]) =>
    call<null>("reorder_route_rules", { ids }),
  setFinalSelector: (tag: string) => call<null>("set_final_selector", { tag }),

  /* ---- routing: selectors ---- */
  listSelectors: () => call<Selector[]>("list_selectors"),
  upsertSelector: (input: SelectorInput) =>
    call<Selector>("upsert_selector", { input }),
  deleteSelector: (tag: string) => call<null>("delete_selector", { tag }),
  switchSelector: (selector: string, providerTag: string) =>
    call<Selector>("switch_selector", { selector, providerTag }),
  runUrltest: (selector: string) =>
    call<ProbeResult[]>("run_urltest", { selector }),

  /* ---- billing / pricing ---- */
  listPricing: () => call<Pricing[]>("list_pricing"),
  upsertPricing: (input: PricingInput) =>
    call<Pricing>("upsert_pricing", { input }),
  deletePricing: (model: string) => call<null>("delete_pricing", { model }),
  billingSummary: (range: TimeRange, groupBy: GroupBy) =>
    call<BillingBucket[]>("billing_summary", { range, groupBy }),
  billingTotals: (range: TimeRange) =>
    call<BillingSummary>("billing_totals", { range }),
  billingTimeseries: (range: TimeRange) =>
    call<HourlyPoint[]>("billing_timeseries", { range }),

  /* ---- cache ---- */
  cacheStats: () => call<CacheStats>("cache_stats"),
  clearCache: (scope: CacheScope) => call<number>("clear_cache", { scope }),
  getCachePolicy: () => call<CachePolicy>("get_cache_policy"),
  setCachePolicy: (policy: CachePolicy) =>
    call<CachePolicy>("set_cache_policy", { policy }),

  /* ---- clients takeover ---- */
  detectClients: () => call<ClientDetect[]>("detect_clients"),
  takeoverStatus: () => call<ClientDetect[]>("takeover_status"),
  takeoverReadiness: () => call<TakeoverReadiness>("takeover_readiness"),
  previewTakeover: (client: string) =>
    call<string>("preview_takeover", { client }),
  applyTakeover: (client: string) =>
    call<TakeoverResult>("apply_takeover", { client }),
  restoreClient: (client: string) =>
    call<TakeoverResult>("restore_client", { client }),

  /* ---- logs ---- */
  queryLogs: (filter: LogFilter) => call<Page<RequestLog>>("query_logs", { filter }),
  getRequestDetail: (requestId: string) =>
    call<RequestDetail>("get_request_detail", { requestId }),
  /** 清空请求明细与原文捕获。计费聚合不受影响。 */
  clearLogs: () => call<ClearResult>("clear_logs"),
};

export type Api = typeof api;
