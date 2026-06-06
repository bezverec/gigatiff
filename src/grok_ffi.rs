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

pub(crate) struct GrokFfiBitmap {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
    pub(crate) decode: Duration,
    pub(crate) convert: Duration,
}

pub(crate) fn render_region(
    path: &Path,
    rect: Rect,
    out_width: u32,
    out_height: u32,
) -> Result<GrokFfiBitmap> {
    ensure_initialized();

    let decode_start = Instant::now();
    let mut stream = unsafe { std::mem::zeroed::<grk::grk_stream_params>() };
    set_stream_path(&mut stream, path)?;
    stream.is_read_stream = true;

    let mut params = unsafe { std::mem::zeroed::<grk::grk_decompress_parameters>() };
    params.core.tile_cache_strategy = grk::GRK_TILE_CACHE_NONE;
    params.force_rgb = true;
    params.upsample = true;

    let codec = CodecGuard::new(unsafe { grk::grk_decompress_init(&mut stream, &mut params) })
        .with_context(|| format!("initializing Grok decompressor for {}", path.display()))?;

    let mut header = unsafe { std::mem::zeroed::<grk::grk_header_info>() };
    header.force_rgb = true;
    header.upsample = true;
    if !unsafe { grk::grk_decompress_read_header(codec.as_ptr(), &mut header) } {
        bail!("reading JPEG2000 header through Grok FFI failed");
    }
    params.core.reduce = reduce_factor(rect, out_width, out_height, header.numresolutions) as u8;
    let (x0, y0, x1, y1) = decode_window(rect, &header.header_image);
    params.dw_x0 = x0 as f64;
    params.dw_y0 = y0 as f64;
    params.dw_x1 = x1 as f64;
    params.dw_y1 = y1 as f64;
    params.dw_reduced = false;
    if !unsafe { grk::grk_decompress_update(&mut params, codec.as_ptr()) } {
        bail!("updating Grok FFI decode window failed");
    }
    if !unsafe { grk::grk_decompress(codec.as_ptr(), ptr::null_mut()) } {
        bail!("Grok FFI decompression failed");
    }

    let image = unsafe { grk::grk_decompress_get_image(codec.as_ptr()) };
    let decode = decode_start.elapsed();

    let convert_start = Instant::now();
    let (width, height, rgba) = unsafe { image_to_rgba(image)? };
    let convert = convert_start.elapsed();

    Ok(GrokFfiBitmap {
        width,
        height,
        rgba,
        decode,
        convert,
    })
}

fn ensure_initialized() {
    INIT.call_once(|| unsafe {
        grk::grk_initialize(ptr::null::<c_char>(), 1, ptr::null_mut());
    });
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

fn reduce_factor(rect: Rect, out_width: u32, out_height: u32, numresolutions: u8) -> u32 {
    let width_scale = rect.width.max(1) / out_width.max(1);
    let height_scale = rect.height.max(1) / out_height.max(1);
    let mut scale = width_scale.min(height_scale);
    let mut reduce = 0;
    while scale >= 2 && reduce < 8 {
        reduce += 1;
        scale /= 2;
    }
    reduce.min(numresolutions.saturating_sub(1) as u32)
}

fn decode_window(rect: Rect, image: &grk::grk_image) -> (u32, u32, u32, u32) {
    let x0 = rect.x.max(image.x0);
    let y0 = rect.y.max(image.y0);
    let x1 = rect.x.saturating_add(rect.width).min(image.x1);
    let y1 = rect.y.saturating_add(rect.height).min(image.y1);
    (x0, y0, x1, y1)
}

unsafe fn image_to_rgba(image: *mut grk::grk_image) -> Result<(u32, u32, Vec<u8>)> {
    if image.is_null() {
        bail!("Grok returned a null image");
    }
    let image_ref = unsafe { &*image };
    let components = image_ref.numcomps as usize;
    if components == 0 || image_ref.comps.is_null() {
        bail!("Grok returned an image without components");
    }
    let comps = unsafe { std::slice::from_raw_parts(image_ref.comps, components) };
    let width = comps[0].w;
    let height = comps[0].h;
    if width == 0 || height == 0 {
        bail!("Grok returned empty image components");
    }
    if comps[0].data.is_null() {
        bail!("Grok returned a component without sample data");
    }

    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    let color_components = components.min(3);
    for y in 0..height as usize {
        for x in 0..width as usize {
            if color_components == 1 {
                let gray = component_sample_to_u8(&comps[0], x, y)?;
                rgba.extend_from_slice(&[gray, gray, gray, 255]);
            } else {
                let r = component_sample_to_u8(&comps[0], x, y)?;
                let g = component_sample_to_u8(&comps[1], x, y)?;
                let b = component_sample_to_u8(&comps[2], x, y)?;
                rgba.extend_from_slice(&[r, g, b, 255]);
            }
        }
    }

    Ok((width, height, rgba))
}

fn component_sample_to_u8(component: &grk::grk_image_comp, x: usize, y: usize) -> Result<u8> {
    if component.data.is_null() {
        bail!("Grok returned a component without sample data");
    }
    if x >= component.w as usize || y >= component.h as usize {
        bail!("Grok component dimensions do not match");
    }
    let index = y
        .checked_mul(component.stride as usize)
        .and_then(|base| base.checked_add(x))
        .ok_or_else(|| anyhow!("Grok component index overflow"))?;
    let sample = unsafe { component_sample(component, index) };
    let value = if component.sgnd {
        let bias = 1i64
            .checked_shl(component.prec.saturating_sub(1) as u32)
            .unwrap_or(0);
        sample.saturating_add(bias).max(0) as u64
    } else {
        sample.max(0) as u64
    };
    let max_value = if component.prec == 0 {
        1
    } else {
        (1u64.checked_shl(component.prec.min(63) as u32).unwrap_or(0)).saturating_sub(1)
    }
    .max(1);
    Ok(((value.min(max_value) * 255) / max_value) as u8)
}

unsafe fn component_sample(component: &grk::grk_image_comp, index: usize) -> i64 {
    match component.data_type {
        2 => unsafe { *(component.data as *const i8).add(index) as i64 },
        1 => unsafe { *(component.data as *const i16).add(index) as i64 },
        _ => unsafe { *(component.data as *const i32).add(index) as i64 },
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
            grk::grk_object_unref(self.ptr);
        }
    }
}
