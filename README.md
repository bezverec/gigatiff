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
`GIGATIFF_GROK_DECOMPRESS`. The Docker image enables this feature and installs Grok tools. Native
non-container Linux deployments need `grokj2k-tools` installed. Grok is AGPL-licensed, so this backend
is kept server-only and optional in Cargo builds.

The server stores encoded IIIF region/tile responses in a persistent cache. By default this is
`cache/server`; it can be changed with `--cache-dir`. Cached files are keyed by source path, file
size, modification time, image dimensions, backend, encoder settings, and the canonical IIIF image
URI, so equivalent request spellings reuse the same cache entry. The cache is pruned after writes to
stay under `--cache-max-mb` (default `4096`). Set `--cache-max-mb 0` to disable the persistent
response cache. Concurrent TIFF render jobs are limited with `--max-concurrent-renders` to avoid
flooding libtiff with too many simultaneous requests during fast OpenSeadragon pan/zoom interaction.

Useful endpoints:

```text
GET /api/images
GET /api/cache
DELETE /api/cache
GET /viewer/<image-id>
GET /iiif/3/<image-id>
GET /iiif/3/<image-id>/info.json
GET /iiif/3/<image-id>/<region>/<size>/<rotation>/<quality>.<format>
```

The root page `/` is a small local dashboard with image links, cache size, last prune/purge summary,
and a manual cache purge action.

IIIF image responses include lightweight diagnostic headers:

```text
x-gigatiff-cache: hit|miss|disabled
x-gigatiff-total-ms
x-gigatiff-cache-read-ms
x-gigatiff-render-ms
x-gigatiff-encode-ms
x-gigatiff-cache-store-ms
x-gigatiff-cache-prune-ms
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
or `--features jpeg2000-grok-ffi` for the direct Grok FFI backend used by the Docker image.

### Docker and Caddy

The Docker image builds the server package plus the Grok JPEG2000 FFI backend and expects image files
mounted at `/data`:

```bash
docker build -t gigatiff-server .
docker run --rm -p 8080:8080 -v "$PWD/images:/data:ro" -v "$PWD/cache:/cache" gigatiff-server
```

The included `docker-compose.yml` runs `gigatiff-server` behind Caddy on `127.0.0.1:18082`:

```bash
mkdir -p images
docker compose up --build
```

Then open `http://127.0.0.1:18082/api/images` or a returned `viewer_url`.

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
headers, output size, and a simple parallel batch:

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
percent-encoding = 2.3.2
png     = 0.18.1
rayon   = 1.12.0
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
vendored `grokj2k-sys` bindings.

## Color Management

The reader loads an embedded ICC profile from the TIFF `IccProfile` tag when present.
The `info` command prints the ICC profile size, and the GUI shows either `ICC ... bytes` or `no ICC`.

The default `libtiff` scanline backend and the raw-strip fallback convert RGB/RGBA/Gray data in both
8-bit and 16-bit formats to sRGB through `lcms2`. If a TIFF has no ICC profile, the viewer keeps the
faster path without a color transform.

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

For JPEG2000 sources, the optional Grok backend decodes only the requested IIIF region through
`grk_decompress` and chooses a JPEG2000 reduction level when the requested output is substantially
smaller than the source region. The server then applies the same IIIF geometry, quality conversion,
encoding, and persistent response cache used by TIFF responses.

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
cache bias. Values are average render phase in milliseconds over five cache-purged requests.

```text
source                         CLI   FFI    CLI   FFI
                               1024  1024   4096  4096
mapa2_no_xmp_clean_master.jp2  16.2   6.3   15.7   7.4 ms
mapa2_master.jp2               26.4  15.2   26.2  18.8 ms
mapa2_no_xmp_clean_user_1_8.jp2
                               27.2  35.2   44.4  54.6 ms
mapa2_user_1_8.jp2             42.2  55.2   58.8  72.1 ms
```

Full-image JPEG2000 thumbnails are where the FFI backend currently provides a functional advantage,
not just a speed tradeoff. In this run, the CLI backend failed all tested `full/512,` JP2 requests
through `grk_decompress`, while the FFI backend completed them successfully:

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
`clang`/`libclang` in the build stage. The benefit is that JPEG2000 decoding stays in-process, avoids
temporary PNM files and process spawning, can clamp reduction levels after reading the header, and
can support full-image IIIF thumbnails that the CLI path currently does not handle reliably.

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
- provides a minimal OpenSeadragon browser viewer,
- targets IIIF Image API 3.0 `level2` with region, size, rotation, mirroring, color/gray/bitonal
  quality, preferred-sizes, profile-link, canonical-link, and base-URI redirect coverage,
- emits PNG, JPEG, and WebP IIIF image responses,
- supports persistent encoded response caching with size pruning and canonical IIIF cache keys,
- exposes cache stats and manual cache purge endpoints,
- includes a tiny generated TIFF fixture and IIIF smoke test for CI,
- limits concurrent TIFF render jobs to avoid flooding libtiff during fast pan/zoom interaction,
- includes Docker and Caddy configuration for local browser testing.

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
- investigate why lower-rate JP2 user-copy region requests are slower through FFI than through the
  CLI backend, especially around Grok reduction/update behavior and component copy cost,
- benchmark full-image JP2 thumbnails separately from tile/region requests and tune reduction
  selection for `full/<width>,` IIIF requests,
- run tile-size and output-size sweeps for WebP/JPEG/PNG and record the best default request shapes,
- add automated OpenSeadragon browser smoke tests on top of the current HTTP-level IIIF smoke test,
- evaluate lossy WebP encoding options once the Rust ecosystem path is stable enough for release builds.

Useful next release and deployment steps:

- publish a Linux server Docker image after the Grok FFI CI path is stable,
- document the server support policy explicitly: native Linux first, Docker/Podman for other systems,
- add Caddy auth/TLS examples for team sharing,
- decide whether the next public version is a combined `0.3.0` release or separate Desktop/Server
  version labels.
