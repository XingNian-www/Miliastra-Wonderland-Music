# 发布打包:构建 release 并按发布布局组装,剔除运行期敏感/临时数据。
# 用法: .\scripts\package-release.ps1 [-OutDir dist\miliastra-release]
param(
    [string]$OutDir = "dist/miliastra-release"
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

Write-Host "== 1/3 构建 release =="
cargo build --release --workspace
if ($LASTEXITCODE -ne 0) { throw "cargo build 失败" }

$stage = [System.IO.Path]::GetFullPath((Join-Path $root $OutDir))
$rootFull = [System.IO.Path]::GetFullPath($root).TrimEnd('\', '/')
if ($stage.TrimEnd('\', '/') -eq $rootFull) {
    throw "发布目录不能是项目根目录: $stage"
}
Write-Host "== 2/3 组装发布目录: $stage =="
if (Test-Path -LiteralPath $stage) {
    Remove-Item -LiteralPath $stage -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $stage | Out-Null

# 主程序与配置
Copy-Item target/release/miliastra-wonderland-music.exe $stage
Copy-Item config.yaml $stage
Copy-Item target/release/miliastra-login-helper.exe $stage

# 依赖目录(只复制静态资源,剔除运行期数据)
foreach ($sub in @("dll", "models", "assets")) {
    $src = Join-Path $root "deps/$sub"
    if (Test-Path $src) {
        Copy-Item -Recurse $src (Join-Path $stage "deps/") -Force
    }
}
# openvino 运行时(如存在于 deps/openvino)
if (Test-Path "$root/deps/openvino") {
    Copy-Item -Recurse "$root/deps/openvino" (Join-Path $stage "deps/") -Force
}

# 文档与示例
New-Item -ItemType Directory -Force -Path "$stage/deps/docs" | Out-Null
Copy-Item LICENSE $stage/deps/docs
Copy-Item THIRD_PARTY_NOTICES.md $stage/deps/docs
foreach ($f in @("turtle_soup.example.yaml", "undercover.example.yaml")) {
    if (Test-Path "$root/$f") { Copy-Item "$root/$f" $stage/deps/docs }
}
Copy-Item "$root/deps/playback.example.yaml" $stage/deps -ErrorAction SilentlyContinue

# 运行期目录只建空骨架(首次启动自动生成)
New-Item -ItemType Directory -Force -Path "$stage/deps/data" | Out-Null
New-Item -ItemType Directory -Force -Path "$stage/deps/logs" | Out-Null
New-Item -ItemType Directory -Force -Path "$stage/deps/cache" | Out-Null
Set-Content -Path "$stage/deps/data/README.txt" -Value "本目录存放运行期数据(配置库/凭证/缓存),打包时不包含,首次启动自动生成。分发时请勿附带本机凭证。" -Encoding utf8

Write-Host "== 3/3 完成 =="
Write-Host "发布目录: $stage"
Write-Host "注意: 分发前确认 deps/data 下无凭证/日志残留;凭证文件应仅存在于本机运行实例。"