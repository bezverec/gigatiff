# GigaTIFF

A memory-conscious TIFF and BigTIFF viewer prototype written in Rust.

The project has two application surfaces:

- a cross-platform desktop viewer, exposed as `gigatiff`, with GUI, metadata, and PNG preview/export commands,
- a Linux/container-targeted IIIF-compatible image server, exposed as `gigatiff-server`, for browser-based viewing.

The default desktop/TIFF pixel backend is `auto`: it prefers direct raw-strip reads for suitable uncompressed
stripped TIFFs and falls back to `libtiff` scanlines for broader TIFF support. A pure Rust TIFF path
is still available as a CLI fallback for supported files.

## Source Layout

The repository is a Cargo workspace with three first-party crates:

- `crates/gigatiff-core` contains shared TIFF/BigTIFF metadata loading, rendering, color conversion,
  PNG export, scanline cache primitives, and the optional Grok JPEG2000 FFI bridge.
- `crates/gigatiff-desktop` builds the cross-platform `gigatiff` desktop/CLI binary with the
  egui/eframe viewer, native file dialogs, recent files, GUI tile scheduling, and PNG viewport export.
- `crates/gigatiff-server` builds the Linux/container-targeted `gigatiff-server` IIIF/OpenSeadragon
  image server.

The vendored `vendor/grokj2k-sys` package is intentionally excluded from default workspace checks. It
is only built when `gigatiff-server` enables the `jpeg2000-grok-ffi` feature.

## Build

Debug workspace build:

```powershell
cargo build
```

Optimized desktop release build:

```powershell
cargo build --release -p gigatiff-desktop --bin gigatiff
```

CPU-specific release build for the current machine:

```powershell
$env:RUSTFLAGS="-C target-cpu=native"; cargo build --release -p gigatiff-desktop --bin gigatiff
```

Executables:

```text
target\debug\gigatiff.exe
target\release\gigatiff.exe
```

The root `cargo build` command builds the workspace packages. Use `-p gigatiff-desktop` or
`-p gigatiff-server` when you want to build one application surface explicitly.

The build script copies `tiff.dll` next to the executable in `target/debug` or `target/release`.

## Release Packaging

GitHub release builds are produced by `.github/workflows/release.yml`. The workflow can be run
manually to test packaging, and it also runs automatically for tags matching `v*`.

The first release line is intentionally published as a prerelease. It produces three archives from
the same tag:

- `gigatiff-<version>-windows-x64.zip` with `gigatiff.exe` and vcpkg DLLs,
- `gigatiff-<version>-linux-x64.zip` with the Linux desktop binary, a `GigaTIFF.desktop` launcher, README, and license,
- `gigatiff-<version>-macos.zip` with the macOS desktop binary, a `GigaTIFF.app` bundle, README, and license.

The Windows archive is closest to download-and-run. On macOS, launch `GigaTIFF.app` to start the GUI
without opening Terminal. On Linux, use the included `.desktop` launcher or install it into the
desktop environment so it runs with `Terminal=false`. Linux and macOS archives may still require
system libraries such as libtiff, lcms2, and GUI runtime dependencies installed through the platform
package manager.

## Desktop Application

The desktop side is the primary standalone viewer. It includes the GUI, the `info` metadata command,
and the `preview` PNG export command.

### Running the GUI

The release executable can be launched directly:

```powershell
target\release\gigatiff.exe
```

If a TIFF path is passed as the first argument, the file opens directly in the GUI:

```powershell
target\release\gigatiff.exe mapa2.tif
```

The older `gui` subcommand is still available:

```powershell
target\debug\gigatiff.exe gui
```

The GUI contains a classic `File` menu:

- `File > Browse...` opens a TIFF/BigTIFF through the native file dialog,
- `File > Export as PNG...` re-renders the current viewport at a higher output size and saves it as PNG,
- `File > Quit` closes the application.

The top toolbar also includes a path field, `Browse`, aspect-preserving `Fit`, `1:1` actual-size
zoom, zoom out, and zoom in controls. Additional viewer controls include a zoom percentage readout,
recent files under the `File` menu, and a bottom status bar with tile loading/cache details.

### CLI Commands

Print metadata without decoding the full image:

```powershell
target\debug\gigatiff.exe info mapa2.tif
```

Export a viewport preview through the default `auto` backend:

```powershell
target\debug\gigatiff.exe preview mapa2.tif --x 0 --y 0 --width 2048 --height 2048 --max-output 512 --out preview.png
```

Preview export uses fast PNG compression by default. The compression level can be changed when file
size matters more than export speed:

```powershell
target\debug\gigatiff.exe preview mapa2.tif --png-compression high --out preview_high.png
```

Use the pure Rust fallback backend:

```powershell
target\debug\gigatiff.exe preview mapa2_no_xmp_clean.tif --backend rust --x 0 --y 0 --width 2048 --height 2048 --max-output 512 --out preview_rust.png
```

Force the `libtiff` scanline backend:

```powershell
target\debug\gigatiff.exe preview mapa2.tif --backend libtiff --x 0 --y 0 --width 2048 --height 2048 --max-output 512 --out preview_libtiff.png
```

## Server Application

The server side is intentionally separate from the desktop viewer. It targets Linux deployment,
primarily through Docker or Podman, and is not part of the cross-platform desktop release archives. It
reuses the same TIFF rendering pipeline for TIFF/BigTIFF sources and can optionally use Grok command
line tools for JPEG2000 sources. Both paths are exposed through HTTP, IIIF-style image URLs, and a
browser viewer.

### Running the Image Server

`gigatiff-server` is a separate binary so the desktop viewer remains standalone. It exposes image
files under a root directory through a small IIIF Image API 3.0-compatible surface and includes a
minimal OpenSeadragon viewer.

```bash
cargo build --release -p gigatiff-server --bin gigatiff-server
target/release/gigatiff-server --root /path/to/tiffs --addr 127.0.0.1:8080
```

Supported source files in the base server package are `.tif` and `.tiff`. JPEG2000 support is
available through the optional `jpeg2000-grok` feature, which shells out to Grok tools:

```bash
cargo build --release -p gigatiff-server --bin gigatiff-server --features jpeg2000-grok
target/release/gigatiff-server --root /path/to/images --addr 127.0.0.1:8080
```

With `jpeg2000-grok`, `.jp2`, `.j2k`, `.j2c`, and `.jpc` files are listed and served. Metadata is
read with `grk_dump`; region rendering uses `grk_decompress` with the requested IIIF region and an
appropriate decode reduction. The command paths can be overridden with `GIGATIFF_GROK_DUMP` and
`GIGATIFF_GROK_DECOMPRESS`. Native non-container Linux deployments need `grokj2k-tools` installed.
Grok is AGPL-licensed, so this backend is kept server-only and optional in Cargo builds.

The Docker image enables `jpeg2000-grok-ffi`, which includes both the direct Grok FFI backend and
the direct OpenJPEG FFI backend. The production default is the hybrid `auto` mode: OpenJPEG is used
as the fast primary decoder and Grok remains available as a fallback or as an explicitly selected
backend. Docker builds pin Grok to upstream release `v20.3.10` (commit
`3c4b4d7037e5b23dec0b73ef326fc60de0bd6e6b`), which includes the fix for lossy RPCL JP2
region/reduce artefacts seen with the earlier 20.3.x release builds plus upstream decompressor
stability fixes from the 20.3.x releases. Both FFI paths avoid spawning external codec
processes or writing temporary PNM files.

The server stores encoded IIIF region/tile responses in a persistent cache. By default this is the
local disk backend under `cache/server`; it can be changed with `--cache-dir`. The alternative
`--cache-backend dragonfly` uses a Redis-compatible Dragonfly server configured through
`--dragonfly-url` and `--cache-namespace`. Cached responses are keyed by source path, file size,
modification time, image dimensions, backend, encoder settings, and the canonical IIIF image URI, so
equivalent request spellings reuse the same cache entry. The disk backend is pruned after writes to
stay under `--cache-max-mb` (default `4096`). Set `--cache-max-mb 0` to disable the disk response
cache. For Dragonfly, size eviction is managed by Dragonfly itself, typically through `maxmemory` and
cache mode. `--cache-ttl-sec` adds optional time-based expiry for both backends; `0` disables TTL
expiry. Cache entries include a source namespace prefix, so individual images can be purged without
deleting the whole response cache. Image responses are also browser-cacheable for 24 hours, so after
changing decoder behavior or purging the server-side cache, use a hard browser reload if old
OpenSeadragon tiles remain visible.

### Operational Limits and Security

The server has built-in guard rails intended for production-style Kramerius deployments:

- `--max-output-pixels` caps encoded response area and also bounds `max`/`full` downsizing.
- `--max-upscale` caps explicit IIIF `^` upscaling. Non-`^` upscale requests are rejected.
- `--max-concurrent-renders` limits global blocking render jobs.
- `--max-concurrent-renders-per-ip` limits concurrent renders from one client key.
- `--max-concurrent-renders-per-file` limits concurrent renders for one source image.
- `--openjpeg-threads` sets the maximum number of worker threads inside each OpenJPEG FFI decode.
  The CLI default is `1`; Docker/ops examples use `4` because it improved the tested large-tile JP2
  first viewport while leaving the setting easy to tune per host.
- `--render-timeout-sec` caps the HTTP response wait for decode/render/encode work.
- `--rate-limit-per-minute` applies a fixed-window per-client request rate limit; `0` disables it.
- `--enforce-read-only-root` rejects startup when the persistent cache directory is inside the image
  root.

Client identity uses `X-Forwarded-For` first, then `X-Real-IP`, then a shared local key. In a real
deployment, put the server behind a trusted reverse proxy and only pass those headers from the proxy.
The image root resolver rejects traversal, overlong identifiers, control characters, unsupported
extensions, and canonical paths that escape the configured root, including symlink escapes. IIIF
region, size, rotation, quality, and format tokens also have length and control-character checks so
malformed URLs fail before expensive metadata or render work starts.

`--enforce-read-only-root` prevents GigaTIFF from using a cache under the served image tree, but the
strongest protection is still to mount `/data` read-only in Docker/Podman and write cache files to a
separate volume. If the Grok CLI fallback is used in production, sandbox it at the container/runtime
level with a read-only root filesystem, no-new-privileges, seccomp/AppArmor, memory/CPU limits, and a
restricted writable temp/cache volume. The Grok FFI/OpenJPEG FFI server path avoids spawning external
decoder commands for normal image responses.

Useful endpoints:

```text
GET /healthz
GET /readyz
GET /api/images
GET /api/cache
DELETE /api/cache
DELETE /api/cache/<image-id>
POST /api/cache/warm/<image-id>
GET /api/info/<image-id>
GET /metrics
GET /viewer/<image-id>
GET /viewer/<image-id>?prewarm=1
GET /iiif/3/<image-id>
GET /iiif/3/<image-id>/info.json
GET /iiif/3/<image-id>/<region>/<size>/<rotation>/<quality>.<format>
```

`GET /healthz` is a cheap liveness probe for container runtimes. `GET /readyz` checks the image
root and the configured response-cache backend; with `--cache-backend dragonfly` it also sends a
Dragonfly/Redis `PING`. Probe and metrics routes bypass the built-in HTTP rate limiter so Kubernetes,
systemd, Caddy, or Prometheus checks do not consume client request quota.

The root page `/` is a small local dashboard with image links, cache size, last prune/purge summary,
and a manual cache purge action.

`GET /api/info/<image-id>` is a GigaTIFF metadata extension, separate from IIIF `info.json`. It
returns technical source metadata such as dimensions, TIFF compression/layout tags, resolution/DPI,
ICC presence, JPEG2000 component precision, tile size, progression order, resolution levels, backend
region-tile support, and a lightweight metadata-based profile validation summary. It is intended for
diagnostics and library-system integration; full archival validation should still use dedicated tools
such as valid2000, JHOVE, or jpylyzer.

`DELETE /api/cache/<image-id>` purges cached responses for one source image. `POST
/api/cache/warm/<image-id>` pre-renders a small default warm set: a full-image WebP thumbnail and the
first advertised WebP tile/region. This is intentionally conservative; future production deployments
can extend the same API toward queue-based prewarming and Dragonfly/Redis-backed shared cache storage.

`GET /viewer/<image-id>?prewarm=1` is an opt-in browser-viewer warm path. It returns the normal
OpenSeadragon HTML, schedules a background cache warm for the first 2048 x 1024 viewport using the
advertised IIIF tile geometry, and marks the response with `x-gigatiff-viewer-prewarm:
scheduled|skipped-recent|disabled|invalid-id`. The default `/viewer/<image-id>` route leaves this
disabled because immediate prewarming with no lead time can compete with the first visible tile
requests. It is most useful when triggered before navigation, for example from an image-list hover,
an "open warmed viewer" action, or a queue that warms likely next images.

Identical encoded response cache misses are coalesced in-process. When two requests race for the
same cache key, one render stores the bytes while the other waits and rechecks the cache. This avoids
duplicated expensive JP2 fallback renders during prewarm/viewer overlap and bursty OpenSeadragon
startup.

`GET /metrics` exposes Prometheus text-format metrics. The server records HTTP request/response
counters, rate-limited request counts, cache hits/misses/disabled responses and hit ratio, cache size
pressure, render job counts, render timeout counts, render queue wait time, active render tasks,
render/decode/encode/cache timing totals, JPEG2000 decode timing per backend, and Grok-to-OpenJPEG
fallback counts. On Linux it also emits process RSS and virtual-memory gauges from
`/proc/self/status`.

Every HTTP response includes `x-request-id`. If the client sends that header, the value is preserved;
otherwise the server generates a local id. Access logs are written as one JSON object per request on
stderr with request id, method, path, coarse route, status, and duration in milliseconds.

IIIF image responses include lightweight diagnostic headers:

```text
x-request-id
x-gigatiff-cache: hit|miss|disabled
x-gigatiff-total-ms
x-gigatiff-cache-read-ms
x-gigatiff-render-ms
x-gigatiff-encode-ms
x-gigatiff-cache-store-ms
x-gigatiff-cache-prune-ms
x-gigatiff-jp2-backend
x-gigatiff-openjpeg-threads
```

The server targets IIIF Image API 3.0 `level2`. It supports `full`/`square`/`x,y,w,h`/
`pct:x,y,w,h` regions; `max`, `full`, `w,`, `,h`, `!w,h`, `w,h`, and `pct:n` sizes; and the IIIF
`^` size prefix for explicit upscaling. Rotation supports `0`, `90`, `180`, and `270` degrees plus
IIIF mirroring with `!`. Qualities are `default`, `color`, `gray`, and `bitonal`; output formats are
`png`, `jpg`/`jpeg`, and `webp`. `info.json` advertises powers-of-two preferred `sizes` for full-image
requests that stay within `maxArea`. The server emits IIIF `Link` headers for the level 2 profile
and canonical image URI. WebP is currently encoded losslessly by the Rust `image` crate; JPEG uses
`--quality` and PNG uses fast compression. See `IIIF_COMPLIANCE.md` for the detailed feature matrix.

Server-only builds avoid the desktop GUI dependencies and are the expected native build mode on
Linux. Use the base command for TIFF-only deployments:

```bash
cargo build --release -p gigatiff-server --bin gigatiff-server
```

Use `--features jpeg2000-grok` for TIFF plus JPEG2000 deployments through Grok command-line tools,
`--features jpeg2000-openjpeg-ffi` for a pure OpenJPEG FFI JPEG2000 backend, or
`--features jpeg2000-grok-ffi` for the hybrid Docker/server path: Grok FFI for normal JP2 region
requests and OpenJPEG FFI for large-tile master JP2 files.

JPEG2000 feature builds also expose a runtime backend policy:

```bash
gigatiff-server --jp2-backend auto
gigatiff-server --jp2-backend grok
gigatiff-server --jp2-backend openjpeg
```

`auto` is the default. In the hybrid `jpeg2000-grok-ffi` build it chooses OpenJPEG FFI for JP2
codestreams whose tile width or height is at least 4096 px, because those NDK-style master files are
the cases where Grok 20.3.3 returned sparse gray-grid region output in local testing. Other JP2
files use Grok FFI because it is faster for the 1024 x 1024 tiled user-copy samples. If an auto
Grok render fails and OpenJPEG FFI is available, the server retries through OpenJPEG FFI. The actual
backend used for an IIIF image response is reported in `x-gigatiff-jp2-backend`.

`--openjpeg-threads` controls the maximum thread count used inside each OpenJPEG FFI region decode.
The server may choose a lower count for small or heavily downsampled OpenJPEG requests, while keeping
the configured maximum for large-tile first-viewport requests. Keep this balanced with
`--max-concurrent-renders-per-ip` and `--max-concurrent-renders-per-file`: for example, the Docker
examples use two concurrent per-file renders and four OpenJPEG threads, so a single first viewport
can use up to roughly eight OpenJPEG worker threads on large-tile master JP2 files. The actual count
used by an IIIF image response is reported in `x-gigatiff-openjpeg-threads`.

### Docker and Caddy

The Docker image builds the server package plus the Grok JPEG2000 FFI backend and the OpenJPEG FFI
fallback, and expects image files mounted at `/data`:

```bash
docker build -t gigatiff-server .
docker run --rm -p 8080:8080 -v "$PWD/images:/data:ro" -v "$PWD/cache:/cache" gigatiff-server
```

The included `docker-compose.yml` runs `gigatiff-server` behind Caddy on `127.0.0.1:18082` and uses
Dragonfly as the shared encoded response cache:

```bash
mkdir -p images
docker compose up --build
```

Then open `http://127.0.0.1:18082/api/images` or a returned `viewer_url`.

To run a native server against an existing Dragonfly instance:

```bash
gigatiff-server \
  --root /path/to/images \
  --cache-backend dragonfly \
  --dragonfly-url redis://127.0.0.1:6379/ \
  --cache-namespace gigatiff-server-response-v10
```

Every server CLI option can also be configured through an environment variable. See
`ops/gigatiff-server.env.example` for the full list. Command-line arguments still take precedence
when both are provided.

### Production Packaging

The repository includes first-pass production deployment templates under `ops/`:

- `ops/gigatiff-server.env.example` contains `GIGATIFF_*` environment variables for native, Compose,
  and Kubernetes deployments.
- `ops/compose.production.yml` runs GigaTIFF Server behind Caddy with Dragonfly as the shared encoded
  response cache, read-only image mounts, a read-only container filesystem, dropped Linux
  capabilities, and health checks.
- `ops/systemd/gigatiff-server.service` is a hardened native Linux service example using
  `EnvironmentFile=/etc/gigatiff/gigatiff-server.env`.
- `ops/kubernetes/gigatiff-server.yaml` is a minimal Deployment/Service example with liveness and
  readiness probes, read-only root filesystem, resource requests/limits, and a ConfigMap-based
  environment.
- `scripts/build-server-image.ps1` builds the server image with Docker BuildKit SBOM and provenance
  attestations enabled.

The Dockerfile uses Debian Trixie for both build and runtime stages (`rust:1.97.1-trixie` and
`debian:trixie-slim`) and exposes `/healthz` as the image healthcheck. The Compose templates pin
Caddy and Dragonfly to explicit tags instead of `latest`; for stricter production reproducibility,
replace tags with image digests in your deployment environment.

Example image build with SBOM/provenance:

```powershell
.\scripts\build-server-image.ps1 -Image ghcr.io/bezverec/gigatiff-server:0.3.3
```

Multi-arch publication can use the same helper when the builder supports the requested platforms:

```powershell
.\scripts\build-server-image.ps1 `
  -Image ghcr.io/bezverec/gigatiff-server:0.3.3 `
  -Platform linux/amd64,linux/arm64 `
  -Push
```

The current runtime configuration surface is CLI plus environment variables. TOML/YAML config files
are still a planned convenience layer, useful once the server has more deployment profiles and
per-backend tuning knobs.

### IIIF Smoke Test

The IIIF smoke helper checks the advertised profile, JSON-LD media type, base URI redirect, CORS,
preferred `sizes`, advertised tile geometry, profile/canonical `Link` headers, representative level 2
image requests, selected negative requests, and canonical cache-key reuse:

```powershell
.\scripts\iiif-smoke.ps1 -BaseUrl http://127.0.0.1:18082 -ImageId mapa2.tif
```

For CI or local testing without large sample images, generate a tiny baseline TIFF fixture and run
the same smoke test with smaller regions:

```powershell
.\scripts\new-test-tiff.ps1 -OutPath images\ci-smoke.tif -Width 64 -Height 64
.\scripts\iiif-smoke.ps1 -BaseUrl http://127.0.0.1:18082 -ImageId ci-smoke.tif -RegionSize 32 -OutputSize 16
```

GitHub Actions includes an `IIIF server smoke` job that generates this fixture, starts the Docker
Compose stack, and runs the smoke test when the workflow is started manually or for GitHub pull
requests. Routine Linux server checks are intentionally handled by Codeberg/Woodpecker to avoid
spending GitHub Actions minutes on every push.

### Server Benchmarks

The first server benchmark helper measures cold/warm tile requests, cache headers, server-side timing
headers, cold render/encode/cache-store phases, output size, and a simple parallel batch:

```powershell
.\scripts\bench-server.ps1 -BaseUrl http://127.0.0.1:18082 -Iterations 5 -Parallel 8
```

Use `-RegionSizes` and `-OutputSizes` to compare multiple IIIF request shapes in one run.
`-PurgeServerCache` clears the server through `DELETE /api/cache`, which is useful when benchmarking
the Docker/Caddy stack. `-OutDir` writes both JSON and CSV artefacts:

```powershell
.\scripts\bench-server.ps1 -BaseUrl http://127.0.0.1:18082 `
    -Formats webp,jpg,png `
    -RegionSizes 256,512,1024 `
    -OutputSizes 128,256,512 `
    -PurgeServerCache `
    -OutDir target/server-benchmarks
```

Use `-ClearCache` when you want to delete files under a local cache directory instead of using the
HTTP purge endpoint. Add `-Json` when another script should consume the table directly from stdout.

JPEG2000 first-load experiments use a dedicated helper because the important user-visible path is
not a single tile, but the first OpenSeadragon viewport worth of advertised tiles:

```powershell
.\scripts\bench-jp2-firstload.ps1 -BaseUrl http://127.0.0.1:18082 `
    -ImageIds mapa2_no_xmp_clean_master.jp2,mapa2_master.jp2 `
    -BatchCount 8 `
    -OutDir target\server-benchmarks-jp2
```

The helper records cold/warm `info.json`, metadata, thumbnail, fixed tile, advertised tile, and
startup-viewport batches. It also stores cache state, JP2 backend, server timing headers, HTTP
status, and any error text so failed OpenJPEG/Grok requests remain part of the benchmark record.
`-ViewerPrewarmDelayMs` additionally measures the opt-in viewer prewarm path by loading
`/viewer/<id>?prewarm=1`, waiting for the configured delay, and then requesting the startup viewport.

JPEG2000 backend quality checks compare a candidate server against an OpenJPEG reference server.
The script requests the same IIIF PNG regions from both endpoints, stores both images, records
timing headers, and fails when mean pixel difference or the ratio of visibly different pixels crosses
the configured thresholds:

```powershell
.\scripts\test-jp2-artifacts.ps1 `
    -CandidateBaseUrl http://127.0.0.1:18110 `
    -ReferenceBaseUrl http://127.0.0.1:18111 `
    -OutDir target\jp2-artifact-tests
```

Run the candidate with `--jp2-backend grok` to test Grok FFI directly, or with `--jp2-backend auto`
to validate the production routing policy. `-IncludeFull512` and `-IncludeFull1024` add full-image
thumbnail requests; the default focuses on region/tile requests because those are the viewer path
most likely to expose JP2 tile artefacts.

## libtiff

The build script links against libtiff through a platform-specific discovery path.

On Windows, it looks for libtiff here by default:

```text
C:\temp\libtiff\install
```

Another installation can be selected through `LIBTIFF_DIR`. The directory must contain:

```text
include\tiff.h
include\tiffio.h
lib\tiff.lib
bin\tiff.dll
```

The same layout is also accepted from vcpkg, for example:

```powershell
vcpkg install tiff:x64-windows lcms:x64-windows
$env:LIBTIFF_DIR="$env:VCPKG_INSTALLATION_ROOT\installed\x64-windows"
cargo build
```

On Linux, install libtiff, lcms2, pkg-config, and the GUI build dependencies:

```bash
sudo apt-get update
sudo apt-get install -y \
  libtiff-dev \
  liblcms2-dev \
  pkg-config \
  libgtk-3-dev \
  libx11-dev \
  libxi-dev \
  libxcursor-dev \
  libxrandr-dev \
  libxinerama-dev \
  libxkbcommon-dev \
  libxkbcommon-x11-dev \
  libwayland-dev \
  libgl1-mesa-dev
cargo build
```

On macOS, Homebrew provides the required libraries:

```bash
brew install libtiff little-cms2 pkg-config
cargo build
```

## Platform Notes

The project has a GitHub Actions CI workflow for Windows, Linux, and macOS. Windows uses vcpkg in CI,
while Linux and macOS use `pkg-config` to discover libtiff. GitHub CI is intentionally limited to
manual runs and GitHub pull requests so release packaging does not consume minutes on every push.

The Codeberg mirror contains a Woodpecker pipeline for routine Linux server checks. It runs in a
`rust:1-trixie` container, installs libtiff/lcms2 development packages, checks formatting, and runs
the server-only test target:

```bash
cargo fmt --all --check
cargo test --locked -p gigatiff-server --lib --bin gigatiff-server
```

The GUI has been primarily exercised on Windows so far. Linux/macOS runtime testing is the next
portability step after CI confirms the project compiles on all three platforms.

## Project Layout

Shared image handling lives in `crates/gigatiff-core/src/`:

- `cache.rs` contains shared scanline cache primitives,
- `color.rs` contains color conversion and fast-path row sampling,
- `render.rs` contains preview rendering, sampling, and PNG export,
- `tiff_info.rs` contains TIFF metadata loading and decoder helpers,
- `grok_ffi.rs` contains the optional Grok JPEG2000 FFI backend,
- `options.rs` contains shared backend and PNG compression enums.

The desktop application lives in `crates/gigatiff-desktop/src/`:

- `main.rs` is the `gigatiff` binary entry point,
- `cli.rs` handles command-line parsing and CLI command dispatch,
- `gui.rs` contains the egui/eframe viewer,
- `gui/cache.rs` contains desktop-only tile/overview cache code,
- `gui/render_queue.rs` contains GUI render queue types.

The server application lives in `crates/gigatiff-server/src/`:

- `main.rs` is the `gigatiff-server` binary entry point,
- `lib.rs` contains the IIIF/OpenSeadragon server implementation and server tests.

## Current Crates

Direct dependencies are pinned to current crates.io releases:

```text
anyhow  = 1.0.102
axum    = 0.8.9
clap    = 4.6.1
eframe  = 0.34.3
image   = 0.25.10
lcms2   = 6.1.1
openjpeg-sys = 1.0.12
percent-encoding = 2.3.2
png     = 0.18.1
rayon   = 1.12.0
redis   = 1.2.2
rfd     = 0.17.2
serde   = 1.0.228
serde_json = 1.0.150
tiff    = 0.11.3
tokio   = 1.52.3
tower-http = 0.6.11
```

Desktop-specific dependencies are isolated in the `gigatiff-desktop` package, and server-specific
HTTP/encoding dependencies are isolated in the `gigatiff-server` package. GitHub CI can check the
desktop app on Windows, Linux, and macOS, while Codeberg/Woodpecker handles the routine Linux
server-only path.

The `jpeg2000-grok` feature on `gigatiff-server` does not add Rust crate dependencies; it enables the
server-side Grok command backend and requires `grk_dump` and `grk_decompress` at runtime. The
`jpeg2000-grok-ffi` feature additionally enables `gigatiff-core/jpeg2000-grok-ffi` and builds the
vendored `grokj2k-sys` bindings. The `jpeg2000-openjpeg-ffi` feature enables
`gigatiff-core/jpeg2000-openjpeg-ffi` and builds `openjpeg-sys`; it is used as a standalone JP2
backend or as the large-tile fallback inside the hybrid `jpeg2000-grok-ffi` server build.

## Color Management

The reader loads an embedded ICC profile from the TIFF `IccProfile` tag when present.
The `info` command prints the ICC profile size, and the GUI shows either `ICC ... bytes` or `no ICC`.

The default `libtiff` scanline backend and the raw-strip fallback convert RGB/RGBA/Gray data in both
8-bit and 16-bit formats to sRGB through `lcms2`. The server also applies embedded JPEG2000 ICC
profiles on the OpenJPEG FFI backend before encoding browser responses. If a source has no ICC
profile, the renderer keeps the faster path without a color transform.

## Performance Notes

### Desktop and CLI Rendering

The GUI renders visible image content as source-aligned tile textures. Missing tiles are rendered by
a small worker pool and inserted incrementally, so panning can reuse tiles that are already in memory
instead of redrawing the full viewport. Rendered tile textures are kept in a 384 MiB LRU cache.
Visible missing tiles are scheduled by distance from the viewport center, so the most important part
of the image appears first. Once all visible tiles are ready, the GUI opportunistically prefetches the
nearest one-tile ring around the viewport. Visible tiles always stay higher priority than prefetch
work.

Each render worker has its own source-row cache and its own single-threaded TIFF decode path. Jobs use
a generation token so outdated renders can stop while the user is still panning or zooming. Rendered
bitmaps are stored in an `Arc` until they are uploaded as GUI textures, so applying a finished render
no longer copies the full RGBA buffer.

Each worker also keeps a 128 MiB LRU cache of raw source-row segments for both libtiff scanlines and
direct raw-strip reads. Repeated viewport renders can reuse already-read source rows before applying
sampling and color conversion. The GUI/CLI timing label reports row cache hits when that worker cache
is used.

The GUI also keeps a persistent full-image overview cache for fast first-window previews. The cached
overview is an internal RGBA file keyed by the TIFF path, file size, modification time, dimensions,
layout, and color metadata. It is drawn as a low-resolution background while higher-resolution tiles
arrive. Cache files live under the platform cache directory, for example
`%LOCALAPPDATA%\GigaTIFF\overview-cache` on Windows, and are pruned to roughly 256 MiB.

In `auto` mode, uncompressed stripped TIFFs use the raw-strip path before libtiff. This avoids reading
whole scanlines when the viewport only needs a narrow horizontal region.

The renderer precomputes source `x` offsets and `y` rows for each viewport before entering the hot
sampling loops. The scanline and raw-strip paths then sample each output row into a compact buffer
and run embedded ICC conversion through `lcms2` per row instead of per pixel. CLI preview output also
reports basic timing for render, read/decode, conversion/blit, and PNG writing.

For TIFFs without embedded ICC profiles, common RGB8/RGBA8/Gray8 and 16-bit variants use a direct
sample-to-RGBA row path. This avoids the intermediate sampled-row buffer and per-pixel conversion
dispatch used by the color-managed path. On x86/x86_64, RGB8 and RGBA8 rows also use a small SSE2
block writer that emits four sampled RGBA pixels at a time before finishing any tail pixels with the
scalar fallback.

For larger ICC-managed viewports, source rows are read sequentially and then sampled/color-converted
in parallel row batches through `rayon`. File access and libtiff handles stay single-threaded; only
the CPU conversion stage is parallelized.

PNG export defaults to the `fast` compression mode because preview/export latency is usually more
important than squeezing out the smallest possible PNG. CLI exports can choose `none`, `fastest`,
`fast`, `balanced`, or `high` with `--png-compression`.

### Server Rendering

For TIFF/BigTIFF sources, the server reuses the shared viewport renderer, then encodes the result as
PNG, JPEG, or WebP for IIIF image responses. It keeps a persistent encoded response cache on disk, so
repeated tile/region requests can skip TIFF reads, sampling, color conversion, and image encoding.

For JPEG2000 sources, the optional Grok CLI backend decodes only the requested IIIF region through
`grk_decompress` and chooses a JPEG2000 reduction level when the requested output is substantially
smaller than the source region. The Docker/default server image uses the direct Grok FFI backend for
ordinary JP2 region requests to avoid process spawning and temporary PNM files.

Large-tile JP2 master copies are a special case. Grok 20.3.3 can report successful region/reduced
decodes for 4096 x 4096 tiled master files while returning sparse gray-grid image data. For those
sources, the hybrid server falls back to the direct OpenJPEG FFI backend, then applies the same IIIF
geometry, quality conversion, encoding, and persistent response cache used by TIFF responses. First
requests for those master tiles are slower than TIFF and Grok user-copy JP2 tiles, but warm requests
are served from the encoded response cache.

The OpenJPEG FFI path reads embedded JP2 ICC profiles and converts decoded Gray/RGB 8-bit and 16-bit
samples to sRGB through `lcms2`. The persistent response cache namespace is bumped when this color
pipeline changes so old cached tiles do not mask corrected output.

## Benchmarks

### Local Benchmark Data

The large benchmark/sample images are intentionally not stored in the repository. Local runs used two
TIFF files in the project root and four derived JPEG2000 files under `images/`; `*.tif`, `*.tiff`,
and `*.jp2` are ignored by Git.

```text
file                              size       dimensions     samples  layout
mapa2_no_xmp_clean.tif            3.34 GiB   41174 x 29077  RGB 8    classic TIFF, uncompressed strips, rows/strip 1024, no ICC
mapa2.tif                         6.69 GiB   41174 x 29077  RGB 16   BigTIFF, one uncompressed strip, embedded ICC 1992 bytes
images/mapa2_no_xmp_clean_master.jp2
                                  1.36 GiB   41174 x 29077  RGB 8    JP2 master, 4096 x 4096 tiles
images/mapa2_no_xmp_clean_user_1_8.jp2
                                  0.42 GiB   41174 x 29077  RGB 8    JP2 user copy, 1024 x 1024 tiles
images/mapa2_master.jp2           4.65 GiB   41174 x 29077  RGB 16   JP2 master, 4096 x 4096 tiles
images/mapa2_user_1_8.jp2         0.84 GiB   41174 x 29077  RGB 16   JP2 user copy, 1024 x 1024 tiles
```

The JP2 master copies were generated with Grok from each TIFF sample:

```bash
grk_compress -i example.tif -o example_master.jp2 \
  -t 4096,4096 -p RPCL -n 6 \
  -c "[256,256],[256,256],[128,128],[128,128],[128,128],[128,128]" \
  -b 64,64 -X -M 1 -S -E -u R -H 1
```

The JP2 user copies used the 1:8 rate list and irreversible transform:

```bash
grk_compress -i example.tif -o example_user_1_8.jp2 \
  -r "362,256,181,128,90,64,45,32,22,16,11,8" -I \
  -t 1024,1024 -p RPCL -n 6 \
  -c "[256,256],[256,256],[128,128],[128,128],[128,128],[128,128]" \
  -b 64,64 -X -M 1 -u R -H 1
```

The `-H 1` thread limit was required for reliable conversion of the multi-gigabyte TIFF inputs in
Docker. Without it, Grok was killed during the larger conversions due to peak memory pressure.

### Desktop and CLI Benchmarks

These are informal release-build measurements from the local sample files on this machine. The binary
was built with:

```powershell
$env:RUSTFLAGS="-C target-cpu=native"; cargo build --release -p gigatiff-desktop --bin gigatiff
```

For a 2048 x 2048 source viewport exported as a 512 x 512 PNG:

```text
mapa2.tif auto:
raw strip reads + lcms2 ICC, total 16.2 ms, read 4.0 ms, convert 4.0 ms, png Fast 3.1 ms

mapa2.tif --backend libtiff:
libtiff scanlines + lcms2 ICC, total 75.7 ms, read 48.3 ms, convert 4.4 ms, png Fast 3.0 ms

mapa2_no_xmp_clean.tif auto:
raw strip reads + SSE2 RGB8 sampling, total 3.0 ms, read 2.3 ms, convert 0.3 ms, png Fast 2.8 ms

mapa2_no_xmp_clean.tif --backend libtiff:
libtiff scanlines + SSE2 RGB8 sampling, total 32.7 ms, read 23.6 ms, convert 0.3 ms, png Fast 2.4 ms
```

PNG compression comparison on `mapa2_no_xmp_clean.tif`, 512 x 512 output:

```text
fast:     png 3.0 ms, 620090 bytes
balanced: png 54.6 ms, 606334 bytes
high:     png 62.9 ms, 606182 bytes
none:     png 1.7 ms, 1049236 bytes
```

Parallel ICC row conversion on `mapa2.tif`, 4096 x 4096 source viewport exported as 2048 x 2048:

```text
RAYON_NUM_THREADS=1:
total 113.8 ms, read 16.4 ms, convert 87.0 ms, png Fast 29.2 ms

default rayon thread pool:
total 69.7 ms, read 16.3 ms, convert 27.3 ms, png Fast 28.8 ms
```

The GUI tile worker pool is not directly represented by these CLI preview timings. In the viewer, the
benefit is interactive: up to four independent tile jobs can be in flight, each with its own
single-threaded TIFF decode path and source-row cache, while the scheduler prioritizes missing tiles
closest to the center of the viewport before filling edges and prefetching nearby tiles.

### Server Benchmarks

Server benchmark through Docker Compose and Caddy on `http://127.0.0.1:18082`, requesting a
512 x 512 source region scaled to 128 px output. The benchmark used
`.\scripts\bench-server.ps1 -Iterations 5 -Parallel 8 -ClearCache` for the cold-cache pass and the
same command without `-ClearCache` for the warm-cache pass.

For tile-size sweeps, use `-RegionSizes 256,512,1024` and `-OutputSizes 128,256,512`. The result
table includes `RegionSize` and `OutputSize` columns so rendering cost, encoding cost, response size,
and cache behavior can be compared across IIIF request shapes. The helper also records server timing
averages for render, encode, cache read, cache store, and cache prune phases when those headers are
present.

Recent TIFF/JPEG2000 server comparison used JPEG output, two source region sizes, and 512 px
output. Each value below reports the average server-side render phase after purging the persistent
response cache before each request. Render time excludes JPEG encoding and cache storage.

The current TIFF values are from the Grok FFI Docker image, but the TIFF path is identical between
the FFI and CLI server builds. A second alternated-order run confirmed that the TIFF differences
between those two images are measurement noise and operating-system page cache effects, not a code
path difference:

```text
source                         previous     current FFI
                               1024  4096   1024  4096
mapa2_no_xmp_clean.tif         42.5  50.1   39.9  44.5 ms
mapa2.tif                      85.9 105.7   81.2 103.0 ms
```

For JPEG2000 region requests, the new FFI path improves the master-copy cases and keeps all results
competitive with the previous documented Grok CLI baseline. The lower-rate user-copy files are more
mixed: compared with the current CLI build, FFI is slower for small region requests, although still
substantially better than the previous documented 4096 px user-copy results.

```text
source                         previous     current FFI
                               1024  4096   1024  4096
mapa2_no_xmp_clean_master.jp2  21.6  20.1    6.3   7.4 ms
mapa2_no_xmp_clean_user_1_8.jp2
                               40.3 124.1   35.2  54.6 ms
mapa2_master.jp2               30.2  30.2   15.2  18.8 ms
mapa2_user_1_8.jp2             49.9 150.8   55.2  72.1 ms
```

The direct FFI-vs-CLI comparison below was run from two Docker images built from the same source
tree. Both used upstream Grok `v20.3.3`; requests were alternated between backends to reduce OS page
cache bias. Values are average render phase in milliseconds over five cache-purged requests. The
large-tile master rows are retained as historical timing data for Grok FFI, but that path is no
longer used for those sources because the rendered image data was visually incorrect.

```text
source                         CLI   FFI    CLI   FFI
                               1024  1024   4096  4096
mapa2_no_xmp_clean_master.jp2  16.2   6.3   15.7   7.4 ms
mapa2_master.jp2               26.4  15.2   26.2  18.8 ms
mapa2_no_xmp_clean_user_1_8.jp2
                               27.2  35.2   44.4  54.6 ms
mapa2_user_1_8.jp2             42.2  55.2   58.8  72.1 ms
```

After the direct OpenJPEG FFI fallback replaced the older `opj_decompress` process fallback for
4096 x 4096 tiled master JP2 files, the same master regions render as continuous image data without
spawning an external process or writing temporary PNM files. The local benchmark below used a Windows
release build with `RUSTFLAGS="-C target-cpu=native"` and the standalone
`jpeg2000-openjpeg-ffi` feature. It ran against `http://127.0.0.1:18085` with a purged persistent
response cache, JPEG output, 512 px output size, three warm iterations, and a parallel batch size of
four:

```powershell
.\scripts\bench-server.ps1 -BaseUrl http://127.0.0.1:18085 `
    -ImageIds mapa2_no_xmp_clean.tif,mapa2.tif,mapa2_no_xmp_clean_master.jp2,mapa2_master.jp2,mapa2_no_xmp_clean_user_1_8.jp2,mapa2_user_1_8.jp2 `
    -Formats jpg -RegionSizes 512,4096 -OutputSizes 512 `
    -Iterations 3 -Parallel 4 -PurgeServerCache `
    -OutDir target\server-benchmarks-openjpeg-ffi
```

```text
image                            region  cold render  cold total  warm avg  bytes
mapa2_no_xmp_clean.tif              512       2.5 ms      7.5 ms    6.1 ms   41954
mapa2_no_xmp_clean.tif             4096       3.7 ms      8.9 ms    7.7 ms   96871
mapa2.tif                           512      11.6 ms     16.4 ms    7.0 ms   39456
mapa2.tif                          4096      19.5 ms     24.9 ms   11.6 ms  101519
mapa2_no_xmp_clean_master.jp2       512     131.8 ms    137.4 ms    5.5 ms   41954
mapa2_no_xmp_clean_master.jp2      4096      96.5 ms    102.0 ms    9.0 ms   96265
mapa2_master.jp2                    512     205.8 ms    211.3 ms    5.9 ms   41954
mapa2_master.jp2                   4096     151.7 ms    157.8 ms    6.2 ms   96086
mapa2_no_xmp_clean_user_1_8.jp2     512     244.4 ms    256.4 ms   11.3 ms   40604
mapa2_no_xmp_clean_user_1_8.jp2    4096     123.7 ms    128.8 ms    6.6 ms   78851
mapa2_user_1_8.jp2                  512     194.5 ms    204.7 ms    5.7 ms   41607
mapa2_user_1_8.jp2                 4096     208.0 ms    213.5 ms    6.0 ms   78850
```

Compared with the older CLI fallback numbers above, the direct OpenJPEG FFI path is roughly 8-12x
faster for the large master JP2 cold render phase on this machine. The standalone OpenJPEG backend is
not intended to replace Grok FFI for all JP2 sources yet: user-copy JP2 tiles are slower here than in
the hybrid Grok FFI path. The intended server configuration is therefore still hybrid: Grok FFI for
normal JP2 region requests and OpenJPEG FFI for large-tile masters that Grok renders incorrectly.
Warm-cache responses stay around 6-12 ms client-side because they come from the encoded response
cache instead of re-running JPEG2000 decode.

The same 4096 -> 512 request shape was also run across JPEG, PNG, and lossless WebP output formats:

```text
image                            fmt   cold render  cold total  warm avg   bytes
mapa2_no_xmp_clean.tif           jpg       60.1 ms    106.5 ms   11.1 ms   96871
mapa2_no_xmp_clean.tif           png       69.7 ms    103.8 ms   25.8 ms  666518
mapa2_no_xmp_clean.tif           webp      69.8 ms    105.9 ms   24.2 ms  501468
mapa2.tif                        jpg      113.1 ms    133.8 ms   12.6 ms  101519
mapa2.tif                        png      117.8 ms    157.1 ms   30.4 ms  669429
mapa2.tif                        webp     110.4 ms    151.0 ms   30.2 ms  521496
mapa2_no_xmp_clean_master.jp2    jpg     1113.9 ms   1136.5 ms   16.6 ms   96265
mapa2_no_xmp_clean_master.jp2    png     1131.2 ms   1168.9 ms   27.9 ms  678752
mapa2_no_xmp_clean_master.jp2    webp    1183.4 ms   1208.0 ms   19.8 ms  472178
mapa2_master.jp2                 jpg     1650.5 ms   1675.0 ms   12.7 ms  101172
mapa2_master.jp2                 png     1653.7 ms   1683.5 ms   29.0 ms  678042
mapa2_master.jp2                 webp    1586.2 ms   1615.1 ms   18.2 ms  494528
mapa2_no_xmp_clean_user_1_8.jp2  jpg       54.4 ms     74.0 ms   10.8 ms   78848
mapa2_no_xmp_clean_user_1_8.jp2  png       57.0 ms     87.8 ms   21.3 ms  584966
mapa2_no_xmp_clean_user_1_8.jp2  webp      57.2 ms     83.8 ms   18.6 ms  407142
mapa2_user_1_8.jp2               jpg       70.4 ms     96.7 ms   10.2 ms   83045
mapa2_user_1_8.jp2               png       70.9 ms    104.6 ms   17.9 ms  585963
mapa2_user_1_8.jp2               webp      71.0 ms     94.4 ms   18.6 ms  431200
```

Full-image JPEG2000 thumbnails are where the FFI backend provides a functional advantage for small
tile/user-copy JP2 files. In this run, the CLI backend failed all tested `full/512,` JP2 requests
through `grk_decompress`, while the FFI backend completed them successfully. Large-tile master
thumbnails now use the OpenJPEG fallback instead of Grok FFI:

```text
source                         FFI full -> 512 render
mapa2_no_xmp_clean_master.jp2         185.2 ms
mapa2_master.jp2                      284.7 ms
mapa2_no_xmp_clean_user_1_8.jp2      1182.4 ms
mapa2_user_1_8.jp2                   1256.0 ms
```

The TIFF and JP2 source files used for this comparison are described in the local benchmark data
section above.

The main cost of the FFI path is operational and maintenance complexity: the Docker build now
compiles upstream Grok and generates Rust bindings against the installed `grok.h`, which requires
`clang`/`libclang` in the build stage. The runtime image also installs OpenJPEG tools for the
large-tile master fallback. The benefit is that ordinary JPEG2000 decoding stays in-process, avoids
temporary PNM files and process spawning, can clamp reduction levels after reading the header, and
can support full-image IIIF thumbnails that the CLI path currently does not handle reliably.

JPEG2000 first-load viewport benchmark after adding `scripts/bench-jp2-firstload.ps1`. The startup
viewport scenario models a 2048 x 1024 first OpenSeadragon view using advertised IIIF tile geometry.
For large-tile master JP2 files, default `512` advertised tiles caused eight cold OpenJPEG fallback
requests. Advertising `1024` tiles reduced that first viewport to two requests and was faster than
both `512` and `2048` in this local run:

```text
run                      image                              startup viewport wall
tile 512                 mapa2_no_xmp_clean_master.jp2                    6612.7 ms
tile 512                 mapa2_master.jp2                                10113.1 ms
tile 1024 experiment     mapa2_no_xmp_clean_master.jp2                    2123.3 ms
tile 1024 experiment     mapa2_master.jp2                                 2766.8 ms
tile 2048 experiment     mapa2_no_xmp_clean_master.jp2                    2853.0 ms
tile 2048 experiment     mapa2_master.jp2                                 3998.5 ms
auto JP2 master policy   mapa2_no_xmp_clean_master.jp2                    1965.0 ms
auto JP2 master policy   mapa2_master.jp2                                 2860.6 ms
```

The resulting policy keeps TIFFs and explicit `--tile-size` settings unchanged, but when the server
uses the default `512` tile size and detects a large-tile JP2 master that is routed to the OpenJPEG
fallback, `info.json` advertises `1024 x 1024` tiles. This does not make a single JP2 decode faster;
it reduces the number of expensive first-viewport decodes.

OpenJPEG FFI thread-count benchmark on the same large-tile JP2 master samples, using the auto
`1024 x 1024` advertised-tile policy above. These runs were sequential Docker runs with a purged
server response cache and warmed operating-system file cache:

```text
openjpeg threads  image                              startup viewport wall  server render
1                 mapa2_no_xmp_clean_master.jp2                  1795.7 ms      1374.9 ms
2                 mapa2_no_xmp_clean_master.jp2                  1414.2 ms      1305.6 ms
4                 mapa2_no_xmp_clean_master.jp2                  1153.4 ms      1066.2 ms
1                 mapa2_master.jp2                               2063.0 ms      1952.1 ms
2                 mapa2_master.jp2                               1907.6 ms      1798.8 ms
4                 mapa2_master.jp2                               1801.4 ms      1710.9 ms
```

The single-tile timings were not uniformly best at four threads, but the real first-viewport scenario
was best at `--openjpeg-threads 4` on this machine. The CLI default remains conservative at `1`, while
Docker/ops examples set `GIGATIFF_OPENJPEG_THREADS=4` as the current large-JP2 starting point.

The next pass changed `--openjpeg-threads` from a fixed per-decode value to a maximum. With
`--openjpeg-threads 4`, large-tile first-viewport requests still use four OpenJPEG threads, while
small/downsampled OpenJPEG requests use two. The benchmark script now records the chosen value from
`x-gigatiff-openjpeg-threads`. The warmed rerun below compares the adaptive policy against the
previous fixed-4 baseline:

```text
image                              scenario                         fixed-4 wall  adaptive wall  delta   threads
mapa2_no_xmp_clean_master.jp2      full_1024                            9415.8 ms     8986.2 ms  -4.6%        2
mapa2_no_xmp_clean_master.jp2      tile_512_to_128                      1122.8 ms      921.8 ms -17.9%        2
mapa2_no_xmp_clean_master.jp2      tile_4096_to_512                      977.9 ms      967.0 ms  -1.1%        2
mapa2_no_xmp_clean_master.jp2      advertised_tile                      1034.2 ms     1046.9 ms  +1.2%        4
mapa2_no_xmp_clean_master.jp2      startup_viewport_advertised_tile     1153.4 ms     1147.8 ms  -0.5%        4
mapa2_master.jp2                   full_1024                           28194.9 ms    27942.0 ms  -0.9%        2
mapa2_master.jp2                   tile_512_to_128                      1438.8 ms     1558.7 ms  +8.3%        2
mapa2_master.jp2                   tile_4096_to_512                     1432.9 ms     1232.7 ms -14.0%        2
mapa2_master.jp2                   advertised_tile                      1972.6 ms     1470.3 ms -25.5%        4
mapa2_master.jp2                   startup_viewport_advertised_tile     1801.4 ms     1644.0 ms  -8.7%        4
```

The result is deliberately modest: it preserves the first-viewport path, improves several expensive
single-request cases, and shows one small-tile regression on the 16-bit master sample. The policy is
therefore treated as a measured heuristic rather than a universal OpenJPEG rule.

OpenJPEG FFI component-to-RGBA conversion now uses a parallel row path only when the OpenJPEG decode
itself is single-threaded. The first attempt also parallelized conversion while OpenJPEG was using
four internal decode threads, but that hurt first-viewport wall time through CPU contention. The
kept path is therefore adaptive: it helps the conservative `--openjpeg-threads 1` mode and stays
disabled for the Docker `--openjpeg-threads 4` default.

```text
openjpeg threads  path                         image                              startup viewport wall
1                 before parallel conversion   mapa2_no_xmp_clean_master.jp2                  1795.7 ms
1                 adaptive parallel conversion mapa2_no_xmp_clean_master.jp2                  1612.2 ms
1                 before parallel conversion   mapa2_master.jp2                               2063.0 ms
1                 adaptive parallel conversion mapa2_master.jp2                               1906.3 ms
1                 before parallel conversion   mapa2_no_xmp_clean_user_1_8.jp2                411.1 ms
1                 adaptive parallel conversion mapa2_no_xmp_clean_user_1_8.jp2                373.7 ms
1                 before parallel conversion   mapa2_user_1_8.jp2                             436.0 ms
1                 adaptive parallel conversion mapa2_user_1_8.jp2                             381.3 ms
```

The next first-load experiment added an opt-in viewer prewarm profile and in-process coalescing for
identical response-cache misses. The prewarm profile renders the first 2048 x 1024 viewport tiles
before the thumbnail, using the advertised tile size. With a 3 second lead time, this turns the
startup viewport into cache hits and makes the visible first view much faster:

```text
image                              baseline startup wall  prewarmed startup wall  lead time
mapa2_no_xmp_clean_master.jp2                  1158.4 ms                 94.0 ms  3000 ms
mapa2_master.jp2                               2038.3 ms                376.8 ms  3000 ms
mapa2_no_xmp_clean_user_1_8.jp2                 347.6 ms                111.2 ms  3000 ms
mapa2_user_1_8.jp2                              437.6 ms                 95.5 ms  3000 ms
```

The same benchmark with zero lead time showed why this path is opt-in instead of automatic on every
viewer open. Coalescing prevented duplicate renders (`x-gigatiff-render-ms` was `0` on the waiting
startup-viewport requests), but the viewport still waited for the prewarm owner and was slower than
letting OpenSeadragon make the first visible requests directly:

```text
image                              baseline startup wall  prewarm delay 0 wall  note
mapa2_no_xmp_clean_master.jp2                  1463.1 ms              2107.5 ms  coalesced wait
mapa2_master.jp2                               2045.5 ms              2860.0 ms  coalesced wait
```

The useful product direction is therefore earlier prewarm triggers, such as the image index, hover or
focus events, explicit warm/open actions, or a production prewarm queue. The server-side coalescing
still helps under burst load because it prevents multiple workers from decoding the same expensive
response at once.

Small WebP tile-size smoke run after the `pct:` IIIF update
(`-ImageIds mapa2_no_xmp_clean.tif -Formats webp -OutputSizes 128,256 -Iterations 2 -Parallel 2 -ClearCache`):

```text
output px  cold ms  warm avg ms  warm server ms  bytes   parallel wall ms
128          223.3         15.4             2.4   27836              72.9
256           52.8         18.1             3.2  105518              84.3
```

```text
Cold-cache single tile requests:

image                     format  cold ms  warm avg ms  bytes
mapa2_no_xmp_clean.tif    webp      293.7         15.1   27836
mapa2_no_xmp_clean.tif    jpg        64.8         15.1    3671
mapa2_no_xmp_clean.tif    png        60.6         16.0   34790
mapa2.tif                 webp       50.7         11.3   27750
mapa2.tif                 jpg        50.0          9.7    3437
mapa2.tif                 png        51.7         10.5   33930

Parallel batch of 8 distinct tile requests:

image                     format  cold wall ms  warm wall ms
mapa2_no_xmp_clean.tif    webp          273.3         149.1
mapa2_no_xmp_clean.tif    jpg           254.7         100.1
mapa2_no_xmp_clean.tif    png           184.5         129.8
mapa2.tif                 webp          190.5          98.2
mapa2.tif                 jpg           184.7          91.8
mapa2.tif                 png           212.6         152.3
```

The server emits `x-gigatiff-cache: miss|hit|disabled`. Warm-cache reads are served from the
persistent encoded response cache, so they avoid TIFF reads, sampling, color conversion, and image
encoding for repeated IIIF requests.

PGO release build after running the representative preview workloads below:

```text
mapa2.tif, 2048 x 2048 source viewport -> 512 x 512 PNG:
total 15.7 ms, read 3.8 ms, convert 3.3 ms, png Fast 3.1 ms

mapa2_no_xmp_clean.tif, 2048 x 2048 source viewport -> 512 x 512 PNG:
total 3.1 ms, read 2.5 ms, convert 0.2 ms, png Fast 3.5 ms

mapa2.tif, 4096 x 4096 source viewport -> 2048 x 2048 PNG:
total 69.4 ms, read 18.0 ms, convert 26.7 ms, png Fast 25.9 ms
```

## PGO Build

Profile-guided optimization is not enabled by default, but the project can be built with Rust/LLVM
PGO when the Rust LLVM tools component is installed. The commands below write merged profile data to
`target/pgo/gigatiff-pgo.profdata`; regenerate it whenever the render hot paths change.

```powershell
rustup component add llvm-tools-preview
$llvmProfdata = "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-msvc\lib\rustlib\x86_64-pc-windows-msvc\bin\llvm-profdata.exe"
```

1. Build an instrumented binary:

```powershell
$env:RUSTFLAGS="-C target-cpu=native -C profile-generate=C:\temp\tiffreader\target\pgo-data"
cargo build --release -p gigatiff-desktop --bin gigatiff
```

2. Run representative workloads:

```powershell
target\release\gigatiff.exe preview mapa2.tif --x 0 --y 0 --width 4096 --height 4096 --max-output 2048 --out pgo_mapa2.png
target\release\gigatiff.exe preview mapa2_no_xmp_clean.tif --x 0 --y 0 --width 4096 --height 4096 --max-output 2048 --out pgo_clean.png
target\release\gigatiff.exe preview mapa2_no_xmp_clean.tif --x 0 --y 0 --width 2048 --height 2048 --max-output 512 --out pgo_clean_512.png
target\release\gigatiff.exe preview mapa2.tif --x 0 --y 0 --width 2048 --height 2048 --max-output 512 --out pgo_mapa2_512.png
target\release\gigatiff.exe preview mapa2.tif --backend libtiff --x 0 --y 0 --width 2048 --height 2048 --max-output 1024 --out pgo_libtiff.png
```

3. Merge the generated profile data:

```powershell
& $llvmProfdata merge -o target\pgo\gigatiff-pgo.profdata target\pgo-data
```

4. Build the optimized binary:

```powershell
$env:RUSTFLAGS="-C target-cpu=native -C profile-use=C:\temp\tiffreader\target\pgo\gigatiff-pgo.profdata -C llvm-args=-pgo-warn-missing-function"
cargo build --release -p gigatiff-desktop --bin gigatiff
```

## Capabilities

### Desktop and CLI

- detects classic TIFF vs BigTIFF from the file header,
- prints dimensions, color type, compression, strip/tile organization, and ICC profile status,
- reads viewports through `TIFFReadScanline` in the `libtiff` backend,
- prefers direct raw-strip reads for suitable uncompressed stripped TIFFs in `auto` mode,
- applies embedded ICC profiles through `lcms2` where supported,
- reports preview timing in the CLI and GUI status bar,
- renders GUI viewports from cached source-aligned tile textures,
- schedules missing GUI tiles by distance from the viewport center,
- prefetches nearby GUI tiles after the visible viewport is cached,
- cancels outdated GUI render work when a newer viewport request arrives,
- caches recently used source-row segments inside each GUI render worker,
- keeps a 384 MiB LRU cache of rendered GUI tile textures,
- keeps a persistent full-image overview cache for faster repeat opens,
- uses a direct no-ICC row conversion fast path for common RGB/Gray/RGBA TIFFs,
- uses an SSE2 block writer for no-ICC RGB8/RGBA8 sampled rows on x86/x86_64,
- parallelizes ICC-managed row conversion with `rayon` while keeping file reads single-threaded,
- uses fast PNG compression by default for lower export latency,
- can directly seek through uncompressed stripped RGB/Gray TIFFs as a fallback,
- can decode additional supported strip/tile TIFFs through the Rust `tiff` crate fallback,
- renders GUI viewport tiles through a small worker pool,
- exports the current viewport as RGBA PNG.

### Server

- serves TIFF/BigTIFF files through a separate IIIF-compatible `gigatiff-server` binary,
- serves JPEG2000 through Grok CLI/FFI feature builds, with an OpenJPEG fallback for large-tile JP2
  master files that Grok 20.3.3 does not region-decode correctly,
- provides a minimal OpenSeadragon browser viewer,
- targets IIIF Image API 3.0 `level2` with region, size, rotation, mirroring, color/gray/bitonal
  quality, preferred-sizes, profile-link, canonical-link, and base-URI redirect coverage,
- emits PNG, JPEG, and WebP IIIF image responses,
- supports persistent encoded response caching with canonical IIIF cache keys, using either local
  disk storage or Dragonfly/Redis-compatible shared storage,
- exposes metadata, cache stats, cache warmup, cache purge, health/readiness, and Prometheus metrics
  endpoints,
- applies per-server, per-client, and per-file render concurrency limits,
- applies request rate limiting, render response timeouts, maximum output pixels, maximum upscale,
  stricter IIIF URL validation, and read-only-root deployment checks,
- includes a tiny generated TIFF fixture and IIIF smoke test for CI,
- includes Docker and Caddy configuration for local browser testing plus first-pass Compose,
  systemd, Kubernetes, and SBOM-oriented production packaging examples.

## Sample Files

`mapa2_no_xmp_clean.tif` is a stripped RGB8 TIFF with 1024-row strips.

`mapa2.tif` is a BigTIFF RGB16 file with one huge full-image strip and an embedded ICC profile.
Without the `libtiff` scanline path or the specialized raw-strip path, a normal chunk decoder would
need to allocate roughly the whole image, which is exactly the memory pressure this viewer avoids.

## Next Steps

Useful next steps for the desktop viewer:

- add automated GUI interaction benchmarks for pan/zoom/tile warm-cache scenarios,
- tune tile sizing, worker count, overview-cache size, and eviction policy from those GUI benchmarks,
- benchmark and tune the explicit SIMD RGB8/RGBA8 path against more sampling ratios,
- continue Linux and macOS runtime validation, packaging, and file-dialog checks.

Useful next steps for the image server:

- keep the Grok FFI backend as the default Docker path and preserve the CLI backend as a fallback
  build feature,
- add CI coverage for `gigatiff-server --features jpeg2000-grok-ffi` in a Linux container with upstream Grok installed,
- benchmark the direct OpenJPEG FFI fallback in the Linux/Docker hybrid build across master files,
  overview requests, output sizes, and repeated warm-cache OpenSeadragon navigation,
- investigate whether Grok has a newer region-decode path or parameter set that fixes sparse
  gray-grid output for 4096 x 4096 RPCL master codestreams,
- tune the direct OpenJPEG FFI path further, especially adaptive thread count, reduction choice, and
  component conversion cost,
- add earlier first-load prewarm triggers for the web UI, starting with image-list hover/focus and an
  explicit warm/open action, then measure browser time-to-first-visible-tile,
- expose request coalescing and viewer-prewarm counters in `/metrics`, so production runs can show
  how often prewarm helps, skips, waits, or races visible requests,
- investigate why lower-rate JP2 user-copy region requests are slower through FFI than through the
  CLI backend, especially around Grok reduction/update behavior and component copy cost,
- benchmark full-image JP2 thumbnails separately from tile/region requests and tune reduction
  selection for both Grok FFI and OpenJPEG fallback paths,
- run tile-size and output-size sweeps for WebP/JPEG/PNG and record the best default request shapes,
- harden and benchmark Dragonfly-backed shared response cache storage for multi-instance deployments,
  including production maxmemory settings and cache prewarming queues,
- add TOML/YAML config-file loading on top of the current CLI/environment configuration surface,
- pin production container images by digest once release image publishing is stable,
- add a real multi-arch Linux server image pipeline for `linux/amd64` and `linux/arm64` if ARM
  deployment becomes useful,
- add automated OpenSeadragon browser smoke tests on top of the current HTTP-level IIIF smoke test,
- evaluate lossy WebP encoding options once the Rust ecosystem path is stable enough for release builds.

Useful next release and deployment steps:

- publish a Linux server Docker image after the Grok FFI CI path is stable,
- document the server support policy explicitly: native Linux first, Docker/Podman for other systems,
- add Caddy auth/TLS examples for team sharing,
- wire SBOM/provenance attestation upload into the release image workflow,
- decide whether the next public version is a combined `0.3.0` release or separate Desktop/Server
  version labels.
