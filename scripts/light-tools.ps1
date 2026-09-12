# Sets $LightScripts for the wrappers in this directory.
#
# The framework IS this repository: the shared light-*.ps1 layer lives right here in scripts/,
# so the wrappers call their siblings directly -- no LIGHT_PATH lookup, no framework checkout to
# find. (light-env.ps1, dot-sourced by those shared scripts, sets LIGHT_PATH to this repo root
# for CMake, since the framework root is the parent of scripts/.)
#
# cargo is put on PATH by light-env.ps1, which the shared scripts dot-source -- so a shared script
# run directly finds it too, not only one reached through a wrapper.
$ErrorActionPreference = 'Stop'

$LightScripts = $PSScriptRoot
