param([switch]$Test)

$ErrorActionPreference = "Stop"
$tools = Join-Path (Split-Path $PSScriptRoot -Parent) ".build-tools"
$compiler = Join-Path $tools "llvm-mingw-20261020-msvcrt-x86_64\bin"
$compat = Join-Path $tools "gcc-compat"
$cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
if (!(Test-Path -LiteralPath (Join-Path $compiler "x86_64-w64-mingw32-clang.exe")) -or !(Test-Path -LiteralPath $compat) -or !(Test-Path -LiteralPath $cargo)) {
    throw "Local LLVM-MinGW or Rust GNU toolchain not found. Install a Rust toolchain and compiler to build normally."
}
$env:PATH = "$compiler;$env:PATH"
$env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "x86_64-w64-mingw32-clang.exe"
$env:CC_x86_64_pc_windows_gnu = "x86_64-w64-mingw32-clang.exe"
$env:CARGO_ENCODED_RUSTFLAGS = "-Lnative=$compat"
if ($Test) {
    & $cargo +stable-x86_64-pc-windows-gnu test --locked
} else {
    & $cargo +stable-x86_64-pc-windows-gnu build --locked --release
}
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
