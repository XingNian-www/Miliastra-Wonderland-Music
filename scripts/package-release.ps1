# 发布打包:构建 release 并按发布布局组装,剔除运行期敏感/临时数据。
# 用法: .\scripts\package-release.ps1 [-OutDir dist\miliastra-release] [-Objdump objdump]
param(
    [ValidateNotNullOrEmpty()]
    [string]$OutDir = "dist/miliastra-release",
    [string]$Objdump = "objdump"
)
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$stage = if ([System.IO.Path]::IsPathRooted($OutDir)) {
    [System.IO.Path]::GetFullPath($OutDir)
} else {
    [System.IO.Path]::GetFullPath((Join-Path $root $OutDir))
}
$distRoot = [System.IO.Path]::GetFullPath((Join-Path $root "dist"))
if (-not $stage.StartsWith($distRoot + [System.IO.Path]::DirectorySeparatorChar, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "发布目录必须是项目 dist 下的子目录: $stage"
}

$modelNames = @("PP-OCRv6_small_det.mnn", "PP-OCRv6_small_rec.mnn", "ppocr_keys_v6_small.txt")
$requiredSources = @("config.example.yaml", "assets/idioms.txt")
$requiredSources += $modelNames | ForEach-Object { "models/$_" }
foreach ($file in $requiredSources) {
    if (-not (Test-Path -LiteralPath (Join-Path $root $file) -PathType Leaf)) {
        throw "发布资源缺失: $file"
    }
}
Get-Command -Name $Objdump -CommandType Application -ErrorAction Stop | Out-Null

Write-Host "== 1/3 构建 release =="
cargo build --release --workspace --locked
if ($LASTEXITCODE -ne 0) { throw "cargo build 失败" }

Write-Host "== 2/3 组装发布目录: $stage =="
if (Test-Path -LiteralPath $stage) {
    Remove-Item -LiteralPath $stage -Recurse -Force
}
& (Join-Path $PSScriptRoot "package-ffmpeg-runtime.ps1") `
    -FfmpegRoot (Join-Path $root "vendor/ffmpeg/8.1/windows-x64") `
    -Destination $stage `
    -Objdump $Objdump

New-Item -ItemType Directory -Force -Path "$stage/deps/bin", "$stage/deps/dll", "$stage/deps/models" | Out-Null
Move-Item -LiteralPath "$stage/miliastra-login-helper.exe" -Destination "$stage/deps/bin"
Get-ChildItem -LiteralPath $stage -Filter "*.dll" -File |
    Move-Item -Destination "$stage/deps/dll"
Copy-Item -LiteralPath (Join-Path $root "config.example.yaml") -Destination "$stage/config.yaml"
Copy-Item -LiteralPath (Join-Path $root "assets") -Destination "$stage/deps" -Recurse
foreach ($name in $modelNames) {
    Copy-Item -LiteralPath (Join-Path $root "models/$name") -Destination "$stage/deps/models"
}

# openvino 运行时(如存在于 deps/openvino)
if (Test-Path -LiteralPath "$root/deps/openvino" -PathType Container) {
    Copy-Item -LiteralPath "$root/deps/openvino" -Destination "$stage/deps" -Recurse
}

# 文档与示例
New-Item -ItemType Directory -Force -Path "$stage/deps/docs" | Out-Null
foreach ($file in @("README.md", "LICENSE", "THIRD_PARTY_NOTICES.md", "turtle_soup.example.yaml", "undercover.example.yaml")) {
    Copy-Item -LiteralPath (Join-Path $root $file) -Destination "$stage/deps/docs"
}
Copy-Item -LiteralPath (Join-Path $root "docs") -Destination "$stage/deps/docs" -Recurse

Write-Host "== 3/3 完成 =="
Write-Host "发布目录: $stage"
