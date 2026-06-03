use std::cmp::max;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tiff::ColorType;
use tiff::decoder::{ChunkType, Decoder, Limits};
use tiff::tags::Tag;

#[derive(Debug, Clone)]
pub(crate) struct ImageInfo {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) color_type: ColorType,
    pub(crate) chunk_type: ChunkType,
    pub(crate) chunk_width: u32,
    pub(crate) chunk_height: u32,
    pub(crate) chunk_count: u32,
    pub(crate) chunks_across: u32,
    pub(crate) compression: Option<u32>,
    pub(crate) bits_per_sample: Option<Vec<u16>>,
    pub(crate) samples_per_pixel: Option<u32>,
    pub(crate) planar_config: Option<u32>,
    pub(crate) photometric: Option<u32>,
    pub(crate) is_bigtiff: bool,
    pub(crate) little_endian: bool,
    pub(crate) rows_per_strip: Option<u32>,
    pub(crate) strip_offsets: Option<Vec<u64>>,
    pub(crate) icc_profile: Option<Arc<[u8]>>,
}

pub(crate) fn print_info(path: &Path) -> Result<()> {
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

pub(crate) fn open_decoder(path: &Path, max_chunk_mb: usize) -> Result<Decoder<BufReader<File>>> {
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

pub(crate) fn load_info(path: &Path) -> Result<ImageInfo> {
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

pub(crate) fn can_read_raw_strips(info: &ImageInfo) -> bool {
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

fn tag_u32(decoder: &mut Decoder<BufReader<File>>, tag: Tag) -> Option<u32> {
    decoder.get_tag_unsigned::<u32>(tag).ok()
}

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
