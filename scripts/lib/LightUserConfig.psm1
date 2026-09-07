# Where things live on THIS machine, and nowhere in version control.
#
# WHY THIS EXISTS: absolute paths were committed all over these repos -- one developer's
# C:/Users/<name>/tools in three launch.json files, three settings.json files, a CMakePresets
# entry, two shell scripts and a project config. Every one of them is guaranteed to fail the
# moment anybody else clones the repo, and they fail in the least helpful way available: not
# "this path is wrong" but a toolchain silently not found, or a dependency quietly fetched from
# GitHub instead of the checkout next door.
#
# THE RULE: a committed file may never name an absolute path. It gets the answer from one of
#   1. an environment variable, if the caller has already set one
#   2. this user config, which is gitignored and machine-specific
#   3. a default derived from the current project's own location
# in that order. (3) is what makes a fresh clone work with no configuration at all, which is the
# real goal -- the user config exists for machines that deviate from the layout, not as
# something everybody has to fill in first.
#
# THE DEFAULT LAYOUT is siblings: a dependency named `pico-sdk` is expected at
# ../pico-sdk relative to the project that needs it. That is how these repos are already checked
# out, so the zero-config path is also the common one.

Set-StrictMode -Version Latest

$script:UserConfigCache = $null
$script:UserConfigPathUsed = $null

#   the user config, or an empty hashtable. Cached: several scripts ask repeatedly and it is a
# file read plus a script invocation each time.
#
#   LIGHT_USER_CONFIG wins so a CI runner or a second checkout can point somewhere else without
# editing anything; otherwise it sits beside the framework, which every project can already find
# because that is how it finds the scripts at all.
function Get-LightUserConfig {
        param([switch]$Refresh)

        if ($script:UserConfigCache -and -not $Refresh) { return $script:UserConfigCache }

        $candidates = @()
        if ($env:LIGHT_USER_CONFIG) { $candidates += $env:LIGHT_USER_CONFIG }
        if ($env:LIGHT_PATH) { $candidates += (Join-Path $env:LIGHT_PATH 'user.config.ps1') }
        $candidates += (Join-Path (Split-Path (Split-Path $PSScriptRoot -Parent) -Parent) 'user.config.ps1')

        foreach ($c in $candidates) {
                if (-not (Test-Path $c)) { continue }
                $loaded = & $c
                if ($loaded -isnot [hashtable]) {
                        throw "user config '$c' must return a hashtable (see user.config.example.ps1)"
                }
                $script:UserConfigCache = $loaded
                $script:UserConfigPathUsed = (Resolve-Path $c).Path
                return $script:UserConfigCache
        }

        # absent is the normal case, not an error -- the sibling defaults are expected to work
        $script:UserConfigCache = @{}
        return $script:UserConfigCache
}

function Get-LightUserConfigPath {
        Get-LightUserConfig | Out-Null
        return $script:UserConfigPathUsed
}

#   resolves a dependency checkout: an environment variable if set, then the user config's
# Projects table, then ../<name> beside the project asking. Returns the path whether or not it
# exists -- callers report a missing dependency far better than this can, naming what needed it.
function Resolve-LightDependency {
        param(
                [Parameter(Mandatory)] [string]$Name,
                [string]$ProjectRoot,
                # environment variable that overrides everything, e.g. PICO_SDK_PATH
                [string]$EnvVar
        )

        if ($EnvVar) {
                $fromEnv = [System.Environment]::GetEnvironmentVariable($EnvVar)
                if ($fromEnv) { return $fromEnv }
        }

        $cfg = Get-LightUserConfig
        if ($cfg.ContainsKey('Projects') -and $cfg.Projects -and $cfg.Projects.ContainsKey($Name)) {
                return $cfg.Projects[$Name]
        }

        #   the sibling default. Relative to the project that needs the dependency, not to this
        # script -- a project checked out somewhere else entirely should find ITS siblings
        if (-not $ProjectRoot) {
                $ProjectRoot = if ($env:LIGHT_PATH) { $env:LIGHT_PATH }
                               else { Split-Path (Split-Path $PSScriptRoot -Parent) -Parent }
        }
        return (Join-Path (Split-Path $ProjectRoot -Parent) $Name)
}

#   resolves a tool directory: the user config's Tools table, then the supplied candidates (which
# should themselves be derived, not absolute), then nothing. Returns $null rather than guessing,
# so a caller can fall back to PATH -- which on Linux is usually where these actually are.
function Resolve-LightToolDir {
        param(
                [Parameter(Mandatory)] [string]$Name,
                [string[]]$Candidates = @()
        )

        $cfg = Get-LightUserConfig
        if ($cfg.ContainsKey('Tools') -and $cfg.Tools -and $cfg.Tools.ContainsKey($Name)) {
                $p = $cfg.Tools[$Name]
                if ($p) { return $p }
        }
        foreach ($c in $Candidates) {
                if ($c -and (Test-Path $c)) { return (Resolve-Path $c).Path }
        }
        return $null
}

Export-ModuleMember -Function Get-LightUserConfig, Get-LightUserConfigPath,
        Resolve-LightDependency, Resolve-LightToolDir
