# Apilot

面向 Agent 客户端的**本地 LLM API 智能网关**。在本地起一个反向代理，把 Claude Code、Codex 等客户端的 `base_url` 接管过来，在中间做协议转换、动态路由、流量监控、成本统计与响应缓存。

## 它解决什么问题

Agent 客户端各自绑定单一 `base_url` 和 API Key。于是：

- 想换个模型或渠道，得手动改配置文件、重启客户端；
- 看不到请求到底花了多少钱、慢在哪一段；
- 客户端说 Anthropic 协议、上游只提供 OpenAI 协议时无法对接；
- 客户端之间不能统一观测，也没法复用重复的请求。

Apilot 把这一层收拢到本地网关里，并提供界面管理。

## 功能

| 功能 | 说明 |
|---|---|
| **客户端接管** | 一键改写入站客户端的 `base_url` 指向本地网关；一键还原到接管前的字节。 |
| **多协议适配** | OpenAI Chat Completions / Anthropic Messages / OpenAI Responses 三协议**双向转换**，含 SSE 流式。渠道可声明自己支持哪几种协议，入站协议命中就直通、不命中才转换 —— 兼容矩阵与损耗见 [docs/PROTOCOL_MATRIX.md](docs/PROTOCOL_MATRIX.md)。 |
| **流量监控** | 全量捕获**双向**请求与响应（客户端→Apilot / Apilot→上游，含上游 URL、映射后的模型名、上游原始状态码），解析流式最终文本，记录耗时与 TTFB。日志可一键清空。 |
| **动态路由** | sing-box 风格的规则链（顺序求值、终结/非终结动作），配合 selector 实现**无需重启的热切换**。 |
| **计费统计** | 参考 new-api 的多级倍率模型，按客户端 / 模型 / 服务商多维统计 token 与费用。 |
| **响应缓存** | 可选的确定性请求缓存，统计命中率与节省费用。 |

## 快速开始

```bash
bun install
bun run tauri dev
```

应用启动后网关自动监听 `127.0.0.1:8787`。

### 配置一个渠道

1. 打开「渠道」页，新建渠道，填入 `base_url` 与 API Key。
2. 点「测试连通」确认能连上上游。
3. 打开「客户端接管」页，对 Claude Code 点「接管」——它会写入 `~/.claude/settings.json`。
4. 重启 Claude Code，流量就会经过 Apilot。

> 接管前可以先点「预览变更」看清要改哪些文件、改成什么样。还原靠的是首次写入前的字节级备份，所以任何时候都能回到最初的状态。

### 热切换模型 / 渠道

在「路由」页的 selector 面板里选一个渠道点切换即可 —— **新请求立刻走新渠道，不需要重启任何东西**，在途请求也仍走原渠道跑完。

要实现「按客户端或模型自动分流」，在下方添加路由规则。规则链是顺序求值的，第一个命中**终结动作**的规则胜出；非终结动作（如改写模型名）会就地生效后继续往下匹配。

## 架构

```
客户端 (Claude Code / Codex / …)
   │  base_url 已被接管到 127.0.0.1:8787
   ▼
axum 路由 ── 按路径定协议 ──▶ 请求上下文
   │
   ├─ 解码为 IR（协议无关的中间表示）
   ├─ 识别客户端来源
   ├─ 路由规则链 ──▶ 选定 selector ──▶ 选定渠道
   ├─ 查缓存 ──命中──▶ 直接用编码器合成响应
   ├─ 编码为目标渠道协议（必要时剥离无法保真的思考块）
   ├─ 发送上游 ──失败且可重试──▶ 换下一个渠道
   ├─ 响应
   │    ├─ 流式：上游 SSE → 解码器 → IR 增量 → 编码器 → 下游 SSE
   │    │        （同协议时走直通，只是旁路统计用量）
   │    └─ 非流式：解码 → 编码回客户端协议
   ├─ 计费结算（倍率 × token 数）
   └─ 落库明细 + 小时聚合 + 推送前端事件
```

### 代码结构

```
src-tauri/src/
  protocol/     协议 IR 与三个 Codec（含流式编解码器）
  gateway/      反向代理服务器、请求管线、SSE 原语
  routing/      规则链引擎、条件求值、selector 热切换
  upstream/     渠道抽象、HTTP 出站、注册表
  billing/      倍率、结算公式、预扣/结算会话
  cache/        缓存键、策略、存储与淘汰
  takeover/     客户端配置接管（保序补丁 + 原子写入）
  storage/      SQLite 与各领域读写
  traffic/      实时统计与前端事件
  commands/     Tauri 命令层
src/            前端（React 19 + Tailwind v4 + shadcn/ui）
```

更细的模块索引、请求全链路追踪与「改什么去哪里」的任务路由表见
**[docs/CODE_MAP.md](docs/CODE_MAP.md)**；给 AI 协作者用的精简导航与项目铁律见
**[CLAUDE.md](CLAUDE.md)**。

### 关键设计取舍

**单一 IR，而不是两两转换器。** 三个协议各自实现一对编解码器，任意两种协议之间的转换都走 `decode(A) → encode(B)`。N 个协议只需 2N 个编解码器，而不是 N² 个。

**能直通就不转换。** 渠道声明自己支持哪几种协议；客户端说哪种协议，命中就原样转发原始字节，没命中才走 IR 转换。转换是有损的（思考签名、`cache_control` 断点、未建模字段都会在这一步丢），直通没有这些问题。判定规则、3×3 矩阵与完整损耗清单见 [docs/PROTOCOL_MATRIX.md](docs/PROTOCOL_MATRIX.md)。

**UnifiedUsage 统一到「不含缓存的 fresh 输入」口径。** Anthropic 的 `input_tokens` 本就不含缓存，而 OpenAI / Responses 的 `prompt_tokens` **含**缓存。各 codec 负责折算到这个口径，否则缓存部分会被重复计费 —— 这是本项目最容易踩的坑，有专门的测试守着。

**规则链区分终结与非终结动作。** 借自 sing-box：非终结动作改写上下文后继续匹配，终结动作命中即停。这样能表达「先按特征改写，再决定去哪」，而不必把所有条件塞进一条巨型规则。

**接管只动「我拥有」的键。** 用户配置文件里的其他内容一律不碰。补丁写入前先解析，解析失败立即中止 —— 宁可接管失败让用户看到报错，也不能把配置文件写没。

**缓存默认关闭，且只缓存 `temperature = 0` 的请求。** 响应缓存会改变语义（同一个问题第二次问会拿到第一次的答案）。`temperature` 缺失时用的是服务商默认值，输出是随机的，这时返回旧答案会让用户以为模型卡住了。要求显式设为 0，是把「我要确定性」变成一个可检验的条件。

## 开发

```bash
# Windows：让 MSVC 工具链对 cargo 可见（本机 MSVC 来自 uv 的 msvclib 包）
source scripts/msvc-env.sh

cd src-tauri
cargo test            # 476 个测试
cargo build           # 构建二进制
```

测试覆盖到了协议转换的字段等价性、SSE 跨 chunk 的 UTF-8 边界、规则链求值顺序、计费公式、接管补丁的解析失败拒绝写入，以及**真实 HTTP 的端到端链路**（起一个假上游，跑通出站 → 流式转码 → 下游）。

### 打包安装包

```bash
bun run build          # 按当前操作系统自动选择
bun run build:win      # Windows：.msi + 安装程序 .exe
bun run build:mac      # macOS：.app + .dmg
bun run build:linux    # Linux：.deb / .rpm / .AppImage
bun run build:web      # 只构建前端（tsc && vite build）
```

产物在 `src-tauri/target/release/bundle/`，脚本跑完会把安装包路径和大小列出来。

安装包**必须在本系统上构建**，Tauri 不支持交叉打包 —— 在 macOS 上执行 `build:win` 会立刻中止并说明原因，
而不是吐出一个装不上的包。Windows 下若用 Git Bash，先 `source scripts/msvc-env.sh`，否则会在链接阶段失败。

平台判定与参数透传都在 [scripts/build.mjs](scripts/build.mjs)：`bun run build -- --debug` 这类额外参数原样转给 `tauri build`。

### 环境说明

本机有两处特殊配置，已在仓库里处理好：

- `.cargo/config.toml` 关闭了证书吊销检查（本机 schannel 访问不到吊销服务器，会让 cargo 的一切 HTTPS 请求失败）。
- `scripts/msvc-env.sh` 把 MSVC 链接器接进 PATH（Git Bash 自带的 `/usr/bin/link` 会抢占 MSVC 的 `link.exe`）。

## 数据与隐私

- 数据存放在 `~/.apilot/`（可用环境变量 `APILOT_HOME` 覆盖）。
- 捕获的请求/响应原文存在 `captures` 表，条数有上限，并且**鉴权头会被隐去后才落库**。
- 上游密钥保存在 `providers` 表，不会通过 API 回传给前端。
- 网关默认只监听 `127.0.0.1`，不对外暴露。
