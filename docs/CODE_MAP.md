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
  - [billing — 计费](#billing--计费848-行)
  - [cache — 响应缓存](#cache--响应缓存1109-行)
  - [takeover — 客户端接管](#takeover--客户端接管1432-行)
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
| `oai_responses/` | OpenAI Responses，同结构（`mod.rs` 内含 request+response） |
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
| `pipeline.rs` | **核心编排**。`handle()` 串起整条链路；`Recorder` 收口记账；`detect_client()` 识别来源；`decode_sse_as_response()` 兜住「上游无视 stream 参数」 |
| `server.rs` | `GatewayServer`：`start` / `stop` / `status` / `base_url`，用 oneshot + graceful shutdown |
| `router.rs` | axum 路由表、`/health`、`/v1/models`、路径兜底（按路径特征猜协议） |
| `stream.rs` | `translate_stream()`（上游流 → 下游流）、`stream_from_response()`（缓存重放流）、**`ContentAccumulator`**（从增量重建完整内容，供缓存写入） |
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
| `model_policy.rs` | **全局模型替换**：`effective_model()`（关闭 / 无条件 / 兜底 / 按客户端四种模式）。判定是纯函数，"有没有渠道"作为参数传入 —— 只有兜底模式才查库 |
| `model_select.rs` | **按模型策略给候选渠道排序**（`order()`）：按优先级 / 按延迟 / 加权随机。随机源与延迟都从外面传，所以三种策略都能脱离数据库测 |
| `metadata.rs` | `RouteMetadata`（贯穿规则链的上下文，可被非终结动作改写）、`RouteOptions` |
| `rule_item.rs` | 匹配条件：`RuleItem` 枚举（client / model / protocol / path / header / token_estimate / logical）、`glob_match` |
| `rule.rs` | `RouteAction`（**终结**：`Final` / `Reject`；**非终结**：`ModelOverride` / `RouteOptions` / `Sniff`）、`RouteRule` |
| `engine.rs` | `Router::route()` —— 遍历规则，终结即停，非终结改写后继续；`RouteOutcome` |
| `selector.rs` | `Selector`（`ArcSwap` 热切换）、`SelectorManager`（按 tag 索引 + 兜底解析）、`SelectionEvent` |

**模型的两次改写，顺序不能调**：先在 `pipeline::handle` 解码后应用全局模型策略
（`model_policy::effective_model`），再跑规则链（`meta` 可被 `ModelOverride` 再改）。
规则链因此看到的是生效模型，也能在它之上继续改。

**三个模型名各司其职，别混用**（`RequestLogRecord` 上同名）：

| 字段 | 含义 | 用在哪 |
|---|---|---|
| `request_model` | 客户端请求的名字 | 只作展示 |
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
| `client.rs` | 共享 `reqwest::Client`，**`NO_PROXY_LIST`**（回环与私有网段绕过系统代理） |

**出站 URL 由渠道的线协议决定，与入站路径无关**：客户端说 Anthropic，
渠道是 OpenAI 类型时就该发到 `/v1/chat/completions`。

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

### `takeover/` — 客户端接管（1432 行）

| 文件 | 内容 |
|---|---|
| `patch.rs` | 保序补丁器：`patch_json` / `patch_toml` / `patch_dotenv`。**解析失败即中止** |
| `engine.rs` | `TakeoverEngine`：`ensure_backup` / `write_atomic` / `commit` / `restore`；`rename_with_retry` |
| `clients.rs` | 每个客户端的 `plan_apply`（Claude / Codex / Gemini）、`config_paths`、`current_base_url`、`describe_all` |
| `floor.rs` | 共享常量：`LOCAL_PLACEHOLDER_KEY`、`CODEX_PROVIDER_NAME` |

**两条设计原则**：
1. **只动「我拥有」的键**，用户配置的其余部分一律不碰（否则一次接管会毁掉 MCP 配置、权限设置）。
2. **还原靠首次写入前的字节级备份**，而不是反向推导原值 —— 后者在用户中途手动改过配置时必然出错。

**写入三段式**：先写同目录临时文件 + `fsync`，再原子 `rename`（Windows 上带退避重试）。
直接 `fs::write` 写到一半被杀会留下截断的 JSON，客户端下次启动直接报配置错误。

---

### `storage/` — 持久化（3316 行）

| 文件 | 内容 |
|---|---|
| `migrations.rs` | **手写 DDL 数组**（不用 sqlx 编译期宏），按 `PRAGMA user_version` 增量执行 |
| `db.rs` | 连接池 + PRAGMA（WAL / foreign_keys / busy_timeout）；`open_memory()` 供测试 |
| `models.rs` | `Provider`、`ProviderKind`、`AuthStyle`、`ProviderModel`、`ProtocolEndpoint`。**`Provider::auth_header()`** 是鉴权头的唯一构造点；`wire_for()` 决定直通还是转换，`endpoint` / `endpoint_verbatim` 是出站 URL 的拼接点 |
| `providers.rs` | 渠道 CRUD、模型映射、**`candidate_channels`**（按每模型优先级排序，路由的候选来源）/ `candidates_for_model`（含停用渠道，模型页展示用）、**`set_model_candidates`**（跨渠道写，与 `set_models` 是同一张表的两个方向）、`has_declared_models`（接管前置条件） |
| `routing.rs` | 路由规则 / selector / 兜底配置的读写；`ensure_default_selector` |
| `pricing.rs` | 单价系数读写；`load_table` 装配 `PricingTable` |
| `logs.rs` | 请求明细 + 双向捕获原文（入站 / 出站）；`query`（动态过滤）、`get_detail`、`clear_all`、`prune_captures` / `prune_logs`。捕获还含流式响应的 `response_content`（IR）与两侧原始 SSE 帧 |
| `model_policies.rs` | 每模型的渠道选择策略读写。**没有行 = 交给 selector 与路由规则** |
| `aggregates.rs` | **`AggregateBuffer`**（内存聚合 + 定期 upsert）、`summary` / `summary_by` / `timeseries` / `p50_ttfb` |

**迁移规则**：`MIGRATIONS` 数组**只追加，不修改已发布的条目**。
每条用 `IF NOT EXISTS` 保证幂等，版本号是下标。

**模型声明的两张表分工**（容易踩坑）：
- `provider_models` 是**真源**：声明「该渠道支持哪些入站模型」，驱动 `channels_for_model`
  的候选筛选。没有行的渠道视为通吃。
- `providers.model_mapping` 是它的**派生读模型**：请求改写（`Provider::upstream_model`）
  和网关 `GET /v1/models` 都读这里。`set_models` 在同一个事务里把前者同步过来。
  手工 `upsert_provider` **不会**反向写 `provider_models` —— 否则在渠道对话框点一次保存
  就会把用户在映射面板里逐条编好的声明冲掉。

---

### `traffic/` — 实时统计与事件（419 行）

| 文件 | 内容 |
|---|---|
| `mod.rs` | `TrafficStats`（并发守护 `ActiveRequest`、累计计数、TTFB 环形窗口）、`TrafficEvent` |
| `events.rs` | `EventBus`（按类型推送 + 节流）、`Throttle`、`event_names` |

**事件名**（前端 `listen` 用的字面量，改了就静默破坏订阅）：

```
apilot://gateway           → GatewayStatus
apilot://traffic           → TrafficEvent { rps, active, ttfb_p50_ms, total_requests }
apilot://selector-changed  → SelectionEvent { selector, provider_tag, reason }
apilot://request           → RequestLog
apilot://cache             → CacheStats
```

并发计数用 **RAII 守卫**而非手工 inc/dec：请求从多个分支提前返回（协议错误、上游失败、
客户端断连），手工递减必然漏掉某条路径，导致并发数只增不减。

---

### `commands/` — Tauri 命令层（775 行）

| 文件 | 命令数 | 内容 |
|---|---|---|
| `app.rs` | 3 | `app_info`、`get_settings`、`update_settings` |
| `gateway.rs` | 3 | `gateway_start` / `stop` / `status` |
| `providers.rs` | 7 | 渠道 CRUD、`test_provider`、模型映射、`fetch_provider_models`（拉上游 `/v1/models`）；同文件的 **`probe()`** 是普通函数而非命令，被路由页复用 |
| `routing.rs` | 10 | 规则 CRUD + 排序、selector CRUD + **`switch_selector`**（热切换）、`run_urltest` |
| `billing.rs` | 6 | 单价 CRUD、`billing_summary` / `totals` / `timeseries` |
| `cache.rs` | 4 | `cache_stats`、`clear_cache`、策略读写 |
| `takeover.rs` | 6 | `detect_clients`、`takeover_status`、`takeover_readiness`（接管前置条件）、`preview_takeover`、`apply_takeover`、`restore_client` |
| `models.rs` | 8 | 模型视角：`list_model_catalog` / `list_model_options`、`get_model_policy`（全局替换）、`upsert_model_policy` / `reset_model_policy` / `switch_model_channel`（每模型选渠道）、`set_model_candidates`、`probe_model_candidates` |
| `logs.rs` | 3 | `query_logs`、`get_request_detail`、`clear_logs`（只清明细与捕获，不动 `usage_hourly`） |

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
| `config/settings.rs` | `AppSettings`（单条 JSON 存 `settings_kv`）、`normalized()` 夹取非法值 |
| `error.rs` | `AppError`：同时实现 `Serialize`（给 Tauri）与 `IntoResponse`（给 axum） |
| `util.rs` | `now_ms` / `now_secs` / `hour_bucket` / `mask_secret` |

---

## 数据库

12 张表，SQLite（WAL 模式）。DDL 真源在 `storage/migrations.rs`。

| 表 | 主键 / 唯一 | 用途 |
|---|---|---|
| `model_policies` | `model` | 每个模型的渠道选择策略（priority / latency / weight + 手动选中的渠道）。**没有行 = 交给 selector 与路由规则** |
| `providers` | `tag` 唯一 | 渠道：base_url、鉴权、**支持的协议集合**（`protocols`，驱动直通/转换的判定）、模型映射、权重、超时 |
| `provider_models` | `(provider_id, model, client_group)` | 模型↔渠道映射（等价 new-api 的 abilities）。**没声明任何模型的渠道视为通吃**。`set_models` 会把声明同步派生成 `providers.model_mapping`（见下） |
| `route_rules` | `id` | 规则链，按 `sort_index` 求值；`items` / `action` 存 JSON |
| `selectors` | `tag` | selector 定义 + **`current_provider`**（热切换的持久化落点） |
| `route_config` | 单行 `id=1` | 兜底 selector |
| `request_logs` | `request_id` 唯一 | 请求明细：token、quota、耗时、**TTFB**、缓存命中、估算偏差；以及**方向信息**：入站 `path`、出站 `upstream_url` / `upstream_model` / `upstream_status` |
| `usage_hourly` | `(bucket_ts, client, provider_tag, model)` | 小时聚合，SUM 后 upsert。**`clear_logs` 不动它** —— 它是计费口径的历史账目 |
| `model_pricing` | `model` | 单价系数 |
| `response_cache` | `key`（sha256） | 缓存条目：响应体、usage、原额度、命中数 |
| `captures` | `request_id` | **两个方向的原文**：入站（客户端→Apilot）与出站（Apilot→上游，含上游 URL / headers / body 与上游原始响应）+ 流式拼接文本（**鉴权头已隐去**） |
| `settings_kv` | `key` | 设置、单价兜底倍率、缓存计数器 |

时间约定：`request_logs.ts` 是 **unix 毫秒**；`usage_hourly.bucket_ts` 是**整点 unix 秒**。

---

## 前端

React 19 + Vite 8 + Tailwind v4 + shadcn/ui。**无路由库** —— `App.tsx` 单壳 + `currentView` 状态切换。

| 位置 | 内容 |
|---|---|
| `src/lib/api.ts` | **契约的唯一真源**：全部类型定义 + 42 个命令的类型化封装 + 统一错误处理。`Protocol` / `PROTOCOL_LABEL` / `PROTOCOL_DEFAULT_PATH` 也在这里，与后端 `Protocol` 的 JSON 名一一对应 |
| `src/lib/events.ts` | `useApilotEvent<T>` hook + 事件负载类型 |
| `src/lib/utils.ts` | `cn`、`quotaToUsd`（1 USD = 500000 quota）、格式化 |
| `src/hooks/queries.ts` | react-query 封装 |
| `src/pages/*.tsx` | 9 个页面：Overview / Clients / Providers / Models / Routing / Traffic / Billing / Cache / Settings。**Models 是模型视角**（全局模型替换 + 每个模型的候选渠道与策略），Providers 是渠道视角 —— 同一份 `provider_models` 的两个方向 |
| `src/components/ui/` | 手写的 shadcn 组件（19 个） |
| `src/components/routing/` | 规则编辑器（递归条件树 + 5 种动作）、拖拽排序、selector 热切换面板 |
| `src/components/providers/` | 渠道对话框与预设（含「支持的协议」声明）、模型映射面板（`ModelPickerDialog` 负责从上游拉列表并勾选） |
| `src/components/traffic/` | 请求详情：`RequestDetailDialog`（顶层「请求 / 响应」两段，内部再分方向与「可视化 / 格式化 / 原始」三态）+ `InspectViews`（按语义渲染 IR：系统提示词、工具列表、对话上下文、回答、思考、工具调用、token 明细） |

**改后端 API 时同步 `src/lib/api.ts`** —— 它是前后端契约的落点，两边不一致不会有编译错误，
只会在运行时静默失败。

---

## 测试

476 个测试，**与实现同文件**（`#[cfg(test)] mod tests`），`cargo test` 全量运行。

| 层次 | 代表 |
|---|---|
| 单元 | 协议往返等价、glob 匹配、计费公式、缓存键、TOML/dotenv 补丁、规则求值顺序 |
| 边界与负面 | SSE 跨 chunk 的 UTF-8、损坏的 JSON 拒绝写入、缓存 token 不小于 prompt 时不 panic、`temperature` 缺失时不缓存 |
| 集成（真实 HTTP） | `gateway/proxy_e2e.rs`：起假上游跑通「出站 → 流式转码 → 下游」，含双向协议转换、代理绕过、上游只回 SSE 的兜底 |

**跑单个模块**：`cargo test gateway::` / `cargo test protocol::oai_chat`

**新增代码时优先补测试而非注释**来固定行为；负面用例（拒绝非法输入、坏数据不 panic）尤其重要。
提交前跑 `cargo check --all-targets`，期望**零警告**。
