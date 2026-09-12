# Opens the light-ui editor on an app crate's UI design file. Thin wrapper over
# $LightScripts/light-edit-ui.ps1 -- the logic is shared, only the project root is local.
#
# USAGE:  scripts/edit-ui.ps1 [-Crate <app crate | path to design.json>] [-Release]
#         scripts/edit-ui.ps1              # lists the available UI designs
param(
        [string]$Crate,
        [switch]$Release
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $PSScriptRoot 'light-tools.ps1')

& (Join-Path $LightScripts 'light-edit-ui.ps1') -Crate $Crate -Release:$Release -ProjectRoot $root
