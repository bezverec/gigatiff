param(
    [Parameter(Mandatory = $true)]
    [string]$OutPath,
    [int]$Width = 64,
    [int]$Height = 48
)

$ErrorActionPreference = "Stop"

if ($Width -le 0 -or $Height -le 0) {
    throw "Width and Height must be greater than zero."
}

$parent = Split-Path -Parent $OutPath
if (-not [string]::IsNullOrWhiteSpace($parent)) {
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
}

$entryCount = 10
$ifdOffset = 8
$ifdSize = 2 + ($entryCount * 12) + 4
$bitsOffset = $ifdOffset + $ifdSize
$bitsBytes = 6
$pixelOffset = $bitsOffset + $bitsBytes
$pixelBytes = $Width * $Height * 3

function Write-IfdEntry {
    param(
        [System.IO.BinaryWriter]$Writer,
        [UInt16]$Tag,
        [UInt16]$Type,
        [UInt32]$Count,
        [UInt32]$Value
    )

    $Writer.Write($Tag)
    $Writer.Write($Type)
    $Writer.Write($Count)
    $Writer.Write($Value)
}

$stream = [System.IO.File]::Open($OutPath, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write)
try {
    $writer = [System.IO.BinaryWriter]::new($stream)
    try {
        $writer.Write([byte[]](0x49, 0x49))
        $writer.Write([UInt16]42)
        $writer.Write([UInt32]$ifdOffset)

        $writer.Write([UInt16]$entryCount)
        Write-IfdEntry $writer 256 4 1 ([UInt32]$Width)
        Write-IfdEntry $writer 257 4 1 ([UInt32]$Height)
        Write-IfdEntry $writer 258 3 3 ([UInt32]$bitsOffset)
        Write-IfdEntry $writer 259 3 1 1
        Write-IfdEntry $writer 262 3 1 2
        Write-IfdEntry $writer 273 4 1 ([UInt32]$pixelOffset)
        Write-IfdEntry $writer 277 3 1 3
        Write-IfdEntry $writer 278 4 1 ([UInt32]$Height)
        Write-IfdEntry $writer 279 4 1 ([UInt32]$pixelBytes)
        Write-IfdEntry $writer 284 3 1 1
        $writer.Write([UInt32]0)

        $writer.Write([UInt16]8)
        $writer.Write([UInt16]8)
        $writer.Write([UInt16]8)

        $xDenominator = [Math]::Max(1, $Width - 1)
        $yDenominator = [Math]::Max(1, $Height - 1)
        $sumDenominator = [Math]::Max(1, $Width + $Height - 2)
        for ($y = 0; $y -lt $Height; $y++) {
            for ($x = 0; $x -lt $Width; $x++) {
                $r = [byte][Math]::Floor($x * 255 / $xDenominator)
                $g = [byte][Math]::Floor($y * 255 / $yDenominator)
                $b = [byte][Math]::Floor(($x + $y) * 255 / $sumDenominator)
                $writer.Write($r)
                $writer.Write($g)
                $writer.Write($b)
            }
        }
    } finally {
        $writer.Dispose()
    }
} finally {
    $stream.Dispose()
}

Write-Host "Wrote $OutPath ($Width x $Height, $pixelBytes RGB bytes)."
