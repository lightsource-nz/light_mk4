# Configures a build tree from a CMake preset, refusing to reuse one belonging to another preset.
#
# WHY THIS EXISTS: presets in these projects collide on their build directory. Four screen-test
# presets, all six crossfire presets and all seven font-crusher presets resolve to the same
# ${sourceDir}/build, because only some of them override binaryDir and the rest inherit it from
# conf-light-base. Switching between two of those does not reconfigure cleanly -- it reuses a
# cache built for a different board, platform or compiler. CMake permits this silently, and the
# result surfaces much later as a link failure or a target quietly built with the wrong toolchain.
#
# So this refuses by default and tells you what the tree actually holds. -Force wipes it, which
# is the only correct way to move a tree from one preset to another.
#
# USAGE:  light-configure.ps1 [-Preset <name>] [-Force] [-ProjectRoot <path>]
param(
        [string]$Preset,
        [switch]$Force,
        [string]$ProjectRoot
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'lib/LightProject.psm1') -Force
. (Join-Path $PSScriptRoot 'light-env.ps1') -Quiet

$config = Get-LightProjectConfig -ProjectRoot ($ProjectRoot ? $ProjectRoot : (Get-LightProjectRoot))
#   defaults to the project's DefaultPreset, or to whichever preset owns its DefaultTarget --
# which is the preset a bare `build.ps1` would use, so the two agree by construction
if (-not $Preset) { $Preset = Resolve-LightDefaultPreset -Config $config }
$tree = Resolve-LightTree -Config $config -Preset $Preset

if (Test-Path $tree) {
        $owner = Get-LightTreePreset -Tree $tree
        $identity = Get-LightTreeIdentity -Tree $tree

        if ($Force) {
                Write-Host "removing existing tree '$tree' (-Force)"
                Remove-Item -Recurse -Force $tree
        }
        elseif ($owner -and $owner -ne $Preset) {
                throw @"
build tree '$tree' is configured for preset '$owner', not '$Preset'.
  currently: $(Format-LightTreeIdentity $identity)
These presets share a build directory, so reconfiguring in place would reuse an incompatible
cache. Re-run with -Force to wipe and reconfigure, or pick a preset that owns its own tree.
"@
        }
        elseif (-not $owner -and $identity) {
                #   no marker: configured before these scripts existed, or by hand. Every tree on
                # a machine that predates this layer is in that state, so falling back to a
                # cache comparison is what makes the guard useful at all rather than only after
                # the first scripted configure.
                $matches = Test-LightTreeMatchesPreset -Config $config -Preset $Preset -Identity $identity
                if ($matches -eq $false) {
                        throw @"
build tree '$tree' does not match preset '$Preset'.
  currently: $(Format-LightTreeIdentity $identity)
  expected:  $(($config.Expect[$Preset].GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" }) -join ', ')
These presets share a build directory, so reconfiguring in place would reuse an incompatible
cache. Re-run with -Force to wipe and reconfigure.
"@
                }
                Write-Warning "tree '$tree' was configured outside these scripts ($(Format-LightTreeIdentity $identity)); reconfiguring in place"
        }
}

Write-Host "configuring '$Preset' -> $tree"
cmake --preset $Preset
if ($LASTEXITCODE -ne 0) {
        throw "cmake --preset $Preset failed with exit code $LASTEXITCODE"
}

Set-LightTreePreset -Tree $tree -Preset $Preset
Write-Host "configured $tree"
