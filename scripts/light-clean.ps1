# Removes build trees.
#
# WHY THIS EXISTS: deleting a build tree is the documented recovery for two failure modes that
# are otherwise unrecoverable -- a configure that ran without PICO_SDK_PATH and cached the host
# compiler, and a tree whose FONT_CRUSHER_PATH points into _deps at a fetched copy of
# font-crusher, so local crush edits silently do nothing. Both need the whole tree gone, not a
# rebuild, and doing it by hand invites deleting the wrong one.
#
# USAGE:  light-clean.ps1 [-Preset <name>] [-All] [-WhatIf]
param(
        [string]$Preset,
        [switch]$All,
        [switch]$WhatIf,
        [string]$ProjectRoot
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'lib/LightProject.psm1') -Force

$config = Get-LightProjectConfig -ProjectRoot ($ProjectRoot ? $ProjectRoot : (Get-LightProjectRoot))

$trees = @()
if ($All) {
        $trees = Get-ChildItem -Path $config.Root -Directory -Filter 'build*' | ForEach-Object { $_.FullName }
} elseif ($Preset) {
        $trees = @(Resolve-LightTree -Config $config -Preset $Preset)
} else {
        throw "specify -Preset <name> or -All"
}

if ($trees.Count -eq 0) {
        Write-Host "nothing to clean"
        return
}

foreach ($tree in $trees) {
        if (-not (Test-Path $tree)) { continue }
        $identity = Format-LightTreeIdentity (Get-LightTreeIdentity -Tree $tree)
        if ($WhatIf) {
                Write-Host "would remove $tree  [$identity]"
        } else {
                Write-Host "removing $tree  [$identity]"
                Remove-Item -Recurse -Force $tree
        }
}
