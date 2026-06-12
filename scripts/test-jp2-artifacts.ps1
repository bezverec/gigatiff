param(
    [string]$CandidateBaseUrl = "http://127.0.0.1:18110",
    [string]$ReferenceBaseUrl = "http://127.0.0.1:18111",
    [string[]]$ImageIds = @(
        "mapa2_no_xmp_clean_master.jp2",
        "mapa2_master.jp2",
        "mapa2_no_xmp_clean_user_1_8.jp2",
        "mapa2_user_1_8.jp2",
        "mc_vkol-00b10d_0001.jp2"
    ),
    [string]$OutDir = "target/jp2-artifact-tests",
    [int]$BadPixelDelta = 32,
    [double]$MeanAbsThreshold = 8.0,
    [double]$BadPixelRatioThreshold = 0.05,
    [int]$SampleStep = 1,
    [switch]$IncludeFull512,
    [switch]$IncludeFull1024,
    [switch]$Json
)

$ErrorActionPreference = "Stop"

Add-Type -AssemblyName System.Drawing

function Escape-ImageId {
    param([string]$ImageId)
    return [Uri]::EscapeDataString($ImageId).Replace("%2F", "/")
}

function Get-HeaderString {
    param(
        [object]$Headers,
        [string]$Name
    )

    $value = ($Headers[$Name] -join ",")
    if ([string]::IsNullOrWhiteSpace($value)) {
        return $null
    }
    return $value
}

function Get-HeaderDouble {
    param(
        [object]$Headers,
        [string]$Name
    )

    $value = Get-HeaderString $Headers $Name
    if ([string]::IsNullOrWhiteSpace($value)) {
        return $null
    }
    return [double]::Parse($value, [Globalization.CultureInfo]::InvariantCulture)
}

function Invoke-ImageRequest {
    param(
        [string]$BaseUrl,
        [string]$Path,
        [string]$OutFile
    )

    $sw = [Diagnostics.Stopwatch]::StartNew()
    $response = Invoke-WebRequest -UseBasicParsing -SkipHttpErrorCheck -Uri "$BaseUrl/$Path" -OutFile $OutFile -PassThru
    $sw.Stop()

    [pscustomobject]@{
        Status = [int]$response.StatusCode
        Ms = [math]::Round($sw.Elapsed.TotalMilliseconds, 2)
        Bytes = if (Test-Path -LiteralPath $OutFile) { (Get-Item -LiteralPath $OutFile).Length } else { 0 }
        ContentType = Get-HeaderString $response.Headers "Content-Type"
        Cache = Get-HeaderString $response.Headers "x-gigatiff-cache"
        Jp2Backend = Get-HeaderString $response.Headers "x-gigatiff-jp2-backend"
        OpenJpegThreads = Get-HeaderString $response.Headers "x-gigatiff-openjpeg-threads"
        ServerTotalMs = Get-HeaderDouble $response.Headers "x-gigatiff-total-ms"
        ServerRenderMs = Get-HeaderDouble $response.Headers "x-gigatiff-render-ms"
        ServerEncodeMs = Get-HeaderDouble $response.Headers "x-gigatiff-encode-ms"
        Error = if ([int]$response.StatusCode -ge 400) { Get-Content -LiteralPath $OutFile -Raw -ErrorAction SilentlyContinue } else { $null }
    }
}

function New-RegionPath {
    param(
        [string]$EncodedId,
        [int]$X,
        [int]$Y,
        [int]$Width,
        [int]$Height,
        [int]$OutputWidth
    )

    $safeOutputWidth = [math]::Max(1, [math]::Min($OutputWidth, $Width))
    return "iiif/3/$EncodedId/$X,$Y,$Width,$Height/$safeOutputWidth,/0/default.png"
}

function Add-RegionCase {
    param(
        [System.Collections.Generic.List[object]]$Cases,
        [string]$Name,
        [string]$EncodedId,
        [int]$ImageWidth,
        [int]$ImageHeight,
        [int]$X,
        [int]$Y,
        [int]$RegionSize,
        [int]$OutputWidth
    )

    if ($X -ge $ImageWidth -or $Y -ge $ImageHeight) {
        return
    }

    $width = [math]::Min($RegionSize, $ImageWidth - $X)
    $height = [math]::Min($RegionSize, $ImageHeight - $Y)
    if ($width -le 0 -or $height -le 0) {
        return
    }

    $Cases.Add([pscustomobject]@{
        Name = $Name
        Path = New-RegionPath -EncodedId $EncodedId -X $X -Y $Y -Width $width -Height $height -OutputWidth $OutputWidth
        X = $X
        Y = $Y
        RegionWidth = $width
        RegionHeight = $height
        OutputWidth = [math]::Max(1, [math]::Min($OutputWidth, $width))
    }) | Out-Null
}

function New-TestCases {
    param(
        [string]$EncodedId,
        [int]$ImageWidth,
        [int]$ImageHeight
    )

    $cases = New-Object System.Collections.Generic.List[object]
    if ($IncludeFull512) {
        $cases.Add([pscustomobject]@{
            Name = "full_512"
            Path = "iiif/3/$EncodedId/full/512,/0/default.png"
            X = $null
            Y = $null
            RegionWidth = $ImageWidth
            RegionHeight = $ImageHeight
            OutputWidth = 512
        }) | Out-Null
    }

    if ($IncludeFull1024) {
        $cases.Add([pscustomobject]@{
            Name = "full_1024"
            Path = "iiif/3/$EncodedId/full/1024,/0/default.png"
            X = $null
            Y = $null
            RegionWidth = $ImageWidth
            RegionHeight = $ImageHeight
            OutputWidth = 1024
        }) | Out-Null
    }

    Add-RegionCase -Cases $cases -Name "top_left_512_to_512" -EncodedId $EncodedId -ImageWidth $ImageWidth -ImageHeight $ImageHeight -X 0 -Y 0 -RegionSize 512 -OutputWidth 512
    Add-RegionCase -Cases $cases -Name "top_left_512_to_128" -EncodedId $EncodedId -ImageWidth $ImageWidth -ImageHeight $ImageHeight -X 0 -Y 0 -RegionSize 512 -OutputWidth 128
    Add-RegionCase -Cases $cases -Name "top_left_4096_to_512" -EncodedId $EncodedId -ImageWidth $ImageWidth -ImageHeight $ImageHeight -X 0 -Y 0 -RegionSize 4096 -OutputWidth 512

    $centerX = [math]::Max(0, [int][math]::Floor(($ImageWidth - [math]::Min(4096, $ImageWidth)) / 2))
    $centerY = [math]::Max(0, [int][math]::Floor(($ImageHeight - [math]::Min(4096, $ImageHeight)) / 2))
    Add-RegionCase -Cases $cases -Name "center_4096_to_512" -EncodedId $EncodedId -ImageWidth $ImageWidth -ImageHeight $ImageHeight -X $centerX -Y $centerY -RegionSize 4096 -OutputWidth 512

    $edgeX = [math]::Max(0, $ImageWidth - [math]::Min(4096, $ImageWidth))
    $edgeY = [math]::Max(0, $ImageHeight - [math]::Min(4096, $ImageHeight))
    Add-RegionCase -Cases $cases -Name "edge_4096_to_512" -EncodedId $EncodedId -ImageWidth $ImageWidth -ImageHeight $ImageHeight -X $edgeX -Y $edgeY -RegionSize 4096 -OutputWidth 512

    return $cases.ToArray()
}

function Compare-Bitmap {
    param(
        [string]$CandidatePath,
        [string]$ReferencePath,
        [int]$BadPixelDelta,
        [int]$SampleStep
    )

    $candidate = $null
    $reference = $null
    try {
        $candidate = [System.Drawing.Bitmap]::new((Resolve-Path -LiteralPath $CandidatePath).ProviderPath)
        $reference = [System.Drawing.Bitmap]::new((Resolve-Path -LiteralPath $ReferencePath).ProviderPath)

        if ($candidate.Width -ne $reference.Width -or $candidate.Height -ne $reference.Height) {
            return [pscustomobject]@{
                DimensionsMatch = $false
                Width = $candidate.Width
                Height = $candidate.Height
                ReferenceWidth = $reference.Width
                ReferenceHeight = $reference.Height
                SampledPixels = 0
                MeanAbs = $null
                MaxDelta = $null
                BadPixelRatio = $null
            }
        }

        $step = [math]::Max(1, $SampleStep)
        $sampled = 0
        $sumAbs = 0.0
        $maxDelta = 0
        $badPixels = 0

        for ($y = 0; $y -lt $candidate.Height; $y += $step) {
            for ($x = 0; $x -lt $candidate.Width; $x += $step) {
                $a = $candidate.GetPixel($x, $y)
                $b = $reference.GetPixel($x, $y)
                $dr = [math]::Abs([int]$a.R - [int]$b.R)
                $dg = [math]::Abs([int]$a.G - [int]$b.G)
                $db = [math]::Abs([int]$a.B - [int]$b.B)
                $pixelMax = [math]::Max($dr, [math]::Max($dg, $db))
                $sumAbs += $dr + $dg + $db
                $maxDelta = [math]::Max($maxDelta, $pixelMax)
                if ($pixelMax -gt $BadPixelDelta) {
                    $badPixels++
                }
                $sampled++
            }
        }

        [pscustomobject]@{
            DimensionsMatch = $true
            Width = $candidate.Width
            Height = $candidate.Height
            ReferenceWidth = $reference.Width
            ReferenceHeight = $reference.Height
            SampledPixels = $sampled
            MeanAbs = [math]::Round($sumAbs / [math]::Max(1, $sampled * 3), 4)
            MaxDelta = $maxDelta
            BadPixelRatio = [math]::Round($badPixels / [double][math]::Max(1, $sampled), 6)
        }
    } finally {
        if ($null -ne $candidate) {
            $candidate.Dispose()
        }
        if ($null -ne $reference) {
            $reference.Dispose()
        }
    }
}

New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$runDir = Join-Path $OutDir $stamp
New-Item -ItemType Directory -Path $runDir -Force | Out-Null

$rows = New-Object System.Collections.Generic.List[object]

foreach ($imageId in $ImageIds) {
    $encodedId = Escape-ImageId $imageId
    $infoResponse = Invoke-WebRequest -UseBasicParsing -Uri "$ReferenceBaseUrl/iiif/3/$encodedId/info.json"
    $infoJson = [Text.Encoding]::UTF8.GetString($infoResponse.Content) | ConvertFrom-Json
    $imageWidth = [int]$infoJson.width
    $imageHeight = [int]$infoJson.height
    $cases = New-TestCases -EncodedId $encodedId -ImageWidth $imageWidth -ImageHeight $imageHeight

    foreach ($case in $cases) {
        $safeName = ($imageId -replace '[^A-Za-z0-9_.-]', '_') + "-" + $case.Name
        $candidatePath = Join-Path $runDir "$safeName-candidate.png"
        $referencePath = Join-Path $runDir "$safeName-reference.png"

        $candidate = Invoke-ImageRequest -BaseUrl $CandidateBaseUrl -Path $case.Path -OutFile $candidatePath
        $reference = Invoke-ImageRequest -BaseUrl $ReferenceBaseUrl -Path $case.Path -OutFile $referencePath

        $comparison = if ($candidate.Status -eq 200 -and $reference.Status -eq 200) {
            Compare-Bitmap -CandidatePath $candidatePath -ReferencePath $referencePath -BadPixelDelta $BadPixelDelta -SampleStep $SampleStep
        } else {
            [pscustomobject]@{
                DimensionsMatch = $false
                Width = $null
                Height = $null
                ReferenceWidth = $null
                ReferenceHeight = $null
                SampledPixels = 0
                MeanAbs = $null
                MaxDelta = $null
                BadPixelRatio = $null
            }
        }

        $passed = $candidate.Status -eq 200 `
            -and $reference.Status -eq 200 `
            -and $comparison.DimensionsMatch `
            -and $comparison.MeanAbs -le $MeanAbsThreshold `
            -and $comparison.BadPixelRatio -le $BadPixelRatioThreshold

        $rows.Add([pscustomobject]@{
            Passed = $passed
            Image = $imageId
            Case = $case.Name
            CandidateMs = $candidate.Ms
            ReferenceMs = $reference.Ms
            CandidateServerMs = $candidate.ServerTotalMs
            ReferenceServerMs = $reference.ServerTotalMs
            CandidateRenderMs = $candidate.ServerRenderMs
            ReferenceRenderMs = $reference.ServerRenderMs
            CandidateBackend = $candidate.Jp2Backend
            ReferenceBackend = $reference.Jp2Backend
            CandidateStatus = $candidate.Status
            ReferenceStatus = $reference.Status
            Width = $comparison.Width
            Height = $comparison.Height
            MeanAbs = $comparison.MeanAbs
            MaxDelta = $comparison.MaxDelta
            BadPixelRatio = $comparison.BadPixelRatio
            CandidateFile = $candidatePath
            ReferenceFile = $referencePath
            Path = $case.Path
            Error = (($candidate.Error, $reference.Error) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }) -join " | "
        }) | Out-Null
    }
}

$csvPath = Join-Path $OutDir "gigatiff-jp2-artifacts-$stamp.csv"
$jsonPath = Join-Path $OutDir "gigatiff-jp2-artifacts-$stamp.json"
$rows.ToArray() | Export-Csv -LiteralPath $csvPath -NoTypeInformation -Encoding UTF8
[pscustomobject]@{
    CandidateBaseUrl = $CandidateBaseUrl
    ReferenceBaseUrl = $ReferenceBaseUrl
    BadPixelDelta = $BadPixelDelta
    MeanAbsThreshold = $MeanAbsThreshold
    BadPixelRatioThreshold = $BadPixelRatioThreshold
    SampleStep = $SampleStep
    OutputDirectory = $runDir
    Results = $rows.ToArray()
} | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $jsonPath -Encoding UTF8

if ($Json) {
    [pscustomobject]@{
        Csv = $csvPath
        Json = $jsonPath
        OutputDirectory = $runDir
        Results = $rows.ToArray()
    } | ConvertTo-Json -Depth 5
} else {
    Write-Host "Saved JP2 artifact results:"
    Write-Host "  $csvPath"
    Write-Host "  $jsonPath"
    Write-Host "  $runDir"
    $rows | Select-Object Passed,Image,Case,CandidateMs,ReferenceMs,CandidateBackend,ReferenceBackend,MeanAbs,MaxDelta,BadPixelRatio,Width,Height | Format-Table -AutoSize
}

if (($rows | Where-Object { -not $_.Passed }).Count -gt 0) {
    exit 1
}
