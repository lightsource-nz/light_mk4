# Opens the light-ui editor on an application crate's UI design file.
#
# WHY THIS EXISTS: a UI is data -- an app crate carries a design.json that crush compiles to an
# LUI blob at build time. Editing that design should be one command from the crate's name, the
# same door build.ps1/flash.ps1 give firmware, instead of remembering where the editor binary
# lands and which JSON feeds which app. This builds tools/light-ui-editor and opens the crate's
# design; the editor saves the JSON back in place, and the ordinary build recompiles the blob (no
# sidecar blob is written into the source tree).
#
# The design is found as <crate>/design.json under crates/ then module/, or a path to a .json may
# be passed directly. With no -Crate, the available designs are listed.
#
# USAGE:  light-edit-ui.ps1 [-Crate <app crate | path to design.json>] [-Release] [-ProjectRoot <dir>]
param(
        [string]$Crate,
        [switch]$Release,
        [string]$ProjectRoot
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'lib/LightProject.psm1') -Force
. (Join-Path $PSScriptRoot 'light-env.ps1') -Quiet

$root = $ProjectRoot ? $ProjectRoot : (Get-LightProjectRoot)

#   every design.json under crates/ or module/ -- the discoverable set, so the list stays true
# without a table to maintain
function Get-LightUiDesigns {
        Get-ChildItem -Path (Join-Path $root 'crates'), (Join-Path $root 'module') -Filter design.json -Recurse -File -ErrorAction SilentlyContinue |
                Sort-Object FullName
}

if (-not $Crate) {
        Write-Host "usage: $($MyInvocation.MyCommand.Name) -Crate <app crate>   (a crate under crates/ or module/ with a design.json)"
        $designs = Get-LightUiDesigns
        if ($designs) {
                Write-Host "available UI designs:"
                foreach ($d in $designs) { Write-Host "  $($d.Directory.Name)" }
        } else {
                Write-Host "no design.json files found under crates/ or module/"
        }
        return
}

#   an explicit .json path, else <crate>/design.json under crates/ then module/
$design = $null
if ($Crate -match '\.json$' -and (Test-Path $Crate)) {
        $design = (Resolve-Path $Crate).Path
} else {
        foreach ($rel in @("crates/$Crate/design.json", "module/$Crate/design.json")) {
                $cand = Join-Path $root $rel
                if (Test-Path $cand) { $design = (Resolve-Path $cand).Path; break }
        }
}
if (-not $design) {
        $names = (Get-LightUiDesigns | ForEach-Object { $_.Directory.Name }) -join ', '
        throw "no UI design for '$Crate' (looked for crates/$Crate/design.json and module/$Crate/design.json). Available: $names"
}

#   a running editor holds a lock on its own binary; replacing it mid-build fails, so end it first
Get-Process -Name light-ui-editor -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

$buildProfile = $Release ? 'release' : 'debug'
Write-Host "building light-ui-editor ($buildProfile)"
Push-Location $root
try {
        $cargoArgs = @('build', '-p', 'light-ui-editor')
        if ($Release) { $cargoArgs += '--release' }
        cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) { throw "building light-ui-editor failed with exit code $LASTEXITCODE" }
} finally {
        Pop-Location
}

$exe = Join-Path $root "target/$buildProfile/light-ui-editor.exe"
if (-not (Test-Path $exe)) { throw "editor binary not found at $exe" }

Write-Host "editing $design"
#   detached, so the terminal returns while the editor runs
Start-Process -FilePath $exe -ArgumentList $design
