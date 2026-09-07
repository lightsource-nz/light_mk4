# Configures, builds and runs a project's host test suite, plus any smoke commands it declares.
#
# WHY THIS EXISTS: the tests only exist on HOST builds -- the framework gates its whole
# test/ subtree on LIGHT_PLATFORM STREQUAL HOST -- so "run the tests" means configuring a
# different tree from the one you were just working in. That is enough friction that the suite
# gets run less often than it should, which defeats the point of having it.
#
# A green CTest run is also not the whole story for font-crusher: crush is the heaviest consumer
# of light_cli (23 commands, registration, parsing, dispatch, aliasing) and exercises the module
# load/unload path end to end, but none of that is under CTest. So projects may declare smoke
# commands that must exit 0.
#
# USAGE:  light-test.ps1 [-Preset <name>] [-NoBuild] [-ProjectRoot <path>]
param(
        [string]$Preset,
        [switch]$NoBuild,
        [string]$ProjectRoot
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'lib/LightProject.psm1') -Force
# for Get-LightExeSuffix in the smoke block. light-env.ps1 imports this too, so it would be in
# scope anyway -- named here because relying on a dot-sourced script's imports is invisible
Import-Module (Join-Path $PSScriptRoot 'lib/LightPlatform.psm1') -Force
. (Join-Path $PSScriptRoot 'light-env.ps1') -Quiet

$config = Get-LightProjectConfig -ProjectRoot ($ProjectRoot ? $ProjectRoot : (Get-LightProjectRoot))
if (-not $config.Test) {
        throw "project '$($config.Name)' declares no Test section in scripts/project.config.ps1"
}

$test = $config.Test
if (-not $Preset) { $Preset = $test.Preset }
$tree = Resolve-LightTree -Config $config -Preset $Preset

if (-not $NoBuild) {
        if (-not (Test-Path (Join-Path $tree 'CMakeCache.txt'))) {
                & (Join-Path $PSScriptRoot 'light-configure.ps1') -Preset $Preset -ProjectRoot $config.Root
        }
        $target = $test.Target
        if ($target) {
                & (Join-Path $PSScriptRoot 'light-build.ps1') -Target $target -Preset $Preset -ProjectRoot $config.Root
        } else {
                Write-Host "building all of $tree"
                cmake --build $tree
                if ($LASTEXITCODE -ne 0) { throw "build failed with exit code $LASTEXITCODE" }
        }
}

$failed = @()

if ($test.Ctest) {
        Write-Host "`n=== ctest: $tree ==="
        #   --no-tests=error, because a bare ctest EXITS 0 when it finds no tests at all. A
        # project that declares Ctest = $true is asserting it has some, so finding none is a
        # failure of this script's premise, not a vacuous pass.
        #   this was not hypothetical: screen-test's Test preset resolved to a tree shared with
        # its rp2040 target configuration, so `test.ps1` built firmware, registered no tests, and
        # printed "all checks passed". Under CI that is the worst available outcome -- a pipeline
        # that stays green precisely because it stopped testing anything.
        ctest --test-dir $tree --output-on-failure --no-tests=error
        if ($LASTEXITCODE -ne 0) { $failed += "ctest (exit $LASTEXITCODE)" }
}

foreach ($smoke in @($test.Smoke)) {
        if (-not $smoke) { continue }
        #   the executable suffix is the platform's, not the config's. A project naming
        # 'bin/crush.exe' is correct on Windows and wrong everywhere else, and a project naming
        # 'bin/crush' is the reverse -- so both spellings are accepted and the one that exists
        # wins. Without this, every smoke command is a "not built" failure on Linux, which reads
        # as a broken build rather than a filename that never applied there.
        $exe = Join-Path $tree $smoke.Exe
        if (-not (Test-Path $exe)) {
                $suffix = Get-LightExeSuffix
                $alt = if ($smoke.Exe -match '\.exe$') { $smoke.Exe -replace '\.exe$', '' }
                       else { "$($smoke.Exe)$suffix" }
                $altPath = Join-Path $tree $alt
                if (Test-Path $altPath) { $exe = $altPath }
        }
        if (-not (Test-Path $exe)) {
                $failed += "smoke '$($smoke.Exe)' (not built)"
                continue
        }
        # cwd matters: crush's own launch configs run from the build root while the exe lives in
        # build/bin, and the relative paths in its arguments are resolved from there
        $cwd = if ($smoke.Cwd) { Join-Path $tree $smoke.Cwd } else { $tree }
        $label = "$($smoke.Exe) $($smoke.Args -join ' ')"
        Write-Host "`n=== smoke: $label ==="
        Push-Location $cwd
        try {
                & $exe @($smoke.Args) | Out-Null
                if ($LASTEXITCODE -ne 0) { $failed += "smoke '$label' (exit $LASTEXITCODE)" }
                else { Write-Host "ok" }
        } finally {
                Pop-Location
        }
}

Write-Host ""
if ($failed.Count -gt 0) {
        throw "FAILED:`n  $($failed -join "`n  ")"
}
Write-Host "all checks passed"
