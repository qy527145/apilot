#!/usr/bin/env bun
// 把 lm-detector 的冻结检测器与展示元数据搬进 Apilot 的内置资产目录。
//
// 这里只搬**运行时真正需要**的东西：
//   - shared_detector.bin  → zstd 压缩后内置（4.4MB → 3.5MB）
//   - unified_bank.json    → 只保留展示字段，派生 models.json（13MB → 几十 KB）
//   - enrollment-suite.json→ 原样作为挑战库
//   - LICENSE              → MIT 声明随资产一起带上
//
// 16.5MB 的 unified_reference.jsonl 不进来：它只用于离线重建检测器，运行时不需要。
// 检测器里是 float32 数值特征，不含原始回答文本；原始回答留在上游仓库。
//
// 用法：
//   bun scripts/vendor-fingerprint.mjs
//   LM_DETECTOR_DIR=/path/to/lm-detector bun scripts/vendor-fingerprint.mjs
//
// 依赖：本机有 lm-detector 的 checkout（默认 ../lm-detector）。生成物提交进仓库；
// 更新后要重新生成 golden fixtures 并跑 `cargo test`。

import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { zstdCompressSync, constants as zlibConstants } from 'node:zlib';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SOURCE = resolve(process.env.LM_DETECTOR_DIR ?? join(ROOT, '..', 'lm-detector'));
const OUT = join(ROOT, 'src-tauri', 'assets', 'fingerprint');

function fail(message) {
  console.error(`\n[vendor-fingerprint] ${message}\n`);
  process.exit(1);
}

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

function readJson(path) {
  return readFile(path, 'utf8').then((text) => JSON.parse(text));
}

/** lm-detector 发布前会把私有网关域名换掉；派生元数据沿用同一规则。 */
function redact(value) {
  if (typeof value === 'string') {
    return value.replace(/\bmono\.[a-z0-9.-]+\b/gi, 'private.invalid');
  }
  if (Array.isArray(value)) return value.map(redact);
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, redact(item)]));
  }
  return value;
}

function gitCommit() {
  try {
    return execFileSync('git', ['-C', SOURCE, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  } catch {
    return 'unknown';
  }
}

async function main() {
  const artifactPath = join(SOURCE, 'data', 'shared_detector.bin');
  const bankPath = join(SOURCE, 'data', 'unified_bank.json');
  const suitePath = join(SOURCE, 'data', 'enrollment-suite.json');
  const licensePath = join(SOURCE, 'LICENSE');

  const [artifact, bankBytes, suiteBytes, license] = await Promise.all([
    readFile(artifactPath).catch(() => fail(`找不到检测器：${artifactPath}`)),
    readFile(bankPath).catch(() => fail(`找不到指纹库：${bankPath}`)),
    readFile(suitePath).catch(() => fail(`找不到挑战库：${suitePath}`)),
    readFile(licensePath, 'utf8').catch(() => fail(`找不到许可证：${licensePath}`)),
  ]);

  // 先看头部，确认拿到的是一份当前 schema 的检测器；完整解码由 Rust 侧负责。
  if (artifact.length < 8) fail('检测器文件太短');
  const headerLength = artifact.readUInt32LE(0);
  if (headerLength <= 0 || headerLength > artifact.length - 4) fail('检测器头部长度不合法');
  const header = JSON.parse(artifact.subarray(4, 4 + headerLength).toString('utf8'));
  if (header.schema !== 'shared-detector-v2') fail(`检测器 schema 不支持：${header.schema}`);
  if (!Array.isArray(header.model_ids) || header.model_ids.length === 0) fail('检测器没有模型身份');
  if (new Set(header.model_ids).size !== header.model_ids.length) fail('检测器模型身份重复');

  const bank = JSON.parse(bankBytes.toString('utf8'));
  if (bank.schema !== 'robust-number-fingerprint-bank') fail(`指纹库 schema 不支持：${bank.schema}`);
  if (bank.built_at !== header.bank_built_at) fail('指纹库与检测器不是同一次构建');
  if (!Array.isArray(bank.models) || bank.models.length !== header.model_ids.length) {
    fail('指纹库与检测器的模型数量不一致');
  }
  if (!bank.models.every((model, index) => model.id === header.model_ids[index])) {
    fail('指纹库与检测器的模型顺序不一致');
  }

  // 只保留展示需要的字段。counts / frequency_references 是几百个数的样本统计，
  // 运行时用不到；把它们留在 13MB 的原始库里即可。
  const models = bank.models.map((model) => ({
    id: model.id,
    display_name: model.display_name || model.id,
    family: model.family || 'other',
    family_name: model.family_name || '其他',
    response_count: model.response_count,
    valid_number_count: model.valid_number_count,
    sources: model.sources ?? {},
    conditions: model.conditions ?? {},
  }));
  const metadata = {
    schema: 'apilot-fingerprint-models-v1',
    built_at: bank.built_at,
    source_commit: gitCommit(),
    models: redact(models),
  };
  const metadataBytes = Buffer.from(JSON.stringify(metadata, null, 2) + '\n');
  const metadataText = metadataBytes.toString('utf8');

  const artifactZst = zstdCompressSync(artifact, {
    params: { [zlibConstants.ZSTD_c_compressionLevel]: 19 },
  });

  await rm(OUT, { recursive: true, force: true });
  await mkdir(OUT, { recursive: true });
  await Promise.all([
    writeFile(join(OUT, 'shared_detector.bin.zst'), artifactZst),
    writeFile(join(OUT, 'models.json'), metadataBytes),
    writeFile(join(OUT, 'challenges.json'), suiteBytes),
    writeFile(join(OUT, 'LICENSE-lm-detector.txt'), license),
  ]);

  const commit = metadata.source_commit;
  const provenance = `# 指纹参考库来源

- 上游仓库：https://github.com/Ikaleio/lm-detector
- 上游 commit：${commit}
- 生成时间：${new Date().toISOString()}
- 许可：MIT（见 \`LICENSE-lm-detector.txt\`）
- 生成脚本：\`scripts/vendor-fingerprint.mjs\`

| 文件 | 大小 | SHA-256 |
|---|---:|---|
| \`data/shared_detector.bin\`（上游原始） | ${artifact.length} | \`${sha256(artifact)}\` |
| \`shared_detector.bin.zst\`（内置） | ${artifactZst.length} | \`${sha256(artifactZst)}\` |
| \`data/unified_bank.json\`（上游原始） | ${bankBytes.length} | \`${sha256(bankBytes)}\` |
| \`models.json\`（派生展示元数据） | ${metadataBytes.length} | \`${sha256(metadataBytes)}\` |
| \`data/enrollment-suite.json\`（挑战库） | ${suiteBytes.length} | \`${sha256(suiteBytes)}\` |

## 更新步骤

1. 更新 \`../lm-detector\`（或设置 \`LM_DETECTOR_DIR\` 指向新的 checkout）。
2. 运行 \`bun scripts/vendor-fingerprint.mjs\`。
3. 运行 \`bun scripts/generate-fingerprint-fixtures.mjs\` 重新生成 golden fixtures。
4. \`cd src-tauri && cargo test\`。
5. 提交生成物与 fixtures。

\`unified_reference.jsonl\` 不随包分发：它只用于离线重建检测器，运行时不需要。
`;
  await writeFile(join(OUT, 'PROVENANCE.md'), provenance);

  console.log(`[vendor-fingerprint] 源: ${SOURCE}`);
  console.log(`[vendor-fingerprint] commit: ${commit}`);
  console.log(`[vendor-fingerprint] 检测器: ${artifact.length} B → ${artifactZst.length} B`);
  console.log(`[vendor-fingerprint] 元数据: ${models.length} 个模型，${metadataBytes.length} B`);
  console.log(`[vendor-fingerprint] 输出: ${OUT}`);
}

await main().catch((error) => fail(error instanceof Error ? error.message : String(error)));
