# 指纹模型检测

Apilot 的「指纹检测」回答一个模型测试回答不了的问题：**这个渠道实际调用的模型
是不是它声称的那个**。

算法移植自 [lm-detector](https://github.com/Ikaleio/lm-detector)（MIT）：让模型凭
第一反应写约 300 个 1–355 的整数，把这组分布与冻结参考库比对，给出最接近的候选。
数字与语义无关，因此中转站很难通过改提示词伪装；但结果是**参考库闭集内的相对
排名**，不是模型身份的证明。

## 两级参考库

| 级别 | 位置 | 谁维护 | 作用 |
|---|---|---|---|
| 远端/内置参考库 | 内置 `src-tauri/assets/fingerprint/`；远端 `~/.apilot/fingerprint/remote/` | 上游项目 + 用户点「检查更新」 | 官方冻结检测器，输出 60 个身份上的校准概率 |
| 本地样本库 | SQLite `fingerprint_samples` | 用户 | 在渠道检测后，把有效回答以**用户确认的权威模型**加入；作为独立 kNN 证据层 |

**本地样本不修改官方检测器。** 两者使用同一套 429 维 `full` 特征，但本地层单独
做最近邻、单独拟合温度；官方概率的校准绑定始终有效。

### 本地样本的门槛

- 每类 **≥3 条**样本才参与本地排名；
- 每类 **≥6 条**样本才进入本地概率集合（通常需要两轮检测，每轮 3 条）；
- 至少两个 label 达到 6 条，且 leave-one-out 的 top-1 高于随机、NLL 低于均匀
  分布，才显示本地概率；否则只显示距离比和排名。

`distance_ratio = 查询回答到该 label 的最近邻距离 / 该 label 自身的
leave-one-out 离散度`。比值约 1 表示"和这个 label 自己的样本一样近"。

## 检测流程

```
detect_model_fingerprint
  → 挑战：默认从 36 条里随机抽 3 条，也可以手动指定 3 条
  → 默认并行发送 3 条请求（可切串行，避免限流）
       每条开始/完成都推 apilot://fingerprint 事件
  → oneshot::send（不写日志、不计费、不查缓存）
  → parse_numbers → 有效数字门槛 max(80, ceil(expected_count × 0.55))
  → 序列形态门槛（lag-1 自相关 ≤ -0.23，小步占比 ≤ 0.05）
  → 官方 ranker（LDA + kNN + nuisance baseline + 位置头）→ 温度 softmax
  → 本地 kNN 证据层（≥3 条/类）
  → 结论层 + 落库 fingerprint_detections
```

## 挑战、进度与检测记录

- **随机（默认）**：每次从 36 条固定挑战中抽 3 条；同一模型多测几轮会用不同
  提示词，比固定三条更稳。
- **手动**：在对话框里从完整挑战库挑 3 条，能直接看到每条提示词与期望数字。
- **进度**：`apilot://fingerprint` 事件按挑战推送 `requesting` / `answered`；
  并行模式下谁先回来就先更新哪张卡片，卡片里有提示词、解析数字/门槛、耗时、
  线协议、URL、错误与回包预览。
- **单条重试**：结果里每条挑战都有「重试这条」——只重跑这一条、保留其余两条并
  重新计算排名，无论上一条是失败还是成功都可以重试。重试结果覆盖原检测记录。
- **占比***：官方排名表额外给出一列对学生全部候选分数做 `softmax(τ=1)` 的相对
  占比，方便在"部分有效、校准不可用"时仍能直观比较。它不是校准概率；三条有效
  回答时以「置信度」列为准。
- **检测记录**：每次检测落一条 `fingerprint_detections`，完整结果（含原始回答）
  在 `result_json`。在「指纹样本 → 检测记录」里可以回看、删除，或从历史结果
  继续加入本地样本库。关闭对话框不会取消后端请求，但结果会出现在这里。

状态：

- `ok`：3 条有效，官方排名 + 校准概率；
- `partial`：1–2 条有效，只有排名、没有概率；
- `unscorable`：没有有效回答；
- `not_model`：数字的相邻变化像程序生成/排序/打乱，拒绝排名，也禁止加入样本库。

## 批量检测

在渠道管理里展开一个渠道，点「批量指纹检测」：

- 默认全选该渠道声明的模型，可搜索、逐个勾选或清空；
- **模型之间按「模型并发」并行**（默认 3，可选 1–10）；每个模型内部仍可按
  开关并行/串行发送 3 条挑战；
- 峰值并发 = 模型并发 ×（单模型并行 ? 3 : 1），界面会实时显示；遇到限流就
  调小模型并发或关掉单模型并行；
- **随机模式下每个模型各自抽 3 条**；想用同一组挑战横向对比，切「手动选择」，
  整批共用这 3 条；
- 运行中每个模型一行：等待 / 检测中（展开 3 条挑战的实时状态与提示词）/
  完成（官方 Top + 置信度或占比、本地 Top、结论）/ 失败（行内错误 + 单独重试）；
- 每行「查看详情」展开完整结果，可以直接加入本地样本库；「重测」只重跑该模型；
- 每个模型的检测都会写入检测记录，批量结束后可在「检测记录」统一回看。

当前批量范围是**一个渠道内**的模型；跨渠道批量暂未实现。

## 置信度口径

- **官方置信度**：参考库内 60 个身份的闭集温度 softmax。库外模型也会得到一个
  "最像"的候选；概率不是身份证明。
- **本地置信度**：本地 label 闭集内的近似概率，只在上述门槛通过时显示。它与
  官方概率的候选集、校准方法和失败模式都不同。
- **不合成单一总概率**。结论卡只给：
  - `agreement`：官方与本地 Top-1 一致；
  - `conflict`：不一致；本地强、官方弱时参考结论给本地，否则给官方并标注分歧；
  - `official`：本地证据不足；
  - `local_only`：官方侧无法评分，只有本地样本；
  - `insufficient`：两边都没有可用结果。

声明模型如果在官方库里，结果会单独显示它的排名与概率。

## 数据资产与更新

内置（`src-tauri/assets/fingerprint/`）：

| 文件 | 说明 |
|---|---|
| `shared_detector.bin.zst` | 冻结检测器，zstd-19，约 3.5 MB；运行时解压 |
| `models.json` | 从 `unified_bank.json` 派生的展示元数据 |
| `challenges.json` | 36 条固定挑战 |
| `LICENSE-lm-detector.txt` / `PROVENANCE.md` | MIT 声明与来源/哈希/更新步骤 |

更新：

```bash
bun run fingerprint:vendor      # 从 ../lm-detector（或 LM_DETECTOR_DIR）重新生成内置资产
bun run fingerprint:fixtures    # 重新生成 golden fixtures
source scripts/msvc-env.sh && cd src-tauri && cargo test
```

运行时的「检查更新」会从
`https://raw.githubusercontent.com/Ikaleio/lm-detector/main/data` 下载检测器与
`unified_bank.json`（约 17 MB），校验 `built_at`、模型 ID、样本数与结构后原子替换。
失败保留当前库；「恢复内置」停用远端目录。下载没有上游签名，页面会显示 SHA-256，
方便手工核对。

## 限制

- **闭集**：参考库快照的 60 个身份之外，结果只能"最像"。
- **本地标注由用户负责**：错误 label 会污染本地层；UI 要求显式确认，并支持删除。
- **渠道不一定配合**：拒绝、写代码、用工具、思考吃满额度、强制 SSE 都会让某条
  回答无效；`oneshot` 目前只处理缓冲响应，强制流式的渠道会报"拿不到文本"。
- **成本**：3 次真实请求，每次最多 8192 output token；默认并行，可在对话框切串行。
  检测前必须确认。
- **数据漂移**：上游库会更新，模型版本替换也可能让旧参考库排名下降；结果里
  记录了参考库 `built_at` 与 SHA-256。

## 代码位置

- 算法：`src-tauri/src/fingerprint/`（`codec` → `parse/features` → `positional` →
  `ranker` → `analyze`；本地层 `local.rs`；引擎/远端 `mod.rs`）
- 存储：`src-tauri/src/storage/fingerprint.rs`（v17 两张表）
- 命令：`src-tauri/src/commands/fingerprint.rs`
- 前端：`src/pages/SamplesPage.tsx`（远端 / 本地 / 检测记录三个页签）、
  `FingerprintDialog.tsx`（检测确认、并行开关、挑战选择、实时进度）、
  `FingerprintResultView.tsx`（结果与加入样本库）、
  `FingerprintHistoryDialog.tsx`（历史记录详情）
- 许可见根目录 `THIRD_PARTY_NOTICES.md`
