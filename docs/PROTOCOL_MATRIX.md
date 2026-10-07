# 协议兼容矩阵

Apilot 在客户端与模型服务商之间做协议转换。这份文档回答三个问题：

1. 一次请求什么时候**直通**、什么时候**转换**；
2. 三种协议两两之间能不能转、转的时候丢什么；
3. 服务商路径不标准导致 404 时怎么用渠道配置修。

---

## 先分清：哪些请求根本不进这张矩阵

只有三种对话入口（含无 `/v1` 的变体）才谈得上协议转换：

```
/v1/messages            /messages
/v1/chat/completions    /chat/completions
/v1/responses           /responses
```

判据是**路径后缀**，所以客户端带自定义 base path（`/anthropic/v1/messages`）照样算。

**其余路径一律原样转发**（`/v1/messages/count_tokens` 是典型）。它们不是模型对话，
body 里没有对话语义，响应也不是补全结果 —— 解成 IR 再编码回去轻则丢字段，
重则把上游的解释直接改坏。这条路只从 body 里取一个 `model` 用来选渠道，
请求与响应原样收发，**上游的报错也原样交给客户端**（"这个服务商不支持它"该由
客户端自己看见，由 Apilot 包装一下反而把它藏了）。

> 与下面说的「直通」不是一回事：那个仍在对话那条路上，只是不做跨协议转码；
> 这个连 IR 都不解。

---

## 判定规则

每个渠道声明一组**支持的协议**（`providers.protocols`，界面在渠道对话框的「支持的协议」）。
客户端说哪种协议，Apilot 按下面两步挑上游协议：

```
wire = 入站协议 ∈ 渠道声明的协议集合 ? 入站协议 : 渠道的首选协议(kind)
```

- `wire == 入站协议` → **直通**：客户端原始字节原样转发（只把 `model` 字段换成映射后的名字），
  不重编码。用量只是旁路统计出来的。
- `wire != 入站协议` → **转换**：`decode(入站) → IR → encode(wire)`。

没声明过协议的渠道，集合退化成 `{kind}`，等价于旧行为。

**所以"能不能直通"完全由渠道配置决定。** 一个同时提供 `/v1/messages` 与
`/v1/chat/completions` 的服务商，把两种都勾上，Claude Code 与 Codex 就都能直通；
只勾一种，另一种客户端每次请求都要转换一遍。

> 之所以优先直通：转换是经 IR 的两段式，未建模的原生字段会在这一步丢掉
> （见下方[损耗清单](#损耗清单)）。直通没有这个问题。

---

## 矩阵

行 = 客户端在用的协议，列 = 该渠道实际使用的协议。上游实际用哪个协议见上一条规则。

| 客户端 ↓ \ 上游 → | Anthropic Messages | OpenAI Chat | OpenAI Responses |
|---|---|---|---|
| **Anthropic Messages** | 直通 | 转换 | 转换 |
| **OpenAI Chat** | 转换 | 直通 | 转换 |
| **OpenAI Responses** | 转换 | 转换 | 直通 |

**九种组合全部可用**，非对角线的那六种走 `decode(A) → IR → encode(B)`。
对角线是直通，前提是渠道把该协议声明进了「支持的协议」。

想确认某次请求实际走了哪条路，看监控页：日志行的「协议」列会标 **直通** 或 **转换**，
详情弹窗里有完整的入站路径与上游 URL 可以对照。

详情弹窗的「可视化」视图**就是 IR 的渲染** —— 后端用对应协议的 codec 把捕获的报文
解一次（`protocol/inspect.rs`），三个协议解出来是同一套结构。所以：

- 对照「请求 · 客户端 → Apilot」与「请求 · Apilot → 上游」，能直接看出转换做了什么
  （工具还在不在、system 提到顶层没有、思考块是不是被剥了）；
- 流式响应没有完整响应体，可视化用的是**由增量重建**的内容块
  （`captures.response_content`）—— 那是流式请求唯一能看到思考与工具调用的地方；
- 「原始」看未经加工的报文（流式就是 SSE 帧），「格式化」看排整齐的 JSON。

---

## 损耗清单

转换不是无损的。下面每一条都对应代码里的具体位置。

### 1. Anthropic 扩展思考的签名无法跨协议保真

`Thinking.signature` / `RedactedThinking` 由 Anthropic 侧签名，转到别的协议再转回来必然失效，
回传会让上游 400。所以跨协议时 Apilot 会**主动剥离**思考块
（`gateway/pipeline.rs::strip_unportable_thinking`），并清掉 `reasoning.budget_tokens`。

同协议直通时不剥，签名原样保留 —— 这也是 Claude Code 走 Anthropic 渠道时
必须直通的原因之一。

### 2. 内容块上的 `cache_control` 只在直通时保留

Anthropic 用 `content[].cache_control: {"type":"ephemeral"}` 标记提示缓存断点。
IR 的 `ContentBlock` 没有建模这个字段（也没有 `extra` 兜底），转换到 OpenAI 系协议时它会被丢掉。

影响：跨协议转换后提示缓存命不命中由上游自己决定，Apilot 无法替它保留断点。
**要保住缓存收益就让该渠道直通 Anthropic 协议。**

### 3. Responses 的 `additional_tools` 会被折叠

OpenAI Responses 允许把工具定义藏在一个 `role: developer` 的 `additional_tools` 条目里
（Codex 就这么发）。解码时它被折叠进普通的 `tools` 数组
（见 `protocol/codec.rs::codex_responses_lite_tools_survive_conversion_to_chat` 的测试）。

折叠是有意的：不折叠的话，转成 Chat 协议后上游收到的请求里根本没有 tools，Codex 直接不可用。

### 4. `finish_reason` 归一后细节变粗

`FinishReason` 把各家的终止原因归一成 `Stop / Length / ToolUse / ContentFilter / Other{value}`：

| 原始值 | 归一为 |
|---|---|
| `stop`、`end_turn`、`stop_sequence` | `Stop` |
| `length`、`max_tokens` | `Length` |
| `tool_use`、`tool_calls`、`function_call` | `ToolUse` |
| `content_filter`、`refusal` | `ContentFilter` |
| 其它 | `Other { value }`（原样带回） |

哪一条 stop sequence 命中了，在这一步丢失。非枚举内的值不会丢。

### 5. 未建模的**顶层**字段靠 `extra` 原样带过去

各协议的 `decode_request` 会把没建模的顶层字段塞进 `UnifiedRequest.extra`
（`#[serde(flatten)]`），`encode_request` 再用 `or_insert` 写回目标请求体
—— 已显式写入的字段优先，不会被覆盖。

同名不等于同义：一个 Anthropic 专有的顶层字段被带到 Chat 请求体里，
严格的上游会因未知字段报 400。这不是 bug 而是取舍（宁可少建模也不要悄悄丢功能），
但遇到上游报"unknown field"时，这是第一个该怀疑的地方。

### 6. 用量口径统一成「不含缓存的 fresh 输入」

`UnifiedUsage.input_tokens` 一律是**不含缓存命中的新增输入**。
Anthropic 本来就是这口径；OpenAI / Responses 的 `prompt_tokens` **含**缓存，
各 codec 负责把 `prompt_tokens_details.cached_tokens` 减掉。

这是计费正确性的前提（缓存 token 有自己的倍率），
守着它的测试是 `protocol/oai_chat/response.rs::cached_tokens_are_subtracted_from_prompt_tokens`。

### 7. 上游无视 `stream:false` 一律回 SSE 时，终止原因退化为 `Stop`

有些中转不认 `stream: false`，永远回 SSE。Apilot 会把这段 SSE 解析成完整响应
（`gateway/pipeline.rs::decode_sse_as_response`），但 `StreamDecoder` 没有把
增量里的 stop_reason 暴露出来，所以这种情况下 `finish_reason` 固定是 `Stop`。
正文是完整的，只有终止原因是近似值。

---

## 用路径覆盖修 404

出站 URL = `base_url` + 该协议的路径。

- 路径**留空**时用协议默认值（`/v1/messages`、`/v1/chat/completions`、`/v1/responses`），
  拼接时会自动避免 `base_url` 与路径都带 `/v1` 造成的重复。
- 路径**填了**就原样拼在 `base_url` 后面，只做斜杠归一，**不会替你补 `/v1`**。

所以服务商把接口挂在子路径下时，直接在渠道里填真实路径：

| 服务商实际地址 | base_url | 路径覆盖 |
|---|---|---|
| `https://gw.example.com/api/chat` | `https://gw.example.com` | `/api/chat` |
| `https://gw.example.com/api/chat` | `https://gw.example.com/api` | `/chat` |
| `https://api.deepseek.com/v1/chat/completions` | `https://api.deepseek.com/v1` | 留空（默认路径已去重） |

第二条是有意为之：**base_url 之外的部分就是你写的那个路径**，
不猜、不推断。猜错的代价是一次必然 404。

### 排查在途 404

监控页点开一条失败请求，看「Apilot → 上游」这一栏：

- **URL** —— 和上面那张表对照，确认 `base_url` + 路径拼出来的是不是你想要的那个。
- **模型** —— 详情里的「上游模型」是映射后真正发出去的名字。上游报 model not found 时先看它。
- **Body** —— 转换后的请求体。跨协议时上游收到的是这个，不是客户端发的那份。

再看「上游 → Apilot」这一栏的响应体原文，服务商通常会直接写明原因是什么。

日志行的「状态码」旁边如果多出一个「上游 404」，说明那是上游返回的 404、
而 Apilot 按自己的约定回给了客户端另一个码 —— 两个数字都留着，就不会对不上。

---

## 各服务商的真实端点

新建渠道时选预设，Apilot 会把这些值直接填进去。下表是它们的依据 ——
**用错路径是这类配置最常见的故障，而且表现就是 404。**

| 服务商 | base_url | Anthropic Messages | OpenAI Chat | OpenAI Responses |
|---|---|---|---|---|
| Anthropic 官方 | `https://api.anthropic.com` | `/v1/messages` | — | — |
| OpenAI 官方 | `https://api.openai.com/v1` | — | `/v1/chat/completions` | `/v1/responses` |
| DeepSeek | `https://api.deepseek.com` | `/anthropic/v1/messages` | `/v1/chat/completions` | `/v1/responses` |
| Moonshot (Kimi) | `https://api.moonshot.cn` | `/anthropic/v1/messages` | `/v1/chat/completions` | — |
| 通义千问（百炼） | `https://dashscope.aliyuncs.com` | `/apps/anthropic/v1/messages` | `/compatible-mode/v1/chat/completions` | — |
| OpenRouter | `https://openrouter.ai/api/v1` | `/v1/messages` | `/v1/chat/completions` | `/v1/responses` |
| 硅基流动 | `https://api.siliconflow.cn/v1` | — | `/v1/chat/completions` | — |
| Ollama / LM Studio | `http://127.0.0.1:11434/v1` 等 | — | `/v1/chat/completions` | — |

「—」表示该服务商没有这个端点（或没核实到），**不是"用默认路径即可"**。
表格里没写就不勾那一种协议，让 Apilot 走转换 —— 转换一定可用，猜路径不一定。

### 两个容易踩的点

**1. Anthropic 兼容入口几乎都不在 `/v1` 下。**
DeepSeek 在 `/anthropic`、Moonshot 在 `/anthropic`、百炼在 `/apps/anthropic`。
拿默认路径 `/v1/messages` 去拼就是 404 —— 而且它长得跟 Anthropic 官方一模一样，
不盯着看很难发现。所以渠道对话框里每个勾选的协议下面都会直接显示拼出来的完整地址。

**2. base_url 带不带 `/v1` 会影响覆盖路径怎么拼。**
覆盖路径是**原样**接在 base_url 后面的。所以 base_url 取到 `/v1` 时，
再写 `/anthropic/...` 就会拼出 `.../v1/anthropic/...`，多一层。
稳妥的做法是 base_url 只填到公共前缀（裸域名），两种协议各自的完整路径都在下面写全 ——
Moonshot 和百炼的预设就是这么配的。

---

## 相关代码

| 关注点 | 位置 |
|---|---|
| 协议 IR 与三个 codec | `src-tauri/src/protocol/` |
| 选协议（直通还是转换） | `upstream/channel.rs::Channel::wire_for` → `storage/models.rs::Provider::wire_for` |
| 路径拼接 | `storage/models.rs::Provider::endpoint` / `endpoint_verbatim` |
| 直通 / 转换的分叉 | `gateway/pipeline.rs::try_outbound` 的 `needs_conversion` |
| 流式转码 | `gateway/stream.rs::translate_stream` |
| 渠道协议声明 | `providers.protocols`（迁移 v2） |
