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
# Windows 预编译依赖的路径原先由仓库 `.cargo/config.toml` 的 [env] 提供；那个文件现在
# 是空的（cargo 的 [env] 对所有 target 生效、且不支持按 target 限定，会让 Linux
# 门禁也读到 Windows 路径，见该文件的注释）。所以本地打包必须在调用 cargo 之前
# 自己把同样的值导进进程环境 —— 显式写在这里，而不是依赖用户 shell 里恰好设过。
$prebuiltEnv = @{
    FFMPEG_DIR      = Join-Path $root "vendor/ffmpeg/8.1/windows-x64"
    MNN_INCLUDE_DIR = Join-Path $root "vendor/mnn/3.6.0/windows-x64/include"
    MNN_LIB_DIR     = Join-Path $root "vendor/mnn/3.6.0/windows-x64/lib"
}
foreach ($name in $prebuiltEnv.Keys) {
    $value = $prebuiltEnv[$name]
    if (-not (Test-Path -LiteralPath $value -PathType Container)) {
        throw "预编译依赖目录缺失: $name=$value（vendor/ 是否完整？）"
    }
    Set-Item -Path "Env:$name" -Value $value
    Write-Host "  $name=$value"
}
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

# OpenVINO FP32 IR 模型(存在时随包分发;缺失不阻断,运行期自动回退 MNN CPU)
$openvinoIrModels = @(
    "PP-OCRv6_small_det.xml",
    "PP-OCRv6_small_det.bin",
    "PP-OCRv6_small_rec.xml",
    "PP-OCRv6_small_rec.bin"
)
foreach ($name in $openvinoIrModels) {
    $source = Join-Path $root "models/$name"
    if (Test-Path -LiteralPath $source -PathType Leaf) {
        Copy-Item -LiteralPath $source -Destination "$stage/deps/models"
    } else {
        Write-Warning "缺少 OpenVINO IR 模型,跳过: models/$name(运行期将回退 MNN CPU)"
    }
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
# 注意 `Copy-Item 'docs' … -Recurse` 在目标目录已存在时会把源目录**作为子目录**放进去，
# 产物变成 deps/docs/docs/…（审阅 B00-P0-6）。用通配符展开 docs/ 的**内容**，目标目录
# 保留为 deps/docs/ 本身。
# 这里必须用 `-Path`（会展开 `*`）而不是 `-LiteralPath`（`*` 会被当成字面文件名）。
$docsSource = Join-Path $root "docs"
if (-not (Test-Path -LiteralPath $docsSource -PathType Container)) {
    throw "文档目录缺失: docs/"
}
$docsItems = @(Get-ChildItem -LiteralPath $docsSource -Force)
if ($docsItems.Count -eq 0) {
    throw "文档目录为空: docs/（发布包必须带用户文档）"
}
Copy-Item -Path (Join-Path $docsSource "*") -Destination "$stage/deps/docs" -Recurse -Force

# 复制失败由文件顶部的 `$ErrorActionPreference = "Stop"` 抛异常，这里不需再查
# $LASTEXITCODE（那是给原生命令用的，对 Copy-Item 这类 cmdlet 没有意义）。

# 成品必须带文档：目录会被上一步创建出来，只断言“目录存在”抓不到 docs/docs 这类错位。
$requiredDocs = @("README.md", "LICENSE", "THIRD_PARTY_NOTICES.md", "index.md")
foreach ($name in $requiredDocs) {
    if (-not (Test-Path -LiteralPath "$stage/deps/docs/$name" -PathType Leaf)) {
        throw "发布包缺少文档: deps/docs/$name"
    }
}
if (Test-Path -LiteralPath "$stage/deps/docs/docs" -PathType Container) {
    throw "发布包出现嵌套的 deps/docs/docs/（Copy-Item 路径写法又退化了）"
}

Write-Host "== 3/3 完成 =="
Write-Host "发布目录: $stage"
