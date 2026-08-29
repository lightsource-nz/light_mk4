# Locates the shared script layer, setting $LightScripts for the wrappers in this directory.
#
# WHY THIS EXISTS: the wrappers reach the framework through LIGHT_PATH, which is the convention
# CMake already uses. But a fresh shell may not have it set, and failing with "cannot find
# light-build.ps1" would send you looking in the wrong place -- so fall back to the sibling
# checkout, which is the same default CMakeLists.txt uses, and say clearly when neither works.
#
# THIS PROJECT'S ADDITION: cargo. rustup installs to ~/.cargo/bin and does not modify PATH, and
# light-env.ps1 knows nothing about Rust; Corrosion inside the CMake configure needs to find it.
# Putting it on PATH here, where the wrappers already establish the toolchain, keeps the shared
# layer untouched for the C projects.
$ErrorActionPreference = 'Stop'

$candidate = if ($env:LIGHT_PATH) {
        $env:LIGHT_PATH
} else {
        # forward slashes: PowerShell accepts them on Windows, while a backslash on Linux is an
        # ordinary filename character, so '..\..\x' there names one file that does not exist
        Join-Path $PSScriptRoot '../../light_framework_mk3'
}

if (-not (Test-Path (Join-Path $candidate 'scripts/light-build.ps1'))) {
        throw "cannot find the light framework scripts. Set LIGHT_PATH to the light_framework_mk3 checkout (tried '$candidate')."
}

$LightScripts = (Resolve-Path (Join-Path $candidate 'scripts')).Path

$cargoBin = Join-Path $HOME '.cargo/bin'
if (Test-Path $cargoBin) {
        $sep = [System.IO.Path]::PathSeparator
        $native = (Resolve-Path $cargoBin).Path
        if (($env:PATH -split [regex]::Escape($sep)) -notcontains $native) {
                $env:PATH = "$native$sep$env:PATH"
        }
} else {
        Write-Warning "rustup is not installed (no $cargoBin); the firmware configure will fail at Corrosion"
}
