use std::cmp::{max, min};
use std::ffi::CString;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::os::raw::{c_char, c_int, c_uint, c_ushort, c_void};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rayon::prelude::*;
use tiff::ColorType;
use tiff::decoder::{ChunkType, Decoder, DecodingResult};

use crate::cache::{ScanlineCache, ScanlineKey};
use crate::cli::{Backend, PngCompression};
use crate::color::{ColorTransform, bits_for_color, samples_for_color, write_sampled_row_rgba};
use crate::tiff_info::{ImageInfo, can_read_raw_strips, open_decoder};

const PARALLEL_ROW_BATCH: usize = 32;

#[derive(Debug)]
pub(crate) struct PreviewBitmap {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
    pub(crate) source: &'static str,
    pub(crate) decoded_chunks: u32,
    pub(crate) stats: RenderStats,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RenderStats {
    pub(crate) total: Duration,
    pub(crate) read: Duration,
    pub(crate) convert: Duration,
    pub(crate) decode: Duration,
    pub(crate) blit: Duration,
    pub(crate) scanline_cache_hits: u32,
    pub(crate) scanline_cache_misses: u32,
}

impl RenderStats {
    pub(crate) fn short_label(self) -> String {
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

pub(crate) fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[derive(Debug)]
pub(crate) struct RenderedPixels {
    pub(crate) rgba: Vec<u8>,
    pub(crate) stats: RenderStats,
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
pub(crate) struct PreviewRequest {
    pub(crate) path: PathBuf,
    pub(crate) rect: Rect,
    pub(crate) max_output: u32,
    pub(crate) backend: Backend,
}

#[derive(Debug)]
pub(crate) struct RenderJob {
    pub(crate) request: PreviewRequest,
    pub(crate) info: ImageInfo,
    pub(crate) max_chunk_mb: usize,
    pub(crate) generation: u64,
}

#[derive(Debug)]
pub(crate) struct RenderResult {
    pub(crate) request: PreviewRequest,
    pub(crate) result: Result<PreviewBitmap>,
}

pub(crate) struct RenderCancel {
    pub(crate) latest_generation: Arc<AtomicU64>,
    pub(crate) generation: u64,
}

impl RenderCancel {
    pub(crate) fn check(&self) -> Result<()> {
        if self.latest_generation.load(Ordering::Relaxed) != self.generation {
            bail!("render cancelled");
        }
        Ok(())
    }
}

pub(crate) fn render_preview(
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

pub(crate) fn save_png(
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
pub(crate) struct Rect {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

pub(crate) fn clamp_rect(
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    image_w: u32,
    image_h: u32,
) -> Result<Rect> {
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

pub(crate) fn fit_size(width: u32, height: u32, max_output: u32) -> (u32, u32) {
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
