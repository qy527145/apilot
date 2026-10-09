#!/usr/bin/env bun
// 安装包构建的统一入口：判定「目标平台」→ 选打包方式（本机编还是交叉编）→ 跑 tauri build → 报出产物。
//
// 命令名指的是**目标平台**，不是「必须在哪个系统上跑」：同一个 `build:win`，在 Windows 上是
// 本机编译出 msi + nsis，在 macOS / Linux 上则走 cargo-xwin 交叉编出 nsis。方式由当前系统决定，
// 用户只需要说要哪个平台的包。
//
// 交叉编译的能力边界是硬的，不是偷懒：
//   → Windows：macOS / Linux 可行（cargo-xwin 拉 MSVC 运行库 + NSIS 组装），但**只能出 nsis**，
//              msi 依赖的 WiX 是 Windows 独占的；
//   → macOS：   只有 macOS 能做（.app / .dmg 靠系统自带的 hdiutil、codesign、iconutil）；
//   → Linux：   deb / rpm / AppImage 的工具链同样是 Linux 独占的。
// 越界的方向在这里直接中止并说明原因 —— 让 cargo 去报错的话，用户看到的是一堆与平台无关的错误。
//
// 用法：
//   bun run build             # 按当前操作系统自动选择
//   bun run build:win         # Windows 安装包（在 macOS 上会自动交叉编）
//   bun run build:mac         # macOS 安装包
//   bun run build:linux       # Linux 安装包
//   bun run build -- --debug  # 额外的 tauri 参数原样透传

import { spawnSync } from 'node:child_process';
import { existsSync, readdirSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

// 安装包的扩展名，用来从 bundle/ 里挑出成品（见 printArtifacts）。
const INSTALLER_EXT = /\.(msi|exe|dmg|deb|rpm|appimage)$/i;

const OS_LABEL = { win32: 'Windows', darwin: 'macOS', linux: 'Linux' };

const TARGETS = {
  win: {
    os: 'win32',
    triple: 'x86_64-pc-windows-msvc',
    label: 'Windows',
    native: 'msi + nsis(.exe)',
    cross: 'nsis(.exe)',
  },
  mac: { os: 'darwin', label: 'macOS', native: 'app + dmg' },
  linux: { os: 'linux', label: 'Linux', native: 'deb + rpm + AppImage' },
};

// 每个平台能在哪些系统上被造出来。没列进来的组合做不到。
const CAN_BUILD = { win: ['win32', 'darwin', 'linux'], mac: ['darwin'], linux: ['linux'] };

const HOST = Object.entries(TARGETS).find(([, t]) => t.os === process.platform)?.[0];

const argv = process.argv.slice(2);

if (argv[0] === '-h' || argv[0] === '--help') {
  console.log(`用法：bun run build [${Object.keys(TARGETS).join('|')}] [tauri 参数...]`);
  process.exit(0);
}

// 第一个参数是平台名就当作显式指定，其余参数一律透传（允许 `build:mac --debug`）。
const requested = Object.hasOwn(TARGETS, argv[0] ?? '') ? argv.shift() : undefined;

function fail(message) {
  console.error(`\n[build] ${message}\n`);
  process.exit(1);
}

function hasCommand(bin, env = process.env) {
  return spawnSync('sh', ['-c', `command -v ${bin}`], { stdio: 'ignore', env }).status === 0;
}

// brew 装的 LLVM 只在 Cellar 里，clang-cl / llvm-rc 不在默认 PATH 上，而 cargo-xwin 是
// **按名字**找它们的。这里补一次 PATH，免得用户每开一个 shell 都要先 export 一遍。
// 返回 env 而不是就地改 process.env：这份修改只该作用于本次构建。
function withLlvmOnPath(env) {
  if (['clang-cl', 'llvm-rc'].every((bin) => hasCommand(bin, env))) return env;

  const candidates = [];
  if (process.platform === 'darwin') {
    const brew = spawnSync('brew', ['--prefix', 'llvm'], { encoding: 'utf8' });
    if (brew.status === 0) candidates.push(join(brew.stdout.trim(), 'bin'));
    candidates.push('/opt/homebrew/opt/llvm/bin', '/usr/local/opt/llvm/bin');
  }
  candidates.push('/usr/lib/llvm/bin');

  const dir = candidates.find((d) => d && existsSync(join(d, 'clang-cl')));
  if (!dir) return env;

  console.log(`[build] 临时把 ${dir} 加进 PATH（不补的话 cargo-xwin 找不到 clang-cl / llvm-rc）。`);
  return { ...env, PATH: `${dir}:${env.PATH ?? ''}` };
}

if (!HOST) {
  fail(`无法识别的操作系统 ${process.platform}，请直接执行 \`bun run tauri build\`。`);
}

const key = requested ?? HOST;
const target = TARGETS[key];
const cross = target.os !== process.platform;

if (!CAN_BUILD[key].includes(process.platform)) {
  const allowed = CAN_BUILD[key].map((os) => OS_LABEL[os]).join(' / ');
  fail(
    `打不出 ${target.label} 的安装包：${target.label} 的打包工具链是它自己系统独占的，` +
      `只能在 ${allowed} 上构建。三个方向里只有「→ Windows」可以交叉。`,
  );
}

// 用户自己写了 --target 就用他的，我们只在没写时补默认三元组。
const targetFlag = argv.indexOf('--target');
const userTarget = targetFlag === -1 ? undefined : argv[targetFlag + 1];
const userBundles = argv.includes('--bundles');

const args = ['run', 'tauri', 'build'];
let env = process.env;

if (cross) {
  env = withLlvmOnPath(env);

  // cargo-xwin 用 clang-cl 当编译器、lld-link 当链接器，MSVC 的 CRT 与 Windows SDK
  // 由它自己从 nuget 拉（缓存在 ~/Library/Caches/cargo-xwin）。这几样缺一个，报错都会
  // 落在编译中途，所以先在这里点明缺什么、怎么装。
  for (const [bin, hint] of [
    ['cargo-xwin', 'cargo install --locked cargo-xwin'],
    ['clang-cl', 'brew install llvm（脚本会尝试自动找到它，找不到才报这个）'],
    ['lld-link', 'brew install llvm'],
    ['llvm-rc', 'brew install llvm'],
  ]) {
    if (!hasCommand(bin, env)) fail(`交叉编译缺少 ${bin}：${hint}。`);
  }

  args.push('--runner', 'cargo-xwin', '--target', userTarget ?? target.triple);

  // msi 要在 Windows 上跑 WiX，交叉时给不了，所以显式只要 nsis —— 否则 tauri 会照
  // tauri.conf.json 的 targets:"all" 去试 msi，然后以一个和 msi 有关的错失败。
  if (!userBundles) args.push('--bundles', 'nsis');

  console.log(`[build] 交叉编译：${OS_LABEL[process.platform]} → ${target.label}`);
  console.log(`[build] 产物：${target.cross}（msi 只能在 Windows 上构建，WiX 是它独占的）`);
} else {
  console.log(`[build] 目标平台：${target.label}（产物：${target.native}）`);
}

// 交叉构建的产物在 target/<三元组>/release 下，与本机构建不共用目录。
const triple = cross ? (userTarget ?? target.triple) : userTarget;
const BUNDLE_DIR = join(
  ROOT,
  'src-tauri',
  'target',
  ...(triple ? [triple] : []),
  'release',
  'bundle',
);

console.log(`[build] 产物目录：${BUNDLE_DIR}`);

// Git Bash 的 /usr/bin/link 会抢占 MSVC 的 link.exe，问题要等到链接阶段才暴露且报错费解。
// 只在 MSYS 环境下提醒（MSYSTEM 由 Git Bash 注入）：PowerShell/cmd 里没有这个冲突。
if (HOST === 'win' && process.env.MSYSTEM && !process.env.APILOT_MSVC_READY) {
  console.warn('[build] 警告：Git Bash 未加载 MSVC 环境，链接阶段可能失败。先执行 `source scripts/msvc-env.sh`。');
}

const result = spawnSync('bun', [...args, ...argv], { cwd: ROOT, stdio: 'inherit', env });

if (result.error) fail(`无法启动 tauri：${result.error.message}`);
if (result.status !== 0) fail(`打包失败（退出码 ${result.status}）。`);

printArtifacts();

// bundle/ 下面既有成品也有中间产物（macOS 的 .app 目录里能翻出成千上万个文件），
// 所以按扩展名筛，并且只往下走两层。
function collectFiles(dir, depth) {
  const files = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (depth > 0) files.push(...collectFiles(full, depth - 1));
    } else {
      files.push(full);
    }
  }
  return files;
}

function printArtifacts() {
  const installers = existsSync(BUNDLE_DIR)
    ? collectFiles(BUNDLE_DIR, 2).filter((f) => INSTALLER_EXT.test(f))
    : [];

  if (installers.length === 0) {
    console.log(`\n[build] 打包完成，但未在 ${BUNDLE_DIR} 找到安装包文件。`);
    return;
  }

  console.log('\n[build] 安装包：');
  for (const file of installers.sort()) {
    const mb = statSync(file).size / 1024 / 1024;
    console.log(`  ${relative(ROOT, file)}  ${mb.toFixed(1)} MB`);
  }
}
