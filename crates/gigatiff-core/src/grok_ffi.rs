use std::ffi::CString;
use std::os::raw::c_char;
use std::path::Path;
use std::ptr;
use std::sync::Once;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use grokj2k_sys as grk;

use crate::render::Rect;

static INIT: Once = Once::new();

pub struct GrokFfiBitmap {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub color_space: u32,
    pub decode: Duration,
    pub convert: Duration,
}

/// Decode a full-resolution source `rect` and return RGBA pixels.
///
/// `out_width` and `out_height` are output-size hints used only to select the
/// closest JPEG 2000 reduction level. Grok decides the actual decoded bitmap
/// dimensions for the selected window and reduction; callers should resample the
/// returned bitmap when they need an exact output size.
pub fn render_region(
    path: &Path,
    rect: Rect,
    out_width: u32,
    out_height: u32,
) -> Result<GrokFfiBitmap> {
    ensure_initialized();

    let decode_start = Instant::now();
    let mut stream = zeroed_stream_params();
    set_stream_path(&mut stream, path)?;
    stream.is_read_stream = true;

    let mut params = zeroed_decompress_parameters();
    params.core.tile_cache_strategy = grk::GRK_TILE_CACHE_NONE;
    apply_rgb_upsample_to_params(&mut params);

    // SAFETY: `stream` and `params` are Grok C structs initialized according
    // to the Grok CLI defaults: zeroed storage followed by explicit fields.
    // Both pointers remain valid for the duration of this call.
    let codec = CodecGuard::new(unsafe { grk::grk_decompress_init(&mut stream, &mut params) })
        .with_context(|| format!("initializing Grok decompressor for {}", path.display()))?;

    let mut header = zeroed_header_info();
    apply_rgb_upsample_to_header(&mut header);
    // SAFETY: `codec` is a non-null Grok decompressor owned by `CodecGuard`,
    // and `header` points to writable storage for Grok to populate.
    if !unsafe { grk::grk_decompress_read_header(codec.as_ptr(), &mut header) } {
        bail!("reading JPEG2000 header through Grok FFI failed");
    }
    params.core.reduce = reduce_factor(rect, out_width, out_height, header.numresolutions)? as u8;
    let (x0, y0, x1, y1) = decode_window(rect, &header.header_image)?;
    params.dw_x0 = x0 as f64;
    params.dw_y0 = y0 as f64;
    params.dw_x1 = x1 as f64;
    params.dw_y1 = y1 as f64;
    params.dw_reduced = false;
    // SAFETY: `codec` is still alive and `params` is the same parameter
    // object family used to create it, with a validated decode window.
    if !unsafe { grk::grk_decompress_update(&mut params, codec.as_ptr()) } {
        bail!("updating Grok FFI decode window failed");
    }
    // SAFETY: Grok owns all internal decode state behind `codec`; the optional
    // plugin callback pointer is null because GigaTIFF does not install one.
    if !unsafe { grk::grk_decompress(codec.as_ptr(), ptr::null_mut()) } {
        bail!("Grok FFI decompression failed");
    }

    // SAFETY: Grok returns a borrowed image pointer tied to the decompressor.
    // `CodecGuard` is kept alive until after RGBA conversion.
    let image = unsafe { grk::grk_decompress_get_image(codec.as_ptr()) };
    let decode = decode_start.elapsed();

    let convert_start = Instant::now();
    // SAFETY: `image` is checked for null and component validity inside
    // `image_to_rgba`; the borrowed image does not outlive `codec`.
    let (width, height, color_space, rgba) = unsafe { image_to_rgba(image)? };
    let convert = convert_start.elapsed();

    Ok(GrokFfiBitmap {
        width,
        height,
        rgba,
        color_space,
        decode,
        convert,
    })
}

fn ensure_initialized() {
    INIT.call_once(|| unsafe {
        // SAFETY: Grok global initialization is documented as process-global.
        // `Once` guarantees this call runs at most once.
        grk::grk_initialize(ptr::null::<c_char>(), 1, ptr::null_mut());
    });
}

fn zeroed_stream_params() -> grk::grk_stream_params {
    // SAFETY: Grok's C API expects `grk_stream_params` to be zero-initialized
    // before callers fill the file path and stream direction fields.
    unsafe { std::mem::zeroed() }
}

fn zeroed_decompress_parameters() -> grk::grk_decompress_parameters {
    // SAFETY: This mirrors Grok's CLI/default initialization pattern for
    // plain-old-data decoder parameters; fields are then set explicitly.
    unsafe { std::mem::zeroed() }
}

fn zeroed_header_info() -> grk::grk_header_info {
    // SAFETY: Grok writes the whole `grk_header_info` structure in
    // `grk_decompress_read_header`; selected flags are set before the call.
    unsafe { std::mem::zeroed() }
}

fn apply_rgb_upsample_to_params(params: &mut grk::grk_decompress_parameters) {
    params.force_rgb = true;
    params.upsample = true;
}

fn apply_rgb_upsample_to_header(header: &mut grk::grk_header_info) {
    // Grok applies these flags while reading the header. Keep the matching
    // decompressor parameters in sync for grk_decompress_update and future
    // decode paths.
    header.force_rgb = true;
    header.upsample = true;
}

fn set_stream_path(stream: &mut grk::grk_stream_params, path: &Path) -> Result<()> {
    let c_path = path_to_cstring(path)?;
    let bytes = c_path.as_bytes_with_nul();
    if bytes.len() > stream.file.len() {
        bail!("path is too long for Grok: {}", path.display());
    }
    for (target, source) in stream.file.iter_mut().zip(bytes.iter().copied()) {
        *target = source as c_char;
    }
    Ok(())
}

fn path_to_cstring(path: &Path) -> Result<CString> {
    CString::new(path.to_string_lossy().as_bytes())
        .with_context(|| format!("path contains NUL byte: {}", path.display()))
}

fn reduce_factor(rect: Rect, out_width: u32, out_height: u32, numresolutions: u8) -> Result<u32> {
    if numresolutions == 0 {
        bail!("Grok reported a JPEG2000 image without resolution levels");
    }

    let width_scale = rect.width.max(1) / out_width.max(1);
    let height_scale = rect.height.max(1) / out_height.max(1);
    let mut scale = width_scale.min(height_scale);
    let mut reduce = 0;
    while scale >= 2 && reduce < 8 {
        reduce += 1;
        scale /= 2;
    }
    Ok(reduce.min(numresolutions.saturating_sub(1) as u32))
}

fn decode_window(rect: Rect, image: &grk::grk_image) -> Result<(u32, u32, u32, u32)> {
    let x0 = rect.x.max(image.x0);
    let y0 = rect.y.max(image.y0);
    let x1 = rect.x.saturating_add(rect.width).min(image.x1);
    let y1 = rect.y.saturating_add(rect.height).min(image.y1);
    if x1 <= x0 || y1 <= y0 {
        bail!("requested JPEG2000 region does not intersect image bounds");
    }
    Ok((x0, y0, x1, y1))
}

unsafe fn image_to_rgba(image: *mut grk::grk_image) -> Result<(u32, u32, u32, Vec<u8>)> {
    if image.is_null() {
        bail!("Grok returned a null image");
    }
    // SAFETY: Null was rejected above; the pointer is borrowed from Grok and
    // remains valid while the owning decompressor is alive.
    let image_ref = unsafe { &*image };
    let components = image_ref.numcomps as usize;
    if components == 0 || image_ref.comps.is_null() {
        bail!("Grok returned an image without components");
    }
    // SAFETY: Grok reports `numcomps` elements at `comps`; null and zero
    // component cases were rejected before constructing the slice.
    let comps = unsafe { std::slice::from_raw_parts(image_ref.comps, components) };
    let width = comps[0].w;
    let height = comps[0].h;
    if width == 0 || height == 0 {
        bail!("Grok returned empty image components");
    }

    let color_components = match components {
        1 => 1,
        3.. => 3,
        _ => bail!("Grok returned unsupported component count: {components}"),
    };
    let views = ComponentViews::new(comps, color_components, width, height)?;

    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow!("Grok output dimensions overflow"))?;
    let mut rgba = Vec::with_capacity(
        pixel_count
            .checked_mul(4)
            .ok_or_else(|| anyhow!("Grok output buffer size overflow"))?,
    );

    if color_components == 1 {
        let gray = &views.components[0];
        for y in 0..height as usize {
            for x in 0..width as usize {
                // SAFETY: Loop bounds are derived from the validated component
                // dimensions stored in `ComponentView`.
                let gray = unsafe { gray.sample_to_u8_unchecked(x, y) };
                rgba.push(gray);
                rgba.push(gray);
                rgba.push(gray);
                rgba.push(255);
            }
        }
    } else {
        let red = &views.components[0];
        let green = &views.components[1];
        let blue = &views.components[2];
        for y in 0..height as usize {
            for x in 0..width as usize {
                // SAFETY: Loop bounds are derived from the validated component
                // dimensions stored in `ComponentView`.
                rgba.push(unsafe { red.sample_to_u8_unchecked(x, y) });
                rgba.push(unsafe { green.sample_to_u8_unchecked(x, y) });
                rgba.push(unsafe { blue.sample_to_u8_unchecked(x, y) });
                rgba.push(255);
            }
        }
    }

    Ok((width, height, image_ref.color_space as u32, rgba))
}

struct ComponentViews {
    components: Vec<ComponentView>,
}

impl ComponentViews {
    fn new(
        comps: &[grk::grk_image_comp],
        color_components: usize,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let mut components = Vec::with_capacity(color_components);
        for (index, component) in comps.iter().take(color_components).enumerate() {
            components.push(
                ComponentView::new(component, width, height)
                    .with_context(|| format!("validating Grok component {index}"))?,
            );
        }
        Ok(Self { components })
    }
}

struct ComponentView {
    data: *const std::ffi::c_void,
    width: usize,
    height: usize,
    stride: usize,
    signed: bool,
    signed_bias: i64,
    max_value: u64,
    reader: SampleReader,
}

impl ComponentView {
    fn new(
        component: &grk::grk_image_comp,
        expected_width: u32,
        expected_height: u32,
    ) -> Result<Self> {
        if component.data.is_null() {
            bail!("Grok returned a component without sample data");
        }
        if component.w != expected_width || component.h != expected_height {
            bail!(
                "Grok component dimensions do not match: got {}x{}, expected {}x{}",
                component.w,
                component.h,
                expected_width,
                expected_height
            );
        }

        let width = component.w as usize;
        let height = component.h as usize;
        let stride = component.stride as usize;
        if stride < width {
            bail!("Grok component stride is smaller than its width");
        }
        height
            .saturating_sub(1)
            .checked_mul(stride)
            .and_then(|base| base.checked_add(width.saturating_sub(1)))
            .ok_or_else(|| anyhow!("Grok component index range overflow"))?;

        let precision = component.prec.clamp(1, 63);
        let signed_bias = if component.sgnd {
            1i64.checked_shl(precision.saturating_sub(1) as u32)
                .unwrap_or(0)
        } else {
            0
        };
        let max_value = (1u64.checked_shl(precision as u32).unwrap_or(0))
            .saturating_sub(1)
            .max(1);

        Ok(Self {
            data: component.data,
            width,
            height,
            stride,
            signed: component.sgnd,
            signed_bias,
            max_value,
            reader: SampleReader::new(component.data_type)?,
        })
    }

    unsafe fn sample_to_u8_unchecked(&self, x: usize, y: usize) -> u8 {
        debug_assert!(x < self.width);
        debug_assert!(y < self.height);
        let index = y * self.stride + x;
        // SAFETY: `ComponentView::new` validated a non-null data pointer,
        // compatible data type, stride >= width, and maximum index range.
        let sample = unsafe { self.reader.read(self.data, index) };
        let value = if self.signed {
            sample.saturating_add(self.signed_bias).max(0) as u64
        } else {
            sample.max(0) as u64
        };
        ((value.min(self.max_value) * 255) / self.max_value) as u8
    }
}

#[derive(Clone, Copy)]
enum SampleReader {
    I32,
    I16,
    I8,
}

impl SampleReader {
    fn new(data_type: grk::grk_data_type) -> Result<Self> {
        // Mirrors Grok's public grk_data_type enum: INT_32=0, INT_16=1,
        // INT_8=2, FLOAT=3, DOUBLE=4. Float outputs are not valid for the
        // current RGBA conversion path.
        match data_type {
            0 => Ok(Self::I32),
            1 => Ok(Self::I16),
            2 => Ok(Self::I8),
            3 | 4 => bail!("Grok returned unsupported floating-point component data"),
            other => bail!("Grok returned unknown component data type: {other}"),
        }
    }

    unsafe fn read(self, data: *const std::ffi::c_void, index: usize) -> i64 {
        match self {
            // SAFETY: The caller selects the reader from Grok's component
            // data_type and validates that `index` is in-bounds for the buffer.
            Self::I8 => unsafe { *(data as *const i8).add(index) as i64 },
            Self::I16 => unsafe { *(data as *const i16).add(index) as i64 },
            Self::I32 => unsafe { *(data as *const i32).add(index) as i64 },
        }
    }
}

struct CodecGuard {
    ptr: *mut grk::grk_object,
}

impl CodecGuard {
    fn new(ptr: *mut grk::grk_object) -> Option<Self> {
        (!ptr.is_null()).then_some(Self { ptr })
    }

    fn as_ptr(&self) -> *mut grk::grk_object {
        self.ptr
    }
}

impl Drop for CodecGuard {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: `CodecGuard` owns exactly one non-null Grok object
            // returned by `grk_decompress_init`.
            grk::grk_object_unref(self.ptr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduce_factor_is_clamped_to_available_resolutions() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 4096,
            height: 4096,
        };

        assert_eq!(reduce_factor(rect, 4096, 4096, 6).unwrap(), 0);
        assert_eq!(reduce_factor(rect, 512, 512, 6).unwrap(), 3);
        assert_eq!(reduce_factor(rect, 1, 1, 3).unwrap(), 2);
        assert!(reduce_factor(rect, 512, 512, 0).is_err());
    }

    #[test]
    fn decode_window_rejects_non_intersecting_rectangles() {
        let mut image = zeroed_image();
        image.x0 = 10;
        image.y0 = 20;
        image.x1 = 110;
        image.y1 = 220;

        assert_eq!(
            decode_window(
                Rect {
                    x: 0,
                    y: 0,
                    width: 20,
                    height: 40,
                },
                &image
            )
            .unwrap(),
            (10, 20, 20, 40)
        );
        assert!(
            decode_window(
                Rect {
                    x: 0,
                    y: 0,
                    width: 5,
                    height: 5,
                },
                &image
            )
            .is_err()
        );
    }

    fn zeroed_image() -> grk::grk_image {
        // SAFETY: Tests only fill and read the image extent fields used by
        // `decode_window`; no Grok API observes this synthetic value.
        unsafe { std::mem::zeroed() }
    }
}
