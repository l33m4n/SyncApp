# Dot-source this before `cargo build` if your Visual Studio install only has the
# OneCore C++ libraries (symptom: "LINK : fatal error LNK1104: cannot open file 'msvcrt.lib'").
#
#   . .\scripts\dev-env.ps1
#   cargo build --release
#
# The permanent fix is to add "MSVC v14x - C++ x64/x86 build tools" in the Visual Studio Installer.

$vsRoots = @(
    "C:\Program Files\Microsoft Visual Studio\18\Insiders",
    "C:\Program Files\Microsoft Visual Studio\18\Community",
    "C:\Program Files\Microsoft Visual Studio\2022\Community",
    "C:\Program Files\Microsoft Visual Studio\2022\BuildTools"
) | Where-Object { Test-Path "$_\VC\Tools\MSVC" }

if (-not $vsRoots) { throw "No Visual Studio C++ toolset found." }
$vsRoot = $vsRoots[0]
$vc = (Get-ChildItem "$vsRoot\VC\Tools\MSVC" | Sort-Object Name -Descending | Select-Object -First 1).FullName
$sdk = "C:\Program Files (x86)\Windows Kits\10"
$ver = (Get-ChildItem "$sdk\Lib" | Sort-Object Name -Descending | Select-Object -First 1).Name

$crt = if (Test-Path "$vc\lib\x64\msvcrt.lib") { "$vc\lib\x64" } else { "$vc\lib\onecore\x64" }

$env:LIB = "$crt;$sdk\Lib\$ver\ucrt\x64;$sdk\Lib\$ver\um\x64"
$env:VSCMD_ARG_TGT_ARCH = "x64"
$env:VCINSTALLDIR = "$vsRoot\VC\"
$env:VCToolsInstallDir = "$vc\"
$env:PATH = "$vc\bin\HostX64\x64;$env:PATH"
Write-Host "MSVC env ready: $vc (CRT libs: $crt, SDK $ver)"
