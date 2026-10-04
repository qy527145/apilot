import type { AuthStyle, ProviderKind } from "@/lib/api";

/** 新建渠道时可一键套用的常用服务商配置。 */
export interface ProviderPreset {
  /** 唯一键，仅用于 React key。 */
  id: string;
  /** 按钮上的显示名。 */
  label: string;
  /** 填充进表单的值。 */
  name: string;
  tag: string;
  kind: ProviderKind;
  base_url: string;
  auth_style: AuthStyle;
}

/**
 * base_url 一律填成能被 `Provider::endpoint` 正确拼上 `/v1/...` 的形式：
 * 带 `/v1`（OpenAI、Moonshot…）或裸域名（DeepSeek、Anthropic）都可以 ——
 * endpoint() 会去重。但像 Gemini 的 `/v1beta/openai`、智谱的 `/api/paas/v4`
 * 这种非 v1 前缀的路径它拼不对，所以不在这里提供，避免给出"看起来能用实则 404"的预设。
 */
export const PROVIDER_PRESETS: ProviderPreset[] = [
  {
    id: "anthropic",
    label: "Anthropic",
    name: "Anthropic 官方",
    tag: "anthropic",
    kind: "anthropic",
    base_url: "https://api.anthropic.com",
    auth_style: "x-api-key",
  },
  {
    id: "openai",
    label: "OpenAI",
    name: "OpenAI 官方",
    tag: "openai",
    kind: "openai_chat",
    base_url: "https://api.openai.com/v1",
    auth_style: "bearer",
  },
  {
    id: "deepseek",
    label: "DeepSeek",
    name: "DeepSeek",
    tag: "deepseek",
    kind: "openai_chat",
    base_url: "https://api.deepseek.com",
    auth_style: "bearer",
  },
  {
    id: "moonshot",
    label: "Moonshot",
    name: "Moonshot (Kimi)",
    tag: "moonshot",
    kind: "openai_chat",
    base_url: "https://api.moonshot.cn/v1",
    auth_style: "bearer",
  },
  {
    id: "dashscope",
    label: "通义千问",
    name: "通义千问 (DashScope)",
    tag: "dashscope",
    kind: "openai_chat",
    base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
    auth_style: "bearer",
  },
  {
    id: "siliconflow",
    label: "硅基流动",
    name: "硅基流动 SiliconFlow",
    tag: "siliconflow",
    kind: "openai_chat",
    base_url: "https://api.siliconflow.cn/v1",
    auth_style: "bearer",
  },
  {
    id: "openrouter",
    label: "OpenRouter",
    name: "OpenRouter",
    tag: "openrouter",
    kind: "openai_chat",
    base_url: "https://openrouter.ai/api/v1",
    auth_style: "bearer",
  },
  {
    id: "ollama",
    label: "Ollama 本地",
    name: "Ollama 本地",
    tag: "ollama",
    kind: "openai_chat",
    base_url: "http://127.0.0.1:11434/v1",
    auth_style: "none",
  },
  {
    id: "lmstudio",
    label: "LM Studio 本地",
    name: "LM Studio 本地",
    tag: "lmstudio",
    kind: "openai_chat",
    base_url: "http://127.0.0.1:1234/v1",
    auth_style: "none",
  },
];
