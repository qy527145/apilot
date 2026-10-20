import { invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";

// 只借个类型：事件负载的形状本来就在 `events.ts` 里，那边也反过来借这里的
// `StreamDelta`。两边都是 `import type`，编译后不留运行时依赖，不成环。
import type { StreamFrame } from "@/lib/events";

/* ================================================================== */
/* 类型契约 —— 严格对齐后端 Rust 结构体（字段 snake_case）            */
/* ================================================================== */

export type Protocol = "anthropic" | "openai_chat" | "openai_responses";
export type ProviderKind = "anthropic" | "openai_chat" | "openai_responses";
export type AuthStyle = "bearer" | "x-api-key" | "none";

/**
 * 渠道的模型元数据（价格 / 能力）来源。
 *
 * - `auto`：默认，等价 models.dev；从预设添加的渠道天然应该绑定目录。
 * - `models_dev` / `litellm`：明确指定目录来源。
 * - `off`：不自动同步，价格全靠手填。
 */
export type MetadataSource = "auto" | "models_dev" | "litellm" | "off";

export const METADATA_SOURCE_LABEL: Record<MetadataSource, string> = {
  auto: "自动（models.dev）",
  models_dev: "models.dev",
  litellm: "LiteLLM",
  off: "不自动同步",
};

/** 渠道级代理怎么选。与后端 `ChannelProxyMode` 一一对应。 */
export type ChannelProxyMode = "inherit" | "direct" | "manual";

export interface ChannelProxy {
  mode: ChannelProxyMode;
  /** 支持 http:// / https:// / socks5:// / socks5h://。 */
  url?: string | null;
}

/** 全局出站代理怎么选。与后端 `ProxyMode` 一一对应。 */
export type ProxyMode = "direct" | "system" | "manual";

export interface ProxySettings {
  mode: ProxyMode;
  url?: string | null;
  /**
   * 忽略上游 TLS 证书校验（等价于 `curl -k`）。
   *
   * 给抓包工具兜底用：mitmproxy 之类要用自签 CA 重新签一遍才能解密 HTTPS，
   * 装了 CA 是正路，这个开关是装不了 CA 时的退路。开启后中间人无法再被发现，
   * 所以默认关，开启时界面上要挂警示。
   *
   * 后端 `ProxySettings.insecure_tls`，作用范围是**所有出站连接**，与走不走代理无关。
   */
  insecure_tls: boolean;
}

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
  name: string;
  kind: ProviderKind;
  base_url: string;
  api_key?: string | null;
  auth_style: AuthStyle;
  /** 该服务商支持哪些协议。留空表示"只支持 kind 那一种"。 */
  protocols: ProtocolEndpoint[];
  extra_headers: Record<string, string>;
  param_override?: unknown | null;
  weight: number;
  priority: number;
  enabled: boolean;
  timeout_ms: number;
  /** 该渠道走不走代理。省略表示跟随全局设置。 */
  proxy?: ChannelProxy;
  /** 模型元数据来源。省略 = `auto`（models.dev）。 */
  metadata_source?: MetadataSource;
}

export interface Provider {
  id: number;
  /** 内部生成的唯一 slug，前端无需展示或编辑。 */
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
  proxy: ChannelProxy;
  /** 模型元数据来源。保存渠道后会按它自动同步价格。 */
  metadata_source: MetadataSource;
  created_at: number;
  updated_at: number;
}

/* --------------------------- 协议自动检测 --------------------------- */

/**
 * 协议探测的判定。与后端 `ProtocolVerdict` 一一对应，用词与能力探测
 * （`CapabilityVerdict`）保持一致。
 *
 * **`inconclusive` 不是凑数的中间态**：鉴权失败、限流、上游 5xx、回包是网页，
 * 都说明不了"这条路径有没有这个协议的入口"。把它压成 false，用户会以为服务商
 * 不支持，而真实原因可能只是密钥没填对。
 */
export type ProtocolVerdict = "supported" | "unsupported" | "inconclusive";

export const PROTOCOL_VERDICT_LABEL: Record<ProtocolVerdict, string> = {
  supported: "有入口",
  unsupported: "无入口",
  inconclusive: "未知",
};

/** 一种协议的探测结果。 */
export interface ProtocolDetection {
  protocol: Protocol;
  verdict: ProtocolVerdict;
  /** 实际探测的出站地址。上游报错时第一个要看的就是它。 */
  url: string;
  status?: number | null;
  latency_ms?: number | null;
  /** 判定依据。 */
  note?: string | null;
}

/**
 * 协议检测的入参。只描述"怎么连"，不含 tag / name —— 检测发生在保存之前。
 *
 * `id` 是给密钥用的：编辑已有渠道时表单不回显密钥，留空表示沿用库里存的那把，
 * 否则检测会不带鉴权头打过去，三种协议一律 401、结论全成"未知"。
 */
export interface ProtocolDetectInput {
  id?: number | null;
  base_url: string;
  api_key?: string | null;
  auth_style: AuthStyle;
  extra_headers: Record<string, string>;
  proxy?: ChannelProxy;
  /** 每种协议要试的路径；`path` 为空表示用协议默认路径。 */
  paths: ProtocolEndpoint[];
}

/* --------------------------- 模型（模型视角） --------------------------- */

/**
 * 模型替换的模式。全局与客户端两级用的是同一套。
 *
 * - `passthrough` 用客户端请求的模型（默认）
 * - `always` 一律换成选定的模型
 * - `fallback` 只当请求的模型在 Apilot 里没有可用渠道时才换
 * - `custom` 自定义规则（映射表或 JS 脚本）
 */
export type ModelPolicyMode = "passthrough" | "always" | "fallback" | "custom";

/** 客户端的模式：多一个「跟随全局」。 */
export type ClientMode =
  | "inherit"
  | "passthrough"
  | "always"
  | "fallback"
  | "custom";

/** 自定义规则的两种写法。两套会同时保存，切换时另一套不丢。 */
export type CustomForm = "table" | "script";

/** 映射表一行的匹配方式。 */
export type MatchKind = "prefix" | "glob" | "regex" | "exact" | "any";

export interface MappingRow {
  /** 只对这个客户端生效；空 = 任何客户端都适用。 */
  client?: string | null;
  match_kind: MatchKind;
  /** 表达式；`any` 时忽略。 */
  pattern?: string | null;
  /** 命中后换成它；空 = 「保持原样」，在此停下且不改写。 */
  target?: string | null;
}

export interface CustomRules {
  form: CustomForm;
  /** 自上而下，首个命中生效。 */
  table: MappingRow[];
  /** JS 源码，须定义一个 `resolve(ctx)`。 */
  script?: string | null;
}

/** 单个客户端的覆盖。没写的字段沿用全局。 */
export interface ClientRule {
  mode: ClientMode;
  /** `always` / `fallback` 用的模型；留空则沿用全局的 `active_model`。 */
  model?: string | null;
  /** `custom` 用的规则。 */
  custom: CustomRules;
}

export interface ModelPolicy {
  mode: ModelPolicyMode;
  /** `always` / `fallback` 的目标模型；客户端规则没写模型时也用它。 */
  active_model?: string | null;
  /** 客户端级覆盖：客户端标识 → 覆盖配置。 */
  per_client: Record<string, ClientRule>;
  /** 全局 `custom` 用的规则。 */
  custom: CustomRules;
}

/** 空的自定义规则 —— 新建客户端覆盖时用它兜底。 */
export function emptyCustomRules(): CustomRules {
  return { form: "table", table: [], script: null };
}

/** 同一个模型在多个渠道都有时怎么选。 */
export type ModelStrategy = "priority" | "latency" | "weight";

/** 一条候选渠道。priority / weight 是**这个模型在**该渠道上的值。 */
export interface ModelCandidate {
  provider_tag: string;
  provider_name: string;
  upstream_model?: string | null;
  priority: number;
  weight: number;
  enabled: boolean;
  /** 最近一次测速的延迟；没测过是 null。 */
  latency_ms?: number | null;
}

export interface ModelPolicyRecord {
  model: string;
  strategy: ModelStrategy;
  /** 手动切换到的渠道；null 表示按策略自动。 */
  active_provider?: string | null;
}

export interface ModelCatalogEntry {
  model: string;
  /** 含被停用的渠道 —— 否则停用之后就再也启用不回来了。 */
  candidates: ModelCandidate[];
  /** null 表示这个模型交给路由规则与选择器管。 */
  policy?: ModelPolicyRecord | null;
  /** 当前会走哪个渠道。加权随机策略下为 null（每次请求重抽）。 */
  primary?: string | null;
}

/** 写候选渠道时的入参（比读模型少几个只读字段）。 */
export interface ModelCandidateInput {
  provider_tag: string;
  upstream_model?: string | null;
  priority: number;
  weight: number;
  enabled: boolean;
}

export interface ModelOption {
  model: string;
  provider_count: number;
}

export interface GatewayStatus {
  running: boolean;
  host: string;
  port: number;
  error?: string | null;
}

/**
 * 接管客户端时对它模型配置做什么。
 *
 * - `off`：不碰客户端配置。
 * - `rename`：写模型名。绕开 Codex 的 Responses Lite，不依赖网关，代价是没有 apply_patch。
 * - `catalog`：下发模型目录地址。拿到完整元数据（含 apply_patch），代价是要多写两个
 *   Codex 开关、且客户端启动时得够得着网关。
 * - `both`：两个都写（默认）—— 目录取不到时正好轮到名字那条路兜底。
 */
export type ClientModelMode = "off" | "rename" | "catalog" | "both";

/**
 * 接管思考的档位。**与写客户端配置无关** —— 它在网关侧改写请求参数，
 * 所以每一条对话请求都生效，不用重启客户端、也不用重新接管一次。
 *
 * - `off`：不接管，客户端发什么就是什么。
 * - `disabled`：不发送思考参数（删掉 `thinking` / `reasoning_effort` / `reasoning`），
 *   由上游按默认处理 —— 不等于"上游一定不思考"。
 * - `low` / `medium` / `high` / `xhigh`：按上游的**线协议**折算成对应参数
 *   （Anthropic 是 `thinking.budget_tokens`，OpenAI 系是 `effort`）。
 */
export type ThinkingMode = "off" | "disabled" | "low" | "medium" | "high" | "xhigh";

export interface AppSettings {
  /** 实际绑定地址。由 `allow_lan` 决定：关着时恒为 127.0.0.1。 */
  listen_host: string;
  listen_port: number;
  autostart_gateway: boolean;
  /** 局域网访问。默认关 —— 关着只绑回环。 */
  allow_lan: boolean;
  /**
   * 允许任意源站的网页调用网关（CORS）。默认开。
   *
   * 关掉后跨源网页请求会被网关挡回去：任意页面都能向本机发请求、花掉渠道额度。
   */
  allow_browser_origins: boolean;
  first_byte_timeout_ms: number;
  idle_timeout_ms: number;
  request_timeout_ms: number;
  capture_enabled: boolean;
  capture_max_entries: number;
  cache_enabled: boolean;
  cache_ttl_secs: number;
  cache_max_entries: number;
  /** 模型替换。默认 `mode: "passthrough"`（不改写）。 */
  model_policy: ModelPolicy;
  /**
   * 接管客户端时怎么让客户端「正确地说话」。默认 `"both"`（两个都写）。
   *
   * 两个手段解决同一件事的两面，详见 `src-tauri/src/codex/mod.rs`。
   */
  client_model_mode: ClientModelMode;
  /** 接管思考（是否思考 / 思考深度）。默认 `"off"`（不接管）。 */
  thinking_mode: ThinkingMode;
  /** 全局出站代理。默认 `mode: "system"`（跟随环境变量）。 */
  proxy: ProxySettings;
  /**
   * 跨协议转 Anthropic 时自动打 prompt cache 断点。默认开。
   * 直通请求不碰（客户端自己的断点原样保留）。
   */
  prompt_cache_injection: boolean;
  /** 请求改写脚本（JS）。空 = 不改写。 */
  request_script: string;
  /** 响应改写脚本（JS，只对非流式生效）。空 = 不改写。 */
  response_script: string;
  /**
   * 流式增量改写脚本（JS，只对流式生效）。空 = 不改写。
   *
   * 每解出一个 delta 调一次，改完立即下发，首字延迟不变。脚本看不到完整回答，
   * 文本增量可能把一个词切成两半。
   */
  response_delta_script: string;
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
  /**
   * 这行价格的来源。
   *
   * `null` / 省略 = **用户手填的**，「从目录更新价格」一律不碰它。
   * 有值（如 `catalog:models.dev`）表示由某份目录导入，下次导入可以覆盖。
   */
  source?: string | null;
  updated_at: number;
}

export interface TimeRange {
  from: number;
  to: number;
}

export type GroupBy = "client" | "model" | "provider";

/** 被折叠进某个生效模型的「客户端请求的模型」（模型策略改写 / 兜底替换）。 */
export interface RequestModelAlias {
  model: string;
  requests: number;
}

export interface BillingBucket {
  key: string;
  requests: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  /** 上游提示缓存命中率：缓存读 /（新鲜输入 + 缓存读）。 */
  prompt_cache_hit_rate: number;
  quota: number;
  /** 命中**本地响应缓存**的请求条数，与上一行不是一回事。 */
  cache_hits: number;
  saved_quota: number;
  /**
   * 三个平均值的样本条数：流式、非缓存命中、且量得出解码窗口的请求。
   * 为 0 时三个平均值都是 null（界面显示「—」）—— 这一桶里没有可比的生成过程。
   */
  sample_requests: number;
  /** 平均首字节耗时（毫秒）。无样本时为 null。 */
  avg_ttft_ms: number | null;
  /** 平均 token 间隔（毫秒）。无样本时为 null。 */
  avg_itl_ms: number | null;
  /** 平均输出速度（token / 秒，按 token 数加权）。无样本时为 null。 */
  avg_tps: number | null;
  /** 只有「按模型」维度非空，其余维度恒为空数组。 */
  request_models: RequestModelAlias[];
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
  cache_read_tokens: number;
  cache_saved_quota: number;
  /** 命中本地响应缓存的请求占比 —— 不是上游提示缓存的命中率。 */
  local_cache_hit_rate: number;
  /** 上游提示缓存命中率：缓存读 /（新鲜输入 + 缓存读）。 */
  prompt_cache_hit_rate: number;
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

export type LogStatus = "ok" | "error";

export interface LogFilter {
  client?: string | null;
  model?: string | null;
  /** 客户端请求的模型名精确匹配（下拉筛选用）。 */
  request_model?: string | null;
  provider_tag?: string | null;
  from?: number | null;
  to?: number | null;
  only_cache_hit?: boolean | null;
  /** 入站协议。认不出的值后端会忽略（等同于不筛）。 */
  protocol?: Protocol | null;
  status?: LogStatus | null;
  is_stream?: boolean | null;
  /** 模型名模糊匹配，同时命中实际路由模型（`model`）和客户端请求模型（`request_model`）。 */
  model_like?: string | null;
  /**
   * 自定义 JS 表达式筛选。后端对「日志字段 + 捕获报文」求值，命中的才返回。
   * 内置对象与写法见 `FilterExprPanel`，实现见 `src-tauri/src/traffic/log_filter.rs`。
   */
  expr?: string | null;
  /** 只看某一个对话的请求。 */
  session_id?: string | null;
  limit: number;
  offset: number;
}

/** 筛选下拉的候选值，来自最近这些请求里实际出现过的内容。 */
export interface LogFacets {
  clients: string[];
  /** 客户端请求的模型名（`request_model`）。 */
  models: string[];
  /** 实际路由到的模型名（`model`）。 */
  routed_models: string[];
  protocols: string[];
}

export interface RequestLog {
  request_id: string;
  ts: number;
  client: string;
  protocol_in: string;
  protocol_out: string;
  provider_tag?: string | null;
  /** 渠道整数 id，用于关联渠道名称。 */
  provider_id?: number | null;
  /** 渠道当前名称（JOIN 自 providers，改名后自动更新）。 */
  provider_name?: string | null;
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
  /** 所属对话（客户端给的会话 ID）。客户端没给时为 null。 */
  session_id?: string | null;
}

// ---------- 对话链路 ----------

export interface SessionFilter {
  client?: string | null;
  from?: number | null;
  to?: number | null;
  limit?: number;
}

/** 固定对哪些请求生效。`tools`：主线与子代理（默认）；`all`：连旁路一起。 */
export type PinScope = "tools" | "all";

export interface SessionPin {
  session_id: string;
  model: string;
  scope: PinScope;
  created_at: number;
}

export interface SessionSummary {
  session_id: string;
  client: string;
  first_ts: number;
  last_ts: number;
  requests: number;
  errors: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  quota: number;
  /**
   * 用户第一条消息里的第一句话。null = 没取到（升级前就存在的对话，或开场只有一张图），
   * 界面回落到「客户端 #会话ID」。判据见 `traffic/session.rs::first_user_line`。
   */
  title?: string | null;
  /** 用到过的生效模型，按首次出现排序。多个 = 中途换过模型。 */
  models: string[];
  /** 这个对话被固定到的模型。 */
  pin?: SessionPin | null;
}

/** 主线 / 子代理 / 旁路（起标题、hook 求值）。判据见 `traffic/session.rs`。 */
export type ThreadKind = "main" | "agent" | "side";

export interface ThreadSummary {
  thread_key: string;
  kind: ThreadKind;
  requests: number;
  first_ts: number;
  last_ts: number;
  compactions: number;
}

export interface TimelineEntry {
  request_id: string;
  ts: number;
  /** null = 不是对话的一轮（count_tokens 等），不属于任何线。 */
  thread_key: string | null;
  model: string;
  request_model: string;
  path: string;
  status_code: number;
  error_message?: string | null;
  message_count?: number | null;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  quota: number;
  latency_ms: number;
  /** 相对同一条线的上一条，消息数骤降 —— 在这之前上下文被压缩过。 */
  compacted: boolean;
  compacted_from?: number | null;
  /** 被会话固定改写之前的模型名。 */
  pinned_from?: string | null;
}

export interface SessionTimeline {
  session_id: string;
  /** 同 `SessionSummary.title`。 */
  title?: string | null;
  threads: ThreadSummary[];
  entries: TimelineEntry[];
  pin?: SessionPin | null;
}

/** 正在进行的请求（还没结束、尚未落库）。与后端 `RequestStarted` 对应。 */
export interface InflightRequest {
  request_id: string;
  ts: number;
  client: string;
  model: string;
  request_model: string;
  path: string;
  protocol_in: string;
  provider_tag: string;
  provider_name: string;
  upstream_url: string;
  is_stream: boolean;
  session_id?: string | null;
  /** 对话里的哪条线（主线 / 子代理 / 旁路）。对话视图靠它把进行中的那条挂到线上。 */
  thread_key?: string | null;
  /**
   * 这条流**已经推给前端的帧**（后端按每请求帧数 / 字节双上限封顶）。
   *
   * 前端是进监控页时才订阅事件的：一条早就跑起来的流，光有元信息补回来
   * 就是一块空白的实时视图 —— 而"进行中"的请求常正卡在一次长工具调用上。
   */
  frames: StreamFrame[];
  /** 补回来的这批不全（流的帧上限到了，或底账只留了最新的那些）。 */
  truncated?: boolean;
}

export interface Page<T> {
  items: T[];
  total: number;
  /**
   * 表达式筛选没扫完时为真 —— 此时 `total` 只是**扫过的那部分**里的匹配数。
   * 普通查询不会带上它。界面必须如实说出来，否则用户会把一个少了的数字当成全部。
   */
  truncated?: boolean;
}

/** 清空日志的结果：各删了多少条。 */
export interface ClearResult {
  logs: number;
  captures: number;
}

/* ------------------------ 请求 / 响应的语义化视图 ------------------------ */

/**
 * 协议无关的 IR。与后端 `protocol/dto.rs` 的 serde 形状一一对应。
 *
 * 三个协议（含跨协议转换后的报文）解码后都落到这套结构上，所以下面这些组件
 * 只需要写一份渲染逻辑。
 */
export type Role = "system" | "user" | "assistant" | "tool";

export type ContentBlock =
  | { type: "text"; text: string }
  | { type: "image"; media_type: string; data: string }
  | { type: "tool_use"; id: string; name: string; input: unknown }
  | {
      type: "tool_result";
      tool_use_id: string;
      content: ContentBlock[];
      is_error: boolean;
    }
  | { type: "thinking"; text: string; signature?: string | null }
  | { type: "redacted_thinking"; data: string }
  /**
   * 后端没建模的内容块，整个原始 JSON 留在 `raw` 里。
   *
   * 出现的场合：客户端先用上了新块类型（Claude Code 带附件时发的 `document`），
   * 或上游发来的是非标准形状。这一块只要求"能看到原文" —— 它本来就不要求
   * 语义化渲染。
   */
  | { type: "unmodeled"; raw: unknown };

export interface UnifiedMessage {
  role: Role;
  content: ContentBlock[];
}

export interface ToolDef {
  name: string;
  /**
   * Codex 的 Responses Lite 用 `type: "namespace"` 把工具分组，后端展平后把组名留在这里。
   *
   * 只用于展示：出站编码一律只用 `name`，因为 OpenAI / Anthropic 的函数名不接受
   * `functions.exec` 这种带点的写法。
   */
  namespace?: string | null;
  description?: string | null;
  /** 统一成 JSON Schema；OpenAI 与 Anthropic 的表述差异由后端抹平。 */
  input_schema: unknown;
}

export type ToolChoice =
  | { type: "auto" }
  | { type: "none" }
  | { type: "required" }
  | { type: "tool"; name: string };

export type FinishReason =
  | { type: "stop" }
  | { type: "length" }
  | { type: "tool_use" }
  | { type: "content_filter" }
  | { type: "other"; value: string };

export interface UnifiedRequest {
  model: string;
  stream: boolean;
  /**
   * Anthropic 放在顶层、OpenAI 放在 messages 里；IR 统一为顶层。
   *
   * ⚠️ 后端对这几个空的集合用了 `skip_serializing_if`，**空时字段会整个消失**，
   * 不是空数组。所以这里是可选的，用的时候必须 `?? []`。
   */
  system?: ContentBlock[];
  messages: UnifiedMessage[];
  tools?: ToolDef[];
  tool_choice?: ToolChoice | null;
  temperature?: number | null;
  top_p?: number | null;
  max_tokens?: number | null;
  stop?: string[];
  reasoning?: {
    enabled: boolean;
    budget_tokens?: number | null;
    effort?: string | null;
  } | null;
  /** 协议原生但未建模的字段，原样带过来。 */
  [extra: string]: unknown;
}

export interface UnifiedResponse {
  id: string;
  model: string;
  content: ContentBlock[];
  finish_reason: FinishReason;
}

export interface UnifiedUsage {
  /** **不含缓存的** fresh 输入。各协议已折算到这个口径。 */
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
  reasoning_tokens: number;
  total_tokens: number;
  source: "upstream" | "local_estimate";
}

/**
 * 流式增量在**实时流**里的推流形态。与后端 `traffic::stream_events::StreamDelta`
 * 一一对应。
 *
 * 为什么要单独一个类型，而不是直接复用 IR 的 `UnifiedDelta`：后者用内部标签
 * `type`，而 `Finish(FinishReason)` 里包着的枚举**也**用 `type` 当标签，两者会拼出
 * `{"type":"finish","type":"tool_use"}` 这种重复键。`JSON.parse` 只留最后一个，
 * 前端就再也认不出这是 finish。所以推流侧换用 `kind`。
 *
 * 只有监控页的实时流用得到它 —— 请求明细那边看的是重建好的 `UnifiedResponse`。
 */
export type StreamDelta =
  | { kind: "message_start"; id: string; model: string }
  | { kind: "block_start"; index: number; block: ContentBlock }
  | { kind: "text"; index: number; text: string }
  | { kind: "thinking"; index: number; text: string }
  // 思考签名只报 index：签名本身很长，实时视图看原始帧就够了。
  | { kind: "thinking_signature"; index: number }
  | { kind: "tool_input"; index: number; partial_json: string }
  | { kind: "block_stop"; index: number }
  | {
      kind: "usage";
      input_tokens?: number | null;
      output_tokens?: number | null;
      cache_read_tokens?: number | null;
      cache_creation_tokens?: number | null;
      reasoning_tokens?: number | null;
      total_tokens?: number | null;
    }
  | { kind: "finish"; reason: FinishReason }
  | { kind: "error"; code: string; message: string };

export interface DecodedRequest {
  protocol: string;
  value?: UnifiedRequest | null;
  /** 解不出来时的原因，直接展示；界面据此退回原始视图。 */
  error?: string | null;
}

export interface DecodedResponse {
  protocol: string;
  value?: UnifiedResponse | null;
  usage?: UnifiedUsage | null;
  error?: string | null;
}

/**
 * 一次请求在四个方向上的语义化视图。
 *
 * `value` 为 null 表示该方向没有捕获或解不出来（看 `error`）；
 * 整个字段为 null 表示压根没有这一侧的报文。
 */
export interface DetailViews {
  /** 客户端 → Apilot */
  inbound_request?: DecodedRequest | null;
  /** Apilot → 上游（转换过的报文在这一份里才是真实形态） */
  upstream_request?: DecodedRequest | null;
  /** 上游 → Apilot */
  upstream_response?: DecodedResponse | null;
  /** Apilot → 客户端 */
  client_response?: DecodedResponse | null;
  /** 流式响应：由增量重建，是流式请求唯一能看到思考与工具调用的地方 */
  streamed_response?: DecodedResponse | null;
}

export interface RequestDetail extends RequestLog {
  /**
   * 平均 token 间隔（毫秒）与输出速度（token / 秒），由后端从已落库的标量推出。
   *
   * 间隔只在流式且量得出解码窗口时才有；非流式测不出间隔，缓存命中不报速度。
   * 为 null 时界面显示「—」。
   */
  itl_ms?: number | null;
  tps?: number | null;

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
  /** 流式响应的结构化内容（IR 的 JSON 文本）。 */
  response_content?: string | null;
  /** 上游原始 SSE 帧。 */
  upstream_stream_raw?: string | null;
  /** 重编码后发给客户端的原始 SSE 帧；直通时为 null（与上游那份相同）。 */
  client_stream_raw?: string | null;
  /** 原始 SSE 帧是否因超过上限被截断。 */
  stream_raw_truncated: boolean;
  /**
   * 每个 SSE 事件的时间点（`StreamTimings` 的 JSON 文本）。
   *
   * 老日志没有这一项 —— 改动之前没记。用 `parseTimings` 解，解不出来就别画时间轴。
   */
  stream_timings?: string | null;

  views: DetailViews;
}

/* --------------------------- 请求回放 / A-B 对比 --------------------------- */

/**
 * 一次回放的结果。**不计费、不写日志** —— 这是用户主动做的对比实验，
 * 不应污染真实用量。
 */
export interface ReplayOutcome {
  ok: boolean;
  request_id: string;
  provider_tag: string;
  provider_name: string;
  /** 入站模型名（客户端视角）。 */
  model: string;
  /** 渠道映射后真正发出去的名字。 */
  upstream_model: string;
  wire: Protocol;
  url: string;
  status?: number | null;
  latency_ms: number;
  text: string;
  /** 解出来的完整内容块（含思考 / 工具调用）。 */
  content: ContentBlock[];
  usage?: UnifiedUsage | null;
  error?: string | null;
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
  /** 日志目录：`~/.apilot/logs`。 */
  log_dir: string;
  started_at: number;
}

export interface ProviderModel {
  model: string;
  upstream_model?: string | null;
}

/* --------------------------- 上游模型自动发现 --------------------------- */

/** 本轮补进某个渠道声明的模型。 */
export interface AddedModels {
  provider_name: string;
  models: string[];
}

/** 拉不到模型清单的渠道。 */
export interface ModelFetchFailure {
  provider_name: string;
  error: string;
}

export interface DiscoveredModels {
  /** 新写进声明的模型（只有这些会改变路由行为）。 */
  added: AddedModels[];
  /**
   * 上游此刻实际返回的模型全集。包含「通吃」渠道（没有任何声明的渠道）——
   * 那些名字不会被写进库，但界面照样要能列出来：选中之后确实能走它们。
   */
  available: string[];
  failures: ModelFetchFailure[];
}

/* --------------------------- 模型能力探测 --------------------------- */

/** Apilot 关心的能力维度。与后端 `storage::capabilities::Capability` 一一对应。 */
export type Capability = "reasoning" | "tools" | "vision";

/**
 * 判定结果。**是三态，不是布尔。**
 *
 * 探测「支不支持工具」时，模型完全可能只是那一次没调工具 —— 记成「不支持」
 * 是撒谎，用户会据此把一条本来能用的渠道判死刑。所以「未知」必须是个能显示
 * 出来的独立状态，不能压成 false。
 */
export type CapabilityVerdict = "supported" | "unsupported" | "inconclusive";

/** 能力来源：`probe` 是实测（反映这条渠道的真实行为），`catalog` 是目录断言。 */
export type CapabilitySource = "probe" | "catalog";

export const CAPABILITY_LABEL: Record<Capability, string> = {
  reasoning: "思考",
  tools: "工具",
  vision: "多模态",
};

export const CAPABILITY_VERDICT_LABEL: Record<CapabilityVerdict, string> = {
  supported: "支持",
  unsupported: "不支持",
  inconclusive: "未知",
};

export const ALL_CAPABILITIES: Capability[] = ["reasoning", "tools", "vision"];

export interface CapabilityRecord {
  provider_id: number;
  model: string;
  capability: Capability;
  verdict: CapabilityVerdict;
  source: CapabilitySource;
  /** 判定依据：具体观察到了什么。排查「凭什么叫它不支持」时看这个。 */
  evidence?: string | null;
  checked_at: number;
}

/** 一次真实模型测试的结果。 */
export interface OneshotOutcome {
  ok: boolean;
  status?: number | null;
  latency_ms: number;
  wire: Protocol;
  /** 实际打到的出站地址，排查「怎么发到那儿去了」时要有。 */
  url: string;
  /** 上游原始回包的截断预览。 */
  preview: string;
  /** 解出来的回答文本。 */
  text: string;
  usage?: UnifiedUsage | null;
  error?: string | null;
}

/* --------------------------- 上游模型目录 --------------------------- */

/**
 * 目录来源。两家的数据形状差别很大，不是互为备份：
 * models.dev 覆盖更全更新（kimi / glm / qwen3-max 只有它有），
 * LiteLLM 模型更多但偏一手大厂。枚举值与后端 serde 的 snake_case 一致。
 */
export type CatalogSource = "models_dev" | "lite_llm";

export const CATALOG_SOURCE_LABEL: Record<CatalogSource, string> = {
  models_dev: "models.dev",
  lite_llm: "LiteLLM",
};

/** 目录导入时对某一行的处置。 */
export type PriceAction = "insert" | "update" | "keep_user_owned" | "unchanged";

export const PRICE_ACTION_LABEL: Record<PriceAction, string> = {
  insert: "新增",
  update: "覆盖",
  keep_user_owned: "跳过（手填）",
  unchanged: "无变化",
};

export interface PriceDiffRow {
  model: string;
  action: PriceAction;
  /** 库里的现值。新增时为 null。 */
  current?: Pricing | null;
  incoming: Pricing;
}

export interface PriceImportStats {
  inserted: number;
  updated: number;
  kept_user_owned: number;
  unchanged: number;
}

export interface CatalogPreview {
  source: CatalogSource;
  source_label: string;
  /** 目录里的条目总数。 */
  total: number;
  /** 其中带价格、且**本项目声明过**的模型数 —— 真正会写库的那批。 */
  priced: number;
  rows: PriceDiffRow[];
  stats: PriceImportStats;
}

export interface CapabilityImportStats {
  providers: number;
  written: number;
  /** 目录里找不到的模型数（含自建、以及目录还没收录的新模型）。 */
  missing: number;
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

/* ================================================================== */
/* 指纹检测与两级样本库                                                */
/* ================================================================== */

export type FingerprintLibrarySource = "embedded" | "remote";

export interface FingerprintModelInfo {
  id: string;
  display_name: string;
  family: string;
  family_name: string;
  response_count: number;
  valid_number_count: number;
  sources: Record<string, number>;
  conditions: Record<string, number>;
}

export interface FingerprintLibraryInfo {
  source: FingerprintLibrarySource;
  built_at: string;
  model_count: number;
  sha256: string;
  source_url?: string | null;
  source_commit?: string | null;
  models: FingerprintModelInfo[];
  challenges: number;
  error?: string | null;
}

export interface FingerprintDiagnostic {
  index: number;
  parsed_numbers: number;
  minimum_numbers: number;
  accepted: boolean;
  raw_out_of_range_count: number;
}

export interface FingerprintSequence {
  autocorrelation: number;
  small_step_share: number;
  model_like: boolean;
}

export interface FingerprintCandidate {
  model: string;
  display_name: string;
  family: string;
  family_name: string;
  score: number;
  probability?: number | null;
}

export interface FingerprintDeclaredRank {
  model: string;
  rank: number;
  score: number;
  probability?: number | null;
  matched_as: string;
}

export interface FingerprintEvidence {
  insufficient: boolean;
  label: string;
  reason: string;
  threshold?: number | null;
  method: string;
}

export interface FingerprintOfficialResult {
  decision: string;
  used_outputs: number;
  calibrated: boolean;
  probability_status: string;
  diagnostics: FingerprintDiagnostic[];
  sequence?: FingerprintSequence | null;
  candidates: FingerprintCandidate[];
  declared?: FingerprintDeclaredRank | null;
  evidence: FingerprintEvidence;
}

export interface FingerprintAttempt {
  index: number;
  challenge_id: string;
  /** 实际发送给模型的提示词。 */
  challenge_prompt?: string;
  expected_count: number;
  ok: boolean;
  status?: number | null;
  latency_ms: number;
  wire: string;
  url: string;
  error?: string | null;
  parsed_numbers: number;
  minimum_numbers: number;
  accepted: boolean;
  text_preview: string;
  /** 原始回答全文；老记录可能没有。 */
  text?: string;
  /** 解析后的整数；前端不展示，加入样本库由后端按检测 id 回查。 */
  numbers?: number[];
}

export interface FingerprintLocalCandidate {
  label: string;
  samples: number;
  distance_ratio: number;
  score: number;
  probability?: number | null;
  official: boolean;
}

export interface FingerprintLocalResult {
  candidates: FingerprintLocalCandidate[];
  calibrated: boolean;
  eligible_labels: number;
  total_labels: number;
}

export interface FingerprintConclusion {
  model?: string | null;
  display_name?: string | null;
  basis: string;
  level: string;
  note: string;
}

export interface FingerprintDetection {
  /** 落库后的检测记录 id；加入本地样本库时用。 */
  id: number;
  declared_model: string;
  upstream_model: string;
  wire: string;
  status: string;
  library: {
    source: FingerprintLibrarySource;
    built_at: string;
    model_count: number;
    sha256: string;
  };
  attempts: FingerprintAttempt[];
  official?: FingerprintOfficialResult | null;
  local?: FingerprintLocalResult | null;
  conclusion: FingerprintConclusion;
  elapsed_ms: number;
}

export interface FingerprintLocalLabel {
  label: string;
  samples: number;
  last_added_at: number;
  official: boolean;
}

export interface FingerprintDetectionSummary {
  id: number;
  provider_id?: number | null;
  provider_tag: string;
  declared_model: string;
  upstream_model: string;
  wire: string;
  library_source: string;
  library_built_at: string;
  status: string;
  created_at: number;
  official_top?: string | null;
  official_probability?: number | null;
  local_top?: string | null;
  local_probability?: number | null;
  conclusion: FingerprintConclusion;
}

export interface FingerprintChallenge {
  challenge_id: string;
  condition: string;
  expected_count: number;
  prompt: string;
  system: string;
}

export interface FingerprintLocalSample {
  id: number;
  label: string;
  challenge_id: string;
  expected_count: number;
  numbers_count: number;
  provider_tag: string;
  declared_model: string;
  created_at: number;
  note?: string | null;
}

export const FINGERPRINT_BASIS_LABEL: Record<string, string> = {
  official: "官方参考库",
  agreement: "官方与本地一致",
  conflict: "存在分歧",
  local_only: "仅本地样本",
  insufficient: "证据不足",
};

export const FINGERPRINT_LEVEL_LABEL: Record<string, string> = {
  high: "高",
  medium: "中",
  low: "低",
};

export const FINGERPRINT_STATUS_LABEL: Record<string, string> = {
  ok: "完成",
  partial: "部分有效",
  unscorable: "无法评分",
  not_model: "不像语言模型回答",
};

export const FINGERPRINT_REMOTE_BASE =
  "https://raw.githubusercontent.com/Ikaleio/lm-detector/main/data";

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
  /** 全量替换该渠道声明支持的模型名。上游重定向名走 `setModelCandidates`。 */
  setProviderModels: (providerId: number, models: string[]) =>
    call<null>("set_provider_models", { providerId, models }),
  /** 拉取上游 `GET {base_url}/v1/models`，返回模型 id 列表。 */
  fetchProviderModels: (id: number) =>
    call<string[]>("fetch_provider_models", { id }),
  /**
   * 自动发现各启用渠道此刻到底有哪些模型。
   *
   * 会给「已经声明过模型」的渠道补上新出现的名字（否则选中也用不了：非通吃渠道
   * 只认声明名）；没有任何声明的「通吃」渠道保持原样。只增不删。
   */
  discoverUpstreamModels: () =>
    call<DiscoveredModels>("discover_upstream_models"),
  /**
   * 就地启用 / 停用。只改这一个字段，**不要**用 `upsertProvider` 代替：
   * 那条路要求把密钥等整套回传，而密钥不回显，改个开关就会把它抹掉。
   */
  setProviderEnabled: (id: number, enabled: boolean) =>
    call<null>("set_provider_enabled", { id, enabled }),
  /**
   * 自动检测该地址支持哪些协议：对三种协议的入口各发一次**故意不合法**的请求
   * （空对象），按回包判定 —— 不消耗 token，也不占配额。结果不落库。
   */
  detectProviderProtocols: (input: ProtocolDetectInput) =>
    call<ProtocolDetection[]>("detect_provider_protocols", { input }),
  /**
   * 按渠道的元数据来源立刻同步一次价格。来源为 `off` 时返回全零统计。
   *
   * 保存渠道时后端已经自动同步；这个命令用于目录更新后手动补一次。
   */
  syncProviderMetadata: (id: number) =>
    call<PriceImportStats>("sync_provider_metadata", { id }),

  /* ---- 上游目录：价格与能力共用同一个网络集成 ---- */
  /**
   * 预览「从目录更新价格会改动什么」，不写库。
   * `refresh` 为真则忽略缓存重新下载（目录本身按天更新，平时不必）。
   */
  catalogPricePreview: (source: CatalogSource, refresh = false) =>
    call<CatalogPreview>("catalog_price_preview", { source, refresh }),
  /** 应用价格更新。后端会**重新算一遍差异**，不信前端传回来的行。 */
  catalogPriceApply: (source: CatalogSource) =>
    call<PriceImportStats>("catalog_price_apply", { source }),
  /** 从目录导入能力标志。`providerId` 省略则处理所有启用的渠道。 */
  catalogCapabilitiesImport: (source: CatalogSource, providerId?: number) =>
    call<CapabilityImportStats>("catalog_capabilities_import", {
      source,
      providerId: providerId ?? null,
    }),
  listCapabilities: (providerId: number) =>
    call<CapabilityRecord[]>("list_capabilities", { providerId }),
  /** 探测一项能力。一次只测一项 —— 每项都要花一次请求的 token。 */
  probeCapability: (providerId: number, model: string, capability: Capability) =>
    call<CapabilityRecord>("probe_capability", { providerId, model, capability }),
  /** 单次模型测试：往这个渠道真发一条最短的对话请求。 */
  testModel: (providerId: number, model: string) =>
    call<OneshotOutcome>("test_model", { providerId, model }),

  /* ---- 指纹检测与两级样本库 ---- */
  /**
   * 检测这个渠道背后的实际模型：串行发 3 条挑战，返回官方排名、本地样本
   * 证据和一致性结论。**会消耗 3 次真实请求的额度**。
   */
  detectModelFingerprint: (
    providerId: number,
    model: string,
    parallel = true,
    challengeIds?: string[] | null,
  ) =>
    call<FingerprintDetection>("detect_model_fingerprint", {
      providerId,
      model,
      parallel,
      challengeIds: challengeIds ?? null,
    }),
  fingerprintLibraryInfo: () =>
    call<FingerprintLibraryInfo>("fingerprint_library_info"),
  /** 36 条固定挑战的全文，供手动选择提示词。 */
  listFingerprintChallenges: () =>
    call<FingerprintChallenge[]>("list_fingerprint_challenges"),
  listFingerprintDetections: (limit = 50) =>
    call<FingerprintDetectionSummary[]>("list_fingerprint_detections", { limit }),
  /** 读取一次历史检测的完整结果；返回的 id 可用于加入样本库。 */
  getFingerprintDetection: (detectionId: number) =>
    call<FingerprintDetection>("get_fingerprint_detection", { detectionId }),
  /** 只重试某一条挑战，保留其余两条并重新计算排名。 */
  retryFingerprintAttempt: (detectionId: number, attemptIndex: number) =>
    call<FingerprintDetection>("retry_fingerprint_attempt", {
      detectionId,
      attemptIndex,
    }),
  deleteFingerprintDetection: (detectionId: number) =>
    call<null>("delete_fingerprint_detection", { detectionId }),
  /** 从 GitHub（或镜像）拉检测器和指纹库；base_url 省略用默认地址。 */
  updateFingerprintLibrary: (baseUrl?: string | null) =>
    call<FingerprintLibraryInfo>("update_fingerprint_library", {
      baseUrl: baseUrl ?? null,
    }),
  /** 停用远端库，回到内置快照。 */
  resetFingerprintLibrary: () =>
    call<FingerprintLibraryInfo>("reset_fingerprint_library"),
  listFingerprintLabels: () =>
    call<FingerprintLocalLabel[]>("list_fingerprint_labels"),
  listFingerprintSamples: (label?: string | null) =>
    call<FingerprintLocalSample[]>("list_fingerprint_samples", {
      label: label ?? null,
    }),
  /** 把一次检测里的有效回答加入本地样本库；数字由后端从落库结果里取。 */
  addFingerprintSamples: (
    detectionId: number,
    label: string,
    sampleIndexes: number[],
    note?: string | null,
  ) =>
    call<number>("add_fingerprint_samples", {
      detectionId,
      label,
      sampleIndexes,
      note: note ?? null,
    }),
  deleteFingerprintSample: (id: number) =>
    call<null>("delete_fingerprint_sample", { id }),
  deleteFingerprintLabel: (label: string) =>
    call<number>("delete_fingerprint_label", { label }),

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
  /** 用系统默认程序打开该客户端的配置文件。 */
  openClientConfig: (client: string) =>
    call<void>("open_client_config", { client }),

  /* ---- logs ---- */
  queryLogs: (filter: LogFilter) => call<Page<RequestLog>>("query_logs", { filter }),
  /** 筛选下拉的候选值。进监控页时取一次即可。 */
  listLogFacets: () => call<LogFacets>("list_log_facets"),
  /** 最近活跃的对话。 */
  listSessions: (filter: SessionFilter) =>
    call<SessionSummary[]>("list_sessions", { filter: { limit: 100, ...filter } }),
  /** 一个对话的完整链路。 */
  getSessionTimeline: (sessionId: string) =>
    call<SessionTimeline>("get_session_timeline", { sessionId }),
  /** 把对话固定到某个模型（已固定过就改成新的）。 */
  pinSessionModel: (sessionId: string, model: string, scope: PinScope) =>
    call<void>("pin_session_model", { sessionId, model, scope }),
  unpinSessionModel: (sessionId: string) =>
    call<void>("unpin_session_model", { sessionId }),
  /**
   * 校验筛选表达式能否编译，返回错因（`null` = 通过）。
   *
   * 刻意**不走 `call`**：那个失败会弹 toast，而这里每次按键都要调一次 ——
   * 用户还在打字时不停弹「语法错误」既吵又没意义。错因由编辑框自己显示。
   */
  validateLogExpr: async (expr: string): Promise<string | null> => {
    try {
      await invoke<void>("validate_log_expr", { expr });
      return null;
    } catch (err) {
      return normalizeError(err).message;
    }
  },
  getRequestDetail: (requestId: string) =>
    call<RequestDetail>("get_request_detail", { requestId }),
  /** 查询当前正在进行的请求。进监控页时调用一次，补齐用户进页前已开始的请求。 */
  getInflightRequests: () => call<InflightRequest[]>("get_inflight_requests"),
  /** 清空请求明细与原文捕获。计费聚合不受影响。 */
  clearLogs: () => call<ClearResult>("clear_logs"),
  /** 删除单条请求明细及其原文捕获。计费聚合不受影响。 */
  deleteLog: (requestId: string) =>
    call<ClearResult>("delete_log", { requestId }),
  /**
   * 把一条捕获过的请求重发到指定渠道 / 模型，供 A/B 对比。
   *
   * **不写日志、不计费、不查缓存**；一律按非流式发，才能拿到完整回答并排比较。
   * 这是一次真实出站请求 —— 请求内容会发送到所选渠道。
   */
  replayRequest: (
    requestId: string,
    providerTag?: string | null,
    model?: string | null,
  ) =>
    call<ReplayOutcome>("replay_request", {
      requestId,
      providerTag: providerTag ?? null,
      model: model ?? null,
    }),

  /* -------------------------- 模型（模型视角） -------------------------- */

  getModelPolicy: () => call<ModelPolicy>("get_model_policy"),
  setModelPolicy: (policy: ModelPolicy) =>
    call<ModelPolicy>("set_model_policy", { policy }),
  /** 校验一段模型脚本；null 表示可用，否则是给用户看的错因。 */
  validateModelScript: (source: string) =>
    call<string | null>("validate_model_script", { source }),
  /** 校验一段请求 / 响应改写脚本；null 表示可用。 */
  validateRewriteScript: (source: string) =>
    call<string | null>("validate_rewrite_script", { source }),

  listModelCatalog: () => call<ModelCatalogEntry[]>("list_model_catalog"),
  listModelOptions: () => call<ModelOption[]>("list_model_options"),

  upsertModelPolicy: (input: ModelPolicyRecord) =>
    call<ModelPolicyRecord>("upsert_model_policy", { input }),
  /** 交回路由规则与选择器管。 */
  resetModelPolicy: (model: string) => call<null>("reset_model_policy", { model }),
  switchModelChannel: (model: string, providerTag: string) =>
    call<ModelPolicyRecord>("switch_model_channel", { model, providerTag }),

  setModelCandidates: (model: string, candidates: ModelCandidateInput[]) =>
    call<null>("set_model_candidates", { model, candidates }),
  /** 对候选渠道跑一次测速，结果缓存起来供「按延迟」策略用。 */
  probeModelCandidates: (model: string) =>
    call<ProbeResult[]>("probe_model_candidates", { model }),
};

export type Api = typeof api;
