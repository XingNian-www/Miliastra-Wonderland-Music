param(
    [Parameter(Mandatory=$true)][string]$BaseUrl,
    [ValidateSet('active','push','download','off')][string]$State='active',
    [string]$OutputDirectory='technical-docs/sandbox-read-only/evidence',
    [string]$EvidenceId='',
    [string]$Label='read-only-observation',
    [string]$Rect='',
    [switch]$NoOcr,
    [long]$Since=0
)
$ErrorActionPreference='Stop'
$taskEndpoint=$BaseUrl.TrimEnd('/')
$taskHeaders=@{}
if ($env:MILIASTRA_HTTP_TOKEN) { $taskHeaders['X-Miliastra-Token']=$env:MILIASTRA_HTTP_TOKEN }
$null=New-Item -ItemType Directory -Force -Path $OutputDirectory
function Invoke-ReadonlyJson($Path,$Body=$null) {
    if ($null -eq $Body) { return Invoke-RestMethod -Uri ($taskEndpoint+$Path) -Headers $taskHeaders -Method Get }
    return Invoke-RestMethod -Uri ($taskEndpoint+$Path) -Headers $taskHeaders -Method Post -ContentType 'application/json; charset=utf-8' -Body ($Body|ConvertTo-Json -Depth 20 -Compress)
}
function Save-ReadonlyEvidence($Evidence) {
    $taskId=[string]$Evidence.id
    if (!$taskId) { $taskId='observation-'+[datetime]::UtcNow.ToString('yyyyMMddTHHmmssfff') }
    $taskSafeId=$taskId -replace '[^a-zA-Z0-9._-]','_'
    $taskFolder=Join-Path $OutputDirectory $taskSafeId
    $null=New-Item -ItemType Directory -Force -Path $taskFolder
    $Evidence|ConvertTo-Json -Depth 70 | Set-Content -LiteralPath (Join-Path $taskFolder 'evidence.json') -Encoding utf8
    $taskSource=$Evidence.source
    if ($taskSource.imagePngBase64) {
        [System.IO.File]::WriteAllBytes((Join-Path $taskFolder 'original.png'),[Convert]::FromBase64String($taskSource.imagePngBase64))
        Get-FileHash -LiteralPath (Join-Path $taskFolder 'original.png') -Algorithm SHA256 | Select-Object Algorithm,Hash | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $taskFolder 'file-sha256.json') -Encoding utf8
    }
    [pscustomobject]@{ directory=$taskFolder; id=$taskId; frameId=$taskSource.frameId; width=$taskSource.width; height=$taskSource.height; sourceMissing=(!$taskSource.imagePngBase64) } | ConvertTo-Json -Compress
}
if ($State -eq 'off') {
    Invoke-ReadonlyJson '/debug/read-only/mode' @{enabled=$false;state='active'} | ConvertTo-Json -Depth 10
    return
}
if ($State -eq 'download') {
    if (!$EvidenceId) { throw 'download必须提供EvidenceId' }
    $taskResponse=Invoke-ReadonlyJson ('/debug/read-only/evidence?id='+[uri]::EscapeDataString($EvidenceId))
    Save-ReadonlyEvidence $taskResponse.evidence
    return
}
$taskMode=Invoke-ReadonlyJson '/debug/read-only/mode' @{enabled=$true;state=$State}
$taskMode | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'mode.json') -Encoding utf8
if ($State -eq 'active') {
    $taskBody=@{label=$Label;ocr=(!$NoOcr);rect=$null}
    if ($Rect) { $taskBody.rect=$Rect|ConvertFrom-Json }
    $taskBody | ConvertTo-Json -Depth 15 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'request.json') -Encoding utf8
    $taskResponse=Invoke-ReadonlyJson '/debug/read-only/read' $taskBody
    Save-ReadonlyEvidence $taskResponse.evidence
    return
}
# Only command/flow events are emitted by this dedicated SSE endpoint.
# This script drains replay once so it is bounded and reproducible; the tool page streams live events.
$taskStream=Invoke-WebRequest -Uri ($taskEndpoint+'/debug/read-only/events?drain=true&since='+$Since) -Headers $taskHeaders
$taskStream.Content | Set-Content -LiteralPath (Join-Path $OutputDirectory 'events.sse') -Encoding utf8
$taskSeen=@{}
foreach ($taskLine in ($taskStream.Content -split '\r?\n')) {
    if (!$taskLine.StartsWith('data: ')) { continue }
    $taskEvent=$taskLine.Substring(6)|ConvertFrom-Json
    if ($taskEvent.evidenceUrl -and !$taskSeen.ContainsKey($taskEvent.evidenceUrl)) {
        $taskSeen[$taskEvent.evidenceUrl]=$true
        $taskEvidence=Invoke-ReadonlyJson $taskEvent.evidenceUrl
        Save-ReadonlyEvidence $taskEvidence.evidence
    }
}
Invoke-ReadonlyJson ('/debug/read-only/entries?since='+$Since) | ConvertTo-Json -Depth 20 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'entries.json') -Encoding utf8
