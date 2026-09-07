# Creates an annotated git tag, which is what mints a release version.
#
# WHY THIS EXISTS: with tags as the source of truth, tagging IS releasing -- so the checks
# belong here rather than in a checklist someone remembers. A tag on a dirty tree records a
# version that exists on no one else's machine; a tag that goes backwards breaks every ordering
# comparison downstream; and a duplicate tag silently does nothing useful.
#
# Deliberately does NOT push by default. A local tag is trivially deleted, a pushed one is not.
#
# USAGE:  light-release.ps1 [-Version 1.2.0] [-Bump minor] [-PreRelease rc.1] [-Message <text>]
#                           [-Push] [-Path <repo>]
#
#   -Version defaults to the next version after the newest tag, bumped by -Bump (minor, matching
# light-version.ps1's own default). That is what makes this callable from CI without encoding a
# number that would then have to be kept in step by hand.
#
#   defaulting it is safe because nothing here is irreversible on its own: the tag is LOCAL
# unless -Push is given, and every check below still applies -- a dirty tree, an existing tag or
# a version that does not move forward all refuse. Pass -Version explicitly for a major release,
# or any time the computed answer is not the intended one.
param(
        [string]$Version,
        [ValidateSet('major', 'minor', 'patch')] [string]$Bump = 'minor',
        [string]$PreRelease,
        [string]$Message,
        [switch]$Push,
        [switch]$AllowDirty,
        [string]$Path
)

$ErrorActionPreference = 'Stop'

$repo = if ($Path) { $Path } else { $PWD.Path }
function git-in { param([string[]]$Arguments) & git -C $repo @Arguments 2>$null }

if (-not (git-in @('rev-parse', '--git-dir'))) { throw "'$repo' is not a git repository" }

#   no -Version: compute the next one from the newest tag. Deliberately reads the tags directly
# rather than shelling out to light-version.ps1, which answers a different question -- it reports
# the version being worked TOWARD including pre-release and build metadata, and this needs a bare
# MAJOR.MINOR.PATCH to bump. An unreleased repository starts at 0.1.0 rather than 0.0.0, since a
# tag that means "nothing has been released" is not worth minting.
if (-not $Version) {
        #   all three components required, so the moving major tag written at the end of this
        # script (v1, v2...) is never mistaken for the release it points at
        $latest = git-in @('tag', '--list', 'v[0-9]*.[0-9]*.[0-9]*', '--sort=-v:refname') | Select-Object -First 1
        if ($latest -and ($latest -replace '^v', '') -match '^(?<maj>\d+)\.(?<min>\d+)\.(?<pat>\d+)') {
                $lmaj = [int]$Matches['maj']; $lmin = [int]$Matches['min']; $lpat = [int]$Matches['pat']
                switch ($Bump) {
                        'major' { $Version = "$($lmaj + 1).0.0" }
                        'minor' { $Version = "$lmaj.$($lmin + 1).0" }
                        'patch' { $Version = "$lmaj.$lmin.$($lpat + 1)" }
                }
                Write-Host "no -Version given: $latest + $Bump -> $Version"
        } else {
                $Version = '0.1.0'
                Write-Host "no -Version given and no existing tags: starting at $Version"
        }
}

$core = $Version -replace '^v', ''
if ($core -notmatch '^(?<maj>\d+)\.(?<min>\d+)\.(?<pat>\d+)$') {
        throw "version '$Version' must be MAJOR.MINOR.PATCH (put any pre-release in -PreRelease)"
}
$maj = [int]$Matches['maj']; $min = [int]$Matches['min']; $pat = [int]$Matches['pat']

if ($PreRelease) {
        if ($PreRelease -notmatch '^[0-9A-Za-z.-]+$') {
                throw "pre-release '$PreRelease' may contain only alphanumerics, dots and hyphens"
        }
        $core = "$core-$PreRelease"
}
$tag = "v$core"

if (-not $AllowDirty -and (git-in @('status', '--porcelain'))) {
        throw "working tree is dirty -- a release must describe a state that exists in the repository. Commit or stash first, or pass -AllowDirty if you know why you want this."
}

if (git-in @('rev-parse', '-q', '--verify', "refs/tags/$tag")) {
        throw "tag '$tag' already exists"
}

#   ordering check against the newest release tag. Pre-releases are excluded from the
# comparison base: v1.3.0-rc.1 does not stop v1.3.0 being released, and comparing against it
# would wrongly reject exactly that.
$previous = git-in @('tag', '--list', 'v[0-9]*.[0-9]*.[0-9]*', '--sort=-v:refname') | Where-Object { $_ -notmatch '-' } | Select-Object -First 1
if ($previous) {
        $prev = $previous -replace '^v', ''
        if ($prev -match '^(?<maj>\d+)\.(?<min>\d+)\.(?<pat>\d+)$') {
                $prevTuple = @([int]$Matches['maj'], [int]$Matches['min'], [int]$Matches['pat'])
                $newTuple = @($maj, $min, $pat)
                $ordered = $false
                for ($i = 0; $i -lt 3; $i++) {
                        if ($newTuple[$i] -gt $prevTuple[$i]) { $ordered = $true; break }
                        if ($newTuple[$i] -lt $prevTuple[$i]) { break }
                }
                # equal tuples are fine only when this is a pre-release OF that version
                if (-not $ordered -and -not ($PreRelease -and ($newTuple -join '.') -eq ($prevTuple -join '.'))) {
                        throw "version $core does not come after the current release $prev"
                }
        }
}

if (-not $Message) { $Message = "release $core" }

Write-Host "tagging $tag at $(git-in @('rev-parse','--short=7','HEAD'))"
git -C $repo tag -a $tag -m $Message
if ($LASTEXITCODE -ne 0) { throw "git tag failed with exit code $LASTEXITCODE" }

if ($Push) {
        git -C $repo push origin $tag
        if ($LASTEXITCODE -ne 0) { throw "git push failed with exit code $LASTEXITCODE" }
        Write-Host "pushed $tag to origin"
} else {
        Write-Host "created $tag locally. Push it with: git -C $repo push origin $tag"
}

#   THE MOVING MAJOR TAG, e.g. v1 following the newest v1.y.z.
#
#   WHY IT EXISTS: other repositories consume this one by ref -- GitHub Actions resolves
# `uses: owner/repo/.github/workflows/x.yml@REF`, and REF must be a concrete branch, tag or SHA.
# There is no "newest tag" ref. That leaves two bad options and one good one:
#     @main            every push here changes every consumer's CI immediately, with nothing
#                      released about it and no way to stage a workflow change
#     @v1.3.0          correct and immutable, but every consumer must be edited on every release
#     @v1              consumers track the latest release in the major line and are edited only
#                      when a deliberately breaking v2 arrives
# The third is the convention the actions ecosystem settled on (actions/checkout@v4), and it is
# what makes "downstream tracks the latest tagged version" mean anything without hand edits.
#
#   -f BECAUSE IT MOVES. This is the one tag here that is expected to be rewritten, which is
# also why it is worth being explicit that anyone wanting an immutable reference should name the
# full version instead. A force-push of a tag is normally a mistake; here it is the mechanism.
#
#   pre-releases deliberately do not move it: handing every consumer a release candidate is the
# opposite of what a stable major ref is for.
if ($PreRelease) {
        Write-Host "pre-release: leaving the v$maj major tag where it is"
} else {
        $majorTag = "v$maj"
        git -C $repo tag -f -a $majorTag -m "$Message (moving major tag for the v$maj line)"
        if ($LASTEXITCODE -ne 0) { throw "git tag of $majorTag failed with exit code $LASTEXITCODE" }
        Write-Host "moved $majorTag to $tag"

        if ($Push) {
                git -C $repo push -f origin $majorTag
                if ($LASTEXITCODE -ne 0) { throw "git push of $majorTag failed with exit code $LASTEXITCODE" }
                Write-Host "pushed $majorTag to origin (force: it moves by design)"
        } else {
                Write-Host "moved $majorTag locally. Push it with: git -C $repo push -f origin $majorTag"
        }
}
