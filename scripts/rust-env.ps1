# AgentKeeper Rust/Tauri 构建环境（PowerShell）
#
# 用法（在仓库根目录执行）:
#   .\scripts\rust-env.ps1
#
# 或 dot-source 以在当前会话保留变量:
#   . .\scripts\rust-env.ps1
#
# 为什么需要它：
#   1) rustup 装好后 .cargo\bin 不在用户 PATH（非交互安装 + --no-modify-path）
#   2) rustc 的 msvc target 不会自己搜 VS 安装目录，不设 INCLUDE/LIB 会报
#      "linker link.exe not found"
#
# 已安装组件见 docs/RUST-ENV.md

$ErrorActionPreference = 'Stop'

# --- 定位 MSVC Build Tools ---
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (Test-Path $vswhere) {
    $vsPath = & $vswhere -products '*' -format value -property installationPath | Select-Object -First 1
} else {
    Write-Warning "vswhere not found, falling back to default path"
    $vsPath = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\2022\BuildTools"
}

if (Test-Path (Join-Path $vsPath 'VC\Tools\MSVC')) {
    $msvcRoot = Join-Path $vsPath 'VC\Tools\MSVC'
    # 按版本号数值排序，不能用 Sort-Object Name（字符串排序会把 14.44 排在 14.9 之后）。
    # 机器上可能并存多个 MSVC 版本，取真正最新的那个。
    $msvcVer = (Get-ChildItem $msvcRoot -Directory |
                Where-Object { $_.Name -match '^(\d+)\.(\d+)\.(\d+)' } |
                ForEach-Object {
                    [pscustomobject]@{
                        Name  = $_.Name
                        Score = [int]$matches[1] * 10000 + [int]$matches[2] * 100 + [int]$matches[3]
                    }
                } |
                Sort-Object Score | Select-Object -Last 1).Name
    $msvcDir = Join-Path $msvcRoot $msvcVer

    # 取最新 Windows SDK（优先 26100，回退 22621）
    $sdkRoot = "${env:ProgramFiles(x86)}\Windows Kits\10"
    $sdkVer = $null
    foreach ($cand in @('10.0.26100.0', '10.0.22621.0')) {
        if (Test-Path (Join-Path $sdkRoot "Lib\$cand")) { $sdkVer = $cand; break }
    }

    if ($sdkVer) {
        $env:INCLUDE = "$sdkRoot\Include\$sdkVer;$sdkRoot\Include\$sdkVer\ucrt;" + (Join-Path $msvcDir 'include')
        $env:LIB = "$sdkRoot\Lib\$sdkVer\ucrt\x64;$sdkRoot\Lib\$sdkVer\um\x64;" + (Join-Path $msvcDir 'lib\x64')
        $env:PATH = "$(Join-Path $msvcDir 'bin\Hostx64\x64');$sdkRoot\bin\$sdkVer\x64;" + $env:PATH
        $env:AK_SDK_VER = $sdkVer
    } else {
        Write-Warning "Windows SDK not found under $sdkRoot"
    }

    $env:AK_MSVC_VER = $msvcVer
} else {
    Write-Warning "MSVC toolchain not found at: $vsPath"
    Write-Warning "see docs/RUST-ENV.md for install instructions"
}

# --- Rust 工具链 ---
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if (Test-Path $cargoBin) {
    $env:PATH = "$cargoBin;" + $env:PATH
} else {
    Write-Warning "$cargoBin not found - is Rust installed?"
}

Write-Host "rust-env ready:"
Write-Host "  cargo   : $((Get-Command cargo -ErrorAction SilentlyContinue).Source)"
Write-Host "  rustc   : $((Get-Command rustc -ErrorAction SilentlyContinue).Source)"
Write-Host "  tauri   : $((Get-Command cargo-tauri -ErrorAction SilentlyContinue).Source)"
if ($env:AK_MSVC_VER)  { Write-Host "  MSVC    : $($env:AK_MSVC_VER)" }
if ($env:AK_SDK_VER)   { Write-Host "  Win SDK : $($env:AK_SDK_VER)" }