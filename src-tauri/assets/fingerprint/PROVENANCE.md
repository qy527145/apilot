# 指纹参考库来源

- 上游仓库：https://github.com/Ikaleio/lm-detector
- 上游 commit：70410db4522387c7d4a6dff8c7a4b6ce0f0239f1
- 生成时间：2026-10-20T13:35:29.842Z
- 许可：MIT（见 `LICENSE-lm-detector.txt`）
- 生成脚本：`scripts/vendor-fingerprint.mjs`

| 文件 | 大小 | SHA-256 |
|---|---:|---|
| `data/shared_detector.bin`（上游原始） | 4385348 | `9117330f225a525d39d75db2616e90657e6b5d9ea1af46770389530de0a418fe` |
| `shared_detector.bin.zst`（内置） | 3533897 | `1b451ca4a5b0cd7f2b04c39bfeecf9affbd13c9312fc51406971764859e0c052` |
| `data/unified_bank.json`（上游原始） | 13013638 | `408aa7efe4e677b0dd5dd1d7eed5b6d7cf435bb9f5e85fd1cf984b4ff051d0ae` |
| `models.json`（派生展示元数据） | 41073 | `3329ce88c06ded46c69a08b3203584a22857d8783eebcb605c38611da1b6b17d` |
| `data/enrollment-suite.json`（挑战库） | 152640 | `fb9eabbf1c36048b94ad0507675bbc8f7d34f5b07a450f4d156cf27e4d6d655a` |

## 更新步骤

1. 更新 `../lm-detector`（或设置 `LM_DETECTOR_DIR` 指向新的 checkout）。
2. 运行 `bun scripts/vendor-fingerprint.mjs`。
3. 运行 `bun scripts/generate-fingerprint-fixtures.mjs` 重新生成 golden fixtures。
4. `cd src-tauri && cargo test`。
5. 提交生成物与 fixtures。

`unified_reference.jsonl` 不随包分发：它只用于离线重建检测器，运行时不需要。
