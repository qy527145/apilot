import type { AuthStyle, ProtocolEndpoint, ProviderKind } from "@/lib/api";

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
  /**
   * 该服务商原生支持的协议。
   *
   * **只写核实过的**。这里多写一条，用户直接对上游发那种协议就可能打到不存在的
   * 路径上 404 —— 而这类 404 的真实原因（路径拼错）从日志里很难一眼看出，
   * 正是要避免的情况。不确定的就不要写，让 Apilot 走协议转换，功能不受影响。
   *
   * 省略 `path` 表示用该协议的默认路径；非默认的（主流服务商的 Anthropic 兼容
   * 入口几乎都是 `/anthropic/...` 这类子路径）必须写全。
   */
  protocols: ProtocolEndpoint[];
}

/**
 * base_url 与 protocols 配合着看：
 *
 * - `path` 省略时走协议默认路径（`/v1/messages` 等），由 `Provider::endpoint`
 *   拼接，会自动处理 base_url 自带 `/v1` 的重复。
 * - `path` 给了就**原样**拼在 base_url 后面，不补 `/v1`。所以服务商把兼容入口
 *   挂在别的前缀下时（DeepSeek 的 `/anthropic`、百炼的 `/apps/anthropic`），
 *   要么把它写进 path，要么把 base_url 收到公共前缀上。
 *
 * 早期版本强求 base_url 必须能被 `endpoint()` 拼上 `/v1/...`，因此不敢收录
 * Gemini、智谱这类非 v1 前缀的服务商。有了按协议覆盖路径的能力，这条限制没有了
 * —— 但每个预设的路径仍然要核实过再写，见 `protocols` 的说明。
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
    // 只声明原生协议。Anthropic 另有一个 OpenAI 兼容层，但官方标注为 beta、
    // 定位是"测试对比模型能力"而非生产接入，且不支持 prompt caching ——
    // 默认替你打开等于悄悄降级，所以不内置；需要的话用户自己勾。
    protocols: [{ protocol: "anthropic" }],
  },
  {
    id: "openai",
    label: "OpenAI",
    name: "OpenAI 官方",
    tag: "openai",
    kind: "openai_chat",
    base_url: "https://api.openai.com/v1",
    auth_style: "bearer",
    protocols: [{ protocol: "openai_chat" }, { protocol: "openai_responses" }],
  },
  {
    id: "deepseek",
    label: "DeepSeek",
    name: "DeepSeek",
    tag: "deepseek",
    kind: "openai_chat",
    base_url: "https://api.deepseek.com",
    auth_style: "bearer",
    // 三种都原生支持：
    //   Chat      https://api.deepseek.com/v1/chat/completions
    //   Responses https://api.deepseek.com/v1/responses（V4-Flash 起支持，为 Codex 加的）
    //   Anthropic https://api.deepseek.com/anthropic/v1/messages
    // 注意 Anthropic 那条挂在 /anthropic 子路径下 —— 用默认路径拼成
    // /v1/messages 就是 404。这是最常见的配置错误。
    protocols: [
      { protocol: "openai_chat" },
      { protocol: "openai_responses" },
      { protocol: "anthropic", path: "/anthropic/v1/messages" },
    ],
  },
  {
    id: "moonshot",
    label: "Moonshot",
    name: "Moonshot (Kimi)",
    tag: "moonshot",
    kind: "openai_chat",
    base_url: "https://api.moonshot.cn",
    auth_style: "bearer",
    // base_url 收在裸域名上，两种协议的入口各走各的完整路径：
    //   Chat      /v1/chat/completions
    //   Anthropic /anthropic/v1/messages（官方文档给的 base_url 是
    //             https://api.moonshot.cn/anthropic，拼上 /v1/messages）
    // 若 base_url 留成 .../v1，那条 /anthropic 覆盖路径会拼成
    // .../v1/anthropic/v1/messages —— 多一层，必然 404。
    // Responses 端点未在官方文档中见到，不写。
    protocols: [
      { protocol: "openai_chat", path: "/v1/chat/completions" },
      { protocol: "anthropic", path: "/anthropic/v1/messages" },
    ],
  },
  {
    id: "dashscope",
    label: "通义千问",
    name: "通义千问 (百炼)",
    tag: "dashscope",
    kind: "openai_chat",
    base_url: "https://dashscope.aliyuncs.com",
    auth_style: "bearer",
    // base_url 收在公共前缀上，两种协议的入口各走各的路径：
    //   Chat      /compatible-mode/v1/chat/completions
    //   Anthropic /apps/anthropic/v1/messages（旧的 claude-code-proxy 只支持
    //             qwen3-coder-plus，官方已建议迁移，不用）
    protocols: [
      { protocol: "openai_chat", path: "/compatible-mode/v1/chat/completions" },
      { protocol: "anthropic", path: "/apps/anthropic/v1/messages" },
    ],
  },
  {
    id: "siliconflow",
    label: "硅基流动",
    name: "硅基流动 SiliconFlow",
    tag: "siliconflow",
    kind: "openai_chat",
    base_url: "https://api.siliconflow.cn/v1",
    auth_style: "bearer",
    // 只核实到 OpenAI 兼容端点，其余的交给 Apilot 转换。
    protocols: [{ protocol: "openai_chat" }],
  },
  {
    id: "openrouter",
    label: "OpenRouter",
    name: "OpenRouter",
    tag: "openrouter",
    kind: "openai_chat",
    base_url: "https://openrouter.ai/api/v1",
    auth_style: "bearer",
    // 三种请求形态都支持，且都在 /v1 下，路径与默认值一致。
    protocols: [
      { protocol: "openai_chat" },
      { protocol: "openai_responses" },
      { protocol: "anthropic" },
    ],
  },
  {
    id: "ollama",
    label: "Ollama 本地",
    name: "Ollama 本地",
    tag: "ollama",
    kind: "openai_chat",
    base_url: "http://127.0.0.1:11434/v1",
    auth_style: "none",
    protocols: [{ protocol: "openai_chat" }],
  },
  {
    id: "lmstudio",
    label: "LM Studio 本地",
    name: "LM Studio 本地",
    tag: "lmstudio",
    kind: "openai_chat",
    base_url: "http://127.0.0.1:1234/v1",
    auth_style: "none",
    protocols: [{ protocol: "openai_chat" }],
  },
];
