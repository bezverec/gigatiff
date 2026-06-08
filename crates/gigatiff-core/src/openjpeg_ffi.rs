use std::ffi::CString;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use lcms2::{DisallowCache, Flags, GlobalContext, Intent, PixelFormat, Profile, Transform};
use openjpeg_sys as opj;

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
) -> Result<OpenJpegBitmap> {
    let reduce = reduce_factor(rect, out_width, out_height);
    let decode_start = Instant::now();
    let decoder = Decoder::open(path, 1, reduce)?;
    set_decode_area(decoder.codec, decoder.image, rect)?;

    let decoded = unsafe { opj::opj_decode(decoder.codec, decoder.stream, decoder.image) } != 0;
    let ended = unsafe { opj::opj_end_decompress(decoder.codec, decoder.stream) } != 0;
    if !decoded || !ended {
        bail!("OpenJPEG FFI decompression failed");
    }
    let decode = decode_start.elapsed();

    let convert_start = Instant::now();
    let (width, height, rgba) = unsafe { image_to_rgba(decoder.image)? };
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
        let mut params = unsafe {
            let mut p = std::mem::MaybeUninit::<opj::opj_dparameters_t>::zeroed();
            opj::opj_set_default_decoder_parameters(p.as_mut_ptr());
            p.assume_init()
        };
        params.cp_reduce = reduce;
        params.decod_format = if is_raw_codestream(path) { 0 } else { 1 };

        let codec = unsafe {
            opj::opj_create_decompress(if is_raw_codestream(path) {
                opj::CODEC_FORMAT::OPJ_CODEC_J2K
            } else {
                opj::CODEC_FORMAT::OPJ_CODEC_JP2
            })
        };
        if codec.is_null() {
            bail!("opj_create_decompress failed");
        }

        let setup_ok = unsafe { opj::opj_setup_decoder(codec, &mut params) } != 0;
        if !setup_ok {
            unsafe { opj::opj_destroy_codec(codec) };
            bail!("opj_setup_decoder failed");
        }

        if threads > 1 {
            let ok = unsafe { opj::opj_codec_set_threads(codec, threads) } != 0;
            if !ok {
                unsafe { opj::opj_destroy_codec(codec) };
                bail!("opj_codec_set_threads failed");
            }
        }

        let c_path = path_to_cstring(path)?;
        let stream = unsafe {
            opj::opj_stream_create_default_file_stream(c_path.as_ptr(), opj::OPJ_TRUE as i32)
        };
        if stream.is_null() {
            unsafe { opj::opj_destroy_codec(codec) };
            bail!("opj_stream_create_default_file_stream failed");
        }

        let mut image: *mut opj::opj_image_t = std::ptr::null_mut();
        let ok = unsafe { opj::opj_read_header(stream, codec, &mut image) } != 0;
        if !ok || image.is_null() {
            unsafe {
                opj::opj_stream_destroy(stream);
                opj::opj_destroy_codec(codec);
            }
            bail!("opj_read_header failed");
        }

        Ok(Self {
            codec,
            stream,
            image,
        })
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
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
    let image_ref = unsafe { &*image };
    if image_ref.numcomps > 0 && image_ref.comps.is_null() {
        bail!("OpenJPEG returned image components without data");
    }
    let comps = unsafe { std::slice::from_raw_parts(image_ref.comps, image_ref.numcomps as usize) };
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

unsafe fn image_to_rgba(image: *mut opj::opj_image_t) -> Result<(u32, u32, Vec<u8>)> {
    if image.is_null() {
        bail!("OpenJPEG returned a null image");
    }
    let image_ref = unsafe { &*image };
    if image_ref.numcomps == 0 || image_ref.comps.is_null() {
        bail!("OpenJPEG returned an image without components");
    }

    let comps = unsafe { std::slice::from_raw_parts(image_ref.comps, image_ref.numcomps as usize) };
    let width = comps[0].w;
    let height = comps[0].h;
    if width == 0 || height == 0 {
        bail!("OpenJPEG returned empty image components");
    }

    let color_transform = unsafe { OpenJpegColorTransform::new(image_ref, comps)? };
    let mut rgba = vec![255u8; width as usize * height as usize * 4];
    if let Some(transform) = color_transform {
        transform.write_rgba(comps, width, height, &mut rgba)?;
        return Ok((width, height, rgba));
    }

    for y in 0..height as usize {
        for x in 0..width as usize {
            let dst = (y * width as usize + x) * 4;
            if comps.len() >= 3 {
                rgba[dst] = component_sample_to_u8(&comps[0], x, y, width, height)?;
                rgba[dst + 1] = component_sample_to_u8(&comps[1], x, y, width, height)?;
                rgba[dst + 2] = component_sample_to_u8(&comps[2], x, y, width, height)?;
            } else {
                let gray = component_sample_to_u8(&comps[0], x, y, width, height)?;
                rgba[dst] = gray;
                rgba[dst + 1] = gray;
                rgba[dst + 2] = gray;
            }
        }
    }

    Ok((width, height, rgba))
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

fn component_sample_to_u8(
    component: &opj::opj_image_comp_t,
    x: usize,
    y: usize,
    out_width: u32,
    out_height: u32,
) -> Result<u8> {
    if component.data.is_null() || component.w == 0 || component.h == 0 {
        bail!("OpenJPEG returned a component without sample data");
    }
    let cx = (x as u64 * component.w as u64 / out_width as u64)
        .min(component.w.saturating_sub(1) as u64) as usize;
    let cy = (y as u64 * component.h as u64 / out_height as u64)
        .min(component.h.saturating_sub(1) as u64) as usize;
    let sample = component_sample(component, cx, cy, component.w, component.h)?;
    Ok(sample_to_u8(sample, component.prec, component.sgnd != 0))
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
