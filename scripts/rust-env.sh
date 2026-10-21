#!/usr/bin/env bash
# AgentKeeper Rust/Tauri 构建环境（Git Bash）
#
# 用法: source scripts/rust-env.sh
#
# 为什么需要它：
#   1) rustup 装好后 .cargo\bin 不在用户 PATH（非交互安装 + --no-modify-path）
#   2) rustc 的 msvc target 不会自己搜 VS 安装目录，不设 INCLUDE/LIB 会报
#      "linker link.exe not found"
#
# 已安装组件见 docs/RUST-ENV.md

set -e

# --- 定位 MSVC Build Tools ---
# Git Bash 没有 ${ProgramFiles(x86)} 这种带括号的变量语法（bash 变量名不允许括号），
# PortableGit 把 ProgramFiles(x86) 映射为 PROGRAMFILES，且路径通常已挂在 PATH 上。
VSWHERE="$(command -v vswhere.exe 2>/dev/null || true)"
if [ -z "$VSWHERE" ]; then
  for base in "${PROGRAMFILES:-}" "/c/Program Files (x86)" "/c/Program Files"; do
    if [ -n "$base" ] && [ -f "$base/Microsoft Visual Studio/Installer/vswhere.exe" ]; then
      VSWHERE="$base/Microsoft Visual Studio/Installer/vswhere.exe"
      break
    fi
  done
fi
if [ -z "$VSWHERE" ] || [ ! -f "$VSWHERE" ]; then
  VSWHERE="/c/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe"
fi

if [ -f "$VSWHERE" ]; then
  VS_PATH="$("$VSWHERE" -products '*' -format value -property installationPath | head -n1)"
else
  echo "rust-env: vswhere not found; falling back to default path" >&2
  VS_PATH="/c/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools"
fi

if [ ! -d "$VS_PATH/VC/Tools/MSVC" ]; then
  echo "rust-env: MSVC toolchain not found at: $VS_PATH" >&2
  echo "rust-env: see docs/RUST-ENV.md for install instructions" >&2
else
  # 取最新 MSVC 版本目录
  MSVC_VER=$(ls -1 "$VS_PATH/VC/Tools/MSVC" | sort -V | tail -n1)
  MSVC_ROOT="$VS_PATH/VC/Tools/MSVC/$MSVC_VER"

  # 取最新 Windows SDK（优先 26100，回退 22621）
  SDK_ROOT="/c/Program Files (x86)/Windows Kits/10"
  SDK_VER=""
  for cand in 10.0.26100.0 10.0.22621.0; do
    if [ -d "$SDK_ROOT/Lib/$cand" ]; then SDK_VER="$cand"; break; fi
  done

  if [ -n "$SDK_VER" ]; then
    export INCLUDE="$SDK_ROOT/Include/$SDK_VER;$SDK_ROOT/Include/$SDK_VER/ucrt;$MSVC_ROOT/include"
    export LIB="$SDK_ROOT/Lib/$SDK_VER/ucrt/x64;$SDK_ROOT/Lib/$SDK_VER/um/x64;$MSVC_ROOT/lib/x64"
    export PATH="$MSVC_ROOT/bin/Hostx64/x64:$SDK_ROOT/bin/$SDK_VER/x64:$PATH"
    export AK_SDK_VER="$SDK_VER"
  else
    echo "rust-env: Windows SDK not found under $SDK_ROOT" >&2
  fi

  export AK_MSVC_VER="$MSVC_VER"
fi

# --- Rust 工具链 ---
# $USERPROFILE 在 Git Bash 里是 Windows 风格路径（C:\Users\...），直接拼会导致
# $USERPROFILE/.cargo/bin 变成含反斜杠的混合路径，cd 失败。这里显式转 POSIX。
if [ -n "$USERPROFILE" ]; then
  CARGO_BIN="$(cygpath -u "$USERPROFILE" 2>/dev/null || echo "$USERPROFILE" | sed 's|\\|/|g')/.cargo/bin"
else
  CARGO_BIN="$HOME/.cargo/bin"
fi
if [ -d "$CARGO_BIN" ]; then
  export PATH="$CARGO_BIN:$PATH"
else
  echo "rust-env: $CARGO_BIN not found — is Rust installed?" >&2
fi

echo "rust-env ready:"
echo "  cargo   : $(command -v cargo || echo 'NOT FOUND')"
echo "  rustc   : $(command -v rustc || echo 'NOT FOUND')"
echo "  tauri   : $(command -v cargo-tauri || echo 'NOT FOUND')"
[ -n "${AK_MSVC_VER:-}" ] && echo "  MSVC    : $AK_MSVC_VER"
[ -n "${AK_SDK_VER:-}" ] && echo "  Win SDK : $AK_SDK_VER"