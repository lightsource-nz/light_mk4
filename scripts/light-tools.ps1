# Sets $LightScripts for the wrappers in this directory, and puts cargo on PATH.
#
# The framework IS this repository: the shared light-*.ps1 layer lives right here in scripts/,
# so the wrappers call their siblings directly -- no LIGHT_PATH lookup, no framework checkout to
# find. (light-env.ps1, dot-sourced by those shared scripts, sets LIGHT_PATH to this repo root
# for CMake, since the framework root is the parent of scripts/.)
#
# THE cargo ADDITION: rustup installs to ~/.cargo/bin and does not modify PATH, and light-env.ps1
# knows nothing about Rust; Corrosion inside the CMake configure needs to find it. Putting it on
# PATH here, where the wrappers already establish the toolchain, keeps the shared layer general.
$ErrorActionPreference = 'Stop'

$LightScripts = $PSScriptRoot

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
