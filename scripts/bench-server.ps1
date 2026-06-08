param(
    [string]$BaseUrl = "http://127.0.0.1:18082",
    [string[]]$ImageIds = @("mapa2_no_xmp_clean.tif", "mapa2.tif"),
    [string[]]$Formats = @("webp", "jpg", "png"),
    [int]$Iterations = 5,
    [int]$Parallel = 8,
    [int]$RegionSize = 512,
    [int[]]$RegionSizes = @(),
    [int]$OutputSize = 128,
    [int[]]$OutputSizes = @(),
    [string]$CacheDir = "cache",
    [switch]$ClearCache,
    [switch]$PurgeServerCache,
    [string]$OutDir = "",
    [switch]$Json
)

$ErrorActionPreference = "Stop"

if ($ClearCache -and (Test-Path $CacheDir)) {
    Get-ChildItem -LiteralPath $CacheDir -Recurse -File | Remove-Item -Force
}

if ($PurgeServerCache) {
    Invoke-RestMethod -Method Delete -Uri "$BaseUrl/api/cache" | Out-Null
}

function Escape-ImageId {
    param([string]$ImageId)
    return [Uri]::EscapeDataString($ImageId).Replace("%2F", "/")
}

function New-TileUrl {
    param(
        [string]$ImageId,
        [string]$Format,
        [int]$RegionSizeValue,
        [int]$OutputSizeValue,
        [int]$Offset
    )

    $encodedId = Escape-ImageId $ImageId
    $x = $Offset
    $y = $Offset
    return "$BaseUrl/iiif/3/$encodedId/$x,$y,$RegionSizeValue,$RegionSizeValue/$OutputSizeValue,/0/default.$Format"
}

function Get-HeaderDouble {
    param(
        [object]$Headers,
        [string]$Name
    )

    $value = ($Headers[$Name] -join ",")
    if ([string]::IsNullOrWhiteSpace($value)) {
        return $null
    }
    return [double]::Parse($value, [Globalization.CultureInfo]::InvariantCulture)
}

function Invoke-TimedRequest {
    param([string]$Url)

    $out = Join-Path $env:TEMP ("gigatiff-bench-" + [Guid]::NewGuid().ToString("N"))
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $response = Invoke-WebRequest -Uri $Url -OutFile $out -PassThru
    $sw.Stop()
    $bytes = (Get-Item -LiteralPath $out).Length
    Remove-Item -LiteralPath $out -Force

    [pscustomobject]@{
        Ms = [math]::Round($sw.Elapsed.TotalMilliseconds, 2)
        Bytes = $bytes
        Cache = ($response.Headers["x-gigatiff-cache"] -join ",")
        ServerTotalMs = Get-HeaderDouble $response.Headers "x-gigatiff-total-ms"
        ServerCacheReadMs = Get-HeaderDouble $response.Headers "x-gigatiff-cache-read-ms"
        ServerRenderMs = Get-HeaderDouble $response.Headers "x-gigatiff-render-ms"
        ServerEncodeMs = Get-HeaderDouble $response.Headers "x-gigatiff-encode-ms"
        ServerCacheStoreMs = Get-HeaderDouble $response.Headers "x-gigatiff-cache-store-ms"
        ServerCachePruneMs = Get-HeaderDouble $response.Headers "x-gigatiff-cache-prune-ms"
        Status = [int]$response.StatusCode
    }
}

function Measure-Stats {
    param([object[]]$Rows)

    $sorted = @($Rows | Sort-Object Ms)
    $count = $sorted.Count
    $avg = ($sorted | Measure-Object Ms -Average).Average
    $serverAvg = ($sorted | Where-Object { $null -ne $_.ServerTotalMs } | Measure-Object ServerTotalMs -Average).Average
    $renderAvg = ($sorted | Where-Object { $null -ne $_.ServerRenderMs } | Measure-Object ServerRenderMs -Average).Average
    $encodeAvg = ($sorted | Where-Object { $null -ne $_.ServerEncodeMs } | Measure-Object ServerEncodeMs -Average).Average
    $cacheReadAvg = ($sorted | Where-Object { $null -ne $_.ServerCacheReadMs } | Measure-Object ServerCacheReadMs -Average).Average
    $cacheStoreAvg = ($sorted | Where-Object { $null -ne $_.ServerCacheStoreMs } | Measure-Object ServerCacheStoreMs -Average).Average
    $cachePruneAvg = ($sorted | Where-Object { $null -ne $_.ServerCachePruneMs } | Measure-Object ServerCachePruneMs -Average).Average
    $p50Index = [math]::Max(0, [math]::Min($count - 1, [math]::Ceiling($count * 0.50) - 1))
    $p95Index = [math]::Max(0, [math]::Min($count - 1, [math]::Ceiling($count * 0.95) - 1))
    $p50 = $sorted[$p50Index].Ms
    $p95 = $sorted[$p95Index].Ms
    $bytes = ($sorted | Measure-Object Bytes -Average).Average

    [pscustomobject]@{
        Count = $count
        AvgMs = [math]::Round($avg, 2)
        ServerAvgMs = if ($null -ne $serverAvg) { [math]::Round($serverAvg, 2) } else { $null }
        ServerRenderAvgMs = if ($null -ne $renderAvg) { [math]::Round($renderAvg, 2) } else { $null }
        ServerEncodeAvgMs = if ($null -ne $encodeAvg) { [math]::Round($encodeAvg, 2) } else { $null }
        ServerCacheReadAvgMs = if ($null -ne $cacheReadAvg) { [math]::Round($cacheReadAvg, 2) } else { $null }
        ServerCacheStoreAvgMs = if ($null -ne $cacheStoreAvg) { [math]::Round($cacheStoreAvg, 2) } else { $null }
        ServerCachePruneAvgMs = if ($null -ne $cachePruneAvg) { [math]::Round($cachePruneAvg, 2) } else { $null }
        P50Ms = [math]::Round($p50, 2)
        P95Ms = [math]::Round($p95, 2)
        AvgBytes = [math]::Round($bytes, 0)
    }
}

$results = New-Object System.Collections.Generic.List[object]
$regionSizesToRun = if ($RegionSizes.Count -gt 0) { $RegionSizes } else { @($RegionSize) }
$outputSizesToRun = if ($OutputSizes.Count -gt 0) { $OutputSizes } else { @($OutputSize) }

foreach ($imageId in $ImageIds) {
    foreach ($format in $Formats) {
        foreach ($regionSizeValue in $regionSizesToRun) {
            foreach ($outputSizeValue in $outputSizesToRun) {
                $url = New-TileUrl -ImageId $imageId -Format $format -RegionSizeValue $regionSizeValue -OutputSizeValue $outputSizeValue -Offset 0
                $first = Invoke-TimedRequest $url
                $second = Invoke-TimedRequest $url

                $warmRows = @()
                for ($i = 0; $i -lt $Iterations; $i++) {
                    $warmRows += Invoke-TimedRequest $url
                }
                $warmStats = Measure-Stats $warmRows

                $parallelUrls = for ($i = 0; $i -lt $Parallel; $i++) {
                    New-TileUrl -ImageId $imageId -Format $format -RegionSizeValue $regionSizeValue -OutputSizeValue $outputSizeValue -Offset (($i + 1) * 64)
                }
                $parallelSw = [Diagnostics.Stopwatch]::StartNew()
                $parallelRows = @(
                    $parallelUrls | ForEach-Object -Parallel {
                        $Url = $_
                        $out = Join-Path $env:TEMP ("gigatiff-bench-job-" + [Guid]::NewGuid().ToString("N"))
                        $sw = [Diagnostics.Stopwatch]::StartNew()
                        $response = Invoke-WebRequest -Uri $Url -OutFile $out -PassThru
                        $sw.Stop()
                        $bytes = (Get-Item -LiteralPath $out).Length
                        Remove-Item -LiteralPath $out -Force
                        [pscustomobject]@{
                            Ms = [math]::Round($sw.Elapsed.TotalMilliseconds, 2)
                            Bytes = $bytes
                            Cache = ($response.Headers["x-gigatiff-cache"] -join ",")
                            ServerCacheReadMs = if ($response.Headers["x-gigatiff-cache-read-ms"]) {
                                [double]::Parse(($response.Headers["x-gigatiff-cache-read-ms"] -join ","), [Globalization.CultureInfo]::InvariantCulture)
                            } else {
                                $null
                            }
                            ServerTotalMs = if ($response.Headers["x-gigatiff-total-ms"]) {
                                [double]::Parse(($response.Headers["x-gigatiff-total-ms"] -join ","), [Globalization.CultureInfo]::InvariantCulture)
                            } else {
                                $null
                            }
                            ServerRenderMs = if ($response.Headers["x-gigatiff-render-ms"]) {
                                [double]::Parse(($response.Headers["x-gigatiff-render-ms"] -join ","), [Globalization.CultureInfo]::InvariantCulture)
                            } else {
                                $null
                            }
                            ServerEncodeMs = if ($response.Headers["x-gigatiff-encode-ms"]) {
                                [double]::Parse(($response.Headers["x-gigatiff-encode-ms"] -join ","), [Globalization.CultureInfo]::InvariantCulture)
                            } else {
                                $null
                            }
                            ServerCacheStoreMs = if ($response.Headers["x-gigatiff-cache-store-ms"]) {
                                [double]::Parse(($response.Headers["x-gigatiff-cache-store-ms"] -join ","), [Globalization.CultureInfo]::InvariantCulture)
                            } else {
                                $null
                            }
                            ServerCachePruneMs = if ($response.Headers["x-gigatiff-cache-prune-ms"]) {
                                [double]::Parse(($response.Headers["x-gigatiff-cache-prune-ms"] -join ","), [Globalization.CultureInfo]::InvariantCulture)
                            } else {
                                $null
                            }
                            Status = [int]$response.StatusCode
                        }
                    }
                )
                $parallelSw.Stop()
                $parallelStats = Measure-Stats $parallelRows

                $results.Add([pscustomobject]@{
                    Image = $imageId
                    Format = $format
                    RegionSize = $regionSizeValue
                    OutputSize = $outputSizeValue
                    ColdMs = $first.Ms
                    ColdCache = $first.Cache
                    ColdServerMs = $first.ServerTotalMs
                    ColdRenderMs = $first.ServerRenderMs
                    ColdEncodeMs = $first.ServerEncodeMs
                    ColdCacheStoreMs = $first.ServerCacheStoreMs
                    ColdCachePruneMs = $first.ServerCachePruneMs
                    WarmProbeMs = $second.Ms
                    WarmProbeCache = $second.Cache
                    WarmProbeServerMs = $second.ServerTotalMs
                    WarmProbeCacheReadMs = $second.ServerCacheReadMs
                    WarmAvgMs = $warmStats.AvgMs
                    WarmServerAvgMs = $warmStats.ServerAvgMs
                    WarmRenderAvgMs = $warmStats.ServerRenderAvgMs
                    WarmEncodeAvgMs = $warmStats.ServerEncodeAvgMs
                    WarmCacheReadAvgMs = $warmStats.ServerCacheReadAvgMs
                    WarmP50Ms = $warmStats.P50Ms
                    WarmP95Ms = $warmStats.P95Ms
                    AvgBytes = $warmStats.AvgBytes
                    Parallel = $Parallel
                    ParallelWallMs = [math]::Round($parallelSw.Elapsed.TotalMilliseconds, 2)
                    ParallelAvgMs = $parallelStats.AvgMs
                    ParallelServerAvgMs = $parallelStats.ServerAvgMs
                    ParallelRenderAvgMs = $parallelStats.ServerRenderAvgMs
                    ParallelEncodeAvgMs = $parallelStats.ServerEncodeAvgMs
                    ParallelCacheStoreAvgMs = $parallelStats.ServerCacheStoreAvgMs
                    ParallelCachePruneAvgMs = $parallelStats.ServerCachePruneAvgMs
                    ParallelP95Ms = $parallelStats.P95Ms
                    ParallelCache = (($parallelRows | ForEach-Object Cache | Sort-Object -Unique) -join ",")
                }) | Out-Null
            }
        }
    }
}

if (-not [string]::IsNullOrWhiteSpace($OutDir)) {
    New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $jsonPath = Join-Path $OutDir "gigatiff-server-bench-$stamp.json"
    $csvPath = Join-Path $OutDir "gigatiff-server-bench-$stamp.csv"
    ConvertTo-Json -InputObject $results.ToArray() -Depth 4 | Set-Content -LiteralPath $jsonPath -Encoding UTF8
    $results.ToArray() | Export-Csv -LiteralPath $csvPath -NoTypeInformation -Encoding UTF8
    if (-not $Json) {
        Write-Host "Saved benchmark results:"
        Write-Host "  $jsonPath"
        Write-Host "  $csvPath"
    }
}

if ($Json) {
    ConvertTo-Json -InputObject $results.ToArray() -Depth 4
} else {
    $results | Format-Table -AutoSize
}
