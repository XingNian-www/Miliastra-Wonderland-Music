[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$FfmpegRoot,

    [string]$MinGwRuntime = (Join-Path $PSScriptRoot "..\vendor\ffmpeg\8.1\windows-x64\bin\libwinpthread-1.dll"),

    [string]$Executable = (Join-Path $PSScriptRoot "..\target\release\miliastra-wonderland-music.exe"),

    [string]$LoginHelper = (Join-Path $PSScriptRoot "..\target\release\miliastra-login-helper.exe"),

    [string]$KugouApi = (Join-Path $PSScriptRoot "..\target\kugou-api.exe"),

    [string]$Mnn = (Join-Path $PSScriptRoot "..\vendor\mnn\3.6.0\windows-x64\bin\MNN.dll"),

    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$Destination,

    [string]$Objdump = "objdump"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$mediaNames = @(
    "avformat-62.dll"
    "avcodec-62.dll"
    "avutil-60.dll"
    "swresample-6.dll"
)
$forbiddenImportPattern = "^(libmpv|webview2loader|avfilter|avdevice|swscale)(-.*)?\.dll$"

function Get-ExistingFile {
    param(
        [string]$Path,
        [string]$Label
    )

    $item = Get-Item -LiteralPath $Path
    if ($item.PSIsContainer) {
        throw "$Label must be a file: $Path"
    }
    return $item
}

function Get-SingleMediaDll {
    param(
        [string]$BinDirectory,
        [string]$Name
    )

    $matches = @(Get-ChildItem -LiteralPath (Join-Path $BinDirectory $Name) -File)
    if ($matches.Count -ne 1) {
        throw "expected exactly one $Name under $BinDirectory; found $($matches.Count)"
    }
    return $matches[0]
}

$objdumpCommand = Get-Command -Name $Objdump -CommandType Application
$objdumpPath = $objdumpCommand.Source

function Get-PeImports {
    param([string]$Path)

    $output = & $objdumpPath -p $Path
    if ($LASTEXITCODE -ne 0) {
        throw "objdump failed for $Path"
    }

    return @(
        foreach ($line in $output) {
            if ($line -match "DLL Name:\s*(.+)$") {
                $Matches[1].Trim().ToLowerInvariant()
            }
        }
    )
}

function Test-SystemImport {
    param([string]$Import)

    return $Import -match "^(api-ms-win|ext-ms-win)-.*\.dll$" -or
        $Import -match "^(advapi32|bcrypt|bcryptprimitives|cfgmgr32|combase|crypt32|dbghelp|d3dcompiler_47|dwmapi|gdi32|imm32|kernel32|kernelbase|iphlpapi|mmdevapi|msvcp[0-9_]*|msvcrt|ncrypt|ntdll|ole32|oleaut32|psapi|propsys|rpcrt4|secur32|setupapi|shell32|shlwapi|ucrtbase|user32|userenv|version|vcruntime[0-9_]*|winhttp|wininet|winmm|wintrust|ws2_32)\.dll$"
}

function Assert-NoForbiddenImport {
    param(
        [string]$FileName,
        [string[]]$Imports
    )

    $forbidden = @($Imports | Where-Object { $_ -match $forbiddenImportPattern })
    if ($forbidden.Count -ne 0) {
        throw "$FileName imports an excluded media or legacy loader DLL: $($forbidden -join ', ')"
    }
}

$ffmpegRootItem = Get-Item -LiteralPath $FfmpegRoot
if (-not $ffmpegRootItem.PSIsContainer) {
    throw "FFmpeg root must be a directory: $FfmpegRoot"
}
$ffmpegBin = Join-Path $ffmpegRootItem.FullName "bin"
if (-not (Test-Path -LiteralPath $ffmpegBin -PathType Container)) {
    throw "FFmpeg root has no bin directory: $ffmpegBin"
}

$executableFile = Get-ExistingFile $Executable "main executable"
$loginHelperFile = Get-ExistingFile $LoginHelper "login helper executable"
$kugouApiFile = Get-ExistingFile $KugouApi "Kugou API executable"
$mnnFile = Get-ExistingFile $Mnn "MNN runtime"
$minGwRuntimeFile = Get-ExistingFile $MinGwRuntime "MinGW runtime"
if ($minGwRuntimeFile.Name.ToLowerInvariant() -ne "libwinpthread-1.dll") {
    throw "MinGW runtime must be libwinpthread-1.dll"
}
$mediaFiles = @(
    Get-SingleMediaDll $ffmpegBin "avformat-62.dll"
    Get-SingleMediaDll $ffmpegBin "avcodec-62.dll"
    Get-SingleMediaDll $ffmpegBin "avutil-60.dll"
    Get-SingleMediaDll $ffmpegBin "swresample-6.dll"
)

if (Test-Path -LiteralPath $Destination) {
    if (@(Get-ChildItem -LiteralPath $Destination -Force).Count -ne 0) {
        throw "destination must be empty so stale DLLs cannot expand the runtime closure: $Destination"
    }
} else {
    New-Item -ItemType Directory -Path $Destination | Out-Null
}
$destinationDirectory = (Resolve-Path -LiteralPath $Destination).Path

$filesToCopy = @(
    $executableFile
    $loginHelperFile
    $kugouApiFile
    $mnnFile
    $mediaFiles
    $minGwRuntimeFile
)
foreach ($file in $filesToCopy) {
    Copy-Item -LiteralPath $file.FullName -Destination $destinationDirectory
}

$expectedFiles = @(
    $executableFile.Name.ToLowerInvariant()
    $loginHelperFile.Name.ToLowerInvariant()
    $kugouApiFile.Name.ToLowerInvariant()
    "mnn.dll"
    $mediaNames | ForEach-Object { $_.ToLowerInvariant() }
    $minGwRuntimeFile.Name.ToLowerInvariant()
)
$actualFiles = @(
    Get-ChildItem -LiteralPath $destinationDirectory -File |
        ForEach-Object { $_.Name.ToLowerInvariant() }
)
$unexpectedFiles = @($actualFiles | Where-Object { $expectedFiles -notcontains $_ })
$missingFiles = @($expectedFiles | Where-Object { $actualFiles -notcontains $_ })
if ($unexpectedFiles.Count -ne 0 -or $missingFiles.Count -ne 0) {
    throw "runtime file set mismatch; unexpected=[$($unexpectedFiles -join ', ')] missing=[$($missingFiles -join ', ')]"
}

$packagedDlls = @{}
foreach ($file in Get-ChildItem -LiteralPath $destinationDirectory -Filter "*.dll" -File) {
    $packagedDlls[$file.Name.ToLowerInvariant()] = $file.FullName
}

$mainImports = Get-PeImports (Join-Path $destinationDirectory $executableFile.Name)
foreach ($mediaName in $mediaNames) {
    if ($mainImports -notcontains $mediaName.ToLowerInvariant()) {
        throw "main executable does not import expected FFmpeg DLL: $mediaName"
    }
}

$roots = @(
    (Join-Path $destinationDirectory $executableFile.Name)
    (Join-Path $destinationDirectory $loginHelperFile.Name)
    (Join-Path $destinationDirectory $kugouApiFile.Name)
    (Join-Path $destinationDirectory $mnnFile.Name)
)
$visited = @{}
$pending = [System.Collections.Generic.Queue[string]]::new()
foreach ($root in $roots) {
    $pending.Enqueue($root)
}
while ($pending.Count -gt 0) {
    $file = $pending.Dequeue()
    $key = $file.ToLowerInvariant()
    if ($visited.ContainsKey($key)) {
        continue
    }
    $visited[$key] = $true
    $imports = Get-PeImports $file
    Assert-NoForbiddenImport (Split-Path -Leaf $file) $imports
    foreach ($import in $imports) {
        if ($packagedDlls.ContainsKey($import)) {
            $pending.Enqueue($packagedDlls[$import])
        } elseif (-not (Test-SystemImport $import)) {
            throw "$(Split-Path -Leaf $file) has an unexpected non-system dependency: $import"
        }
    }
}

Write-Output "Packaged native playback runtime in $destinationDirectory"
Write-Output "Files: $($expectedFiles -join ', ')"
