# Linux-side worker for light-coverage.ps1 -- the half that runs where clang and llvm-cov are.
#
# THE SAME SCRIPT ON BOTH PLATFORMS: on Linux it is run directly, on Windows it is run inside
# WSL. Nothing in here knows the difference, which is deliberate -- the platform decision is made
# once, in light-coverage.ps1, and everything below operates on Linux paths the caller has
# already translated (a no-op on Linux).
#
# WHY POWERSHELL AND NOT BASH, which it was until now: the layer already requires pwsh on Linux,
# so a bash worker was the only file in it written in a second language, with its own quoting
# rules, its own array syntax and its own way of being wrong. One language means one set of
# habits. The cost is that WSL must have pwsh installed, which the launcher checks for and says
# so plainly -- see Get-LightWslPwsh.
#
# TAKES ONE ARGUMENT: a parameter file, which is a .ps1 returning a hashtable -- the same shape
# as project.config.ps1. Not positional arguments: wsl.exe joins whatever it is given into a
# single command line that is then re-parsed, so an ignore-regex containing '(' or '|' would be
# read as syntax and the run would die before it started. A file is quoted once, by the writer,
# and never re-parsed.
#
# USAGE: pwsh -NoProfile -File light-coverage-worker.ps1 <params-file.ps1>
param(
        [Parameter(Mandatory)] [string]$ParamsFile
)

$ErrorActionPreference = 'Stop'

$p = & $ParamsFile
if ($p -isnot [hashtable]) { throw "params file '$ParamsFile' must return a hashtable" }

$src       = $p.Source
$html      = $p.Html
$objglob   = $p.Objects
$ignore    = $p.IgnoreRegex
$light     = $p.LightPath
$functions = $p.Functions
$reuse     = [bool]$p.Reuse
$extraArgs = @($p.CMakeArgs)

#   THE BUILD TREE LIVES UNDER $HOME, and the worker decides that rather than the caller, because
# only this side knows where a native filesystem actually is.
#
#   this job is almost entirely small-file I/O -- thousands of object files, then a test binary
# per case -- which is the worst case for a Windows drive seen through /mnt/c (or for any mounted
# share on a Linux host).
#
#   MEASURED on the framework, full configure + build + test: 131.7s with both source and
# tree on /mnt/c, 103.5s with the tree moved here. Moving the SOURCE as well (below) took it to
# 16.6s warm. The tree alone was only 21% because the reads still crossed the mount.
#
#   only the HTML report is written back to the project, so it can be opened from the host.
$work = Join-Path $HOME "cov-$($p.ProjectName)"
$build = $work
if (-not (Test-Path $work)) { New-Item -ItemType Directory -Force -Path $work | Out-Null }
Write-Host "build   : $build"

#   THE SOURCE IS MIRRORED TOO, when it lives across a mount. Moving only the build tree fixed
# the writes; the reads turned out to cost more.
#
#   Sync-LightWslMirror lives in lib/LightPlatform.psm1 rather than here, because this is not a
# coverage concern: it is the cost of running ANYTHING in WSL against a tree on /mnt/, and every
# WSL run should pay the mirror once rather than the crossing repeatedly. The measurements, and
# why it is rsync rather than cp -au or a robocopy push, are documented at the function.
Import-Module (Join-Path $PSScriptRoot 'lib/LightPlatform.psm1') -Force

#   returns the source unchanged when there is nothing to gain (already native) or nothing to do
# it with (no rsync), so the mirrored and unmirrored cases share one path below
$effectiveSrc = Sync-LightWslMirror -From $src -Name $p.ProjectName
$effectiveLight = $light
if ($effectiveSrc -ne $src) {
        Write-Host "mirror  : $effectiveSrc"

        #   the framework is mirrored SEPARATELY when it is not the project being measured.
        # LIGHT_PATH is what light_version.cmake runs git against on every build, so leaving it
        # pointing across the mount would keep the 20s version check for screen-test, crossfire
        # and font-crusher -- i.e. for every project except this one
        if ($light -and $light -ne $src) {
                $effectiveLight = Sync-LightWslMirror -From $light -Name 'light_mk4'
                if ($effectiveLight -ne $light) {
                        Write-Host "        + framework mirrored to $effectiveLight"
                }
        } elseif ($light -eq $src) {
                $effectiveLight = $effectiveSrc
        }

        #   rewrite the LIGHT_PATH the launcher passed, which names the original location.
        # Everything else in CMakeArgs is a dependency the build only reads, so it can stay
        # across the mount without costing a per-build git call
        $extraArgs = @($extraArgs | ForEach-Object {
                if ($_ -like '-DLIGHT_PATH=*') { "-DLIGHT_PATH=$effectiveLight" } else { $_ }
        })
}

#   the versioned binaries where they exist, the unversioned ones otherwise. Debian ships
# llvm-cov-19 with no unsuffixed alias unless llvm-defaults is installed
$profdata = 'llvm-profdata-19'
$cov      = 'llvm-cov-19'
if (-not (Get-Command $profdata -ErrorAction SilentlyContinue)) {
        $profdata = 'llvm-profdata'; $cov = 'llvm-cov'
}
if (-not (Get-Command $cov -ErrorAction SilentlyContinue)) {
        throw "no llvm-cov found (looked for llvm-cov-19 and llvm-cov). Install clang and the llvm tools in this environment."
}

# native exit codes, not exceptions: $ErrorActionPreference does not apply to them
function Invoke-Checked {
    param([string]$What, [scriptblock]$Body, [string]$LogFile)

    & $Body
    if ($LASTEXITCODE -ne 0) {
        Write-Host "$What FAILED"
        if ($LogFile -and (Test-Path $LogFile)) { Get-Content $LogFile -Tail 25 }
        exit 1
    }
}

#   a build tree remembers the source directory it was generated from, and CMake refuses to
# reuse one whose source has moved: "The source ... does not match the source ... used to
# generate cache." That is a hard stop with no suggested action beyond re-running by hand.
#
#   it happens for an ordinary reason -- the mirror location changed (it was ~/src-<name> before
# it became ~/light-mirror/<name>, so that siblings resolve inside the mirror) -- and it will
# happen again to anyone whose checkout moves. The tree is entirely regenerable, so wiping it is
# the right answer rather than making a human read a CMake error and work out that rm is safe.
$cacheFile = Join-Path $build 'CMakeCache.txt'
if (Test-Path $cacheFile) {
        $home_dir = (Select-String -Path $cacheFile -Pattern '^CMAKE_HOME_DIRECTORY:INTERNAL=(.*)$' |
                Select-Object -First 1).Matches.Groups[1].Value
        if ($home_dir -and $home_dir -ne $effectiveSrc) {
                Write-Host "note    : tree was generated from '$home_dir', source is now '$effectiveSrc' -- reconfiguring from scratch"
                Remove-Item -Recurse -Force $build
                New-Item -ItemType Directory -Force -Path $build | Out-Null
        }
}

#   -Reuse reports off whatever is already in the tree. Guarded on the profile actually being
# there: falling back to a full run is right, silently reporting nothing is not
$skipBuild = $false
if ($reuse -and (Test-Path (Join-Path $build 'cov.profdata'))) {
        Write-Host "== reusing existing profile data =="
        $skipBuild = $true
}

#   cmake and ctest are invoked FROM the working directory, not merely pointed at it. Anything
# they create with a relative path -- a stray log, a test's own output file, ctest's temporary
# state -- then lands on the fast filesystem too, rather than back across the mount because the
# caller's cwd happened to be on the project side
Push-Location $work
try {

if (-not $skipBuild) {
        #   -fprofile-instr-generate/-fcoverage-mapping are clang spellings, so CMAKE_CXX_COMPILER
        # has to be clang++ as well: these projects enable C, CXX and ASM, and CMake otherwise
        # picks /usr/bin/c++ (g++), which rejects the flag while linking its own compiler test.
        #   the warning suppressions are not cosmetic -- this code is built with gcc day to day
        # and clang is stricter about several things that are not what we are measuring here
        Write-Host "== configuring coverage build =="
        $cfgLog = "$build.cfg.log"

        #   -ffile-prefix-map rewrites the paths the compiler EMBEDS -- in the coverage mapping
        # and in debug info -- back to where the source really lives. Without it the whole report
        # describes $HOME/src-<project>, a copy nobody edits, and every filename in the HTML is a
        # path that means nothing to the reader.
        #   this is what makes mirroring invisible: the build reads fast local files, the report
        # names the real ones, and llvm-cov needs no help reconciling them.
        $prefixMaps = @()
        if ($effectiveSrc -ne $src) { $prefixMaps += "-ffile-prefix-map=$effectiveSrc=$src" }
        if ($effectiveLight -and $effectiveLight -ne $light) {
                $prefixMaps += "-ffile-prefix-map=$effectiveLight=$light"
        }
        $cFlags = (@('-fprofile-instr-generate', '-fcoverage-mapping', '-g',
                     '-Wno-implicit-function-declaration', '-Wno-int-conversion') + $prefixMaps) -join ' '

        Invoke-Checked -What 'CONFIGURE' -LogFile $cfgLog -Body {
                cmake -S $effectiveSrc -B $build -G Ninja `
                        -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++ `
                        -DCMAKE_BUILD_TYPE=Debug -DLIGHT_RUN_MODE=DEBUG `
                        "-DCMAKE_C_FLAGS=$cFlags" `
                        "-DCMAKE_EXE_LINKER_FLAGS=-fprofile-instr-generate" `
                        @extraArgs *> $cfgLog
        }

        Write-Host "== building =="
        $buildLog = "$build.build.log"
        Invoke-Checked -What 'BUILD' -LogFile $buildLog -Body { cmake --build $build *> $buildLog }

        Write-Host "== running tests =="
        $profDir = Join-Path $build 'prof'
        if (Test-Path $profDir) { Remove-Item -Recurse -Force $profDir }
        New-Item -ItemType Directory -Force -Path $profDir | Out-Null

        #   %p gives each test PROCESS its own counter file; ctest runs many, and a single shared
        # file would have them overwrite each other
        $env:LLVM_PROFILE_FILE = Join-Path $profDir '%p.profraw'
        $ctestLog = "$build.ctest.log"
        #   NOT Invoke-Checked: a failing suite still produced profile data, and reporting
        # coverage for a red run is useful. The pass/fail line is echoed either way
        ctest --test-dir $build *> $ctestLog
        $summary = (Select-String -Path $ctestLog -Pattern 'tests passed' | Select-Object -First 1)
        if ($summary) { Write-Host "   $($summary.Line.Trim())" }

        $raw = @(Get-ChildItem (Join-Path $profDir '*.profraw') -ErrorAction SilentlyContinue)
        if ($raw.Count -eq 0) {
                Write-Host "NO PROFILE DATA -- the suite ran no instrumented binaries"
                exit 1
        }
        Write-Host "   $($raw.Count) profile files"

        Invoke-Checked -What 'PROFDATA MERGE' -Body {
                & $profdata merge -sparse @($raw.FullName) -o (Join-Path $build 'cov.profdata')
        }
}

} finally {
        #   restored even on a failure or an `exit` from inside, so this never leaves the caller's
        # shell sitting in the working directory -- which matters on Linux, where the worker runs
        # in the SAME process as the launcher rather than in a WSL child
        Pop-Location
}

#   every test binary has to be named: llvm-cov reads the coverage map out of the objects, so a
# binary left out simply does not appear, silently understating the result.
#   'auto' discovers every instrumented executable in the tree, which is what projects whose test
# binaries sit at several different depths need (screen-test puts them under light_audio/, rend/
# and light_framework/test/ alike). CMakeFiles holds the compiler-probe binaries, which carry no
# coverage map and would only add noise.
#
#   THE EXECUTABLE BIT IS NOT A RELIABLE FILTER, and this is the most important check in the
# file. A build tree on a Windows drive -- /mnt/c under WSL, or the project directory itself on a
# Linux host mounting one -- reports EVERY file as executable, so cmake_install.cmake and
# context.json sail straight through it. Handing either to llvm-cov fails the whole run with
# 'not recognized as a valid object file' or 'invalid tapi_tbd_version section', neither of which
# points at the actual problem. It used to guard only the 'auto' branch, and the glob branch got
# away with it purely because the tree happened to live on ext4; both go through it now.
function Test-IsElf {
        param([string]$Path)

        if (-not (Test-Path $Path -PathType Leaf)) { return $false }
        try {
                $fs = [System.IO.File]::OpenRead($Path)
                try {
                        $magic = [byte[]]::new(4)
                        if ($fs.Read($magic, 0, 4) -ne 4) { return $false }
                        return ($magic[0] -eq 0x7F -and $magic[1] -eq 0x45 -and
                                $magic[2] -eq 0x4C -and $magic[3] -eq 0x46)   # \x7F E L F
                } finally { $fs.Dispose() }
        } catch { return $false }
}

$bins = @()
if ($objglob -eq 'auto') {
        $bins = @(Get-ChildItem -Path $build -Recurse -File -ErrorAction SilentlyContinue |
                Where-Object {
                        $_.FullName -notmatch '/CMakeFiles/' -and
                        $_.FullName -notmatch '_deps' -and
                        $_.Name -notmatch '\.(so|a|cmake)$' -and
                        $_.Name -notmatch '\.so\.'
                } |
                Where-Object { Test-IsElf $_.FullName } |
                ForEach-Object { $_.FullName })
} else {
        $bins = @(Get-ChildItem -Path (Join-Path $build $objglob) -ErrorAction SilentlyContinue |
                Where-Object { Test-IsElf $_.FullName } |
                ForEach-Object { $_.FullName })
}

if ($bins.Count -eq 0) {
        Write-Host "NO OBJECTS matched '$objglob' under $build"
        exit 1
}
Write-Host "   $($bins.Count) instrumented binaries"

# -object per binary, which is how llvm-cov takes more than one
$objs = @()
foreach ($b in $bins) { $objs += @('-object', $b) }
$profData = Join-Path $build 'cov.profdata'

#   report paths are mapped back to the REAL source, so the tables and the HTML name the file
# you would open in your editor rather than a copy under $HOME that nobody edits. Without this
# the whole report silently describes a mirror, which is the kind of wrong that looks right.
#   nothing needed here: -ffile-prefix-map (see the configure step) makes the compiler record
# the ORIGINAL paths in the coverage mapping, so the report already names the files you would
# open in an editor.
#
#   --path-equivalence was tried first and is the wrong tool. It tells llvm-cov where to FIND
# sources whose recorded paths do not exist locally; it does not rewrite what the report
# DISPLAYS. The text table hid that, because it prints paths relative to a common root, and the
# mirror only showed up when the generated HTML was grepped -- 36 files still naming
# /home/<user>/src-light_mk4.
$pathMap = @()

Write-Host ""
#   stderr deliberately NOT discarded. llvm-cov reports "functions have mismatched data" and
# similar on stderr while still printing a table, and swallowing that turns a wrong number into a
# silent one
& $cov report @objs @pathMap "-instr-profile=$profData" "-ignore-filename-regex=$ignore"
if ($LASTEXITCODE -ne 0) { Write-Host "REPORT FAILED"; exit 1 }

#   the per-function view. Sources are searched for under the project AND the framework, because
# a project's build pulls light_core's sources in from outside its own tree, and the file you
# want broken down is as often one of those as one of its own
if ($functions) {
        $roots = @($src)
        if ($light) { $roots += $light }
        $srcs = @($roots | ForEach-Object {
                        Get-ChildItem -Path $_ -Recurse -File -Filter $functions -ErrorAction SilentlyContinue
                } |
                Where-Object { $_.FullName -notmatch '/build' -and $_.FullName -notmatch '_deps' } |
                ForEach-Object { $_.FullName } | Sort-Object -Unique)

        Write-Host ""
        if ($srcs.Count -eq 0) {
                Write-Host "no source file matching '$functions' under $src$(if ($light) { " or $light" })"
        } else {
                Write-Host "== per-function coverage: $functions =="
                #   the first binary goes POSITIONALLY and the rest as -object. Passing them all
                # as -object makes llvm-cov read the SOURCE argument as the main binary, which
                # fails with 'not a valid object file' and reads like a broken source path rather
                # than a missing binary
                & $cov report $bins[0] @($objs[2..($objs.Count - 1)]) @pathMap `
                        "-instr-profile=$profData" -show-functions @srcs
                if ($LASTEXITCODE -ne 0) { Write-Host "PER-FUNCTION REPORT FAILED" }
        }
}

if (Test-Path $html) { Remove-Item -Recurse -Force $html }
& $cov show @objs @pathMap "-instr-profile=$profData" "-ignore-filename-regex=$ignore" `
        -format=html "-output-dir=$html" -show-line-counts-or-regions *> $null
Write-Host ""
Write-Host "html report: $html/index.html"
