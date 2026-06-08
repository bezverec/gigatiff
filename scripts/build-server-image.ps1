param(
    [string]$Image = "ghcr.io/bezverec/gigatiff-server:0.2.0",
    [string[]]$Platform = @("linux/amd64"),
    [switch]$Push
)

$ErrorActionPreference = "Stop"

$args = @(
    "buildx", "build",
    "--file", "Dockerfile",
    "--tag", $Image,
    "--platform", ($Platform -join ","),
    "--sbom=true",
    "--provenance=true"
)

if ($Push) {
    $args += "--push"
} else {
    if ($Platform.Count -gt 1) {
        throw "Docker buildx --load supports one platform. Use -Push for multi-arch images."
    }
    $args += "--load"
}

$args += "."
docker @args
