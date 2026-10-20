#!/usr/bin/env bun
// 生成 Rust 移植用的 golden fixtures。
//
// 算法从 lm-detector 的 TypeScript 原版移植，最大的风险是浮点运算顺序/特征维度
// 与 JS 不一致 —— 那种错误不会让代码崩，只会让排名悄悄偏一点。这里用原版实现
// 对固定输入算一遍，把结果写成 JSON；Rust 测试逐项比对，移植就不再靠肉眼。
//
// 依赖：../lm-detector 的 checkout（或 LM_DETECTOR_DIR）。只在开发时运行；
// 生成物提交进仓库，测试本身不依赖外部仓库。
//
// 用法：
//   bun scripts/generate-fingerprint-fixtures.mjs

import { createHash } from 'node:crypto';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SOURCE = resolve(process.env.LM_DETECTOR_DIR ?? join(ROOT, '..', 'lm-detector'));
const OUT = join(ROOT, 'src-tauri', 'src', 'fingerprint', 'fixtures');

function fail(message) {
  console.error(`\n[generate-fingerprint-fixtures] ${message}\n`);
  process.exit(1);
}

async function load(relative) {
  return import(pathToFileURL(join(SOURCE, relative)).href);
}

/** 固定种子的 LCG：fixture 要可复现，不能用 crypto。 */
function lcg(seed) {
  let state = seed >>> 0;
  return () => {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
    return state / 0x100000000;
  };
}

function randomNumbers(seed, count) {
  const next = lcg(seed);
  return Array.from({ length: count }, () => 1 + Math.floor(next() * 355));
}

function sha256(text) {
  return createHash('sha256').update(text).digest('hex');
}

async function main() {
  const core = await load('shared/fingerprint-core.js');
  const positional = await load('shared/positional.ts');
  const codec = await load('shared/detector-codec.ts');
  const detector = await load('shared/shared-detector.ts');

  const artifactBytes = await readFile(join(SOURCE, 'data', 'shared_detector.bin'));
  const artifact = codec.decodeDetector(new Uint8Array(artifactBytes));
  const rawBank = JSON.parse(await readFile(join(SOURCE, 'data', 'unified_bank.json'), 'utf8'));
  const bank = {
    schema: rawBank.schema,
    built_at: rawBank.built_at,
    models: rawBank.models.map((model) => ({
      id: model.id,
      display_name: model.display_name ?? model.id,
      family: model.family,
      family_name: model.family_name,
      response_count: model.response_count,
    })),
  };

  // ---- 解析 ----
  const parseInputs = [
    '1, 2, 3, 355, 356, 0',
    'Here are the values: 12 34 56 and then 7',
    'prefix 1 2 3 后接 4 5 6',
    '',
    '999999999999999999999999999999 7',
    '1.5 2.5 3.5',
    '1 2 3\n4 5 6',
    '０１２ 123',
  ];
  const parseCases = parseInputs.map((text) => ({
    text,
    numbers: core.parseNumbers(text),
  }));

  // ---- 特征 ----
  const featureNumbers = [
    randomNumbers(7, 300),
    core.parseNumbers('1, 2, 3, 355, 356, 0'),
  ];
  const featureCases = featureNumbers.map((numbers) => ({
    numbers,
    counts: core.countNumbers(numbers),
    hellinger: core.hellingerFeature(core.countNumbers(numbers)),
    ordered: core.orderedBlockFeature(numbers),
    // 位置头的输出同时覆盖 position kernel 与 sequence 两块特征。
    positional: positional.positionalDecision(numbers, artifact.ranker.positional),
  }));

  // ---- 完整排名 ----
  const lines = (await readFile(join(SOURCE, 'data', 'unified_reference.jsonl'), 'utf8'))
    .trim()
    .split('\n')
    .map((line) => JSON.parse(line));
  const modelIds = ['gpt-5.5', 'claude-opus-5.5', 'deepseek-v4-pro'];
  const rankingCases = modelIds.map((id) => {
    const batch = lines.find((item) => item.model.id === id);
    if (!batch) fail(`参考库里没有 ${id}`);
    const samples = batch.samples.slice(0, 3).map((sample) => ({
      text: sample.text,
      expected_count: sample.expected_count,
    }));
    if (samples.length !== 3) fail(`${id} 的参考回答不足 3 条`);
    const analysis = detector.analyzeSharedOutputs(samples, bank, artifact);
    return {
      model: id,
      outputs: samples,
      parsed: samples.map((sample) => core.parseNumbers(sample.text)),
      analysis,
    };
  });

  // ---- 序列形态门槛 ----
  const reference = rankingCases[0].outputs;
  const random = Array.from({ length: 3 }, (_, index) => ({
    text: randomNumbers(100 + index, 300).join(', '),
    expected_count: 300,
  }));
  const sequenceCases = {
    model_like: detector.analyzeSharedOutputs(reference, bank, artifact).sequence,
    programmatic: detector.analyzeSharedOutputs(random, bank, artifact).sequence,
    programmatic_decision: detector.analyzeSharedOutputs(random, bank, artifact).decision,
  };

  // ---- 本地 kNN 证据层 ----
  // 每个 label 取 9 条参考回答，查询用第一个 label 的前 3 条。这样本地测试
  // 能覆盖"≥6 条才拟合温度"和">=3 条才参与排名"两条门槛。
  const localLabels = ['gpt-5.5', 'claude-opus-5.5'].map((id) => {
    const batch = lines.find((item) => item.model.id === id);
    if (!batch) fail(`参考库里没有 ${id}`);
    return {
      label: id,
      samples: batch.samples.slice(0, 9).map((sample) => core.parseNumbers(sample.text)),
    };
  });
  const localCases = {
    labels: localLabels,
    queries: localLabels[0].samples.slice(0, 3),
  };

  await mkdir(OUT, { recursive: true });
  const files = {
    'parse_cases.json': { artifact_sha256: sha256(artifactBytes), cases: parseCases },
    'features_cases.json': { cases: featureCases },
    'ranking_cases.json': { cases: rankingCases },
    'sequence_cases.json': sequenceCases,
    'local_cases.json': localCases,
  };
  for (const [name, value] of Object.entries(files)) {
    await writeFile(join(OUT, name), JSON.stringify(value, null, 2) + '\n');
  }

  console.log(`[generate-fingerprint-fixtures] 源: ${SOURCE}`);
  console.log(`[generate-fingerprint-fixtures] 输出: ${OUT}`);
  console.log(
    `[generate-fingerprint-fixtures] 排名 top-1: ` +
      rankingCases.map((item) => `${item.model}→${item.analysis.prediction}`).join(', '),
  );
}

await main().catch((error) => fail(error instanceof Error ? error.message : String(error)));
