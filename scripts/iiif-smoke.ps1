param(
    [string]$BaseUrl = "http://127.0.0.1:18082",
    [string]$ImageId = "mapa2.tif",
    [int]$RegionSize = 512,
    [int]$OutputSize = 256,
    [switch]$SkipCachePurge,
    [switch]$Json
)

$ErrorActionPreference = "Stop"

function Escape-ImageId {
    param([string]$Value)
    return [Uri]::EscapeDataString($Value).Replace("%2F", "/")
}

function Assert-True {
    param(
        [bool]$Condition,
        [string]$Message
    )

    if (-not $Condition) {
        throw $Message
    }
}

function Get-Header {
    param(
        [object]$Headers,
        [string]$Name
    )

    return ($Headers[$Name] -join ",")
}

function Get-JsonProperty {
    param(
        [object]$Object,
        [string]$Name
    )

    return $Object.PSObject.Properties[$Name].Value
}

function Get-ResponseText {
    param([object]$Content)

    if ($Content -is [byte[]]) {
        return [Text.Encoding]::UTF8.GetString($Content)
    }
    return [string]$Content
}

function Invoke-NoRedirect {
    param([string]$Uri)

    $handler = [System.Net.Http.HttpClientHandler]::new()
    $handler.AllowAutoRedirect = $false
    $client = [System.Net.Http.HttpClient]::new($handler)
    try {
        return $client.GetAsync($Uri).GetAwaiter().GetResult()
    } finally {
        $client.Dispose()
        $handler.Dispose()
    }
}

function Invoke-Head {
    param([string]$Uri)

    return Invoke-WebRequest -UseBasicParsing -Method Head -Uri $Uri
}

function Invoke-CorsPreflight {
    param([string]$Uri)

    return Invoke-WebRequest -UseBasicParsing -Method Options -Uri $Uri -Headers @{
        Origin = "http://example.test"
        "Access-Control-Request-Method" = "GET"
    }
}

function Add-Result {
    param(
        [string]$Name,
        [string]$Status,
        [string]$Detail
    )

    [pscustomobject]@{
        Name = $Name
        Status = $Status
        Detail = $Detail
    }
}

$encodedId = Escape-ImageId $ImageId
$RegionSize = [Math]::Max(1, $RegionSize)
$OutputSize = [Math]::Max(1, $OutputSize)
$regionHalf = [Math]::Max(1, [int][Math]::Floor($RegionSize / 2))
$outputHalf = [Math]::Max(1, [int][Math]::Floor($OutputSize / 2))
$results = New-Object System.Collections.Generic.List[object]

try {
    if (-not $SkipCachePurge) {
        Invoke-RestMethod -Method Delete -Uri "$BaseUrl/api/cache" | Out-Null
        $results.Add((Add-Result "cache purge" "pass" "server cache cleared")) | Out-Null
    }

    $infoResponse = Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/iiif/3/$encodedId/info.json"
    $info = Get-ResponseText $infoResponse.Content | ConvertFrom-Json
    $infoLink = Get-Header $infoResponse.Headers "Link"
    $infoContentType = Get-Header $infoResponse.Headers "Content-Type"

    $profile = Get-JsonProperty $info "profile"
    $qualities = Get-JsonProperty $info "qualities"
    $extraFeatures = Get-JsonProperty $info "extraFeatures"

    Assert-True ($profile -eq "level2") "info.json profile should be level2"
    Assert-True ($infoContentType -like "*application/ld+json*") "info.json should use JSON-LD media type"
    Assert-True ($infoLink -like "*level2.json*rel=`"profile`"*") "info.json should include profile Link header"
    foreach ($quality in @("default", "color", "gray", "bitonal")) {
        Assert-True ($qualities -contains $quality) "info.json should advertise quality $quality"
    }
    foreach ($feature in @("baseUriRedirect", "canonicalLinkHeader", "cors", "jsonldMediaType", "mirroring", "profileLinkHeader", "rotationBy90s", "sizeUpscaling")) {
        Assert-True ($extraFeatures -contains $feature) "info.json should advertise extra feature $feature"
    }
    $results.Add((Add-Result "info.json" "pass" "profile=$profile")) | Out-Null

    $redirect = Invoke-NoRedirect "$BaseUrl/iiif/3/$encodedId"
    Assert-True ([int]$redirect.StatusCode -eq 303) "base URI should return 303"
    Assert-True ($redirect.Headers.Location.ToString() -eq "/iiif/3/$encodedId/info.json") "base URI redirect should point to info.json"
    $results.Add((Add-Result "baseUriRedirect" "pass" "303 $($redirect.Headers.Location)")) | Out-Null

    $head = Invoke-Head "$BaseUrl/iiif/3/$encodedId/info.json"
    Assert-True ([int]$head.StatusCode -eq 200) "HEAD info.json should return 200"
    Assert-True ((Get-Header $head.Headers "Content-Type") -like "*application/ld+json*") "HEAD info.json should include JSON-LD content type"
    $results.Add((Add-Result "HEAD info.json" "pass" "status=$($head.StatusCode)")) | Out-Null

    $cors = Invoke-CorsPreflight "$BaseUrl/iiif/3/$encodedId/info.json"
    Assert-True ([int]$cors.StatusCode -in @(200, 204)) "CORS preflight should return 200 or 204"
    Assert-True ((Get-Header $cors.Headers "Access-Control-Allow-Origin") -eq "*") "CORS preflight should allow all origins"
    $results.Add((Add-Result "OPTIONS CORS" "pass" "status=$($cors.StatusCode)")) | Out-Null

    $cases = @(
        @{ Name = "full max jpg"; Path = "full/max/0/default.jpg"; Type = "image/jpeg" },
        @{ Name = "pixel wh rotate gray png"; Path = "0,0,$RegionSize,$regionHalf/$OutputSize,$outputHalf/90/gray.png"; Type = "image/png" },
        @{ Name = "square width mirror bitonal webp"; Path = "square/$OutputSize,/!180/bitonal.webp"; Type = "image/webp" },
        @{ Name = "pct caret rotate jpg"; Path = "pct:0,0,1,1/^pct:200/270/color.jpg"; Type = "image/jpeg" }
    )

    foreach ($case in $cases) {
        $url = "$BaseUrl/iiif/3/$encodedId/$($case.Path)"
        $out = Join-Path $env:TEMP ("gigatiff-iiif-smoke-" + [Guid]::NewGuid().ToString("N"))
        try {
            $response = Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $out -PassThru
            $bytes = (Get-Item -LiteralPath $out).Length
            $contentType = Get-Header $response.Headers "Content-Type"
            $link = Get-Header $response.Headers "Link"

            Assert-True ([int]$response.StatusCode -eq 200) "$($case.Name) should return 200"
            Assert-True ($bytes -gt 0) "$($case.Name) should return bytes"
            Assert-True ($contentType -like "$($case.Type)*") "$($case.Name) should return $($case.Type)"
            Assert-True ((Get-Header $response.Headers "Cache-Control") -like "*max-age=86400*") "$($case.Name) should include long cache-control"
            Assert-True ($link -like "*level2.json*rel=`"profile`"*") "$($case.Name) should include profile Link"
            Assert-True ($link -like "*rel=`"canonical`"*") "$($case.Name) should include canonical Link"
            $results.Add((Add-Result $case.Name "pass" "$bytes bytes")) | Out-Null
        } finally {
            if (Test-Path -LiteralPath $out) {
                Remove-Item -LiteralPath $out -Force
            }
        }
    }

    $canonicalA = "$BaseUrl/iiif/3/$encodedId/0,0,$RegionSize,$RegionSize/$OutputSize,/0/default.jpg"
    $canonicalB = "$BaseUrl/iiif/3/$encodedId/0,0,$RegionSize,$RegionSize/$OutputSize,$OutputSize/0/default.jpeg"
    $cacheA = Invoke-WebRequest -UseBasicParsing -Uri $canonicalA
    $cacheB = Invoke-WebRequest -UseBasicParsing -Uri $canonicalB
    $cacheStatusA = Get-Header $cacheA.Headers "x-gigatiff-cache"
    $cacheStatusB = Get-Header $cacheB.Headers "x-gigatiff-cache"
    $linkA = Get-Header $cacheA.Headers "Link"
    $linkB = Get-Header $cacheB.Headers "Link"

    Assert-True ($cacheStatusA -eq "miss") "first canonical cache request should miss after purge"
    Assert-True ($cacheStatusB -eq "hit") "equivalent canonical cache request should hit"
    Assert-True ($linkA -eq $linkB) "equivalent canonical requests should return the same Link header"
    $results.Add((Add-Result "canonical cache key" "pass" "$cacheStatusA then $cacheStatusB")) | Out-Null

    $badCases = @(
        @{ Name = "reject arbitrary rotation"; Path = "0,0,$RegionSize,$regionHalf/$OutputSize,/45/default.jpg"; Expected = 400 },
        @{ Name = "reject upscaling without caret"; Path = "0,0,$RegionSize,$RegionSize/$($RegionSize * 2),/0/default.jpg"; Expected = 400 }
    )

    foreach ($case in $badCases) {
        $response = Invoke-WebRequest -UseBasicParsing -SkipHttpErrorCheck -Uri "$BaseUrl/iiif/3/$encodedId/$($case.Path)"
        Assert-True ([int]$response.StatusCode -eq $case.Expected) "$($case.Name) should return $($case.Expected)"
        $results.Add((Add-Result $case.Name "pass" "status=$($response.StatusCode)")) | Out-Null
    }
} catch {
    $results.Add((Add-Result "failure" "fail" $_.Exception.Message)) | Out-Null
    if ($Json) {
        $results | ConvertTo-Json -Depth 4
    } else {
        $results | Format-Table -AutoSize
    }
    exit 1
}

if ($Json) {
    $results | ConvertTo-Json -Depth 4
} else {
    $results | Format-Table -AutoSize
}
