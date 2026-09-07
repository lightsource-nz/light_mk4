# Builds a target, configuring its tree first if it does not exist yet.
#
# WHY THIS EXISTS: knowing which preset builds a given target, and which tree that preset owns,
# was folklore -- eight flashable targets across screen-test alone, spread over five build trees,
# with two of the build presets naming a target ('screentest') that does not exist in any
# CMakeLists. Here the mapping is declared once in the project's scripts/project.config.ps1 and
# every wrapper reads it.
#
# The linker warning filter is not cosmetic: every RP2350 link emits two warnings about the
# FLASH memory region that are inherent to how the pico-sdk linker scripts are composed. Left in,
# they train you to ignore linker warnings, which is the last habit you want on an embedded
# project. -Verbose shows everything.
#
# USAGE:  light-build.ps1 [-Target <name>] [-Preset <name>] [-Clean] [-Verbose]
param(
        [string]$Target,
        [string]$Preset,
        [switch]$Clean,
        [string]$ProjectRoot
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'lib/LightProject.psm1') -Force
. (Join-Path $PSScriptRoot 'light-env.ps1') -Quiet

$config = Get-LightProjectConfig -ProjectRoot ($ProjectRoot ? $ProjectRoot : (Get-LightProjectRoot))
#   defaults to the project's DefaultTarget, so a bare invocation does the obvious thing from
# a terminal and CI need not encode a target name it would have to keep in step
if (-not $Target) { $Target = Resolve-LightDefaultTarget -Config $config }
if (-not $Preset) { $Preset = Resolve-LightTargetPreset -Config $config -Target $Target }
$tree = Resolve-LightTree -Config $config -Preset $Preset

#   AN EXISTING TREE IS CHECKED, not trusted. This used to configure only when CMakeCache.txt was
# absent and otherwise build whatever was there -- which meant `-Preset X` against a tree
# belonging to preset Y silently built Y and reported success for X.
#
#   that is not theoretical and it is not cosmetic: these projects' presets share build
# directories on purpose (all six crossfire presets resolve to ${sourceDir}/build), so the tree
# in front of you is routinely the wrong one. Building crossfire_main with
# -Preset conf-crossfire-debug against a leftover pico2 tree compiled the rp2350 chip port,
# linked, and printed "built crossfire_main" -- an rp2040 build that was nothing of the kind.
# light-configure.ps1 has guarded exactly this since it was written; the guard simply was not
# reached from here.
#
#   the marker is cross-checked against the cache rather than believed, because a stale marker is
# a thing that happens: the tree that produced the above claimed 'conf-crossfire-trace' while its
# cache said pico2.
if (-not (Test-Path (Join-Path $tree 'CMakeCache.txt'))) {
        & (Join-Path $PSScriptRoot 'light-configure.ps1') -Preset $Preset -ProjectRoot $config.Root
} else {
        $owner = Get-LightTreePreset -Tree $tree
        $identity = Get-LightTreeIdentity -Tree $tree
        $mismatch = $null

        if ($owner -and $owner -ne $Preset) {
                $mismatch = "it is configured for preset '$owner'"
        } elseif ((Test-LightTreeMatchesPreset -Config $config -Preset $Preset -Identity $identity) -eq $false) {
                $mismatch = "its cache does not match preset '$Preset'"
        }

        if ($mismatch) {
                throw @"
build tree '$tree' cannot be used for preset '$Preset': $mismatch.
  currently: $(Format-LightTreeIdentity $identity)
These presets share a build directory, so building here would compile against an incompatible
cache and report success for a configuration it never built. Reconfigure with:
  light-configure.ps1 -Preset $Preset -Force
"@
        }
}

$buildArgs = @($tree, '--target', $Target)
if ($Clean) { $buildArgs += '--clean-first' }

Write-Host "building $Target ($Preset) in $tree"

#   known-harmless and unavoidable: objects.rp2350.ld declares no FLASH region of its own, and
# pico_flash_region.ld then redeclares the one the SDK provides. Neither indicates a problem
# with the image, and both fire on every single link.
$noise = 'warning: memory region `FLASH'' not declared|warning: redeclaration of memory region `FLASH'''

if ($VerbosePreference -ne 'SilentlyContinue') {
        cmake --build @buildArgs
} else {
        cmake --build @buildArgs 2>&1 | Where-Object { $_ -notmatch $noise } | ForEach-Object { Write-Host $_ }
}

# $LASTEXITCODE survives the pipeline above; PowerShell sets it from the native command
if ($LASTEXITCODE -ne 0) {
        throw "build of '$Target' failed with exit code $LASTEXITCODE"
}
Write-Host "built $Target"
