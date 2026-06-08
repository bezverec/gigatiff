use std::collections::HashMap;
#[cfg(feature = "jpeg2000-grok")]
use std::ffi::OsString;
use std::fs;
use std::io::Cursor;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
#[cfg(feature = "jpeg2000-grok")]
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::Request;
use axum::extract::{Path as AxumPath, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, HOST, LINK, LOCATION};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::{Parser, ValueEnum};
use image::ExtendedColorType;
use image::ImageEncoder;
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use redis::Commands;
use serde::Serialize;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use gigatiff_core::options::{Backend, PngCompression};
#[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
use gigatiff_core::render::{PreviewBitmap, RenderStats};
use gigatiff_core::render::{Rect, clamp_rect, render_preview};
use gigatiff_core::tiff_info::{ImageInfo, load_info};

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

const MAX_IIIF_TAIL_LEN: usize = 4096;
const MAX_IIIF_IDENTIFIER_LEN: usize = 2048;
const MAX_IIIF_IDENTIFIER_SEGMENTS: usize = 64;
const MAX_IIIF_IDENTIFIER_SEGMENT_LEN: usize = 255;
const MAX_IIIF_TOKEN_LEN: usize = 128;
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "GigaTIFF Server: an IIIF-compatible TIFF/BigTIFF/JPEG2000 image server"
)]
struct ServerCli {
    /// Directory containing TIFF/BigTIFF/JPEG2000 files exposed by the server.
    #[arg(long, env = "GIGATIFF_ROOT", default_value = ".")]
    root: PathBuf,

    /// HTTP bind address.
    #[arg(long, env = "GIGATIFF_ADDR", default_value = "127.0.0.1:8080")]
    addr: SocketAddr,

    /// Preferred IIIF tile size advertised in info.json.
    #[arg(long, env = "GIGATIFF_TILE_SIZE", default_value_t = 512)]
    tile_size: u32,

    /// Maximum encoded response area in pixels.
    #[arg(long, env = "GIGATIFF_MAX_OUTPUT_PIXELS", default_value_t = 16_777_216)]
    max_output_pixels: u32,

    /// Maximum decoded TIFF chunk allocation in MiB.
    #[arg(long, env = "GIGATIFF_MAX_CHUNK_MB", default_value_t = 256)]
    max_chunk_mb: usize,

    /// JPEG quality. WebP is currently emitted losslessly by the image crate.
    #[arg(long, env = "GIGATIFF_QUALITY", default_value_t = 85)]
    quality: u8,

    /// Pixel backend used for rendering.
    #[arg(long, env = "GIGATIFF_BACKEND", value_enum, default_value_t = Backend::Auto)]
    backend: Backend,

    /// JPEG2000 backend policy.
    #[arg(long, env = "GIGATIFF_JP2_BACKEND", value_enum, default_value_t = Jp2BackendPolicy::Auto)]
    jp2_backend: Jp2BackendPolicy,

    /// Worker threads used inside each OpenJPEG FFI decode. Use 1 for single-threaded decoding.
    #[arg(long, env = "GIGATIFF_OPENJPEG_THREADS", default_value_t = 1)]
    openjpeg_threads: usize,

    /// Directory for persistent encoded IIIF region/tile responses.
    #[arg(long, env = "GIGATIFF_CACHE_DIR", default_value = "cache/server")]
    cache_dir: PathBuf,

    /// Persistent response-cache backend.
    #[arg(
        long,
        env = "GIGATIFF_CACHE_BACKEND",
        value_enum,
        default_value_t = ResponseCacheBackend::Disk
    )]
    cache_backend: ResponseCacheBackend,

    /// Dragonfly/Redis-compatible URL used when --cache-backend dragonfly is selected.
    #[arg(
        long,
        env = "GIGATIFF_DRAGONFLY_URL",
        default_value = "redis://127.0.0.1:6379/"
    )]
    dragonfly_url: String,

    /// Cache key namespace used for shared cache backends.
    #[arg(
        long,
        env = "GIGATIFF_CACHE_NAMESPACE",
        default_value = "gigatiff-server-response-v10"
    )]
    cache_namespace: String,

    /// Maximum persistent response cache size in MiB. Use 0 to disable the response cache.
    #[arg(long, env = "GIGATIFF_CACHE_MAX_MB", default_value_t = 4096)]
    cache_max_mb: u64,

    /// Minimum interval between cache pruning passes.
    #[arg(long, env = "GIGATIFF_CACHE_PRUNE_INTERVAL_SEC", default_value_t = 60)]
    cache_prune_interval_sec: u64,

    /// Maximum age for persistent response-cache entries in seconds. Use 0 to disable TTL expiry.
    #[arg(long, env = "GIGATIFF_CACHE_TTL_SEC", default_value_t = 0)]
    cache_ttl_sec: u64,

    /// Maximum number of concurrent render jobs.
    #[arg(long, env = "GIGATIFF_MAX_CONCURRENT_RENDERS", default_value_t = 4)]
    max_concurrent_renders: usize,

    /// Maximum number of concurrent render jobs from one client key. Use 0 to disable.
    #[arg(
        long,
        env = "GIGATIFF_MAX_CONCURRENT_RENDERS_PER_IP",
        default_value_t = 2
    )]
    max_concurrent_renders_per_ip: usize,

    /// Maximum number of concurrent render jobs for one source file. Use 0 to disable.
    #[arg(
        long,
        env = "GIGATIFF_MAX_CONCURRENT_RENDERS_PER_FILE",
        default_value_t = 2
    )]
    max_concurrent_renders_per_file: usize,

    /// Maximum render/decode/encode time per IIIF response in seconds.
    #[arg(long, env = "GIGATIFF_RENDER_TIMEOUT_SEC", default_value_t = 120)]
    render_timeout_sec: u64,

    /// Maximum allowed IIIF upscale multiplier when the ^ size prefix is used.
    #[arg(long, env = "GIGATIFF_MAX_UPSCALE", default_value_t = 4.0)]
    max_upscale: f64,

    /// Maximum HTTP requests per client key per minute. Use 0 to disable.
    #[arg(long, env = "GIGATIFF_RATE_LIMIT_PER_MINUTE", default_value_t = 600)]
    rate_limit_per_minute: u32,

    /// Reject startup if configured writable cache paths are inside the image root.
    #[arg(long, env = "GIGATIFF_ENFORCE_READ_ONLY_ROOT", default_value_t = false)]
    enforce_read_only_root: bool,
}

#[derive(Clone)]
struct AppState {
    root: Arc<PathBuf>,
    tile_size: u32,
    max_output_pixels: u32,
    max_chunk_mb: usize,
    quality: u8,
    backend: Backend,
    jp2_backend: Jp2BackendPolicy,
    #[cfg_attr(not(feature = "jpeg2000-openjpeg-ffi"), allow(dead_code))]
    openjpeg_threads: usize,
    cache_dir: Arc<PathBuf>,
    cache_backend: ResponseCacheBackend,
    dragonfly_cache: Option<Arc<DragonflyCache>>,
    cache_namespace: Arc<String>,
    cache_max_bytes: u64,
    cache_prune_interval: Duration,
    cache_ttl: Option<Duration>,
    last_cache_prune: Arc<Mutex<Instant>>,
    last_cache_prune_report: Arc<Mutex<CachePruneReport>>,
    render_permits: Arc<Semaphore>,
    max_concurrent_renders_per_ip: usize,
    max_concurrent_renders_per_file: usize,
    ip_render_permits: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    file_render_permits: Arc<Mutex<HashMap<PathBuf, Arc<Semaphore>>>>,
    render_timeout: Duration,
    max_upscale: f64,
    rate_limit_per_minute: u32,
    rate_limits: Arc<Mutex<HashMap<String, RateLimitBucket>>>,
    info_cache: Arc<Mutex<HashMap<PathBuf, Arc<ServerImageInfo>>>>,
    metrics: Arc<AppMetrics>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ResponseCacheBackend {
    Disk,
    Dragonfly,
}

impl ResponseCacheBackend {
    fn label(self) -> &'static str {
        match self {
            Self::Disk => "disk",
            Self::Dragonfly => "dragonfly",
        }
    }
}

struct DragonflyCache {
    client: redis::Client,
    url_display: String,
}

#[derive(Default)]
struct AppMetrics {
    next_request_id: AtomicU64,
    http_requests_total: AtomicU64,
    http_responses_total: AtomicU64,
    http_errors_total: AtomicU64,
    cache_hits_total: AtomicU64,
    cache_misses_total: AtomicU64,
    cache_disabled_total: AtomicU64,
    render_jobs_total: AtomicU64,
    render_jobs_failed_total: AtomicU64,
    render_timeouts_total: AtomicU64,
    rate_limited_requests_total: AtomicU64,
    render_queue_wait_ms_total: AtomicU64,
    render_queue_wait_count: AtomicU64,
    render_active: AtomicU64,
    render_ms_total: AtomicU64,
    render_decode_ms_total: AtomicU64,
    render_encode_ms_total: AtomicU64,
    render_cache_read_ms_total: AtomicU64,
    render_cache_store_ms_total: AtomicU64,
    render_cache_prune_ms_total: AtomicU64,
    jp2_grok_cli_decode_ms_total: AtomicU64,
    jp2_grok_ffi_decode_ms_total: AtomicU64,
    jp2_openjpeg_ffi_decode_ms_total: AtomicU64,
    jp2_grok_to_openjpeg_fallbacks_total: AtomicU64,
}

#[derive(Clone, Copy)]
struct RateLimitBucket {
    window_start: Instant,
    count: u32,
}

struct RenderGuards {
    _global: OwnedSemaphorePermit,
    _per_ip: Option<OwnedSemaphorePermit>,
    _per_file: Option<OwnedSemaphorePermit>,
}

#[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Jp2BackendPolicy {
    Auto,
    Grok,
    Openjpeg,
}

#[cfg(not(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi")))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Jp2BackendPolicy {
    Auto,
}

impl Default for Jp2BackendPolicy {
    fn default() -> Self {
        Self::Auto
    }
}

impl Jp2BackendPolicy {
    #[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
    fn cache_label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            #[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
            Self::Grok => "grok",
            #[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
            Self::Openjpeg => "openjpeg",
        }
    }
}

#[derive(Debug, Clone)]
struct ServerImageInfo {
    width: u32,
    height: u32,
    source: ServerImageSource,
}

impl ServerImageInfo {
    fn from_tiff(info: ImageInfo) -> Self {
        Self {
            width: info.width,
            height: info.height,
            source: ServerImageSource::Tiff(info),
        }
    }

    fn source_label(&self) -> &'static str {
        match &self.source {
            ServerImageSource::Tiff(_) => "tiff",
            #[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
            ServerImageSource::Jpeg2000(_) => "jpeg2000",
        }
    }
}

#[derive(Debug, Clone)]
enum ServerImageSource {
    Tiff(ImageInfo),
    #[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
    Jpeg2000(Jpeg2000Info),
}

#[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
#[derive(Debug, Clone, Default)]
struct Jpeg2000Info {
    components: Option<u32>,
    precision: Option<u32>,
    tile_width: Option<u32>,
    tile_height: Option<u32>,
    progression_order: Option<String>,
    resolution_levels: Option<u32>,
    icc_profile_len: Option<u32>,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Jp2RenderBackend {
    GrokCli,
    GrokFfi,
    OpenJpegFfi,
}

impl Jp2RenderBackend {
    fn label(self) -> &'static str {
        match self {
            Self::GrokCli => "grok-cli",
            Self::GrokFfi => "grok-ffi",
            Self::OpenJpegFfi => "openjpeg-ffi",
        }
    }
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
    modified_unix: Option<u64>,
    info_url: String,
    metadata_url: String,
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
    backend: &'static str,
    cache_dir: String,
    namespace: String,
    max_bytes: u64,
    ttl_sec: Option<u64>,
    current_bytes: u64,
    file_count: usize,
    prune_interval_sec: u64,
    last_prune: CachePruneReport,
}

#[derive(Serialize)]
struct CacheWarmReport {
    id: String,
    attempted: usize,
    rendered: usize,
    failed: usize,
    requests: Vec<CacheWarmRequestReport>,
}

#[derive(Serialize)]
struct CacheWarmRequestReport {
    canonical_path: Option<String>,
    cache_status: Option<&'static str>,
    error: Option<String>,
}

#[derive(Serialize)]
struct MetadataCheck {
    name: &'static str,
    status: &'static str,
    message: String,
}

#[derive(Serialize)]
struct ProbeResponse {
    status: &'static str,
    version: &'static str,
    checks: Vec<ProbeCheck>,
}

#[derive(Serialize)]
struct ProbeCheck {
    name: &'static str,
    status: &'static str,
    message: String,
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
    if cli.render_timeout_sec == 0 {
        bail!("--render-timeout-sec must be greater than zero");
    }
    if !cli.max_upscale.is_finite() || cli.max_upscale < 1.0 {
        bail!("--max-upscale must be finite and at least 1.0");
    }
    let cache_dir = if cli.cache_backend == ResponseCacheBackend::Disk && cli.cache_max_mb > 0 {
        fs::create_dir_all(&cli.cache_dir)
            .with_context(|| format!("creating cache dir {}", cli.cache_dir.display()))?;
        cli.cache_dir
            .canonicalize()
            .with_context(|| format!("opening cache dir {}", cli.cache_dir.display()))?
    } else {
        cli.cache_dir
    };
    if cli.enforce_read_only_root {
        validate_read_only_root_mode(
            &root,
            &cache_dir,
            cli.cache_backend == ResponseCacheBackend::Disk && cli.cache_max_mb > 0,
        )?;
    }
    if cli.cache_namespace.trim().is_empty() {
        bail!("--cache-namespace must not be empty");
    }
    let dragonfly_cache = if cli.cache_backend == ResponseCacheBackend::Dragonfly {
        let client = redis::Client::open(cli.dragonfly_url.as_str()).with_context(|| {
            format!(
                "opening Dragonfly cache at {}",
                sanitize_cache_url(&cli.dragonfly_url)
            )
        })?;
        Some(Arc::new(DragonflyCache {
            client,
            url_display: sanitize_cache_url(&cli.dragonfly_url),
        }))
    } else {
        None
    };

    let state = Arc::new(AppState {
        root: Arc::new(root),
        tile_size: cli.tile_size,
        max_output_pixels: cli.max_output_pixels,
        max_chunk_mb: cli.max_chunk_mb,
        quality: cli.quality,
        backend: cli.backend,
        jp2_backend: cli.jp2_backend,
        openjpeg_threads: cli.openjpeg_threads.max(1),
        cache_dir: Arc::new(cache_dir),
        cache_backend: cli.cache_backend,
        dragonfly_cache,
        cache_namespace: Arc::new(cli.cache_namespace),
        cache_max_bytes: cli.cache_max_mb.saturating_mul(1024 * 1024),
        cache_prune_interval: Duration::from_secs(cli.cache_prune_interval_sec),
        cache_ttl: (cli.cache_ttl_sec > 0).then(|| Duration::from_secs(cli.cache_ttl_sec)),
        last_cache_prune: Arc::new(Mutex::new(stale_cache_prune_instant())),
        last_cache_prune_report: Arc::new(Mutex::new(CachePruneReport::default())),
        render_permits: Arc::new(Semaphore::new(cli.max_concurrent_renders)),
        max_concurrent_renders_per_ip: cli.max_concurrent_renders_per_ip,
        max_concurrent_renders_per_file: cli.max_concurrent_renders_per_file,
        ip_render_permits: Arc::new(Mutex::new(HashMap::new())),
        file_render_permits: Arc::new(Mutex::new(HashMap::new())),
        render_timeout: Duration::from_secs(cli.render_timeout_sec),
        max_upscale: cli.max_upscale,
        rate_limit_per_minute: cli.rate_limit_per_minute,
        rate_limits: Arc::new(Mutex::new(HashMap::new())),
        info_cache: Arc::new(Mutex::new(HashMap::new())),
        metrics: Arc::new(AppMetrics::default()),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/api/images", get(list_images))
        .route("/api/info/{*id}", get(api_info))
        .route("/api/cache/warm/{*id}", post(prewarm_cache))
        .route(
            "/api/cache/{*id}",
            axum::routing::delete(purge_cache_identifier),
        )
        .route("/api/cache", get(cache_stats).delete(purge_cache))
        .route("/viewer/{*id}", get(viewer))
        .route("/iiif/3/{*tail}", get(iiif))
        .layer(from_fn_with_state(
            Arc::clone(&state),
            observability_middleware,
        ))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = TcpListener::bind(cli.addr).await?;
    println!("GigaTIFF Server listening on http://{}", cli.addr);
    axum::serve(listener, app).await?;
    Ok(())
}

fn validate_read_only_root_mode(root: &Path, cache_dir: &Path, cache_enabled: bool) -> Result<()> {
    if cache_enabled && cache_dir.starts_with(root) {
        bail!(
            "--enforce-read-only-root requires --cache-dir to be outside --root; cache dir {} is inside {}",
            cache_dir.display(),
            root.display()
        );
    }
    Ok(())
}

fn sanitize_cache_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let Some((authority, suffix)) = rest.split_once('/') else {
        return url.to_string();
    };
    let sanitized_authority = authority
        .rsplit_once('@')
        .map(|(_, host)| format!("***@{host}"))
        .unwrap_or_else(|| authority.to_string());
    format!("{scheme}://{sanitized_authority}/{suffix}")
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
    :root {
      color-scheme: light;
      --ink: #111;
      --paper: #f4f4f0;
      --panel: #fff;
      --accent: #c7ff2e;
      --muted: #5f625c;
    }
    * { box-sizing: border-box; }
    body {
      font: 14px/1.4 Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      margin: 0;
      color: var(--ink);
      background:
        linear-gradient(90deg, rgba(17,17,17,.05) 1px, transparent 1px),
        linear-gradient(rgba(17,17,17,.05) 1px, transparent 1px),
        var(--paper);
      background-size: 24px 24px;
    }
    main { max-width: 1180px; margin: 0 auto; padding: 28px 18px 40px; }
    header {
      display: flex;
      justify-content: space-between;
      align-items: flex-end;
      gap: 16px;
      margin-bottom: 18px;
      border: 2px solid var(--ink);
      background: var(--panel);
      box-shadow: 6px 6px 0 var(--ink);
      padding: 18px;
    }
    h1 {
      font-size: clamp(34px, 6vw, 72px);
      line-height: .88;
      margin: 0;
      letter-spacing: 0;
      text-transform: uppercase;
    }
    h2 {
      display: inline-block;
      font-size: 13px;
      letter-spacing: .08em;
      margin: 0 0 14px;
      padding: 5px 8px;
      text-transform: uppercase;
      background: var(--accent);
      border: 2px solid var(--ink);
    }
    section {
      background: var(--panel);
      border: 2px solid var(--ink);
      box-shadow: 6px 6px 0 var(--ink);
      padding: 16px;
      margin-bottom: 18px;
    }
    table { width: 100%; border-collapse: collapse; border: 2px solid var(--ink); background: var(--panel); }
    th, td { padding: 10px 8px; border: 2px solid var(--ink); text-align: left; vertical-align: middle; }
    th { background: var(--ink); color: #fff; font-size: 12px; text-transform: uppercase; letter-spacing: .06em; }
    tr:nth-child(even) td { background: #f7f7f4; }
    a { color: var(--ink); font-weight: 750; text-decoration: underline; text-decoration-thickness: 2px; text-underline-offset: 3px; }
    a:hover { background: var(--accent); text-decoration: none; }
    .muted { color: var(--muted); margin-top: 8px; font-weight: 650; text-transform: uppercase; letter-spacing: .04em; }
    .toolbar { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; justify-content: flex-end; }
    .list-tools {
      display: grid;
      grid-template-columns: minmax(220px, 1fr) auto;
      gap: 10px;
      align-items: end;
      margin-bottom: 14px;
    }
    .field label, .sort label {
      display: block;
      color: var(--muted);
      font-size: 12px;
      font-weight: 850;
      letter-spacing: .06em;
      margin-bottom: 5px;
      text-transform: uppercase;
    }
    input[type="search"] {
      width: 100%;
      border: 2px solid var(--ink);
      border-radius: 0;
      background: #fff;
      color: var(--ink);
      font: inherit;
      font-weight: 750;
      padding: 9px 10px;
      outline: none;
      box-shadow: 3px 3px 0 var(--ink);
    }
    input[type="search"]:focus { background: var(--accent); }
    .sort-buttons { display: flex; flex-wrap: wrap; gap: 8px; justify-content: flex-end; }
    button {
      border: 2px solid var(--ink);
      background: #fff;
      color: var(--ink);
      padding: 8px 11px;
      border-radius: 0;
      cursor: pointer;
      font: inherit;
      font-weight: 800;
      text-transform: uppercase;
      box-shadow: 3px 3px 0 var(--ink);
    }
    button:hover { background: var(--accent); transform: translate(-1px, -1px); box-shadow: 4px 4px 0 var(--ink); }
    button.active { background: var(--accent); }
    button:active { transform: translate(2px, 2px); box-shadow: 1px 1px 0 var(--ink); }
    button:disabled { opacity: .45; cursor: wait; transform: none; box-shadow: 3px 3px 0 var(--ink); }
    dl { display: grid; grid-template-columns: repeat(auto-fit, minmax(170px, 1fr)); gap: 0; margin: 0; border: 2px solid var(--ink); }
    dl > div { min-height: 82px; padding: 10px; border: 1px solid var(--ink); background: #fff; }
    dt { color: var(--muted); font-size: 12px; font-weight: 800; text-transform: uppercase; letter-spacing: .06em; }
    dd { margin: 4px 0 0; font-size: 18px; font-weight: 850; word-break: break-word; }
    @media (max-width: 760px) {
      header { align-items: stretch; flex-direction: column; }
      .list-tools { grid-template-columns: 1fr; }
      .sort-buttons { justify-content: flex-start; }
      .toolbar { justify-content: flex-start; }
      section { overflow-x: auto; }
      table { min-width: 720px; }
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
      <div class="list-tools">
        <div class="field">
          <label for="image-search">Search by filename</label>
          <input id="image-search" type="search" autocomplete="off" placeholder="mapa2, 0001, jp2...">
        </div>
        <div class="sort">
          <label>Sort</label>
          <div class="sort-buttons" role="group" aria-label="Image sorting">
            <button type="button" class="active" data-sort="name-asc">Name A-Z</button>
            <button type="button" data-sort="name-desc">Name Z-A</button>
            <button type="button" data-sort="modified-desc">Newest</button>
            <button type="button" data-sort="modified-asc">Oldest</button>
          </div>
        </div>
      </div>
      <table>
        <thead>
          <tr><th>Name</th><th>Modified</th><th>Viewer</th><th>IIIF Info</th><th>Metadata</th><th>Cache</th></tr>
        </thead>
        <tbody id="images"></tbody>
      </table>
    </section>
  </main>
  <script>
    const cacheEl = document.getElementById("cache");
    const imagesEl = document.getElementById("images");
    const searchEl = document.getElementById("image-search");
    const sortButtons = Array.from(document.querySelectorAll("[data-sort]"));
    let allImages = [];
    let sortMode = "name-asc";
    const fmtBytes = (value) => {
      if (!value) return "0 B";
      const units = ["B", "KiB", "MiB", "GiB"];
      let size = value;
      let unit = 0;
      while (size >= 1024 && unit < units.length - 1) { size /= 1024; unit++; }
      return `${size.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
    };
    const fmtTime = (value) => value ? new Date(value * 1000).toLocaleString() : "n/a";
    const fmtTtl = (value) => value ? `${value}s` : "disabled";
    const fmtCacheSize = (cache) => cache.max_bytes
      ? `${fmtBytes(cache.current_bytes)} / ${fmtBytes(cache.max_bytes)}`
      : fmtBytes(cache.current_bytes);
    const escapeHtml = (value) => String(value ?? "").replace(/[&<>"']/g, (char) => ({
      "&": "&amp;",
      "<": "&lt;",
      ">": "&gt;",
      '"': "&quot;",
      "'": "&#39;"
    })[char]);
    const compareName = (a, b) => a.label.localeCompare(b.label, undefined, { numeric: true, sensitivity: "base" });
    const compareModified = (a, b) => (a.modified_unix ?? 0) - (b.modified_unix ?? 0);
    function visibleImages() {
      const query = searchEl.value.trim().toLocaleLowerCase();
      const filtered = query
        ? allImages.filter((image) => image.label.toLocaleLowerCase().includes(query))
        : [...allImages];
      filtered.sort((a, b) => {
        if (sortMode === "name-desc") return compareName(b, a);
        if (sortMode === "modified-desc") return compareModified(b, a) || compareName(a, b);
        if (sortMode === "modified-asc") return compareModified(a, b) || compareName(a, b);
        return compareName(a, b);
      });
      return filtered;
    }
    function renderImages() {
      const images = visibleImages();
      imagesEl.innerHTML = images.map((image) => `
        <tr>
          <td>${escapeHtml(image.label)}</td>
          <td>${escapeHtml(fmtTime(image.modified_unix))}</td>
          <td><a href="${escapeHtml(image.viewer_url)}">Open</a></td>
          <td><a href="${escapeHtml(image.info_url)}">info.json</a></td>
          <td><a href="${escapeHtml(image.metadata_url)}">metadata</a></td>
          <td>
            <button type="button" data-cache-action="warm" data-id="${escapeHtml(image.id)}">Warm</button>
            <button type="button" data-cache-action="purge" data-id="${escapeHtml(image.id)}">Purge</button>
          </td>
        </tr>
      `).join("");
      if (images.length === 0) {
        imagesEl.innerHTML = `<tr><td colspan="6">No matching images</td></tr>`;
      }
    }
    async function refresh() {
      const [cache, images] = await Promise.all([
        fetch("/api/cache").then((r) => r.json()),
        fetch("/api/images").then((r) => r.json())
      ]);
      allImages = images;
      cacheEl.innerHTML = `
        <div><dt>Status</dt><dd>${cache.enabled ? "enabled" : "disabled"}</dd></div>
        <div><dt>Backend</dt><dd>${cache.backend}</dd></div>
        <div><dt>Namespace</dt><dd>${cache.namespace}</dd></div>
        <div><dt>Size</dt><dd>${fmtCacheSize(cache)}</dd></div>
        <div><dt>TTL</dt><dd>${fmtTtl(cache.ttl_sec)}</dd></div>
        <div><dt>Files</dt><dd>${cache.file_count}</dd></div>
        <div><dt>Location</dt><dd>${cache.cache_dir}</dd></div>
        <div><dt>Last Prune</dt><dd>${fmtTime(cache.last_prune.last_finished_unix)}</dd></div>
        <div><dt>Removed Last Prune</dt><dd>${cache.last_prune.removed_files} files, ${fmtBytes(cache.last_prune.removed_bytes)}</dd></div>
      `;
      renderImages();
    }
    document.getElementById("refresh").addEventListener("click", refresh);
    document.getElementById("purge").addEventListener("click", async () => {
      await fetch("/api/cache", { method: "DELETE" });
      await refresh();
    });
    searchEl.addEventListener("input", renderImages);
    sortButtons.forEach((button) => {
      button.addEventListener("click", () => {
        sortMode = button.dataset.sort;
        sortButtons.forEach((item) => item.classList.toggle("active", item === button));
        renderImages();
      });
    });
    imagesEl.addEventListener("click", async (event) => {
      const button = event.target.closest("button[data-cache-action]");
      if (!button) return;
      const id = encodeURIComponent(button.dataset.id).replaceAll("%2F", "/");
      const action = button.dataset.cacheAction;
      button.disabled = true;
      try {
        await fetch(action === "warm" ? `/api/cache/warm/${id}` : `/api/cache/${id}`, {
          method: action === "warm" ? "POST" : "DELETE"
        });
        await refresh();
      } finally {
        button.disabled = false;
      }
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

async fn healthz() -> Response {
    Json(ProbeResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        checks: vec![ProbeCheck {
            name: "process",
            status: "ok",
            message: "server process is accepting HTTP requests".to_string(),
        }],
    })
    .into_response()
}

async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    let result = tokio::task::spawn_blocking(move || build_readiness_probe(&state))
        .await
        .map_err(|err| anyhow!("readiness task failed: {err}"))
        .and_then(|result| result);

    match result {
        Ok(probe) => {
            let status = if probe.status == "ok" {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            (status, Json(probe)).into_response()
        }
        Err(err) => error_response(StatusCode::SERVICE_UNAVAILABLE, err),
    }
}

async fn observability_middleware(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| {
            let id = state
                .metrics
                .next_request_id
                .fetch_add(1, Ordering::Relaxed)
                + 1;
            format!("gigatiff-{id}")
        });
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let route = classify_route(&path);

    if let Ok(value) = HeaderValue::from_str(&request_id) {
        request.headers_mut().insert("x-request-id", value);
    }
    state
        .metrics
        .http_requests_total
        .fetch_add(1, Ordering::Relaxed);

    let mut response =
        if !should_rate_limit_route(route) || check_rate_limit(&state, request.headers()) {
            next.run(request).await
        } else {
            state
                .metrics
                .rate_limited_requests_total
                .fetch_add(1, Ordering::Relaxed);
            error_response(
                StatusCode::TOO_MANY_REQUESTS,
                anyhow!("rate limit exceeded for this client"),
            )
        };
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }

    let elapsed = started.elapsed();
    let status = response.status().as_u16();
    state
        .metrics
        .http_responses_total
        .fetch_add(1, Ordering::Relaxed);
    if status >= 500 {
        state
            .metrics
            .http_errors_total
            .fetch_add(1, Ordering::Relaxed);
    }
    eprintln!(
        "{}",
        json!({
            "event": "http_request",
            "request_id": request_id,
            "method": method,
            "path": path,
            "route": route,
            "status": status,
            "duration_ms": duration_ms_u64(elapsed)
        })
    );

    response
}

fn check_rate_limit(state: &AppState, headers: &HeaderMap) -> bool {
    let limit = state.rate_limit_per_minute;
    if limit == 0 {
        return true;
    }

    let key = client_key(headers);
    let now = Instant::now();
    let mut buckets = state.rate_limits.lock().expect("rate limit mutex poisoned");
    buckets.retain(|_, bucket| {
        now.duration_since(bucket.window_start) < RATE_LIMIT_WINDOW + RATE_LIMIT_WINDOW
    });
    let bucket = buckets.entry(key).or_insert(RateLimitBucket {
        window_start: now,
        count: 0,
    });
    if now.duration_since(bucket.window_start) >= RATE_LIMIT_WINDOW {
        bucket.window_start = now;
        bucket.count = 0;
    }
    if bucket.count >= limit {
        return false;
    }
    bucket.count += 1;
    true
}

fn should_rate_limit_route(route: &str) -> bool {
    !matches!(route, "healthz" | "readyz" | "metrics")
}

fn client_key(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("local")
        .to_string()
}

async fn acquire_render_guards(
    state: &AppState,
    image_path: &Path,
    headers: &HeaderMap,
) -> Result<RenderGuards> {
    let global = Arc::clone(&state.render_permits)
        .acquire_owned()
        .await
        .map_err(|err| anyhow!(err))?;
    let per_ip = if state.max_concurrent_renders_per_ip > 0 {
        let semaphore = keyed_semaphore(
            &state.ip_render_permits,
            client_key(headers),
            state.max_concurrent_renders_per_ip,
        );
        Some(
            semaphore
                .acquire_owned()
                .await
                .map_err(|err| anyhow!(err))?,
        )
    } else {
        None
    };
    let per_file = if state.max_concurrent_renders_per_file > 0 {
        let semaphore = keyed_semaphore(
            &state.file_render_permits,
            image_path.to_path_buf(),
            state.max_concurrent_renders_per_file,
        );
        Some(
            semaphore
                .acquire_owned()
                .await
                .map_err(|err| anyhow!(err))?,
        )
    } else {
        None
    };

    Ok(RenderGuards {
        _global: global,
        _per_ip: per_ip,
        _per_file: per_file,
    })
}

fn keyed_semaphore<K>(
    map: &Mutex<HashMap<K, Arc<Semaphore>>>,
    key: K,
    permits: usize,
) -> Arc<Semaphore>
where
    K: Eq + std::hash::Hash,
{
    let mut map = map.lock().expect("render semaphore mutex poisoned");
    Arc::clone(
        map.entry(key)
            .or_insert_with(|| Arc::new(Semaphore::new(permits))),
    )
}

fn build_readiness_probe(state: &AppState) -> Result<ProbeResponse> {
    let mut checks = Vec::new();
    let mut ready = true;

    if state.root.is_dir() {
        checks.push(ProbeCheck {
            name: "image_root",
            status: "ok",
            message: state.root.display().to_string(),
        });
    } else {
        ready = false;
        checks.push(ProbeCheck {
            name: "image_root",
            status: "error",
            message: format!("{} is not a readable directory", state.root.display()),
        });
    }

    match state.cache_backend {
        ResponseCacheBackend::Disk => {
            if state.cache_max_bytes == 0 {
                checks.push(ProbeCheck {
                    name: "response_cache",
                    status: "disabled",
                    message: "disk response cache is disabled".to_string(),
                });
            } else if state.cache_dir.is_dir() {
                checks.push(ProbeCheck {
                    name: "response_cache",
                    status: "ok",
                    message: state.cache_dir.display().to_string(),
                });
            } else {
                ready = false;
                checks.push(ProbeCheck {
                    name: "response_cache",
                    status: "error",
                    message: format!("{} is not a cache directory", state.cache_dir.display()),
                });
            }
        }
        ResponseCacheBackend::Dragonfly => match dragonfly_ping(state) {
            Ok(()) => checks.push(ProbeCheck {
                name: "response_cache",
                status: "ok",
                message: cache_location(state),
            }),
            Err(err) => {
                ready = false;
                checks.push(ProbeCheck {
                    name: "response_cache",
                    status: "error",
                    message: err.to_string(),
                });
            }
        },
    }

    Ok(ProbeResponse {
        status: if ready { "ok" } else { "degraded" },
        version: env!("CARGO_PKG_VERSION"),
        checks,
    })
}

fn dragonfly_ping(state: &AppState) -> Result<()> {
    let Some(cache) = &state.dragonfly_cache else {
        bail!("Dragonfly cache backend is selected but no client is configured");
    };
    let mut connection = cache
        .client
        .get_connection()
        .with_context(|| "connecting to Dragonfly response cache")?;
    let pong: String = redis::cmd("PING")
        .query(&mut connection)
        .with_context(|| "pinging Dragonfly response cache")?;
    if pong.eq_ignore_ascii_case("PONG") {
        Ok(())
    } else {
        bail!("Dragonfly PING returned {pong}");
    }
}

async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    match build_prometheus_metrics(&state) {
        Ok(body) => {
            let mut response = body.into_response();
            response.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
            );
            response
        }
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

async fn api_info(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let image_path = match resolve_id(&state.root, &id) {
        Ok(path) => path,
        Err(err) => return error_response(StatusCode::BAD_REQUEST, err),
    };

    let info = match load_cached_info(&state, &image_path).await {
        Ok(info) => info,
        Err(err) => return error_response(StatusCode::NOT_FOUND, err),
    };

    let origin = request_origin(&headers);
    let encoded_id = encode_id(&id);
    Json(build_metadata_response(
        &id,
        &encoded_id,
        &origin,
        &image_path,
        &info,
    ))
    .into_response()
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

async fn purge_cache_identifier(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let image_path = match resolve_id(&state.root, &id) {
        Ok(path) => path,
        Err(err) => return error_response(StatusCode::BAD_REQUEST, err),
    };

    let result = tokio::task::spawn_blocking(move || {
        purge_response_cache_for_identifier(&state, &image_path)
    })
    .await
    .map_err(|err| anyhow!("cache identifier purge task failed: {err}"))
    .and_then(|result| result);

    match result {
        Ok(stats) => Json(stats).into_response(),
        Err(err) => error_response(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn prewarm_cache(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let image_path = match resolve_id(&state.root, &id) {
        Ok(path) => path,
        Err(err) => return error_response(StatusCode::BAD_REQUEST, err),
    };

    let queue_started = Instant::now();
    let guards = match acquire_render_guards(&state, &image_path, &headers).await {
        Ok(guards) => guards,
        Err(err) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, anyhow!(err)),
    };
    record_queue_wait(&state.metrics, queue_started.elapsed());
    let render_timeout = state.render_timeout;
    let metrics = Arc::clone(&state.metrics);

    let task = tokio::task::spawn_blocking(move || {
        let _guards = guards;
        state.metrics.render_active.fetch_add(1, Ordering::Relaxed);
        let result = prewarm_cache_blocking(&state, id, image_path);
        state.metrics.render_active.fetch_sub(1, Ordering::Relaxed);
        result
    });
    let result = match tokio::time::timeout(render_timeout, task).await {
        Ok(joined) => joined
            .map_err(|err| anyhow!("cache prewarm task failed: {err}"))
            .and_then(|result| result),
        Err(_) => {
            metrics
                .render_timeouts_total
                .fetch_add(1, Ordering::Relaxed);
            Err(anyhow!(
                "cache prewarm exceeded --render-timeout-sec ({}s)",
                render_timeout.as_secs()
            ))
        }
    };

    match result {
        Ok(report) => Json(report).into_response(),
        Err(err) => {
            let status = if err.to_string().contains("--render-timeout-sec") {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_REQUEST
            };
            error_response(status, err)
        }
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
    :root {{
      --ink: #111;
      --paper: #f4f4f0;
      --accent: #c7ff2e;
    }}
    * {{ box-sizing: border-box; }}
    html, body {{ width: 100%; height: 100%; margin: 0; }}
    body {{
      overflow: hidden;
      color: var(--ink);
      background: #0e1111;
      font: 13px/1.35 Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    }}
    .topbar {{
      position: fixed;
      inset: 0 0 auto 0;
      z-index: 10;
      height: 46px;
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 12px;
      padding: 0 12px;
      background: var(--paper);
      border-bottom: 2px solid var(--ink);
    }}
    .brand {{
      display: flex;
      align-items: baseline;
      gap: 10px;
      min-width: 0;
      font-weight: 900;
      text-transform: uppercase;
      letter-spacing: .04em;
    }}
    .brand span {{
      display: inline-block;
      padding: 4px 7px;
      background: var(--accent);
      border: 2px solid var(--ink);
    }}
    .brand small {{
      color: #565a54;
      font-size: 11px;
      font-weight: 850;
      white-space: nowrap;
    }}
    .actions {{ display: flex; gap: 8px; align-items: center; }}
    a {{
      color: var(--ink);
      border: 2px solid var(--ink);
      background: #fff;
      padding: 6px 9px;
      text-decoration: none;
      font-weight: 850;
      text-transform: uppercase;
      box-shadow: 3px 3px 0 var(--ink);
    }}
    a:hover {{ background: var(--accent); }}
    #viewer {{
      position: absolute;
      inset: 46px 0 0 0;
      background: #0e1111;
    }}
    .navigator {{
      border: 2px solid #f4f4f0 !important;
      background: #111 !important;
    }}
    .openseadragon-canvas:focus {{ outline: 2px solid var(--accent); outline-offset: -2px; }}
    @media (max-width: 700px) {{
      .brand small {{ display: none; }}
      .topbar {{ height: 48px; padding: 0 8px; }}
      #viewer {{ top: 48px; }}
      a {{ padding: 6px 7px; }}
    }}
  </style>
</head>
<body>
  <div class="topbar">
    <div class="brand"><span>GigaTIFF</span><small>IIIF Viewer</small></div>
    <div class="actions">
      <a href="/">Index</a>
      <a href="{info_url}">Info</a>
    </div>
  </div>
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
    let mut body = json!({
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
        "sizes": preferred_sizes(info.width, info.height, state.max_output_pixels)
    });

    if should_advertise_tiles(&info) {
        let (tile_width, tile_height) = advertised_tile_size(state.tile_size, &info);
        body["tiles"] = json!([{
            "width": tile_width,
            "height": tile_height,
            "scaleFactors": scale_factors(info.width, info.height)
        }]);
    }

    let mut response = Json(body).into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(
            "application/ld+json;profile=\"http://iiif.io/api/image/3/context.json\"",
        ),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    insert_jpeg2000_info_headers(response.headers_mut(), &info);
    insert_profile_link_header(response.headers_mut());
    response
}

#[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
fn insert_jpeg2000_info_headers(headers: &mut HeaderMap, info: &ServerImageInfo) {
    let ServerImageSource::Jpeg2000(jpeg2000) = &info.source else {
        return;
    };

    insert_optional_u32_header(headers, "x-gigatiff-jp2-precision", jpeg2000.precision);
    insert_optional_u32_header(headers, "x-gigatiff-jp2-tile-width", jpeg2000.tile_width);
    insert_optional_u32_header(headers, "x-gigatiff-jp2-tile-height", jpeg2000.tile_height);
    headers.insert(
        "x-gigatiff-jp2-tiles-supported",
        HeaderValue::from_static(if jpeg2000_supports_region_tiles(jpeg2000) {
            "true"
        } else {
            "false"
        }),
    );
}

#[cfg(not(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi")))]
fn insert_jpeg2000_info_headers(_headers: &mut HeaderMap, _info: &ServerImageInfo) {}

#[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
fn insert_optional_u32_header(headers: &mut HeaderMap, name: &'static str, value: Option<u32>) {
    if let Some(value) = value {
        if let Ok(value) = HeaderValue::from_str(&value.to_string()) {
            headers.insert(name, value);
        }
    }
}

fn build_metadata_response(
    id: &str,
    encoded_id: &str,
    origin: &str,
    image_path: &Path,
    info: &ServerImageInfo,
) -> serde_json::Value {
    let metadata = fs::metadata(image_path).ok();
    let modified_unix = metadata
        .as_ref()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs());
    let file_size = metadata.as_ref().map(fs::Metadata::len);

    json!({
        "api": "gigatiff-metadata-v1",
        "id": id,
        "source_type": info.source_label(),
        "links": {
            "viewer": format!("{origin}/viewer/{encoded_id}"),
            "iiif_info": format!("{origin}/iiif/3/{encoded_id}/info.json"),
            "metadata": format!("{origin}/api/info/{encoded_id}")
        },
        "file": {
            "name": image_path.file_name().and_then(|name| name.to_str()),
            "size_bytes": file_size,
            "modified_unix": modified_unix
        },
        "dimensions": {
            "width": info.width,
            "height": info.height
        },
        "technical": technical_metadata_json(info),
        "color": color_metadata_json(info),
        "profile_validation": lightweight_profile_validation(info)
    })
}

fn technical_metadata_json(info: &ServerImageInfo) -> serde_json::Value {
    match &info.source {
        ServerImageSource::Tiff(tiff_info) => json!({
            "format": if tiff_info.is_bigtiff { "BigTIFF" } else { "TIFF" },
            "byte_order": if tiff_info.little_endian { "little-endian" } else { "big-endian" },
            "color_type": format!("{:?}", tiff_info.color_type),
            "bits_per_sample": tiff_info.bits_per_sample,
            "samples_per_pixel": tiff_info.samples_per_pixel,
            "compression": {
                "tag": tiff_info.compression,
                "name": tiff_info.compression.and_then(tiff_compression_name)
            },
            "photometric": {
                "tag": tiff_info.photometric,
                "name": tiff_info.photometric.and_then(tiff_photometric_name)
            },
            "planar_configuration": tiff_info.planar_config,
            "layout": {
                "chunk_type": format!("{:?}", tiff_info.chunk_type),
                "chunk_width": tiff_info.chunk_width,
                "chunk_height": tiff_info.chunk_height,
                "chunk_count": tiff_info.chunk_count,
                "chunks_across": tiff_info.chunks_across,
                "rows_per_strip": tiff_info.rows_per_strip
            },
            "resolution": {
                "x": tiff_info.x_resolution,
                "y": tiff_info.y_resolution,
                "unit_tag": tiff_info.resolution_unit,
                "unit": tiff_info.resolution_unit.and_then(tiff_resolution_unit_name)
            }
        }),
        #[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
        ServerImageSource::Jpeg2000(jpeg2000) => json!({
            "format": "JPEG2000",
            "components": jpeg2000.components,
            "precision": jpeg2000.precision,
            "tile": {
                "width": jpeg2000.tile_width,
                "height": jpeg2000.tile_height
            },
            "progression_order": jpeg2000.progression_order,
            "resolution_levels": jpeg2000.resolution_levels,
            "region_tiles_supported": jpeg2000_supports_region_tiles(jpeg2000),
            "openjpeg_fallback_recommended": should_use_openjpeg_fallback(jpeg2000)
        }),
    }
}

fn color_metadata_json(info: &ServerImageInfo) -> serde_json::Value {
    let icc_profile_len = match &info.source {
        ServerImageSource::Tiff(tiff_info) => tiff_info.icc_profile.as_ref().map(|icc| icc.len()),
        #[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
        ServerImageSource::Jpeg2000(jpeg2000) => jpeg2000.icc_profile_len.map(|len| len as usize),
    };
    let color_management = match &info.source {
        ServerImageSource::Tiff(_) => "lcms2-for-embedded-tiff-icc",
        #[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
        ServerImageSource::Jpeg2000(_) => "lcms2-for-embedded-jp2-icc-on-openjpeg-ffi",
    };

    json!({
        "icc": {
            "present": icc_profile_len.is_some_and(|len| len > 0),
            "bytes": icc_profile_len
        },
        "server_output_space": "sRGB",
        "color_management": color_management
    })
}

fn lightweight_profile_validation(info: &ServerImageInfo) -> serde_json::Value {
    let checks = match &info.source {
        ServerImageSource::Tiff(tiff_info) => vec![
            MetadataCheck {
                name: "supported_color_type",
                status: if supported_tiff_color_type_name(&format!("{:?}", tiff_info.color_type)) {
                    "ok"
                } else {
                    "warning"
                },
                message: format!("{:?}", tiff_info.color_type),
            },
            MetadataCheck {
                name: "embedded_icc_profile",
                status: if tiff_info.icc_profile.is_some() {
                    "ok"
                } else {
                    "warning"
                },
                message: tiff_info
                    .icc_profile
                    .as_ref()
                    .map(|icc| format!("{} bytes", icc.len()))
                    .unwrap_or_else(|| "not present".to_string()),
            },
            MetadataCheck {
                name: "resolution_tags",
                status: if tiff_info.x_resolution.is_some()
                    && tiff_info.y_resolution.is_some()
                    && tiff_info.resolution_unit.is_some()
                {
                    "ok"
                } else {
                    "warning"
                },
                message: format!(
                    "x={:?}, y={:?}, unit={:?}",
                    tiff_info.x_resolution, tiff_info.y_resolution, tiff_info.resolution_unit
                ),
            },
        ],
        #[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
        ServerImageSource::Jpeg2000(jpeg2000) => vec![
            MetadataCheck {
                name: "embedded_icc_profile",
                status: if jpeg2000.icc_profile_len.unwrap_or_default() > 0 {
                    "ok"
                } else {
                    "warning"
                },
                message: jpeg2000
                    .icc_profile_len
                    .map(|len| format!("{len} bytes"))
                    .unwrap_or_else(|| "not detected".to_string()),
            },
            MetadataCheck {
                name: "tile_geometry",
                status: if jpeg2000.tile_width.is_some() && jpeg2000.tile_height.is_some() {
                    "ok"
                } else {
                    "warning"
                },
                message: format!("{:?} x {:?}", jpeg2000.tile_width, jpeg2000.tile_height),
            },
            MetadataCheck {
                name: "resolution_levels",
                status: if jpeg2000.resolution_levels.is_some() {
                    "ok"
                } else {
                    "info"
                },
                message: jpeg2000
                    .resolution_levels
                    .map(|levels| levels.to_string())
                    .unwrap_or_else(|| "not detected".to_string()),
            },
            MetadataCheck {
                name: "region_tile_support",
                status: if jpeg2000_supports_region_tiles(jpeg2000) {
                    "ok"
                } else {
                    "warning"
                },
                message: if jpeg2000_supports_region_tiles(jpeg2000) {
                    "supported by configured server backend".to_string()
                } else {
                    "region tile support is limited for this JP2 profile".to_string()
                },
            },
        ],
    };

    let status = if checks.iter().any(|check| check.status == "fail") {
        "fail"
    } else if checks.iter().any(|check| check.status == "warning") {
        "warning"
    } else {
        "ok"
    };

    json!({
        "validator": "gigatiff-lightweight-metadata",
        "status": status,
        "note": "This is a fast metadata-based check, not a replacement for valid2000, JHOVE, or jpylyzer.",
        "checks": checks
    })
}

fn supported_tiff_color_type_name(color_type: &str) -> bool {
    matches!(
        color_type,
        "Gray(8)" | "Gray(16)" | "RGB(8)" | "RGB(16)" | "RGBA(8)" | "RGBA(16)"
    )
}

fn tiff_compression_name(tag: u32) -> Option<&'static str> {
    Some(match tag {
        1 => "none",
        5 => "lzw",
        7 => "jpeg",
        8 => "deflate",
        32773 => "packbits",
        32946 => "deflate-old",
        34712 => "jpeg2000",
        _ => return None,
    })
}

fn tiff_photometric_name(tag: u32) -> Option<&'static str> {
    Some(match tag {
        0 => "white-is-zero",
        1 => "black-is-zero",
        2 => "rgb",
        3 => "palette",
        5 => "cmyk",
        6 => "ycbcr",
        8 => "cielab",
        _ => return None,
    })
}

fn tiff_resolution_unit_name(tag: u32) -> Option<&'static str> {
    Some(match tag {
        1 => "none",
        2 => "inch",
        3 => "centimeter",
        _ => return None,
    })
}

fn classify_route(path: &str) -> &'static str {
    if path == "/" {
        "index"
    } else if path == "/healthz" {
        "healthz"
    } else if path == "/readyz" {
        "readyz"
    } else if path == "/metrics" {
        "metrics"
    } else if path == "/api/images" {
        "api_images"
    } else if path == "/api/cache" {
        "api_cache"
    } else if path.starts_with("/api/cache/warm/") {
        "api_cache_warm"
    } else if path.starts_with("/api/cache/") {
        "api_cache_identifier"
    } else if path.starts_with("/api/info/") {
        "api_info"
    } else if path.starts_with("/viewer/") {
        "viewer"
    } else if path.starts_with("/iiif/3/") {
        "iiif"
    } else {
        "other"
    }
}

fn record_queue_wait(metrics: &AppMetrics, duration: Duration) {
    metrics
        .render_queue_wait_count
        .fetch_add(1, Ordering::Relaxed);
    metrics
        .render_queue_wait_ms_total
        .fetch_add(duration_ms_u64(duration), Ordering::Relaxed);
}

fn record_cache_status(metrics: &AppMetrics, cache_status: &str) {
    match cache_status {
        "hit" => {
            metrics.cache_hits_total.fetch_add(1, Ordering::Relaxed);
        }
        "miss" => {
            metrics.cache_misses_total.fetch_add(1, Ordering::Relaxed);
        }
        "disabled" => {
            metrics.cache_disabled_total.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

fn record_response_timing(metrics: &AppMetrics, timing: ResponseTiming) {
    metrics
        .render_ms_total
        .fetch_add(duration_ms_u64(timing.render), Ordering::Relaxed);
    metrics
        .render_encode_ms_total
        .fetch_add(duration_ms_u64(timing.encode), Ordering::Relaxed);
    metrics
        .render_cache_read_ms_total
        .fetch_add(duration_ms_u64(timing.cache_read), Ordering::Relaxed);
    metrics
        .render_cache_store_ms_total
        .fetch_add(duration_ms_u64(timing.cache_store), Ordering::Relaxed);
    metrics
        .render_cache_prune_ms_total
        .fetch_add(duration_ms_u64(timing.cache_prune), Ordering::Relaxed);
}

fn record_decode_timing(
    metrics: &AppMetrics,
    decode: Duration,
    jp2_backend: Option<Jp2RenderBackend>,
) {
    let decode_ms = duration_ms_u64(decode);
    metrics
        .render_decode_ms_total
        .fetch_add(decode_ms, Ordering::Relaxed);
    match jp2_backend {
        Some(Jp2RenderBackend::GrokCli) => {
            metrics
                .jp2_grok_cli_decode_ms_total
                .fetch_add(decode_ms, Ordering::Relaxed);
        }
        Some(Jp2RenderBackend::GrokFfi) => {
            metrics
                .jp2_grok_ffi_decode_ms_total
                .fetch_add(decode_ms, Ordering::Relaxed);
        }
        Some(Jp2RenderBackend::OpenJpegFfi) => {
            metrics
                .jp2_openjpeg_ffi_decode_ms_total
                .fetch_add(decode_ms, Ordering::Relaxed);
        }
        None => {}
    }
}

fn duration_ms_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn build_prometheus_metrics(state: &AppState) -> Result<String> {
    let metrics = &state.metrics;
    let cache_stats = build_cache_stats(state)?;
    let cache_pressure = if cache_stats.max_bytes > 0 {
        cache_stats.current_bytes as f64 / cache_stats.max_bytes as f64
    } else {
        0.0
    };
    let hits = metrics.cache_hits_total.load(Ordering::Relaxed);
    let misses = metrics.cache_misses_total.load(Ordering::Relaxed);
    let cache_requests = hits.saturating_add(misses);
    let hit_ratio = if cache_requests > 0 {
        hits as f64 / cache_requests as f64
    } else {
        0.0
    };
    let render_active = metrics.render_active.load(Ordering::Relaxed);
    let render_available = state.render_permits.available_permits() as u64;
    let render_capacity = render_active.saturating_add(render_available);
    let process_memory = read_process_memory();

    let mut out = String::new();
    write_metric_header(
        &mut out,
        "gigatiff_http_requests_total",
        "counter",
        "HTTP requests observed by the server.",
    );
    write_metric(
        &mut out,
        "gigatiff_http_requests_total",
        metrics.http_requests_total.load(Ordering::Relaxed),
    );
    write_metric_header(
        &mut out,
        "gigatiff_http_responses_total",
        "counter",
        "HTTP responses emitted by the server.",
    );
    write_metric(
        &mut out,
        "gigatiff_http_responses_total",
        metrics.http_responses_total.load(Ordering::Relaxed),
    );
    write_metric_header(
        &mut out,
        "gigatiff_http_errors_total",
        "counter",
        "HTTP 5xx responses emitted by the server.",
    );
    write_metric(
        &mut out,
        "gigatiff_http_errors_total",
        metrics.http_errors_total.load(Ordering::Relaxed),
    );
    write_metric_header(
        &mut out,
        "gigatiff_rate_limited_requests_total",
        "counter",
        "HTTP requests rejected by the per-client rate limiter.",
    );
    write_metric(
        &mut out,
        "gigatiff_rate_limited_requests_total",
        metrics.rate_limited_requests_total.load(Ordering::Relaxed),
    );

    write_metric_header(
        &mut out,
        "gigatiff_cache_hits_total",
        "counter",
        "Persistent response-cache hits.",
    );
    write_metric(&mut out, "gigatiff_cache_hits_total", hits);
    write_metric_header(
        &mut out,
        "gigatiff_cache_misses_total",
        "counter",
        "Persistent response-cache misses.",
    );
    write_metric(&mut out, "gigatiff_cache_misses_total", misses);
    write_metric_header(
        &mut out,
        "gigatiff_cache_disabled_total",
        "counter",
        "Responses rendered while persistent cache was disabled.",
    );
    write_metric(
        &mut out,
        "gigatiff_cache_disabled_total",
        metrics.cache_disabled_total.load(Ordering::Relaxed),
    );
    write_metric_header(
        &mut out,
        "gigatiff_cache_hit_ratio",
        "gauge",
        "Persistent response-cache hit ratio excluding disabled responses.",
    );
    write_metric_f64(&mut out, "gigatiff_cache_hit_ratio", hit_ratio);
    write_metric_header(
        &mut out,
        "gigatiff_cache_current_bytes",
        "gauge",
        "Current persistent response-cache size in bytes.",
    );
    write_metric(
        &mut out,
        "gigatiff_cache_current_bytes",
        cache_stats.current_bytes,
    );
    write_metric_header(
        &mut out,
        "gigatiff_cache_max_bytes",
        "gauge",
        "Configured persistent response-cache size limit in bytes.",
    );
    write_metric(&mut out, "gigatiff_cache_max_bytes", cache_stats.max_bytes);
    write_metric_header(
        &mut out,
        "gigatiff_cache_pressure_ratio",
        "gauge",
        "Persistent response-cache size divided by configured maximum.",
    );
    write_metric_f64(&mut out, "gigatiff_cache_pressure_ratio", cache_pressure);
    write_metric_header(
        &mut out,
        "gigatiff_cache_files",
        "gauge",
        "Persistent response-cache file count.",
    );
    write_metric(
        &mut out,
        "gigatiff_cache_files",
        cache_stats.file_count as u64,
    );
    if let Some(rss_bytes) = process_memory.resident_bytes {
        write_metric_header(
            &mut out,
            "gigatiff_process_resident_memory_bytes",
            "gauge",
            "Resident memory used by the server process in bytes.",
        );
        write_metric(
            &mut out,
            "gigatiff_process_resident_memory_bytes",
            rss_bytes,
        );
    }
    if let Some(virtual_bytes) = process_memory.virtual_bytes {
        write_metric_header(
            &mut out,
            "gigatiff_process_virtual_memory_bytes",
            "gauge",
            "Virtual memory used by the server process in bytes.",
        );
        write_metric(
            &mut out,
            "gigatiff_process_virtual_memory_bytes",
            virtual_bytes,
        );
    }

    write_metric_header(
        &mut out,
        "gigatiff_render_jobs_total",
        "counter",
        "IIIF render jobs, including cache hits.",
    );
    write_metric(
        &mut out,
        "gigatiff_render_jobs_total",
        metrics.render_jobs_total.load(Ordering::Relaxed),
    );
    write_metric_header(
        &mut out,
        "gigatiff_render_jobs_failed_total",
        "counter",
        "Failed IIIF render jobs.",
    );
    write_metric(
        &mut out,
        "gigatiff_render_jobs_failed_total",
        metrics.render_jobs_failed_total.load(Ordering::Relaxed),
    );
    write_metric_header(
        &mut out,
        "gigatiff_render_timeouts_total",
        "counter",
        "IIIF render jobs that exceeded the configured response timeout.",
    );
    write_metric(
        &mut out,
        "gigatiff_render_timeouts_total",
        metrics.render_timeouts_total.load(Ordering::Relaxed),
    );
    write_metric_header(
        &mut out,
        "gigatiff_render_active",
        "gauge",
        "Currently active blocking render tasks.",
    );
    write_metric(&mut out, "gigatiff_render_active", render_active);
    write_metric_header(
        &mut out,
        "gigatiff_render_queue_available_permits",
        "gauge",
        "Available render concurrency permits.",
    );
    write_metric(
        &mut out,
        "gigatiff_render_queue_available_permits",
        render_available,
    );
    write_metric_header(
        &mut out,
        "gigatiff_render_queue_capacity",
        "gauge",
        "Approximate configured render concurrency capacity.",
    );
    write_metric(&mut out, "gigatiff_render_queue_capacity", render_capacity);
    write_metric_header(
        &mut out,
        "gigatiff_render_queue_wait_ms_total",
        "counter",
        "Total time spent waiting for render permits in milliseconds.",
    );
    write_metric(
        &mut out,
        "gigatiff_render_queue_wait_ms_total",
        metrics.render_queue_wait_ms_total.load(Ordering::Relaxed),
    );
    write_metric_header(
        &mut out,
        "gigatiff_render_queue_wait_count",
        "counter",
        "Number of render permit waits.",
    );
    write_metric(
        &mut out,
        "gigatiff_render_queue_wait_count",
        metrics.render_queue_wait_count.load(Ordering::Relaxed),
    );

    write_metric_header(
        &mut out,
        "gigatiff_render_ms_total",
        "counter",
        "Total render phase duration in milliseconds.",
    );
    write_metric(
        &mut out,
        "gigatiff_render_ms_total",
        metrics.render_ms_total.load(Ordering::Relaxed),
    );
    write_metric_header(
        &mut out,
        "gigatiff_render_decode_ms_total",
        "counter",
        "Total backend decode duration in milliseconds.",
    );
    write_metric(
        &mut out,
        "gigatiff_render_decode_ms_total",
        metrics.render_decode_ms_total.load(Ordering::Relaxed),
    );
    write_metric_header(
        &mut out,
        "gigatiff_render_encode_ms_total",
        "counter",
        "Total output encoding duration in milliseconds.",
    );
    write_metric(
        &mut out,
        "gigatiff_render_encode_ms_total",
        metrics.render_encode_ms_total.load(Ordering::Relaxed),
    );
    write_metric_header(
        &mut out,
        "gigatiff_render_cache_read_ms_total",
        "counter",
        "Total cache read duration in milliseconds.",
    );
    write_metric(
        &mut out,
        "gigatiff_render_cache_read_ms_total",
        metrics.render_cache_read_ms_total.load(Ordering::Relaxed),
    );
    write_metric_header(
        &mut out,
        "gigatiff_render_cache_store_ms_total",
        "counter",
        "Total cache store duration in milliseconds.",
    );
    write_metric(
        &mut out,
        "gigatiff_render_cache_store_ms_total",
        metrics.render_cache_store_ms_total.load(Ordering::Relaxed),
    );
    write_metric_header(
        &mut out,
        "gigatiff_render_cache_prune_ms_total",
        "counter",
        "Total cache prune duration in milliseconds.",
    );
    write_metric(
        &mut out,
        "gigatiff_render_cache_prune_ms_total",
        metrics.render_cache_prune_ms_total.load(Ordering::Relaxed),
    );

    write_metric_header(
        &mut out,
        "gigatiff_jp2_grok_cli_decode_ms_total",
        "counter",
        "Total Grok CLI JPEG2000 decode duration in milliseconds.",
    );
    write_metric(
        &mut out,
        "gigatiff_jp2_grok_cli_decode_ms_total",
        metrics.jp2_grok_cli_decode_ms_total.load(Ordering::Relaxed),
    );
    write_metric_header(
        &mut out,
        "gigatiff_jp2_grok_ffi_decode_ms_total",
        "counter",
        "Total Grok FFI JPEG2000 decode duration in milliseconds.",
    );
    write_metric(
        &mut out,
        "gigatiff_jp2_grok_ffi_decode_ms_total",
        metrics.jp2_grok_ffi_decode_ms_total.load(Ordering::Relaxed),
    );
    write_metric_header(
        &mut out,
        "gigatiff_jp2_openjpeg_ffi_decode_ms_total",
        "counter",
        "Total OpenJPEG FFI JPEG2000 decode duration in milliseconds.",
    );
    write_metric(
        &mut out,
        "gigatiff_jp2_openjpeg_ffi_decode_ms_total",
        metrics
            .jp2_openjpeg_ffi_decode_ms_total
            .load(Ordering::Relaxed),
    );
    write_metric_header(
        &mut out,
        "gigatiff_jp2_grok_to_openjpeg_fallbacks_total",
        "counter",
        "JPEG2000 render fallbacks from Grok to OpenJPEG FFI.",
    );
    write_metric(
        &mut out,
        "gigatiff_jp2_grok_to_openjpeg_fallbacks_total",
        metrics
            .jp2_grok_to_openjpeg_fallbacks_total
            .load(Ordering::Relaxed),
    );

    Ok(out)
}

fn write_metric_header(out: &mut String, name: &str, metric_type: &str, help: &str) {
    out.push_str("# HELP ");
    out.push_str(name);
    out.push(' ');
    out.push_str(help);
    out.push('\n');
    out.push_str("# TYPE ");
    out.push_str(name);
    out.push(' ');
    out.push_str(metric_type);
    out.push('\n');
}

fn write_metric(out: &mut String, name: &str, value: u64) {
    out.push_str(name);
    out.push(' ');
    out.push_str(&value.to_string());
    out.push('\n');
}

fn write_metric_f64(out: &mut String, name: &str, value: f64) {
    out.push_str(name);
    out.push(' ');
    out.push_str(&format!("{value:.6}"));
    out.push('\n');
}

#[derive(Default)]
struct ProcessMemory {
    resident_bytes: Option<u64>,
    virtual_bytes: Option<u64>,
}

fn read_process_memory() -> ProcessMemory {
    #[cfg(target_os = "linux")]
    {
        let Ok(status) = fs::read_to_string("/proc/self/status") else {
            return ProcessMemory::default();
        };
        let mut memory = ProcessMemory::default();
        for line in status.lines() {
            if let Some(value) = line.strip_prefix("VmRSS:") {
                memory.resident_bytes = parse_proc_status_kb(value);
            } else if let Some(value) = line.strip_prefix("VmSize:") {
                memory.virtual_bytes = parse_proc_status_kb(value);
            }
        }
        memory
    }

    #[cfg(not(target_os = "linux"))]
    {
        ProcessMemory::default()
    }
}

#[cfg(target_os = "linux")]
fn parse_proc_status_kb(value: &str) -> Option<u64> {
    value
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|kb| kb.saturating_mul(1024))
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

    let queue_started = Instant::now();
    let guards = match acquire_render_guards(&state, &image_path, &headers).await {
        Ok(guards) => guards,
        Err(err) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, anyhow!(err)),
    };
    record_queue_wait(&state.metrics, queue_started.elapsed());
    let render_timeout = state.render_timeout;
    let metrics = Arc::clone(&state.metrics);

    let task = tokio::task::spawn_blocking(move || {
        let _guards = guards;
        state.metrics.render_active.fetch_add(1, Ordering::Relaxed);
        let result = render_iiif_image(&state, image_path, request);
        state.metrics.render_active.fetch_sub(1, Ordering::Relaxed);
        result
    });
    let result = match tokio::time::timeout(render_timeout, task).await {
        Ok(joined) => joined
            .map_err(|err| anyhow!("render task failed: {err}"))
            .and_then(|result| result),
        Err(_) => {
            metrics
                .render_timeouts_total
                .fetch_add(1, Ordering::Relaxed);
            Err(anyhow!(
                "render exceeded --render-timeout-sec ({}s)",
                render_timeout.as_secs()
            ))
        }
    };

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
            if let Some(jp2_backend) = rendered.jp2_backend {
                response.headers_mut().insert(
                    "x-gigatiff-jp2-backend",
                    HeaderValue::from_static(jp2_backend),
                );
            }
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
        Err(err) => {
            metrics
                .render_jobs_failed_total
                .fetch_add(1, Ordering::Relaxed);
            let status = if err.to_string().contains("--render-timeout-sec") {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_REQUEST
            };
            error_response(status, err)
        }
    }
}

struct RenderedResponse {
    bytes: Vec<u8>,
    content_type: &'static str,
    cache_status: &'static str,
    canonical_path: String,
    timing: ResponseTiming,
    jp2_backend: Option<&'static str>,
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
    state
        .metrics
        .render_jobs_total
        .fetch_add(1, Ordering::Relaxed);
    let info = load_cached_info_blocking(state, &image_path)?;
    let rect = parse_region(&request.region, info.width, info.height)?;
    let (out_width, out_height) = parse_size(&request.size, rect, state.max_output_pixels)?;
    validate_upscale(out_width, out_height, rect, state.max_upscale)?;
    let rotation = parse_rotation(&request.rotation)?;
    let canonical_path = canonical_image_path(
        &request,
        &rect,
        info.width,
        info.height,
        out_width,
        out_height,
        rotation,
    );

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

    let jp2_backend = select_jp2_backend(state, &info)?;
    let jp2_fallback_cache_backend =
        jp2_backend.and_then(|backend| fallback_cache_backend(state.jp2_backend, backend));

    let cache_path = if response_cache_enabled(state) {
        let path = response_cache_path(
            &state.cache_dir,
            &image_path,
            &info,
            &canonical_path,
            out_width,
            out_height,
            state,
            jp2_backend,
        )?;
        let cache_read_start = Instant::now();
        if let Some(bytes) = read_cached_response(state, &path)? {
            timing.cache_read = cache_read_start.elapsed();
            timing.total = total_start.elapsed();
            record_cache_status(&state.metrics, "hit");
            record_response_timing(&state.metrics, timing);
            return Ok(RenderedResponse {
                bytes,
                content_type: content_type(&request.format),
                cache_status: "hit",
                canonical_path,
                timing,
                jp2_backend: jp2_backend.map(Jp2RenderBackend::label),
            });
        }
        if let Some(fallback_backend) = jp2_fallback_cache_backend {
            let fallback_path = response_cache_path(
                &state.cache_dir,
                &image_path,
                &info,
                &canonical_path,
                out_width,
                out_height,
                state,
                Some(fallback_backend),
            )?;
            if let Some(bytes) = read_cached_response(state, &fallback_path)? {
                timing.cache_read = cache_read_start.elapsed();
                timing.total = total_start.elapsed();
                record_cache_status(&state.metrics, "hit");
                record_response_timing(&state.metrics, timing);
                return Ok(RenderedResponse {
                    bytes,
                    content_type: content_type(&request.format),
                    cache_status: "hit",
                    canonical_path,
                    timing,
                    jp2_backend: Some(fallback_backend.label()),
                });
            }
        }
        timing.cache_read = cache_read_start.elapsed();
        Some(path)
    } else {
        None
    };

    let render_start = Instant::now();
    let (preview, actual_jp2_backend) = match &info.source {
        ServerImageSource::Tiff(tiff_info) => (
            render_preview(
                &image_path,
                tiff_info,
                rect,
                out_width.max(out_height),
                state.max_chunk_mb,
                state.backend,
                None,
                None,
            )?,
            None,
        ),
        #[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
        ServerImageSource::Jpeg2000(jpeg2000_info) => render_jpeg2000_preview(
            &image_path,
            jpeg2000_info,
            rect,
            out_width,
            out_height,
            jp2_backend.ok_or_else(|| anyhow!("missing JPEG2000 backend selection"))?,
            state.jp2_backend == Jp2BackendPolicy::Auto,
            state.openjpeg_threads,
        )?,
    };
    timing.render = render_start.elapsed();
    record_decode_timing(&state.metrics, preview.stats.decode, actual_jp2_backend);
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
        let actual_cache_path = if actual_jp2_backend != jp2_backend {
            response_cache_path(
                &state.cache_dir,
                &image_path,
                &info,
                &canonical_path,
                out_width,
                out_height,
                state,
                actual_jp2_backend,
            )?
        } else {
            cache_path
        };
        store_cached_response(state, &actual_cache_path, &bytes)?;
        timing.cache_store = store_start.elapsed();
        let prune_start = Instant::now();
        prune_response_cache_throttled(state)?;
        timing.cache_prune = prune_start.elapsed();
        "miss"
    } else {
        "disabled"
    };
    timing.total = total_start.elapsed();
    if let (Some(initial), Some(actual)) = (jp2_backend, actual_jp2_backend) {
        if is_grok_backend(initial) && actual == Jp2RenderBackend::OpenJpegFfi {
            state
                .metrics
                .jp2_grok_to_openjpeg_fallbacks_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    record_cache_status(&state.metrics, cache_status);
    record_response_timing(&state.metrics, timing);
    Ok(RenderedResponse {
        bytes,
        content_type,
        cache_status,
        canonical_path,
        timing,
        jp2_backend: actual_jp2_backend.map(Jp2RenderBackend::label),
    })
}

fn prewarm_cache_blocking(
    state: &AppState,
    id: String,
    image_path: PathBuf,
) -> Result<CacheWarmReport> {
    if !response_cache_enabled(state) {
        bail!("response cache is disabled");
    }

    let info = load_cached_info_blocking(state, &image_path)?;
    let requests = prewarm_requests(&id, &info, state.tile_size);
    let attempted = requests.len();
    let mut rendered = 0usize;
    let mut reports = Vec::with_capacity(attempted);

    for request in requests {
        match render_iiif_image(state, image_path.clone(), request) {
            Ok(rendered_response) => {
                rendered += usize::from(rendered_response.cache_status != "hit");
                reports.push(CacheWarmRequestReport {
                    canonical_path: Some(rendered_response.canonical_path),
                    cache_status: Some(rendered_response.cache_status),
                    error: None,
                });
            }
            Err(err) => {
                reports.push(CacheWarmRequestReport {
                    canonical_path: None,
                    cache_status: None,
                    error: Some(err.to_string()),
                });
            }
        }
    }

    let failed = reports
        .iter()
        .filter(|report| report.error.is_some())
        .count();
    Ok(CacheWarmReport {
        id,
        attempted,
        rendered,
        failed,
        requests: reports,
    })
}

fn prewarm_requests(id: &str, info: &ServerImageInfo, tile_size: u32) -> Vec<IiifImageRequest> {
    let tile_width = tile_size.min(info.width).max(1);
    let tile_height = tile_size.min(info.height).max(1);
    let thumbnail_size = if info.width <= 512 && info.height <= 512 {
        "max".to_string()
    } else {
        "!512,512".to_string()
    };
    vec![
        IiifImageRequest {
            id: id.to_string(),
            region: "full".to_string(),
            size: thumbnail_size,
            rotation: "0".to_string(),
            quality: "default".to_string(),
            format: ImageFormat::Webp,
        },
        IiifImageRequest {
            id: id.to_string(),
            region: format!("0,0,{tile_width},{tile_height}"),
            size: format!("{tile_width},{tile_height}"),
            rotation: "0".to_string(),
            quality: "default".to_string(),
            format: ImageFormat::Webp,
        },
    ]
}

fn response_cache_path(
    cache_dir: &Path,
    image_path: &Path,
    info: &ServerImageInfo,
    canonical_path: &str,
    out_width: u32,
    out_height: u32,
    state: &AppState,
    jp2_backend: Option<Jp2RenderBackend>,
) -> Result<PathBuf> {
    let _ = jp2_backend;
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
    hash.write_bytes(b"gigatiff-server-response-v10");
    hash.write_bytes(canonical.to_string_lossy().as_bytes());
    hash.write_u64(metadata.len());
    hash.write_u64(modified.as_secs());
    hash.write_u64(modified.subsec_nanos() as u64);
    hash.write_u64(info.width as u64);
    hash.write_u64(info.height as u64);
    hash.write_bytes(info.source_label().as_bytes());
    match &info.source {
        ServerImageSource::Tiff(tiff_info) => {
            hash.write_u64(tiff_info.chunk_width as u64);
            hash.write_u64(tiff_info.chunk_height as u64);
        }
        #[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
        ServerImageSource::Jpeg2000(jpeg2000_info) => {
            hash.write_u64(jpeg2000_info.components.unwrap_or_default() as u64);
            hash.write_u64(jpeg2000_info.precision.unwrap_or_default() as u64);
            hash.write_u64(jpeg2000_info.tile_width.unwrap_or_default() as u64);
            hash.write_u64(jpeg2000_info.tile_height.unwrap_or_default() as u64);
            hash.write_u64(u64::from(jpeg2000_supports_region_tiles(jpeg2000_info)));
            hash.write_u64(u64::from(should_use_openjpeg_fallback(jpeg2000_info)));
            hash.write_bytes(state.jp2_backend.cache_label().as_bytes());
            hash.write_bytes(
                jp2_backend
                    .map(Jp2RenderBackend::label)
                    .unwrap_or("none")
                    .as_bytes(),
            );
        }
    }
    hash.write_u64(out_width as u64);
    hash.write_u64(out_height as u64);
    hash.write_bytes(canonical_path.as_bytes());
    hash.write_u64(state.quality as u64);
    hash.write_bytes(format!("{:?}", state.backend).as_bytes());

    let extension = canonical_path
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .unwrap_or("cache");
    let source_prefix = source_cache_prefix(image_path);
    let filename = format!("{source_prefix}-{:016x}.{extension}", hash.finish());
    Ok(cache_dir.join(&source_prefix[0..2]).join(filename))
}

fn source_cache_prefix(image_path: &Path) -> String {
    let canonical = image_path
        .canonicalize()
        .unwrap_or_else(|_| image_path.to_path_buf());
    let mut hash = Fnv1a64::new();
    hash.write_bytes(b"gigatiff-server-source-v1");
    hash.write_bytes(canonical.to_string_lossy().as_bytes());
    format!("{:016x}", hash.finish())
}

fn response_cache_enabled(state: &AppState) -> bool {
    match state.cache_backend {
        ResponseCacheBackend::Disk => state.cache_max_bytes > 0,
        ResponseCacheBackend::Dragonfly => state.dragonfly_cache.is_some(),
    }
}

fn response_cache_key(state: &AppState, path: &Path) -> Result<String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("cache path has no valid file name: {}", path.display()))?;
    Ok(format!("{}:{file_name}", state.cache_namespace))
}

fn read_cached_response(state: &AppState, path: &Path) -> Result<Option<Vec<u8>>> {
    match state.cache_backend {
        ResponseCacheBackend::Disk => read_cached_response_disk(path, state.cache_ttl),
        ResponseCacheBackend::Dragonfly => read_cached_response_dragonfly(state, path),
    }
}

fn read_cached_response_disk(path: &Path, ttl: Option<Duration>) -> Result<Option<Vec<u8>>> {
    if let Some(ttl) = ttl {
        if cache_file_expired(path, ttl)? {
            let _ = fs::remove_file(path);
            return Ok(None);
        }
    }
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading cache file {}", path.display())),
    }
}

fn read_cached_response_dragonfly(state: &AppState, path: &Path) -> Result<Option<Vec<u8>>> {
    let Some(cache) = &state.dragonfly_cache else {
        return Ok(None);
    };
    let key = response_cache_key(state, path)?;
    let mut connection = cache
        .client
        .get_connection()
        .with_context(|| "connecting to Dragonfly response cache")?;
    let bytes: Option<Vec<u8>> = connection
        .get(&key)
        .with_context(|| format!("reading Dragonfly cache key {key}"))?;
    Ok(bytes)
}

fn cache_file_expired(path: &Path, ttl: Duration) -> Result<bool> {
    let modified = match fs::metadata(path).and_then(|metadata| metadata.modified()) {
        Ok(modified) => modified,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(err).with_context(|| format!("reading metadata for {}", path.display()));
        }
    };
    Ok(modified.elapsed().unwrap_or_default() > ttl)
}

fn store_cached_response(state: &AppState, path: &Path, bytes: &[u8]) -> Result<()> {
    match state.cache_backend {
        ResponseCacheBackend::Disk => store_cached_response_disk(path, bytes),
        ResponseCacheBackend::Dragonfly => store_cached_response_dragonfly(state, path, bytes),
    }
}

fn store_cached_response_disk(path: &Path, bytes: &[u8]) -> Result<()> {
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

fn store_cached_response_dragonfly(state: &AppState, path: &Path, bytes: &[u8]) -> Result<()> {
    let Some(cache) = &state.dragonfly_cache else {
        return Ok(());
    };
    let key = response_cache_key(state, path)?;
    let mut connection = cache
        .client
        .get_connection()
        .with_context(|| "connecting to Dragonfly response cache")?;
    if let Some(ttl) = state.cache_ttl {
        let _: () = connection
            .set_ex(&key, bytes, ttl.as_secs())
            .with_context(|| format!("writing Dragonfly cache key {key}"))?;
    } else {
        let _: () = connection
            .set(&key, bytes)
            .with_context(|| format!("writing Dragonfly cache key {key}"))?;
    }
    Ok(())
}

fn prune_response_cache_throttled(state: &AppState) -> Result<()> {
    if !response_cache_enabled(state) || state.cache_backend == ResponseCacheBackend::Dragonfly {
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

    let report = prune_response_cache(&state.cache_dir, state.cache_max_bytes, state.cache_ttl)?;
    *state
        .last_cache_prune_report
        .lock()
        .map_err(|_| anyhow!("cache prune report lock poisoned"))? = report;
    Ok(())
}

fn prune_response_cache(
    cache_dir: &Path,
    max_bytes: u64,
    ttl: Option<Duration>,
) -> Result<CachePruneReport> {
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
    let mut removed_files = 0u64;
    let mut removed_bytes = 0u64;
    if let Some(ttl) = ttl {
        let now = UNIX_EPOCH.elapsed().unwrap_or_default();
        files.retain(|file| {
            if now.saturating_sub(file.modified) <= ttl {
                return true;
            }
            match fs::remove_file(&file.path) {
                Ok(()) => {
                    total_bytes = total_bytes.saturating_sub(file.bytes);
                    removed_files += 1;
                    removed_bytes = removed_bytes.saturating_add(file.bytes);
                    false
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    total_bytes = total_bytes.saturating_sub(file.bytes);
                    false
                }
                Err(_) => true,
            }
        });
    }

    if total_bytes <= max_bytes {
        remove_empty_cache_dirs(cache_dir, cache_dir)?;
        return Ok(CachePruneReport {
            last_started_unix: Some(started),
            last_finished_unix: Some(current_unix_secs()),
            removed_files,
            removed_bytes,
        });
    }

    files.sort_by_key(|file| file.modified);
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
    let (file_count, current_bytes) = match state.cache_backend {
        ResponseCacheBackend::Disk => {
            if state.cache_max_bytes > 0 && state.cache_dir.exists() {
                let mut files = Vec::new();
                let mut total_bytes = 0u64;
                collect_cache_files(&state.cache_dir, &mut files, &mut total_bytes)?;
                (files.len(), total_bytes)
            } else {
                (0, 0)
            }
        }
        ResponseCacheBackend::Dragonfly => dragonfly_cache_stats(state)?,
    };

    let last_prune = state
        .last_cache_prune_report
        .lock()
        .map_err(|_| anyhow!("cache prune report lock poisoned"))?
        .clone();

    Ok(CacheStats {
        enabled: response_cache_enabled(state),
        backend: state.cache_backend.label(),
        cache_dir: cache_location(state),
        namespace: state.cache_namespace.to_string(),
        max_bytes: if state.cache_backend == ResponseCacheBackend::Disk {
            state.cache_max_bytes
        } else {
            0
        },
        ttl_sec: state.cache_ttl.map(|ttl| ttl.as_secs()),
        current_bytes,
        file_count,
        prune_interval_sec: state.cache_prune_interval.as_secs(),
        last_prune,
    })
}

fn cache_location(state: &AppState) -> String {
    match state.cache_backend {
        ResponseCacheBackend::Disk => state.cache_dir.display().to_string(),
        ResponseCacheBackend::Dragonfly => state
            .dragonfly_cache
            .as_ref()
            .map(|cache| cache.url_display.clone())
            .unwrap_or_else(|| "dragonfly:disabled".to_string()),
    }
}

fn dragonfly_cache_stats(state: &AppState) -> Result<(usize, u64)> {
    let Some(cache) = &state.dragonfly_cache else {
        return Ok((0, 0));
    };
    let mut connection = cache
        .client
        .get_connection()
        .with_context(|| "connecting to Dragonfly response cache")?;
    let pattern = format!("{}:*", state.cache_namespace);
    let keys: Vec<String> = connection
        .scan_match(pattern)
        .with_context(|| "scanning Dragonfly response cache keys")?
        .collect::<redis::RedisResult<Vec<_>>>()
        .with_context(|| "collecting Dragonfly response cache keys")?;
    let mut total_bytes = 0u64;
    for key in &keys {
        if let Ok(bytes) = connection.get::<_, Vec<u8>>(key) {
            total_bytes = total_bytes.saturating_add(bytes.len() as u64);
        }
    }
    Ok((keys.len(), total_bytes))
}

fn purge_response_cache_for_identifier(state: &AppState, image_path: &Path) -> Result<CacheStats> {
    if state.cache_backend == ResponseCacheBackend::Dragonfly {
        return purge_dragonfly_response_cache_for_identifier(state, image_path);
    }

    let started = current_unix_secs();
    let source_prefix = source_cache_prefix(image_path);
    let prefix = format!("{source_prefix}-");
    let mut removed_files = 0u64;
    let mut removed_bytes = 0u64;

    if state.cache_max_bytes > 0 && state.cache_dir.exists() {
        let mut files = Vec::new();
        let mut total_bytes = 0u64;
        collect_cache_files(&state.cache_dir, &mut files, &mut total_bytes)?;
        for file in files {
            let Some(name) = file.path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.starts_with(&prefix) {
                continue;
            }
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

fn purge_response_cache(state: &AppState) -> Result<CacheStats> {
    if state.cache_backend == ResponseCacheBackend::Dragonfly {
        return purge_dragonfly_response_cache(state);
    }

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

fn purge_dragonfly_response_cache_for_identifier(
    state: &AppState,
    image_path: &Path,
) -> Result<CacheStats> {
    let started = current_unix_secs();
    let source_prefix = source_cache_prefix(image_path);
    let key_prefix = format!("{}:{source_prefix}-", state.cache_namespace);
    let removed = purge_dragonfly_keys(state, &format!("{key_prefix}*"))?;
    update_cache_prune_report(state, started, removed.0, removed.1)?;
    build_cache_stats(state)
}

fn purge_dragonfly_response_cache(state: &AppState) -> Result<CacheStats> {
    let started = current_unix_secs();
    let removed = purge_dragonfly_keys(state, &format!("{}:*", state.cache_namespace))?;
    update_cache_prune_report(state, started, removed.0, removed.1)?;
    build_cache_stats(state)
}

fn purge_dragonfly_keys(state: &AppState, pattern: &str) -> Result<(u64, u64)> {
    let Some(cache) = &state.dragonfly_cache else {
        return Ok((0, 0));
    };
    let mut connection = cache
        .client
        .get_connection()
        .with_context(|| "connecting to Dragonfly response cache")?;
    let keys: Vec<String> = connection
        .scan_match(pattern)
        .with_context(|| format!("scanning Dragonfly cache keys with pattern {pattern}"))?
        .collect::<redis::RedisResult<Vec<_>>>()
        .with_context(|| "collecting Dragonfly response cache keys")?;
    let mut removed_bytes = 0u64;
    for key in &keys {
        if let Ok(bytes) = connection.get::<_, Vec<u8>>(key) {
            removed_bytes = removed_bytes.saturating_add(bytes.len() as u64);
        }
    }
    if !keys.is_empty() {
        let _: () = connection
            .del(&keys)
            .with_context(|| "deleting Dragonfly response cache keys")?;
    }
    Ok((keys.len() as u64, removed_bytes))
}

fn update_cache_prune_report(
    state: &AppState,
    started: u64,
    removed_files: u64,
    removed_bytes: u64,
) -> Result<()> {
    *state
        .last_cache_prune_report
        .lock()
        .map_err(|_| anyhow!("cache prune report lock poisoned"))? = CachePruneReport {
        last_started_unix: Some(started),
        last_finished_unix: Some(current_unix_secs()),
        removed_files,
        removed_bytes,
    };
    Ok(())
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

fn validate_iiif_tail(tail: &str) -> Result<()> {
    if tail.is_empty() || tail.len() > MAX_IIIF_TAIL_LEN {
        bail!("IIIF request path is empty or too long");
    }
    if tail.chars().any(char::is_control) {
        bail!("IIIF request path contains control characters");
    }
    Ok(())
}

fn validate_iiif_token(value: &str, label: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_IIIF_TOKEN_LEN {
        bail!("IIIF {label} token is empty or too long");
    }
    if value.chars().any(char::is_control) {
        bail!("IIIF {label} token contains control characters");
    }
    Ok(())
}

fn validate_identifier_text(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > MAX_IIIF_IDENTIFIER_LEN {
        bail!("IIIF identifier is empty or too long");
    }
    if id.chars().any(char::is_control) {
        bail!("IIIF identifier contains control characters");
    }
    let segment_count = id.split('/').count();
    if segment_count > MAX_IIIF_IDENTIFIER_SEGMENTS {
        bail!("IIIF identifier has too many path segments");
    }
    for segment in id.split('/') {
        if segment.is_empty() || segment.len() > MAX_IIIF_IDENTIFIER_SEGMENT_LEN {
            bail!("IIIF identifier segment is empty or too long");
        }
    }
    Ok(())
}

fn parse_iiif_image_request(tail: &str) -> Result<IiifImageRequest> {
    validate_iiif_tail(tail)?;
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
    validate_iiif_token(quality, "quality")?;
    validate_iiif_token(format, "format")?;
    validate_iiif_token(&region, "region")?;
    validate_iiif_token(&size, "size")?;
    validate_iiif_token(&rotation, "rotation")?;
    validate_identifier_text(&id)?;

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
    image_width: u32,
    image_height: u32,
    out_width: u32,
    out_height: u32,
    rotation: IiifRotation,
) -> String {
    format!(
        "/iiif/3/{}/{}/{}/{}/{}.{}",
        encode_id(&request.id),
        canonical_region(rect, image_width, image_height),
        canonical_size(&request.size, out_width, out_height),
        canonical_rotation(rotation),
        request.quality,
        request.format.extension()
    )
}

fn canonical_region(rect: &Rect, image_width: u32, image_height: u32) -> String {
    if rect.x == 0 && rect.y == 0 && rect.width == image_width && rect.height == image_height {
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
    validate_iiif_token(rotation, "rotation")?;
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

fn parse_region(region: &str, image_width: u32, image_height: u32) -> Result<Rect> {
    validate_iiif_token(region, "region")?;
    if region == "full" || region == "max" {
        return Ok(Rect {
            x: 0,
            y: 0,
            width: image_width,
            height: image_height,
        });
    }

    if region == "square" {
        let side = image_width.min(image_height);
        return Ok(Rect {
            x: (image_width - side) / 2,
            y: (image_height - side) / 2,
            width: side,
            height: side,
        });
    }

    if let Some(percent_region) = region.strip_prefix("pct:") {
        let parts = parse_percentage_parts(percent_region, 4, "region")?;
        let x = percent_to_u32(parts[0], image_width, false);
        let y = percent_to_u32(parts[1], image_height, false);
        let width = percent_to_u32(parts[2], image_width, true);
        let height = percent_to_u32(parts[3], image_height, true);
        if width == 0 || height == 0 {
            bail!("pct region width and height must be greater than zero");
        }
        return clamp_rect(x, y, width, height, image_width, image_height);
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
        image_width,
        image_height,
    )
}

fn parse_size(size: &str, rect: Rect, max_output_pixels: u32) -> Result<(u32, u32)> {
    validate_iiif_token(size, "size")?;
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

fn validate_upscale(width: u32, height: u32, rect: Rect, max_upscale: f64) -> Result<()> {
    let width_scale = width as f64 / rect.width.max(1) as f64;
    let height_scale = height as f64 / rect.height.max(1) as f64;
    if width_scale > max_upscale || height_scale > max_upscale {
        bail!("IIIF upscale exceeds --max-upscale ({max_upscale})");
    }
    Ok(())
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

#[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
fn load_jpeg2000_info(path: &Path) -> Result<ServerImageInfo> {
    #[cfg(feature = "jpeg2000-grok")]
    {
        let output = Command::new(grok_dump_command())
            .arg("-i")
            .arg(path)
            .output()
            .with_context(|| "running grk_dump for JPEG2000 metadata")?;

        if !output.status.success() {
            bail!(
                "grk_dump failed for {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let mut dump = String::from_utf8_lossy(&output.stdout).to_string();
        if !output.stderr.is_empty() {
            dump.push('\n');
            dump.push_str(&String::from_utf8_lossy(&output.stderr));
        }

        let info = parse_grok_dump_info(&dump)?;
        let mut jpeg2000 = info.jpeg2000;
        enrich_jpeg2000_info_from_openjpeg(path, &mut jpeg2000);
        return Ok(ServerImageInfo {
            width: info.width,
            height: info.height,
            source: ServerImageSource::Jpeg2000(jpeg2000),
        });
    }

    #[cfg(all(not(feature = "jpeg2000-grok"), feature = "jpeg2000-openjpeg-ffi"))]
    {
        let info = gigatiff_core::openjpeg_ffi::read_info(path)?;
        let precision = info.components.first().map(|component| component.precision);
        return Ok(ServerImageInfo {
            width: info.width,
            height: info.height,
            source: ServerImageSource::Jpeg2000(Jpeg2000Info {
                components: Some(info.components.len() as u32),
                precision,
                tile_width: None,
                tile_height: None,
                progression_order: None,
                resolution_levels: None,
                icc_profile_len: Some(info.icc_profile_len).filter(|len| *len > 0),
            }),
        });
    }
}

#[cfg(all(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
fn enrich_jpeg2000_info_from_openjpeg(path: &Path, info: &mut Jpeg2000Info) {
    let Ok(openjpeg) = gigatiff_core::openjpeg_ffi::read_info(path) else {
        return;
    };
    info.components
        .get_or_insert(openjpeg.components.len() as u32);
    if let Some(component) = openjpeg.components.first() {
        info.precision.get_or_insert(component.precision);
    }
    if openjpeg.icc_profile_len > 0 {
        info.icc_profile_len.get_or_insert(openjpeg.icc_profile_len);
    }
}

#[cfg(all(feature = "jpeg2000-grok", not(feature = "jpeg2000-openjpeg-ffi")))]
fn enrich_jpeg2000_info_from_openjpeg(_path: &Path, _info: &mut Jpeg2000Info) {}

#[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
fn select_jp2_backend(
    state: &AppState,
    info: &ServerImageInfo,
) -> Result<Option<Jp2RenderBackend>> {
    let ServerImageSource::Jpeg2000(jpeg2000) = &info.source else {
        return Ok(None);
    };

    match state.jp2_backend {
        Jp2BackendPolicy::Auto => {
            if should_use_openjpeg_fallback(jpeg2000) {
                return openjpeg_backend()
                    .ok_or_else(|| {
                        anyhow!("JPEG2000 auto selected OpenJPEG, but it is not enabled")
                    })
                    .map(Some);
            }
            grok_backend()
                .or_else(openjpeg_backend)
                .ok_or_else(|| anyhow!("no JPEG2000 backend is enabled"))
                .map(Some)
        }
        #[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
        Jp2BackendPolicy::Grok => grok_backend()
            .ok_or_else(|| anyhow!("--jp2-backend grok requires a Grok JPEG2000 feature"))
            .map(Some),
        #[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
        Jp2BackendPolicy::Openjpeg => openjpeg_backend()
            .ok_or_else(|| anyhow!("--jp2-backend openjpeg requires jpeg2000-openjpeg-ffi"))
            .map(Some),
    }
}

#[cfg(not(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi")))]
fn select_jp2_backend(
    _state: &AppState,
    _info: &ServerImageInfo,
) -> Result<Option<Jp2RenderBackend>> {
    Ok(None)
}

#[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
fn grok_backend() -> Option<Jp2RenderBackend> {
    #[cfg(feature = "jpeg2000-grok-ffi")]
    {
        return Some(Jp2RenderBackend::GrokFfi);
    }

    #[cfg(all(not(feature = "jpeg2000-grok-ffi"), feature = "jpeg2000-grok"))]
    {
        return Some(Jp2RenderBackend::GrokCli);
    }

    #[cfg(not(feature = "jpeg2000-grok"))]
    {
        None
    }
}

fn openjpeg_backend() -> Option<Jp2RenderBackend> {
    #[cfg(feature = "jpeg2000-openjpeg-ffi")]
    {
        return Some(Jp2RenderBackend::OpenJpegFfi);
    }

    #[cfg(not(feature = "jpeg2000-openjpeg-ffi"))]
    {
        None
    }
}

fn fallback_cache_backend(
    policy: Jp2BackendPolicy,
    backend: Jp2RenderBackend,
) -> Option<Jp2RenderBackend> {
    if policy == Jp2BackendPolicy::Auto && is_grok_backend(backend) {
        openjpeg_backend()
    } else {
        None
    }
}

fn is_grok_backend(backend: Jp2RenderBackend) -> bool {
    matches!(
        backend,
        Jp2RenderBackend::GrokCli | Jp2RenderBackend::GrokFfi
    )
}

#[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
fn jpeg2000_supports_region_tiles(info: &Jpeg2000Info) -> bool {
    #[cfg(feature = "jpeg2000-openjpeg-ffi")]
    {
        let _ = info;
        return true;
    }

    #[cfg(all(not(feature = "jpeg2000-openjpeg-ffi"), feature = "jpeg2000-grok-ffi"))]
    {
        return info.tile_width.is_some() && info.tile_height.is_some();
    }

    #[cfg(all(
        not(feature = "jpeg2000-openjpeg-ffi"),
        not(feature = "jpeg2000-grok-ffi")
    ))]
    {
        let has_small_tiles = info.tile_width.unwrap_or_default() < 4096
            && info.tile_height.unwrap_or_default() < 4096;
        let has_high_precision = info.precision.unwrap_or_default() >= 16;
        has_small_tiles || has_high_precision
    }
}

#[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
fn render_jpeg2000_preview(
    path: &Path,
    info: &Jpeg2000Info,
    rect: Rect,
    out_width: u32,
    out_height: u32,
    backend: Jp2RenderBackend,
    allow_grok_fallback: bool,
    openjpeg_threads: usize,
) -> Result<(PreviewBitmap, Option<Jp2RenderBackend>)> {
    let _ = info;
    let render = match backend {
        Jp2RenderBackend::GrokCli => {
            render_jpeg2000_grok_cli_preview(path, rect, out_width, out_height)
                .with_context(|| "rendering JPEG2000 through Grok CLI")
        }
        Jp2RenderBackend::GrokFfi => {
            render_jpeg2000_grok_ffi_preview(path, rect, out_width, out_height)
                .with_context(|| "rendering JPEG2000 through Grok FFI")
        }
        Jp2RenderBackend::OpenJpegFfi => {
            render_jpeg2000_openjpeg_preview(path, rect, out_width, out_height, openjpeg_threads)
                .with_context(|| "rendering JPEG2000 through OpenJPEG FFI")
        }
    };

    match render {
        Ok(bitmap) => Ok((bitmap, Some(backend))),
        Err(err) if allow_grok_fallback && is_grok_backend(backend) => {
            let Some(fallback) = openjpeg_backend() else {
                return Err(err);
            };
            let bitmap = render_jpeg2000_openjpeg_preview(
                path,
                rect,
                out_width,
                out_height,
                openjpeg_threads,
            )
            .with_context(|| format!("{err}; fallback to OpenJPEG FFI also failed"))?;
            Ok((bitmap, Some(fallback)))
        }
        Err(err) => Err(err),
    }
}

#[cfg(all(feature = "jpeg2000-grok", not(feature = "jpeg2000-grok-ffi")))]
fn render_jpeg2000_grok_cli_preview(
    path: &Path,
    rect: Rect,
    out_width: u32,
    out_height: u32,
) -> Result<PreviewBitmap> {
    let total_start = Instant::now();
    let temp_path = std::env::temp_dir().join(format!(
        "gigatiff-grok-{}-{}.ppm",
        std::process::id(),
        UNIX_EPOCH.elapsed().unwrap_or_default().as_nanos()
    ));

    let mut command = Command::new(grok_decompress_command());
    command
        .arg("-i")
        .arg(path)
        .arg("-o")
        .arg(&temp_path)
        .arg("-d")
        .arg(format!(
            "{},{},{},{}",
            rect.x,
            rect.y,
            rect.x.saturating_add(rect.width),
            rect.y.saturating_add(rect.height)
        ))
        .arg("-f");

    let reduce = grok_reduce_factor(rect, out_width, out_height);
    if reduce > 0 {
        command.arg("-r").arg(reduce.to_string());
    }

    let decode_start = Instant::now();
    let output = command
        .output()
        .with_context(|| "running grk_decompress for JPEG2000 region")?;
    let decode = decode_start.elapsed();

    if !output.status.success() {
        let _ = fs::remove_file(&temp_path);
        bail!(
            "grk_decompress failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let read_start = Instant::now();
    let bytes = fs::read(&temp_path)
        .with_context(|| format!("reading Grok output {}", temp_path.display()))?;
    let _ = fs::remove_file(&temp_path);
    let (decoded_width, decoded_height, rgba) = parse_pnm_rgba(&bytes)?;
    let read = read_start.elapsed();

    let convert_start = Instant::now();
    let rgba = if decoded_width == out_width && decoded_height == out_height {
        rgba
    } else {
        resize_nearest_rgba(&rgba, decoded_width, decoded_height, out_width, out_height)
    };
    let convert = convert_start.elapsed();

    Ok(PreviewBitmap {
        width: out_width,
        height: out_height,
        rgba,
        source: "grok-jpeg2000",
        decoded_chunks: 1,
        stats: RenderStats {
            total: total_start.elapsed(),
            read,
            convert,
            decode,
            ..RenderStats::default()
        },
    })
}

#[cfg(all(
    any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"),
    not(all(feature = "jpeg2000-grok", not(feature = "jpeg2000-grok-ffi")))
))]
fn render_jpeg2000_grok_cli_preview(
    _path: &Path,
    _rect: Rect,
    _out_width: u32,
    _out_height: u32,
) -> Result<PreviewBitmap> {
    bail!("Grok CLI JPEG2000 backend is not enabled")
}

#[cfg(feature = "jpeg2000-grok-ffi")]
fn render_jpeg2000_grok_ffi_preview(
    path: &Path,
    rect: Rect,
    out_width: u32,
    out_height: u32,
) -> Result<PreviewBitmap> {
    let total_start = Instant::now();
    let ffi = gigatiff_core::grok_ffi::render_region(path, rect, out_width, out_height)?;
    let convert_start = Instant::now();
    let rgba = if ffi.width == out_width && ffi.height == out_height {
        ffi.rgba
    } else {
        resize_nearest_rgba(&ffi.rgba, ffi.width, ffi.height, out_width, out_height)
    };
    let resize = convert_start.elapsed();

    Ok(PreviewBitmap {
        width: out_width,
        height: out_height,
        rgba,
        source: "grok-ffi-jpeg2000",
        decoded_chunks: 1,
        stats: RenderStats {
            total: total_start.elapsed(),
            read: Duration::ZERO,
            convert: ffi.convert + resize,
            decode: ffi.decode,
            ..RenderStats::default()
        },
    })
}

#[cfg(all(
    any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"),
    not(feature = "jpeg2000-grok-ffi")
))]
fn render_jpeg2000_grok_ffi_preview(
    _path: &Path,
    _rect: Rect,
    _out_width: u32,
    _out_height: u32,
) -> Result<PreviewBitmap> {
    bail!("Grok FFI JPEG2000 backend is not enabled")
}

#[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
fn should_use_openjpeg_fallback(info: &Jpeg2000Info) -> bool {
    info.tile_width.unwrap_or_default() >= 4096 || info.tile_height.unwrap_or_default() >= 4096
}

#[cfg(feature = "jpeg2000-openjpeg-ffi")]
fn render_jpeg2000_openjpeg_preview(
    path: &Path,
    rect: Rect,
    out_width: u32,
    out_height: u32,
    openjpeg_threads: usize,
) -> Result<PreviewBitmap> {
    let total_start = Instant::now();
    let ffi = gigatiff_core::openjpeg_ffi::render_region(
        path,
        rect,
        out_width,
        out_height,
        openjpeg_threads.min(i32::MAX as usize) as i32,
    )?;
    let convert_start = Instant::now();
    let rgba = if ffi.width == out_width && ffi.height == out_height {
        ffi.rgba
    } else {
        resize_nearest_rgba(&ffi.rgba, ffi.width, ffi.height, out_width, out_height)
    };
    let resize = convert_start.elapsed();

    Ok(PreviewBitmap {
        width: out_width,
        height: out_height,
        rgba,
        source: "openjpeg-ffi-jpeg2000",
        decoded_chunks: 1,
        stats: RenderStats {
            total: total_start.elapsed(),
            read: Duration::ZERO,
            convert: ffi.convert + resize,
            decode: ffi.decode,
            ..RenderStats::default()
        },
    })
}

#[cfg(all(
    any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"),
    not(feature = "jpeg2000-openjpeg-ffi")
))]
fn render_jpeg2000_openjpeg_preview(
    _path: &Path,
    _rect: Rect,
    _out_width: u32,
    _out_height: u32,
    _openjpeg_threads: usize,
) -> Result<PreviewBitmap> {
    bail!("OpenJPEG FFI JPEG2000 backend is not enabled")
}

#[cfg(feature = "jpeg2000-grok")]
fn grok_reduce_factor(rect: Rect, out_width: u32, out_height: u32) -> u32 {
    let width_scale = rect.width.max(1) / out_width.max(1);
    let height_scale = rect.height.max(1) / out_height.max(1);
    let mut scale = width_scale.min(height_scale);
    let mut reduce = 0;
    while scale >= 2 && reduce < 8 {
        reduce += 1;
        scale /= 2;
    }
    reduce
}

#[cfg(feature = "jpeg2000-grok")]
#[derive(Debug, Clone)]
struct ParsedGrokInfo {
    width: u32,
    height: u32,
    jpeg2000: Jpeg2000Info,
}

#[cfg(feature = "jpeg2000-grok")]
fn parse_grok_dump_info(text: &str) -> Result<ParsedGrokInfo> {
    let fields = grok_numeric_fields(text);
    let x0 = field_value(&fields, &["x0"]).unwrap_or(0);
    let y0 = field_value(&fields, &["y0"]).unwrap_or(0);
    let x1 = field_value(&fields, &["x1"]);
    let y1 = field_value(&fields, &["y1"]);
    let width = x1
        .and_then(|x1| x1.checked_sub(x0))
        .or_else(|| field_value(&fields, &["width"]));
    let height = y1
        .and_then(|y1| y1.checked_sub(y0))
        .or_else(|| field_value(&fields, &["height"]));
    let width = width
        .filter(|value| *value > 0)
        .ok_or_else(|| anyhow!("could not parse JPEG2000 width from grk_dump output"))?;
    let height = height
        .filter(|value| *value > 0)
        .ok_or_else(|| anyhow!("could not parse JPEG2000 height from grk_dump output"))?;

    Ok(ParsedGrokInfo {
        width,
        height,
        jpeg2000: Jpeg2000Info {
            components: field_value(&fields, &["numcomps", "components", "num_components"]),
            precision: field_value(&fields, &["prec", "precision", "bpp"]),
            tile_width: field_value(&fields, &["tdx", "tile_width"]),
            tile_height: field_value(&fields, &["tdy", "tile_height"]),
            progression_order: grok_progression_order(text),
            resolution_levels: field_value(
                &fields,
                &["numresolutions", "numresolution", "resolutions", "levels"],
            ),
            icc_profile_len: field_value(
                &fields,
                &["icc_profile_len", "iccprofilelen", "icc_profile_length"],
            )
            .filter(|len| *len > 0),
        },
    })
}

#[cfg(feature = "jpeg2000-grok")]
fn grok_progression_order(text: &str) -> Option<String> {
    let lowered = text.to_ascii_lowercase();
    ["lrcp", "rlcp", "rpcl", "pcrl", "cprl"]
        .iter()
        .find(|order| {
            lowered
                .split(|ch: char| !ch.is_ascii_alphanumeric())
                .any(|token| token == **order)
        })
        .map(|order| order.to_ascii_uppercase())
}

#[cfg(feature = "jpeg2000-grok")]
fn grok_numeric_fields(text: &str) -> HashMap<String, u32> {
    let mut fields = HashMap::new();
    for line in text.lines() {
        let tokens = grok_line_tokens(line);
        for pair in tokens.windows(2) {
            if let Ok(value) = pair[1].parse::<u32>() {
                fields.entry(pair[0].clone()).or_insert(value);
            }
        }
    }
    fields
}

#[cfg(feature = "jpeg2000-grok")]
fn grok_line_tokens(line: &str) -> Vec<String> {
    line.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

#[cfg(feature = "jpeg2000-grok")]
fn field_value(fields: &HashMap<String, u32>, names: &[&str]) -> Option<u32> {
    names.iter().find_map(|name| fields.get(*name).copied())
}

#[cfg(feature = "jpeg2000-grok")]
fn parse_pnm_rgba(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut cursor = PnmCursor { bytes, pos: 0 };
    let magic = cursor.next_token()?;
    let samples_per_pixel = match magic.as_slice() {
        b"P5" => 1usize,
        b"P6" => 3usize,
        _ => bail!("unsupported Grok PNM output format"),
    };
    let width = parse_ascii_u32(&cursor.next_token()?, "PNM width")?;
    let height = parse_ascii_u32(&cursor.next_token()?, "PNM height")?;
    let max_value = parse_ascii_u32(&cursor.next_token()?, "PNM max value")?;
    if width == 0 || height == 0 || max_value == 0 {
        bail!("invalid PNM dimensions or max value");
    }

    cursor.consume_raster_separator()?;
    let bytes_per_sample = if max_value <= 255 { 1usize } else { 2usize };
    let sample_count = width as usize * height as usize * samples_per_pixel;
    let expected_len = sample_count
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| anyhow!("PNM raster is too large"))?;
    if cursor.bytes.len().saturating_sub(cursor.pos) < expected_len {
        bail!("truncated PNM raster");
    }

    let raster = &cursor.bytes[cursor.pos..cursor.pos + expected_len];
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for pixel in 0..(width as usize * height as usize) {
        let sample_at = |sample: usize| -> u8 {
            let index = (pixel * samples_per_pixel + sample) * bytes_per_sample;
            let value = if bytes_per_sample == 1 {
                raster[index] as u32
            } else {
                u16::from_be_bytes([raster[index], raster[index + 1]]) as u32
            };
            ((value * 255) / max_value).min(255) as u8
        };

        if samples_per_pixel == 1 {
            let gray = sample_at(0);
            rgba.extend_from_slice(&[gray, gray, gray, 255]);
        } else {
            rgba.extend_from_slice(&[sample_at(0), sample_at(1), sample_at(2), 255]);
        }
    }

    Ok((width, height, rgba))
}

#[cfg(feature = "jpeg2000-grok")]
struct PnmCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

#[cfg(feature = "jpeg2000-grok")]
impl<'a> PnmCursor<'a> {
    fn next_token(&mut self) -> Result<Vec<u8>> {
        self.skip_whitespace_and_comments();
        let start = self.pos;
        while self.pos < self.bytes.len() && !self.bytes[self.pos].is_ascii_whitespace() {
            if self.bytes[self.pos] == b'#' {
                break;
            }
            self.pos += 1;
        }
        if self.pos == start {
            bail!("missing PNM token");
        }
        Ok(self.bytes[start..self.pos].to_vec())
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
                self.pos += 1;
            }
            if self.pos < self.bytes.len() && self.bytes[self.pos] == b'#' {
                while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
                    self.pos += 1;
                }
                continue;
            }
            break;
        }
    }

    fn consume_raster_separator(&mut self) -> Result<()> {
        if self.pos >= self.bytes.len() || !self.bytes[self.pos].is_ascii_whitespace() {
            bail!("missing PNM raster separator");
        }
        self.pos += 1;
        Ok(())
    }
}

#[cfg(feature = "jpeg2000-grok")]
fn parse_ascii_u32(bytes: &[u8], label: &str) -> Result<u32> {
    let value = std::str::from_utf8(bytes).with_context(|| format!("invalid {label}"))?;
    value
        .parse()
        .with_context(|| format!("invalid {label} '{value}'"))
}

#[cfg(feature = "jpeg2000-grok")]
fn grok_dump_command() -> OsString {
    std::env::var_os("GIGATIFF_GROK_DUMP").unwrap_or_else(|| OsString::from("grk_dump"))
}

#[cfg(feature = "jpeg2000-grok")]
fn grok_decompress_command() -> OsString {
    std::env::var_os("GIGATIFF_GROK_DECOMPRESS").unwrap_or_else(|| OsString::from("grk_decompress"))
}

async fn load_cached_info(state: &AppState, path: &Path) -> Result<Arc<ServerImageInfo>> {
    let state = state.clone();
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || load_cached_info_blocking(&state, &path))
        .await
        .map_err(|err| anyhow!("info task failed: {err}"))?
}

fn load_cached_info_blocking(state: &AppState, path: &Path) -> Result<Arc<ServerImageInfo>> {
    if let Some(info) = state
        .info_cache
        .lock()
        .map_err(|_| anyhow!("info cache lock poisoned"))?
        .get(path)
        .cloned()
    {
        return Ok(info);
    }

    let info = Arc::new(load_server_image_info(path)?);
    state
        .info_cache
        .lock()
        .map_err(|_| anyhow!("info cache lock poisoned"))?
        .insert(path.to_path_buf(), Arc::clone(&info));
    Ok(info)
}

fn load_server_image_info(path: &Path) -> Result<ServerImageInfo> {
    if is_tiff_path(path) {
        return Ok(ServerImageInfo::from_tiff(load_info(path)?));
    }

    if is_jpeg2000_path(path) {
        #[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
        {
            return load_jpeg2000_info(path);
        }
        #[cfg(not(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi")))]
        {
            bail!("JPEG2000 support requires a JPEG2000 Cargo feature");
        }
    }

    bail!("unsupported image format");
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
        if !is_supported_image_path(&path) {
            continue;
        }
        let id = relative_id(root, &path)?;
        let encoded_id = encode_id(&id);
        let modified_unix = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());
        images.push(ImageListItem {
            label: id.clone(),
            modified_unix,
            id,
            info_url: format!("/iiif/3/{encoded_id}/info.json"),
            metadata_url: format!("/api/info/{encoded_id}"),
            viewer_url: format!("/viewer/{encoded_id}"),
        });
    }
    Ok(())
}

fn resolve_id(root: &Path, id: &str) -> Result<PathBuf> {
    validate_identifier_text(id)?;
    let decoded = percent_decode_str(id)
        .decode_utf8()
        .with_context(|| format!("decoding identifier '{id}'"))?;
    validate_identifier_text(&decoded)?;
    let mut relative = PathBuf::new();
    for segment in decoded.split('/') {
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.contains('\\')
            || segment.chars().any(char::is_control)
        {
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
    if !is_supported_image_path(&path) {
        bail!("identifier does not point to a supported image file");
    }
    if !path.exists() {
        bail!("image not found");
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("opening image {}", path.display()))?;
    if !canonical.starts_with(root) {
        bail!("image path escapes configured root");
    }
    if !is_supported_image_path(&canonical) {
        bail!("identifier does not point to a supported image file");
    }
    Ok(canonical)
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

fn is_jpeg2000_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "jp2" | "j2k" | "j2c" | "jpc"
            )
        })
        .unwrap_or(false)
}

fn is_supported_image_path(path: &Path) -> bool {
    is_tiff_path(path) || is_jpeg2000_path(path)
}

fn should_advertise_tiles(info: &ServerImageInfo) -> bool {
    match &info.source {
        ServerImageSource::Tiff(_) => true,
        #[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
        ServerImageSource::Jpeg2000(jpeg2000) => jpeg2000_supports_region_tiles(jpeg2000),
    }
}

fn advertised_tile_size(configured_tile_size: u32, _info: &ServerImageInfo) -> (u32, u32) {
    #[cfg(any(feature = "jpeg2000-grok", feature = "jpeg2000-openjpeg-ffi"))]
    if configured_tile_size == 512 {
        if let ServerImageSource::Jpeg2000(jpeg2000) = &_info.source {
            if should_use_openjpeg_fallback(jpeg2000) {
                return (1024, 1024);
            }
        }
    }

    (configured_tile_size, configured_tile_size)
}

fn error_response(status: StatusCode, err: anyhow::Error) -> Response {
    if std::env::var_os("GIGATIFF_VERBOSE_ERRORS").is_none() {
        return (status, err.to_string()).into_response();
    }
    let mut message = err.to_string();
    for cause in err.chain().skip(1) {
        message.push_str("\ncaused by: ");
        message.push_str(&cause.to_string());
    }
    (status, message).into_response()
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
        let rect = parse_region(&request.region, info.width, info.height).unwrap();
        let (out_width, out_height) = parse_size(&request.size, rect, 16_777_216).unwrap();
        let rotation = parse_rotation(&request.rotation).unwrap();

        assert_eq!(
            canonical_image_path(
                &request,
                &rect,
                info.width,
                info.height,
                out_width,
                out_height,
                rotation
            ),
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
            parse_region("square", info.width, info.height).unwrap(),
            Rect {
                x: 1024,
                y: 0,
                width: 2048,
                height: 2048,
            }
        );
        assert_eq!(
            parse_region("pct:25,25,50,50", info.width, info.height).unwrap(),
            Rect {
                x: 1024,
                y: 512,
                width: 2048,
                height: 1024,
            }
        );
        assert_eq!(
            parse_region("pct:0,0,100,100", info.width, info.height).unwrap(),
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
            parse_region("4000,2000,500,500", info.width, info.height).unwrap(),
            Rect {
                x: 4000,
                y: 2000,
                width: 96,
                height: 48,
            }
        );
        assert!(parse_region("4096,0,1,1", info.width, info.height).is_err());
        assert!(parse_region("0,2048,1,1", info.width, info.height).is_err());
        assert!(parse_region("0,0,0,1", info.width, info.height).is_err());
        assert!(parse_region("pct:100,0,10,10", info.width, info.height).is_err());
        assert!(parse_region("pct:0,0,0,10", info.width, info.height).is_err());
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
    fn rejects_upscale_above_configured_limit() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 500,
        };

        assert!(validate_upscale(4000, 2000, rect, 4.0).is_ok());
        assert!(validate_upscale(4001, 2000, rect, 4.0).is_err());
        assert!(validate_upscale(4000, 2001, rect, 4.0).is_err());
    }

    #[test]
    fn rejects_overlong_or_control_iiif_tokens() {
        let long_tail = "a".repeat(MAX_IIIF_TAIL_LEN + 1);
        assert!(parse_iiif_image_request(&long_tail).is_err());
        assert!(parse_iiif_image_request("map.tif/full/max/0/default.webp").is_ok());
        assert!(parse_iiif_image_request("map.tif/full/max/\n/default.webp").is_err());
        assert!(parse_iiif_image_request("map.tif/full/pct:10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000/0/default.webp").is_err());
    }

    #[test]
    fn rate_limiter_uses_client_window() {
        let state = dummy_state(unique_temp_dir("rate-limit"));
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("192.0.2.10"));
        let state = AppState {
            rate_limit_per_minute: 2,
            ..state
        };

        assert!(check_rate_limit(&state, &headers));
        assert!(check_rate_limit(&state, &headers));
        assert!(!check_rate_limit(&state, &headers));
    }

    #[test]
    fn probes_are_classified_and_not_rate_limited() {
        assert_eq!(classify_route("/healthz"), "healthz");
        assert_eq!(classify_route("/readyz"), "readyz");
        assert!(!should_rate_limit_route("healthz"));
        assert!(!should_rate_limit_route("readyz"));
        assert!(!should_rate_limit_route("metrics"));
        assert!(should_rate_limit_route("iiif"));
    }

    #[test]
    fn readiness_probe_reports_disk_cache() {
        let temp = unique_temp_dir("readyz");
        let cache = temp.join("cache");
        fs::create_dir_all(&cache).unwrap();
        let mut state = dummy_state(cache);
        state.root = Arc::new(temp);

        let probe = build_readiness_probe(&state).unwrap();
        assert_eq!(probe.status, "ok");
        assert_eq!(probe.checks.len(), 2);
        assert_eq!(probe.checks[0].name, "image_root");
        assert_eq!(probe.checks[1].name, "response_cache");
        assert_eq!(probe.checks[1].status, "ok");
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
        let info = ServerImageInfo::from_tiff(dummy_info());

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
        let full_rect = parse_region(&full.region, info.width, info.height).unwrap();
        let (full_width, full_height) = parse_size(&full.size, full_rect, 16_777_216).unwrap();
        let full_canonical = canonical_image_path(
            &full,
            &full_rect,
            info.width,
            info.height,
            full_width,
            full_height,
            parse_rotation(&full.rotation).unwrap(),
        );
        let equivalent_rect =
            parse_region(&pixel_equivalent.region, info.width, info.height).unwrap();
        let (equivalent_width, equivalent_height) =
            parse_size(&pixel_equivalent.size, equivalent_rect, 16_777_216).unwrap();
        let equivalent_canonical = canonical_image_path(
            &pixel_equivalent,
            &equivalent_rect,
            info.width,
            info.height,
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
            None,
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
            None,
        )
        .unwrap();
        assert_eq!(full_path, equivalent_path);

        let changed = IiifImageRequest {
            size: "1024,".to_string(),
            ..full
        };
        let changed_rect = parse_region(&changed.region, info.width, info.height).unwrap();
        let (changed_width, changed_height) =
            parse_size(&changed.size, changed_rect, 16_777_216).unwrap();
        let changed_canonical = canonical_image_path(
            &changed,
            &changed_rect,
            info.width,
            info.height,
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
            None,
        )
        .unwrap();
        assert_ne!(full_path, changed_path);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn store_cached_response_round_trips_bytes() {
        let temp = unique_temp_dir("cache-store");
        let cache_path = temp.join("ab").join("abcdef.webp");
        store_cached_response_disk(&cache_path, b"cached bytes").unwrap();
        assert_eq!(
            read_cached_response_disk(&cache_path, None)
                .unwrap()
                .unwrap(),
            b"cached bytes"
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn prune_response_cache_removes_old_files_until_under_limit() {
        let temp = unique_temp_dir("cache-prune");
        store_cached_response_disk(&temp.join("aa").join("one.webp"), b"aaaa").unwrap();
        store_cached_response_disk(&temp.join("bb").join("two.webp"), b"bbbb").unwrap();

        let report = prune_response_cache(&temp, 4, None).unwrap();

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
    fn prune_response_cache_removes_expired_files_before_size_check() {
        let temp = unique_temp_dir("cache-ttl");
        store_cached_response_disk(&temp.join("aa").join("one.webp"), b"aaaa").unwrap();

        let report = prune_response_cache(&temp, 1024, Some(Duration::ZERO)).unwrap();

        assert_eq!(report.removed_files, 1);
        assert_eq!(report.removed_bytes, 4);
        let mut files = Vec::new();
        let mut total_bytes = 0;
        collect_cache_files(&temp, &mut files, &mut total_bytes).unwrap();
        assert_eq!(files.len(), 0);
        assert_eq!(total_bytes, 0);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn cache_stats_and_purge_report_size_and_removals() {
        let temp = unique_temp_dir("cache-stats");
        let state = dummy_state(temp.clone());
        store_cached_response_disk(&temp.join("aa").join("one.webp"), b"aaaa").unwrap();
        store_cached_response_disk(&temp.join("bb").join("two.webp"), b"bbbbbb").unwrap();

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

    #[test]
    fn purge_response_cache_for_identifier_only_removes_matching_source_prefix() {
        let temp = unique_temp_dir("cache-purge-id");
        let state = dummy_state(temp.join("cache"));
        let source = temp.join("map.tif");
        let other = temp.join("other.tif");
        fs::create_dir_all(&*state.cache_dir).unwrap();
        fs::write(&source, b"source").unwrap();
        fs::write(&other, b"other").unwrap();

        let source_prefix = source_cache_prefix(&source);
        let other_prefix = source_cache_prefix(&other);
        store_cached_response_disk(
            &state
                .cache_dir
                .join(&source_prefix[0..2])
                .join(format!("{source_prefix}-one.webp")),
            b"one",
        )
        .unwrap();
        store_cached_response_disk(
            &state
                .cache_dir
                .join(&other_prefix[0..2])
                .join(format!("{other_prefix}-two.webp")),
            b"two",
        )
        .unwrap();

        let stats = purge_response_cache_for_identifier(&state, &source).unwrap();

        assert_eq!(stats.last_prune.removed_files, 1);
        assert_eq!(stats.file_count, 1);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn prewarm_requests_include_thumbnail_and_first_tile() {
        let info = ServerImageInfo::from_tiff(dummy_info());
        let requests = prewarm_requests("map.tif", &info, 512);

        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].region, "full");
        assert_eq!(requests[0].size, "!512,512");
        assert_eq!(requests[1].region, "0,0,512,512");
        assert_eq!(requests[1].format.extension(), "webp");
    }

    #[test]
    fn prometheus_metrics_include_cache_render_and_jpeg2000_series() {
        let temp = unique_temp_dir("metrics");
        let state = dummy_state(temp);
        state.metrics.cache_hits_total.store(3, Ordering::Relaxed);
        state.metrics.cache_misses_total.store(1, Ordering::Relaxed);
        state
            .metrics
            .jp2_grok_to_openjpeg_fallbacks_total
            .store(2, Ordering::Relaxed);

        let metrics = build_prometheus_metrics(&state).unwrap();

        assert!(metrics.contains("gigatiff_http_requests_total"));
        assert!(metrics.contains("gigatiff_cache_hit_ratio 0.750000"));
        assert!(metrics.contains("gigatiff_render_queue_available_permits"));
        assert!(metrics.contains("gigatiff_jp2_grok_to_openjpeg_fallbacks_total 2"));
    }

    #[test]
    fn supported_image_paths_include_tiff_and_jpeg2000() {
        assert!(is_supported_image_path(Path::new("map.tif")));
        assert!(is_supported_image_path(Path::new("map.TIFF")));
        assert!(is_supported_image_path(Path::new("scan.jp2")));
        assert!(is_supported_image_path(Path::new("scan.j2k")));
        assert!(!is_supported_image_path(Path::new("scan.png")));
    }

    #[test]
    fn advertised_tile_size_keeps_configured_tiff_size() {
        let info = ServerImageInfo::from_tiff(dummy_info());
        assert_eq!(advertised_tile_size(512, &info), (512, 512));
        assert_eq!(advertised_tile_size(1024, &info), (1024, 1024));
    }

    #[test]
    fn metadata_response_includes_tiff_technical_fields() {
        let info = ServerImageInfo::from_tiff(dummy_info());

        let metadata = build_metadata_response(
            "folder/map.tif",
            "folder%2Fmap.tif",
            "http://example.test",
            Path::new("folder/map.tif"),
            &info,
        );

        assert_eq!(metadata["api"], "gigatiff-metadata-v1");
        assert_eq!(metadata["source_type"], "tiff");
        assert_eq!(metadata["technical"]["format"], "TIFF");
        assert_eq!(metadata["technical"]["compression"]["name"], "none");
        assert_eq!(metadata["technical"]["resolution"]["unit"], "inch");
        assert_eq!(metadata["color"]["icc"]["present"], false);
        assert_eq!(
            metadata["links"]["metadata"],
            "http://example.test/api/info/folder%2Fmap.tif"
        );
    }

    #[cfg(feature = "jpeg2000-grok")]
    #[test]
    fn parses_grok_dump_metadata() {
        let dump = r#"
            image {
              x0=0, y0=0, x1=4096, y1=2048
              numcomps=3
              tdx=4096, tdy=4096
              numresolutions=6
              progression order: RPCL
            }
            comp 0: prec=12
        "#;

        let info = parse_grok_dump_info(dump).unwrap();

        assert_eq!(info.width, 4096);
        assert_eq!(info.height, 2048);
        assert_eq!(info.jpeg2000.components, Some(3));
        assert_eq!(info.jpeg2000.precision, Some(12));
        assert_eq!(info.jpeg2000.tile_width, Some(4096));
        assert_eq!(info.jpeg2000.tile_height, Some(4096));
        assert_eq!(info.jpeg2000.resolution_levels, Some(6));
        assert_eq!(info.jpeg2000.progression_order.as_deref(), Some("RPCL"));
    }

    #[cfg(feature = "jpeg2000-grok")]
    #[test]
    fn jpeg2000_metadata_controls_iiif_tile_advertisement() {
        #[cfg(not(feature = "jpeg2000-grok-ffi"))]
        {
            let info = ServerImageInfo {
                width: 4096,
                height: 2048,
                source: ServerImageSource::Jpeg2000(Jpeg2000Info {
                    tile_width: Some(4096),
                    tile_height: Some(4096),
                    precision: Some(8),
                    ..Jpeg2000Info::default()
                }),
            };
            assert!(!should_advertise_tiles(&info));
        }

        let info = ServerImageInfo {
            width: 4096,
            height: 2048,
            source: ServerImageSource::Jpeg2000(Jpeg2000Info {
                tile_width: Some(1024),
                tile_height: Some(1024),
                precision: Some(8),
                ..Jpeg2000Info::default()
            }),
        };
        assert!(should_advertise_tiles(&info));
    }

    #[cfg(feature = "jpeg2000-grok-ffi")]
    #[test]
    fn jpeg2000_ffi_advertises_large_tiles_for_full_resolution_fallback() {
        let info = ServerImageInfo {
            width: 4096,
            height: 2048,
            source: ServerImageSource::Jpeg2000(Jpeg2000Info {
                tile_width: Some(4096),
                tile_height: Some(4096),
                precision: Some(8),
                ..Jpeg2000Info::default()
            }),
        };
        assert!(should_advertise_tiles(&info));
    }

    #[cfg(any(feature = "jpeg2000-grok-ffi", feature = "jpeg2000-openjpeg-ffi"))]
    #[test]
    fn large_tile_jpeg2000_uses_larger_default_advertised_tiles() {
        let info = ServerImageInfo {
            width: 41174,
            height: 29077,
            source: ServerImageSource::Jpeg2000(Jpeg2000Info {
                tile_width: Some(4096),
                tile_height: Some(4096),
                precision: Some(8),
                ..Jpeg2000Info::default()
            }),
        };
        assert_eq!(advertised_tile_size(512, &info), (1024, 1024));
        assert_eq!(advertised_tile_size(2048, &info), (2048, 2048));
    }

    #[cfg(feature = "jpeg2000-grok")]
    #[test]
    fn jpeg2000_region_tile_support_prefers_small_tiles_or_high_precision() {
        assert!(jpeg2000_supports_region_tiles(&Jpeg2000Info {
            tile_width: Some(1024),
            tile_height: Some(1024),
            precision: Some(8),
            ..Jpeg2000Info::default()
        }));
        assert!(jpeg2000_supports_region_tiles(&Jpeg2000Info {
            tile_width: Some(4096),
            tile_height: Some(4096),
            precision: Some(16),
            ..Jpeg2000Info::default()
        }));
        let large_8bit = Jpeg2000Info {
            tile_width: Some(4096),
            tile_height: Some(4096),
            precision: Some(8),
            ..Jpeg2000Info::default()
        };
        #[cfg(feature = "jpeg2000-grok-ffi")]
        assert!(jpeg2000_supports_region_tiles(&large_8bit));
        #[cfg(not(feature = "jpeg2000-grok-ffi"))]
        assert!(!jpeg2000_supports_region_tiles(&large_8bit));
    }

    #[cfg(feature = "jpeg2000-grok")]
    #[test]
    fn parses_pnm_rgb_and_grayscale_to_rgba() {
        let rgb = b"P6\n2 1\n255\n\xff\x00\x00\x00\x80\xff";
        let (width, height, rgba) = parse_pnm_rgba(rgb).unwrap();
        assert_eq!((width, height), (2, 1));
        assert_eq!(rgba, vec![255, 0, 0, 255, 0, 128, 255, 255]);

        let gray_16 = b"P5\n1 1\n65535\n\x80\x00";
        let (width, height, rgba) = parse_pnm_rgba(gray_16).unwrap();
        assert_eq!((width, height), (1, 1));
        assert_eq!(rgba, vec![127, 127, 127, 255]);
    }

    fn dummy_state(cache_dir: PathBuf) -> AppState {
        AppState {
            root: Arc::new(PathBuf::from(".")),
            tile_size: 512,
            max_output_pixels: 1024 * 1024,
            max_chunk_mb: 256,
            quality: 85,
            backend: Backend::Auto,
            jp2_backend: Jp2BackendPolicy::Auto,
            openjpeg_threads: 1,
            cache_dir: Arc::new(cache_dir),
            cache_backend: ResponseCacheBackend::Disk,
            dragonfly_cache: None,
            cache_namespace: Arc::new("gigatiff-server-response-v10-test".to_string()),
            cache_max_bytes: 4096 * 1024 * 1024,
            cache_prune_interval: Duration::from_secs(60),
            cache_ttl: None,
            last_cache_prune: Arc::new(Mutex::new(stale_cache_prune_instant())),
            last_cache_prune_report: Arc::new(Mutex::new(CachePruneReport::default())),
            render_permits: Arc::new(Semaphore::new(4)),
            max_concurrent_renders_per_ip: 2,
            max_concurrent_renders_per_file: 2,
            ip_render_permits: Arc::new(Mutex::new(HashMap::new())),
            file_render_permits: Arc::new(Mutex::new(HashMap::new())),
            render_timeout: Duration::from_secs(120),
            max_upscale: 4.0,
            rate_limit_per_minute: 600,
            rate_limits: Arc::new(Mutex::new(HashMap::new())),
            info_cache: Arc::new(Mutex::new(HashMap::new())),
            metrics: Arc::new(AppMetrics::default()),
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
            x_resolution: Some(300.0),
            y_resolution: Some(300.0),
            resolution_unit: Some(2),
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
