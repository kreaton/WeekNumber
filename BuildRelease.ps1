# BuildRelease.ps1
# Builds the next version, packs it with Velopack and publishes it as a GitHub release.
# GitHub Actions runs this on every push to master (.github/workflows/release.yml).
#   .\BuildRelease.ps1             build, pack and upload
#   .\BuildRelease.ps1 -LocalOnly  build and pack into .\Releases only
#
# Requires the vpk tool, matching the velopack crate version in Cargo.toml:
#   dotnet tool install -g vpk --version 1.2.161
# and, unless -LocalOnly, a GitHub token: $env:GH_TOKEN in CI, otherwise the GitHub CLI (gh auth login).
param([switch]$LocalOnly)

Set-Location -Path $PSScriptRoot
$ErrorActionPreference = 'Stop'

$AppName = "WeekNumber"
$ExeName = "weeknumber.exe"
$RepoUrl = "https://github.com/kreaton/WeekNumber"
$FirstVersion = "1.0.0"
$PublishDir = ".\publish"
$ReleasesDir = ".\Releases"

if (-not $LocalOnly) {
    # Fetch the latest published release so the version continues from it and vpk can build a delta
    $token = if ($env:GH_TOKEN) { $env:GH_TOKEN } else { gh auth token }
    if (-not $token) { throw "No GitHub token (set GH_TOKEN or run: gh auth login)" }
    vpk download github --repoUrl $RepoUrl --token $token -o $ReleasesDir
    if ($LASTEXITCODE -ne 0) { throw "vpk download failed" }
}

# Find the highest version already packed. It is read from the full package's file name because
# vpk download only fetches the package: a clean Releases folder has no releases.win.json.
$largestVersion = Get-ChildItem -Path $ReleasesDir -Filter "$AppName-*-full.nupkg" -ErrorAction SilentlyContinue
    | ForEach-Object { [version]($_.Name -replace "^$AppName-|-full\.nupkg$") }
    | Sort-Object -Descending
    | Select-Object -First 1
if ($largestVersion) {
    # Increment the patch number by 1
    $newVersion = "{0}.{1}.{2}" -f $largestVersion.Major, $largestVersion.Minor, ($largestVersion.Build + 1)
}
else {
    $newVersion = $FirstVersion
}

Write-Host "Building $AppName version $newVersion (win-x64, static C runtime)"
& .\build.ps1
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
if (Test-Path $PublishDir) { Remove-Item $PublishDir -Recurse -Force }
New-Item -ItemType Directory -Path $PublishDir | Out-Null
Copy-Item ".\target\release\$ExeName" $PublishDir

Write-Host "Packing $AppName version $newVersion with Velopack"
vpk pack -u $AppName -v $newVersion -p $PublishDir -e $ExeName -o $ReleasesDir --runtime win-x64
if ($LASTEXITCODE -ne 0) { throw "vpk pack failed" }

if (-not $LocalOnly) {
    Write-Host "Uploading $AppName version $newVersion to GitHub"
    # In CI, tag the commit that was built rather than whatever the branch points at by now
    $commitish = if ($env:GITHUB_SHA) { @("--targetCommitish", $env:GITHUB_SHA) } else { @() }
    vpk upload github --repoUrl $RepoUrl --token $token -o $ReleasesDir --publish --tag "v$newVersion" --releaseName "$AppName $newVersion" @commitish
    if ($LASTEXITCODE -ne 0) { throw "vpk upload failed" }
}

Write-Host "Built $AppName version $newVersion"
