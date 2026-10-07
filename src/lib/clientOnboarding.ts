import type { Protocol } from "@/lib/api";

/**
 * 手动接入说明的内容源。
 *
 * 与 `docs/PROTOCOL_MATRIX.md` 分工不同：那边讲「什么时候直通、转换丢什么」，
 * 这边只讲「一个自研客户端要发什么才能打通」。**这里只写不会随配置变化的事实**
 * （必填字段、终止标记、路径），凡是取决于用户自己配了哪些渠道的，一律不写。
 */
export interface OnboardingProtocol {
  /** 网关入口路径。`/v1/xxx` 与 `/xxx` 两种写法都注册了，这里给标准的那份。 */
  path: string;
  /** 请求体的必填字段。 */
  required: string;
  /** 流式的终止标记（`Protocol::stream_terminator`）。 */
  terminator: string;
  /** 完整 curl 示例，`{base}` 由页面替换成实际 base_url。 */
  curl: string;
  /** 客户端一般要配的环境变量。值为 `{base}` 时同样做替换。 */
  env: Array<{ name: string; value: string; note: string }>;
  /** 该协议特有的坑。 */
  notes: string[];
}

export const ONBOARDING: Record<Protocol, OnboardingProtocol> = {
  anthropic: {
    path: "/v1/messages",
    required: "`model` 与 `messages`；`max_tokens` 可省（转给上游时补 8192）",
    terminator: "message_stop",
    curl: `curl {base}/v1/messages \\
  -H 'content-type: application/json' \\
  -H 'anthropic-version: 2023-06-01' \\
  -H 'x-api-key: apilot-local' \\
  -d '{
    "model": "claude-sonnet-5",
    "max_tokens": 1024,
    "stream": true,
    "messages": [{ "role": "user", "content": "你好" }]
  }'`,
    env: [
      {
        name: "ANTHROPIC_BASE_URL",
        value: "{base}",
        note: "不要带 /v1 —— SDK 自己会拼 /v1/messages",
      },
      {
        name: "ANTHROPIC_AUTH_TOKEN",
        value: "apilot-local",
        note: "占位值，随便填；见下方「鉴权」",
      },
    ],
    notes: [
      "`anthropic-version` 必须带。Apilot 原样转发给上游，Anthropic 系上游缺了这个头直接 400。",
      "`ANTHROPIC_API_KEY` 若与 `ANTHROPIC_AUTH_TOKEN` 同时存在，多数客户端优先用前者 —— 接管功能会主动删掉它，手动配置时也建议一并清掉。",
    ],
  },

  openai_chat: {
    path: "/v1/chat/completions",
    required: "`model` 与 `messages`",
    terminator: "[DONE]",
    curl: `curl {base}/v1/chat/completions \\
  -H 'content-type: application/json' \\
  -H 'authorization: Bearer apilot-local' \\
  -d '{
    "model": "gpt-5",
    "stream": true,
    "messages": [{ "role": "user", "content": "你好" }]
  }'`,
    env: [
      {
        name: "OPENAI_BASE_URL",
        value: "{base}/v1",
        note: "要带 /v1 —— OpenAI SDK 只拼 /chat/completions",
      },
      {
        name: "OPENAI_API_KEY",
        value: "apilot-local",
        note: "占位值，随便填；见下方「鉴权」",
      },
    ],
    notes: [
      "`max_tokens` 与 `max_completion_tokens` 都认，原样转给上游。",
    ],
  },

  openai_responses: {
    path: "/v1/responses",
    required: "`model`；`input` 可省（表示空输入）",
    terminator: "response.completed",
    curl: `curl {base}/v1/responses \\
  -H 'content-type: application/json' \\
  -H 'authorization: Bearer apilot-local' \\
  -d '{
    "model": "gpt-5",
    "stream": true,
    "input": "你好"
  }'`,
    env: [
      {
        name: "OPENAI_BASE_URL",
        value: "{base}/v1",
        note: "Codex 这类客户端会自己拼 /responses",
      },
      {
        name: "OPENAI_API_KEY",
        value: "apilot-local",
        note: "占位值，随便填；见下方「鉴权」",
      },
    ],
    notes: [
      "`input` 既可以是字符串，也可以是 item 数组（`[{type:\"message\", role, content}]`）。",
      "工具定义藏在 `role: developer` 的 `additional_tools` 条目里也认（Codex 就这么发），会被折叠进普通 `tools`。",
    ],
  },
};

/** 三种协议共用的一段说明 —— 鉴权对三个入口是完全一样的。 */
export const AUTH_NOTE = [
  "Apilot **不校验**入站密钥，填什么都能过 —— 它只负责把请求转给上游，凭据用的是你配在渠道里的那一份。",
  "客户端发的 `authorization` / `x-api-key` / `api-key` 会被**丢弃**，换成渠道自己的鉴权头（落库前也已隐去）。所以不要在客户端里放真实 key：它既不起作用，还会白白多一处泄漏点。",
  "反过来，`anthropic-version`、`anthropic-beta`、`content-type` 这类非鉴权头会原样转发给上游。",
];

/** 把示例里的 `{base}` 换成实际地址。 */
export function fillBase(text: string, base: string): string {
  return text.split("{base}").join(base);
}
