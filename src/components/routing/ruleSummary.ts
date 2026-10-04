import type { Protocol, RouteAction, RuleItem } from "@/lib/api";

export const ITEM_TYPE_LABELS: Record<RuleItem["type"], string> = {
  client: "客户端",
  model: "模型",
  protocol: "协议",
  path: "路径",
  header: "请求头",
  token_estimate: "Token 估计",
  logical: "逻辑组合",
};

export const ACTION_TYPE_LABELS: Record<RouteAction["type"], string> = {
  final: "终结 · 选定选择器",
  reject: "终结 · 拒绝请求",
  model_override: "改写模型名",
  route_options: "路由选项",
  sniff: "仅采集特征",
};

export const PROTOCOLS: Protocol[] = [
  "anthropic",
  "openai_chat",
  "openai_responses",
];

export function emptyItem(type: RuleItem["type"]): RuleItem {
  switch (type) {
    case "client":
      return { type: "client", any: [] };
    case "model":
      return { type: "model", patterns: [] };
    case "protocol":
      return { type: "protocol", any: [] };
    case "path":
      return { type: "path", prefixes: [] };
    case "header":
      return { type: "header", name: "", equals: null };
    case "token_estimate":
      return { type: "token_estimate", min: null, max: null };
    case "logical":
      return { type: "logical", mode: "and", invert: false, rules: [] };
  }
}

export function summarizeItem(item: RuleItem): string {
  switch (item.type) {
    case "client":
      return `客户端 ∈ [${item.any.join(", ") || "空"}]`;
    case "model":
      return `模型 glob [${item.patterns.join(", ") || "空"}]`;
    case "protocol":
      return `协议 ∈ [${item.any.join(", ") || "空"}]`;
    case "path":
      return `路径前缀 [${item.prefixes.join(", ") || "空"}]`;
    case "header":
      return item.equals
        ? `Header ${item.name || "?"} = ${item.equals}`
        : `存在 Header ${item.name || "?"}`;
    case "token_estimate": {
      const lo = item.min ?? "-∞";
      const hi = item.max ?? "+∞";
      return `Token 估计 ∈ [${lo}, ${hi}]`;
    }
    case "logical": {
      const op = item.mode === "and" ? " 且 " : " 或 ";
      const inner = item.rules.map(summarizeItem).join(op) || "空";
      return item.invert ? `非(${inner})` : `(${inner})`;
    }
  }
}

export function summarizeAction(action: RouteAction): string {
  switch (action.type) {
    case "final":
      return `终结 → 选择器 ${action.selector || "?"}`;
    case "reject":
      return `拒绝：${action.reason || "未填原因"}`;
    case "model_override":
      return `改写模型 → ${action.model || "?"}`;
    case "route_options": {
      const parts: string[] = [];
      if (action.target_selector) parts.push(`目标选择器 ${action.target_selector}`);
      if (action.cache !== null && action.cache !== undefined)
        parts.push(`缓存 ${action.cache ? "开" : "关"}`);
      return parts.length ? `路由选项：${parts.join("，")}` : "路由选项";
    }
    case "sniff":
      return "仅采集特征";
  }
}

export function emptyAction(type: RouteAction["type"]): RouteAction {
  switch (type) {
    case "final":
      return { type: "final", selector: "" };
    case "reject":
      return { type: "reject", reason: "" };
    case "model_override":
      return { type: "model_override", model: "" };
    case "route_options":
      return { type: "route_options", target_selector: null, cache: null };
    case "sniff":
      return { type: "sniff" };
  }
}
