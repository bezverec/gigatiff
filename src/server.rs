use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{Path as AxumPath, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, HOST, LINK, LOCATION};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use clap::Parser;
use image::ExtendedColorType;
use image::ImageEncoder;
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use serde::Serialize;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::options::{Backend, PngCompression};
use crate::render::{Rect, clamp_rect, render_preview};
use crate::tiff_info::{ImageInfo, load_info};

const ID_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/')
    .add(b'\\');

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "GigaTIFF Server: an IIIF-compatible TIFF/BigTIFF image server"
)]
struct ServerCli {
    /// Directory containing TIFF/BigTIFF files exposed by the server.
    #[arg(long, default_value = ".")]
    root: PathBuf,

    /// HTTP bind address.
    #[arg(long, default_value = "127.0.0.1:8080")]
    addr: SocketAddr,

    /// Preferred IIIF tile size advertised in info.json.
    #[arg(long, default_value_t = 512)]
    tile_size: u32,

    /// Maximum encoded response area in pixels.
    #[arg(long, default_value_t = 16_777_216)]
    max_output_pixels: u32,

    /// Maximum decoded TIFF chunk allocation in MiB.
    #[arg(long, default_value_t = 256)]
    max_chunk_mb: usize,

    /// JPEG quality. WebP is currently emitted losslessly by the image crate.
    #[arg(long, default_value_t = 85)]
    quality: u8,

    /// Pixel backend used for rendering.
    #[arg(long, value_enum, default_value_t = Backend::Auto)]
    backend: Backend,

    /// Directory for persistent encoded IIIF region/tile responses.
    #[arg(long, default_value = "cache/server")]
    cache_dir: PathBuf,

    /// Maximum persistent response cache size in MiB. Use 0 to disable the response cache.
    #[arg(long, default_value_t = 4096)]
    cache_max_mb: u64,

    /// Minimum interval between cache pruning passes.
    #[arg(long, default_value_t = 60)]
    cache_prune_interval_sec: u64,

    /// Maximum number of concurrent TIFF render jobs.
    #[arg(long, default_value_t = 4)]
    max_concurrent_renders: usize,
}

#[derive(Clone)]
struct AppState {
    root: Arc<PathBuf>,
    tile_size: u32,
    max_output_pixels: u32,
    max_chunk_mb: usize,
    quality: u8,
    backend: Backend,
    cache_dir: Arc<PathBuf>,
    cache_max_bytes: u64,
    cache_prune_interval: Duration,
    last_cache_prune: Arc<Mutex<Instant>>,
    last_cache_prune_report: Arc<Mutex<CachePruneReport>>,
    render_permits: Arc<Semaphore>,
    info_cache: Arc<Mutex<HashMap<PathBuf, Arc<ImageInfo>>>>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct CachePruneReport {
    last_started_unix: Option<u64>,
    last_finished_unix: Option<u64>,
    removed_files: u64,
    removed_bytes: u64,
}

#[derive(Serialize)]
struct ImageListItem {
    id: String,
    label: String,
    info_url: String,
    viewer_url: String,
}

#[derive(Clone)]
enum ImageFormat {
    Png,
    Jpeg,
    Webp,
}

impl ImageFormat {
    fn extension(&self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
        }
    }
}

#[derive(Clone)]
struct IiifImageRequest {
    id: String,
    region: String,
    size: String,
    rotation: String,
    quality: String,
    format: ImageFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IiifRotation {
    mirror: bool,
    degrees: u16,
}

#[derive(Serialize)]
struct CacheStats {
    enabled: bool,
    cache_dir: String,
    max_bytes: u64,
    current_bytes: u64,
    file_count: usize,
    prune_interval_sec: u64,
    last_prune: CachePruneReport,
}

pub async fn run_from_cli() -> Result<()> {
    let cli = ServerCli::parse();
    let root = cli
        .root
        .canonicalize()
        .with_context(|| format!("opening image root {}", cli.root.display()))?;

    if cli.tile_size == 0 {
        bail!("--tile-size must be greater than zero");
    }
    if cli.max_output_pixels == 0 {
        bail!("--max-output-pixels must be greater than zero");
    }
    if cli.max_concurrent_renders == 0 {
        bail!("--max-concurrent-renders must be greater than zero");
    }
    let cache_dir = if cli.cache_max_mb > 0 {
        fs::create_dir_all(&cli.cache_dir)
            .with_context(|| format!("creating cache dir {}", cli.cache_dir.display()))?;
        cli.cache_dir
            .canonicalize()
            .with_context(|| format!("opening cache dir {}", cli.cache_dir.display()))?
    } else {
        cli.cache_dir
    };

    let state = Arc::new(AppState {
        root: Arc::new(root),
        tile_size: cli.tile_size,
        max_output_pixels: cli.max_output_pixels,
        max_chunk_mb: cli.max_chunk_mb,
        quality: cli.quality,
        backend: cli.backend,
        cache_dir: Arc::new(cache_dir),
        cache_max_bytes: cli.cache_max_mb.saturating_mul(1024 * 1024),
        cache_prune_interval: Duration::from_secs(cli.cache_prune_interval_sec),
        last_cache_prune: Arc::new(Mutex::new(stale_cache_prune_instant())),
        last_cache_prune_report: Arc::new(Mutex::new(CachePruneReport::default())),
        render_permits: Arc::new(Semaphore::new(cli.max_concurrent_renders)),
        info_cache: Arc::new(Mutex::new(HashMap::new())),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/images", get(list_images))
        .route("/api/cache", get(cache_stats).delete(purge_cache))
        .route("/viewer/{*id}", get(viewer))
        .route("/iiif/3/{*tail}", get(iiif))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = TcpListener::bind(cli.addr).await?;
    println!("GigaTIFF Server listening on http://{}", cli.addr);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<String> {
    Html(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>GigaTIFF Server</title>
  <style>
    :root { color-scheme: light dark; }
    body { font: 15px system-ui, sans-serif; margin: 0; color: #17202a; background: #f6f8fb; }
    main { max-width: 1080px; margin: 0 auto; padding: 32px 20px; }
    header { display: flex; justify-content: space-between; align-items: baseline; gap: 16px; margin-bottom: 24px; }
    h1 { font-size: 28px; margin: 0; }
    h2 { font-size: 17px; margin: 0 0 12px; }
    section { background: white; border: 1px solid #dce3ec; border-radius: 8px; padding: 16px; margin-bottom: 16px; }
    table { width: 100%; border-collapse: collapse; }
    th, td { padding: 10px 8px; border-bottom: 1px solid #edf1f6; text-align: left; }
    th { color: #52616f; font-size: 13px; }
    a { color: #087f74; text-decoration: none; }
    a:hover { text-decoration: underline; }
    .muted { color: #667788; }
    .toolbar { display: flex; gap: 8px; align-items: center; }
    button { border: 1px solid #cbd5e1; background: #fff; padding: 7px 10px; border-radius: 6px; cursor: pointer; }
    button:hover { background: #f1f5f9; }
    dl { display: grid; grid-template-columns: repeat(auto-fit, minmax(180px, 1fr)); gap: 10px 18px; margin: 0; }
    dt { color: #52616f; font-size: 13px; }
    dd { margin: 3px 0 0; font-weight: 650; }
    @media (prefers-color-scheme: dark) {
      body { color: #e5edf6; background: #101419; }
      section, button { background: #171d24; border-color: #2b3642; }
      th, td { border-bottom-color: #26313d; }
      th, .muted, dt { color: #9caebb; }
      button:hover { background: #202a34; }
    }
  </style>
</head>
<body>
  <main>
    <header>
      <div>
        <h1>GigaTIFF Server</h1>
        <div class="muted">IIIF image service for large TIFF and BigTIFF files</div>
      </div>
      <div class="toolbar">
        <button type="button" id="refresh">Refresh</button>
        <button type="button" id="purge">Purge Cache</button>
      </div>
    </header>

    <section>
      <h2>Cache</h2>
      <dl id="cache"></dl>
    </section>

    <section>
      <h2>Images</h2>
      <table>
        <thead>
          <tr><th>Name</th><th>Viewer</th><th>IIIF Info</th></tr>
        </thead>
        <tbody id="images"></tbody>
      </table>
    </section>
  </main>
  <script>
    const cacheEl = document.getElementById("cache");
    const imagesEl = document.getElementById("images");
    const fmtBytes = (value) => {
      if (!value) return "0 B";
      const units = ["B", "KiB", "MiB", "GiB"];
      let size = value;
      let unit = 0;
      while (size >= 1024 && unit < units.length - 1) { size /= 1024; unit++; }
      return `${size.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
    };
    const fmtTime = (value) => value ? new Date(value * 1000).toLocaleString() : "n/a";
    async function refresh() {
      const [cache, images] = await Promise.all([
        fetch("/api/cache").then((r) => r.json()),
        fetch("/api/images").then((r) => r.json())
      ]);
      cacheEl.innerHTML = `
        <div><dt>Status</dt><dd>${cache.enabled ? "enabled" : "disabled"}</dd></div>
        <div><dt>Size</dt><dd>${fmtBytes(cache.current_bytes)} / ${fmtBytes(cache.max_bytes)}</dd></div>
        <div><dt>Files</dt><dd>${cache.file_count}</dd></div>
        <div><dt>Last Prune</dt><dd>${fmtTime(cache.last_prune.last_finished_unix)}</dd></div>
        <div><dt>Removed Last Prune</dt><dd>${cache.last_prune.removed_files} files, ${fmtBytes(cache.last_prune.removed_bytes)}</dd></div>
      `;
      imagesEl.innerHTML = images.map((image) => `
        <tr>
          <td>${image.label}</td>
          <td><a href="${image.viewer_url}">Open</a></td>
          <td><a href="${image.info_url}">info.json</a></td>
        </tr>
      `).join("");
    }
    document.getElementById("refresh").addEventListener("click", refresh);
    document.getElementById("purge").addEventListener("click", async () => {
      await fetch("/api/cache", { method: "DELETE" });
      await refresh();
    });
    refresh().catch((error) => {
      cacheEl.innerHTML = `<div><dt>Error</dt><dd>${error}</dd></div>`;
    });
  </script>
</body>
</html>"#
            .to_string(),
    )
}

async fn list_images(State(state): State<Arc<AppState>>) -> Response {
    let root = Arc::clone(&state.root);
    let result = tokio::task::spawn_blocking(move || collect_images(&root))
        .await
        .map_err(|err| anyhow!("image scan task failed: {err}"))
        .and_then(|result| result);

    match result {
        Ok(images) => Json(images).into_response(),
        Err(err) => error_response(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn cache_stats(State(state): State<Arc<AppState>>) -> Response {
    let result = tokio::task::spawn_blocking(move || build_cache_stats(&state))
        .await
        .map_err(|err| anyhow!("cache stats task failed: {err}"))
        .and_then(|result| result);

    match result {
        Ok(stats) => Json(stats).into_response(),
        Err(err) => error_response(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn purge_cache(State(state): State<Arc<AppState>>) -> Response {
    let result = tokio::task::spawn_blocking(move || purge_response_cache(&state))
        .await
        .map_err(|err| anyhow!("cache purge task failed: {err}"))
        .and_then(|result| result);

    match result {
        Ok(stats) => Json(stats).into_response(),
        Err(err) => error_response(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn viewer(AxumPath(id): AxumPath<String>) -> Response {
    let encoded_id = encode_id(&id);
    let info_url = format!("/iiif/3/{encoded_id}/info.json");
    Html(format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>GigaTIFF Viewer</title>
  <script src="https://cdn.jsdelivr.net/npm/openseadragon@5.0.1/build/openseadragon/openseadragon.min.js"></script>
  <style>
    html, body, #viewer {{ width: 100%; height: 100%; margin: 0; background: #101418; }}
  </style>
</head>
<body>
  <div id="viewer"></div>
  <script>
    OpenSeadragon({{
      id: "viewer",
      prefixUrl: "https://cdn.jsdelivr.net/npm/openseadragon@5.0.1/build/openseadragon/images/",
      tileSources: "{info_url}",
      showNavigator: true,
      visibilityRatio: 1,
      constrainDuringPan: true
    }});
  </script>
</body>
</html>"#
    ))
    .into_response()
}

async fn iiif(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(tail): AxumPath<String>,
) -> Response {
    if let Some(id) = tail.strip_suffix("/info.json") {
        return iiif_info(state, headers, id.to_string()).await;
    }

    match parse_iiif_image_request(&tail) {
        Ok(request) => iiif_image(state, headers, request).await,
        Err(err) => match resolve_id(&state.root, &tail) {
            Ok(_) => iiif_base_redirect(&tail),
            Err(_) => error_response(StatusCode::BAD_REQUEST, err),
        },
    }
}

fn iiif_base_redirect(id: &str) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    if let Ok(location) = HeaderValue::from_str(&format!("/iiif/3/{id}/info.json")) {
        response.headers_mut().insert(LOCATION, location);
    }
    response
}

async fn iiif_info(state: Arc<AppState>, headers: HeaderMap, id: String) -> Response {
    let image_path = match resolve_id(&state.root, &id) {
        Ok(path) => path,
        Err(err) => return error_response(StatusCode::BAD_REQUEST, err),
    };

    let info = match load_cached_info(&state, &image_path).await {
        Ok(info) => info,
        Err(err) => return error_response(StatusCode::NOT_FOUND, err),
    };

    let service_id = format!("{}/iiif/3/{}", request_origin(&headers), encode_id(&id));
    let body = json!({
        "@context": "http://iiif.io/api/image/3/context.json",
        "id": service_id,
        "type": "ImageService3",
        "protocol": "http://iiif.io/api/image",
        "profile": "level2",
        "width": info.width,
        "height": info.height,
        "maxArea": state.max_output_pixels,
        "preferredFormats": ["webp", "png"],
        "extraFormats": ["webp", "png", "jpg"],
        "extraFeatures": [
            "baseUriRedirect",
            "canonicalLinkHeader",
            "cors",
            "jsonldMediaType",
            "mirroring",
            "profileLinkHeader",
            "rotationBy90s",
            "sizeUpscaling"
        ],
        "extraQualities": ["color", "gray", "bitonal"],
        "sizes": preferred_sizes(info.width, info.height, state.max_output_pixels),
        "tiles": [{
            "width": state.tile_size,
            "height": state.tile_size,
            "scaleFactors": scale_factors(info.width, info.height)
        }]
    });

    let mut response = Json(body).into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(
            "application/ld+json;profile=\"http://iiif.io/api/image/3/context.json\"",
        ),
    );
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=60"),
    );
    insert_profile_link_header(response.headers_mut());
    response
}

async fn iiif_image(
    state: Arc<AppState>,
    headers: HeaderMap,
    request: IiifImageRequest,
) -> Response {
    let image_path = match resolve_id(&state.root, &request.id) {
        Ok(path) => path,
        Err(err) => return error_response(StatusCode::BAD_REQUEST, err),
    };

    let permit = match Arc::clone(&state.render_permits).acquire_owned().await {
        Ok(permit) => permit,
        Err(err) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, anyhow!(err)),
    };

    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        render_iiif_image(&state, image_path, request)
    })
    .await
    .map_err(|err| anyhow!("render task failed: {err}"))
    .and_then(|result| result);

    match result {
        Ok(rendered) => {
            let mut response = rendered.bytes.into_response();
            response.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static(rendered.content_type),
            );
            response.headers_mut().insert(
                "x-gigatiff-cache",
                HeaderValue::from_static(rendered.cache_status),
            );
            insert_ms_header(
                response.headers_mut(),
                "x-gigatiff-total-ms",
                rendered.timing.total,
            );
            insert_ms_header(
                response.headers_mut(),
                "x-gigatiff-cache-read-ms",
                rendered.timing.cache_read,
            );
            insert_ms_header(
                response.headers_mut(),
                "x-gigatiff-render-ms",
                rendered.timing.render,
            );
            insert_ms_header(
                response.headers_mut(),
                "x-gigatiff-encode-ms",
                rendered.timing.encode,
            );
            insert_ms_header(
                response.headers_mut(),
                "x-gigatiff-cache-store-ms",
                rendered.timing.cache_store,
            );
            insert_ms_header(
                response.headers_mut(),
                "x-gigatiff-cache-prune-ms",
                rendered.timing.cache_prune,
            );
            response.headers_mut().insert(
                CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=86400"),
            );
            insert_image_link_header(
                response.headers_mut(),
                &format!("{}{}", request_origin(&headers), rendered.canonical_path),
            );
            response
        }
        Err(err) => error_response(StatusCode::BAD_REQUEST, err),
    }
}

struct RenderedResponse {
    bytes: Vec<u8>,
    content_type: &'static str,
    cache_status: &'static str,
    canonical_path: String,
    timing: ResponseTiming,
}

#[derive(Debug, Clone, Copy, Default)]
struct ResponseTiming {
    total: Duration,
    cache_read: Duration,
    render: Duration,
    encode: Duration,
    cache_store: Duration,
    cache_prune: Duration,
}

fn render_iiif_image(
    state: &AppState,
    image_path: PathBuf,
    request: IiifImageRequest,
) -> Result<RenderedResponse> {
    let total_start = Instant::now();
    let mut timing = ResponseTiming::default();
    let info = load_cached_info_blocking(state, &image_path)?;
    let rect = parse_region(&request.region, &info)?;
    let (out_width, out_height) = parse_size(&request.size, rect, state.max_output_pixels)?;
    let rotation = parse_rotation(&request.rotation)?;
    let canonical_path =
        canonical_image_path(&request, &rect, &info, out_width, out_height, rotation);

    if !matches!(
        request.quality.as_str(),
        "default" | "color" | "gray" | "bitonal"
    ) {
        bail!("unsupported IIIF quality '{}'", request.quality);
    }
    if out_width as u64 * out_height as u64 > state.max_output_pixels as u64 {
        bail!(
            "requested output {} x {} exceeds --max-output-pixels",
            out_width,
            out_height
        );
    }

    let cache_path = if state.cache_max_bytes > 0 {
        let path = response_cache_path(
            &state.cache_dir,
            &image_path,
            &info,
            &canonical_path,
            out_width,
            out_height,
            state,
        )?;
        let cache_read_start = Instant::now();
        if let Ok(bytes) = fs::read(&path) {
            timing.cache_read = cache_read_start.elapsed();
            timing.total = total_start.elapsed();
            return Ok(RenderedResponse {
                bytes,
                content_type: content_type(&request.format),
                cache_status: "hit",
                canonical_path,
                timing,
            });
        }
        timing.cache_read = cache_read_start.elapsed();
        Some(path)
    } else {
        None
    };

    let render_start = Instant::now();
    let preview = render_preview(
        &image_path,
        &info,
        rect,
        out_width.max(out_height),
        state.max_chunk_mb,
        state.backend,
        None,
        None,
    )?;
    timing.render = render_start.elapsed();
    let mut rgba = if preview.width == out_width && preview.height == out_height {
        preview.rgba
    } else {
        resize_nearest_rgba(
            &preview.rgba,
            preview.width,
            preview.height,
            out_width,
            out_height,
        )
    };
    let (final_width, final_height) = apply_geometry(&mut rgba, out_width, out_height, rotation);
    apply_quality(&mut rgba, &request.quality);

    let encode_start = Instant::now();
    let (bytes, content_type) = encode_response(
        &request.format,
        final_width,
        final_height,
        &rgba,
        state.quality,
    )?;
    timing.encode = encode_start.elapsed();
    let cache_status = if let Some(cache_path) = cache_path {
        let store_start = Instant::now();
        store_cached_response(&cache_path, &bytes)?;
        timing.cache_store = store_start.elapsed();
        let prune_start = Instant::now();
        prune_response_cache_throttled(state)?;
        timing.cache_prune = prune_start.elapsed();
        "miss"
    } else {
        "disabled"
    };
    timing.total = total_start.elapsed();
    Ok(RenderedResponse {
        bytes,
        content_type,
        cache_status,
        canonical_path,
        timing,
    })
}

fn response_cache_path(
    cache_dir: &Path,
    image_path: &Path,
    info: &ImageInfo,
    canonical_path: &str,
    out_width: u32,
    out_height: u32,
    state: &AppState,
) -> Result<PathBuf> {
    let metadata = fs::metadata(image_path)?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .unwrap_or_default();
    let canonical = image_path
        .canonicalize()
        .unwrap_or_else(|_| image_path.to_path_buf());

    let mut hash = Fnv1a64::new();
    hash.write_bytes(b"gigatiff-server-response-v1");
    hash.write_bytes(canonical.to_string_lossy().as_bytes());
    hash.write_u64(metadata.len());
    hash.write_u64(modified.as_secs());
    hash.write_u64(modified.subsec_nanos() as u64);
    hash.write_u64(info.width as u64);
    hash.write_u64(info.height as u64);
    hash.write_u64(info.chunk_width as u64);
    hash.write_u64(info.chunk_height as u64);
    hash.write_u64(out_width as u64);
    hash.write_u64(out_height as u64);
    hash.write_bytes(canonical_path.as_bytes());
    hash.write_u64(state.quality as u64);
    hash.write_bytes(format!("{:?}", state.backend).as_bytes());

    let extension = canonical_path
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .unwrap_or("cache");
    let filename = format!("{:016x}.{extension}", hash.finish());
    Ok(cache_dir.join(&filename[0..2]).join(filename))
}

fn store_cached_response(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let suffix = UNIX_EPOCH.elapsed().unwrap_or_default().as_nanos();
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("cache");
    let tmp = path.with_extension(format!("{extension}.tmp.{}.{}", std::process::id(), suffix));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path).or_else(|_| {
        fs::copy(&tmp, path)?;
        fs::remove_file(&tmp)?;
        Ok::<(), std::io::Error>(())
    })?;
    Ok(())
}

fn prune_response_cache_throttled(state: &AppState) -> Result<()> {
    if state.cache_max_bytes == 0 {
        return Ok(());
    }

    {
        let mut last_prune = state
            .last_cache_prune
            .lock()
            .map_err(|_| anyhow!("cache prune lock poisoned"))?;
        if last_prune.elapsed() < state.cache_prune_interval {
            return Ok(());
        }
        *last_prune = Instant::now();
    }

    let report = prune_response_cache(&state.cache_dir, state.cache_max_bytes)?;
    *state
        .last_cache_prune_report
        .lock()
        .map_err(|_| anyhow!("cache prune report lock poisoned"))? = report;
    Ok(())
}

fn prune_response_cache(cache_dir: &Path, max_bytes: u64) -> Result<CachePruneReport> {
    let started = current_unix_secs();
    if max_bytes == 0 || !cache_dir.exists() {
        return Ok(CachePruneReport {
            last_started_unix: Some(started),
            last_finished_unix: Some(current_unix_secs()),
            removed_files: 0,
            removed_bytes: 0,
        });
    }

    let mut files = Vec::new();
    let mut total_bytes = 0u64;
    collect_cache_files(cache_dir, &mut files, &mut total_bytes)?;
    if total_bytes <= max_bytes {
        return Ok(CachePruneReport {
            last_started_unix: Some(started),
            last_finished_unix: Some(current_unix_secs()),
            removed_files: 0,
            removed_bytes: 0,
        });
    }

    files.sort_by_key(|file| file.modified);
    let mut removed_files = 0u64;
    let mut removed_bytes = 0u64;
    for file in files {
        if total_bytes <= max_bytes {
            break;
        }
        match fs::remove_file(&file.path) {
            Ok(()) => {
                total_bytes = total_bytes.saturating_sub(file.bytes);
                removed_files += 1;
                removed_bytes = removed_bytes.saturating_add(file.bytes);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                total_bytes = total_bytes.saturating_sub(file.bytes);
            }
            Err(err) => {
                return Err(err).with_context(|| format!("removing {}", file.path.display()));
            }
        }
    }

    remove_empty_cache_dirs(cache_dir, cache_dir)?;
    Ok(CachePruneReport {
        last_started_unix: Some(started),
        last_finished_unix: Some(current_unix_secs()),
        removed_files,
        removed_bytes,
    })
}

fn build_cache_stats(state: &AppState) -> Result<CacheStats> {
    let (file_count, current_bytes) = if state.cache_max_bytes > 0 && state.cache_dir.exists() {
        let mut files = Vec::new();
        let mut total_bytes = 0u64;
        collect_cache_files(&state.cache_dir, &mut files, &mut total_bytes)?;
        (files.len(), total_bytes)
    } else {
        (0, 0)
    };

    let last_prune = state
        .last_cache_prune_report
        .lock()
        .map_err(|_| anyhow!("cache prune report lock poisoned"))?
        .clone();

    Ok(CacheStats {
        enabled: state.cache_max_bytes > 0,
        cache_dir: state.cache_dir.display().to_string(),
        max_bytes: state.cache_max_bytes,
        current_bytes,
        file_count,
        prune_interval_sec: state.cache_prune_interval.as_secs(),
        last_prune,
    })
}

fn purge_response_cache(state: &AppState) -> Result<CacheStats> {
    let started = current_unix_secs();
    let mut removed_files = 0u64;
    let mut removed_bytes = 0u64;

    if state.cache_max_bytes > 0 && state.cache_dir.exists() {
        let mut files = Vec::new();
        let mut total_bytes = 0u64;
        collect_cache_files(&state.cache_dir, &mut files, &mut total_bytes)?;
        for file in files {
            match fs::remove_file(&file.path) {
                Ok(()) => {
                    removed_files += 1;
                    removed_bytes = removed_bytes.saturating_add(file.bytes);
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(err).with_context(|| format!("removing {}", file.path.display()));
                }
            }
        }
        remove_empty_cache_dirs(&state.cache_dir, &state.cache_dir)?;
    }

    {
        let mut report = state
            .last_cache_prune_report
            .lock()
            .map_err(|_| anyhow!("cache prune report lock poisoned"))?;
        *report = CachePruneReport {
            last_started_unix: Some(started),
            last_finished_unix: Some(current_unix_secs()),
            removed_files,
            removed_bytes,
        };
    }

    build_cache_stats(state)
}

struct CachedResponseFile {
    path: PathBuf,
    bytes: u64,
    modified: Duration,
}

fn collect_cache_files(
    dir: &Path,
    files: &mut Vec<CachedResponseFile>,
    total_bytes: &mut u64,
) -> Result<()> {
    for entry in
        fs::read_dir(dir).with_context(|| format!("reading cache dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_cache_files(&path, files, total_bytes)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }

        let bytes = metadata.len();
        *total_bytes = total_bytes.saturating_add(bytes);
        files.push(CachedResponseFile {
            path,
            bytes,
            modified: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .unwrap_or_default(),
        });
    }
    Ok(())
}

fn remove_empty_cache_dirs(root: &Path, dir: &Path) -> Result<bool> {
    let mut is_empty = true;
    for entry in
        fs::read_dir(dir).with_context(|| format!("reading cache dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            if !remove_empty_cache_dirs(root, &path)? {
                is_empty = false;
            }
        } else {
            is_empty = false;
        }
    }

    if is_empty && dir != root {
        fs::remove_dir(dir)
            .with_context(|| format!("removing empty cache dir {}", dir.display()))?;
    }
    Ok(is_empty)
}

fn content_type(format: &ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Webp => "image/webp",
    }
}

fn insert_ms_header(headers: &mut HeaderMap, name: &'static str, duration: Duration) {
    let value = format!("{:.2}", duration.as_secs_f64() * 1000.0);
    if let Ok(value) = HeaderValue::from_str(&value) {
        headers.insert(name, value);
    }
}

fn insert_profile_link_header(headers: &mut HeaderMap) {
    headers.insert(
        LINK,
        HeaderValue::from_static("<http://iiif.io/api/image/3/level2.json>;rel=\"profile\""),
    );
}

fn insert_image_link_header(headers: &mut HeaderMap, canonical_url: &str) {
    let value = format!(
        "<http://iiif.io/api/image/3/level2.json>;rel=\"profile\", <{canonical_url}>;rel=\"canonical\""
    );
    if let Ok(value) = HeaderValue::from_str(&value) {
        headers.insert(LINK, value);
    }
}

fn current_unix_secs() -> u64 {
    UNIX_EPOCH.elapsed().unwrap_or_default().as_secs()
}

fn parse_iiif_image_request(tail: &str) -> Result<IiifImageRequest> {
    let mut parts = tail.rsplitn(5, '/');
    let quality_format = parts
        .next()
        .ok_or_else(|| anyhow!("missing IIIF quality/format"))?;
    let rotation = parts
        .next()
        .ok_or_else(|| anyhow!("missing IIIF rotation"))?
        .to_string();
    let size = parts
        .next()
        .ok_or_else(|| anyhow!("missing IIIF size"))?
        .to_string();
    let region = parts
        .next()
        .ok_or_else(|| anyhow!("missing IIIF region"))?
        .to_string();
    let id = parts
        .next()
        .ok_or_else(|| anyhow!("missing IIIF identifier"))?
        .to_string();

    let (quality, format) = quality_format
        .rsplit_once('.')
        .ok_or_else(|| anyhow!("missing IIIF format extension"))?;

    Ok(IiifImageRequest {
        id,
        region,
        size,
        rotation,
        quality: quality.to_string(),
        format: parse_format(format)?,
    })
}

fn parse_format(format: &str) -> Result<ImageFormat> {
    match format.to_ascii_lowercase().as_str() {
        "png" => Ok(ImageFormat::Png),
        "jpg" | "jpeg" => Ok(ImageFormat::Jpeg),
        "webp" => Ok(ImageFormat::Webp),
        other => bail!("unsupported image format '{other}'"),
    }
}

fn canonical_image_path(
    request: &IiifImageRequest,
    rect: &Rect,
    info: &ImageInfo,
    out_width: u32,
    out_height: u32,
    rotation: IiifRotation,
) -> String {
    format!(
        "/iiif/3/{}/{}/{}/{}/{}.{}",
        encode_id(&request.id),
        canonical_region(rect, info),
        canonical_size(&request.size, out_width, out_height),
        canonical_rotation(rotation),
        request.quality,
        request.format.extension()
    )
}

fn canonical_region(rect: &Rect, info: &ImageInfo) -> String {
    if rect.x == 0 && rect.y == 0 && rect.width == info.width && rect.height == info.height {
        "full".to_string()
    } else {
        format!("{},{},{},{}", rect.x, rect.y, rect.width, rect.height)
    }
}

fn canonical_size(size: &str, out_width: u32, out_height: u32) -> String {
    if size == "max" || size == "full" {
        return "max".to_string();
    }
    if size == "^max" {
        return "^max".to_string();
    }

    let prefix = if size.starts_with('^') { "^" } else { "" };
    format!("{prefix}{out_width},{out_height}")
}

fn canonical_rotation(rotation: IiifRotation) -> String {
    if rotation.mirror {
        format!("!{}", rotation.degrees)
    } else {
        rotation.degrees.to_string()
    }
}

fn parse_rotation(rotation: &str) -> Result<IiifRotation> {
    let (mirror, value) = if let Some(value) = rotation.strip_prefix('!') {
        (true, value)
    } else {
        (false, rotation)
    };
    let degrees: f64 = value
        .parse()
        .with_context(|| format!("invalid IIIF rotation '{rotation}'"))?;
    if !degrees.is_finite() || !(0.0..=360.0).contains(&degrees) {
        bail!("IIIF rotation must be between 0 and 360 degrees");
    }

    let normalized = if (degrees - 360.0).abs() < f64::EPSILON {
        0.0
    } else {
        degrees
    };
    let rounded = normalized.round();
    if (normalized - rounded).abs() > f64::EPSILON || !matches!(rounded as u16, 0 | 90 | 180 | 270)
    {
        bail!("only IIIF rotations 0, 90, 180, and 270 are supported");
    }

    Ok(IiifRotation {
        mirror,
        degrees: rounded as u16,
    })
}

fn stale_cache_prune_instant() -> Instant {
    let now = Instant::now();
    now.checked_sub(Duration::from_secs(3600)).unwrap_or(now)
}

fn parse_region(region: &str, info: &ImageInfo) -> Result<Rect> {
    if region == "full" || region == "max" {
        return Ok(Rect {
            x: 0,
            y: 0,
            width: info.width,
            height: info.height,
        });
    }

    if region == "square" {
        let side = info.width.min(info.height);
        return Ok(Rect {
            x: (info.width - side) / 2,
            y: (info.height - side) / 2,
            width: side,
            height: side,
        });
    }

    if let Some(percent_region) = region.strip_prefix("pct:") {
        let parts = parse_percentage_parts(percent_region, 4, "region")?;
        let x = percent_to_u32(parts[0], info.width, false);
        let y = percent_to_u32(parts[1], info.height, false);
        let width = percent_to_u32(parts[2], info.width, true);
        let height = percent_to_u32(parts[3], info.height, true);
        if width == 0 || height == 0 {
            bail!("pct region width and height must be greater than zero");
        }
        return clamp_rect(x, y, width, height, info.width, info.height);
    }

    let parts: Vec<u32> = region
        .split(',')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("invalid IIIF region '{region}'"))?;
    if parts.len() != 4 || parts[2] == 0 || parts[3] == 0 {
        bail!("region must be full, square, x,y,w,h, or pct:x,y,w,h");
    }
    clamp_rect(
        parts[0],
        parts[1],
        parts[2],
        parts[3],
        info.width,
        info.height,
    )
}

fn parse_size(size: &str, rect: Rect, max_output_pixels: u32) -> Result<(u32, u32)> {
    if size == "max" || size == "full" {
        return fit_to_max_area(rect.width, rect.height, max_output_pixels, false);
    }

    let allow_upscale = size.starts_with('^');
    let size = size.strip_prefix('^').unwrap_or(size);
    if allow_upscale && size == "max" {
        return fit_to_max_area(rect.width, rect.height, max_output_pixels, true);
    }

    let constrained = size.strip_prefix('!').unwrap_or(size);
    if let Some(percent_size) = constrained.strip_prefix("pct:") {
        let scale = parse_percentage(percent_size, "size")? / 100.0;
        return constrain_size(
            scaled_by_float(rect.width, scale),
            scaled_by_float(rect.height, scale),
            rect,
            allow_upscale,
        );
    }

    if let Some(width) = constrained.strip_suffix(',') {
        let width = parse_positive_u32(width, "width")?;
        return constrain_size(width, scaled_height(rect, width), rect, allow_upscale);
    }
    if let Some(height) = constrained.strip_prefix(',') {
        let height = parse_positive_u32(height, "height")?;
        return constrain_size(scaled_width(rect, height), height, rect, allow_upscale);
    }

    if let Some((width, height)) = constrained.split_once(',') {
        let width = parse_positive_u32(width, "width")?;
        let height = parse_positive_u32(height, "height")?;
        if size.starts_with('!') {
            let width_scale = width as f64 / rect.width as f64;
            let height_scale = height as f64 / rect.height as f64;
            let scale = width_scale.min(height_scale);
            let scaled_width = ((rect.width as f64 * scale).floor() as u32).max(1);
            let scaled_height = ((rect.height as f64 * scale).floor() as u32).max(1);
            return constrain_size(scaled_width, scaled_height, rect, allow_upscale);
        }
        return constrain_size(width, height, rect, allow_upscale);
    }

    bail!("unsupported IIIF size '{size}'")
}

fn constrain_size(width: u32, height: u32, rect: Rect, allow_upscale: bool) -> Result<(u32, u32)> {
    if width == 0 || height == 0 {
        bail!("IIIF output width and height must be greater than zero");
    }
    if !allow_upscale && (width > rect.width || height > rect.height) {
        bail!("IIIF size requests that upscale must use the ^ prefix");
    }

    Ok((width, height))
}

fn fit_to_max_area(
    width: u32,
    height: u32,
    max_area: u32,
    allow_upscale: bool,
) -> Result<(u32, u32)> {
    if width == 0 || height == 0 || max_area == 0 {
        bail!("IIIF output width, height, and max area must be greater than zero");
    }

    let area = width as u64 * height as u64;
    if area == max_area as u64 || area < max_area as u64 && !allow_upscale {
        return Ok((width, height));
    }

    let scale = (max_area as f64 / area as f64).sqrt();
    let scaled_width = scaled_by_float(width, scale);
    let scaled_height = scaled_by_float(height, scale);
    constrain_area(scaled_width, scaled_height, max_area)
}

fn constrain_area(mut width: u32, mut height: u32, max_area: u32) -> Result<(u32, u32)> {
    if width == 0 || height == 0 {
        bail!("IIIF output width and height must be greater than zero");
    }

    while width as u64 * height as u64 > max_area as u64 {
        if width >= height {
            width -= 1;
        } else {
            height -= 1;
        }
    }
    Ok((width, height))
}

fn scaled_by_float(value: u32, scale: f64) -> u32 {
    if !scale.is_finite() || scale < 0.0 {
        return 0;
    }
    (value as f64 * scale).floor().clamp(0.0, u32::MAX as f64) as u32
}

fn parse_percentage_parts(value: &str, expected: usize, label: &str) -> Result<Vec<f64>> {
    let parts: Vec<f64> = value
        .split(',')
        .map(|part| parse_percentage(part, label))
        .collect::<Result<_>>()?;
    if parts.len() != expected {
        bail!("pct {label} must contain {expected} comma-separated values");
    }
    Ok(parts)
}

fn parse_percentage(value: &str, label: &str) -> Result<f64> {
    let parsed: f64 = value
        .parse()
        .with_context(|| format!("invalid pct {label} value '{value}'"))?;
    if !parsed.is_finite() || parsed < 0.0 {
        bail!("pct {label} values must be finite and non-negative");
    }
    Ok(parsed)
}

fn percent_to_u32(percent: f64, full: u32, ceil: bool) -> u32 {
    let value = percent * full as f64 / 100.0;
    if ceil { value.ceil() } else { value.floor() }.clamp(0.0, u32::MAX as f64) as u32
}

fn parse_positive_u32(value: &str, label: &str) -> Result<u32> {
    let parsed: u32 = value
        .parse()
        .with_context(|| format!("invalid IIIF {label} '{value}'"))?;
    if parsed == 0 {
        bail!("IIIF {label} must be greater than zero");
    }
    Ok(parsed)
}

fn scaled_width(rect: Rect, height: u32) -> u32 {
    ((rect.width as u64 * height as u64) / rect.height as u64)
        .max(1)
        .min(u32::MAX as u64) as u32
}

fn scaled_height(rect: Rect, width: u32) -> u32 {
    ((rect.height as u64 * width as u64) / rect.width as u64)
        .max(1)
        .min(u32::MAX as u64) as u32
}

fn encode_response(
    format: &ImageFormat,
    width: u32,
    height: u32,
    rgba: &[u8],
    quality: u8,
) -> Result<(Vec<u8>, &'static str)> {
    match format {
        ImageFormat::Png => {
            let mut bytes = Vec::new();
            let mut encoder = png::Encoder::new(Cursor::new(&mut bytes), width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(PngCompression::Fast.to_png());
            let mut writer = encoder.write_header()?;
            writer.write_image_data(rgba)?;
            drop(writer);
            Ok((bytes, content_type(format)))
        }
        ImageFormat::Jpeg => {
            let rgb = rgba_to_rgb(rgba);
            let mut bytes = Vec::new();
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality);
            encoder.write_image(&rgb, width, height, ExtendedColorType::Rgb8)?;
            Ok((bytes, content_type(format)))
        }
        ImageFormat::Webp => {
            let mut bytes = Vec::new();
            let encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut bytes);
            encoder.write_image(rgba, width, height, ExtendedColorType::Rgba8)?;
            Ok((bytes, content_type(format)))
        }
    }
}

struct Fnv1a64 {
    hash: u64,
}

impl Fnv1a64 {
    fn new() -> Self {
        Self {
            hash: 0xcbf29ce484222325,
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.hash ^= *byte as u64;
            self.hash = self.hash.wrapping_mul(0x100000001b3);
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.write_bytes(&value.to_le_bytes());
    }

    fn finish(self) -> u64 {
        self.hash
    }
}

fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for pixel in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&pixel[0..3]);
    }
    rgb
}

fn apply_geometry(
    rgba: &mut Vec<u8>,
    width: u32,
    height: u32,
    rotation: IiifRotation,
) -> (u32, u32) {
    if rotation.mirror {
        mirror_horizontal_rgba(rgba, width, height);
    }

    match rotation.degrees {
        0 => (width, height),
        90 => {
            *rgba = rotate_90_rgba(rgba, width, height);
            (height, width)
        }
        180 => {
            *rgba = rotate_180_rgba(rgba, width, height);
            (width, height)
        }
        270 => {
            *rgba = rotate_270_rgba(rgba, width, height);
            (height, width)
        }
        _ => unreachable!("parse_rotation only allows right-angle rotations"),
    }
}

fn mirror_horizontal_rgba(rgba: &mut [u8], width: u32, height: u32) {
    let row_len = width as usize * 4;
    for y in 0..height as usize {
        let row = &mut rgba[y * row_len..(y + 1) * row_len];
        for x in 0..width as usize / 2 {
            let left = x * 4;
            let right = (width as usize - 1 - x) * 4;
            for channel in 0..4 {
                row.swap(left + channel, right + channel);
            }
        }
    }
}

fn rotate_90_rgba(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut rotated = vec![255u8; rgba.len()];
    let dst_width = height as usize;
    for y in 0..height as usize {
        for x in 0..width as usize {
            let src = (y * width as usize + x) * 4;
            let dst_x = height as usize - 1 - y;
            let dst_y = x;
            let dst = (dst_y * dst_width + dst_x) * 4;
            rotated[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
        }
    }
    rotated
}

fn rotate_180_rgba(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut rotated = vec![255u8; rgba.len()];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let src = (y * width as usize + x) * 4;
            let dst_x = width as usize - 1 - x;
            let dst_y = height as usize - 1 - y;
            let dst = (dst_y * width as usize + dst_x) * 4;
            rotated[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
        }
    }
    rotated
}

fn rotate_270_rgba(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut rotated = vec![255u8; rgba.len()];
    let dst_width = height as usize;
    for y in 0..height as usize {
        for x in 0..width as usize {
            let src = (y * width as usize + x) * 4;
            let dst_x = y;
            let dst_y = width as usize - 1 - x;
            let dst = (dst_y * dst_width + dst_x) * 4;
            rotated[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
        }
    }
    rotated
}

fn apply_quality(rgba: &mut [u8], quality: &str) {
    match quality {
        "default" | "color" => {}
        "gray" => {
            for pixel in rgba.chunks_exact_mut(4) {
                let gray = luminance(pixel);
                pixel[0] = gray;
                pixel[1] = gray;
                pixel[2] = gray;
            }
        }
        "bitonal" => {
            for pixel in rgba.chunks_exact_mut(4) {
                let value = if luminance(pixel) >= 128 { 255 } else { 0 };
                pixel[0] = value;
                pixel[1] = value;
                pixel[2] = value;
            }
        }
        _ => unreachable!("render_iiif_image validates quality"),
    }
}

fn luminance(pixel: &[u8]) -> u8 {
    ((u16::from(pixel[0]) * 30 + u16::from(pixel[1]) * 59 + u16::from(pixel[2]) * 11) / 100) as u8
}

fn resize_nearest_rgba(
    rgba: &[u8],
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
) -> Vec<u8> {
    let mut resized = vec![255u8; dst_width as usize * dst_height as usize * 4];
    for y in 0..dst_height {
        let src_y = (y as u64 * src_height as u64 / dst_height as u64) as usize;
        for x in 0..dst_width {
            let src_x = (x as u64 * src_width as u64 / dst_width as u64) as usize;
            let src = (src_y * src_width as usize + src_x) * 4;
            let dst = (y as usize * dst_width as usize + x as usize) * 4;
            resized[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
        }
    }
    resized
}

async fn load_cached_info(state: &AppState, path: &Path) -> Result<Arc<ImageInfo>> {
    let state = state.clone();
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || load_cached_info_blocking(&state, &path))
        .await
        .map_err(|err| anyhow!("info task failed: {err}"))?
}

fn load_cached_info_blocking(state: &AppState, path: &Path) -> Result<Arc<ImageInfo>> {
    if let Some(info) = state
        .info_cache
        .lock()
        .map_err(|_| anyhow!("info cache lock poisoned"))?
        .get(path)
        .cloned()
    {
        return Ok(info);
    }

    let info = Arc::new(load_info(path)?);
    state
        .info_cache
        .lock()
        .map_err(|_| anyhow!("info cache lock poisoned"))?
        .insert(path.to_path_buf(), Arc::clone(&info));
    Ok(info)
}

fn collect_images(root: &Path) -> Result<Vec<ImageListItem>> {
    let mut images = Vec::new();
    collect_images_inner(root, root, &mut images)?;
    images.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(images)
}

fn collect_images_inner(root: &Path, dir: &Path, images: &mut Vec<ImageListItem>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_images_inner(root, &path, images)?;
            continue;
        }
        if !is_tiff_path(&path) {
            continue;
        }
        let id = relative_id(root, &path)?;
        let encoded_id = encode_id(&id);
        images.push(ImageListItem {
            label: id.clone(),
            id,
            info_url: format!("/iiif/3/{encoded_id}/info.json"),
            viewer_url: format!("/viewer/{encoded_id}"),
        });
    }
    Ok(())
}

fn resolve_id(root: &Path, id: &str) -> Result<PathBuf> {
    let decoded = percent_decode_str(id)
        .decode_utf8()
        .with_context(|| format!("decoding identifier '{id}'"))?;
    let mut relative = PathBuf::new();
    for segment in decoded.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." || segment.contains('\\') {
            bail!("invalid image identifier '{id}'");
        }
        relative.push(segment);
    }

    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            bail!("invalid image identifier '{id}'");
        }
    }

    let path = root.join(relative);
    if !is_tiff_path(&path) {
        bail!("identifier does not point to a TIFF file");
    }
    if !path.exists() {
        bail!("image not found");
    }
    Ok(path)
}

fn relative_id(root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(root)?;
    let id = relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => Ok(value.to_string_lossy().into_owned()),
            _ => Err(anyhow!("invalid path component in {}", path.display())),
        })
        .collect::<Result<Vec<_>>>()?
        .join("/");
    Ok(id)
}

fn encode_id(id: &str) -> String {
    utf8_percent_encode(id, ID_ENCODE_SET).to_string()
}

fn request_origin(headers: &HeaderMap) -> String {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http");
    let host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    format!("{scheme}://{host}")
}

fn scale_factors(width: u32, height: u32) -> Vec<u32> {
    let mut factors = Vec::new();
    let mut factor = 1u32;
    let max_dim = width.max(height).max(1);
    while factor <= max_dim {
        factors.push(factor);
        if factor > u32::MAX / 2 {
            break;
        }
        factor *= 2;
    }
    factors
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct IiifSize {
    width: u32,
    height: u32,
}

fn preferred_sizes(width: u32, height: u32, max_area: u32) -> Vec<IiifSize> {
    let mut factors = Vec::new();
    let mut factor = 1u32;
    let max_factor = width.min(height).max(1);
    while factor <= max_factor {
        factors.push(factor);
        if factor > u32::MAX / 2 {
            break;
        }
        factor *= 2;
    }

    let mut sizes = Vec::new();
    for factor in factors.into_iter().rev() {
        let size = IiifSize {
            width: (width / factor).max(1),
            height: (height / factor).max(1),
        };
        if size.width as u64 * size.height as u64 <= max_area as u64 && sizes.last() != Some(&size)
        {
            sizes.push(size);
        }
    }
    sizes
}

fn is_tiff_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| matches!(ext.to_ascii_lowercase().as_str(), "tif" | "tiff"))
        .unwrap_or(false)
}

fn error_response(status: StatusCode, err: anyhow::Error) -> Response {
    (status, err.to_string()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iiif_image_request_with_slash_identifier() {
        let request =
            parse_iiif_image_request("folder/map.tif/0,0,512,512/256,/0/default.webp").unwrap();
        assert_eq!(request.id, "folder/map.tif");
        assert_eq!(request.region, "0,0,512,512");
        assert_eq!(request.size, "256,");
        assert_eq!(request.rotation, "0");
        assert_eq!(request.quality, "default");
    }

    #[test]
    fn parses_right_angle_rotation_and_mirroring() {
        assert_eq!(
            parse_rotation("90").unwrap(),
            IiifRotation {
                mirror: false,
                degrees: 90
            }
        );
        assert_eq!(
            parse_rotation("!270").unwrap(),
            IiifRotation {
                mirror: true,
                degrees: 270
            }
        );
        assert_eq!(parse_rotation("360").unwrap().degrees, 0);
        assert!(parse_rotation("45").is_err());
        assert!(parse_rotation("361").is_err());
    }

    #[test]
    fn applies_mirroring_before_rotation() {
        let red = [255, 0, 0, 255];
        let green = [0, 255, 0, 255];
        let blue = [0, 0, 255, 255];
        let white = [255, 255, 255, 255];
        let mut rgba = [red, green, blue, white].concat();

        let (width, height) = apply_geometry(
            &mut rgba,
            2,
            2,
            IiifRotation {
                mirror: true,
                degrees: 90,
            },
        );

        assert_eq!((width, height), (2, 2));
        assert_eq!(rgba, [white, green, blue, red].concat());
    }

    #[test]
    fn applies_gray_and_bitonal_quality() {
        let mut gray = vec![10, 20, 30, 255, 200, 200, 200, 128];
        apply_quality(&mut gray, "gray");
        assert_eq!(gray, vec![18, 18, 18, 255, 200, 200, 200, 128]);

        let mut bitonal = vec![10, 20, 30, 255, 200, 200, 200, 128];
        apply_quality(&mut bitonal, "bitonal");
        assert_eq!(bitonal, vec![0, 0, 0, 255, 255, 255, 255, 128]);
    }

    #[test]
    fn iiif_base_redirect_points_to_info_json() {
        let response = iiif_base_redirect("folder/map.tif");

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(LOCATION).unwrap(),
            "/iiif/3/folder/map.tif/info.json"
        );
    }

    #[test]
    fn canonical_image_path_normalizes_region_size_rotation_and_format() {
        let info = dummy_info();
        let request = IiifImageRequest {
            id: "folder/map.tif".to_string(),
            region: "pct:25,25,50,50".to_string(),
            size: "1024,".to_string(),
            rotation: "!90".to_string(),
            quality: "default".to_string(),
            format: ImageFormat::Jpeg,
        };
        let rect = parse_region(&request.region, &info).unwrap();
        let (out_width, out_height) = parse_size(&request.size, rect, 16_777_216).unwrap();
        let rotation = parse_rotation(&request.rotation).unwrap();

        assert_eq!(
            canonical_image_path(&request, &rect, &info, out_width, out_height, rotation),
            "/iiif/3/folder%2Fmap.tif/1024,512,2048,1024/1024,512/!90/default.jpg"
        );
    }

    #[test]
    fn inserts_iiif_link_headers() {
        let mut headers = HeaderMap::new();
        insert_profile_link_header(&mut headers);
        assert_eq!(
            headers.get(LINK).unwrap(),
            "<http://iiif.io/api/image/3/level2.json>;rel=\"profile\""
        );

        insert_image_link_header(
            &mut headers,
            "http://example.test/iiif/3/map.tif/full/max/0/default.jpg",
        );
        assert_eq!(
            headers.get(LINK).unwrap(),
            "<http://iiif.io/api/image/3/level2.json>;rel=\"profile\", <http://example.test/iiif/3/map.tif/full/max/0/default.jpg>;rel=\"canonical\""
        );
    }

    #[test]
    fn preferred_sizes_are_full_image_variants_within_area_limits() {
        assert_eq!(
            preferred_sizes(4096, 2048, 16_777_216),
            vec![
                IiifSize {
                    width: 2,
                    height: 1
                },
                IiifSize {
                    width: 4,
                    height: 2
                },
                IiifSize {
                    width: 8,
                    height: 4
                },
                IiifSize {
                    width: 16,
                    height: 8
                },
                IiifSize {
                    width: 32,
                    height: 16
                },
                IiifSize {
                    width: 64,
                    height: 32
                },
                IiifSize {
                    width: 128,
                    height: 64
                },
                IiifSize {
                    width: 256,
                    height: 128
                },
                IiifSize {
                    width: 512,
                    height: 256
                },
                IiifSize {
                    width: 1024,
                    height: 512
                },
                IiifSize {
                    width: 2048,
                    height: 1024
                },
                IiifSize {
                    width: 4096,
                    height: 2048
                },
            ]
        );

        assert_eq!(
            preferred_sizes(4096, 2048, 1_048_576)
                .last()
                .map(|size| (size.width, size.height)),
            Some((1024, 512))
        );
    }

    #[test]
    fn parses_bounding_box_size() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 4000,
            height: 2000,
        };
        assert_eq!(
            parse_size("!512,512", rect, 16_777_216).unwrap(),
            (512, 256)
        );
    }

    #[test]
    fn parses_square_and_percentage_regions() {
        let info = dummy_info();

        assert_eq!(
            parse_region("square", &info).unwrap(),
            Rect {
                x: 1024,
                y: 0,
                width: 2048,
                height: 2048,
            }
        );
        assert_eq!(
            parse_region("pct:25,25,50,50", &info).unwrap(),
            Rect {
                x: 1024,
                y: 512,
                width: 2048,
                height: 1024,
            }
        );
        assert_eq!(
            parse_region("pct:0,0,100,100", &info).unwrap(),
            Rect {
                x: 0,
                y: 0,
                width: 4096,
                height: 2048,
            }
        );
    }

    #[test]
    fn clips_regions_at_image_edges_and_rejects_empty_regions() {
        let info = dummy_info();

        assert_eq!(
            parse_region("4000,2000,500,500", &info).unwrap(),
            Rect {
                x: 4000,
                y: 2000,
                width: 96,
                height: 48,
            }
        );
        assert!(parse_region("4096,0,1,1", &info).is_err());
        assert!(parse_region("0,2048,1,1", &info).is_err());
        assert!(parse_region("0,0,0,1", &info).is_err());
        assert!(parse_region("pct:100,0,10,10", &info).is_err());
        assert!(parse_region("pct:0,0,0,10", &info).is_err());
    }

    #[test]
    fn parses_iiif_size_forms() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 4000,
            height: 2000,
        };

        assert_eq!(parse_size("max", rect, 8_000_000).unwrap(), (4000, 2000));
        assert_eq!(parse_size("full", rect, 8_000_000).unwrap(), (4000, 2000));
        assert_eq!(parse_size("max", rect, 2_000_000).unwrap(), (2000, 1000));
        assert_eq!(
            parse_size("pct:50", rect, 16_777_216).unwrap(),
            (2000, 1000)
        );
        assert_eq!(parse_size("2000,", rect, 16_777_216).unwrap(), (2000, 1000));
        assert_eq!(parse_size(",1000", rect, 16_777_216).unwrap(), (2000, 1000));
        assert_eq!(
            parse_size("2000,1000", rect, 16_777_216).unwrap(),
            (2000, 1000)
        );
        assert_eq!(
            parse_size("1000,1000", rect, 16_777_216).unwrap(),
            (1000, 1000)
        );
        assert_eq!(
            parse_size("!512,512", rect, 16_777_216).unwrap(),
            (512, 256)
        );
    }

    #[test]
    fn parses_upscale_size_forms_with_caret() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 4000,
            height: 2000,
        };
        let small = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 500,
        };

        assert_eq!(parse_size("^max", small, 2_000_000).unwrap(), (2000, 1000));
        assert_eq!(
            parse_size("^pct:200", rect, 64_000_000).unwrap(),
            (8000, 4000)
        );
        assert_eq!(
            parse_size("^8000,", rect, 64_000_000).unwrap(),
            (8000, 4000)
        );
        assert_eq!(
            parse_size("^,4000", rect, 64_000_000).unwrap(),
            (8000, 4000)
        );
        assert_eq!(
            parse_size("^8000,4000", rect, 64_000_000).unwrap(),
            (8000, 4000)
        );
        assert_eq!(
            parse_size("^!8000,8000", rect, 64_000_000).unwrap(),
            (8000, 4000)
        );
    }

    #[test]
    fn rejects_size_upscaling_without_caret() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 4000,
            height: 2000,
        };

        assert!(parse_size("pct:200", rect, 64_000_000).is_err());
        assert!(parse_size("8000,", rect, 64_000_000).is_err());
        assert!(parse_size(",4000", rect, 64_000_000).is_err());
        assert!(parse_size("8000,4000", rect, 64_000_000).is_err());
        assert!(parse_size("!8000,8000", rect, 64_000_000).is_err());
        assert!(parse_size("pct:0", rect, 64_000_000).is_err());
    }

    #[test]
    fn rejects_traversal_identifier() {
        assert!(resolve_id(Path::new("."), "../map.tif").is_err());
        assert!(resolve_id(Path::new("."), "nested/../map.tif").is_err());
    }

    #[test]
    fn response_cache_path_changes_with_request() {
        let temp = unique_temp_dir("cache-key");
        fs::create_dir_all(&temp).unwrap();
        let image_path = temp.join("map.tif");
        fs::write(&image_path, b"dummy").unwrap();
        let state = dummy_state(temp.join("cache"));
        let info = dummy_info();

        let full = IiifImageRequest {
            id: "map.tif".to_string(),
            region: "full".to_string(),
            size: "max".to_string(),
            rotation: "0".to_string(),
            quality: "default".to_string(),
            format: ImageFormat::Webp,
        };
        let pixel_equivalent = IiifImageRequest {
            region: "0,0,4096,2048".to_string(),
            size: "full".to_string(),
            ..full.clone()
        };
        let full_rect = parse_region(&full.region, &info).unwrap();
        let (full_width, full_height) = parse_size(&full.size, full_rect, 16_777_216).unwrap();
        let full_canonical = canonical_image_path(
            &full,
            &full_rect,
            &info,
            full_width,
            full_height,
            parse_rotation(&full.rotation).unwrap(),
        );
        let equivalent_rect = parse_region(&pixel_equivalent.region, &info).unwrap();
        let (equivalent_width, equivalent_height) =
            parse_size(&pixel_equivalent.size, equivalent_rect, 16_777_216).unwrap();
        let equivalent_canonical = canonical_image_path(
            &pixel_equivalent,
            &equivalent_rect,
            &info,
            equivalent_width,
            equivalent_height,
            parse_rotation(&pixel_equivalent.rotation).unwrap(),
        );
        assert_eq!(full_canonical, equivalent_canonical);

        let full_path = response_cache_path(
            &state.cache_dir,
            &image_path,
            &info,
            &full_canonical,
            full_width,
            full_height,
            &state,
        )
        .unwrap();
        let equivalent_path = response_cache_path(
            &state.cache_dir,
            &image_path,
            &info,
            &equivalent_canonical,
            equivalent_width,
            equivalent_height,
            &state,
        )
        .unwrap();
        assert_eq!(full_path, equivalent_path);

        let changed = IiifImageRequest {
            size: "1024,".to_string(),
            ..full
        };
        let changed_rect = parse_region(&changed.region, &info).unwrap();
        let (changed_width, changed_height) =
            parse_size(&changed.size, changed_rect, 16_777_216).unwrap();
        let changed_canonical = canonical_image_path(
            &changed,
            &changed_rect,
            &info,
            changed_width,
            changed_height,
            parse_rotation(&changed.rotation).unwrap(),
        );
        let changed_path = response_cache_path(
            &state.cache_dir,
            &image_path,
            &info,
            &changed_canonical,
            changed_width,
            changed_height,
            &state,
        )
        .unwrap();
        assert_ne!(full_path, changed_path);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn store_cached_response_round_trips_bytes() {
        let temp = unique_temp_dir("cache-store");
        let cache_path = temp.join("ab").join("abcdef.webp");
        store_cached_response(&cache_path, b"cached bytes").unwrap();
        assert_eq!(fs::read(&cache_path).unwrap(), b"cached bytes");
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn prune_response_cache_removes_old_files_until_under_limit() {
        let temp = unique_temp_dir("cache-prune");
        store_cached_response(&temp.join("aa").join("one.webp"), b"aaaa").unwrap();
        store_cached_response(&temp.join("bb").join("two.webp"), b"bbbb").unwrap();

        let report = prune_response_cache(&temp, 4).unwrap();

        let mut files = Vec::new();
        let mut total_bytes = 0;
        collect_cache_files(&temp, &mut files, &mut total_bytes).unwrap();
        assert!(total_bytes <= 4);
        assert_eq!(files.len(), 1);
        assert_eq!(report.removed_files, 1);
        assert_eq!(report.removed_bytes, 4);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn cache_stats_and_purge_report_size_and_removals() {
        let temp = unique_temp_dir("cache-stats");
        let state = dummy_state(temp.clone());
        store_cached_response(&temp.join("aa").join("one.webp"), b"aaaa").unwrap();
        store_cached_response(&temp.join("bb").join("two.webp"), b"bbbbbb").unwrap();

        let stats = build_cache_stats(&state).unwrap();
        assert_eq!(stats.file_count, 2);
        assert_eq!(stats.current_bytes, 10);

        let purged = purge_response_cache(&state).unwrap();
        assert_eq!(purged.file_count, 0);
        assert_eq!(purged.current_bytes, 0);
        assert_eq!(purged.last_prune.removed_files, 2);
        assert_eq!(purged.last_prune.removed_bytes, 10);
        let _ = fs::remove_dir_all(temp);
    }

    fn dummy_state(cache_dir: PathBuf) -> AppState {
        AppState {
            root: Arc::new(PathBuf::from(".")),
            tile_size: 512,
            max_output_pixels: 1024 * 1024,
            max_chunk_mb: 256,
            quality: 85,
            backend: Backend::Auto,
            cache_dir: Arc::new(cache_dir),
            cache_max_bytes: 4096 * 1024 * 1024,
            cache_prune_interval: Duration::from_secs(60),
            last_cache_prune: Arc::new(Mutex::new(stale_cache_prune_instant())),
            last_cache_prune_report: Arc::new(Mutex::new(CachePruneReport::default())),
            render_permits: Arc::new(Semaphore::new(4)),
            info_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn dummy_info() -> ImageInfo {
        ImageInfo {
            width: 4096,
            height: 2048,
            color_type: tiff::ColorType::RGB(8),
            chunk_type: tiff::decoder::ChunkType::Strip,
            chunk_width: 4096,
            chunk_height: 128,
            chunk_count: 16,
            chunks_across: 1,
            compression: Some(1),
            bits_per_sample: Some(vec![8, 8, 8]),
            samples_per_pixel: Some(3),
            planar_config: Some(1),
            photometric: Some(2),
            is_bigtiff: false,
            little_endian: true,
            rows_per_strip: Some(128),
            strip_offsets: Some(vec![0]),
            icc_profile: None,
        }
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "gigatiff-{label}-{}-{}",
            std::process::id(),
            UNIX_EPOCH.elapsed().unwrap_or_default().as_nanos()
        ))
    }
}
