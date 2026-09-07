# Host-platform differences, in one place.
#
# WHY THIS EXISTS: these scripts were written on Windows and had Windows baked into them in ways
# that were easy to miss -- Win32_SerialPort, Get-Volume, a hardcoded C:/Users/.../tools, a ';'
# PATH separator, wsl.exe as the only route to clang. None of that is conceptually
# Windows-specific; every one has a Linux answer. Scattering `if ($IsWindows)` through nine
# scripts would work and would rot, because the next script would copy whichever branch its
# author happened to be running.
#
# So each difference is answered ONCE here, and the scripts ask a question rather than test a
# platform. The test to apply when adding to this file: if a caller has to know which OS it is
# on to use your function, the abstraction is in the wrong place.
#
# $IsWindows/$IsLinux/$IsMacOS are automatic variables in PowerShell 6+. They do NOT exist in
# Windows PowerShell 5.1, where every one reads as $null and every check silently takes the
# wrong branch -- hence the version guard at the bottom, which fails loudly instead.

Set-StrictMode -Version Latest

function Get-LightPlatform {
        if ($IsWindows) { return 'Windows' }
        if ($IsLinux)   { return 'Linux' }
        if ($IsMacOS)   { return 'MacOS' }
        throw "cannot determine host platform"
}

# '.exe' on Windows, '' elsewhere. For composing tool paths that are otherwise identical.
function Get-LightExeSuffix {
        if ($IsWindows) { return '.exe' } else { return '' }
}

#   ';' on Windows, ':' everywhere else. Splitting $env:PATH on a hardcoded ';' silently yields
# one enormous element on Linux, so the "is this already on PATH" test never matches and PATH
# grows without bound on every dot-source
function Get-LightPathSeparator {
        return [System.IO.Path]::PathSeparator
}

#   where third-party toolchains live. LIGHT_TOOLS_PATH overrides; otherwise a per-platform
# default that at least exists as a convention. This used to be a hardcoded absolute path
# containing one developer's username, which is not something a second machine can satisfy
function Get-LightToolRoot {
        if ($env:LIGHT_TOOLS_PATH) { return $env:LIGHT_TOOLS_PATH }
        return (Join-Path $HOME 'tools')
}

# First of `Candidates` that exists, else the first found on PATH by `Name`, else $null. Lets a
# caller state its preferences without also writing the search.
function Find-LightTool {
        param(
                [Parameter(Mandatory)] [string]$Name,
                [string[]]$Candidates = @()
        )

        foreach ($c in $Candidates) {
                if ($c -and (Test-Path $c)) { return (Resolve-Path $c).Path }
        }
        $cmd = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue |
                Select-Object -First 1
        if ($cmd) { return $cmd.Source }
        return $null
}

# --- WSL, which is a Windows-only concept -------------------------------------------------

#   true only when a usable WSL distro is present. On Linux this is always false, and that is
# the point: the coverage script runs its Linux worker natively there rather than looking for a
# Linux VM it is already inside
function Test-LightWsl {
        param([string]$Distro = 'Debian')

        if (-not $IsWindows) { return $false }
        $wsl = Get-Command wsl.exe -ErrorAction SilentlyContinue
        if (-not $wsl) { return $false }
        # -l -q lists installed distros; wsl.exe emits UTF-16, which PowerShell handles, but the
        # names carry stray nulls on some builds
        $distros = (& wsl.exe -l -q 2>$null) -replace "`0", '' | ForEach-Object { $_.Trim() }
        return [bool]($distros -contains $Distro)
}

#   the absolute path to pwsh INSIDE a WSL distro, or $null.
#
#   discovery is necessary rather than paranoid: `wsl -d <distro> -- pwsh` runs a NON-login shell,
# whose PATH does not include ~/.local/bin, which is exactly where a tarball install of PowerShell
# puts its symlink. So the obvious invocation fails with "pwsh: command not found" on a machine
# that has pwsh installed and working. Asking a login shell once, then using the absolute path it
# reports, works regardless of how it was installed.
function Get-LightWslPwsh {
        param([string]$Distro = 'Debian')

        if (-not $IsWindows) { return $null }
        $found = (& wsl.exe -d $Distro -- bash -lc 'command -v pwsh' 2>$null)
        if ($LASTEXITCODE -ne 0 -or -not $found) { return $null }
        return ($found -replace "`0", '').Trim()
}

# C:\Users\x\y -> /mnt/c/Users/x/y. Windows-only by definition; on Linux a path is already the
# path WSL would see, because there is no WSL.
function ConvertTo-LightWslPath {
        param([Parameter(Mandatory)] [string]$Path)

        $full = (Resolve-Path $Path).Path
        if (-not $IsWindows) { return $full }
        $drive = $full.Substring(0, 1).ToLower()
        return "/mnt/$drive" + ($full.Substring(2) -replace '\\', '/')
}

#   Mirrors a source tree from a Windows mount onto the WSL filesystem, and returns the path to
# use. Called from INSIDE the distro (the paths are Linux paths).
#
# WHY EVERY WSL RUN SHOULD USE THIS: reading a tree through /mnt/ is the dominant cost of any
# build launched in WSL, and it is not a small factor.
#
#   MEASURED on the framework, source on /mnt/c:
#     git status --porcelain      17.8-28.9s   |  on an ext4 mirror:  0.4s
#     light-version.ps1           19.9s        |                      0.8s
#     cmake configure             48.5s        |                     23.4s
#   the version check is the one that hurts, because light_version.cmake runs it on EVERY build
# by design (so a binary cannot report a commit it was not built from), and it is dominated by
# git stat-ing every tracked file across the mount. No cheaper git incantation exists: -uno and
# `git diff --quiet` measured 24s and 28.9s. The crossing is the cost, so the fix is not to cross.
#
#   rsync, not cp -au, and that is a measurement rather than a preference: cp -au took 102.3s on
# an UNCHANGED tree -- worse than what it was meant to save -- because it stats every file across
# the mount one at a time. rsync incremental is 5.1s. A Windows-side robocopy push was also tried
# and is slower than pulling from this side (5.4s vs 3.2s, and Windows-only).
#
#   .git is INCLUDED deliberately: the version step needs it, and it is the part that was slow.
# build*/ is excluded -- large, regenerable, and some are the host's own trees, which mean
# nothing over here.
#
#   MIRRORS INTO ONE SHARED ROOT, keeping each project's real directory name:
#
#     /mnt/c/Users/x/projects/c/screen-test   ->  ~/light-mirror/screen-test
#     /mnt/c/Users/x/projects/c/light_display ->  ~/light-mirror/light_display
#
#   the layout is the point, not tidiness. Every project here resolves its dependencies as
# ../<name> (see light_resolve_project and Resolve-LightDependency), so mirrors must remain
# SIBLINGS OF EACH OTHER or that resolution silently escapes the mirror and goes back across the
# mount -- or finds nothing at all. An earlier form named them ~/src-<name>, which broke exactly
# that: from ~/src-screen-test, ../light_display is ~/light_display and does not exist. Coverage
# survived it only because it passes LIGHT_PATH explicitly; a build or test run would not.
#
#   returns $From unchanged when there is nothing to gain (already native) or nothing to do it
# with (no rsync), so callers can use the result unconditionally.
function Sync-LightWslMirror {
        param(
                [Parameter(Mandatory)] [string]$From,
                # what to call the mirror; defaults to the source directory's own name, which is
                # what keeps sibling resolution working and is almost always what you want
                [string]$Name
        )

        if (-not $From.StartsWith('/mnt/')) { return $From }
        if (-not (Get-Command rsync -ErrorAction SilentlyContinue)) {
                Write-Host "note    : rsync not installed; using '$From' across the mount, which is markedly slower"
                return $From
        }
        if (-not $Name) { $Name = Split-Path $From -Leaf }

        #   the root must exist first: rsync creates only the FINAL path component, so a
        # destination two levels deep fails with `mkdir ... No such file or directory` and the
        # caller falls back to the mount -- which is slow rather than broken, and so goes
        # unnoticed exactly when you are trying to measure the mirror. (--mkpath would also do
        # it, but is rsync 3.2.3+; creating the directory works everywhere.)
        $root = Join-Path $HOME 'light-mirror'
        if (-not (Test-Path $root)) { New-Item -ItemType Directory -Force -Path $root | Out-Null }

        $to = Join-Path $root $Name
        rsync -a --delete --exclude 'build*/' "$From/" "$to/"
        if ($LASTEXITCODE -ne 0) {
                Write-Host "rsync of '$Name' failed -- using it in place instead"
                return $From
        }
        return $to
}

#   mirrors a project AND the dependency checkouts beside it, so the mirrored tree resolves its
# siblings within the mirror. Returns the mirrored project path.
#
#   `Also` names the sibling directories to bring across. Only those that exist are synced, so a
# caller can list every dependency any project might want without checking first.
function Sync-LightWslProject {
        param(
                [Parameter(Mandatory)] [string]$From,
                [string[]]$Also = @('light_mk4', 'light_display', 'font-crusher')
        )

        $mirrored = Sync-LightWslMirror -From $From
        if ($mirrored -eq $From) { return $From }   # nothing to mirror, or no rsync

        $parent = Split-Path $From -Parent
        foreach ($dep in $Also) {
            $depPath = Join-Path $parent $dep
            if (Test-Path $depPath) { Sync-LightWslMirror -From $depPath -Name $dep | Out-Null }
        }
        return $mirrored
}

# --- USB serial devices --------------------------------------------------------------------

#   every serial device matching a USB VID/PID, as objects with .Device (what you open) and
# .Description. Matching the PID as well as the VID is not optional: a CMSIS-DAP probe shares
# RP2's vendor ID, and a 1200-baud reset sent to the probe does nothing while looking exactly
# like a board that ignored it.
#
#   Windows reads WMI. Linux walks sysfs rather than parsing /dev/serial/by-id names, because
# the by-id string is composed from USB descriptor text -- it carries the product NAME, which
# firmware chooses and which differs between an app and the bootloader, while idVendor/idProduct
# are the numbers actually being matched on.
function Find-LightSerialPort {
        param(
                [Parameter(Mandatory)] [string]$VendorId,     # '2E8A'
                [Parameter(Mandatory)] [string]$ProductId     # '0009'
        )

        if ($IsWindows) {
                $pattern = "*VID_$($VendorId.ToUpper())&PID_$($ProductId.ToUpper())*"
                return @(Get-CimInstance Win32_SerialPort -ErrorAction SilentlyContinue |
                        Where-Object { $_.PNPDeviceID -like $pattern } |
                        ForEach-Object {
                                [pscustomobject]@{ Device = $_.DeviceID; Description = $_.Name }
                        })
        }

        #   NOT $pid: that is a read-only automatic variable holding this process's own ID, and
        # assigning to it throws -- so this whole function failed on Linux the moment it was
        # called, taking light-flash.ps1 and light-console.ps1 with it. Never exercised, because
        # the boards have only ever been flashed from Windows.
        $vid = $VendorId.ToLower().TrimStart('0').PadLeft(4, '0')
        $prod = $ProductId.ToLower().TrimStart('0').PadLeft(4, '0')
        $found = @()
        foreach ($tty in @(Get-ChildItem /sys/class/tty -ErrorAction SilentlyContinue |
                        Where-Object { $_.Name -match '^tty(ACM|USB)' })) {
                #   the tty's `device` link points at the USB INTERFACE; idVendor/idProduct live
                # on the parent device, one level up. Walking a couple of levels covers both
                # layouts without hardcoding either
                $dev = Join-Path $tty.FullName 'device'
                if (-not (Test-Path $dev)) { continue }
                $node = $dev
                for ($i = 0; $i -lt 4; $i++) {
                        $vf = Join-Path $node 'idVendor'
                        $pf = Join-Path $node 'idProduct'
                        if ((Test-Path $vf) -and (Test-Path $pf)) {
                                if (((Get-Content $vf -Raw).Trim() -eq $vid) -and
                                    ((Get-Content $pf -Raw).Trim() -eq $prod)) {
                                        $nameFile = Join-Path $node 'product'
                                        $desc = if (Test-Path $nameFile) { (Get-Content $nameFile -Raw).Trim() } else { $tty.Name }
                                        $found += [pscustomobject]@{
                                                Device = "/dev/$($tty.Name)"; Description = $desc
                                        }
                                }
                                break
                        }
                        $node = Join-Path $node '..'
                }
        }
        return $found
}

# --- the RP2 BOOTSEL mass-storage volume ---------------------------------------------------

#   the mounted BOOTSEL volume as an object with .Path (a directory a UF2 can be copied INTO)
# and .Name, or $null. Returning a path rather than a drive letter is what makes the callers
# platform-free -- and on Windows it also sidesteps a real failure: Get-Volume can return a
# volume with no DriveLetter, and composing 'C:\...\:\' out of that yields a path that fails to
# copy while a careless script reports success.
function Get-LightBootselVolume {
        if ($IsWindows) {
                $v = Get-Volume -ErrorAction SilentlyContinue |
                        Where-Object { $_.FileSystemLabel -like 'RP*' -and $_.DriveLetter } |
                        Select-Object -First 1
                if (-not $v) { return $null }
                return [pscustomobject]@{ Path = "$($v.DriveLetter):\"; Name = $v.FileSystemLabel }
        }

        #   Linux may or may not automount it. Resolve the label to its device node, then look
        # that device up in /proc/mounts -- present-but-unmounted is a genuinely different
        # situation from absent, and the caller can say so
        $byLabel = '/dev/disk/by-label'
        $node = $null
        if (Test-Path $byLabel) {
                $link = Get-ChildItem $byLabel -ErrorAction SilentlyContinue |
                        Where-Object { $_.Name -like 'RPI-RP*' } | Select-Object -First 1
                if ($link) { $node = (Resolve-Path $link.FullName -ErrorAction SilentlyContinue).Path }
        }
        if (-not $node) { return $null }

        foreach ($line in (Get-Content /proc/mounts -ErrorAction SilentlyContinue)) {
                $parts = $line -split '\s+'
                if ($parts.Count -lt 2) { continue }
                if ($parts[0] -eq $node) {
                        # /proc/mounts octal-escapes spaces and friends
                        $mount = $parts[1] -replace '\\040', ' '
                        return [pscustomobject]@{ Path = $mount; Name = 'RPI-RP2'; Device = $node }
                }
        }
        # known to exist, but nothing has mounted it
        return [pscustomobject]@{ Path = $null; Name = 'RPI-RP2'; Device = $node }
}

#   PowerShell 5.1 has no $IsWindows, so every platform test above would read $null and quietly
# take the Linux branch on Windows. Refusing to load is the only safe answer
if ($PSVersionTable.PSVersion.Major -lt 6) {
        throw "these scripts require PowerShell 7 or later (found $($PSVersionTable.PSVersion)). Windows PowerShell 5.1 lacks the automatic platform variables every check here depends on."
}

Export-ModuleMember -Function Get-LightPlatform, Get-LightExeSuffix, Get-LightPathSeparator,
        Get-LightToolRoot, Find-LightTool, Test-LightWsl, Get-LightWslPwsh, ConvertTo-LightWslPath,
        Sync-LightWslMirror, Sync-LightWslProject, Find-LightSerialPort, Get-LightBootselVolume
