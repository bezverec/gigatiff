use anyhow::{Context, Result, bail};
use lcms2::{DisallowCache, Flags, GlobalContext, Intent, PixelFormat, Profile, Transform};
use tiff::ColorType;

type LcmsTransform = Transform<u8, u8, GlobalContext, DisallowCache>;

pub(crate) enum ColorTransform {
    Rgb8ToRgb8(LcmsTransform),
    Rgb16ToRgb8(LcmsTransform),
    Rgba8ToRgb8(LcmsTransform),
    Rgba16ToRgb8(LcmsTransform),
    Gray8ToRgb8(LcmsTransform),
    Gray16ToRgb8(LcmsTransform),
}

impl ColorTransform {
    pub(crate) fn new(color_type: ColorType, icc_profile: Option<&[u8]>) -> Result<Option<Self>> {
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

pub(crate) fn write_sampled_row_rgba(
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
            write_raw_sampled_row_rgb8(src_row, src_x_byte_offsets, out);
        }
        ColorType::RGBA(8) => {
            write_raw_sampled_row_rgba8(src_row, src_x_byte_offsets, out);
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

fn write_raw_sampled_row_rgb8(src_row: &[u8], src_x_byte_offsets: &[usize], out: &mut [u8]) {
    let written = write_raw_sampled_row_rgb8_simd(src_row, src_x_byte_offsets, out);
    for (dst, &src) in out[written * 4..]
        .chunks_exact_mut(4)
        .zip(&src_x_byte_offsets[written..])
    {
        dst[0..3].copy_from_slice(&src_row[src..src + 3]);
        dst[3] = 255;
    }
}

fn write_raw_sampled_row_rgba8(src_row: &[u8], src_x_byte_offsets: &[usize], out: &mut [u8]) {
    let written = write_raw_sampled_row_rgba8_simd(src_row, src_x_byte_offsets, out);
    for (dst, &src) in out[written * 4..]
        .chunks_exact_mut(4)
        .zip(&src_x_byte_offsets[written..])
    {
        dst.copy_from_slice(&src_row[src..src + 4]);
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn write_raw_sampled_row_rgb8_simd(
    src_row: &[u8],
    src_x_byte_offsets: &[usize],
    out: &mut [u8],
) -> usize {
    if src_x_byte_offsets.len() < 4 {
        return 0;
    }

    #[cfg(target_arch = "x86")]
    use std::arch::x86::{__m128i, _mm_setr_epi8, _mm_storeu_si128};
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::{__m128i, _mm_setr_epi8, _mm_storeu_si128};

    let chunks = src_x_byte_offsets.len() / 4;
    for chunk in 0..chunks {
        let src = &src_x_byte_offsets[chunk * 4..chunk * 4 + 4];
        let dst = chunk * 16;
        unsafe {
            let rgba = _mm_setr_epi8(
                src_row[src[0]] as i8,
                src_row[src[0] + 1] as i8,
                src_row[src[0] + 2] as i8,
                -1,
                src_row[src[1]] as i8,
                src_row[src[1] + 1] as i8,
                src_row[src[1] + 2] as i8,
                -1,
                src_row[src[2]] as i8,
                src_row[src[2] + 1] as i8,
                src_row[src[2] + 2] as i8,
                -1,
                src_row[src[3]] as i8,
                src_row[src[3] + 1] as i8,
                src_row[src[3] + 2] as i8,
                -1,
            );
            _mm_storeu_si128(out.as_mut_ptr().add(dst).cast::<__m128i>(), rgba);
        }
    }

    chunks * 4
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn write_raw_sampled_row_rgb8_simd(
    _src_row: &[u8],
    _src_x_byte_offsets: &[usize],
    _out: &mut [u8],
) -> usize {
    0
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn write_raw_sampled_row_rgba8_simd(
    src_row: &[u8],
    src_x_byte_offsets: &[usize],
    out: &mut [u8],
) -> usize {
    if src_x_byte_offsets.len() < 4 {
        return 0;
    }

    #[cfg(target_arch = "x86")]
    use std::arch::x86::{__m128i, _mm_setr_epi32, _mm_storeu_si128};
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::{__m128i, _mm_setr_epi32, _mm_storeu_si128};

    let chunks = src_x_byte_offsets.len() / 4;
    for chunk in 0..chunks {
        let src = &src_x_byte_offsets[chunk * 4..chunk * 4 + 4];
        let dst = chunk * 16;
        unsafe {
            let rgba = _mm_setr_epi32(
                std::ptr::read_unaligned(src_row.as_ptr().add(src[0]).cast::<i32>()),
                std::ptr::read_unaligned(src_row.as_ptr().add(src[1]).cast::<i32>()),
                std::ptr::read_unaligned(src_row.as_ptr().add(src[2]).cast::<i32>()),
                std::ptr::read_unaligned(src_row.as_ptr().add(src[3]).cast::<i32>()),
            );
            _mm_storeu_si128(out.as_mut_ptr().add(dst).cast::<__m128i>(), rgba);
        }
    }

    chunks * 4
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn write_raw_sampled_row_rgba8_simd(
    _src_row: &[u8],
    _src_x_byte_offsets: &[usize],
    _out: &mut [u8],
) -> usize {
    0
}

fn u16_to_u8(bytes: &[u8], little_endian: bool) -> u8 {
    if little_endian { bytes[1] } else { bytes[0] }
}

pub(crate) fn samples_for_color(color_type: ColorType) -> Result<usize> {
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

pub(crate) fn bits_for_color(color_type: ColorType) -> Result<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_sampled_row_rgb8_writes_rgba_directly() {
        let src_row = [1, 2, 3, 10, 20, 30, 40, 50, 60];
        let offsets = [6, 0];
        let mut out = [0u8; 8];

        write_raw_sampled_row_rgba(&src_row, &offsets, ColorType::RGB(8), true, &mut out).unwrap();

        assert_eq!(out, [40, 50, 60, 255, 1, 2, 3, 255]);
    }

    #[test]
    fn raw_sampled_row_rgb8_handles_simd_chunks_and_tail() {
        let src_row = [1, 2, 3, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];
        let offsets = [12, 0, 6, 3, 9];
        let mut out = [0u8; 20];

        write_raw_sampled_row_rgba(&src_row, &offsets, ColorType::RGB(8), true, &mut out).unwrap();

        assert_eq!(
            out,
            [
                100, 110, 120, 255, 1, 2, 3, 255, 40, 50, 60, 255, 10, 20, 30, 255, 70, 80, 90, 255
            ]
        );
    }

    #[test]
    fn raw_sampled_row_rgba8_handles_simd_chunks_and_tail() {
        let src_row = [
            1, 2, 3, 4, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160,
        ];
        let offsets = [16, 0, 8, 4, 12];
        let mut out = [0u8; 20];

        write_raw_sampled_row_rgba(&src_row, &offsets, ColorType::RGBA(8), true, &mut out).unwrap();

        assert_eq!(
            out,
            [
                130, 140, 150, 160, 1, 2, 3, 4, 50, 60, 70, 80, 10, 20, 30, 40, 90, 100, 110, 120
            ]
        );
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
