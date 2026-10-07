# 这份 Codex 资产是哪来的

两个文件都是 **OpenAI Codex**（<https://github.com/openai/codex>，Apache-2.0）里的原文，
逐字节拷过来，没有改动：

| 文件 | 在 Codex 仓库里的位置 |
|---|---|
| `models.json` | `codex-rs/models-manager/models.json` |
| `prompt.md` | `codex-rs/models-manager/prompt.md` |

快照取自 Codex **0.160.x 时代的 `main`**（2026-10-07 的 `95ec468`）。

## 为什么要拷进来

见 `src-tauri/src/codex/mod.rs` 的模块说明：Codex 用不用 Responses Lite 只由模型元数据
决定，网关必须把这份目录接管过来才能把它关掉。而且这份目录是**整体替换**客户端内置那
一份的（设了 `model_catalog_url` 之后 Codex 的 `catalog_source` 变成 `ExplicitProvider`，
不再和自己的内置目录合并），所以每一条都得带上，包括我们不改的那些。

`prompt.md` 一起拷，是因为元数据条目里的 `instructions_template` 是必填的 —— Codex
反序列化时会校验「`base_instructions` 和 `model_messages.instructions_template` 不能都
没有」，缺了**客户端直接起不来**。原本被 Lite 关住的那批条目，其提示词是为 code mode
写的（在教模型调 `functions.exec`），关掉 code mode 后就用这份通用的替换。

## 更新它

Codex 改这份目录时（加新模型、加新字段），可以整份换掉：

```bash
cp <codex>/codex-rs/models-manager/models.json src-tauri/assets/codex/models.json
cp <codex>/codex-rs/models-manager/prompt.md  src-tauri/assets/codex/prompt.md
cd src-tauri && cargo test codex::
```

`src/codex/mod.rs` 的测试会兜住最常见的那几种坏法：漏掉 slug、提示词变空、Lite 标志没
关干净。

**注意版本方向**：这份快照比客户端新一点是安全的（新字段对老客户端是未知字段，会被
忽略）；反过来不行 —— 老快照可能缺新客户端**必填**的字段，那会让客户端拉不到目录。
拉不到只是退回它自己的兜底元数据（能用，只是没有 `apply_patch`），不会坏，但能力会降。
理想情况下，客户端升级后跟着刷一次。

## 许可证

Codex 以 Apache-2.0 发布，这两个文件是它的原文。按 Apache-2.0 的要求把它的 NOTICE
一并带上（原文照抄，见 `NOTICE`）：

```
OpenAI Codex
Copyright 2025 OpenAI

This project includes code derived from [Ratatui](https://github.com/ratatui/ratatui), licensed under the MIT license.
Copyright (c) 2016-2022 Florian Dehau
Copyright (c) 2023-2025 The Ratatui Developers
```
