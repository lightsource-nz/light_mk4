# Sets up the toolchain environment every light project build depends on.
#
# WHY THIS EXISTS: this contract was only ever written down in .vscode/settings.json -- and in
# crossfire's case that file is gitignored, so the knowledge existed on exactly one machine.
# Anything driving a build from a plain shell had to rediscover it, and the failures are
# indirect enough to cost real time. Both rules below were learned by breaking them.
#
# USAGE:  . $env:LIGHT_PATH/scripts/light-env.ps1   [-Quiet]
#
# Dot-sourced, not run: it modifies the current session's environment. Running it as a child
# process would set variables that vanish when it exits.
param(
        [switch]$Quiet
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'lib/LightPlatform.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'lib/LightUserConfig.psm1') -Force

#   every absolute path below used to be hardcoded to one developer's Windows home directory,
# which no second machine could satisfy and no Linux machine could even parse. They are now
# derived: from this script's location where that is possible, and from an overridable root
# otherwise. LIGHT_TOOLS_PATH names where third-party toolchains are unpacked
$tools = Get-LightToolRoot

#   LIGHT_PATH is the anchor for everything else -- CMake reads it from the environment
# (screen-test/CMakeLists.txt) and light_preinit.cmake resolves the framework against it.
# Derived from this script's own location so a moved or cloned checkout still works.
$frameworkRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path

#   every path exported to the environment goes through this. CMake parses these values as CMake
# strings, where a backslash starts an escape -- "C:\Users\..." fails outright with
# "Invalid character escape '\U'". The paths that used to be hardcoded here were written with
# forward slashes and so never hit it; deriving them with Join-Path produces the native
# separator, which does. Forward slashes are accepted by CMake, PowerShell and Windows alike
function script:_ForCMake([string]$p) {
        if (-not $p) { return $p }
        return ($p -replace '\\', '/')
}

if (-not $env:LIGHT_PATH) { $env:LIGHT_PATH = (_ForCMake $frameworkRoot) }

#   dependency checkouts, resolved by Resolve-LightDependency: an environment variable if the
# shell already set one, then the gitignored user config, then ../<name> beside the project that
# needs it. The sibling default is what makes a fresh clone build with no configuration.
#
#   resolved against the project being worked in when there is one -- so a project checked out
# somewhere else finds ITS siblings -- falling back to the framework's own location otherwise
#
#   NAMED $envProjectRoot, AND THAT MATTERS. This script is DOT-SOURCED by every wrapper, so
# everything it assigns lands in the caller's own scope -- and PowerShell variable names are
# case-insensitive, so a `$projectRoot` here IS the caller's `$ProjectRoot` parameter. Every
# wrapper that passed -ProjectRoot had it silently replaced, one line before it was read, by
# whatever Get-LightProjectRoot made of the current directory. font-crusher's scripts/build.ps1
# run from another project's directory therefore built the OTHER project's default target and
# reported success -- exactly the class of wrong-tree failure this script layer exists to stop.
$envProjectRoot = $frameworkRoot
try {
        Import-Module (Join-Path $PSScriptRoot 'lib/LightProject.psm1') -Force
        $envProjectRoot = Get-LightProjectRoot
} catch {
        # not inside a project (or no CMakePresets.json above the cwd) -- the framework's own
        # parent is the right answer then, and is the same directory in a normal checkout
}

#   PICO_SDK_PATH MUST be set before the FIRST configure of any pico tree. The pico presets pass
# it through as $env{PICO_SDK_PATH}, and when it is empty pico_sdk_init() never runs. That fails
# late and misleadingly -- as "Unknown CMake command pico_enable_stdio_semihosting" -- and worse,
# the failed configure caches the HOST compiler, so a later correct configure still builds
# boot_stage2 with w64devkit's cc.exe. The only recovery is deleting the build tree.
$env:PICO_SDK_PATH = _ForCMake (Resolve-LightDependency -Name 'pico-sdk' -ProjectRoot $envProjectRoot -EnvVar 'PICO_SDK_PATH')

#   without this, rend's own Findcrush.cmake defaults FONT_CRUSHER_PATH relative to ITSELF,
# misses the sibling checkout, and silently FetchContent-fetches font-crusher from GitHub --
# so local crush edits have no effect at all. screen-test works around it in CMake; crossfire
# does not, and every one of its build trees is currently building against a fetched copy.
$env:FONT_CRUSHER_PATH = _ForCMake (Resolve-LightDependency -Name 'font-crusher' -ProjectRoot $envProjectRoot -EnvVar 'FONT_CRUSHER_PATH')

#   the toolchain directories to prepend, which are genuinely different per platform rather
# than the same thing spelled two ways.
#
#   on WINDOWS these are unpacked distributions under the tools root: w64devkit supplies the
# host gcc/cmake/ninja and must come FIRST, before anything else that ships a cmake.
#   on LINUX the host toolchain, cmake, ninja and openocd are normally distro packages already
# on PATH, so the list is near-empty by design -- an ARM toolchain unpacked under the tools
# root is still honoured if it is there, and nothing is warned about if it is not.
#
#   the RISC-V toolchain is deliberately ABSENT on both: it reaches the build through
# PICO_TOOLCHAIN_PATH in the riscv preset, and putting it on PATH breaks the ARM trees.
#   each is resolved as: user config entry, then the derived candidates below, then nothing --
# see lib/LightUserConfig.psm1. Nothing here is an absolute path, and the user config that may
# contain one is gitignored
$w64devkit  = Resolve-LightToolDir -Name 'w64devkit'     -Candidates @("$tools/w64devkit/bin")
$armBin     = Resolve-LightToolDir -Name 'arm-toolchain' -Candidates @(
        "$tools/arm-gnu-toolchain-14.2.rel1-mingw-w64-x86_64-arm-none-eabi/bin",
        "$tools/arm-gnu-toolchain/bin")
$openocdBin = Resolve-LightToolDir -Name 'openocd'       -Candidates @(
        "$tools/xpack-openocd-0.12.0-7/bin",
        "$tools/xpack-openocd/bin")

if ($IsWindows) {
        $required = @($w64devkit, $armBin, $openocdBin, "$env:LOCALAPPDATA/Microsoft/WinGet/Links") |
                Where-Object { $_ }
        $optional = @()
} else {
        # a packaged arm-none-eabi-gcc / openocd already on PATH is the norm here, so nothing is
        # required and nothing is warned about
        $required = @()
        $optional = @($armBin, $openocdBin) | Where-Object { $_ }
}

#   exported so the things that CANNOT call this script can still avoid hardcoding: the riscv
# CMake preset reads PICO_TOOLCHAIN_PATH, and .vscode/launch.json reads these through
# ${env:...} substitution. Set rather than defaulted-if-empty, since they describe this machine
$riscv = Resolve-LightToolDir -Name 'riscv-toolchain' -Candidates @(
        "$tools/riscv-toolchain-15-x64-win",
        "$tools/riscv-toolchain")
if ($riscv -and -not $env:PICO_TOOLCHAIN_PATH) { $env:PICO_TOOLCHAIN_PATH = _ForCMake $riscv }
if ($armBin) { $env:LIGHT_ARM_TOOLCHAIN_BIN = $armBin }
if ($openocdBin) { $env:LIGHT_OPENOCD_BIN = $openocdBin }
if ($w64devkit) { $env:LIGHT_W64DEVKIT_BIN = $w64devkit }
# the host debugger the cppdbg launch configs use; w64devkit's gdb where there is one, else PATH
$hostGdb = Find-LightTool -Name "gdb$(Get-LightExeSuffix)" -Candidates @(
        ($w64devkit ? [System.IO.Path]::Combine($w64devkit, "gdb$(Get-LightExeSuffix)") : $null))
if ($hostGdb) { $env:LIGHT_HOST_GDB = $hostGdb }
#   [System.IO.Path]::GetDirectoryName rather than Split-Path: Split-Path resolves through
# PowerShell's drive providers and THROWS on a path whose drive does not exist ("Cannot find
# drive. A drive with the name 'D' does not exist"). A user config naming a tool on a drive that
# is not currently mounted is a perfectly ordinary thing, and it must not take down light-env
$ocdPrefix = if ($openocdBin) { [System.IO.Path]::GetDirectoryName($openocdBin) } else { $null }
#   ...and [System.IO.Path]::Combine rather than Join-Path, for the same reason: Join-Path
# resolves drives too, so it throws on the same input GetDirectoryName was chosen to survive
$ocdScripts = Resolve-LightToolDir -Name 'openocd-scripts' -Candidates @(
        ($ocdPrefix ? [System.IO.Path]::Combine($ocdPrefix, 'openocd/scripts') : $null),
        ($ocdPrefix ? [System.IO.Path]::Combine($ocdPrefix, 'share/openocd/scripts') : $null))
if ($ocdScripts) { $env:LIGHT_OPENOCD_SCRIPTS = $ocdScripts }

$sep = Get-LightPathSeparator
$missing = @()
foreach ($dir in ($required + $optional)) {
        if (-not (Test-Path $dir)) {
                # only the required set is worth complaining about
                if ($required -contains $dir) { $missing += $dir }
                continue
        }
        $native = (Resolve-Path $dir).Path
        # prepend only if absent, so repeated dot-sourcing does not grow PATH without bound.
        # splitting on a hardcoded ';' would never match on Linux and would do exactly that
        if (($env:PATH -split [regex]::Escape($sep)) -notcontains $native) {
                $env:PATH = "$native$sep$env:PATH"
        }
}

if ($missing.Count -gt 0) {
        Write-Warning "toolchain directories not found (builds needing them will fail):`n  $($missing -join "`n  ")"
}

#   a build needs these three regardless of platform, and finding them missing here is a far
# better message than cmake's. Not fatal: a HOST-only build of one project may legitimately
# not have an ARM toolchain installed
foreach ($t in @('cmake', 'ninja')) {
        if (-not (Find-LightTool -Name $t)) {
                Write-Warning "'$t' is not on PATH -- configure and build will fail"
        }
}

if (-not $Quiet) {
        $ucp = Get-LightUserConfigPath
        Write-Host "platform          = $(Get-LightPlatform)"
        Write-Host "user config       = $($ucp ? $ucp : '(none -- using derived defaults)')"
        Write-Host "LIGHT_PATH        = $env:LIGHT_PATH"
        Write-Host "LIGHT_TOOLS_PATH  = $tools"
        Write-Host "PICO_SDK_PATH     = $env:PICO_SDK_PATH"
        Write-Host "FONT_CRUSHER_PATH = $env:FONT_CRUSHER_PATH"
        if ($env:PICO_TOOLCHAIN_PATH) { Write-Host "PICO_TOOLCHAIN_PATH = $env:PICO_TOOLCHAIN_PATH" }
}
