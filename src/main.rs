use std::cmp::{max, min};
use std::collections::VecDeque;
use std::ffi::CString;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::os::raw::{c_char, c_int, c_uint, c_ushort, c_void};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use eframe::egui;
use lcms2::{DisallowCache, Flags, GlobalContext, Intent, PixelFormat, Profile, Transform};
use rayon::prelude::*;
use tiff::ColorType;
use tiff::decoder::{ChunkType, Decoder, DecodingResult, Limits};
use tiff::tags::Tag;

const PARALLEL_ROW_BATCH: usize = 32;
const GUI_TILE_SIZE: f32 = 384.0;
const GUI_PREFETCH_TILE_RADIUS: u32 = 1;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "GigaTIFF: a memory-conscious TIFF/BigTIFF viewer"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// TIFF/BigTIFF opened directly in the GUI when no subcommand is used.
    path: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print metadata without decoding the full image.
    Info { path: PathBuf },
    /// Open a small desktop viewer.
    Gui { path: Option<PathBuf> },
    /// Decode only the chunks intersecting a source rectangle and save a PNG preview.
    Preview {
        path: PathBuf,

        #[arg(long, default_value_t = 0)]
        x: u32,

        #[arg(long, default_value_t = 0)]
        y: u32,

        #[arg(long, default_value_t = 1024)]
        width: u32,

        #[arg(long, default_value_t = 1024)]
        height: u32,

        #[arg(long, default_value_t = 1024)]
        max_output: u32,

        #[arg(long, default_value = "preview.png")]
        out: PathBuf,

        /// Maximum decoded chunk allocation in MiB.
        #[arg(long, default_value_t = 256)]
        max_chunk_mb: usize,

        /// Pixel backend used for rendering.
        #[arg(long, value_enum, default_value_t = Backend::Auto)]
        backend: Backend,

        /// PNG compression used for preview export.
        #[arg(long, value_enum, default_value_t = PngCompression::Fast)]
        png_compression: PngCompression,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Backend {
    Auto,
    Libtiff,
    Rust,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum PngCompression {
    None,
    Fastest,
    Fast,
    Balanced,
    High,
}

impl PngCompression {
    fn to_png(self) -> png::Compression {
        match self {
            Self::None => png::Compression::NoCompression,
            Self::Fastest => png::Compression::Fastest,
            Self::Fast => png::Compression::Fast,
            Self::Balanced => png::Compression::Balanced,
            Self::High => png::Compression::High,
        }
    }
}

#[derive(Debug, Clone)]
struct ImageInfo {
    width: u32,
    height: u32,
    color_type: ColorType,
    chunk_type: ChunkType,
    chunk_width: u32,
    chunk_height: u32,
    chunk_count: u32,
    chunks_across: u32,
    compression: Option<u32>,
    bits_per_sample: Option<Vec<u16>>,
    samples_per_pixel: Option<u32>,
    planar_config: Option<u32>,
    photometric: Option<u32>,
    is_bigtiff: bool,
    little_endian: bool,
    rows_per_strip: Option<u32>,
    strip_offsets: Option<Vec<u64>>,
    icc_profile: Option<Arc<[u8]>>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Info { path }) => print_info(&path),
        Some(Command::Gui { path }) => run_gui(path),
        Some(Command::Preview {
            path,
            x,
            y,
            width,
            height,
            max_output,
            out,
            max_chunk_mb,
            backend,
            png_compression,
        }) => write_preview(
            &path,
            x,
            y,
            width,
            height,
            max_output,
            &out,
            max_chunk_mb,
            backend,
            png_compression,
        ),
        None => run_gui(cli.path),
    }
}

#[derive(Debug)]
struct PreviewBitmap {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    source: &'static str,
    decoded_chunks: u32,
    stats: RenderStats,
}

#[derive(Debug, Clone, Copy, Default)]
struct RenderStats {
    total: Duration,
    read: Duration,
    convert: Duration,
    decode: Duration,
    blit: Duration,
    scanline_cache_hits: u32,
    scanline_cache_misses: u32,
}

impl RenderStats {
    fn short_label(self) -> String {
        let mut label = format!(
            "total {:.1} ms, read {:.1} ms, convert {:.1} ms",
            ms(self.total),
            ms(self.read + self.decode),
            ms(self.convert + self.blit)
        );
        let cache_rows = self.scanline_cache_hits + self.scanline_cache_misses;
        if cache_rows > 0 {
            label.push_str(&format!(
                ", row cache {}/{}",
                self.scanline_cache_hits, cache_rows
            ));
        }
        label
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[derive(Debug)]
struct RenderedPixels {
    rgba: Vec<u8>,
    stats: RenderStats,
}

struct SamplingPlan {
    src_y: Vec<u32>,
    src_x: Vec<u32>,
    src_x_byte_offsets: Vec<usize>,
}

impl SamplingPlan {
    fn new(rect: Rect, out_width: u32, out_height: u32, bytes_per_pixel: usize) -> Self {
        let src_y = (0..out_height)
            .map(|oy| rect.y + ((oy as u64 * rect.height as u64) / out_height as u64) as u32)
            .collect();
        let src_x: Vec<u32> = (0..out_width)
            .map(|ox| rect.x + ((ox as u64 * rect.width as u64) / out_width as u64) as u32)
            .collect();
        let src_x_byte_offsets = src_x
            .iter()
            .map(|x| (x - rect.x) as usize * bytes_per_pixel)
            .collect();

        Self {
            src_y,
            src_x,
            src_x_byte_offsets,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewRequest {
    path: PathBuf,
    rect: Rect,
    max_output: u32,
    backend: Backend,
}

#[derive(Debug)]
struct RenderJob {
    request: PreviewRequest,
    info: ImageInfo,
    max_chunk_mb: usize,
    generation: u64,
}

#[derive(Debug)]
struct RenderResult {
    request: PreviewRequest,
    result: Result<PreviewBitmap>,
}

struct TileTexture {
    texture: egui::TextureHandle,
    width: u32,
    height: u32,
    source: &'static str,
    stats: RenderStats,
    bytes: usize,
}

struct TileTextureCache {
    entries: VecDeque<(PreviewRequest, Arc<TileTexture>)>,
    byte_limit: usize,
    bytes: usize,
}

struct VisibleTile {
    request: PreviewRequest,
    screen_rect: egui::Rect,
    uv_rect: egui::Rect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScanlineKey {
    path: PathBuf,
    y: u32,
    x: u32,
    width: u32,
    bytes_per_pixel: usize,
}

struct ScanlineCache {
    entries: VecDeque<(ScanlineKey, Arc<Vec<u8>>)>,
    byte_limit: usize,
    bytes: usize,
}

struct ViewerApp {
    path_input: String,
    path: Option<PathBuf>,
    info: Option<ImageInfo>,
    last_request: Option<PreviewRequest>,
    pending_request: Option<PreviewRequest>,
    render_tx: Sender<RenderJob>,
    render_rx: Receiver<RenderResult>,
    render_generation: u64,
    latest_generation: Arc<AtomicU64>,
    tile_cache: TileTextureCache,
    texture_serial: u64,
    status: String,
    source_label: String,
    center_x: f64,
    center_y: f64,
    view_width: f64,
    max_chunk_mb: usize,
    last_drag_delta: egui::Vec2,
}

fn run_gui(path: Option<PathBuf>) -> Result<()> {
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "GigaTIFF",
        native_options,
        Box::new(move |cc| Ok(Box::new(ViewerApp::new(path.clone(), cc.egui_ctx.clone())))),
    )
    .map_err(|err| anyhow!("GUI error: {err}"))
}

struct RenderCancel {
    latest_generation: Arc<AtomicU64>,
    generation: u64,
}

impl RenderCancel {
    fn check(&self) -> Result<()> {
        if self.latest_generation.load(Ordering::Relaxed) != self.generation {
            bail!("render cancelled");
        }
        Ok(())
    }
}

fn spawn_render_worker(
    ctx: egui::Context,
    latest_generation: Arc<AtomicU64>,
) -> (Sender<RenderJob>, Receiver<RenderResult>) {
    let (job_tx, job_rx) = mpsc::channel::<RenderJob>();
    let (result_tx, result_rx) = mpsc::channel::<RenderResult>();

    thread::spawn(move || {
        let mut scanline_cache = ScanlineCache::new(128 * 1024 * 1024);
        while let Ok(mut job) = job_rx.recv() {
            while let Ok(newer) = job_rx.try_recv() {
                job = newer;
            }

            let cancel = RenderCancel {
                latest_generation: Arc::clone(&latest_generation),
                generation: job.generation,
            };
            let result = render_preview(
                &job.request.path,
                &job.info,
                job.request.rect,
                job.request.max_output,
                job.max_chunk_mb,
                job.request.backend,
                Some(&cancel),
                Some(&mut scanline_cache),
            );

            if result_tx
                .send(RenderResult {
                    request: job.request,
                    result,
                })
                .is_err()
            {
                break;
            }
            ctx.request_repaint();
        }
    });

    (job_tx, result_rx)
}

impl TileTextureCache {
    fn new(byte_limit: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            byte_limit,
            bytes: 0,
        }
    }

    fn get(&mut self, request: &PreviewRequest) -> Option<Arc<TileTexture>> {
        let index = self
            .entries
            .iter()
            .position(|(cached_request, _)| cached_request == request)?;
        let entry = self.entries.remove(index)?;
        let tile = Arc::clone(&entry.1);
        self.entries.push_back(entry);
        Some(tile)
    }

    fn contains(&self, request: &PreviewRequest) -> bool {
        self.entries
            .iter()
            .any(|(cached_request, _)| cached_request == request)
    }

    fn insert(&mut self, request: PreviewRequest, tile: Arc<TileTexture>) {
        if let Some(index) = self
            .entries
            .iter()
            .position(|(cached_request, _)| cached_request == &request)
        {
            if let Some((_, old)) = self.entries.remove(index) {
                self.bytes = self.bytes.saturating_sub(old.bytes);
            }
        }

        self.bytes += tile.bytes;
        self.entries.push_back((request, tile));

        while self.bytes > self.byte_limit && self.entries.len() > 1 {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(old.bytes);
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

impl ScanlineCache {
    fn new(byte_limit: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            byte_limit,
            bytes: 0,
        }
    }

    fn get(&mut self, key: &ScanlineKey) -> Option<Arc<Vec<u8>>> {
        let index = self
            .entries
            .iter()
            .position(|(cached_key, _)| cached_key == key)?;
        let entry = self.entries.remove(index)?;
        let row = Arc::clone(&entry.1);
        self.entries.push_back(entry);
        Some(row)
    }

    fn insert(&mut self, key: ScanlineKey, row: Arc<Vec<u8>>) {
        if row.len() > self.byte_limit {
            return;
        }

        if let Some(index) = self
            .entries
            .iter()
            .position(|(cached_key, _)| cached_key == &key)
        {
            if let Some((_, old)) = self.entries.remove(index) {
                self.bytes = self.bytes.saturating_sub(old.len());
            }
        }

        self.bytes += row.len();
        self.entries.push_back((key, row));

        while self.bytes > self.byte_limit {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(old.len());
            } else {
                break;
            }
        }
    }
}

impl ViewerApp {
    fn new(path: Option<PathBuf>, ctx: egui::Context) -> Self {
        let fallback = PathBuf::from("mapa2.tif");
        let initial = path.or_else(|| fallback.exists().then_some(fallback));
        let latest_generation = Arc::new(AtomicU64::new(0));
        let (render_tx, render_rx) = spawn_render_worker(ctx, Arc::clone(&latest_generation));
        let mut app = Self {
            path_input: initial
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            path: None,
            info: None,
            last_request: None,
            pending_request: None,
            render_tx,
            render_rx,
            render_generation: 0,
            latest_generation,
            tile_cache: TileTextureCache::new(384 * 1024 * 1024),
            texture_serial: 0,
            status: "Ready".to_string(),
            source_label: String::new(),
            center_x: 0.0,
            center_y: 0.0,
            view_width: 1024.0,
            max_chunk_mb: 256,
            last_drag_delta: egui::Vec2::ZERO,
        };

        if !app.path_input.trim().is_empty() {
            app.open_current_path();
        }

        app
    }

    fn open_current_path(&mut self) {
        let path = PathBuf::from(self.path_input.trim());
        match load_info(&path) {
            Ok(info) => {
                self.center_x = info.width as f64 / 2.0;
                self.center_y = info.height as f64 / 2.0;
                self.view_width = info.width as f64;
                self.status = format!("Opened {}", path.display());
                self.path = Some(path);
                self.info = Some(info);
                self.last_request = None;
                self.pending_request = None;
                self.tile_cache.clear();
                self.source_label.clear();
            }
            Err(err) => {
                self.status = format!("Open failed: {err:#}");
            }
        }
    }

    fn fit_view(&mut self) {
        if let Some(info) = &self.info {
            self.center_x = info.width as f64 / 2.0;
            self.center_y = info.height as f64 / 2.0;
            self.view_width = info.width as f64;
            self.last_request = None;
        }
    }

    fn zoom(&mut self, factor: f64) {
        if let Some(info) = &self.info {
            self.view_width = (self.view_width * factor).clamp(32.0, info.width as f64);
            self.last_request = None;
        }
    }

    fn drain_render_results(&mut self, ctx: &egui::Context) {
        while let Ok(rendered) = self.render_rx.try_recv() {
            match rendered.result {
                Ok(bitmap) => {
                    let bitmap = Arc::new(bitmap);

                    if self.pending_request.as_ref() == Some(&rendered.request) {
                        self.insert_tile_texture(ctx, rendered.request, &bitmap);
                        self.pending_request = None;
                    }
                }
                Err(err) => {
                    if self.pending_request.as_ref() == Some(&rendered.request) {
                        self.status = format!("Render failed: {err:#}");
                        self.pending_request = None;
                    }
                }
            }
        }
    }

    fn insert_tile_texture(
        &mut self,
        ctx: &egui::Context,
        request: PreviewRequest,
        bitmap: &Arc<PreviewBitmap>,
    ) {
        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [bitmap.width as usize, bitmap.height as usize],
            &bitmap.rgba,
        );
        self.texture_serial = self.texture_serial.wrapping_add(1);
        let texture = ctx.load_texture(
            format!("giga-tile-{}", self.texture_serial),
            color_image,
            egui::TextureOptions::LINEAR,
        );
        let tile = Arc::new(TileTexture {
            texture,
            width: bitmap.width,
            height: bitmap.height,
            source: bitmap.source,
            stats: bitmap.stats,
            bytes: bitmap.rgba.len(),
        });
        self.tile_cache.insert(request, tile);
        ctx.request_repaint();
    }

    fn queue_render(&mut self, request: PreviewRequest, info: &ImageInfo, status: String) {
        if self.pending_request.as_ref() == Some(&request) {
            return;
        }

        self.render_generation = self.render_generation.wrapping_add(1);
        self.latest_generation
            .store(self.render_generation, Ordering::Relaxed);

        let send_result = self.render_tx.send(RenderJob {
            request: request.clone(),
            info: info.clone(),
            max_chunk_mb: self.max_chunk_mb,
            generation: self.render_generation,
        });

        if send_result.is_ok() {
            self.pending_request = Some(request);
            self.status = status;
        } else {
            self.status = "Render worker stopped".to_string();
        }
    }

    fn browse_file(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("TIFF", &["tif", "tiff"])
            .add_filter("All files", &["*"])
            .pick_file()
        {
            self.path_input = path.display().to_string();
            self.open_current_path();
        }
    }

    fn export_current_view_png(&mut self) {
        let Some(request) = self.last_request.clone() else {
            self.status = "Nothing to export yet".to_string();
            return;
        };
        let Some(info) = self.info.clone() else {
            self.status = "No TIFF is open".to_string();
            return;
        };

        let Some(path) = rfd::FileDialog::new()
            .add_filter("PNG", &["png"])
            .set_file_name("viewport.png")
            .save_file()
        else {
            return;
        };

        let result = render_preview(
            &request.path,
            &info,
            request.rect,
            4096,
            self.max_chunk_mb,
            request.backend,
            None,
            None,
        )
        .and_then(|bitmap| {
            save_png(
                &path,
                bitmap.width,
                bitmap.height,
                &bitmap.rgba,
                PngCompression::Fast,
            )?;
            Ok(bitmap)
        });

        match result {
            Ok(bitmap) => {
                self.status = format!(
                    "Exported {} ({} x {})",
                    path.display(),
                    bitmap.width,
                    bitmap.height
                );
            }
            Err(err) => self.status = format!("Export failed: {err:#}"),
        }
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Browse...").clicked() {
                    ui.close();
                    self.browse_file();
                }

                ui.separator();

                let can_export = self.last_request.is_some();
                if ui
                    .add_enabled(can_export, egui::Button::new("Export as PNG..."))
                    .clicked()
                {
                    ui.close();
                    self.export_current_view_png();
                }

                ui.separator();

                if ui.button("Quit").clicked() {
                    ui.close();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }

    fn render_canvas(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.drain_render_results(ctx);

        let available = ui.available_size();
        if available.x < 1.0 || available.y < 1.0 {
            return;
        }
        let (canvas, response) = ui.allocate_exact_size(available, egui::Sense::drag());
        let painter = ui.painter_at(canvas);
        painter.rect_filled(canvas, 0.0, egui::Color32::from_rgb(28, 30, 31));

        let (Some(info), Some(path)) = (self.info.clone(), self.path.clone()) else {
            painter.text(
                canvas.center(),
                egui::Align2::CENTER_CENTER,
                "Open a TIFF file",
                egui::FontId::proportional(18.0),
                egui::Color32::LIGHT_GRAY,
            );
            return;
        };

        if response.dragged() {
            let delta = response.drag_delta() - self.last_drag_delta;
            self.last_drag_delta = response.drag_delta();
            let view = self.current_rect(&info, canvas.size());
            self.center_x -= delta.x as f64 * view.width as f64 / canvas.width() as f64;
            self.center_y -= delta.y as f64 * view.height as f64 / canvas.height() as f64;
            self.last_request = None;
        } else {
            self.last_drag_delta = egui::Vec2::ZERO;
        }

        if response.hovered() {
            let scroll = ui.input(|input| input.smooth_scroll_delta.y);
            if scroll.abs() > 0.1 {
                self.zoom(0.998_f64.powf(scroll as f64));
            }
        }

        let source_rect = self.current_rect(&info, canvas.size());
        let max_output = (canvas.width().max(canvas.height()) * ctx.pixels_per_point())
            .round()
            .clamp(256.0, 2048.0) as u32;
        let request = PreviewRequest {
            path: path.clone(),
            rect: source_rect,
            max_output,
            backend: Backend::Auto,
        };
        self.last_request = Some(request);

        let tiles = visible_tile_requests(
            &path,
            &info,
            source_rect,
            canvas,
            ctx.pixels_per_point(),
            Backend::Auto,
        );
        let total_tiles = tiles.len();
        let mut rendered_tiles = 0usize;
        let mut missing_tile = None;
        let mut latest_tile_label = None;

        for tile in tiles {
            if let Some(texture_tile) = self.tile_cache.get(&tile.request) {
                rendered_tiles += 1;
                latest_tile_label = Some(format!(
                    "{} x {}, {}, {}",
                    texture_tile.width,
                    texture_tile.height,
                    texture_tile.source,
                    texture_tile.stats.short_label()
                ));
                painter.image(
                    texture_tile.texture.id(),
                    tile.screen_rect,
                    tile.uv_rect,
                    egui::Color32::WHITE,
                );
            } else if missing_tile.is_none() {
                missing_tile = Some(tile.request);
            }
        }

        if let Some(tile_request) = missing_tile {
            if self.pending_request.as_ref() != Some(&tile_request) {
                self.queue_render(
                    tile_request.clone(),
                    &info,
                    format!(
                        "Rendering tile {}/{} for x={} y={} w={} h={}...",
                        rendered_tiles + 1,
                        total_tiles,
                        tile_request.rect.x,
                        tile_request.rect.y,
                        tile_request.rect.width,
                        tile_request.rect.height
                    ),
                );
            }
        } else if total_tiles > 0 {
            let mut prefetch_queued = false;

            if self.pending_request.is_none() {
                for tile_request in prefetch_tile_requests(
                    &path,
                    &info,
                    source_rect,
                    canvas,
                    ctx.pixels_per_point(),
                    Backend::Auto,
                ) {
                    if !self.tile_cache.contains(&tile_request) {
                        self.queue_render(
                            tile_request.clone(),
                            &info,
                            format!(
                                "Prefetching tile x={} y={} w={} h={}...",
                                tile_request.rect.x,
                                tile_request.rect.y,
                                tile_request.rect.width,
                                tile_request.rect.height
                            ),
                        );
                        prefetch_queued = true;
                        break;
                    }
                }
            }

            if !prefetch_queued {
                self.status = format!(
                    "Viewport x={} y={} w={} h={} (tiles cached)",
                    source_rect.x, source_rect.y, source_rect.width, source_rect.height
                );
            }
        }

        self.source_label = format!(
            "tiles {}/{}, tile cache {}{}",
            rendered_tiles,
            total_tiles,
            self.tile_cache.len(),
            latest_tile_label
                .as_ref()
                .map(|label| format!(", {label}"))
                .unwrap_or_default()
        );

        if rendered_tiles > 0 {
            painter.rect_stroke(
                canvas,
                0.0,
                egui::Stroke::new(1.0, egui::Color32::from_gray(70)),
                egui::StrokeKind::Inside,
            );
        }

        painter.text(
            canvas.left_top() + egui::vec2(10.0, 10.0),
            egui::Align2::LEFT_TOP,
            &self.source_label,
            egui::FontId::monospace(12.0),
            egui::Color32::WHITE,
        );

        if rendered_tiles == 0 {
            painter.text(
                canvas.center(),
                egui::Align2::CENTER_CENTER,
                "Rendering...",
                egui::FontId::proportional(18.0),
                egui::Color32::LIGHT_GRAY,
            );
        }
    }

    fn current_rect(&self, info: &ImageInfo, canvas_size: egui::Vec2) -> Rect {
        if self.view_width >= info.width as f64 - 0.5 {
            return Rect {
                x: 0,
                y: 0,
                width: info.width,
                height: info.height,
            };
        }

        let aspect = (canvas_size.x.max(1.0) / canvas_size.y.max(1.0)) as f64;
        let mut width = self.view_width.clamp(1.0, info.width as f64);
        let mut height = width / aspect;

        if height > info.height as f64 {
            height = info.height as f64;
            width = (height * aspect).min(info.width as f64);
        }

        let half_w = width / 2.0;
        let half_h = height / 2.0;
        let center_x = self.center_x.clamp(half_w, info.width as f64 - half_w);
        let center_y = self.center_y.clamp(half_h, info.height as f64 - half_h);
        let x = (center_x - half_w).round().max(0.0) as u32;
        let y = (center_y - half_h).round().max(0.0) as u32;

        Rect {
            x,
            y,
            width: width.round().max(1.0).min((info.width - x) as f64) as u32,
            height: height.round().max(1.0).min((info.height - y) as f64) as u32,
        }
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        egui::Panel::top("toolbar").show_inside(ui, |ui| {
            self.menu_bar(ui, &ctx);
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("TIFF");
                let path_edit = ui.text_edit_singleline(&mut self.path_input);
                if path_edit.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    self.open_current_path();
                }
                if ui.button("Browse").clicked() {
                    self.browse_file();
                }
                if ui.button("Fit").clicked() {
                    self.fit_view();
                }
                if ui.button("-").clicked() {
                    self.zoom(1.25);
                }
                if ui.button("+").clicked() {
                    self.zoom(0.8);
                }
                ui.label(&self.status);
            });

            if let Some(info) = &self.info {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("{} x {}", info.width, info.height));
                    ui.separator();
                    ui.label(format!("{:?}", info.color_type));
                    ui.separator();
                    ui.label(format!("{:?}", info.chunk_type));
                    ui.separator();
                    ui.label(format!(
                        "chunk {} x {}, count {}",
                        info.chunk_width, info.chunk_height, info.chunk_count
                    ));
                    ui.separator();
                    ui.label(if info.is_bigtiff { "BigTIFF" } else { "TIFF" });
                    ui.separator();
                    ui.label(match &info.icc_profile {
                        Some(profile) => format!("ICC {} bytes", profile.len()),
                        None => "no ICC".to_string(),
                    });
                });
            }
        });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.render_canvas(ui, &ctx);
        });
    }
}

fn visible_tile_requests(
    path: &Path,
    info: &ImageInfo,
    source_rect: Rect,
    canvas: egui::Rect,
    pixels_per_point: f32,
    backend: Backend,
) -> Vec<VisibleTile> {
    let (cols, rows, tile_source_w, tile_source_h) = gui_tile_shape(source_rect, canvas);
    let mut tiles = Vec::with_capacity(cols as usize * rows as usize);
    let source_right = source_rect.x + source_rect.width;
    let source_bottom = source_rect.y + source_rect.height;
    let mut tile_y = (source_rect.y / tile_source_h) * tile_source_h;

    while tile_y < source_bottom {
        let mut tile_x = (source_rect.x / tile_source_w) * tile_source_w;
        let tile_h = tile_source_h.min(info.height - tile_y);

        while tile_x < source_right {
            let tile_w = tile_source_w.min(info.width - tile_x);
            let ix0 = tile_x.max(source_rect.x);
            let iy0 = tile_y.max(source_rect.y);
            let ix1 = (tile_x + tile_w).min(source_right);
            let iy1 = (tile_y + tile_h).min(source_bottom);

            if ix1 > ix0 && iy1 > iy0 {
                let screen_x0 = canvas.left()
                    + ((ix0 - source_rect.x) as f32 / source_rect.width as f32) * canvas.width();
                let screen_x1 = canvas.left()
                    + ((ix1 - source_rect.x) as f32 / source_rect.width as f32) * canvas.width();
                let screen_y0 = canvas.top()
                    + ((iy0 - source_rect.y) as f32 / source_rect.height as f32) * canvas.height();
                let screen_y1 = canvas.top()
                    + ((iy1 - source_rect.y) as f32 / source_rect.height as f32) * canvas.height();
                let screen_rect = egui::Rect::from_min_max(
                    egui::pos2(screen_x0, screen_y0),
                    egui::pos2(screen_x1, screen_y1),
                );
                let uv_rect = egui::Rect::from_min_max(
                    egui::pos2(
                        (ix0 - tile_x) as f32 / tile_w as f32,
                        (iy0 - tile_y) as f32 / tile_h as f32,
                    ),
                    egui::pos2(
                        (ix1 - tile_x) as f32 / tile_w as f32,
                        (iy1 - tile_y) as f32 / tile_h as f32,
                    ),
                );
                let full_tile_screen_w = tile_w as f32 / source_rect.width as f32 * canvas.width();
                let full_tile_screen_h =
                    tile_h as f32 / source_rect.height as f32 * canvas.height();
                let max_output = (full_tile_screen_w.max(full_tile_screen_h) * pixels_per_point)
                    .round()
                    .clamp(128.0, 768.0) as u32;

                tiles.push(VisibleTile {
                    request: PreviewRequest {
                        path: path.to_path_buf(),
                        rect: Rect {
                            x: tile_x,
                            y: tile_y,
                            width: tile_w,
                            height: tile_h,
                        },
                        max_output,
                        backend,
                    },
                    screen_rect,
                    uv_rect,
                });
            }

            tile_x = tile_x.saturating_add(tile_source_w);
        }

        tile_y = tile_y.saturating_add(tile_source_h);
    }

    tiles
}

fn prefetch_tile_requests(
    path: &Path,
    info: &ImageInfo,
    source_rect: Rect,
    canvas: egui::Rect,
    pixels_per_point: f32,
    backend: Backend,
) -> Vec<PreviewRequest> {
    let (_, _, tile_source_w, tile_source_h) = gui_tile_shape(source_rect, canvas);
    let source_right = source_rect.x + source_rect.width;
    let source_bottom = source_rect.y + source_rect.height;
    let visible_start_x = (source_rect.x / tile_source_w) * tile_source_w;
    let visible_start_y = (source_rect.y / tile_source_h) * tile_source_h;
    let visible_end_x = ((source_right - 1) / tile_source_w) * tile_source_w;
    let visible_end_y = ((source_bottom - 1) / tile_source_h) * tile_source_h;
    let radius_w = tile_source_w.saturating_mul(GUI_PREFETCH_TILE_RADIUS);
    let radius_h = tile_source_h.saturating_mul(GUI_PREFETCH_TILE_RADIUS);
    let prefetch_start_x = visible_start_x.saturating_sub(radius_w);
    let prefetch_start_y = visible_start_y.saturating_sub(radius_h);
    let prefetch_end_x = visible_end_x.saturating_add(radius_w);
    let prefetch_end_y = visible_end_y.saturating_add(radius_h);
    let view_center_x = source_rect.x as i128 + source_rect.width as i128 / 2;
    let view_center_y = source_rect.y as i128 + source_rect.height as i128 / 2;
    let mut requests = Vec::new();
    let mut tile_y = prefetch_start_y;

    while tile_y <= prefetch_end_y && tile_y < info.height {
        let mut tile_x = prefetch_start_x;
        let tile_h = tile_source_h.min(info.height - tile_y);

        while tile_x <= prefetch_end_x && tile_x < info.width {
            let visible_tile = tile_x >= visible_start_x
                && tile_x <= visible_end_x
                && tile_y >= visible_start_y
                && tile_y <= visible_end_y;

            if !visible_tile {
                let tile_w = tile_source_w.min(info.width - tile_x);
                let full_tile_screen_w = tile_w as f32 / source_rect.width as f32 * canvas.width();
                let full_tile_screen_h =
                    tile_h as f32 / source_rect.height as f32 * canvas.height();
                let max_output = (full_tile_screen_w.max(full_tile_screen_h) * pixels_per_point)
                    .round()
                    .clamp(128.0, 768.0) as u32;
                let tile_center_x = tile_x as i128 + tile_w as i128 / 2;
                let tile_center_y = tile_y as i128 + tile_h as i128 / 2;
                let dx = tile_center_x - view_center_x;
                let dy = tile_center_y - view_center_y;
                let priority = (dx * dx + dy * dy) as u128;

                requests.push((
                    priority,
                    PreviewRequest {
                        path: path.to_path_buf(),
                        rect: Rect {
                            x: tile_x,
                            y: tile_y,
                            width: tile_w,
                            height: tile_h,
                        },
                        max_output,
                        backend,
                    },
                ));
            }

            tile_x = tile_x.saturating_add(tile_source_w);
        }

        tile_y = tile_y.saturating_add(tile_source_h);
    }

    requests.sort_by_key(|(priority, _)| *priority);
    requests.into_iter().map(|(_, request)| request).collect()
}

fn gui_tile_shape(source_rect: Rect, canvas: egui::Rect) -> (u32, u32, u32, u32) {
    let cols = (canvas.width() / GUI_TILE_SIZE).ceil().max(1.0) as u32;
    let rows = (canvas.height() / GUI_TILE_SIZE).ceil().max(1.0) as u32;
    let tile_source_w = source_rect.width.div_ceil(cols).max(1);
    let tile_source_h = source_rect.height.div_ceil(rows).max(1);
    (cols, rows, tile_source_w, tile_source_h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_info(width: u32, height: u32) -> ImageInfo {
        ImageInfo {
            width,
            height,
            color_type: ColorType::RGB(8),
            chunk_type: ChunkType::Strip,
            chunk_width: width,
            chunk_height: 256,
            chunk_count: height.div_ceil(256),
            chunks_across: 1,
            compression: None,
            bits_per_sample: Some(vec![8, 8, 8]),
            samples_per_pixel: Some(3),
            planar_config: Some(1),
            photometric: Some(2),
            is_bigtiff: false,
            little_endian: true,
            rows_per_strip: Some(256),
            strip_offsets: None,
            icc_profile: None,
        }
    }

    #[test]
    fn prefetch_tiles_skip_visible_tiles() {
        let info = dummy_info(4096, 4096);
        let source_rect = Rect {
            x: 512,
            y: 512,
            width: 1024,
            height: 1024,
        };
        let canvas = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        let path = Path::new("sample.tif");
        let visible = visible_tile_requests(path, &info, source_rect, canvas, 1.0, Backend::Auto);
        let prefetch = prefetch_tile_requests(path, &info, source_rect, canvas, 1.0, Backend::Auto);

        assert!(!prefetch.is_empty());
        for request in prefetch {
            assert!(
                !visible.iter().any(|tile| tile.request == request),
                "prefetch returned a visible tile: {:?}",
                request.rect
            );
        }
    }

    #[test]
    fn prefetch_tiles_stay_inside_image_bounds() {
        let info = dummy_info(1024, 768);
        let source_rect = Rect {
            x: 0,
            y: 0,
            width: 1024,
            height: 768,
        };
        let canvas = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(640.0, 480.0));
        let path = Path::new("sample.tif");

        for request in prefetch_tile_requests(path, &info, source_rect, canvas, 1.0, Backend::Auto)
        {
            assert!(request.rect.x < info.width);
            assert!(request.rect.y < info.height);
            assert!(request.rect.x + request.rect.width <= info.width);
            assert!(request.rect.y + request.rect.height <= info.height);
        }
    }

    #[test]
    fn raw_sampled_row_rgb8_writes_rgba_directly() {
        let src_row = [1, 2, 3, 10, 20, 30, 40, 50, 60];
        let offsets = [6, 0];
        let mut out = [0u8; 8];

        write_raw_sampled_row_rgba(&src_row, &offsets, ColorType::RGB(8), true, &mut out).unwrap();

        assert_eq!(out, [40, 50, 60, 255, 1, 2, 3, 255]);
    }

    #[test]
    fn raw_sampled_row_rgb16_respects_endianness() {
        let little = [0x34, 0x12, 0x78, 0x56, 0xbc, 0x9a];
        let big = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc];
        let offsets = [0];
        let mut little_out = [0u8; 4];
        let mut big_out = [0u8; 4];

        write_raw_sampled_row_rgba(&little, &offsets, ColorType::RGB(16), true, &mut little_out)
            .unwrap();
        write_raw_sampled_row_rgba(&big, &offsets, ColorType::RGB(16), false, &mut big_out)
            .unwrap();

        assert_eq!(little_out, [0x12, 0x56, 0x9a, 255]);
        assert_eq!(big_out, [0x12, 0x56, 0x9a, 255]);
    }
}

fn print_info(path: &Path) -> Result<()> {
    let info = load_info(path)?;

    println!("file: {}", path.display());
    println!("bigtiff: {}", info.is_bigtiff);
    println!("size: {} x {}", info.width, info.height);
    println!("color: {:?}", info.color_type);
    println!("chunk type: {:?}", info.chunk_type);
    println!(
        "chunk size: {} x {} ({} chunks, {} across)",
        info.chunk_width, info.chunk_height, info.chunk_count, info.chunks_across
    );
    println!("compression tag: {}", opt_u32(info.compression));
    println!(
        "bits per sample: {}",
        opt_vec(info.bits_per_sample.as_deref())
    );
    println!("samples per pixel: {}", opt_u32(info.samples_per_pixel));
    println!("photometric tag: {}", opt_u32(info.photometric));
    println!("planar config: {}", opt_u32(info.planar_config));
    println!(
        "icc profile: {}",
        info.icc_profile
            .as_ref()
            .map(|profile| format!("{} bytes", profile.len()))
            .unwrap_or_else(|| "n/a".to_string())
    );

    Ok(())
}

fn write_preview(
    path: &Path,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    max_output: u32,
    out: &Path,
    max_chunk_mb: usize,
    backend: Backend,
    png_compression: PngCompression,
) -> Result<()> {
    if width == 0 || height == 0 {
        bail!("preview width and height must be greater than zero");
    }
    if max_output == 0 {
        bail!("max-output must be greater than zero");
    }

    let info = load_info(path)?;
    let rect = clamp_rect(x, y, width, height, info.width, info.height)?;
    let preview = render_preview(
        path,
        &info,
        rect,
        max_output,
        max_chunk_mb,
        backend,
        None,
        None,
    )?;

    let save_start = Instant::now();
    save_png(
        out,
        preview.width,
        preview.height,
        &preview.rgba,
        png_compression,
    )?;
    let save_time = save_start.elapsed();
    println!(
        "wrote {} ({} x {}, {}, decoded {} chunk(s), {}, png {:?} {:.1} ms)",
        out.display(),
        preview.width,
        preview.height,
        preview.source,
        preview.decoded_chunks,
        preview.stats.short_label(),
        png_compression,
        ms(save_time)
    );
    Ok(())
}

fn render_preview(
    path: &Path,
    info: &ImageInfo,
    rect: Rect,
    max_output: u32,
    max_chunk_mb: usize,
    backend: Backend,
    cancel: Option<&RenderCancel>,
    mut scanline_cache: Option<&mut ScanlineCache>,
) -> Result<PreviewBitmap> {
    let total_start = Instant::now();
    let (out_width, out_height) = fit_size(rect.width, rect.height, max_output);

    if backend != Backend::Libtiff && can_read_raw_strips(info) {
        let mut rendered = render_raw_strip_preview(
            path,
            info,
            rect,
            out_width,
            out_height,
            cancel,
            scanline_cache.as_deref_mut(),
        )?;
        rendered.stats.total = total_start.elapsed();
        return Ok(PreviewBitmap {
            width: out_width,
            height: out_height,
            rgba: rendered.rgba,
            source: preview_source("raw strip reads", info),
            decoded_chunks: 0,
            stats: rendered.stats,
        });
    }

    if backend != Backend::Rust {
        let mut rendered = render_libtiff_scanline_preview(
            path,
            info,
            rect,
            out_width,
            out_height,
            cancel,
            scanline_cache.as_deref_mut(),
        )?;
        rendered.stats.total = total_start.elapsed();
        return Ok(PreviewBitmap {
            width: out_width,
            height: out_height,
            rgba: rendered.rgba,
            source: preview_source("libtiff scanlines", info),
            decoded_chunks: 0,
            stats: rendered.stats,
        });
    }

    let mut decoder = open_decoder(path, max_chunk_mb)?;
    let mut rgba = vec![255u8; out_width as usize * out_height as usize * 4];
    let mut stats = RenderStats::default();
    let sampling = SamplingPlan::new(rect, out_width, out_height, 4);

    let chunk_x0 = rect.x / info.chunk_width;
    let chunk_y0 = rect.y / info.chunk_height;
    let chunk_x1 = (rect.x + rect.width - 1) / info.chunk_width;
    let chunk_y1 = (rect.y + rect.height - 1) / info.chunk_height;

    let mut decoded_chunks = 0u32;
    for chunk_y in chunk_y0..=chunk_y1 {
        for chunk_x in chunk_x0..=chunk_x1 {
            if let Some(cancel) = cancel {
                cancel.check()?;
            }
            if chunk_x >= info.chunks_across {
                continue;
            }

            let chunk_index = chunk_y
                .checked_mul(info.chunks_across)
                .and_then(|v| v.checked_add(chunk_x))
                .ok_or_else(|| anyhow!("chunk index overflow"))?;

            if chunk_index >= info.chunk_count {
                continue;
            }

            let (data_width, data_height) = decoder.chunk_data_dimensions(chunk_index);
            let chunk_origin_x = chunk_x * info.chunk_width;
            let chunk_origin_y = chunk_y * info.chunk_height;
            let decode_start = Instant::now();
            let chunk_rgba = decode_chunk_rgba(&mut decoder, &info, chunk_index)
                .with_context(|| format!("decoding chunk {}", chunk_index))?;
            stats.decode += decode_start.elapsed();
            let blit_start = Instant::now();
            blit_chunk_to_preview(
                &mut rgba,
                out_width,
                &sampling,
                &chunk_rgba,
                data_width,
                data_height,
                chunk_origin_x,
                chunk_origin_y,
            );
            stats.blit += blit_start.elapsed();
            decoded_chunks += 1;
        }
    }
    stats.total = total_start.elapsed();

    Ok(PreviewBitmap {
        width: out_width,
        height: out_height,
        rgba,
        source: "chunk decoder",
        decoded_chunks,
        stats,
    })
}

fn preview_source(base: &'static str, info: &ImageInfo) -> &'static str {
    match (base, info.icc_profile.is_some()) {
        ("libtiff scanlines", true) => "libtiff scanlines + lcms2 ICC",
        ("raw strip reads", true) => "raw strip reads + lcms2 ICC",
        _ => base,
    }
}

fn open_decoder(path: &Path, max_chunk_mb: usize) -> Result<Decoder<BufReader<File>>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut limits = Limits::default();
    limits.decoding_buffer_size = max_chunk_mb * 1024 * 1024;
    limits.intermediate_buffer_size = max(16, max_chunk_mb / 2) * 1024 * 1024;
    limits.ifd_value_size = 64 * 1024 * 1024;

    let decoder = Decoder::new(reader)
        .with_context(|| format!("reading TIFF directory from {}", path.display()))?
        .with_limits(limits);
    Ok(decoder)
}

fn load_info(path: &Path) -> Result<ImageInfo> {
    let mut decoder = open_decoder(path, 256)?;
    read_info(path, &mut decoder)
}

fn read_info(path: &Path, decoder: &mut Decoder<BufReader<File>>) -> Result<ImageInfo> {
    let (width, height) = decoder.dimensions()?;
    let color_type = decoder.colortype()?;
    let chunk_type = decoder.get_chunk_type();
    let (chunk_width, chunk_height) = decoder.chunk_dimensions();
    let chunk_count = match chunk_type {
        ChunkType::Strip => decoder.strip_count()?,
        ChunkType::Tile => decoder.tile_count()?,
    };
    let chunks_across = max(1, width.div_ceil(chunk_width));

    Ok(ImageInfo {
        width,
        height,
        color_type,
        chunk_type,
        chunk_width,
        chunk_height,
        chunk_count,
        chunks_across,
        compression: tag_u32(decoder, Tag::Compression),
        bits_per_sample: decoder.get_tag_u16_vec(Tag::BitsPerSample).ok(),
        samples_per_pixel: tag_u32(decoder, Tag::SamplesPerPixel),
        planar_config: tag_u32(decoder, Tag::PlanarConfiguration),
        photometric: tag_u32(decoder, Tag::PhotometricInterpretation),
        is_bigtiff: sniff_header(path).map(|h| h.is_bigtiff).unwrap_or(false),
        little_endian: sniff_header(path).map(|h| h.little_endian).unwrap_or(true),
        rows_per_strip: tag_u32(decoder, Tag::RowsPerStrip),
        strip_offsets: decoder.get_tag_u64_vec(Tag::StripOffsets).ok(),
        icc_profile: decoder
            .get_tag_u8_vec(Tag::IccProfile)
            .ok()
            .map(Arc::<[u8]>::from),
    })
}

fn can_read_raw_strips(info: &ImageInfo) -> bool {
    if info.chunk_type != ChunkType::Strip {
        return false;
    }
    if info.compression != Some(1) || info.planar_config.unwrap_or(1) != 1 {
        return false;
    }
    if info.rows_per_strip.is_none() || info.strip_offsets.as_ref().is_none_or(Vec::is_empty) {
        return false;
    }

    matches!(
        info.color_type,
        ColorType::Gray(8)
            | ColorType::Gray(16)
            | ColorType::RGB(8)
            | ColorType::RGB(16)
            | ColorType::RGBA(8)
            | ColorType::RGBA(16)
    )
}

#[repr(C)]
struct TiffHandle {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn TIFFOpen(name: *const c_char, mode: *const c_char) -> *mut TiffHandle;
    fn TIFFClose(tif: *mut TiffHandle);
    fn TIFFScanlineSize64(tif: *mut TiffHandle) -> u64;
    fn TIFFReadScanline(
        tif: *mut TiffHandle,
        buf: *mut c_void,
        row: c_uint,
        sample: c_ushort,
    ) -> c_int;
    fn TIFFIsTiled(tif: *mut TiffHandle) -> c_int;
}

struct LibtiffFile {
    handle: *mut TiffHandle,
}

impl LibtiffFile {
    fn open(path: &Path) -> Result<Self> {
        let path = CString::new(path.to_string_lossy().as_bytes())
            .context("TIFF path contains an embedded NUL byte")?;
        let mode = CString::new("r")?;
        let handle = unsafe { TIFFOpen(path.as_ptr(), mode.as_ptr()) };
        if handle.is_null() {
            bail!("libtiff could not open the file");
        }
        Ok(Self { handle })
    }
}

impl Drop for LibtiffFile {
    fn drop(&mut self) {
        unsafe {
            TIFFClose(self.handle);
        }
    }
}

fn render_libtiff_scanline_preview(
    path: &Path,
    info: &ImageInfo,
    rect: Rect,
    out_width: u32,
    out_height: u32,
    cancel: Option<&RenderCancel>,
    mut scanline_cache: Option<&mut ScanlineCache>,
) -> Result<RenderedPixels> {
    if info.chunk_type == ChunkType::Tile {
        bail!("libtiff scanline backend does not handle tiled TIFF yet");
    }
    if info.planar_config.unwrap_or(1) != 1 {
        bail!("libtiff scanline backend supports only contiguous planar config");
    }

    let tif = LibtiffFile::open(path)?;
    if unsafe { TIFFIsTiled(tif.handle) } != 0 {
        bail!("libtiff scanline backend does not handle tiled TIFF yet");
    }

    let samples = samples_for_color(info.color_type)?;
    let bits = bits_for_color(info.color_type)?;
    if bits != 8 && bits != 16 {
        bail!("libtiff scanline backend supports only 8-bit and 16-bit samples");
    }

    let bytes_per_sample = usize::from(bits / 8);
    let bytes_per_pixel = samples * bytes_per_sample;
    let color_transform = ColorTransform::new(info.color_type, info.icc_profile.as_deref())?;
    let expected_min_scanline = info.width as usize * bytes_per_pixel;
    let scanline_size = unsafe { TIFFScanlineSize64(tif.handle) } as usize;
    if scanline_size < expected_min_scanline {
        bail!(
            "libtiff scanline size {} is smaller than expected {}",
            scanline_size,
            expected_min_scanline
        );
    }

    let mut row = vec![0u8; scanline_size];
    let mut rgba = vec![255u8; out_width as usize * out_height as usize * 4];
    let mut sampled = vec![0u8; out_width as usize * bytes_per_pixel];
    let mut normalized = Vec::new();
    let mut lcms_rgb = Vec::new();
    let mut stats = RenderStats::default();
    let sampling = SamplingPlan::new(rect, out_width, out_height, bytes_per_pixel);
    let row_start = rect.x as usize * bytes_per_pixel;
    let row_end = row_start + rect.width as usize * bytes_per_pixel;
    let parallel_rows = should_parallel_rows(color_transform.as_ref(), out_height);
    let mut row_batch = Vec::<Arc<Vec<u8>>>::new();
    let mut row_batch_first_oy = 0usize;

    for (oy, &src_y) in sampling.src_y.iter().enumerate() {
        if let Some(cancel) = cancel {
            cancel.check()?;
        }
        let cache_key = ScanlineKey {
            path: path.to_path_buf(),
            y: src_y,
            x: rect.x,
            width: rect.width,
            bytes_per_pixel,
        };
        let cached_row = scanline_cache
            .as_deref_mut()
            .and_then(|cache| cache.get(&cache_key));
        let row_segment = if let Some(cached_row) = cached_row {
            stats.scanline_cache_hits += 1;
            cached_row
        } else {
            stats.scanline_cache_misses += u32::from(scanline_cache.is_some());
            let read_start = Instant::now();
            let ok = unsafe {
                TIFFReadScanline(
                    tif.handle,
                    row.as_mut_ptr().cast::<c_void>(),
                    src_y as c_uint,
                    0,
                )
            };
            if ok != 1 {
                bail!("libtiff failed to read scanline {}", src_y);
            }
            stats.read += read_start.elapsed();

            let row_segment = Arc::new(row[row_start..row_end].to_vec());
            if let Some(cache) = scanline_cache.as_deref_mut() {
                cache.insert(cache_key, Arc::clone(&row_segment));
            }
            row_segment
        };

        let convert_start = Instant::now();
        if parallel_rows {
            if row_batch.is_empty() {
                row_batch_first_oy = oy;
            }
            row_batch.push(row_segment);
            if row_batch.len() >= PARALLEL_ROW_BATCH {
                flush_parallel_rows(
                    &mut row_batch,
                    row_batch_first_oy,
                    out_width,
                    &sampling,
                    bytes_per_pixel,
                    info.color_type,
                    cfg!(target_endian = "little"),
                    color_transform.as_ref(),
                    &mut rgba,
                )?;
            }
        } else {
            write_sampled_row_rgba(
                row_segment.as_slice(),
                &sampling.src_x_byte_offsets,
                bytes_per_pixel,
                info.color_type,
                cfg!(target_endian = "little"),
                color_transform.as_ref(),
                &mut rgba[oy * out_width as usize * 4..(oy + 1) * out_width as usize * 4],
                &mut sampled,
                &mut normalized,
                &mut lcms_rgb,
            )?;
        }
        stats.convert += convert_start.elapsed();
    }
    let convert_start = Instant::now();
    flush_parallel_rows(
        &mut row_batch,
        row_batch_first_oy,
        out_width,
        &sampling,
        bytes_per_pixel,
        info.color_type,
        cfg!(target_endian = "little"),
        color_transform.as_ref(),
        &mut rgba,
    )?;
    stats.convert += convert_start.elapsed();

    Ok(RenderedPixels { rgba, stats })
}

fn render_raw_strip_preview(
    path: &Path,
    info: &ImageInfo,
    rect: Rect,
    out_width: u32,
    out_height: u32,
    cancel: Option<&RenderCancel>,
    mut scanline_cache: Option<&mut ScanlineCache>,
) -> Result<RenderedPixels> {
    let mut file = File::open(path)?;
    let samples = samples_for_color(info.color_type)?;
    let bits = bits_for_color(info.color_type)?;
    let bytes_per_sample = usize::from(bits / 8);
    let bytes_per_pixel = samples * bytes_per_sample;
    let row_stride = info.width as u64 * bytes_per_pixel as u64;
    let read_len = rect.width as usize * bytes_per_pixel;
    let mut row = vec![0u8; read_len];
    let mut rgba = vec![255u8; out_width as usize * out_height as usize * 4];
    let color_transform = ColorTransform::new(info.color_type, info.icc_profile.as_deref())?;
    let mut sampled = vec![0u8; out_width as usize * bytes_per_pixel];
    let mut normalized = Vec::new();
    let mut lcms_rgb = Vec::new();
    let mut stats = RenderStats::default();
    let rows_per_strip = info.rows_per_strip.unwrap();
    let strip_offsets = info.strip_offsets.as_ref().unwrap();
    let sampling = SamplingPlan::new(rect, out_width, out_height, bytes_per_pixel);
    let parallel_rows = should_parallel_rows(color_transform.as_ref(), out_height);
    let mut row_batch = Vec::<Arc<Vec<u8>>>::new();
    let mut row_batch_first_oy = 0usize;

    for (oy, &src_y) in sampling.src_y.iter().enumerate() {
        if let Some(cancel) = cancel {
            cancel.check()?;
        }
        let strip_index = (src_y / rows_per_strip) as usize;
        let strip_offset = *strip_offsets
            .get(strip_index)
            .ok_or_else(|| anyhow!("missing strip offset {}", strip_index))?;
        let row_in_strip = src_y % rows_per_strip;
        let offset = strip_offset
            + row_in_strip as u64 * row_stride
            + rect.x as u64 * bytes_per_pixel as u64;

        let cache_key = ScanlineKey {
            path: path.to_path_buf(),
            y: src_y,
            x: rect.x,
            width: rect.width,
            bytes_per_pixel,
        };
        let cached_row = scanline_cache
            .as_deref_mut()
            .and_then(|cache| cache.get(&cache_key));
        let row_segment = if let Some(cached_row) = cached_row {
            stats.scanline_cache_hits += 1;
            cached_row
        } else {
            stats.scanline_cache_misses += u32::from(scanline_cache.is_some());
            let read_start = Instant::now();
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut row)?;
            stats.read += read_start.elapsed();

            let row_segment = Arc::new(row.clone());
            if let Some(cache) = scanline_cache.as_deref_mut() {
                cache.insert(cache_key, Arc::clone(&row_segment));
            }
            row_segment
        };

        let convert_start = Instant::now();
        if parallel_rows {
            if row_batch.is_empty() {
                row_batch_first_oy = oy;
            }
            row_batch.push(row_segment);
            if row_batch.len() >= PARALLEL_ROW_BATCH {
                flush_parallel_rows(
                    &mut row_batch,
                    row_batch_first_oy,
                    out_width,
                    &sampling,
                    bytes_per_pixel,
                    info.color_type,
                    info.little_endian,
                    color_transform.as_ref(),
                    &mut rgba,
                )?;
            }
        } else {
            write_sampled_row_rgba(
                row_segment.as_slice(),
                &sampling.src_x_byte_offsets,
                bytes_per_pixel,
                info.color_type,
                info.little_endian,
                color_transform.as_ref(),
                &mut rgba[oy * out_width as usize * 4..(oy + 1) * out_width as usize * 4],
                &mut sampled,
                &mut normalized,
                &mut lcms_rgb,
            )?;
        }
        stats.convert += convert_start.elapsed();
    }
    let convert_start = Instant::now();
    flush_parallel_rows(
        &mut row_batch,
        row_batch_first_oy,
        out_width,
        &sampling,
        bytes_per_pixel,
        info.color_type,
        info.little_endian,
        color_transform.as_ref(),
        &mut rgba,
    )?;
    stats.convert += convert_start.elapsed();

    Ok(RenderedPixels { rgba, stats })
}

type LcmsTransform = Transform<u8, u8, GlobalContext, DisallowCache>;

enum ColorTransform {
    Rgb8ToRgb8(LcmsTransform),
    Rgb16ToRgb8(LcmsTransform),
    Rgba8ToRgb8(LcmsTransform),
    Rgba16ToRgb8(LcmsTransform),
    Gray8ToRgb8(LcmsTransform),
    Gray16ToRgb8(LcmsTransform),
}

impl ColorTransform {
    fn new(color_type: ColorType, icc_profile: Option<&[u8]>) -> Result<Option<Self>> {
        let Some(icc_profile) = icc_profile else {
            return Ok(None);
        };

        let Some(input_format) = lcms_input_format(color_type) else {
            return Ok(None);
        };

        let input = Profile::new_icc(icc_profile).context("reading embedded ICC profile")?;
        let output = Profile::new_srgb();
        let transform = LcmsTransform::new_flags_context(
            GlobalContext::new(),
            &input,
            input_format,
            &output,
            PixelFormat::RGB_8,
            Intent::Perceptual,
            Flags::NO_CACHE | Flags::BLACKPOINT_COMPENSATION,
        )
        .context("creating lcms2 sRGB transform")?;

        Ok(Some(match color_type {
            ColorType::RGB(8) => Self::Rgb8ToRgb8(transform),
            ColorType::RGB(16) => Self::Rgb16ToRgb8(transform),
            ColorType::RGBA(8) => Self::Rgba8ToRgb8(transform),
            ColorType::RGBA(16) => Self::Rgba16ToRgb8(transform),
            ColorType::Gray(8) => Self::Gray8ToRgb8(transform),
            ColorType::Gray(16) => Self::Gray16ToRgb8(transform),
            _ => return Ok(None),
        }))
    }

    fn transform_row(
        &self,
        input: &[u8],
        bytes_per_pixel: usize,
        little_endian: bool,
        out: &mut [u8],
        normalized: &mut Vec<u8>,
        rgb: &mut Vec<u8>,
    ) {
        let (transform, needs_u16_normalization) = match self {
            Self::Rgb8ToRgb8(transform)
            | Self::Rgba8ToRgb8(transform)
            | Self::Gray8ToRgb8(transform) => (transform, false),
            Self::Rgb16ToRgb8(transform)
            | Self::Rgba16ToRgb8(transform)
            | Self::Gray16ToRgb8(transform) => {
                (transform, little_endian != cfg!(target_endian = "little"))
            }
        };

        let original_input = input;
        let transform_input = if needs_u16_normalization {
            normalized.resize(original_input.len(), 0);
            normalize_u16_samples_to_native_endian(original_input, little_endian, normalized);
            normalized.as_slice()
        } else {
            original_input
        };

        let pixels = original_input.len() / bytes_per_pixel;
        rgb.resize(pixels * 3, 0);
        transform.transform_pixels(transform_input, rgb);

        for index in 0..pixels {
            let rgb_src = index * 3;
            let rgba_dst = index * 4;
            out[rgba_dst..rgba_dst + 3].copy_from_slice(&rgb[rgb_src..rgb_src + 3]);

            let px_src = index * bytes_per_pixel;
            out[rgba_dst + 3] = source_alpha_u8(
                &original_input[px_src..px_src + bytes_per_pixel],
                self,
                little_endian,
            );
        }
    }
}

fn source_alpha_u8(px: &[u8], transform: &ColorTransform, little_endian: bool) -> u8 {
    match transform {
        ColorTransform::Rgba8ToRgb8(_) => px[3],
        ColorTransform::Rgba16ToRgb8(_) => u16_to_u8(&px[6..8], little_endian),
        _ => 255,
    }
}

fn lcms_input_format(color_type: ColorType) -> Option<PixelFormat> {
    match color_type {
        ColorType::RGB(8) => Some(PixelFormat::RGB_8),
        ColorType::RGB(16) => Some(PixelFormat::RGB_16),
        ColorType::RGBA(8) => Some(PixelFormat::RGBA_8),
        ColorType::RGBA(16) => Some(PixelFormat::RGBA_16),
        ColorType::Gray(8) => Some(PixelFormat::GRAY_8),
        ColorType::Gray(16) => Some(PixelFormat::GRAY_16),
        _ => None,
    }
}

fn normalize_u16_samples_to_native_endian(src: &[u8], little_endian: bool, dst: &mut [u8]) {
    for (index, sample) in src.chunks_exact(2).enumerate() {
        let value = if little_endian {
            u16::from_le_bytes([sample[0], sample[1]])
        } else {
            u16::from_be_bytes([sample[0], sample[1]])
        };
        let bytes = value.to_ne_bytes();
        let offset = index * 2;
        dst[offset] = bytes[0];
        dst[offset + 1] = bytes[1];
    }
}

fn write_sampled_row_rgba(
    src_row: &[u8],
    src_x_byte_offsets: &[usize],
    bytes_per_pixel: usize,
    color_type: ColorType,
    little_endian: bool,
    color_transform: Option<&ColorTransform>,
    out: &mut [u8],
    sampled: &mut Vec<u8>,
    normalized: &mut Vec<u8>,
    lcms_rgb: &mut Vec<u8>,
) -> Result<()> {
    let out_width = src_x_byte_offsets.len();

    if color_transform.is_none() {
        return write_raw_sampled_row_rgba(
            src_row,
            src_x_byte_offsets,
            color_type,
            little_endian,
            out,
        );
    }

    sampled.resize(out_width as usize * bytes_per_pixel, 0);
    for (ox, &src) in src_x_byte_offsets.iter().enumerate() {
        let dst = ox * bytes_per_pixel;
        sampled[dst..dst + bytes_per_pixel].copy_from_slice(&src_row[src..src + bytes_per_pixel]);
    }

    let color_transform = color_transform.expect("checked above");
    color_transform.transform_row(
        sampled,
        bytes_per_pixel,
        little_endian,
        out,
        normalized,
        lcms_rgb,
    );
    Ok(())
}

fn should_parallel_rows(color_transform: Option<&ColorTransform>, out_height: u32) -> bool {
    color_transform.is_some() && out_height >= 128 && rayon::current_num_threads() > 1
}

fn flush_parallel_rows(
    rows: &mut Vec<Arc<Vec<u8>>>,
    first_oy: usize,
    out_width: u32,
    sampling: &SamplingPlan,
    bytes_per_pixel: usize,
    color_type: ColorType,
    little_endian: bool,
    color_transform: Option<&ColorTransform>,
    rgba: &mut [u8],
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let row_len = out_width as usize * 4;
    let start = first_oy * row_len;
    let end = start + rows.len() * row_len;
    rgba[start..end]
        .par_chunks_mut(row_len)
        .zip(rows.par_iter())
        .try_for_each(|(out, row)| {
            let mut sampled = Vec::new();
            let mut normalized = Vec::new();
            let mut lcms_rgb = Vec::new();
            write_sampled_row_rgba(
                row.as_slice(),
                &sampling.src_x_byte_offsets,
                bytes_per_pixel,
                color_type,
                little_endian,
                color_transform,
                out,
                &mut sampled,
                &mut normalized,
                &mut lcms_rgb,
            )
        })?;
    rows.clear();
    Ok(())
}

fn write_raw_sampled_row_rgba(
    src_row: &[u8],
    src_x_byte_offsets: &[usize],
    color_type: ColorType,
    little_endian: bool,
    out: &mut [u8],
) -> Result<()> {
    match color_type {
        ColorType::Gray(8) => {
            for (dst, &src) in out.chunks_exact_mut(4).zip(src_x_byte_offsets) {
                let g = src_row[src];
                dst.copy_from_slice(&[g, g, g, 255]);
            }
        }
        ColorType::RGB(8) => {
            for (dst, &src) in out.chunks_exact_mut(4).zip(src_x_byte_offsets) {
                dst[0..3].copy_from_slice(&src_row[src..src + 3]);
                dst[3] = 255;
            }
        }
        ColorType::RGBA(8) => {
            for (dst, &src) in out.chunks_exact_mut(4).zip(src_x_byte_offsets) {
                dst.copy_from_slice(&src_row[src..src + 4]);
            }
        }
        ColorType::Gray(16) => {
            for (dst, &src) in out.chunks_exact_mut(4).zip(src_x_byte_offsets) {
                let g = u16_to_u8(&src_row[src..src + 2], little_endian);
                dst.copy_from_slice(&[g, g, g, 255]);
            }
        }
        ColorType::RGB(16) => {
            for (dst, &src) in out.chunks_exact_mut(4).zip(src_x_byte_offsets) {
                dst.copy_from_slice(&[
                    u16_to_u8(&src_row[src..src + 2], little_endian),
                    u16_to_u8(&src_row[src + 2..src + 4], little_endian),
                    u16_to_u8(&src_row[src + 4..src + 6], little_endian),
                    255,
                ]);
            }
        }
        ColorType::RGBA(16) => {
            for (dst, &src) in out.chunks_exact_mut(4).zip(src_x_byte_offsets) {
                dst.copy_from_slice(&[
                    u16_to_u8(&src_row[src..src + 2], little_endian),
                    u16_to_u8(&src_row[src + 2..src + 4], little_endian),
                    u16_to_u8(&src_row[src + 4..src + 6], little_endian),
                    u16_to_u8(&src_row[src + 6..src + 8], little_endian),
                ]);
            }
        }
        other => bail!("unsupported raw strip conversion for {:?}", other),
    }
    Ok(())
}

fn u16_to_u8(bytes: &[u8], little_endian: bool) -> u8 {
    if little_endian { bytes[1] } else { bytes[0] }
}

fn samples_for_color(color_type: ColorType) -> Result<usize> {
    Ok(match color_type {
        ColorType::Gray(_) => 1,
        ColorType::GrayA(_) => 2,
        ColorType::RGB(_) => 3,
        ColorType::RGBA(_) => 4,
        ColorType::CMYK(_) => 4,
        ColorType::CMYKA(_) => 5,
        ColorType::YCbCr(_) => 3,
        ColorType::Palette(_) => 1,
        ColorType::Multiband { num_samples, .. } => num_samples as usize,
        other => bail!("unsupported color type {:?}", other),
    })
}

fn bits_for_color(color_type: ColorType) -> Result<u8> {
    Ok(match color_type {
        ColorType::Gray(bits)
        | ColorType::GrayA(bits)
        | ColorType::RGB(bits)
        | ColorType::RGBA(bits)
        | ColorType::CMYK(bits)
        | ColorType::CMYKA(bits)
        | ColorType::YCbCr(bits)
        | ColorType::Palette(bits)
        | ColorType::Multiband {
            bit_depth: bits, ..
        } => bits,
        other => bail!("unsupported color type {:?}", other),
    })
}

fn decode_chunk_rgba(
    decoder: &mut Decoder<BufReader<File>>,
    info: &ImageInfo,
    chunk_index: u32,
) -> Result<Vec<u8>> {
    let (width, height) = decoder.chunk_data_dimensions(chunk_index);
    let decoded = decoder.read_chunk(chunk_index)?;
    convert_to_rgba(decoded, info.color_type, width, height)
}

fn convert_to_rgba(
    decoded: DecodingResult,
    color_type: ColorType,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    let pixels = width as usize * height as usize;
    let mut rgba = Vec::with_capacity(pixels * 4);

    match (decoded, color_type) {
        (DecodingResult::U8(buf), ColorType::Gray(8)) => {
            for g in buf {
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
        }
        (DecodingResult::U8(buf), ColorType::RGB(8)) => {
            for px in buf.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
        }
        (DecodingResult::U8(buf), ColorType::RGBA(8)) => {
            rgba.extend_from_slice(&buf);
        }
        (DecodingResult::U8(buf), ColorType::GrayA(8)) => {
            for px in buf.chunks_exact(2) {
                rgba.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
        }
        (DecodingResult::U8(buf), ColorType::CMYK(8)) => {
            for px in buf.chunks_exact(4) {
                let c = px[0] as u16;
                let m = px[1] as u16;
                let y = px[2] as u16;
                let k = px[3] as u16;
                let r = 255u16.saturating_sub(min(255, c + k)) as u8;
                let g = 255u16.saturating_sub(min(255, m + k)) as u8;
                let b = 255u16.saturating_sub(min(255, y + k)) as u8;
                rgba.extend_from_slice(&[r, g, b, 255]);
            }
        }
        (DecodingResult::U16(buf), ColorType::Gray(16)) => {
            for g16 in buf {
                let g = (g16 >> 8) as u8;
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
        }
        (DecodingResult::U16(buf), ColorType::RGB(16)) => {
            for px in buf.chunks_exact(3) {
                rgba.extend_from_slice(&[
                    (px[0] >> 8) as u8,
                    (px[1] >> 8) as u8,
                    (px[2] >> 8) as u8,
                    255,
                ]);
            }
        }
        (DecodingResult::U16(buf), ColorType::RGBA(16)) => {
            for px in buf.chunks_exact(4) {
                rgba.extend_from_slice(&[
                    (px[0] >> 8) as u8,
                    (px[1] >> 8) as u8,
                    (px[2] >> 8) as u8,
                    (px[3] >> 8) as u8,
                ]);
            }
        }
        (other, ct) => {
            bail!("unsupported preview conversion for {:?} / {:?}", ct, other);
        }
    }

    if rgba.len() != pixels * 4 {
        bail!(
            "decoded chunk size mismatch: got {} RGBA bytes, expected {}",
            rgba.len(),
            pixels * 4
        );
    }
    Ok(rgba)
}

fn blit_chunk_to_preview(
    preview: &mut [u8],
    out_width: u32,
    sampling: &SamplingPlan,
    chunk_rgba: &[u8],
    chunk_width: u32,
    chunk_height: u32,
    chunk_origin_x: u32,
    chunk_origin_y: u32,
) {
    let chunk_right = chunk_origin_x + chunk_width;
    let chunk_bottom = chunk_origin_y + chunk_height;

    for (oy, &src_y) in sampling.src_y.iter().enumerate() {
        if src_y < chunk_origin_y || src_y >= chunk_bottom {
            continue;
        }

        for (ox, &src_x) in sampling.src_x.iter().enumerate() {
            if src_x < chunk_origin_x || src_x >= chunk_right {
                continue;
            }

            let cx = src_x - chunk_origin_x;
            let cy = src_y - chunk_origin_y;
            let src = (cy as usize * chunk_width as usize + cx as usize) * 4;
            let dst = (oy * out_width as usize + ox) * 4;
            preview[dst..dst + 4].copy_from_slice(&chunk_rgba[src..src + 4]);
        }
    }
}

fn save_png(
    path: &Path,
    width: u32,
    height: u32,
    rgba: &[u8],
    compression: PngCompression,
) -> Result<()> {
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(compression.to_png());
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

fn clamp_rect(x: u32, y: u32, width: u32, height: u32, image_w: u32, image_h: u32) -> Result<Rect> {
    if x >= image_w || y >= image_h {
        bail!("rectangle starts outside the image");
    }

    Ok(Rect {
        x,
        y,
        width: min(width, image_w - x),
        height: min(height, image_h - y),
    })
}

fn fit_size(width: u32, height: u32, max_output: u32) -> (u32, u32) {
    if width <= max_output && height <= max_output {
        return (width, height);
    }

    if width >= height {
        let out_h = max(
            1,
            ((height as u64 * max_output as u64) / width as u64) as u32,
        );
        (max_output, out_h)
    } else {
        let out_w = max(
            1,
            ((width as u64 * max_output as u64) / height as u64) as u32,
        );
        (out_w, max_output)
    }
}

fn tag_u32(decoder: &mut Decoder<BufReader<File>>, tag: Tag) -> Option<u32> {
    decoder.get_tag_unsigned::<u32>(tag).ok()
}

#[derive(Debug, Clone, Copy)]
struct TiffHeader {
    is_bigtiff: bool,
    little_endian: bool,
}

fn sniff_header(path: &Path) -> Result<TiffHeader> {
    let mut file = File::open(path)?;
    let mut header = [0u8; 4];
    file.read_exact(&mut header)?;
    let little_endian = match &header[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => bail!("not a TIFF byte-order marker"),
    };
    Ok(TiffHeader {
        is_bigtiff: matches!(header, [b'I', b'I', 43, 0] | [b'M', b'M', 0, 43]),
        little_endian,
    })
}

fn opt_u32(value: Option<u32>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "n/a".to_string())
}

fn opt_vec(value: Option<&[u16]>) -> String {
    value
        .map(|items| {
            items
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| "n/a".to_string())
}
