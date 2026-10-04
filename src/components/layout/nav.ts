import type { LucideIcon } from "lucide-react";
import {
  Activity,
  BarChart3,
  Database,
  GitBranch,
  LayoutDashboard,
  MonitorSmartphone,
  Server,
  Settings,
} from "lucide-react";

export type ViewKey =
  | "overview"
  | "clients"
  | "providers"
  | "routing"
  | "traffic"
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
    key: "routing",
    label: "路由",
    icon: GitBranch,
    description: "选择器热切换与路由规则链",
  },
  {
    key: "traffic",
    label: "监控",
    icon: Activity,
    description: "实时请求流与明细",
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

export function isViewKey(v: string | null): v is ViewKey {
  return !!v && NAV_ITEMS.some((n) => n.key === v);
}
