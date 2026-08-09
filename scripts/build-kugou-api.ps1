[CmdletBinding()]
param(
    [string]$Destination = (Join-Path $PSScriptRoot "..\target\kugou-api.exe"),
    [string]$Repository = "https://github.com/MakcRe/KuGouMusicApi.git",
    [string]$Tag = "v1.6.0",
    [string]$NodeVersion = "18",
    [string]$Npm = "npm",
    [string]$Npx = "npx"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$destinationItem = Get-Item -LiteralPath $Destination -ErrorAction SilentlyContinue
if ($destinationItem -and $destinationItem.PSIsContainer) {
    throw "sidecar 输出路径必须是文件: $Destination"
}
$destinationDirectory = Split-Path -Parent $Destination
New-Item -ItemType Directory -Force -Path $destinationDirectory | Out-Null

$node = Get-Command node -ErrorAction Stop
$nodeMajor = (& $node.Source --version).Trim() -replace '^v', '' -split '\.' | Select-Object -First 1
if ([int]$nodeMajor -lt [int]$NodeVersion) {
    throw "Node.js 主版本过低: 需要 >= $NodeVersion，实际为 $nodeMajor"
}
Get-Command git -ErrorAction Stop | Out-Null
Get-Command $Npm -ErrorAction Stop | Out-Null
Get-Command $Npx -ErrorAction Stop | Out-Null

$worktree = Join-Path $env:TEMP ("KuGouMusicApi-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $worktree | Out-Null
try {
    git clone --depth 1 --branch $Tag $Repository $worktree
    Push-Location $worktree
    try {
        & $Npm install --ignore-scripts
        if ($LASTEXITCODE -ne 0) { throw "KuGouMusicApi npm install 失败" }
        & $Npx pkg . --targets "node$NodeVersion-win-x64" --output $Destination --no-bytecode
        if ($LASTEXITCODE -ne 0) { throw "KuGouMusicApi sidecar 打包失败" }
    } finally {
        Pop-Location
    }
    if (-not (Test-Path -LiteralPath $Destination -PathType Leaf)) {
        throw "sidecar 输出文件不存在: $Destination"
    }
} finally {
    if (Test-Path -LiteralPath $worktree) {
        Remove-Item -LiteralPath $worktree -Recurse -Force -ErrorAction SilentlyContinue
    }
}
Write-Output "已生成酷狗概念版 API sidecar: $Destination"
