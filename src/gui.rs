use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use anyhow::{Result, anyhow};
use eframe::egui;

use crate::cache::{ScanlineCache, TileTexture, TileTextureCache};
use crate::cli::{Backend, PngCompression};
use crate::render::{
    PreviewBitmap, PreviewRequest, Rect, RenderCancel, RenderJob, RenderResult, render_preview,
    save_png,
};
use crate::tiff_info::{ImageInfo, load_info};

const GUI_TILE_SIZE: f32 = 384.0;
const GUI_PREFETCH_TILE_RADIUS: u32 = 1;

struct VisibleTile {
    request: PreviewRequest,
    screen_rect: egui::Rect,
    uv_rect: egui::Rect,
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

pub(crate) fn run_gui(path: Option<PathBuf>) -> Result<()> {
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "GigaTIFF",
        native_options,
        Box::new(move |cc| Ok(Box::new(ViewerApp::new(path.clone(), cc.egui_ctx.clone())))),
    )
    .map_err(|err| anyhow!("GUI error: {err}"))
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
    use tiff::ColorType;
    use tiff::decoder::ChunkType;

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
}
