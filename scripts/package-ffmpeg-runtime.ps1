[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$FfmpegRoot,

    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$MinGwRuntime,

    [string]$Executable = (Join-Path $PSScriptRoot "..\target\release\miliastra-playerd.exe"),

    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$Destination,

    [string]$Objdump = "objdump"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

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
        [string]$Pattern
    )

    $matches = @(Get-ChildItem -Path (Join-Path $BinDirectory $Pattern) -File)
    if ($matches.Count -ne 1) {
        throw "expected exactly one $Pattern under $BinDirectory; found $($matches.Count)"
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

$ffmpegRootItem = Get-Item -LiteralPath $FfmpegRoot
if (-not $ffmpegRootItem.PSIsContainer) {
    throw "FFmpeg root must be a directory: $FfmpegRoot"
}
$ffmpegBin = Join-Path $ffmpegRootItem.FullName "bin"
if (-not (Test-Path -LiteralPath $ffmpegBin -PathType Container)) {
    throw "FFmpeg root has no bin directory: $ffmpegBin"
}

$executableFile = Get-ExistingFile $Executable "executable"
$minGwRuntimeFile = Get-ExistingFile $MinGwRuntime "MinGW runtime"
$mediaFiles = @(
    Get-SingleMediaDll $ffmpegBin "avformat-*.dll"
    Get-SingleMediaDll $ffmpegBin "avcodec-*.dll"
    Get-SingleMediaDll $ffmpegBin "avutil-*.dll"
    Get-SingleMediaDll $ffmpegBin "swresample-*.dll"
)

if (Test-Path -LiteralPath $Destination) {
    if (@(Get-ChildItem -LiteralPath $Destination -Force).Count -ne 0) {
        throw "destination must be empty so stale DLLs cannot expand the runtime closure: $Destination"
    }
} else {
    New-Item -ItemType Directory -Path $Destination | Out-Null
}
$destinationDirectory = (Resolve-Path -LiteralPath $Destination).Path

Copy-Item -LiteralPath $executableFile.FullName -Destination $destinationDirectory
foreach ($file in $mediaFiles) {
    Copy-Item -LiteralPath $file.FullName -Destination $destinationDirectory
}
Copy-Item -LiteralPath $minGwRuntimeFile.FullName -Destination $destinationDirectory

$expectedMediaImports = @($mediaFiles | ForEach-Object { $_.Name.ToLowerInvariant() })
$exeImports = Get-PeImports (Join-Path $destinationDirectory $executableFile.Name)
foreach ($expected in $expectedMediaImports) {
    if ($exeImports -notcontains $expected) {
        throw "executable does not import expected FFmpeg DLL: $expected"
    }
}
if (@($exeImports | Where-Object { $_ -match "^(avfilter|avdevice|swscale|libmpv).*\.dll$" }).Count -ne 0) {
    throw "executable imports an excluded media DLL: $($exeImports -join ', ')"
}

$allowedExecutableImportPatterns = @(
    "^api-ms-win-.*\.dll$"
    "^ext-ms-.*\.dll$"
    "^(advapi32|bcrypt|bcryptprimitives|combase|crypt32|kernel32|kernelbase|mmdevapi|msvcrt|ncrypt|ntdll|ole32|oleaut32|secur32|user32|vcruntime140|ws2_32)\.dll$"
)
$unexpectedExecutableImports = @(
    $exeImports | Where-Object {
        $import = $_
        ($expectedMediaImports -notcontains $import) -and
        -not ($allowedExecutableImportPatterns | Where-Object { $import -match $_ })
    }
)
if ($unexpectedExecutableImports.Count -ne 0) {
    throw "executable has an unexpected non-system dependency: $($unexpectedExecutableImports -join ', ')"
}

$allowedMediaClosure = @(
    $expectedMediaImports
    $minGwRuntimeFile.Name.ToLowerInvariant()
    "bcrypt.dll"
    "crypt32.dll"
    "kernel32.dll"
    "kernelbase.dll"
    "msvcrt.dll"
    "ncrypt.dll"
    "ntdll.dll"
    "ole32.dll"
    "oleaut32.dll"
    "secur32.dll"
    "user32.dll"
    "ws2_32.dll"
)

foreach ($file in $mediaFiles) {
    $imports = Get-PeImports (Join-Path $destinationDirectory $file.Name)
    $unexpected = @($imports | Where-Object { $allowedMediaClosure -notcontains $_ })
    if ($unexpected.Count -ne 0) {
        throw "$($file.Name) has an unexpected non-system dependency: $($unexpected -join ', ')"
    }
}

$runtimeImports = Get-PeImports (Join-Path $destinationDirectory $minGwRuntimeFile.Name)
$unexpectedRuntimeImports = @(
    $runtimeImports | Where-Object { @("kernel32.dll", "msvcrt.dll", "ntdll.dll") -notcontains $_ }
)
if ($unexpectedRuntimeImports.Count -ne 0) {
    throw "$($minGwRuntimeFile.Name) has an unexpected dependency: $($unexpectedRuntimeImports -join ', ')"
}

Write-Output "Packaged FFmpeg runtime in $destinationDirectory"
Write-Output "Media DLLs: $($expectedMediaImports -join ', '), $($minGwRuntimeFile.Name)"
if ($exeImports -contains "vcruntime140.dll") {
    Write-Warning "The executable imports VCRUNTIME140.dll; install the matching Microsoft Visual C++ Redistributable. This script packages the FFmpeg media closure, not Microsoft system runtimes."
}
