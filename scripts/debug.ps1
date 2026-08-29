# Starts an OpenOCD server, and optionally gdb, against a built light_mk4 target.
#
# USAGE:  scripts/debug.ps1 [-Target <name>] [-ServerOnly] [-Attach] [-NoBuild]
#                          [-Ex <cmd>[,<cmd>...]] [-Batch]
#
#     scripts/debug.ps1 -Target light_mk4_pico2 -Batch                          # load over SWD and stop
#     scripts/debug.ps1 -Target light_mk4_pico2 -Batch -Ex 'monitor reset run'  # load and leave running
param(
        [string]$Target = 'light_mk4_pico2',
        [string]$Preset,
        [switch]$ServerOnly,
        [switch]$Attach,
        [switch]$NoBuild,
        [string[]]$Ex,
        [switch]$Batch
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $PSScriptRoot 'light-tools.ps1')

& (Join-Path $LightScripts 'light-debug.ps1') -Target $Target -Preset $Preset `
        -ServerOnly:$ServerOnly -Attach:$Attach -NoBuild:$NoBuild `
        -Ex $Ex -Batch:$Batch -ProjectRoot $root
