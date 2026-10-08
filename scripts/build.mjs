#!/usr/bin/env bun
// 安装包构建的统一入口：判定当前系统 → 校验平台 → 跑 tauri build → 报出产物路径。
//
// 为什么不给每个平台单独写一条 `tauri build`：平台差异有四处（Windows 要先有 MSVC 环境、
// macOS 出 dmg、Linux 的 rpm 依赖外部 rpmbuild、跑错系统时 Tauri 根本打不出来）。
// 散在文档里靠人记迟早会漂移，收在这里才能保证 `build` 与 `build:win` 走的是同一条路。
//
// 用法：
//   bun run build             # 按当前操作系统自动选择
//   bun run build:win         # 显式指定；跑在别的系统上直接中止
//   bun run build -- --debug  # 额外的 tauri 参数原样透传

import { spawnSync } from 'node:child_process';
import { existsSync, readdirSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const BUNDLE_DIR = join(ROOT, 'src-tauri', 'target', 'release', 'bundle');

// 安装包的扩展名，用来从 bundle/ 里挑出成品（见 printArtifacts）。
const INSTALLER_EXT = /\.(msi|exe|dmg|deb|rpm|appimage)$/i;

// key 是命令行里用的名字，os 是 node 的 process.platform。
// Tauri 出不了跨系统的安装包（Windows 的 msi/nsis 只能在 Windows 上打，macOS 的 dmg 同理），
// 所以「显式指定平台」的用处是防呆：跑错机器时立刻停下，而不是等 cargo 抛一堆看不懂的错。
const TARGETS = {
  win: { os: 'win32', label: 'Windows', artifacts: 'msi + nsis(.exe)' },
  mac: { os: 'darwin', label: 'macOS', artifacts: 'app + dmg' },
  linux: { os: 'linux', label: 'Linux', artifacts: 'deb + rpm + AppImage' },
};

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

if (!HOST) {
  fail(`无法识别的操作系统 ${process.platform}，请直接执行 \`bun run tauri build\`。`);
}

if (requested && requested !== HOST) {
  fail(
    `当前系统是 ${TARGETS[HOST].label}，打不出 ${TARGETS[requested].label} 的安装包 —— ` +
      `Tauri 的安装包必须在本系统上构建。请在 ${TARGETS[requested].label} 机器上执行 \`bun run build:${requested}\`。`,
  );
}

const target = TARGETS[HOST];
console.log(`[build] 目标平台：${target.label}（产物：${target.artifacts}）`);
console.log(`[build] 产物目录：${BUNDLE_DIR}`);

// Git Bash 的 /usr/bin/link 会抢占 MSVC 的 link.exe，问题要等到链接阶段才暴露且报错费解。
// 只在 MSYS 环境下提醒（MSYSTEM 由 Git Bash 注入）：PowerShell/cmd 里没有这个冲突。
if (HOST === 'win' && process.env.MSYSTEM && !process.env.APILOT_MSVC_READY) {
  console.warn('[build] 警告：Git Bash 未加载 MSVC 环境，链接阶段可能失败。先执行 `source scripts/msvc-env.sh`。');
}

const result = spawnSync('bun', ['run', 'tauri', 'build', ...argv], {
  cwd: ROOT,
  stdio: 'inherit',
});

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
