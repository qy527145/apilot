# CLAUDE.md

本文件每次会话都会自动加载，请保持精简。详细的模块地图在 [docs/CODE_MAP.md](docs/CODE_MAP.md)，
协议兼容矩阵（何时直通 / 何时转换 / 转换损耗）在 [docs/PROTOCOL_MATRIX.md](docs/PROTOCOL_MATRIX.md)。

## 这是什么

Apilot —— 面向 Agent 客户端（Claude Code / Codex / Gemini CLI）的**本地 LLM API 网关**。
在本地起反向代理接管客户端的 `base_url`，在中间做协议转换、动态路由、流量监控、计费统计与响应缓存。
Tauri 2 + React 19，后端约 21k 行 Rust。

## 构建与测试

**本机必须先加载 MSVC 环境**，否则链接阶段会失败（Git Bash 自带的 `/usr/bin/link` 会抢占 MSVC 的 `link.exe`）：

```bash
source scripts/msvc-env.sh     # 每个新 shell 都要执行一次
cd src-tauri
cargo test                     # 856 个测试
cargo check --all-targets      # 期望零警告
cargo build
```

前端在仓库根目录：`bun run build:web`（即 `tsc && vite build`）。打包安装包用 `bun run build`（按当前系统自选）、
`build:win` / `build:mac` / `build:linux`。**命令名指目标平台**，入口 [scripts/build.mjs](scripts/build.mjs)
按当前系统决定本机编还是交叉编：只有「→ Windows」能交叉（macOS/Linux 出 nsis，msi 要 Windows），
macOS 与 Linux 的包只能在各自系统上构建。**`tauri.conf.json` 的 `beforeBuildCommand` 必须指向
`build:web`——指向 `build` 会自我递归。**

CI 打包在 [.github/workflows/build-installers.yml](.github/workflows/build-installers.yml)：推 `v*` 标签或手动触发，
三平台各自在目标系统的 runner 上**本机**打包（不涉及交叉编译）。**版本号必须四处一致**
（tag / `tauri.conf.json` / `Cargo.toml` / `package.json`），不一致会在编译前失败。

其他环境坑见 [README.md](README.md) 的「环境说明」。

## 架构一览

```
客户端 ──base_url 指向 127.0.0.1:8787──▶ axum 路由
   └─ 解码为 IR → 路由规则链 → selector 选渠道 → 查缓存
      → 编码为渠道协议 → 发上游 → 流式/非流式转码 → 结算 → 落库
```

| 层 | 目录 | 职责 |
|---|---|---|
| 协议 | `src-tauri/src/protocol/` | 协议无关的 IR + 三个协议的编解码器（含流式） |
| 网关 | `src-tauri/src/gateway/` | HTTP 服务器、请求管线、SSE 原语、流式转发 |
| 路由 | `src-tauri/src/routing/` | 规则链引擎、条件求值、selector 热切换 |
| 上游 | `src-tauri/src/upstream/` | 渠道抽象、HTTP 出站、注册表 |
| 计费 | `src-tauri/src/billing/` | 倍率、结算公式、预扣/结算会话 |
| 缓存 | `src-tauri/src/cache/` | 缓存键、策略、存储与 LRU 淘汰 |
| 接管 | `src-tauri/src/takeover/` | 客户端配置的保序补丁与原子写入 |
| 存储 | `src-tauri/src/storage/` | SQLite 连接、迁移、各领域读写 |
| 命令 | `src-tauri/src/commands/` | Tauri 命令层（62 个） |
| 前端 | `src/` | 9 个页面 + shadcn/ui 组件 |

## 改什么去哪里

| 我要… | 主要改动点 |
|---|---|
| **加一个新协议**（如 Gemini） | `protocol/dto.rs` 加 `Protocol` 变体 → 新建 `protocol/<name>/`（request/response/stream/mod）→ `protocol/codec.rs::CodecRegistry::new` 注册 → 按需加 `gateway/router.rs` 路由 |
| **加一个新客户端接管** | `takeover/clients.rs` 加 `ClientId` 变体 + `config_paths()` + `plan_apply()`（顺带补 `stored_base_url()`）；`config/paths.rs` 加路径函数 |
| **改「客户端配置里的网关地址」** | 写地址的只有一条路：`gateway/server.rs::base_url()`（通配监听地址会折算成回环）。网关换地址后跟着改的逻辑是 `takeover/clients.rs::repoint_taken_over`，**触发点在 `gateway/server.rs::serve_on` 而不是设置命令里**（只有那里知道网关真正跑在哪）。**Codex 还要顺手重启它的常驻 app-server**（`takeover/codex_daemon.rs`）—— 接管/还原/换地址三条路都接上了，漏掉任一条，用户看到的就是「明明改了却不生效」|
| **改接管策略 / 模型策略什么时候生效** | **保存即重写**：`update_settings` / `set_model_policy` 之后跑 `commands/app.rs::reapply_after_policy_change` → `takeover/clients.rs::reapply_taken_over`（只碰**会消费 `ClientPlan`** 的客户端，目前只有 Codex；产出与现状一致就不写，网关没起时用配置里现存的地址）。判据是 `takeover/clients.rs::plan_inputs_differ`（只看真会写进客户端的那两样）—— 与「换地址」那条（`repoint_taken_over`，只看地址）**不是同一个判据**，别合并 |
| **改「Codex 走不走 Responses Lite」** | **`use_responses_lite` 是唯一开关**（决定工具走 `input[].additional_tools` 还是顶层 `tools`），而它来自 Codex 的**内置模型目录** —— GPT 系名字内置就是 Lite，而上游对这形状常常「收下、200、静默忽略」。两条对策由 `AppSettings::client_model_mode` 四选一（默认 `both`，见 `takeover/clients.rs::codex_config`）：写模型名（`rename`，不依赖网关但没有 `apply_patch`）/ 下发目录（`catalog`，有 `apply_patch` 但要多两个开关 + 网关得可达）/ 都写（`both`）/ 不碰。目录内容在 `codex/mod.rs`（**分两半**：vendored 那半管 GPT 系名字，`entry` 那半管 Apilot 自己的模型名，**三个字段必须一起改**），出口是 `gateway/router.rs` 的 `/codex/models` |
| **加一种渠道鉴权方式** | `storage/models.rs::AuthStyle` → 同文件 `Provider::auth_header()`（**鉴权头的唯一构造点**，出站转发、连通探测、拉模型列表都走它） |
| **改「直通还是转换」的判定** | `storage/models.rs::Provider::wire_for`（渠道声明的协议集合命中就直通）+ `gateway/pipeline.rs::prefer_native_protocol`（多渠道路由时同协议优先）→ 同步更新 [docs/PROTOCOL_MATRIX.md](docs/PROTOCOL_MATRIX.md) |
| **改出站 URL 拼接** | `storage/models.rs::Provider::endpoint`（协议默认路径，会补 `/v1`）/ `endpoint_verbatim`（用户手写路径，**不补** `/v1`） |
| **改渠道启停 / 协议自动检测** | 启停走 `storage/providers.rs::set_enabled`（只改启用位，**别借道 `upsert`** —— 密钥不回显，改个开关就会把它抹掉）+ `commands/providers.rs::set_provider_enabled`（写完必须 `reload_providers`）。检测的判定全在 `commands/providers.rs::judge_protocol`，探测体 `PROBE_BODY` 是**故意的空对象**（不消耗 token）→ 前端 `ProviderDialog` 的「自动检测」 |
| **改「用哪个模型」（模型名）** | 判定在 `routing/model_policy.rs::effective_model`（在 `gateway/pipeline.rs` 解码后、路由前应用）；两级配置（全局 + 客户端覆盖）在 `config/settings.rs::ModelPolicy::effective` 里拼成一条规则；模式 4 的 JS 沙箱在 `routing/model_script.rs`。规则链的 `ModelOverride` 在它之上再改 |
| **改「用哪个渠道」（服务商）** | `routing/model_select.rs::order`（按模型策略排序）+ `gateway/pipeline.rs::build_candidates`（未配策略时回落 selector）。**优先级是「模型策略 > selector」，别反过来**。策略存在 `model_policies` 表，界面上在**路由页**改 |
| **改计费公式** | `billing/engine.rs::settle`（唯一真源）→ 对应更新其测试；倍率字段在 `billing/pricing.rs` |
| **加一种路由匹配条件** | `routing/rule_item.rs` 加 `RuleItem` 变体（`matches` + `describe` + 测试）→ 前端 `src/components/routing/RuleEditor.tsx` |
| **加一种路由动作** | `routing/rule.rs::RouteAction`（注意 `is_final` 的归类）→ `routing/engine.rs::route` 的 match → 前端 `ruleSummary.ts` |
| **加一个 Tauri 命令** | 在 `commands/<域>.rs` 写 → `lib.rs` 的 `generate_handler!` 用**完整路径**（不能 re-export）→ 前端 `src/lib/api.ts` |
| **改缓存策略** | `cache/policy.rs::is_cacheable` + `cache/key.rs`（键必须覆盖所有影响输出的因素） |
| **改监控详情的可视化** | `protocol/inspect.rs`（把捕获报文解成 IR，只解码不搬运）+ `src/components/traffic/InspectViews.tsx`（渲染 IR）。**不要新建视图模型** —— IR 已经协议无关且字段齐备 |
| **改 SSE 处理** | `gateway/sse.rs`（原语）→ `gateway/stream.rs`（转发管线） |
| **加数据库表/字段** | `storage/migrations.rs` **追加**一条迁移（不要改已发布的），同步 `docs/CODE_MAP.md` |
| **改前端页面** | `src/pages/<Name>Page.tsx` + `src/lib/api.ts` 的类型与封装 |

## 铁律

这些都是踩过坑总结出来的，改动相关代码时务必守住：

1. **`UnifiedUsage.input_tokens` 是不含缓存的 fresh 输入。**
   Anthropic 本就不含，OpenAI / Responses / Gemini 的 `prompt_tokens` **含**缓存。
   各 codec 负责折算到这个口径。破坏它会导致缓存 token 被重复计费。
   → 有专门测试守着：`protocol/oai_chat/response.rs::cached_tokens_are_subtracted_from_prompt_tokens`

2. **同协议直通，跨协议才重编码。**
   `gateway/pipeline.rs` 里 `needs_conversion` 为 false 时转发**原始字节**，只旁路统计用量。
   「同协议」指**入站协议 == 渠道实际使用的协议**，由 `Provider::wire_for` 按渠道声明的协议集合决定，
   不是"入站协议 == 渠道的 kind"。渠道声明了入站协议就直通，没声明才回落到 kind 做转换。
   但有个例外：响应若是从 SSE 还原出来的，必须重新编码 —— 见 `decoded_from_sse`。

   **另有一条与原样转发不同的路，别混**：路径不是三种对话入口之一时走
   `pipeline::handle_raw`，**连 IR 都不解**（`/v1/messages/count_tokens` 这类非对话
   接口），请求与响应原样收发、上游报错不包装。铁律说的"直通"是上面那条。

3. **规则链区分终结与非终结动作。** `RouteAction::is_final()` 决定命中后是否停止求值。
   新增动作时想清楚它属于哪类；非终结动作只改写 `RouteMetadata` 然后继续。

4. **接管只动「我拥有」的键**，用户配置的其余部分一律不碰。
   补丁前先解析，解析失败**立即中止**，绝不从空文档重建。

5. **缓存只对 `temperature` 显式为 `0` 的请求生效。** `None` 表示用服务商默认值（通常是 1.0），
   输出是随机的，缓存会返回错误结果。这条不能放宽。

6. **鉴权头落库前必须隐去。** 见 `gateway/pipeline.rs::headers_to_json`。

7. **Tauri 命令不能用 `pub use` 转发**，`generate_handler!` 里必须写完整路径。
   原因：`#[tauri::command]` 生成的 `__cmd__*` 宏项不参与 re-export。

8. **不要在 async 里跨 `await` 持锁。** 从 `DashMap` / `RwLock` 取出对象后先 `.clone()` 成 `Arc`
   再释放守卫；`ArcSwap` 没有守卫，`load_full()` 返回 `Arc` 即结束。

9. **发往回环与私有网段的请求要绕过系统代理。** 清单在 `upstream/client.rs::NO_PROXY_LIST`。
   少了它，配了 `HTTP_PROXY` 的机器连不上本地的 ollama / LM Studio。

10. **解不出不等于要失败。** 客户端与上游总在先用上新结构（Claude Code 带附件时发的
    `document` 内容块）。IR 不认识就**整块原样留着**（`ContentBlock::Unmodeled`）放请求过去，
    只在跨协议转换时才降级 —— 直通那条路本就不需要理解它们，报 400 挡掉的反而是完全正常的路。
    响应侧同理：非流式解不出就透传原文（`gateway/pipeline.rs::try_outbound`），
    流式的坏帧原样转发给客户端（`gateway/stream.rs`）。

## 代码约定

- **注释用中文，写「为什么」不写「是什么」。** 项目里大量注释在解释取舍原因（为什么不这么做、
  什么情况下会出错）。新增代码请沿用这个风格 —— 一个 `// 递增计数` 是噪音，
  一个 `// 用 RAII 而非手工递减：提前返回的分支太多，手工会漏` 才是有效信息。
- **测试与实现同文件**，放在底部 `#[cfg(test)] mod tests`。测试名描述行为而非方法名
  （`cached_tokens_are_subtracted` 而不是 `test_decode_usage_2`）。
- **优先补测试而不是补注释**来固定行为；负面用例（拒绝非法输入、坏数据不 panic）尤其重要。
- 错误统一用 `error.rs::AppError`，`thiserror` 派生；Tauri 侧序列化成 `{ code, message }`。
- 提交前跑 `cargo check --all-targets`，**期望零警告**。不要用 `#[allow(dead_code)]` 掩盖；
  真没用的代码就删掉。
