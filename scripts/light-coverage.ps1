# Measures which code paths a project's test suite actually exercises.
#
# WHY THIS EXISTS: the suites report pass/fail and nothing about reach. A green run says the
# tested paths work; it says nothing about how much of the module was tested at all, and that
# gap is where the expensive bugs live -- the heap corruption fixed in font-crusher sat in a
# lookup function with no test touching it.
#
# WHY IT USES WSL *ON WINDOWS*: there is no usable coverage reporting on the Windows toolchain.
# gcov is present but gcovr/lcov are not, and `python` there is the Microsoft Store stub rather
# than an interpreter. clang's -fprofile-instr-generate with llvm-cov is installed under WSL
# (see the ASan work) and gives region, function, line AND branch coverage plus browsable HTML.
#
# ON LINUX there is nothing to cross into: the same worker script runs directly, against the
# same clang and llvm-cov, with no path translation because none is needed. The WSL hop is an
# implementation detail of being on Windows, not part of what this script does -- so the
# platform decision is made once, here, and the worker is identical either way.
#
# The trade to be aware of: this measures a CLANG build of the code, not the toolchain you ship
# with. For "which paths does the suite reach" that is the same answer; for anything
# compiler-specific it is not.
#
# USAGE:  light-coverage.ps1 [-ProjectRoot <path>] [-NoHtml] [-Distro Debian]
#                            [-Functions <glob>] [-Reuse]
#
#   -Functions takes a source filename glob (stream.c, rend_*.c) and adds a PER-FUNCTION
# breakdown for the files that match, on top of the usual per-file table. That is the view you
# actually want when deciding what to test next: a file at 60% tells you there is work to do,
# the function list tells you where, and which of the gaps are worth closing.
#
#   -Reuse skips configure/build/test and reports off the profile data already in the tree.
# The full run is minutes, and when you are reading the same numbers several ways in a row --
# whole project, then one file's functions -- repeating it buys nothing.
param(
        [string]$ProjectRoot,
        [string]$Distro = 'Debian',
        [switch]$NoHtml,
        [string]$Functions,
        [switch]$Reuse
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'lib/LightProject.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'lib/LightPlatform.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'lib/LightUserConfig.psm1') -Force

$config = Get-LightProjectConfig -ProjectRoot ($ProjectRoot ? $ProjectRoot : (Get-LightProjectRoot))
if (-not $config.Coverage) {
        throw "project '$($config.Name)' declares no Coverage section in scripts/project.config.ps1"
}
$cov = $config.Coverage

#   the one decision this script makes about where it is running. On Windows the worker has to
# be reached through WSL and every path handed to it translated; on Linux it is simply run, and
# ConvertTo-LightWslPath is the identity there, so the rest of this script is written once
$useWsl = $IsWindows
if ($useWsl -and -not (Test-LightWsl -Distro $Distro)) {
        throw "coverage on Windows needs a WSL distro named '$Distro' with clang and llvm-cov installed (see the header). Found no such distro -- 'wsl -l -q' lists what is available, and -Distro selects a different one."
}

$srcWsl = ConvertTo-LightWslPath $config.Root
$lightWsl = ConvertTo-LightWslPath (Join-Path $PSScriptRoot '..')
#   the build tree is NOT passed: the worker puts it under its own $HOME, because that is the
# only place it can be sure is a native Linux filesystem. Building onto a Windows drive -- /mnt/c
# under WSL, or a mounted share on a Linux host -- is slower for a job that is almost entirely
# small-file I/O. Measured on this project, full configure+build+test from clean: 131.7s with the
# tree on /mnt/c, 103.5s under $HOME.
#   letting the worker decide also removes the awkward bit that was here before, where this side
# emitted a literal '$HOME' for the far side to interpolate. Nothing has to quote anything now;
# the worker just asks for its own home.
# the HTML goes under the project, so it can be opened from the host's browser.
# Created here because path resolution needs a real directory and this will not exist first time
$htmlWin = Join-Path $config.Root 'build-coverage/html'
$covRoot = Split-Path $htmlWin -Parent
if (-not (Test-Path $covRoot)) { New-Item -ItemType Directory -Path $covRoot -Force | Out-Null }
$htmlWsl = "$(ConvertTo-LightWslPath $covRoot)/html"

#   the dependency checkouts, resolved and translated here rather than named in each project's
# config. They have to be passed explicitly: the coverage build tree is not beside the source, so
# CMake's own sibling-relative defaults resolve to the wrong place and rend quietly FetchContents
# font-crusher from GitHub instead of using the checkout next door.
#   only passed when the resolved directory actually exists, so a project that needs neither is
# not handed a broken path
$extra = @("-DLIGHT_PATH=$lightWsl")
foreach ($dep in @(
        @{ Name = 'pico-sdk';      Var = 'PICO_SDK_PATH' },
        @{ Name = 'font-crusher';  Var = 'FONT_CRUSHER_PATH' },
        @{ Name = 'light_usb';     Var = 'LIGHT_USB_PATH' },
        @{ Name = 'light_display'; Var = 'LIGHT_DISPLAY_PATH' },
        @{ Name = 'light_ui';      Var = 'LIGHT_UI_PATH' })) {
        $p = Resolve-LightDependency -Name $dep.Name -ProjectRoot $config.Root -EnvVar $dep.Var
        if ($p -and (Test-Path $p)) {
                $extra += "-D$($dep.Var)=$(ConvertTo-LightWslPath $p)"
        }
}
foreach ($kv in @($cov.CMakeArgs)) { if ($kv) { $extra += $kv } }

$worker = "$lightWsl/scripts/light-coverage-worker.ps1"
$ignore = $cov.IgnoreRegex ? $cov.IgnoreRegex : '(/lib/|/usr/|sanitizers/|_deps/)'

#   parameters go in a FILE rather than on the command line. wsl.exe joins its arguments into a
# single command line that is then re-parsed, so an ignore-regex containing '(' or '|' would be
# read as syntax and the run would die before it started. A file is quoted once, here, and never
# re-parsed.
#   a .ps1 returning a hashtable, the same shape as project.config.ps1 -- the worker is
# PowerShell now, so this is data it can simply evaluate rather than a shell fragment it has to
# source. Written with LF endings: it is read by pwsh on Linux either way
$paramsWin = Join-Path $covRoot 'params.ps1'
$lines = @(
        '@{',
        "        Source      = '$srcWsl'",
        "        ProjectName = '$($config.Name)'",
        "        Html        = '$htmlWsl'",
        "        Objects     = '$($cov.Objects)'",
        "        IgnoreRegex = '$ignore'",
        "        LightPath   = '$lightWsl'",
        "        Functions   = '$Functions'",
        "        Reuse       = `$$($Reuse ? 'true' : 'false')",
        "        CMakeArgs   = @($(($extra | ForEach-Object { "'$_'" }) -join ', '))",
        '}'
)
[System.IO.File]::WriteAllText($paramsWin, ($lines -join "`n") + "`n")
$paramsWsl = "$(ConvertTo-LightWslPath $covRoot)/params.ps1"

Write-Host "platform: $(Get-LightPlatform)$(if ($useWsl) { " (worker runs in WSL '$Distro')" })"
Write-Host "project : $($config.Name)"
Write-Host "source  : $srcWsl"
Write-Host ""

if ($useWsl) {
        #   the absolute path to pwsh inside the distro, discovered rather than assumed: a
        # non-login `wsl -- pwsh` does not see ~/.local/bin, which is where a tarball install
        # puts it, so the obvious invocation fails on a machine where pwsh works perfectly well
        $wslPwsh = Get-LightWslPwsh -Distro $Distro
        if (-not $wslPwsh) {
                throw "no pwsh found inside WSL distro '$Distro'. The coverage worker is a PowerShell script, so the distro needs PowerShell 7 installed -- see https://learn.microsoft.com/powershell/scripting/install/install-debian (a tarball into ~/powershell with a symlink on PATH is enough)."
        }
        & wsl.exe -d $Distro -- $wslPwsh -NoProfile -File $worker $paramsWsl
} else {
        # already running under pwsh on Linux, so the worker is simply invoked
        & $worker $paramsWsl
}
$rc = $LASTEXITCODE

if ($rc -ne 0) {
        throw "coverage run failed with exit code $rc"
}
if (-not $NoHtml -and (Test-Path (Join-Path $htmlWin 'index.html'))) {
        Write-Host "html: $(Join-Path $htmlWin 'index.html')"
}
