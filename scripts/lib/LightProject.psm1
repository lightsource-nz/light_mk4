# Shared helpers for the light framework's build/flash scripts.
#
# WHY THIS EXISTS: every script in this directory needs the same three answers -- which project
# am I in, which build tree does this preset own, and is that tree currently configured for
# something else. Answering them once here is what keeps the per-project wrappers down to a
# handful of lines, and what stops each script inventing its own idea of where things live.
#
# The build-tree question is not academic. Four screen-test presets, all six crossfire presets
# and all seven font-crusher presets resolve to the same ${sourceDir}/build, so switching
# configuration silently reuses an incompatible CMake cache. That failure is quiet, and it
# surfaces much later as a link error or, worse, a target built with the wrong compiler.

$script:PresetMarkerFile = '.light-preset'

# Walks up from a starting directory looking for the marker of a project root. CMakePresets.json
# rather than .git, because a submodule checkout is its own git repo but is not a project you
# configure -- building from inside module/light_ui should find screen-test, not light_ui.
function Get-LightProjectRoot {
        param([string]$StartPath = $PWD.Path)

        $dir = (Resolve-Path $StartPath).Path
        while ($dir) {
                if (Test-Path (Join-Path $dir 'CMakePresets.json')) {
                        return $dir
                }
                $parent = Split-Path $dir -Parent
                if ($parent -eq $dir) { break }
                $dir = $parent
        }
        throw "no CMakePresets.json found at or above '$StartPath' -- run this from inside a light project"
}

# Loads <project>/scripts/project.config.ps1, which declares the per-project defaults: which
# preset owns which build tree, and which preset builds a given target. That mapping is data
# rather than logic on purpose -- six build trees on this machine were hand-configured and have
# no preset at all, and their settings existed only inside their own CMakeCache.txt.
function Get-LightProjectConfig {
        param([string]$ProjectRoot)

        if (-not $ProjectRoot) { $ProjectRoot = Get-LightProjectRoot }
        $configPath = Join-Path $ProjectRoot 'scripts/project.config.ps1'
        if (-not (Test-Path $configPath)) {
                throw "no project config at '$configPath' -- expected a scripts/project.config.ps1 declaring this project's presets and targets"
        }

        $config = & $configPath
        if ($config -isnot [hashtable]) {
                throw "'$configPath' must return a hashtable"
        }
        $config['Root'] = $ProjectRoot
        return $config
}

# Absolute path of the build tree a preset owns. Falls back to <root>/build, which is what every
# preset inherits from conf-light-base when it does not override binaryDir -- and is precisely
# why the collisions exist.
function Resolve-LightTree {
        param(
                [Parameter(Mandatory)] [hashtable]$Config,
                [Parameter(Mandatory)] [string]$Preset
        )

        $rel = if ($Config.Trees -and $Config.Trees.ContainsKey($Preset)) { $Config.Trees[$Preset] } else { 'build' }
        return (Join-Path $Config.Root $rel)
}

# The preset that last configured a tree, or $null. CMake records no such thing itself, so the
# configure script drops a marker file -- an exact answer, where comparing cache variables can
# only ever be a guess about which preset produced them.
function Get-LightTreePreset {
        param([Parameter(Mandatory)] [string]$Tree)

        $marker = Join-Path $Tree $script:PresetMarkerFile
        if (Test-Path $marker) { return (Get-Content $marker -Raw).Trim() }
        return $null
}

function Set-LightTreePreset {
        param(
                [Parameter(Mandatory)] [string]$Tree,
                [Parameter(Mandatory)] [string]$Preset
        )

        Set-Content -Path (Join-Path $Tree $script:PresetMarkerFile) -Value $Preset -NoNewline
}

# Reads the handful of cache variables that identify what a tree was configured for. Used to
# describe an existing tree in a refusal message: "this is configured for X" is far more useful
# than "this is configured for something else".
function Get-LightTreeIdentity {
        param([Parameter(Mandatory)] [string]$Tree)

        $cache = Join-Path $Tree 'CMakeCache.txt'
        if (-not (Test-Path $cache)) { return $null }

        $wanted = @('LIGHT_BOARD', 'LIGHT_PLATFORM', 'LIGHT_SYSTEM', 'LIGHT_ARCH', 'PICO_PLATFORM', 'CMAKE_C_COMPILER', 'CMAKE_BUILD_TYPE')
        $identity = [ordered]@{}
        foreach ($line in Get-Content $cache) {
                # CMakeCache lines are NAME:TYPE=VALUE; internal entries repeat names with
                # -ADVANCED etc, so match the exact name only
                if ($line -match '^([A-Za-z_0-9]+):[A-Z]+=(.*)$') {
                        if ($wanted -contains $Matches[1]) { $identity[$Matches[1]] = $Matches[2] }
                }
        }
        return $identity
}

# Compares a tree's cache against what a preset is expected to produce, for the trees that
# predate these scripts and so carry no marker file. That is currently EVERY tree on this
# machine, which makes this the case that matters -- a marker-only guard would wave through
# exactly the reconfigurations it exists to catch. Returns $null when the config declares no
# expectation for this preset, meaning "cannot tell".
function Test-LightTreeMatchesPreset {
        param(
                [Parameter(Mandatory)] [hashtable]$Config,
                [Parameter(Mandatory)] [string]$Preset,
                [Parameter(Mandatory)] $Identity
        )

        if (-not $Config.Expect -or -not $Config.Expect.ContainsKey($Preset)) { return $null }
        if (-not $Identity) { return $null }

        foreach ($entry in $Config.Expect[$Preset].GetEnumerator()) {
                $actual = $Identity[$entry.Key]
                # an unset cache variable is not a mismatch on its own -- it may simply not have
                # been written for this configuration
                if ($null -ne $actual -and $actual -ne $entry.Value) { return $false }
        }
        return $true
}

function Format-LightTreeIdentity {
        param($Identity)

        if (-not $Identity) { return '(not configured)' }
        return (($Identity.GetEnumerator() | Where-Object { $_.Value } | ForEach-Object { "$($_.Key)=$($_.Value)" }) -join ', ')
}

#   the target to act on when the caller named none. Every utility script defaults its otherwise
# mandatory parameters, so that `scripts/build.ps1` with no arguments does the obvious thing from
# a terminal, and a CI job does not have to encode a target name it would then have to keep in
# step with the project.
#
#   a missing DefaultTarget is a configuration error rather than a prompt: these scripts have to
# run non-interactively, and PowerShell's own Mandatory prompt is exactly the behaviour being
# removed -- in CI it hangs waiting on stdin that never comes.
function Resolve-LightDefaultTarget {
        param([Parameter(Mandatory)] [hashtable]$Config)

        if ($Config.DefaultTarget) { return $Config.DefaultTarget }
        $known = if ($Config.Targets) { ($Config.Targets.Keys | Sort-Object) -join ', ' } else { '(none declared)' }
        throw "no target given and '$($Config.Name)' declares no DefaultTarget in scripts/project.config.ps1. Pass -Target, or add one. Known targets: $known"
}

#   the preset to configure when the caller named none: an explicit DefaultPreset if the project
# declares one, otherwise whichever preset owns the default target -- which is the preset a bare
# `build.ps1` would use anyway, so the two stay consistent by construction
function Resolve-LightDefaultPreset {
        param([Parameter(Mandatory)] [hashtable]$Config)

        if ($Config.DefaultPreset) { return $Config.DefaultPreset }
        $target = Resolve-LightDefaultTarget -Config $Config
        return (Resolve-LightTargetPreset -Config $Config -Target $target)
}

# Which preset builds this target, from the project config.
function Resolve-LightTargetPreset {
        param(
                [Parameter(Mandatory)] [hashtable]$Config,
                [Parameter(Mandatory)] [string]$Target
        )

        if ($Config.Targets -and $Config.Targets.ContainsKey($Target)) {
                return $Config.Targets[$Target].Preset
        }
        $known = if ($Config.Targets) { ($Config.Targets.Keys | Sort-Object) -join ', ' } else { '(none declared)' }
        throw "target '$Target' is not declared in this project's scripts/project.config.ps1. Known targets: $known"
}

Export-ModuleMember -Function Get-LightProjectRoot, Get-LightProjectConfig, Resolve-LightTree,
        Get-LightTreePreset, Set-LightTreePreset, Get-LightTreeIdentity, Format-LightTreeIdentity,
        Test-LightTreeMatchesPreset, Resolve-LightTargetPreset,
        Resolve-LightDefaultTarget, Resolve-LightDefaultPreset
