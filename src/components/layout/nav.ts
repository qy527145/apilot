import type { LucideIcon } from "lucide-react";
import {
  Activity,
  BarChart3,
  Database,
  Fingerprint,
  GitBranch,
  LayoutDashboard,
  MonitorSmartphone,
  Server,
  Settings,
  Wrench,
} from "lucide-react";

export type ViewKey =
  | "overview"
  | "clients"
  | "providers"
  | "modelrouting"
  | "samples"
  | "traffic"
  | "tools"
  | "billing"
  | "cache"
  | "settings";

export interface NavItem {
  key: ViewKey;
  label: string;
  icon: LucideIcon;
  description: string;
}

export const NAV_ITEMS: NavItem[] = [
  {
    key: "overview",
    label: "概览",
    icon: LayoutDashboard,
    description: "网关状态与关键指标一览",
  },
  {
    key: "clients",
    label: "客户端接管",
    icon: MonitorSmartphone,
    description: "接管本机 CLI 客户端的 API 配置",
  },
  {
    key: "providers",
    label: "渠道管理",
    icon: Server,
    description: "上游 LLM 渠道与模型映射",
  },
  {
    key: "modelrouting",
    label: "模型路由",
    icon: GitBranch,
    description: "决定一条请求落到哪个渠道的哪个模型",
  },
  {
    key: "samples",
    label: "指纹样本",
    icon: Fingerprint,
    description: "远端参考库与本地模型样本",
  },
  {
    key: "traffic",
    label: "监控",
    icon: Activity,
    description: "实时请求流与明细",
  },
  {
    key: "tools",
    label: "工具分布",
    icon: Wrench,
    description: "各 harness 调用了哪些工具",
  },
  {
    key: "billing",
    label: "统计",
    icon: BarChart3,
    description: "用量聚合与单价系数",
  },
  {
    key: "cache",
    label: "缓存",
    icon: Database,
    description: "响应缓存命中与策略",
  },
  {
    key: "settings",
    label: "设置",
    icon: Settings,
    description: "监听、超时与捕获配置",
  },
];

export const VIEW_STORAGE_KEY = "apilot.currentView";

/**
 * 已保存的视图键是否还有效。
 *
 * 「模型」与「路由」曾是两个页面，现在合并成 `modelrouting`。老存档里存着
 * `models` / `routing` 时映射到新键，而不是让用户一进来就落回概览页。
 */
const LEGACY_VIEWS: Record<string, ViewKey> = {
  models: "modelrouting",
  routing: "modelrouting",
};

export function normalizeView(v: string | null): ViewKey | null {
  if (!v) return null;
  if (v in LEGACY_VIEWS) return LEGACY_VIEWS[v];
  return NAV_ITEMS.some((n) => n.key === v) ? (v as ViewKey) : null;
}
