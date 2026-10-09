#!/usr/bin/env bun
// 版本升级的统一入口：改一处，四处同步，顺手把标签打上。
//
// 版本号散在四个文件里，各有各的读者，少改一个都不会当场报错，而是拖到后面才炸：
//   package.json               前端工程自己的版本
//   src-tauri/tauri.conf.json  安装包的文件名与元信息（Apilot_0.1.1_x64-setup.exe 就出在这里）
//   src-tauri/Cargo.toml       进二进制 —— 界面里显示的版本来自 env!("CARGO_PKG_VERSION")
//   src-tauri/Cargo.lock       根包在 lock 里的版本。漏掉它，下次 cargo build 会顺手改一遍
//                              工作区，于是「没动代码却是脏的」，很难归因
//
// 而 CI 的 verify job 拿 tag 与后三处逐一比对，对不上直接失败（见 .github/workflows/build-installers.yml）。
// 手工改四处总会漏一个 —— 0.1.1 那次就漏了 package.json 和 tauri.conf.json —— 所以合并成一条命令。
//
// 用法：
//   bun run upgrade                  # 只打印四处当前版本号，用来诊断不一致
//   bun run upgrade patch            # 0.1.1 → 0.1.2
//   bun run upgrade minor            # 0.1.1 → 0.2.0
//   bun run upgrade major            # 0.1.1 → 1.0.0
//   bun run upgrade 0.2.0            # 直接指定，也可以用来把四处重新对齐
//   bun run upgrade patch --no-git   # 只改文件，不提交不打标签
//   bun run upgrade patch --push     # 连提交带标签推到远端（会触发三平台打包）

import { spawnSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

// 四个版本存放点的相对路径。顺序固定，输出与提交都用它，免得每次顺序抖动。
const PKG = 'package.json';
const TAURI_CONF = 'src-tauri/tauri.conf.json';
const CARGO_TOML = 'src-tauri/Cargo.toml';
const CARGO_LOCK = 'src-tauri/Cargo.lock';

const FILES = [PKG, TAURI_CONF, CARGO_TOML, CARGO_LOCK];

// 允许预发布后缀（0.2.0-rc.1）：CI 的校验是拿 tag 去 v 后整串比对，带后缀也照样对得上。
const SEMVER = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;

function fail(message) {
  console.error(`\n[upgrade] ${message}\n`);
  process.exit(1);
}

function read(rel) {
  return readFileSync(join(ROOT, rel), 'utf8');
}

function write(rel, text) {
  writeFileSync(join(ROOT, rel), text);
}

// ---- 读取 ----

// Cargo.toml 里 `version = "..."` 这种行在别的 section 下也可能出现（`[dependencies.foo]` 就会写），
// 所以只在 [package] 段内找，而不是全文正则 —— 全文捞到的是第几个版本行，取决于依赖怎么排。
function cargoSectionField(text, section, field) {
  let current = '';
  for (const line of text.split('\n')) {
    const header = /^\s*\[([^\]]+)\]\s*$/.exec(line);
    if (header) {
      current = header[1];
      continue;
    }
    if (current !== section) continue;
    const match = new RegExp(`^${field}\\s*=\\s*"([^"]*)"`).exec(line);
    if (match) return match[1];
  }
  return undefined;
}

// Cargo.lock 里根包也是普通的一个 [[package]] 块，靠 name 定位。
function cargoLockVersion(text) {
  const match = /\[\[package\]\]\nname = "apilot"\nversion = "([^"]*)"/.exec(text);
  return match?.[1];
}

// 不 try 的话，改坏的 tauri.conf.json 会让用户看到的是一段 JS 栈回溯，
// 而不是「哪个文件、坏在哪一行」。这两个文件都常被脚本改，坏掉的概率不低。
function jsonVersion(text, rel) {
  try {
    return JSON.parse(text).version;
  } catch (error) {
    fail(`${rel} 不是合法 JSON：${error.message}`);
  }
}

function readVersions() {
  const pkg = read(PKG);
  const conf = read(TAURI_CONF);
  const toml = read(CARGO_TOML);
  const lock = read(CARGO_LOCK);

  return {
    [PKG]: jsonVersion(pkg, PKG),
    [TAURI_CONF]: jsonVersion(conf, TAURI_CONF),
    [CARGO_TOML]: cargoSectionField(toml, 'package', 'version'),
    [CARGO_LOCK]: cargoLockVersion(lock),
  };
}

// ---- 写入 ----

// 两个 JSON 都只改顶层那一个 "version"。不用 JSON.parse + stringify 重写：那会把文件
// 按 JS 的格式重新排一遍（缩进、数组换行都可能变），diff 里混进一大堆与版本无关的改动。
function setJsonVersion(text, next, rel) {
  const pattern = /(^\s*"version"\s*:\s*)"[^"]*"/m;
  if (!pattern.test(text)) fail(`${rel} 里找不到顶层的 "version" 字段。`);

  const out = text.replace(pattern, `$1"${next}"`);

  // 替换是按文本做的，写完必须解析一遍确认没改坏；顺带确认改的真是顶层那个。
  let parsed;
  try {
    parsed = JSON.parse(out);
  } catch (error) {
    fail(`${rel} 改写后不是合法 JSON：${error.message}`);
  }
  if (parsed.version !== next) fail(`${rel} 改写后顶层 version 仍是 ${parsed.version}。`);
  return out;
}

function setCargoTomlVersion(text, next) {
  const lines = text.split('\n');
  let section = '';
  for (let i = 0; i < lines.length; i++) {
    const header = /^\s*\[([^\]]+)\]\s*$/.exec(lines[i]);
    if (header) {
      section = header[1];
      continue;
    }
    if (section !== 'package') continue;
    if (/^version\s*=/.test(lines[i])) {
      lines[i] = lines[i].replace(/"[^"]*"/, `"${next}"`);
      return lines.join('\n');
    }
  }
  fail(`${CARGO_TOML} 的 [package] 段里没有 version 行。`);
}

function setCargoLockVersion(text, next) {
  const pattern = /(\[\[package\]\]\nname = "apilot"\nversion = )"[^"]*"/;
  if (!pattern.test(text)) fail(`${CARGO_LOCK} 里找不到 name = "apilot" 的包块。`);
  return text.replace(pattern, `$1"${next}"`);
}

// ---- git ----

function git(args) {
  const result = spawnSync('git', args, { cwd: ROOT, encoding: 'utf8' });
  if (result.error) fail(`无法执行 git：${result.error.message}`);
  return result;
}

function gitOk(args) {
  const result = git(args);
  if (result.status !== 0) fail(`git ${args.join(' ')} 失败：\n${result.stderr.trim()}`);
  return result.stdout.trim();
}

// 只在指定路径上找差异，好判断「这四处是否已经和 HEAD 一致」。
function hasChanges(paths) {
  return git(['diff', '--quiet', 'HEAD', '--', ...paths]).status !== 0;
}

// 磁盘上对了不代表提交里也对，而 CI 读的是提交。0.1.1 那次正是四处都改好了、却只提交了
// Cargo.toml 与 Cargo.lock 两个文件，tag 指着那个旧提交，校验才会读到 package.json=0.1.0。
// 版本号看着没问题时最该提醒的就是这一条，所以单独拎出来。
function uncommittedNote() {
  const changed = git(['diff', '--name-only', 'HEAD', '--', ...FILES]).stdout.trim();
  if (!changed) return null;
  return `这四处里有 ${changed.split('\n').length} 个文件还没提交，CI 校验读的是提交里的版本号，记得提交。`;
}

// ---- 参数 ----

const argv = process.argv.slice(2);

if (argv.includes('-h') || argv.includes('--help')) {
  console.log(`用法：bun run upgrade [patch|minor|major|<版本号>] [--no-git] [--push]`);
  process.exit(0);
}

const flags = new Set(argv.filter((arg) => arg.startsWith('--')));
const unknownFlag = [...flags].find((f) => !['--no-git', '--push', '--help'].includes(f));
if (unknownFlag) fail(`不认识的参数 ${unknownFlag}。用 --help 看用法。`);

const noGit = flags.has('--no-git');
const push = flags.has('--push');
const args = argv.filter((arg) => !arg.startsWith('--'));

if (args.length > 1) fail(`只接受一个版本参数，收到 ${args.length} 个：${args.join(' ')}`);

// ---- 当前状态 ----

const versions = readVersions();
const current = versions[PKG];
const inconsistent = Object.entries(versions).filter(([, v]) => v !== current);

// 这行格式与 CI 的 verify job 一致，方便直接比对。传进来的是**改完之后**的映射：
// 拿一开始读到的旧值去拼，会印出「tag=v0.1.2 … package.json=0.1.1」这种自相矛盾的行。
function report(map, v) {
  return `tag=v${v}  tauri.conf.json=${map[TAURI_CONF]}  Cargo.toml=${map[CARGO_TOML]}  package.json=${map[PKG]}`;
}

if (args.length === 0) {
  console.log('[upgrade] 四处当前版本：');
  for (const [rel, v] of Object.entries(versions)) {
    console.log(`  ${v ?? '(缺失)'}  ${rel}`);
  }
  if (inconsistent.length > 0) {
    console.log(`\n[upgrade] 四处不一致（以 package.json 的 ${current} 为准）。`);
    console.log('[upgrade] 用 `bun run upgrade <版本号>` 重新对齐，或 `bun run upgrade patch` 往上走一格。');
  } else {
    console.log(`\n[upgrade] 四处一致：${current}`);
  }

  const note = uncommittedNote();
  if (note) console.log(`\n[upgrade] ${note}`);
  process.exit(0);
}

// ---- 算出目标版本 ----

const spec = args[0];
let next;

if (SEMVER.test(spec)) {
  next = spec;
} else if (['major', 'minor', 'patch'].includes(spec)) {
  if (!SEMVER.test(current)) fail(`${PKG} 里的版本号 ${current} 不是合法的 semver，无法按 ${spec} 递增。`);
  const [major, minor, patch] = current.split('.').map(Number);
  // 预发布状态（0.2.0-rc.1）往上走一格时，丢掉后缀：它已经在编号上了，不必再叠一层。
  next =
    spec === 'major' ? `${major + 1}.0.0`
    : spec === 'minor' ? `${major}.${minor + 1}.0`
    : `${major}.${minor}.${patch + 1}`;
} else {
  fail(`无法理解的版本参数 ${spec}：要么是 major / minor / patch，要么是形如 0.2.0 的版本号。`);
}

const tag = `v${next}`;

if (next === current && inconsistent.length === 0) {
  console.log(`[upgrade] 四处已经是 ${current}，版本号无需改动。`);
  // 文件到位不代表标签也在：上一次中途失败、或版本号是手工改的，标签可能压根没打上。
  // 这里如实说一句，免得用户以为「命令跑过了」就万事俱备。
  if (!noGit) {
    const tagged = git(['rev-parse', '-q', '--verify', `refs/tags/${tag}`]).status === 0;
    console.log(tagged ? `[upgrade] 标签 ${tag} 已存在。` : `[upgrade] 标签 ${tag} 还没打：git tag ${tag}`);

    const note = uncommittedNote();
    if (note) console.log(`[upgrade] ${note}`);
  }
  process.exit(0);
}

if (!noGit) {
  // 标签先查重：打完才发现撞车，就得回头删标签、改回文件，比现在就停下麻烦得多。
  if (git(['rev-parse', '-q', '--verify', `refs/tags/${tag}`]).status === 0) {
    fail(`标签 ${tag} 已存在。换个版本号，或先删掉它：git tag -d ${tag}`);
  }
  const branch = gitOk(['rev-parse', '--abbrev-ref', 'HEAD']);
  if (branch === 'HEAD') fail('当前处于游离 HEAD，先切到一个分支再升级。');
}

if (inconsistent.length > 0) {
  console.log('[upgrade] 注意：四处原本就不一致，本次会一并改齐。');
}

// ---- 改文件 ----

const updates = [
  [PKG, (t) => setJsonVersion(t, next, PKG)],
  [TAURI_CONF, (t) => setJsonVersion(t, next, TAURI_CONF)],
  [CARGO_TOML, (t) => setCargoTomlVersion(t, next)],
  [CARGO_LOCK, (t) => setCargoLockVersion(t, next)],
];

for (const [rel, apply] of updates) {
  write(rel, apply(read(rel)));
  console.log(`[upgrade] ${rel}  ${versions[rel]} → ${next}`);
}

// 落盘后重新读一遍。前面每个 setter 都自检过，但那验的是「写进去的字符串」；
// 这里验的是「磁盘上确实是这样」，两者之间还隔着一次写入。
const after = readVersions();
const wrong = Object.entries(after).filter(([, v]) => v !== next);
if (wrong.length > 0) {
  fail(`写完后版本号仍不齐：${wrong.map(([rel, v]) => `${rel}(${v})`).join('  ')}。`);
}

// ---- 提交与打标签 ----

if (noGit) {
  console.log('\n[upgrade] 已跳过 git（--no-git）。别忘了提交这四处并打标签：');
  console.log(`  git commit -m "upgrade: ${next}" && git tag ${tag}`);
} else {
  // --only：只提交这四个路径。否则一旦用户自己预先 stage 了别的文件，
  // 普通 commit 会把他没写完的东西一起卷进这个版本提交里。
  if (hasChanges(FILES)) {
    gitOk(['commit', '--only', ...FILES, '-m', `upgrade: ${next}`]);
    console.log(`\n[upgrade] 已提交：upgrade: ${next}`);
  } else {
    // 正常走不到这里（版本号变了就一定有 diff，哪怕只是覆盖写回同一个值），
    // 留着是为了万一 git 认为无改动时，别让 commit 抛一句 "nothing to commit" 把人搞懵。
    console.log('\n[upgrade] 文件内容与 HEAD 一致，跳过提交。');
  }

  gitOk(['tag', tag]);
  console.log(`[upgrade] 已打标签 ${tag}`);

  if (push) {
    // 同时推分支和标签：只推标签的话，Release 页面上那个提交在远端还不存在。
    // 这一步之后 CI 会开始三平台打包，几十分钟量级。
    gitOk(['push', 'origin', 'HEAD', tag]);
    console.log(`[upgrade] 已推送 HEAD 与 ${tag}，CI 开始三平台打包。`);
    console.log('[upgrade] 构建完会挂到一个**草稿** Release，要点「发布」才对用户可见。');
  } else {
    console.log(`\n[upgrade] 还没推远端。确认无误后：git push origin HEAD ${tag}`);
    console.log('[upgrade] 推上去会触发三平台打包（CI 会先校验版本号，四处对不上就失败）。');
  }
}

console.log(`\n[upgrade] ${report(after, next)}`);
