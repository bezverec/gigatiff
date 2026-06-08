param(
    [string]$BaseUrl = "http://127.0.0.1:18082",
    [string[]]$ImageIds = @(
        "mapa2_no_xmp_clean_master.jp2",
        "mapa2_master.jp2",
        "mapa2_no_xmp_clean_user_1_8.jp2",
        "mapa2_user_1_8.jp2"
    ),
    [string]$Format = "webp",
    [int]$BatchCount = 8,
    [int]$StartupViewportWidth = 2048,
    [int]$StartupViewportHeight = 1024,
    [int]$ViewerPrewarmDelayMs = 1000,
    [string]$OutDir = "target/server-benchmarks-jp2",
    [switch]$SkipCachePurge,
    [switch]$Json
)

$ErrorActionPreference = "Stop"

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

function Invoke-TimedRequest {
    param(
        [string]$Url,
        [string]$Scenario,
        [string]$ImageId,
        [string]$Phase
    )

    $out = Join-Path ([IO.Path]::GetTempPath()) ("gigatiff-jp2-firstload-" + [Guid]::NewGuid().ToString("N"))
    try {
        $sw = [Diagnostics.Stopwatch]::StartNew()
        $response = Invoke-WebRequest -UseBasicParsing -SkipHttpErrorCheck -Uri $Url -OutFile $out -PassThru
        $sw.Stop()
        $bytes = if (Test-Path -LiteralPath $out) { (Get-Item -LiteralPath $out).Length } else { 0 }

        [pscustomobject]@{
            Image = $ImageId
            Scenario = $Scenario
            Phase = $Phase
            Url = $Url
            Status = [int]$response.StatusCode
            Ms = [math]::Round($sw.Elapsed.TotalMilliseconds, 2)
            Bytes = $bytes
            ContentType = Get-HeaderString $response.Headers "Content-Type"
            Cache = Get-HeaderString $response.Headers "x-gigatiff-cache"
            Jp2Backend = Get-HeaderString $response.Headers "x-gigatiff-jp2-backend"
            OpenJpegThreads = Get-HeaderString $response.Headers "x-gigatiff-openjpeg-threads"
            ViewerPrewarm = Get-HeaderString $response.Headers "x-gigatiff-viewer-prewarm"
            Jp2TileWidth = Get-HeaderString $response.Headers "x-gigatiff-jp2-tile-width"
            Jp2TileHeight = Get-HeaderString $response.Headers "x-gigatiff-jp2-tile-height"
            Jp2TilesSupported = Get-HeaderString $response.Headers "x-gigatiff-jp2-tiles-supported"
            ServerTotalMs = Get-HeaderDouble $response.Headers "x-gigatiff-total-ms"
            ServerCacheReadMs = Get-HeaderDouble $response.Headers "x-gigatiff-cache-read-ms"
            ServerRenderMs = Get-HeaderDouble $response.Headers "x-gigatiff-render-ms"
            ServerEncodeMs = Get-HeaderDouble $response.Headers "x-gigatiff-encode-ms"
            ServerCacheStoreMs = Get-HeaderDouble $response.Headers "x-gigatiff-cache-store-ms"
            ServerCachePruneMs = Get-HeaderDouble $response.Headers "x-gigatiff-cache-prune-ms"
            Error = if ([int]$response.StatusCode -ge 400) { (Get-Content -LiteralPath $out -Raw -ErrorAction SilentlyContinue) } else { $null }
        }
    } catch {
        $sw.Stop()
        [pscustomobject]@{
            Image = $ImageId
            Scenario = $Scenario
            Phase = $Phase
            Url = $Url
            Status = $null
            Ms = [math]::Round($sw.Elapsed.TotalMilliseconds, 2)
            Bytes = 0
            ContentType = $null
            Cache = $null
            Jp2Backend = $null
            OpenJpegThreads = $null
            ViewerPrewarm = $null
            Jp2TileWidth = $null
            Jp2TileHeight = $null
            Jp2TilesSupported = $null
            ServerTotalMs = $null
            ServerCacheReadMs = $null
            ServerRenderMs = $null
            ServerEncodeMs = $null
            ServerCacheStoreMs = $null
            ServerCachePruneMs = $null
            Error = $_.Exception.Message
        }
    } finally {
        if (Test-Path -LiteralPath $out) {
            Remove-Item -LiteralPath $out -Force
        }
    }
}

function Invoke-CachePurge {
    param(
        [string]$BaseUrl,
        [string]$EncodedId
    )

    if (-not $SkipCachePurge) {
        Invoke-RestMethod -Method Delete -Uri "$BaseUrl/api/cache/$EncodedId" | Out-Null
    }
}

function Add-ColdWarmScenario {
    param(
        [System.Collections.Generic.List[object]]$Rows,
        [string]$BaseUrl,
        [string]$ImageId,
        [string]$EncodedId,
        [string]$Scenario,
        [string]$Path
    )

    Invoke-CachePurge -BaseUrl $BaseUrl -EncodedId $EncodedId
    $url = "$BaseUrl/$Path"
    $Rows.Add((Invoke-TimedRequest -Url $url -Scenario $Scenario -ImageId $ImageId -Phase "cold")) | Out-Null
    $Rows.Add((Invoke-TimedRequest -Url $url -Scenario $Scenario -ImageId $ImageId -Phase "warm")) | Out-Null
}

function Get-Percentile {
    param(
        [object[]]$Rows,
        [double]$Percentile
    )

    if ($Rows.Count -eq 0) {
        return $null
    }
    $sorted = @($Rows | Sort-Object Ms)
    $index = [math]::Max(0, [math]::Min($sorted.Count - 1, [math]::Ceiling($sorted.Count * $Percentile) - 1))
    return [math]::Round($sorted[$index].Ms, 2)
}

function Measure-Batch {
    param(
        [string[]]$Urls,
        [string]$ImageId,
        [string]$Scenario
    )

    $started = [Diagnostics.Stopwatch]::StartNew()
    $rows = @(
        $Urls | ForEach-Object -Parallel {
            $url = $_
            $out = Join-Path ([IO.Path]::GetTempPath()) ("gigatiff-jp2-firstload-job-" + [Guid]::NewGuid().ToString("N"))
            try {
                $sw = [Diagnostics.Stopwatch]::StartNew()
                $response = Invoke-WebRequest -UseBasicParsing -SkipHttpErrorCheck -Uri $url -OutFile $out -PassThru
                $sw.Stop()
                $bytes = if (Test-Path -LiteralPath $out) { (Get-Item -LiteralPath $out).Length } else { 0 }
                $header = {
                    param($Headers, $Name)
                    $value = ($Headers[$Name] -join ",")
                    if ([string]::IsNullOrWhiteSpace($value)) {
                        return $null
                    }
                    return $value
                }
                $doubleHeader = {
                    param($Headers, $Name)
                    $value = & $header $Headers $Name
                    if ([string]::IsNullOrWhiteSpace($value)) {
                        return $null
                    }
                    return [double]::Parse($value, [Globalization.CultureInfo]::InvariantCulture)
                }
                [pscustomobject]@{
                    Ms = [math]::Round($sw.Elapsed.TotalMilliseconds, 2)
                    Bytes = $bytes
                    Status = [int]$response.StatusCode
                    Cache = & $header $response.Headers "x-gigatiff-cache"
                    Jp2Backend = & $header $response.Headers "x-gigatiff-jp2-backend"
                    OpenJpegThreads = & $header $response.Headers "x-gigatiff-openjpeg-threads"
                    ViewerPrewarm = & $header $response.Headers "x-gigatiff-viewer-prewarm"
                    ServerTotalMs = & $doubleHeader $response.Headers "x-gigatiff-total-ms"
                    ServerRenderMs = & $doubleHeader $response.Headers "x-gigatiff-render-ms"
                    ServerEncodeMs = & $doubleHeader $response.Headers "x-gigatiff-encode-ms"
                    ServerCacheReadMs = & $doubleHeader $response.Headers "x-gigatiff-cache-read-ms"
                    ServerCacheStoreMs = & $doubleHeader $response.Headers "x-gigatiff-cache-store-ms"
                    Error = if ([int]$response.StatusCode -ge 400) { (Get-Content -LiteralPath $out -Raw -ErrorAction SilentlyContinue) } else { $null }
                }
            } catch {
                $sw.Stop()
                [pscustomobject]@{
                    Ms = [math]::Round($sw.Elapsed.TotalMilliseconds, 2)
                    Bytes = 0
                    Status = $null
                    Cache = $null
                    Jp2Backend = $null
                    OpenJpegThreads = $null
                    ViewerPrewarm = $null
                    ServerTotalMs = $null
                    ServerRenderMs = $null
                    ServerEncodeMs = $null
                    ServerCacheReadMs = $null
                    ServerCacheStoreMs = $null
                    Error = $_.Exception.Message
                }
            } finally {
                if (Test-Path -LiteralPath $out) {
                    Remove-Item -LiteralPath $out -Force
                }
            }
        }
    )
    $started.Stop()

    $avg = ($rows | Measure-Object Ms -Average).Average
    $serverAvg = ($rows | Where-Object { $null -ne $_.ServerTotalMs } | Measure-Object ServerTotalMs -Average).Average
    $renderAvg = ($rows | Where-Object { $null -ne $_.ServerRenderMs } | Measure-Object ServerRenderMs -Average).Average
    $encodeAvg = ($rows | Where-Object { $null -ne $_.ServerEncodeMs } | Measure-Object ServerEncodeMs -Average).Average
    $bytesAvg = ($rows | Measure-Object Bytes -Average).Average

    [pscustomobject]@{
        Image = $ImageId
        Scenario = $Scenario
        Phase = "batch"
        Url = "$($Urls.Count) parallel requests"
        Status = (($rows | ForEach-Object Status | Sort-Object -Unique) -join ",")
        Ms = [math]::Round($started.Elapsed.TotalMilliseconds, 2)
        Bytes = if ($null -ne $bytesAvg) { [math]::Round($bytesAvg, 0) } else { $null }
        ContentType = $null
        Cache = (($rows | ForEach-Object Cache | Sort-Object -Unique) -join ",")
        Jp2Backend = (($rows | ForEach-Object Jp2Backend | Sort-Object -Unique) -join ",")
        OpenJpegThreads = (($rows | ForEach-Object OpenJpegThreads | Sort-Object -Unique) -join ",")
        ViewerPrewarm = (($rows | ForEach-Object ViewerPrewarm | Sort-Object -Unique) -join ",")
        Jp2TileWidth = $null
        Jp2TileHeight = $null
        Jp2TilesSupported = $null
        ServerTotalMs = if ($null -ne $serverAvg) { [math]::Round($serverAvg, 2) } else { $null }
        ServerCacheReadMs = $null
        ServerRenderMs = if ($null -ne $renderAvg) { [math]::Round($renderAvg, 2) } else { $null }
        ServerEncodeMs = if ($null -ne $encodeAvg) { [math]::Round($encodeAvg, 2) } else { $null }
        ServerCacheStoreMs = $null
        ServerCachePruneMs = $null
        BatchCount = $Urls.Count
        BatchAvgMs = if ($null -ne $avg) { [math]::Round($avg, 2) } else { $null }
        BatchP50Ms = Get-Percentile -Rows $rows -Percentile 0.50
        BatchP95Ms = Get-Percentile -Rows $rows -Percentile 0.95
        Error = (($rows | Where-Object { -not [string]::IsNullOrWhiteSpace($_.Error) } | Select-Object -ExpandProperty Error -Unique) -join " | ")
    }
}

$rows = New-Object System.Collections.Generic.List[object]
$metadata = New-Object System.Collections.Generic.List[object]

foreach ($imageId in $ImageIds) {
    $encodedId = Escape-ImageId $imageId
    $infoResponse = Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/iiif/3/$encodedId/info.json"
    $info = ([Text.Encoding]::UTF8.GetString($infoResponse.Content) | ConvertFrom-Json)
    $apiInfo = Invoke-RestMethod -Uri "$BaseUrl/api/info/$encodedId"
    $tile = @($info.tiles)[0]
    $scaleFactors = if ($null -ne $tile) { @($tile.scaleFactors) } else { @() }
    $firstScale = if ($scaleFactors.Count -gt 0) { [int]$scaleFactors[0] } else { 1 }
    $advertisedTileWidth = if ($null -ne $tile) { [int]$tile.width } else { 512 }
    $advertisedTileHeight = if ($null -ne $tile -and $null -ne $tile.height) { [int]$tile.height } else { $advertisedTileWidth }
    $advertisedRegionWidth = [math]::Min([int]$info.width, $advertisedTileWidth * $firstScale)
    $advertisedRegionHeight = [math]::Min([int]$info.height, $advertisedTileHeight * $firstScale)
    $advertisedOutputWidth = [math]::Max(1, [int][math]::Ceiling($advertisedRegionWidth / [double]$firstScale))
    $advertisedOutputHeight = [math]::Max(1, [int][math]::Ceiling($advertisedRegionHeight / [double]$firstScale))

    $metadata.Add([pscustomobject]@{
        Image = $imageId
        Width = [int]$info.width
        Height = [int]$info.height
        InfoTileWidth = $advertisedTileWidth
        InfoTileHeight = $advertisedTileHeight
        InfoScaleFactors = ($scaleFactors -join ",")
        Jp2TileWidth = Get-HeaderString $infoResponse.Headers "x-gigatiff-jp2-tile-width"
        Jp2TileHeight = Get-HeaderString $infoResponse.Headers "x-gigatiff-jp2-tile-height"
        Jp2TilesSupported = Get-HeaderString $infoResponse.Headers "x-gigatiff-jp2-tiles-supported"
        ApiSource = $apiInfo.source
    }) | Out-Null

    Add-ColdWarmScenario -Rows $rows -BaseUrl $BaseUrl -ImageId $imageId -EncodedId $encodedId -Scenario "info_json" -Path "iiif/3/$encodedId/info.json"
    Add-ColdWarmScenario -Rows $rows -BaseUrl $BaseUrl -ImageId $imageId -EncodedId $encodedId -Scenario "api_info" -Path "api/info/$encodedId"
    Add-ColdWarmScenario -Rows $rows -BaseUrl $BaseUrl -ImageId $imageId -EncodedId $encodedId -Scenario "full_512" -Path "iiif/3/$encodedId/full/512,/0/default.$Format"
    Add-ColdWarmScenario -Rows $rows -BaseUrl $BaseUrl -ImageId $imageId -EncodedId $encodedId -Scenario "full_1024" -Path "iiif/3/$encodedId/full/1024,/0/default.$Format"
    Add-ColdWarmScenario -Rows $rows -BaseUrl $BaseUrl -ImageId $imageId -EncodedId $encodedId -Scenario "tile_512_to_128" -Path "iiif/3/$encodedId/0,0,512,512/128,/0/default.$Format"
    Add-ColdWarmScenario -Rows $rows -BaseUrl $BaseUrl -ImageId $imageId -EncodedId $encodedId -Scenario "tile_4096_to_512" -Path "iiif/3/$encodedId/0,0,4096,4096/512,/0/default.$Format"
    Add-ColdWarmScenario -Rows $rows -BaseUrl $BaseUrl -ImageId $imageId -EncodedId $encodedId -Scenario "advertised_tile" -Path "iiif/3/$encodedId/0,0,$advertisedRegionWidth,$advertisedRegionHeight/$advertisedOutputWidth,$advertisedOutputHeight/0/default.$Format"

    Invoke-CachePurge -BaseUrl $BaseUrl -EncodedId $encodedId
    $urls = for ($i = 0; $i -lt $BatchCount; $i++) {
        $columns = [math]::Max(1, [int][math]::Ceiling([math]::Sqrt($BatchCount)))
        $x = ($i % $columns) * 512
        $y = [int][math]::Floor($i / $columns) * 512
        "$BaseUrl/iiif/3/$encodedId/$x,$y,512,512/128,/0/default.$Format"
    }
    $rows.Add((Measure-Batch -Urls $urls -ImageId $imageId -Scenario "startup_batch_512_to_128")) | Out-Null

    Invoke-CachePurge -BaseUrl $BaseUrl -EncodedId $encodedId
    $advertisedColumns = [math]::Max(1, [int][math]::Ceiling($StartupViewportWidth / [double]$advertisedOutputWidth))
    $advertisedRows = [math]::Max(1, [int][math]::Ceiling($StartupViewportHeight / [double]$advertisedOutputHeight))
    $urls = for ($row = 0; $row -lt $advertisedRows; $row++) {
        for ($column = 0; $column -lt $advertisedColumns; $column++) {
            $x = $column * $advertisedRegionWidth
            $y = $row * $advertisedRegionHeight
            if ($x -ge [int]$info.width -or $y -ge [int]$info.height) {
                continue
            }
            $regionWidth = [math]::Min($advertisedRegionWidth, [int]$info.width - $x)
            $regionHeight = [math]::Min($advertisedRegionHeight, [int]$info.height - $y)
            $outputWidth = [math]::Max(1, [int][math]::Ceiling($regionWidth / [double]$firstScale))
            $outputHeight = [math]::Max(1, [int][math]::Ceiling($regionHeight / [double]$firstScale))
            "$BaseUrl/iiif/3/$encodedId/$x,$y,$regionWidth,$regionHeight/$outputWidth,$outputHeight/0/default.$Format"
        }
    }
    $rows.Add((Measure-Batch -Urls @($urls | Where-Object { $_ }) -ImageId $imageId -Scenario "startup_viewport_advertised_tile")) | Out-Null

    Invoke-CachePurge -BaseUrl $BaseUrl -EncodedId $encodedId
    $viewerUrl = "$BaseUrl/viewer/$encodedId`?prewarm=1"
    $rows.Add((Invoke-TimedRequest -Url $viewerUrl -Scenario "viewer_html_prewarm" -ImageId $imageId -Phase "trigger")) | Out-Null
    if ($ViewerPrewarmDelayMs -gt 0) {
        Start-Sleep -Milliseconds $ViewerPrewarmDelayMs
    }
    $rows.Add((Measure-Batch -Urls @($urls | Where-Object { $_ }) -ImageId $imageId -Scenario "viewer_prewarmed_startup_viewport")) | Out-Null
}

if (-not [string]::IsNullOrWhiteSpace($OutDir)) {
    New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $jsonPath = Join-Path $OutDir "gigatiff-jp2-firstload-$stamp.json"
    $csvPath = Join-Path $OutDir "gigatiff-jp2-firstload-$stamp.csv"
    $metadataPath = Join-Path $OutDir "gigatiff-jp2-firstload-metadata-$stamp.csv"
    [pscustomobject]@{
        BaseUrl = $BaseUrl
        Format = $Format
        BatchCount = $BatchCount
        StartupViewportWidth = $StartupViewportWidth
        StartupViewportHeight = $StartupViewportHeight
        ViewerPrewarmDelayMs = $ViewerPrewarmDelayMs
        SkipCachePurge = [bool]$SkipCachePurge
        Metadata = $metadata.ToArray()
        Results = $rows.ToArray()
    } | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $jsonPath -Encoding UTF8
    $rows.ToArray() | Export-Csv -LiteralPath $csvPath -NoTypeInformation -Encoding UTF8
    $metadata.ToArray() | Export-Csv -LiteralPath $metadataPath -NoTypeInformation -Encoding UTF8
    if (-not $Json) {
        Write-Host "Saved JP2 first-load benchmark results:"
        Write-Host "  $jsonPath"
        Write-Host "  $csvPath"
        Write-Host "  $metadataPath"
    }
}

if ($Json) {
    [pscustomobject]@{
        Metadata = $metadata.ToArray()
        Results = $rows.ToArray()
    } | ConvertTo-Json -Depth 6
} else {
    $rows | Select-Object Image,Scenario,Phase,Status,Ms,ServerTotalMs,ServerRenderMs,ServerEncodeMs,Cache,Jp2Backend,OpenJpegThreads,ViewerPrewarm,Bytes,Error | Format-Table -AutoSize
}
