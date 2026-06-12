use std::ffi::CString;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use lcms2::{DisallowCache, Flags, GlobalContext, Intent, PixelFormat, Profile, Transform};
use openjpeg_sys as opj;
use rayon::prelude::*;

use crate::render::Rect;

type LcmsTransform = Transform<u8, u8, GlobalContext, DisallowCache>;

pub struct OpenJpegBitmap {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub decode: Duration,
    pub convert: Duration,
}

#[derive(Debug, Clone)]
pub struct OpenJpegInfo {
    pub width: u32,
    pub height: u32,
    pub components: Vec<OpenJpegComponentInfo>,
    pub icc_profile_len: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct OpenJpegComponentInfo {
    pub width: u32,
    pub height: u32,
    pub dx: u32,
    pub dy: u32,
    pub precision: u32,
    pub signed: bool,
}

pub fn read_info(path: &Path) -> Result<OpenJpegInfo> {
    let decoder = Decoder::open(path, 1, 0)?;
    unsafe { info_from_image(decoder.image) }
}

pub fn render_region(
    path: &Path,
    rect: Rect,
    out_width: u32,
    out_height: u32,
    threads: i32,
) -> Result<OpenJpegBitmap> {
    let reduce = reduce_factor(rect, out_width, out_height);
    let decode_start = Instant::now();
    let decoder = Decoder::open(path, threads.max(1), reduce)?;
    set_decode_area(decoder.codec, decoder.image, rect)?;

    // SAFETY: `decoder` owns live OpenJPEG codec, stream, and image handles.
    // OpenJPEG reports decode/end failures through integer return codes.
    let decoded = unsafe { opj::opj_decode(decoder.codec, decoder.stream, decoder.image) } != 0;
    // SAFETY: Same handles as above; `opj_end_decompress` finalizes stream
    // state and does not transfer ownership.
    let ended = unsafe { opj::opj_end_decompress(decoder.codec, decoder.stream) } != 0;
    if !decoded || !ended {
        bail!("OpenJPEG FFI decompression failed");
    }
    let decode = decode_start.elapsed();

    let convert_start = Instant::now();
    // SAFETY: `decoder.image` is owned by `decoder` and remains valid until
    // after conversion. `image_to_rgba` validates component pointers/sizes.
    let (width, height, rgba) = unsafe { image_to_rgba(decoder.image, threads.max(1))? };
    let convert = convert_start.elapsed();

    Ok(OpenJpegBitmap {
        width,
        height,
        rgba,
        decode,
        convert,
    })
}

struct Decoder {
    codec: *mut opj::opj_codec_t,
    stream: *mut opj::opj_stream_t,
    image: *mut opj::opj_image_t,
}

impl Decoder {
    fn open(path: &Path, threads: i32, reduce: u32) -> Result<Self> {
        let mut params = default_decoder_parameters();
        params.cp_reduce = reduce;
        params.decod_format = if is_raw_codestream(path) { 0 } else { 1 };

        // SAFETY: OpenJPEG returns either a valid codec pointer for the chosen
        // codestream format or null on allocation/setup failure.
        let codec = RawCodec::new(unsafe {
            opj::opj_create_decompress(if is_raw_codestream(path) {
                opj::CODEC_FORMAT::OPJ_CODEC_J2K
            } else {
                opj::CODEC_FORMAT::OPJ_CODEC_JP2
            })
        })
        .ok_or_else(|| anyhow!("opj_create_decompress failed"))?;

        // SAFETY: `codec` is a live OpenJPEG decoder and `params` was
        // initialized by `opj_set_default_decoder_parameters`.
        let setup_ok = unsafe { opj::opj_setup_decoder(codec.as_ptr(), &mut params) } != 0;
        if !setup_ok {
            bail!("opj_setup_decoder failed");
        }

        if threads > 1 {
            // SAFETY: `codec` remains live; OpenJPEG validates the thread
            // count and reports failure through the return code.
            let ok = unsafe { opj::opj_codec_set_threads(codec.as_ptr(), threads) } != 0;
            if !ok {
                bail!("opj_codec_set_threads failed");
            }
        }

        let c_path = path_to_cstring(path)?;
        // SAFETY: `c_path` is a valid NUL-terminated path and lives for the
        // duration of the call. OpenJPEG copies/opens it immediately.
        let stream = RawStream::new(unsafe {
            opj::opj_stream_create_default_file_stream(c_path.as_ptr(), opj::OPJ_TRUE as i32)
        })
        .ok_or_else(|| anyhow!("opj_stream_create_default_file_stream failed"))?;

        let mut image: *mut opj::opj_image_t = std::ptr::null_mut();
        // SAFETY: `stream` and `codec` are live OpenJPEG objects and `image`
        // points to writable storage for OpenJPEG to return image metadata.
        let ok = unsafe { opj::opj_read_header(stream.as_ptr(), codec.as_ptr(), &mut image) } != 0;
        let Some(image) = RawImage::new(image) else {
            bail!("opj_read_header failed");
        };
        if !ok {
            bail!("opj_read_header failed");
        }

        Ok(Self {
            codec: codec.into_raw(),
            stream: stream.into_raw(),
            image: image.into_raw(),
        })
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: `Decoder` owns these OpenJPEG objects after successful
            // construction. Null checks make partial/manual construction safe.
            if !self.image.is_null() {
                opj::opj_image_destroy(self.image);
            }
            if !self.stream.is_null() {
                opj::opj_stream_destroy(self.stream);
            }
            if !self.codec.is_null() {
                opj::opj_destroy_codec(self.codec);
            }
        }
    }
}

fn default_decoder_parameters() -> opj::opj_dparameters_t {
    let mut params = std::mem::MaybeUninit::<opj::opj_dparameters_t>::zeroed();
    // SAFETY: OpenJPEG's documented initialization path writes default decoder
    // parameters into caller-provided storage.
    unsafe {
        opj::opj_set_default_decoder_parameters(params.as_mut_ptr());
        params.assume_init()
    }
}

struct RawCodec {
    ptr: *mut opj::opj_codec_t,
}

impl RawCodec {
    fn new(ptr: *mut opj::opj_codec_t) -> Option<Self> {
        (!ptr.is_null()).then_some(Self { ptr })
    }

    fn as_ptr(&self) -> *mut opj::opj_codec_t {
        self.ptr
    }

    fn into_raw(self) -> *mut opj::opj_codec_t {
        let ptr = self.ptr;
        std::mem::forget(self);
        ptr
    }
}

impl Drop for RawCodec {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: `RawCodec` owns a non-null pointer returned by
            // `opj_create_decompress` until `into_raw` transfers ownership.
            opj::opj_destroy_codec(self.ptr);
        }
    }
}

struct RawStream {
    ptr: *mut opj::opj_stream_t,
}

impl RawStream {
    fn new(ptr: *mut opj::opj_stream_t) -> Option<Self> {
        (!ptr.is_null()).then_some(Self { ptr })
    }

    fn as_ptr(&self) -> *mut opj::opj_stream_t {
        self.ptr
    }

    fn into_raw(self) -> *mut opj::opj_stream_t {
        let ptr = self.ptr;
        std::mem::forget(self);
        ptr
    }
}

impl Drop for RawStream {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: `RawStream` owns a non-null pointer returned by
            // `opj_stream_create_default_file_stream` until ownership transfer.
            opj::opj_stream_destroy(self.ptr);
        }
    }
}

struct RawImage {
    ptr: *mut opj::opj_image_t,
}

impl RawImage {
    fn new(ptr: *mut opj::opj_image_t) -> Option<Self> {
        (!ptr.is_null()).then_some(Self { ptr })
    }

    fn into_raw(self) -> *mut opj::opj_image_t {
        let ptr = self.ptr;
        std::mem::forget(self);
        ptr
    }
}

impl Drop for RawImage {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: `RawImage` owns a non-null pointer returned by
            // `opj_read_header` until ownership is transferred to `Decoder`.
            opj::opj_image_destroy(self.ptr);
        }
    }
}

fn set_decode_area(
    codec: *mut opj::opj_codec_t,
    image: *mut opj::opj_image_t,
    rect: Rect,
) -> Result<()> {
    let x1 = rect
        .x
        .checked_add(rect.width)
        .ok_or_else(|| anyhow!("OpenJPEG decode window x overflow"))?;
    let y1 = rect
        .y
        .checked_add(rect.height)
        .ok_or_else(|| anyhow!("OpenJPEG decode window y overflow"))?;
    let ok = unsafe {
        // SAFETY: `codec` and `image` are owned by a live `Decoder`, and the
        // window was checked for integer overflow before conversion.
        opj::opj_set_decode_area(
            codec,
            image,
            rect.x as i32,
            rect.y as i32,
            x1 as i32,
            y1 as i32,
        )
    } != 0;
    if !ok {
        bail!("opj_set_decode_area failed");
    }
    Ok(())
}

unsafe fn info_from_image(image: *mut opj::opj_image_t) -> Result<OpenJpegInfo> {
    if image.is_null() {
        bail!("OpenJPEG returned a null image");
    }
    // SAFETY: Null was rejected above and the image is owned by `Decoder`.
    let image_ref = unsafe { &*image };
    if image_ref.numcomps > 0 && image_ref.comps.is_null() {
        bail!("OpenJPEG returned image components without data");
    }
    let comps: &[opj::opj_image_comp_t] = if image_ref.numcomps == 0 {
        &[]
    } else {
        // SAFETY: OpenJPEG reports `numcomps` elements at `comps`, and the
        // non-null pointer case was validated above.
        unsafe { std::slice::from_raw_parts(image_ref.comps, image_ref.numcomps as usize) }
    };
    let components = comps
        .iter()
        .map(|component| OpenJpegComponentInfo {
            width: component.w,
            height: component.h,
            dx: component.dx,
            dy: component.dy,
            precision: component.prec,
            signed: component.sgnd != 0,
        })
        .collect();

    Ok(OpenJpegInfo {
        width: image_ref.x1.saturating_sub(image_ref.x0),
        height: image_ref.y1.saturating_sub(image_ref.y0),
        components,
        icc_profile_len: image_ref.icc_profile_len,
    })
}

unsafe fn image_to_rgba(
    image: *mut opj::opj_image_t,
    decode_threads: i32,
) -> Result<(u32, u32, Vec<u8>)> {
    if image.is_null() {
        bail!("OpenJPEG returned a null image");
    }
    // SAFETY: Null was rejected above and the image is owned by `Decoder`.
    let image_ref = unsafe { &*image };
    if image_ref.numcomps == 0 || image_ref.comps.is_null() {
        bail!("OpenJPEG returned an image without components");
    }

    // SAFETY: `numcomps > 0` and non-null `comps` were validated before
    // constructing this slice.
    let comps = unsafe { std::slice::from_raw_parts(image_ref.comps, image_ref.numcomps as usize) };
    let width = comps[0].w;
    let height = comps[0].h;
    if width == 0 || height == 0 {
        bail!("OpenJPEG returned empty image components");
    }

    // SAFETY: ICC buffer access is validated inside `OpenJpegColorTransform`.
    let color_transform = unsafe { OpenJpegColorTransform::new(image_ref, comps)? };
    let mut rgba = vec![255u8; width as usize * height as usize * 4];
    if let Some(transform) = color_transform {
        transform.write_rgba(comps, width, height, &mut rgba)?;
        return Ok((width, height, rgba));
    }

    let component_views = component_views(comps)?;
    let row_len = width as usize * 4;
    if should_parallel_component_convert(width, height, decode_threads) {
        rgba.par_chunks_mut(row_len)
            .enumerate()
            .try_for_each(|(y, row)| {
                write_component_row(&component_views, y, width, height, row)
            })?;
    } else {
        for (y, row) in rgba.chunks_mut(row_len).enumerate() {
            write_component_row(&component_views, y, width, height, row)?;
        }
    }

    Ok((width, height, rgba))
}

#[derive(Clone, Copy)]
struct ComponentView {
    data_addr: usize,
    width: u32,
    height: u32,
    precision: u32,
    signed: bool,
}

fn component_views(components: &[opj::opj_image_comp_t]) -> Result<Vec<ComponentView>> {
    components
        .iter()
        .map(|component| {
            if component.data.is_null() || component.w == 0 || component.h == 0 {
                bail!("OpenJPEG returned a component without sample data");
            }
            Ok(ComponentView {
                data_addr: component.data as usize,
                width: component.w,
                height: component.h,
                precision: component.prec,
                signed: component.sgnd != 0,
            })
        })
        .collect()
}

fn should_parallel_component_convert(width: u32, height: u32, decode_threads: i32) -> bool {
    decode_threads <= 1
        && width as usize * height as usize >= 256 * 256
        && rayon::current_num_threads() > 1
}

fn write_component_row(
    components: &[ComponentView],
    y: usize,
    width: u32,
    height: u32,
    row: &mut [u8],
) -> Result<()> {
    if components.len() >= 3 {
        for (x, rgba) in row.chunks_exact_mut(4).enumerate() {
            rgba[0] = component_view_sample_to_u8(&components[0], x, y, width, height)?;
            rgba[1] = component_view_sample_to_u8(&components[1], x, y, width, height)?;
            rgba[2] = component_view_sample_to_u8(&components[2], x, y, width, height)?;
            rgba[3] = 255;
        }
    } else {
        let component = components
            .first()
            .ok_or_else(|| anyhow!("OpenJPEG returned an image without components"))?;
        for (x, rgba) in row.chunks_exact_mut(4).enumerate() {
            let gray = component_view_sample_to_u8(component, x, y, width, height)?;
            rgba[0] = gray;
            rgba[1] = gray;
            rgba[2] = gray;
            rgba[3] = 255;
        }
    }
    Ok(())
}

fn component_view_sample_to_u8(
    component: &ComponentView,
    x: usize,
    y: usize,
    out_width: u32,
    out_height: u32,
) -> Result<u8> {
    let cx = (x as u64 * component.width as u64 / out_width as u64)
        .min(component.width.saturating_sub(1) as u64) as usize;
    let cy = (y as u64 * component.height as u64 / out_height as u64)
        .min(component.height.saturating_sub(1) as u64) as usize;
    let index = cy
        .checked_mul(component.width as usize)
        .and_then(|base| base.checked_add(cx))
        .ok_or_else(|| anyhow!("OpenJPEG component index overflow"))?;
    let data = component.data_addr as *const i32;
    // SAFETY: `ComponentView` is created only from non-null OpenJPEG component
    // data. The index is bounded by component width/height above.
    let sample = unsafe { *data.add(index) };
    Ok(sample_to_u8(sample, component.precision, component.signed))
}

enum OpenJpegColorTransform {
    Gray8(LcmsTransform),
    Gray16(LcmsTransform),
    Rgb8(LcmsTransform),
    Rgb16(LcmsTransform),
}

impl OpenJpegColorTransform {
    unsafe fn new(
        image: &opj::opj_image_t,
        components: &[opj::opj_image_comp_t],
    ) -> Result<Option<Self>> {
        if image.icc_profile_buf.is_null() || image.icc_profile_len == 0 {
            return Ok(None);
        }

        let Some(first) = components.first() else {
            return Ok(None);
        };
        let color_components = if components.len() >= 3 { 3 } else { 1 };
        let precision = first.prec;
        if precision == 0 || precision > 16 {
            return Ok(None);
        }

        let input_format = match (color_components, precision <= 8) {
            (1, true) => PixelFormat::GRAY_8,
            (1, false) => PixelFormat::GRAY_16,
            (3, true) => PixelFormat::RGB_8,
            (3, false) => PixelFormat::RGB_16,
            _ => return Ok(None),
        };

        // SAFETY: The ICC pointer is non-null and its length is non-zero as
        // checked above. The profile buffer is owned by the OpenJPEG image and
        // remains valid for the duration of transform construction.
        let icc_profile = unsafe {
            std::slice::from_raw_parts(image.icc_profile_buf, image.icc_profile_len as usize)
        };
        let input = Profile::new_icc(icc_profile).context("reading JPEG2000 ICC profile")?;
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
        .context("creating lcms2 JPEG2000 sRGB transform")?;

        Ok(Some(match (color_components, precision <= 8) {
            (1, true) => Self::Gray8(transform),
            (1, false) => Self::Gray16(transform),
            (3, true) => Self::Rgb8(transform),
            (3, false) => Self::Rgb16(transform),
            _ => unreachable!("input format was checked above"),
        }))
    }

    fn write_rgba(
        &self,
        components: &[opj::opj_image_comp_t],
        width: u32,
        height: u32,
        rgba: &mut [u8],
    ) -> Result<()> {
        let color_components = self.color_components();
        for component in &components[..color_components] {
            if component.data.is_null() || component.w == 0 || component.h == 0 {
                bail!("OpenJPEG returned a component without sample data");
            }
        }

        let row_samples = width as usize * color_components * self.bytes_per_sample();
        let mut input = vec![0u8; row_samples];
        let mut rgb = vec![0u8; width as usize * 3];
        for y in 0..height as usize {
            self.write_input_row(components, width, height, y, &mut input)?;
            self.transform_pixels(&input, &mut rgb);
            let row_offset = y * width as usize * 4;
            for (x, rgb_px) in rgb.chunks_exact(3).enumerate() {
                let dst = row_offset + x * 4;
                rgba[dst..dst + 3].copy_from_slice(rgb_px);
                rgba[dst + 3] = 255;
            }
        }

        Ok(())
    }

    fn write_input_row(
        &self,
        components: &[opj::opj_image_comp_t],
        out_width: u32,
        out_height: u32,
        y: usize,
        input: &mut [u8],
    ) -> Result<()> {
        let color_components = self.color_components();
        let bytes_per_sample = self.bytes_per_sample();
        for x in 0..out_width as usize {
            for component_index in 0..color_components {
                let component = &components[component_index];
                let sample = component_sample(component, x, y, out_width, out_height)
                    .with_context(|| {
                        format!("reading OpenJPEG component {component_index} sample")
                    })?;
                let dst = (x * color_components + component_index) * bytes_per_sample;
                if bytes_per_sample == 1 {
                    input[dst] = sample_to_u8(sample, component.prec, component.sgnd != 0);
                } else {
                    let value = sample_to_u16(sample, component.prec, component.sgnd != 0);
                    input[dst..dst + 2].copy_from_slice(&value.to_ne_bytes());
                }
            }
        }
        Ok(())
    }

    fn transform_pixels(&self, input: &[u8], rgb: &mut [u8]) {
        match self {
            Self::Gray8(transform)
            | Self::Gray16(transform)
            | Self::Rgb8(transform)
            | Self::Rgb16(transform) => transform.transform_pixels(input, rgb),
        }
    }

    fn color_components(&self) -> usize {
        match self {
            Self::Gray8(_) | Self::Gray16(_) => 1,
            Self::Rgb8(_) | Self::Rgb16(_) => 3,
        }
    }

    fn bytes_per_sample(&self) -> usize {
        match self {
            Self::Gray8(_) | Self::Rgb8(_) => 1,
            Self::Gray16(_) | Self::Rgb16(_) => 2,
        }
    }
}

fn component_sample(
    component: &opj::opj_image_comp_t,
    x: usize,
    y: usize,
    out_width: u32,
    out_height: u32,
) -> Result<i32> {
    if component.data.is_null() || component.w == 0 || component.h == 0 {
        bail!("OpenJPEG returned a component without sample data");
    }
    let cx = (x as u64 * component.w as u64 / out_width as u64)
        .min(component.w.saturating_sub(1) as u64) as usize;
    let cy = (y as u64 * component.h as u64 / out_height as u64)
        .min(component.h.saturating_sub(1) as u64) as usize;
    let index = cy
        .checked_mul(component.w as usize)
        .and_then(|base| base.checked_add(cx))
        .ok_or_else(|| anyhow!("OpenJPEG component index overflow"))?;
    // SAFETY: The component data pointer and dimensions were validated before
    // sampling, and the computed index is within those dimensions.
    Ok(unsafe { *component.data.add(index) })
}

fn sample_to_u8(sample: i32, precision: u32, signed: bool) -> u8 {
    let precision = precision.clamp(1, 31);
    let value = if signed {
        sample.saturating_add(1i32.checked_shl(precision.saturating_sub(1)).unwrap_or(0))
    } else {
        sample
    };
    let max_value = ((1u64 << precision) - 1).max(1);
    let clamped = value.clamp(0, max_value as i32) as u64;
    ((clamped * 255 + max_value / 2) / max_value) as u8
}

fn sample_to_u16(sample: i32, precision: u32, signed: bool) -> u16 {
    let precision = precision.clamp(1, 31);
    let value = if signed {
        sample.saturating_add(1i32.checked_shl(precision.saturating_sub(1)).unwrap_or(0))
    } else {
        sample
    };
    let max_value = ((1u64 << precision) - 1).max(1);
    let clamped = value.clamp(0, max_value as i32) as u64;
    ((clamped * 65535 + max_value / 2) / max_value) as u16
}

fn reduce_factor(rect: Rect, out_width: u32, out_height: u32) -> u32 {
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

fn path_to_cstring(path: &Path) -> Result<CString> {
    CString::new(path.to_string_lossy().as_bytes())
        .with_context(|| format!("path contains NUL byte: {}", path.display()))
}

fn is_raw_codestream(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| matches!(ext.to_ascii_lowercase().as_str(), "j2k" | "j2c" | "jpc"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduce_factor_selects_power_of_two_level() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 4096,
            height: 2048,
        };

        assert_eq!(reduce_factor(rect, 4096, 2048), 0);
        assert_eq!(reduce_factor(rect, 2048, 1024), 1);
        assert_eq!(reduce_factor(rect, 512, 256), 3);
    }

    #[test]
    fn reduce_factor_uses_smaller_axis_scale() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 4096,
            height: 2048,
        };

        assert_eq!(reduce_factor(rect, 1024, 2048), 0);
        assert_eq!(reduce_factor(rect, 1024, 512), 2);
    }
}
