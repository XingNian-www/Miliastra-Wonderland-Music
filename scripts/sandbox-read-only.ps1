param(
    [Parameter(Mandatory=$true)][string]$BaseUrl,
    [ValidateSet('active','push','download','off')][string]$State='active',
    [string]$OutputDirectory='technical-docs/sandbox-read-only/evidence',
    [string]$EvidenceId='',
    [string]$Label='read-only-observation',
    [string]$Rect='',
    [switch]$NoOcr,
    [long]$Since=0,
    [string]$Instance=''
)
$ErrorActionPreference='Stop'
$taskEndpoint=$BaseUrl.TrimEnd('/')
$taskHeaders=@{}
if ($env:MILIASTRA_HTTP_TOKEN) { $taskHeaders['X-Miliastra-Token']=$env:MILIASTRA_HTTP_TOKEN }
$null=New-Item -ItemType Directory -Force -Path $OutputDirectory
function Invoke-ReadonlyJson($Path,$Body=$null) {
    $taskOptions=@{Uri=($taskEndpoint+$Path);Headers=$taskHeaders;Method='Get';SkipHttpErrorCheck=$true}
    if ($null -ne $Body) {
        $taskOptions.Method='Post';$taskOptions.ContentType='application/json; charset=utf-8'
        $taskOptions.Body=$Body|ConvertTo-Json -Depth 20 -Compress
    }
    $taskHttp=Invoke-WebRequest @taskOptions
    $taskValue=$taskHttp.Content|ConvertFrom-Json -Depth 100
    if ($taskHttp.StatusCode -lt 200 -or $taskHttp.StatusCode -ge 300 -or $taskValue.ok -ne $true) {
        $taskException=[System.Exception]::new(('HTTP '+$taskHttp.StatusCode+': '+$taskValue.error+' '+$taskValue.message))
        $taskException.Data['response']=$taskValue
        throw $taskException
    }
    return $taskValue
}
function Get-ReadonlySection([string]$Id,[string]$Section,[hashtable]$Snapshot) {
    [long]$taskCursor=0
    $taskBudget=8192
    $taskText=[System.Text.StringBuilder]::new()
    while ($true) {
        $taskPath='/debug/read-only/evidence?id='+[uri]::EscapeDataString($Id)+'&section='+$Section+'&cursor='+$taskCursor+'&limit=10&maxBytes='+$taskBudget
        if ($Snapshot.ContainsKey('revision')) { $taskPath+='&revision='+$Snapshot.revision }
        try { $taskValue=Invoke-ReadonlyJson $taskPath }
        catch {
            $taskError=$_.Exception.Data['response']
            if ($taskError.error -eq 'page_budget_too_small' -and $taskError.cursor -eq $taskCursor -and $taskError.nextCursor -eq $taskCursor -and $taskBudget -lt 65536) {
                $taskBudget=[Math]::Min(65536,$taskBudget*2)
                continue
            }
            throw
        }
        $taskEvidence=$taskValue.evidence;$taskPage=$taskEvidence.page
        if ($taskEvidence.id -ne $Id -or $taskEvidence.section -ne $Section -or !$taskEvidence.instance -or $null -eq $taskEvidence.revision -or $taskPage.encoding -ne 'json-utf8' -or $taskPage.revision -ne $taskEvidence.revision -or $taskPage.cursor -ne $taskCursor -or $taskPage.done -isnot [bool] -or $taskPage.items -isnot [array]) { throw 'Invalid evidence page envelope' }
        if ($Snapshot.ContainsKey('instance') -and $Snapshot.instance -ne $taskEvidence.instance) { throw 'instance conflict: restart the complete download' }
        if ($Snapshot.ContainsKey('revision') -and $Snapshot.revision -ne $taskEvidence.revision) { throw 'revision conflict: restart the complete download' }
        $Snapshot.instance=[string]$taskEvidence.instance;$Snapshot.revision=$taskEvidence.revision
        $taskOffset=$taskCursor
        foreach ($taskItem in $taskPage.items) {
            if ($taskItem.section -ne $Section -or $taskItem.offset -ne $taskOffset -or $taskItem.text -isnot [string]) { throw 'Non-contiguous UTF-8 section fragment' }
            $null=$taskText.Append($taskItem.text)
            $taskOffset+=[System.Text.Encoding]::UTF8.GetByteCount($taskItem.text)
        }
        if ($taskPage.nextCursor -ne $taskOffset -or (!$taskPage.done -and $taskOffset -le $taskCursor)) { throw 'Invalid UTF-8 byte cursor progress' }
        if ($taskPage.done) { return ($taskText.ToString()|ConvertFrom-Json -Depth 100 -NoEnumerate) }
        $taskCursor=$taskPage.nextCursor
    }
}
function Get-ReadonlyEvidence([string]$Id,[string]$Instance='') {
    $taskSnapshot=@{}
    if ($Instance) { $taskSnapshot.instance=$Instance }
    $taskMetadata=Get-ReadonlySection $Id 'metadata' $taskSnapshot
    $taskOcr=Get-ReadonlySection $Id 'ocr' $taskSnapshot
    $taskParameters=Get-ReadonlySection $Id 'parameters' $taskSnapshot
    $taskHistory=Get-ReadonlySection $Id 'history' $taskSnapshot
    if ($taskMetadata.id -ne $Id -or $null -eq $taskParameters.task -or $taskHistory.history -isnot [array]) { throw 'Invalid complete evidence sections' }
    $taskResult=@{}
    foreach ($taskProperty in $taskMetadata.PSObject.Properties) { $taskResult[$taskProperty.Name]=$taskProperty.Value }
    $taskTask=@{}
    foreach ($taskProperty in $taskParameters.task.PSObject.Properties) { $taskTask[$taskProperty.Name]=$taskProperty.Value }
    if ($null -ne $taskOcr) { $taskTask.ocr=$taskOcr }
    $taskResult.task=$taskTask
    foreach ($taskProperty in $taskHistory.PSObject.Properties) { $taskResult[$taskProperty.Name]=$taskProperty.Value }
    return [pscustomobject]@{evidence=$taskResult;snapshot=$taskSnapshot}
}
function Save-ReadonlyEvidence([string]$Id,[string]$Instance='') {
    $taskFull=Get-ReadonlyEvidence $Id $Instance
    $taskEvidence=$taskFull.evidence;$taskSnapshot=$taskFull.snapshot
    $taskSafeInstance=$taskSnapshot.instance -replace '[^a-zA-Z0-9._-]','_'
    $taskSafeId=$Id -replace '[^a-zA-Z0-9._-]','_'
    $taskFolder=Join-Path (Join-Path $OutputDirectory $taskSafeInstance) $taskSafeId
    $null=New-Item -ItemType Directory -Force -Path $taskFolder
    $taskPng=Join-Path $taskFolder 'original.png'
    $taskHashPath=Join-Path $taskFolder 'file-sha256.json'
    $taskTemp=Join-Path $taskFolder ('image-'+[guid]::NewGuid().ToString('N')+'.part')
    $taskSource=$taskEvidence.source;$taskHasImage=$false
    try {
        if ($taskSource -and $taskEvidence.imageRetained -ne $false -and $taskSource.imageRetained -ne $false) {
            $taskImagePath='/debug/read-only/image?id='+[uri]::EscapeDataString($Id)+'&instance='+[uri]::EscapeDataString($taskSnapshot.instance)+'&revision='+$taskSnapshot.revision
            $taskHttp=Invoke-WebRequest -Uri ($taskEndpoint+$taskImagePath) -Headers $taskHeaders -OutFile $taskTemp -PassThru -SkipHttpErrorCheck
            if ($taskHttp.StatusCode -lt 200 -or $taskHttp.StatusCode -ge 300) {
                $taskImageError=Get-Content -LiteralPath $taskTemp -Raw|ConvertFrom-Json
                if ([string]$taskImageError.error -notlike 'image_unavailable*') { throw ('Image HTTP '+$taskHttp.StatusCode+': '+$taskImageError.error) }
                $taskEvidence.missingReason=[string]$taskImageError.error
                $taskEvidence.imageRetained=$false
                $taskSource|Add-Member -NotePropertyName imageRetained -NotePropertyValue $false -Force
            } else {
                if (($taskHttp.Headers['X-ReadOnly-Instance'] -join '') -ne $taskSnapshot.instance -or ($taskHttp.Headers['X-ReadOnly-Revision'] -join '') -ne [string]$taskSnapshot.revision) { throw 'Image instance/revision conflict' }
                $taskPixelSha=$taskHttp.Headers['X-Pixel-Sha256'] -join ''
                $taskFileSha=$taskHttp.Headers['X-File-Sha256'] -join ''
                if ($taskPixelSha -notmatch '^[a-fA-F0-9]{64}$' -or $taskFileSha -notmatch '^[a-fA-F0-9]{64}$') { throw 'Invalid PNG SHA-256 headers' }
                $taskBytes=[System.IO.File]::ReadAllBytes($taskTemp)
                $taskSignature=137,80,78,71,13,10,26,10
                if ($taskBytes.Length -lt 8) { throw 'Image route did not return a PNG' }
                for ($taskIndex=0;$taskIndex -lt 8;$taskIndex++) { if ($taskBytes[$taskIndex] -ne $taskSignature[$taskIndex]) { throw 'Image route did not return a PNG' } }
                $taskFileHash=Get-FileHash -LiteralPath $taskTemp -Algorithm SHA256
                if ($taskFileHash.Hash -ne $taskFileSha) { throw 'PNG file SHA-256 mismatch' }
                Move-Item -LiteralPath $taskTemp -Destination $taskPng -Force
                [pscustomobject]@{algorithm='SHA256';fileSha256=$taskFileHash.Hash.ToLowerInvariant();pixelSha256=$taskPixelSha;instance=$taskSnapshot.instance;revision=$taskSnapshot.revision} | ConvertTo-Json | Set-Content -LiteralPath $taskHashPath -Encoding utf8
                $taskSource|Add-Member -NotePropertyName pixelSha256 -NotePropertyValue $taskPixelSha -Force
                $taskSource|Add-Member -NotePropertyName fileSha256 -NotePropertyValue $taskFileSha -Force
                $taskSource|Add-Member -NotePropertyName imagePngBase64 -NotePropertyValue ([Convert]::ToBase64String($taskBytes)) -Force
                $taskHasImage=$true
            }
        }
        if (!$taskHasImage) {
            foreach ($taskOldFile in @($taskPng,$taskHashPath)) { if (Test-Path -LiteralPath $taskOldFile) { Remove-Item -LiteralPath $taskOldFile } }
            if (!$taskEvidence.missingReason) { $taskEvidence.missingReason='exact_source_frame_not_retained' }
        }
        $taskEvidence|ConvertTo-Json -Depth 100 | Set-Content -LiteralPath (Join-Path $taskFolder 'evidence.json') -Encoding utf8
        [pscustomobject]@{directory=$taskFolder;id=$Id;instance=$taskSnapshot.instance;revision=$taskSnapshot.revision;frameId=$taskSource.frameId;width=$taskSource.width;height=$taskSource.height;sourceMissing=(!$taskHasImage);missingReason=$taskEvidence.missingReason} | ConvertTo-Json -Compress
    } finally { if (Test-Path -LiteralPath $taskTemp) { Remove-Item -LiteralPath $taskTemp } }
}
function Get-ReadonlyEntries([long]$Cursor,[string]$Instance='') {
    $taskEvents=[System.Collections.Generic.List[object]]::new();$taskPages=[System.Collections.Generic.List[object]]::new()
    $taskBudget=8192;$taskLatest=$null
    while ($true) {
        $taskPath='/debug/read-only/entries?since='+$Cursor+'&limit=10&maxBytes='+$taskBudget
        if ($Instance) { $taskPath+='&instance='+[uri]::EscapeDataString($Instance) }
        try { $taskValue=Invoke-ReadonlyJson $taskPath }
        catch {
            $taskError=$_.Exception.Data['response']
            if ($taskError.error -eq 'page_budget_too_small' -and $taskError.cursor -eq $Cursor -and $taskError.nextCursor -eq $Cursor -and $taskBudget -lt 65536) { $taskBudget=[Math]::Min(65536,$taskBudget*2);continue }
            throw
        }
        $taskEntries=$taskValue.entries
        if (!$taskEntries.instance -or $taskEntries.events -isnot [array] -or $taskEntries.hasMore -isnot [bool] -or $null -eq $taskEntries.nextCursor) { throw 'Invalid entries page' }
        if ($taskEntries.reset -or ($Instance -and $Instance -ne $taskEntries.instance)) { throw 'entries instance reset: rerun from Since 0' }
        if ($taskEntries.gap) { Write-Warning ('Event replay has a gap: '+($taskEntries.gap|ConvertTo-Json -Compress)) }
        $Instance=[string]$taskEntries.instance
        foreach ($taskEvent in $taskEntries.events) { $taskEvents.Add($taskEvent) }
        $taskPages.Add($taskEntries);$taskLatest=$taskEntries
        if (!$taskEntries.hasMore) { break }
        if ($taskEntries.nextCursor -le $Cursor) { throw 'entries cursor failed to advance' }
        $Cursor=$taskEntries.nextCursor
    }
    return [pscustomobject]@{ok=$true;entries=@{events=$taskEvents.ToArray();nextCursor=$taskLatest.nextCursor;hasMore=$false;instance=$Instance;revision=$taskLatest.revision;gap=@($taskPages|Where-Object gap|ForEach-Object gap);reset=$false;latestSequence=$taskLatest.latestSequence;loss=$taskLatest.loss};pages=$taskPages.ToArray()}
}
if ($State -eq 'off') {
    Invoke-ReadonlyJson '/debug/read-only/mode' @{enabled=$false;state='active'} | ConvertTo-Json -Depth 10
    return
}
if ($State -eq 'download') {
    if (!$EvidenceId) { throw 'download必须提供EvidenceId' }
    Save-ReadonlyEvidence $EvidenceId $Instance
    return
}
$taskMode=Invoke-ReadonlyJson '/debug/read-only/mode' @{enabled=$true;state=$State}
$taskMode | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'mode.json') -Encoding utf8
if ($State -eq 'active') {
    $taskBody=@{label=$Label;ocr=(!$NoOcr);rect=$null;response='metadata'}
    if ($Rect) { $taskBody.rect=$Rect|ConvertFrom-Json }
    $taskBody | ConvertTo-Json -Depth 15 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'request.json') -Encoding utf8
    $taskResponse=Invoke-ReadonlyJson '/debug/read-only/read' $taskBody
    Save-ReadonlyEvidence $taskResponse.evidence.id $taskResponse.evidence.instance
    return
}
# Drain bounded event replay once; the tool page subscribes to live SSE.
if ($Since -gt 0 -and !$Instance) {
    $taskPreviousReplayPath=Join-Path $OutputDirectory 'entries.json'
    if (Test-Path -LiteralPath $taskPreviousReplayPath) {
        $taskPreviousReplay=Get-Content -LiteralPath $taskPreviousReplayPath -Raw|ConvertFrom-Json -Depth 100
        $Instance=[string]$taskPreviousReplay.entries.instance
    }
    if (!$Instance) { throw 'Since续传必须提供上次entries.instance（-Instance），或使用相同OutputDirectory中的entries.json；首次请用Since 0' }
}
$taskReplayInstance=if ($Instance) { $Instance } else { [string]$taskMode.mode.instance }
$taskReplay=Get-ReadonlyEntries $Since $taskReplayInstance
$taskReplay | ConvertTo-Json -Depth 100 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'entries.json') -Encoding utf8
$taskSeen=@{}
foreach ($taskEvent in $taskReplay.entries.events) {
    $taskPayload=$taskEvent.payload
    if ($taskPayload.id -and !$taskSeen.ContainsKey($taskPayload.id)) {
        $taskSeen[$taskPayload.id]=$true
        Save-ReadonlyEvidence $taskPayload.id $taskReplay.entries.instance
    }
}
