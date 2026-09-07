# Derives a semantic version string from git state.
#
# WHY THIS EXISTS: nothing in any of these repos could say which commit built a binary. Every
# version string was a hand-typed "0.1.0" in a header, most of them never read by any code, and
# no repo had a single git tag. A binary in someone else's hands could not be traced back to
# the source that produced it, which is the first thing you need when a field report arrives.
#
# Annotated tags are the source of truth. This script only reports; light-release.ps1 is the
# only thing that creates a version.
#
# OUTPUT
#   on an annotated tag, clean:      1.2.3
#   5 commits past v1.2.3:           1.3.0-dev.5+feature-x.gabc1234
#   no tags at all:                  0.1.0-dev.<commits>+<branch>.g<sha>
#   dirty working tree:              ...+<branch>.g<sha>.dirty
#
# WHY THE VERSION IS BUMPED for a dev build: semver orders a pre-release BEFORE its release, so
# calling a build five commits past v1.2.3 "1.2.3-dev.5" would sort it behind the 1.2.3 it is
# actually ahead of. A dev version names what is being worked TOWARD, not what was last shipped.
#
# USAGE:  light-version.ps1 [-Bump minor|patch|major] [-Json] [-Path <repo>]
param(
        [ValidateSet('major', 'minor', 'patch')] [string]$Bump = 'minor',
        [switch]$Json,
        [string]$Path
)

$ErrorActionPreference = 'Stop'

$repo = if ($Path) { $Path } else { $PWD.Path }
function git-in { param([string[]]$Arguments) & git -C $repo @Arguments 2>$null }

if (-not (git-in @('rev-parse', '--git-dir'))) {
        throw "'$repo' is not a git repository"
}

$sha = (git-in @('rev-parse', '--short=7', 'HEAD'))
if (-not $sha) {
        throw "repository '$repo' has no commits"
}

$branch = (git-in @('rev-parse', '--abbrev-ref', 'HEAD'))
#   semver build metadata allows only [0-9A-Za-z-], so 'feature/x' has to become 'feature-x'.
# Detached HEAD reports 'HEAD', which is useless as a label but harmless as one.
$branchLabel = ($branch -replace '[^0-9A-Za-z-]', '-').Trim('-')
if (-not $branchLabel) { $branchLabel = 'nobranch' }

$dirty = [bool](git-in @('status', '--porcelain'))

#   --abbrev=0 gives the bare tag name; the full describe gives us the commit distance.
#   THE GLOB MUST REQUIRE ALL THREE COMPONENTS. 'v[0-9]*' also matches the moving major tag that
# light-release.ps1 maintains for workflow consumers (v1, following the newest v1.y.z), and
# git describe picks by commit distance rather than by name -- so with both v1 and v1.3.0 on the
# same commit it will happily answer 'v1', which is not a semantic version. This threw, CMake
# swallowed the failure through its fallback, and every project's baked-in version silently
# became 0.0.0-unknown. Requiring two literal dots excludes the major tag by construction.
$tagGlob = 'v[0-9]*.[0-9]*.[0-9]*'
$lastTag = (git-in @('describe', '--tags', '--abbrev=0', '--match', $tagGlob))

if ($lastTag) {
        $describe = (git-in @('describe', '--tags', '--long', '--match', $tagGlob))
        # v1.2.3-5-gabc1234 -> tag v1.2.3, 5 commits ahead
        if ($describe -notmatch '^(?<tag>.+)-(?<count>\d+)-g[0-9a-f]+$') {
                throw "could not parse 'git describe' output: '$describe'"
        }
        $count = [int]$Matches['count']
        $base = $Matches['tag'] -replace '^v', ''
} else {
        # no tags anywhere. Count all commits so the number still increases monotonically, and
        # start from 0.0.0 -- which the bump below then carries to 0.1.0-dev.N, matching the
        # 0.1.0 that light-release.ps1 mints for a repo with no tags. A pre-release sorts BEFORE
        # its release, so that first real 0.1.0 necessarily sorts above every build made before it.
        $count = [int](git-in @('rev-list', '--count', 'HEAD'))
        $base = '0.0.0'
}

if ($base -notmatch '^(?<maj>\d+)\.(?<min>\d+)\.(?<pat>\d+)(?<pre>-[0-9A-Za-z.-]+)?$') {
        throw "last tag '$lastTag' is not a semantic version"
}
$maj = [int]$Matches['maj']; $min = [int]$Matches['min']; $pat = [int]$Matches['pat']
$basePre = $Matches['pre']

if ($count -eq 0 -and -not $dirty) {
        # exactly on the tag with nothing modified: this IS that release
        $version = "$maj.$min.$pat$basePre"
} else {
        #   bump only when the tag is a full release. If the last tag was itself a pre-release
        # (v1.3.0-rc.1) then 1.3.0 is already the target, and bumping again would skip it.
        if (-not $basePre) {
                switch ($Bump) {
                        'major' { $maj++; $min = 0; $pat = 0 }
                        'minor' { $min++; $pat = 0 }
                        'patch' { $pat++ }
                }
        }
        $meta = "$branchLabel.g$sha"
        if ($dirty) { $meta += '.dirty' }
        $version = "$maj.$min.$pat-dev.$count+$meta"
}

if ($Json) {
        return ([ordered]@{
                version   = $version
                base      = if ($lastTag) { $lastTag } else { $null }
                commits   = $count
                branch    = $branch
                sha       = $sha
                dirty     = $dirty
        } | ConvertTo-Json -Compress)
}
return $version
