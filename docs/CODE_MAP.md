# 代码地图

按模块展开的详细索引。用于快速定位「某个能力实现在哪」。
精简版导航与铁律见 [CLAUDE.md](../CLAUDE.md)。

行数为参考量级，用来判断一个模块是"读十分钟"还是"读半小时"。

---

## 目录

- [请求全链路](#请求全链路) — 一次请求经过哪些文件
- [协议兼容矩阵](PROTOCOL_MATRIX.md) — 什么时候直通、什么时候转换、转换丢什么
- [后端模块](#后端模块)
  - [protocol — 协议 IR 与编解码](#protocol--协议-ir-与编解码6409-行)
  - [gateway — 反向代理与请求管线](#gateway--反向代理与请求管线3364-行)
  - [routing — 模型选择、规则链与热切换](#routing--模型选择规则链与热切换1874-行)
  - [upstream — 上游渠道](#upstream--上游渠道771-行)
  - [catalog — 上游模型目录](#catalog--上游模型目录价格--能力)
  - [codex — 发给客户端的模型目录](#codex--发给客户端的模型目录)
  - [billing — 计费](#billing--计费848-行)
  - [cache — 响应缓存](#cache--响应缓存1109-行)
  - [takeover — 客户端接管](#takeover--客户端接管1763-行)
  - [storage — 持久化](#storage--持久化3316-行)
  - [traffic — 实时统计与事件](#traffic--实时统计与事件419-行)
  - [commands — Tauri 命令层](#commands--tauri-命令层775-行)
  - [shell / config / error / util](#shellconfigerrorutil)
- [数据库](#数据库)
- [前端](#前端)
- [测试](#测试)

---

## 请求全链路

一次请求（`POST /v1/messages`）依次经过：

| # | 位置 | 做什么 |
|---|---|---|
| 1 | `gateway/router.rs` | 按路径定协议，取出 body |
| 2 | `gateway/pipeline.rs::handle` | 分配 request_id、识别客户端、建 `Recorder` |
| 3 | `protocol/codec.rs::Codec::decode_request` | 字节 → `UnifiedRequest`（IR） |
| 4 | `protocol/shared/tokens.rs::estimate_request_tokens` | 估算输入 token（供路由分流与预扣） |
| 5 | `routing/engine.rs::Router::route` | 顺序跑规则链，首个终结动作胜出 |
| 6 | `routing/selector.rs::SelectorManager::resolve` | selector → 具体渠道（`Arc<dyn Outbound>`） |
| 7 | `cache/key.rs` + `cache/store.rs` | 可缓存则查缓存，命中直接返回 |
| 8 | `protocol/codec.rs::Codec::encode_request` | 必要时编码为目标渠道协议 |
| 9 | `upstream/channel.rs::Channel::prepare` | 拼 URL、注入鉴权头、应用渠道级 header |
| 10 | `upstream/channel.rs::Channel::dial` | 发请求；失败且可重试则换渠道 |
| 11 | `gateway/stream.rs::translate_stream` | 流式：上游 SSE → 解码 → IR 增量 → 编码 → 下游 |
| 12 | `billing/engine.rs::BillingEngine::settle` | 按用量与倍率算额度 |
| 13 | `gateway/pipeline.rs::Recorder::finish` | 落库明细 + 内存聚合 + 推送前端事件 |

非流式走 `pipeline.rs::finish_buffered`，逻辑与 11 对应。

---

## 后端模块

### `protocol/` — 协议 IR 与编解码（6409 行）

核心抽象：**所有协议转换都经过 IR**。N 个协议只需 2N 个编解码器，而不是 N² 个两两转换器。

| 文件 | 内容 |
|---|---|
| `dto.rs` | IR 定义：`Protocol`、`Role`、`ContentBlock`、`UnifiedRequest` / `UnifiedResponse`、`UnifiedUsage`、`UnifiedDelta`、`FinishReason` |
| `codec.rs` | `Codec` / `StreamDecoder` / `StreamEncoder` trait、`ConvertError`、**`CodecRegistry`**（转换调度入口） |
| `anthropic/` | Anthropic Messages 的 `mod`（Codec 实现）/ `request` / `response` / `stream` |
| `oai_chat/` | OpenAI Chat Completions，同结构 |
| `oai_responses/` | OpenAI Responses，同结构（`mod.rs` 内含 request+response）。**流式编码器的 `output_item.done` 必须带完整 item** —— Codex 只从这里取工具调用，见 [PROTOCOL_MATRIX.md](PROTOCOL_MATRIX.md#客户端的硬契约responses-流靠-output_itemdone-收工具调用) |
| `shared/tokens.rs` | 无上游 usage 时的本地 token 估算（按 CJK / 拉丁字符分档） |
| `shared/tools.rs` | 工具调用的跨协议处理（分片 JSON 累积、结果拍平） |
| `inspect.rs` | 把**捕获的报文**解成 IR，供监控页做语义化展示。解四个方向（入站在此协议、出站协议各一份请求与响应），流式响应从 `captures.response_content` 还原。解码失败只填 `error`，不抛错 |

**关键约定**：`UnifiedUsage.input_tokens` 是**不含缓存的 fresh 输入**。
Anthropic 的 `input_tokens` 本就不含缓存；OpenAI 与 Responses 的 `prompt_tokens` **含**
`cached_tokens`，由各自 codec 做减法折算。这是全项目最容易踩的坑 —— 破坏它会导致缓存被重复计费。

**流式三件套**：每个协议提供 `new_stream_decoder()`（上游 SSE → IR 增量）与
`new_stream_encoder()`（IR 增量 → 下游 SSE）。上游的拆分顺序未必合法，编码器负责补齐
（如 Anthropic 要求 `message_start` 在最前、`content_block_start` 先于同 index 的 delta）。

**未建模字段进 `extra`**（`IndexMap`，保序）原样透传。把字段列进每个 codec 的
`KNOWN_TOP_LEVEL` 等于承诺「编码时会写回去」，漏写就是静默丢数据 —— 加字段时注意。

**空集合字段会整个消失**：`UnifiedRequest` 的 `system` / `tools` / `stop` 带
`skip_serializing_if`，为空时 JSON 里没有这个键（不是空数组）。前端若当成必有数组去
`.map()`，会在"这次请求没带工具"这种最常见的场景下崩掉。
→ 有测试守着：`dto.rs::empty_collections_are_omitted_from_the_wire_format`。

---

### `gateway/` — 反向代理与请求管线（3364 行）

| 文件 | 内容 |
|---|---|
| `pipeline.rs` | **核心编排**。`handle()` 串起整条链路；`handle_raw()` 是**非对话请求的原样转发**（见下）；`Recorder` 收口记账；`detect_client()` 识别来源；`decode_sse_as_response()` 兜住「上游无视 stream 参数」 |
| `server.rs` | `GatewayServer`：`start` / `start_at` / `stop` / `status` / `base_url`，用 oneshot + graceful shutdown。**`rebind` 负责"改了监听地址要真的换过去"**：端口变了先绑新的探路（绑不上就原样退回，绝不先停再试），新地址起不来则退回老地址；`needs_rebind` 是它的纯函数判据 |
| `router.rs` | axum 路由表、`/health`、`/v1/models`、路径兜底。兜底按**后缀**判：是三种对话入口之一就交给 `handle`，**其余一律交给 `handle_raw` 原样转发** —— 客户端会发 `/v1/messages/count_tokens` 这类非对话接口，硬解成对话再编码回去会把它改坏 |

**两种"直通"别混**（`CLAUDE.md` 铁律 2 说的是前者）：

| | 什么时候 | 做了什么 |
|---|---|---|
| **同协议直通** | 入站协议 == 渠道声明的协议 | 仍在对话那条路上。转发原始字节，只把 `model` 换成映射后的名字；不解码再编码 |
| **非对话原样转发** | 路径不是三种对话入口 | 走 `handle_raw`。**连 IR 都不解**，只从 body 里取一个 `model` 用来选渠道，请求与响应都原样收发，上游的报错也不包装 |
| `stream.rs` | `translate_stream()`（上游流 → 下游流）、`stream_from_response()`（缓存重放流）、**`ContentAccumulator`**（从增量重建完整内容，供缓存写入）、**`StreamTimings`**（每个事件的时间点，供时间轴；平行数组 + 名字表，两千帧几 KB）。`delta_display_name()` 那套名字**必须与前端 `StreamDelta` 的 serde 标签逐字相同**，有测试钉着 |
| `sse.rs` | SSE 原语：`take_sse_block`（两种分隔符取最早）、`append_utf8_safe`（跨 chunk 多字节）、`parse_event` / `encode_event` |
| `proxy_e2e.rs` | 端到端测试：起真实上游 HTTP 服务跑通全链路 |

**两个关键分支**：
- `needs_conversion` 为 false 时转发**原始字节**（只旁路统计），避免无谓的字段丢失。
  是否为 false 由 `Outbound::wire_for(入站协议)` 决定 —— 渠道声明了该协议就直通。
- 但若响应是从 SSE 还原的（`decoded_from_sse`），即使协议相同也必须重新编码 ——
  原始字节是 SSE 正文，透传会把 `event:` 行喂给要 JSON 的客户端。

**两个方向的追踪**：入站在 `handle` 建 `CaptureRecord`，出站在 `try_outbound` 里
`prepare()` 之后补 `UpstreamTrace`（URL / headers / body / 上游状态码 / 上游原始响应）。
流式路径没有 `Recorder::finish`，收尾在 `finalize_stream`，所以入站那份捕获必须
显式 take 过去合并 —— 直接新建一条空捕获会被 `INSERT OR REPLACE` 覆盖成空值。

**记账收口**：无论成功、失败、流式中断还是客户端断连，都从 `Recorder::finish` / `fail`
同一条路径写日志与聚合，避免某个分支漏账。

---

### `routing/` — 模型选择、规则链与热切换（1874 行）

借自 sing-box 的模型。**规则顺序求值，首个终结动作胜出。**

| 文件 | 内容 |
|---|---|
| `model_policy.rs` | **模型替换的判定**：「这次请求该用哪个模型名」。全局与客户端两级在 `ModelPolicy::effective()` 里拼成一条规则，`effective_model()` 再把那条规则算成模型名。判定是纯函数 —— 需要向外部问的两件事（有没有渠道、跑一段脚本）都从 `ModelEnv` 注入，所以四种模式（不改写 / 强制覆盖 / 兜底 / 自定义规则）都能脱离数据库与 JS 引擎测 |
| `model_script.rs` | **用户 JS 脚本的执行器**（QuickJS，见 `Cargo.toml` 的 rquickjs）。脚本被限制成 `ctx` 的**纯函数**（无 I/O、无 `hasChannels()`），因此结果能按 (脚本, 模型, 客户端) 缓存，命中完全不进引擎。任何语法错 / 抛错 / 超时 / 非字符串返回都折成"不改写"，**绝不失败请求** |
| `model_select.rs` | **按模型策略给候选渠道排序**（`order()`）：按优先级 / 按延迟 / 加权随机。随机源与延迟都从外面传，所以三种策略都能脱离数据库测 |
| `metadata.rs` | `RouteMetadata`（贯穿规则链的上下文，可被非终结动作改写）、`RouteOptions` |
| `rule_item.rs` | 匹配条件：`RuleItem` 枚举（client / model / protocol / path / header / token_estimate / logical）、`glob_match` |
| `rule.rs` | `RouteAction`（**终结**：`Final` / `Reject`；**非终结**：`ModelOverride` / `RouteOptions` / `Sniff`）、`RouteRule` |
| `engine.rs` | `Router::route()` —— 遍历规则，终结即停，非终结改写后继续；`RouteOutcome` |
| `selector.rs` | `Selector`（`ArcSwap` 热切换）、`SelectorManager`（按 tag 索引 + 兜底解析）、`SelectionEvent` |

**模型的两次改写，顺序不能调**：先在 `pipeline::handle` 解码后应用模型替换策略
（`model_policy::effective_model` —— 它内部先按客户端把两级配置拼成一条规则），
再跑规则链（`meta` 可被 `ModelOverride` 再改）。
规则链因此看到的是生效模型，也能在它之上继续改。

**三个模型名各司其职，别混用**（`RequestLogRecord` 上同名）：

| 字段 | 含义 | 用在哪 |
|---|---|---|
| `request_model` | 客户端请求的名字 | 展示；也进 `usage_hourly` 供统计页标出改写 |
| `model` | 生效模型（全局策略 + 规则改写之后） | 计费、聚合、**缓存键**、模型列表筛选 |
| `upstream_model` | 渠道 `model_mapping` 之后真正发出去的名字 | 只在出站报文里 |

混用会出事：缓存键若跟 `request_model` 走，换了模型会直接命中上一个模型生成的答案。

**「用哪个渠道」的两条路径**（`pipeline::build_candidates`）：

- 模型在 `model_policies` 里有行 → 按 `model_select::order` 排序，第一个当主渠道；
- 没有行 → 沿用 `resolve(selector)` 选出的那个（**旧行为，一键不动**）。

`model_policies` 表里没有行 == 交给 selector 与规则链，这条界线是既有配置不被破坏的基础。

**热切换为什么不需要重启**：`Selector` 的当前选中项存在 `ArcSwapOption<String>` 里，
读侧每次请求只做一次原子读 + 一次 `DashMap` 查表，拿到的 `Arc<dyn Outbound>` 在请求生命周期内稳定。
即使此刻切换，在途请求仍走旧渠道跑完，新请求立刻走新渠道。

**命名空间注意**：`ProviderRegistry` 的「默认渠道」是**渠道 tag**，与 selector tag 是两套命名空间。
它只在「指定的 selector 不存在」时兜底，取优先级最高的启用渠道。

---

### `upstream/` — 上游渠道（771 行）

| 文件 | 内容 |
|---|---|
| `outbound.rs` | `Outbound` trait（`tag` / `wire` / `prepare` / `dial`）、`PreparedRequest`、`UpstreamResponse` / `UpstreamBody`、`UpstreamError::is_retryable` |
| `channel.rs` | `Channel` —— 唯一的 HTTP 实现。转发请求头时的**剔除清单**、鉴权头注入、`looks_like_json` 判定 |
| `registry.rs` | `ProviderRegistry`：`DashMap<String, Arc<dyn Outbound>>` + `ArcSwapOption<String>` 默认渠道 |
| `client.rs` | 共享 `reqwest::Client` 与**出站代理**。`ProxySpec`（解析出的出站路径）、`ClientSpec`（路径 + TLS 策略，**也是 `ClientPool` 的缓存键**）、`resolve_global` / `resolve_channel`（把设置折算成 spec）、`NO_PROXY_LIST`（回环与私有网段绕过代理） |
| `oneshot.rs` | **脱离网关管线的单次真实请求**：协议编码 → `prepare` → `dial`。模型测试与能力探测都走它 —— 走管线会写日志、计费、查缓存，对一次探测全是副作用 |

**出站 URL 由渠道的线协议决定，与入站路径无关**：客户端说 Anthropic，
渠道是 OpenAI 类型时就该发到 `/v1/chat/completions`。

**代理分两级**：全局 `AppSettings.proxy`（跟随环境变量 / 直连 / 指定地址），
渠道 `Provider.proxy`（跟随全局 / 直连 / 指定地址）。解析成 `ProxySpec` 后按它
在 `ClientPool` 里取客户端 —— 客户端数量 = 不同 `ClientSpec` 数，不是渠道数。
`Direct` 分支必须显式 `no_proxy()`，否则 reqwest 会自己读环境变量绕过「直连」。

**TLS 校验收在 `ClientSpec` 里，不随渠道变化**：`ProxySettings.insecure_tls`
（「忽略 TLS 证书校验」，抓包时免装 CA）对所有出站连接生效，与走不走代理无关。
它必须参与缓存键 —— `ClientPool` 跨 `reload` 存活，键里不带它就会在用户拨开关后
把旧客户端还回去，表现为"开关无效，重启才好"。根证书走系统存储
（`Cargo.toml` 里 reqwest 开 `rustls-tls-native-roots`）；用 reqwest 默认的
`rustls-tls` 会只认编译期打包的 Mozilla 根，装了 CA 也照样 `UnknownIssuer`。

---

### `catalog/` — 上游模型目录（价格 + 能力）

| 文件 | 内容 |
|---|---|
| `mod.rs` | `CatalogSource`（models.dev / LiteLLM）、归一化后的 `CatalogModel`、`to_pricing`（价格 → 倍率）、`index_by_model`（压平去重）、`fetch` |
| `models_dev.rs` | 按厂商分组的 JSON。价格已是 $/1M，直接用 |
| `litellm.rs` | 扁平 JSON，价格是 $/token，**要乘 1e6** |

`probe.rs`（顶层模块）是目录的对照面：**主动探测**某渠道某模型支不支持
思考/工具/多模态。三种能力的判法**刻意不一样** —— 工具和图片不支持时上游会返
4xx，所以「被接受」即「支持」；思考则不然，OpenAI 系对不认识的 `reasoning_effort`
是静默忽略，只能看回包里有没有思考块。

**两个来源不是互为备份**：models.dev 覆盖更全更新（kimi / glm / qwen3-max
只有它有，DeepSeek 已是 v4 命名），LiteLLM 模型更多但偏一手大厂。实测两者对
个别模型（如 DeepSeek）报价能差一倍，所以界面上必须标出来源。

**为什么不合成一个网络集成**：公开目录本来就同时维护价格和能力标志，分两次
下载只是把同一个文件拉两遍。

---

### `codex/` — 发给客户端的模型目录

上游目录（上面那个 `catalog/`）是**收进来**的：价格、能力。这里是**发出去**的：
网关反过来告诉 Codex 客户端「模型长什么样」。两者只共享"模型目录"这个词。

| 文件 | 内容 |
|---|---|
| 文件 | 内容 |
|---|---|
| `mod.rs` | `catalog(names)`（内置目录 + Apilot 自己的模型名，都关掉 Responses Lite）、`entry`（为 Apilot 的模型名生成的条目）、`CATALOG_PATH` |
| `../assets/codex/` | vendored 的 Codex 原文（`models.json` / `prompt.md`）+ 来源与更新说明 |

**目录分两半，缺一不可**（`gateway/router.rs::apilot_model_names` 负责收集第二半要用到的名字）：

- **内置那半**（vendored，逐条关掉 Lite）：管 GPT 系名字。
- **Apilot 自己那半**（`entry`，为 Apilot 会给客户端用的模型名各生成一条）：管
  「接管时写入当前模型」写的那些名字。

目录是**按名字**生效的 —— 客户端用哪个名字，就查哪条。少了第二半，只要客户端用的不是
GPT 系名字，就会落到 Codex 自己的兜底元数据上（那里没有 `apply_patch`），于是
**「配了目录」和「换了模型名」互相抵消** —— 配了半天一点用没有。

**接管会不会用它，由 `AppSettings::client_model_mode` 决定**（客户端页四选一，默认 `both`）：
`catalog` / `both` 会把 `model_catalog_url` 写进客户端配置，`off` / `rename` 则会把
它（和那两个开关）**删掉**。细节与四个模式各自的取舍见上面 takeover 一节。

> 这两条路现在**互补**：目录取到 → 完整元数据（含 `apply_patch`）；取不到（网关没起 /
> 目录被撤）→ 退回 Codex 兜底，经典工具集、没有 `apply_patch`，但至少不是 Lite。

**为什么要有它。** Codex 用不用 Responses Lite（把工具塞进 `input[].additional_tools`，
形状是 `namespace > custom`）**只由模型元数据里的 `use_responses_lite` 决定**，而这份
元数据来自它自己的内置目录 —— `gpt-6-sol` 这类 GPT 系名字内置就是 `true`。

要命的地方是：不少上游对这个形状**收下请求、返回 200，却完全不解析**（DeepSeek 的
`/v1/responses` 就是）。直通转发看不出任何异常，模型那侧却一个工具都没有，只能把调用
写成 `<｜DSML｜｜ calls>` 这样的正文 —— 客户端看到的是"模型把工具调用当普通文本吐出来"。

所以网关把目录接过来，逐条把 Lite 关掉再回给客户端。三个字段必须**一起**改：
`use_responses_lite = false`（工具走顶层）、`tool_mode = "direct"`（否则 code mode 的
自由格式 `exec` 会跑到顶层，而上游对顶层 `custom` 普遍只接受 `apply_patch`）、提示词换成
通用那份（被 Lite 关住的条目，其提示词在教模型调 `functions.exec`，工具集一换就对不上）。
细节见 `mod.rs` 的模块注释。

目录是**整体替换**客户端内置那份的，所以 vendored 文件里每一条都得留着，包括不改的。
拉取失败只会让客户端退回自己的兜底元数据（能用，只是没有 `apply_patch`），不会坏。

**它比换模型名多出来的只有 `apply_patch`**：兜底元数据（换名字那条路拿到的）不带这个工具，
模型就只能用 shell 写文件。多出来的代价是客户端启动时必须够得着网关，以及那两个开关。


---

### `billing/` — 计费（848 行）

| 文件 | 内容 |
|---|---|
| `quota.rs` | 单位换算。`QUOTA_PER_UNIT = 500_000`（1 USD = 500000 quota），整数 `i64` 饱和运算 |
| `pricing.rs` | `ModelPricing`（各级倍率）、**`PricingTable`**（热替换，支持按日期后缀回退匹配家族价格） |
| `engine.rs` | **`BillingEngine::settle`** —— 结算公式的唯一真源，产出 `QuotaBreakdown` |
| `session.rs` | `BillingSession`：预扣 → 结算 → 退款，幂等；记录估算偏差 |

**结算公式**（`model_ratio = 1` 对应每 100 万 token 2 美元，故 1 token × 倍率 = 1 quota）：

```
prompt_units     = fresh_input + cache_read × cache_ratio + cache_create × cache_create_ratio
completion_units = output × completion_ratio
quota            = (prompt_units + completion_units) × model_ratio × group_ratio
quota           ×= Π other_ratios
quota           += tool_call_surcharge × 工具调用次数
有倍率却算得 ≤ 0 时兜底为 1
```

`cache_saved_quota` 单独算：`cache_read × (1 - cache_ratio) × 倍率`，只计正数。

---

### `cache/` — 响应缓存（1109 行）

| 文件 | 内容 |
|---|---|
| `policy.rs` | `CachePolicy`、**`is_cacheable`**（只放行 `temperature` 显式为 0 的请求）、`CacheScope` |
| `key.rs` | `cache_key` —— sha256(协议 + 模型 + system + 消息 + 工具 + 采样参数 + 思考配置) |
| `store.rs` | `ResponseCache`：DB 读写、命中计数、LRU 淘汰（**优先淘汰从未命中的**）、过期清理 |

**为什么只缓存 `temperature == 0`**：`temperature` 缺失表示用服务商默认值（通常 1.0），
输出是随机的，返回旧答案会让用户以为模型卡住。要求显式设为 0，是把「我要确定性」
变成一个可检验的条件。这条不能放宽。

**键的取舍**：只放影响输出的因素，`stream` 标志刻意**不**参与（流式与非流式应共享同一份结果）。

---

### `takeover/` — 客户端接管（1763 行）

| 文件 | 内容 |
|---|---|
| `patch.rs` | 保序补丁器：`patch_json` / `patch_toml` / `patch_dotenv`。**解析失败即中止** |
| `engine.rs` | `TakeoverEngine`：`ensure_backup` / `write_atomic` / `commit` / `restore`；`rename_with_retry` |
| `clients.rs` | 每个客户端的 `plan_apply`（Claude / Codex / Gemini）、`config_paths`、`current_base_url`、`stored_base_url`、`describe_all`、`repoint_taken_over`（网关换地址后把**已接管**的客户端改指过去，见下） |
| `floor.rs` | 共享常量：`LOCAL_PLACEHOLDER_KEY`、`CODEX_PROVIDER_NAME` |

**两条设计原则**：
1. **只动「我拥有」的键**，用户配置的其余部分一律不碰（否则一次接管会毁掉 MCP 配置、权限设置）。
2. **还原靠首次写入前的字节级备份**，而不是反向推导原值 —— 后者在用户中途手动改过配置时必然出错。

**写入三段式**：先写同目录临时文件 + `fsync`，再原子 `rename`（Windows 上带退避重试）。
直接 `fs::write` 写到一半被杀会留下截断的 JSON，客户端下次启动直接报配置错误。

**跟着网关地址走**：网关换监听地址后，已接管的客户端配置里还留着老地址。同步逻辑是
`clients::repoint_taken_over`，触发点却不在设置命令里，而在 **`gateway/server.rs::serve_on`**
—— 只有那里知道网关真正跑在哪（改完设置未必生效：端口被占会退回老地址；端口填 0 时
真实端口也是那一刻才分配）。启动、换地址、退回老地址三条路都汇到 `serve_on`，一处全覆盖。
判据是「客户端现在指的地址 ≠ 目标地址」，不是整份文件比 —— 后者会把用户手加的模型覆盖、
密钥也当成"不一致"，然后被 `plan_apply` 抹掉。

**Codex 还会按模式多写两样东西**（`plan_codex` + `AppSettings::client_model_mode`，
客户端页四选一，默认 `both`）。两样都在解决同一件事：**别让客户端走 Responses Lite**
（工具塞进 `input[].additional_tools`）—— 那形状对不少上游是**静默失效**的：收下请求、
返回 200、工具一个不认，模型只能把调用写成 DSML 正文。

| 模式 | 写什么 | 换来什么 / 代价 |
|---|---|---|
| `off` | 无 | 什么都不碰 |
| `rename` | `model` | 换成 Codex 不认识的模型名 → 元数据退回兜底那份（经典顶层 `tools`）。**不用联网、不依赖任何客户端开关**；代价是没有 `apply_patch`，且按模型名配的路由规则会跟着变 |
| `catalog` | `model_catalog_url` + `features.api_key_model_discovery` + `suppress_unstable_features_warning` | 拿到完整元数据（**含 `apply_patch`**）；代价是要多写两个开关、客户端启动时得够得着网关（取不到会静默退回 Lite） |
| `both` | 上面两套 | **默认**。互补：目录取不到时正好轮到名字那条路兜底 |

两个细节值得记住：

- **`model` 键只写不删。** 那是用户原本选的模型名，被我们覆盖过之后已经拿不回来了；
  删掉只会退回 Codex 的默认模型 —— 也就是又走 Lite。想恢复原值只有「还原」。
  目录那**三个键则会删**：留着不是"无害的残留"（客户端还在拉我们的目录、一个开发中
  特性还开着、一个全局的「别警告我」开关还挂着，而用户界面上已经关掉了它），所以
  `patch::TomlOp` 加了 `Remove*`。代价是：万一用户在我们接管**之前**就自己设过同名键，
  切回 `off` 会把他的值一并清掉 —— 撞车极罕见，且比"留着我们塞进去的东西"好解释；
  要精确回到原样就用「还原」（写回接管前的原始字节）。
- **改模式保存即生效**：设置写完就调 `reapply_taken_over` 把已接管的客户端重写一遍
  （判据与触发点见下）。

Claude Code / Gemini 那边**没有**注入模型：Claude 的接管反而是**清掉**模型覆盖键的，
要给它注入得先想清楚和那个动作的关系。

**Codex 还有一件必须顺手做的事：重启它的常驻 app-server**（`takeover/codex_daemon.rs`）。

Codex 的 TUI / 桌面版**不是自己读配置发请求**的 —— 它们连一个常驻的 app-server 进程，
provider、`base_url`、模型元数据都是**那个进程启动时**读的。所以改完配置：

- 光重启客户端没用（它只是重新连上那个老进程），
- 现象极具欺骗性：客户端进程明明是刚起的，行为却完全是老配置的。

`codex exec` 不经过它（日志里 `rpc.transport="in-process"`），所以那边一直是对的，只有
TUI / 桌面版会中招 —— 而用户不可能猜到要去重启一个后台进程。三个改动路径都接上了：
接管、还原、以及 `repoint_taken_over`（地址变了同样要重启）。只重启**正在跑**的进程
（`codex exec` 的用户全程不需要它，凭空拉起来是越界）；重启失败**不影响接管成败**，
只在提示里让用户自己执行 `codex app-server daemon restart`。

**两条自动路，判据不同，别合并**：

- **策略**（`client_model_mode` + 每个客户端的模型名）：`commands/app.rs` 在
  `update_settings` / `set_model_policy` 之后调 `reapply_taken_over`，保存即重写。它只碰
  **会消费 `ClientPlan`** 的客户端（目前只有 Codex），产出与现状逐字节一致时一个字节都不
  写；网关没在跑时退回客户端配置里现存的地址（地址本就不该在这次改动里变）。
- **地址**：「客户端现在指的地址 ≠ 目标地址」才写。触发点是 `serve_on`（启动 / 换地址 /
  退回老地址），逐客户端算模型名（模型策略允许给单个客户端单独指定），因此它收的是
  `&AppSettings` 而不是一个模型名。

---

### `storage/` — 持久化（3316 行）

| 文件 | 内容 |
|---|---|
| `migrations.rs` | **手写 DDL 数组**（不用 sqlx 编译期宏），按 `PRAGMA user_version` 增量执行 |
| `db.rs` | 连接池 + PRAGMA（WAL / foreign_keys / busy_timeout）；`open_memory()` 供测试 |
| `models.rs` | `Provider`、`ProviderKind`、`AuthStyle`、`ProviderModel`、`ProtocolEndpoint`。**`Provider::auth_header()`** 是鉴权头的唯一构造点；`wire_for()` 决定直通还是转换，`endpoint` / `endpoint_verbatim` 是出站 URL 的拼接点 |
| `providers.rs` | 渠道 CRUD、**`set_enabled`**（渠道页那枚启停开关：只改启用位，不走整份 `upsert`）、模型映射、**`candidate_channels`**（按每模型优先级排序，路由的候选来源）/ `candidates_for_model`（含停用渠道，模型页展示用）、**`set_model_candidates`**（跨渠道写，与 `set_models` 是同一张表的两个方向）、`has_declared_models`（接管前置条件） |
| `routing.rs` | 路由规则 / selector / 兜底配置的读写；`ensure_default_selector` |
| `pricing.rs` | 单价系数读写；`load_table` 装配 `PricingTable` |
| `logs.rs` | 请求明细 + 双向捕获原文（入站 / 出站 / **发给客户端的响应头**）；`query`（动态过滤：时间、客户端、模型、协议、状态、是否流式）、`get_detail`、`facets`（筛选下拉的候选值）、`clear_all`、`prune_captures` / `prune_logs`。捕获还含流式响应的 `response_content`（IR）与两侧原始 SSE 帧 |
| `model_policies.rs` | 每模型的渠道选择策略读写。**没有行 = 交给 selector 与路由规则** |
| `aggregates.rs` | **`AggregateBuffer`**（内存聚合 + 定期 upsert）、`summary` / `summary_by` / `timeseries` / `p50_ttfb` / `speed_averages`（按 token 加权的平均 TTFT / ITL / TPS，样本口径的唯一真源在 `logs::speed_sample`）。按模型汇总时额外返回 `request_models`（被折叠进该生效模型的客户端模型名），供统计页标出改写 |

**迁移规则**：`MIGRATIONS` 数组**只追加，不修改已发布的条目**。
每条用 `IF NOT EXISTS` 保证幂等，版本号是下标。

**模型声明的两张表分工**（容易踩坑）：
- `provider_models` 是**真源**：声明「该渠道支持哪些入站模型」以及「该模型在这个渠道上
  真正叫什么」（`upstream_model`），驱动 `channels_for_model` 的候选筛选。
  没有行的渠道视为通吃。
- `providers.model_mapping` 是它的**派生读模型**：请求改写（`Provider::upstream_model`）
  与网关 `GET /v1/models` 都读这里。任何一处写完都在同一个事务里重算它，
  `upsert_provider` 也是 —— 它**不收**调用方传来的映射，否则编辑渠道对话框会拿
  打开时的旧快照把别处刚配好的重定向覆盖回去。
- 两个写入口是同一张表的两个视角，各自全量替换自己那一维，所以谁都不能把对方那一列
  冲成默认值：`set_models`（渠道页，「这个渠道有哪些模型」）保留已有的
  `upstream_model` / `enabled`；`set_model_candidates`（路由页，「这个模型在哪些渠道上、
  上游叫什么」）保留渠道维。

---

### `traffic/` — 实时统计与事件（1480 行）

| 文件 | 内容 |
|---|---|
| `mod.rs` | `TrafficStats`（并发守护 `ActiveRequest`、累计计数、TTFB 环形窗口）、`TrafficEvent` |
| `events.rs` | `EventBus`（按类型推送 + 节流）、`Throttle`、`event_names` |
| `stream_events.rs` | **流式实时事件**：`StreamBatcher`（攒批 + 封顶 + 单帧截断，纯逻辑）、`StreamEmitter`（观察者实现，`Drop` 兜底断连）、`StreamDelta`（推流用的增量形态） |
| `log_filter.rs` | **监控页「自定义表达式」筛选的求值器**：QuickJS 里跑用户表达式，逐行判断要不要显示。线程本地引擎。**报文按需取用**：元数据里只放哨兵，表达式真读到才回调 Rust 解析；于是要走两趟 —— `probe`（只带元数据的探测趟，报出哪些行需要报文）与 `filter`（带真报文的终趟）。单行 1s / 整批 2s / 报文 64MB 预算，撞上批预算标 `truncated` 而不是报错。求值放在**后端**是因为日志分页、报文在 `captures` 另一张表 —— 前端只看得见当前页。内置对象与字段见 `expr_meta` |

**事件名**（前端 `listen` 用的字面量，改了就静默破坏订阅）：

```
apilot://gateway           → GatewayStatus
apilot://traffic           → TrafficEvent { rps, active, ttfb_p50_ms, total_requests }
apilot://selector-changed  → SelectionEvent { selector, provider_tag, reason }
apilot://request           → RequestLog
apilot://cache             → CacheStats
apilot://request-start     → RequestStarted   （监控页「进行中」的进入点）
apilot://request-end       → RequestFinished  （离开点；不节流，见下）
apilot://stream            → StreamEvent      （流式请求的实时事件批次）
```

并发计数用 **RAII 守卫**而非手工 inc/dec：请求从多个分支提前返回（协议错误、上游失败、
客户端断连），手工递减必然漏掉某条路径，导致并发数只增不减。

**`request-end` 为什么不复用 `apilot://request`**：后者走 `EventBus::request`，
带 250ms 节流且会合并负载 —— 当「结束」信号用会让一部分请求永远停在「进行中」。
`request-start` / `request-end` 都是小而必达、不节流的。

**流式请求的结束由 `StreamEmitter` 负责，不走 `Recorder::finish`**：流真正跑完
才算结束，而 `Recorder` 早就返回了。客户端断连时转发生成器在 `yield` 点被丢弃，
`on_finish` 根本不会执行 —— `StreamEmitter` 的 `Drop` 是唯一的兜底。

**推流增量用 `StreamDelta` 而不是直接序列化 `UnifiedDelta`**：后者用内部标签
`type`，而 `Finish(FinishReason)` 里包的枚举**也**用 `type` 当标签，会拼出
`{"type":"finish","type":"tool_use"}` 这种重复键，前端 `JSON.parse` 只留最后一个。
推流侧因此换用 `kind`。

---

### `commands/` — Tauri 命令层（775 行）

| 文件 | 命令数 | 内容 |
|---|---|---|
| `app.rs` | 5 | `app_info`、`get_settings`、`update_settings`、`set_model_policy`（只改模型策略，避免整份 `AppSettings` 回传冲掉别处刚改的设置）、`validate_model_script`（只编译不执行，给脚本文本框做行内报错） |
| `gateway.rs` | 3 | `gateway_start` / `stop` / `status` |
| `providers.rs` | 9 | 渠道 CRUD、`test_provider`、**`set_provider_enabled`**（渠道页的启停开关：只改启用位，写完重载注册表）、模型声明（`set_provider_models` 只收模型名，上游名归 `models.rs`）、`fetch_provider_models`（拉上游 `/v1/models`）、**`detect_provider_protocols`**（协议自动检测：三种入口各发一次空体请求，判定见 **`judge_protocol`** —— 只有"路径存在"的证据才算数，且**不写库**）；同文件的 **`probe()`** 是普通函数而非命令，被路由页复用 |
| `routing.rs` | 10 | 规则 CRUD + 排序、selector CRUD + **`switch_selector`**（热切换）、`run_urltest` |
| `billing.rs` | 6 | 单价 CRUD、`billing_summary` / `totals` / `timeseries` |
| `cache.rs` | 4 | `cache_stats`、`clear_cache`、策略读写 |
| `takeover.rs` | 7 | `detect_clients`、`takeover_status`、`takeover_readiness`（接管前置条件）、`preview_takeover`、`apply_takeover`、`restore_client`、`open_client_config`（后端按 client 解析路径后用系统默认程序打开，不收前端传的路径） |
| `models.rs` | 8 | 模型视角：`list_model_catalog` / `list_model_options`、`get_model_policy`（模型替换，与 `app.rs` 的 `set_model_policy` 一对）、`upsert_model_policy` / `reset_model_policy` / `switch_model_channel`（每模型选渠道）、`set_model_candidates`、`probe_model_candidates` |
| `logs.rs` | 3 | `query_logs`、`list_log_facets`（筛选下拉的候选值），`get_request_detail`、`clear_logs`（只清明细与捕获，不动 `usage_hourly`） |

**命令注册**：全部在 `lib.rs` 的 `generate_handler!` 里，用**完整路径**。
`#[tauri::command]` 生成的 `__cmd__*` 宏项不参与 re-export，不能用 `pub use` 转发。

**参数命名**：Rust 侧 `snake_case` 参数在前端要用 **camelCase**（`providerId`、`groupBy`）；
返回结构体的字段是 **snake_case**。

---

### `shell/`、`config/`、`error/`、`util`

| 文件 | 内容 |
|---|---|
| `shell.rs` | **`AppShell`** —— 所有共享依赖的唯一所有权根（db / settings / registry / selectors / router / pricing / cache / aggregates / traffic / events / gateway）。`bootstrap()` 装配全部状态；`reload_*()` 做配置热重载；`spawn_background_tasks()` 跑流量推送、聚合落库、日志清理 |
| `config/paths.rs` | 全部路径解析。用 `dirs::home_dir()` 而非 `HOME` 环境变量；`APILOT_HOME` 可覆盖数据根目录 |
| `config/settings.rs` | `AppSettings`（单条 JSON 存 `settings_kv`）、`normalized()` 夹取非法值。`ModelPolicy` 也在这里：两级模式 + 客户端覆盖 + 自定义规则，**`normalized()` 同时负责老存档的折算**（`"off"` 靠 serde alias，`per_client` 的字符串值靠 untagged —— 认不出一个枚举串会让整份设置回落默认值）。`ProxySettings` 同理：`#[serde(other)]` 的 `Unknown` 兜住拼错的模式串；`insecure_tls` 是新加字段，靠容器上的 `#[serde(default)]` 让老存档（没有这个键）读出来是「严格校验」而不是整份回落默认） |
| `error.rs` | `AppError`：同时实现 `Serialize`（给 Tauri）与 `IntoResponse`（给 axum） |
| `util.rs` | `now_ms` / `now_secs` / `hour_bucket` / `mask_secret` |

---

## 数据库

13 张表，SQLite（WAL 模式）。DDL 真源在 `storage/migrations.rs`。

| 表 | 主键 / 唯一 | 用途 |
|---|---|---|
| `model_policies` | `model` | 每个模型的渠道选择策略（priority / latency / weight + 手动选中的渠道）。**没有行 = 交给 selector 与路由规则** |
| `providers` | `tag` 唯一（内部生成，界面不显示） | 渠道：base_url、鉴权、**支持的协议集合**（`protocols`，驱动直通/转换的判定）、派生的模型映射、权重、超时、**代理覆盖**（`proxy`，NULL = 跟随全局） |
| `provider_models` | `(provider_id, model, client_group)` | 模型↔渠道映射（等价 new-api 的 abilities）。**没声明任何模型的渠道视为通吃**。`upstream_model` 就是「客户端发这个名，上游该收哪个名」。写入都会重算 `providers.model_mapping`（见上） |
| `route_rules` | `id` | 规则链，按 `sort_index` 求值；`items` / `action` 存 JSON |
| `selectors` | `tag` | selector 定义 + **`current_provider`**（热切换的持久化落点） |
| `route_config` | 单行 `id=1` | 兜底 selector |
| `request_logs` | `request_id` 唯一 | 请求明细：token、quota、耗时、**TTFB**、缓存命中、估算偏差；以及**方向信息**：入站 `path`、出站 `upstream_url` / `upstream_model` / `upstream_status` |
| `usage_hourly` | `(bucket_ts, client, provider_tag, model, request_model)` | 小时聚合，SUM 后 upsert。`model` 是**生效模型**（计费口径），`request_model` 是客户端原名 —— 两者一起进主键，统计页才答得出"我发的 gpt-6-sol 怎么算在 deepseek-flash 这行"。**`clear_logs` 不动它** —— 它是计费口径的历史账目。另有观感指标的**累加量**（`sample_requests` / `ttfb_sum_ms` / `decode_ms_sum` / `decode_tokens_sum`）：平均值不能相加，所以存分子分母、汇总时再相除 |
| `model_pricing` | `model` | 单价系数。`source` 为 NULL = **用户手填**（批量导入一律不动），有值 = 由某份目录导入、可被同来源的下次导入覆盖 |
| `model_capabilities` | `(provider_id, model, capability)` | 这个**渠道上这个模型**支不支持思考/工具/多模态。`verdict` 是三态（supported / unsupported / inconclusive）—— 探测「支不支持工具」时模型可能只是那一次没调工具，记成布尔就是撒谎。`source` 分 probe（实测，花 token）与 catalog（目录断言，零成本）；**覆盖优先级写在 `storage::capabilities` 的 upsert SQL 里**：实测且明确 > 目录 > 实测但不确定 |
| `response_cache` | `key`（sha256） | 缓存条目：响应体、usage、原额度、命中数 |
| `captures` | `request_id` | **两个方向的原文**：入站（客户端→Apilot）与出站（Apilot→上游，含上游 URL / headers / body 与上游原始响应）+ 流式拼接文本（**鉴权头已隐去**）+ 每个 SSE 事件的时间点（`stream_timings`，JSON，供时间轴） |
| `settings_kv` | `key` | 设置、单价兜底倍率、缓存计数器 |

时间约定：`request_logs.ts` 是 **unix 毫秒**；`usage_hourly.bucket_ts` 是**整点 unix 秒**。

---

## 前端

React 19 + Vite 8 + Tailwind v4 + shadcn/ui。**无路由库** —— `App.tsx` 单壳 + `currentView` 状态切换。

| 位置 | 内容 |
|---|---|
| `src/lib/api.ts` | **契约的唯一真源**：全部类型定义 + 62 个命令的类型化封装 + 统一错误处理。`Protocol` / `PROTOCOL_LABEL` / `PROTOCOL_DEFAULT_PATH` 也在这里，与后端 `Protocol` 的 JSON 名一一对应 |
| `src/lib/events.ts` | `useApilotEvent<T>` hook + 事件负载类型 |
| `src/lib/logExpr.ts` | 监控页自定义表达式筛选的**前端那一半**：为「进行中」请求组装内置对象并求值（已落库的走后端 QuickJS）。两边必须同语义，改一个就要改另一个 |
| `src/lib/utils.ts` | `cn`、`quotaToUsd`（1 USD = 500000 quota）、格式化 |
| `src/hooks/queries.ts` | react-query 封装 |
| `src/pages/*.tsx` | 9 个页面：Overview / Clients / Providers / Models / Routing / Traffic / Billing / Cache / Settings。**两页分工**：Models 只管**模型名**（全局 + 客户端两级替换、模型并集列表只读）；Routing 管**渠道**（selector 热切换、规则链、每个模型走哪个渠道、以及该渠道上的**上游模型名**）。Providers 是渠道视角 —— 同一份 `provider_models` 的三个方向 |
| `src/components/ui/` | 手写的 shadcn 组件（19 个） |
| `src/components/models/` | 模型名那一轴：`ModelPolicyCard`（两级四模式）、`CustomRuleEditor`（映射表 / JS 双轨）、`ModelChannelPicker`（渠道选择 + 每渠道的上游模型名，挂在路由页） |
| `src/components/routing/` | 规则编辑器（递归条件树 + 5 种动作）、拖拽排序、selector 热切换面板 |
| `src/components/providers/` | 渠道对话框与预设（含「支持的协议」声明与**自动检测** —— 逐个协议探一次，只补勾不取消；新建后自动拉一次上游模型列表并声明）、模型声明面板 `ProviderModelsPanel`（`ModelPickerDialog` 负责从上游拉列表并勾选；每行还有**单次模型测试**与三枚**能力徽标**，点徽标即实测一项）；`CatalogCapabilitiesDialog` / `CatalogPriceDialog` 是两个目录导入入口 |
| `src/components/traffic/` | 请求详情：`RequestDetailDialog`（顶层「请求 / 响应 / 时间轴」三段，前两段内部再分方向与「可视化 / 格式化 / 原始」三态）+ `InspectViews`（按语义渲染 IR：系统提示词、工具列表、对话上下文、回答、思考、工具调用、token 明细）+ **`StreamTimeline`**（每个事件一根耗时条；实时与明细两处共用，数据一个是内存里的 `at_ms`、一个是从库里读的 `stream_timings`）；`LiveStreamDialog`（**进行中**的流式请求：左侧事件时间轴 + 右侧「内容」（增量折叠）/「原文」两视图，数据来自 `apilot://stream`，不查库）；`FilterExprPanel`（监控页的**自定义表达式**筛选：编辑框 + 后端行内校验 + 示例 + 内置对象结构说明）；`TimeRangePicker`（监控页的**自定义时间范围**：按钮上直接显示所选区间，弹层里两个时间框 + 常用区间；与预设页签互斥） |

**改后端 API 时同步 `src/lib/api.ts`** —— 它是前后端契约的落点，两边不一致不会有编译错误，
只会在运行时静默失败。

---

## 测试

856 个测试，**与实现同文件**（`#[cfg(test)] mod tests`），`cargo test` 全量运行。

| 层次 | 代表 |
|---|---|
| 单元 | 协议往返等价、glob 匹配、计费公式、缓存键、TOML/dotenv 补丁、规则求值顺序 |
| 边界与负面 | SSE 跨 chunk 的 UTF-8、损坏的 JSON 拒绝写入、缓存 token 不小于 prompt 时不 panic、`temperature` 缺失时不缓存 |
| 集成（真实 HTTP） | `gateway/proxy_e2e.rs`：起假上游跑通「出站 → 流式转码 → 下游」，含双向协议转换、代理绕过、上游只回 SSE 的兜底 |

**跑单个模块**：`cargo test gateway::` / `cargo test protocol::oai_chat`

**新增代码时优先补测试而非注释**来固定行为；负面用例（拒绝非法输入、坏数据不 panic）尤其重要。
提交前跑 `cargo check --all-targets`，期望**零警告**。
