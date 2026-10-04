#!/usr/bin/env bash
# 让 MSVC 链接器与 Windows SDK 对 cargo/rustc 可见。
#
# 本机的 MSVC 工具链来自 uv 安装的 `msvclib` 包，没有走 Visual Studio 安装器，
# 因此没有全局的 vcvars 环境。而 Git Bash 的 /usr/bin/link.exe（MSYS 的 coreutils link）
# 会抢占 MSVC 的 link.exe，导致链接阶段报 "extra operand ... Try 'link --help'"。
#
# 用法：
#   source scripts/msvc-env.sh
#   cargo test

# 已初始化过就跳过，避免重复叠加 PATH。
if [ -n "${APILOT_MSVC_READY:-}" ]; then
  return 0 2>/dev/null || exit 0
fi

_msvclib_root() {
  local candidates=(
    "${MSVCLIB_ROOT:-}"
    "$HOME/AppData/Roaming/uv/tools/msvclib/Lib/site-packages/msvclib"
    "$HOME/.local/share/uv/tools/msvclib/Lib/site-packages/msvclib"
  )
  local c
  for c in "${candidates[@]}"; do
    if [ -n "$c" ] && [ -d "$c/VC/Tools/MSVC" ]; then
      printf '%s\n' "$c"
      return 0
    fi
  done
  return 1
}

_MSVCLIB=$(_msvclib_root) || {
  echo "msvc-env: 找不到 msvclib 工具链，请设置 MSVCLIB_ROOT" >&2
  return 1 2>/dev/null || exit 1
}

# 取版本号最大的那个 MSVC 目录。
_MSVC_VER=$(ls -1 "$_MSVCLIB/VC/Tools/MSVC" | sort -V | tail -1)
_SDK_VER=$(ls -1 "$_MSVCLIB/Windows Kits/10/Lib" | sort -V | tail -1)

_MSVC_BIN="$_MSVCLIB/VC/Tools/MSVC/$_MSVC_VER/bin/Hostx64/x64"
_SDK_BIN="$_MSVCLIB/Windows Kits/10/bin/$_SDK_VER/x64"
_MSVC_LIB="$_MSVCLIB/VC/Tools/MSVC/$_MSVC_VER/lib/x64"
_UCRT_LIB="$_MSVCLIB/Windows Kits/10/Lib/$_SDK_VER/ucrt/x64"
_UM_LIB="$_MSVCLIB/Windows Kits/10/Lib/$_SDK_VER/um/x64"

_MSVC_INC="$_MSVCLIB/VC/Tools/MSVC/$_MSVC_VER/include"
_UCRT_INC="$_MSVCLIB/Windows Kits/10/Include/$_SDK_VER/ucrt"
_UM_INC="$_MSVCLIB/Windows Kits/10/Include/$_SDK_VER/um"
_SHARED_INC="$_MSVCLIB/Windows Kits/10/Include/$_SDK_VER/shared"

# PATH 必须在 /usr/bin 之前，否则 MSYS 的 link.exe 会再次抢占。
export PATH="$_MSVC_BIN:$_SDK_BIN:$PATH"

# LIB / INCLUDE 交给链接器和 cl.exe，用 Windows 原生路径分隔符。
_win() { cygpath -w "$1" 2>/dev/null || printf '%s' "$1"; }

export LIB="$(_win "$_MSVC_LIB");$(_win "$_UCRT_LIB");$(_win "$_UM_LIB")"
export INCLUDE="$(_win "$_MSVC_INC");$(_win "$_UCRT_INC");$(_win "$_UM_INC");$(_win "$_SHARED_INC")"

# 校验：必须解析到 MSVC 的 link 而不是 MSYS 的。
if ! command -v link >/dev/null 2>&1; then
  echo "msvc-env: PATH 上找不到 link" >&2
fi

export APILOT_MSVC_READY=1
