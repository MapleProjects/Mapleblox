//! Mapleblox ETC2/EAC texture decoding.
//!
//! This crate exists because the previous implementation decoded textures on the
//! GPU by injecting its own command buffers into the guest's submits and by
//! rewriting the guest's barriers, and synchronised that work with a timeline
//! semaphore signalled from another thread. When that value never arrived the
//! guest's render fence stayed unsignalled, the driver's own timeout expired
//! (12 s on NVIDIA 620.70) and the client froze.
//!
//! Here the conversion is pure CPU work over a byte slice. Nothing in this crate
//! touches a queue, a fence or a semaphore, so no failure in it can leave a GPU
//! wait pending: the worst case is a texture that arrives unconverted.
//!
//! The algorithms are ported from `src/graphics/etc2_decoder.cc`, which stays the
//! reference. `tests/reference/generate.cc` runs that reference over
//! deterministic pseudo-random blocks and writes `tests/vectors.bin`; the test
//! below decodes the same blocks here and compares every byte.

pub const ABI_VERSION: u32 = 3;

pub const ERR_OK: i32 = 0;
pub const ERR_INVALID_ARGUMENT: i32 = -1;
pub const ERR_BUFFER_TOO_SMALL: i32 = -2;

/// Block layouts, in the same order as `EtcFormat` in the C++ decoder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum EtcFormat {
    /// ETC2 RGB: 8 bytes per 4x4 block, RGBA8 output with alpha 255.
    Rgb8 = 0,
    /// ETC2 RGB with one-bit punch-through alpha: 8 bytes, index 2 is clear.
    Rgb8A1 = 1,
    /// ETC2 RGBA: EAC alpha block first, then the RGB block, 16 bytes.
    Rgba8 = 2,
    /// EAC R11: one 16-bit channel, 8 bytes per block.
    EacR11 = 3,
    /// EAC R11 signed.
    EacR11Signed = 4,
    /// EAC RG11: two 16-bit channels, 16 bytes per block.
    EacRg11 = 5,
    /// EAC RG11 signed.
    EacRg11Signed = 6,
}

impl EtcFormat {
    fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Rgb8),
            1 => Some(Self::Rgb8A1),
            2 => Some(Self::Rgba8),
            3 => Some(Self::EacR11),
            4 => Some(Self::EacR11Signed),
            5 => Some(Self::EacRg11),
            6 => Some(Self::EacRg11Signed),
            _ => None,
        }
    }

    /// Compressed bytes one 4x4 block occupies.
    fn block_bytes(self) -> usize {
        match self {
            Self::Rgba8 | Self::EacRg11 | Self::EacRg11Signed => 16,
            _ => 8,
        }
    }

    /// Decoded bytes per texel.
    fn texel_bytes(self) -> usize {
        match self {
            Self::EacR11 | Self::EacR11Signed => 2,
            _ => 4,
        }
    }

    fn is_rgba(self) -> bool {
        matches!(self, Self::Rgb8 | Self::Rgb8A1 | Self::Rgba8)
    }

    fn is_signed(self) -> bool {
        matches!(self, Self::EacR11Signed | Self::EacRg11Signed)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeError {
    InvalidArgument,
    BufferTooSmall,
}

impl DecodeError {
    fn code(self) -> i32 {
        match self {
            Self::InvalidArgument => ERR_INVALID_ARGUMENT,
            Self::BufferTooSmall => ERR_BUFFER_TOO_SMALL,
        }
    }
}

/// Signed 3-bit delta used by the differential and EAC base colours.
const K_DELTA: [i32; 8] = [0, 1, 2, 3, -4, -3, -2, -1];

const ETC1_MODIFIERS: [[i32; 4]; 8] = [
    [2, 8, -2, -8],
    [5, 17, -5, -17],
    [9, 29, -9, -29],
    [13, 42, -13, -42],
    [18, 60, -18, -60],
    [24, 80, -24, -80],
    [33, 106, -33, -106],
    [47, 183, -47, -183],
];

#[rustfmt::skip]
const EAC_MODIFIERS: [[i32; 8]; 16] = [
    [-3, -6, -9, -15, 2, 5, 8, 14],  [-3, -7, -10, -13, 2, 6, 9, 12],
    [-2, -5, -8, -13, 1, 4, 7, 12],  [-2, -4, -6, -13, 1, 3, 5, 12],
    [-3, -6, -8, -12, 2, 5, 7, 11],  [-3, -7, -9, -11, 2, 6, 8, 10],
    [-4, -7, -8, -11, 3, 6, 7, 10],  [-3, -5, -8, -11, 2, 4, 7, 10],
    [-2, -6, -8, -10, 1, 5, 7, 9],   [-2, -5, -8, -10, 1, 4, 7, 9],
    [-2, -4, -8, -10, 1, 3, 7, 9],   [-2, -5, -7, -10, 1, 4, 6, 9],
    [-3, -4, -7, -10, 2, 3, 6, 9],   [-1, -2, -3, -10, 0, 1, 2, 9],
    [-4, -6, -8, -9, 3, 5, 7, 8],    [-3, -5, -7, -9, 2, 4, 6, 8],
];

const K_DISTANCE: [i32; 8] = [3, 6, 11, 16, 23, 32, 41, 64];

fn expand4(value: i32) -> i32 {
    (value << 4) | value
}

fn expand5(value: i32) -> i32 {
    (value << 3) | (value >> 2)
}

fn expand6(value: i32) -> i32 {
    (value << 2) | (value >> 4)
}

fn expand7(value: i32) -> i32 {
    (value << 1) | (value >> 6)
}

fn clamp255(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// One texel, in the byte order the C++ decoder writes.
type Texel = [u8; 4];

/// Decodes one ETC2 RGB block into 16 column-major texels (`x * 4 + y`), the
/// same layout `DecodeRgbBlock` produces before the image loop transposes it.
fn decode_rgb_block(b: &[u8], punchthrough: bool, out: &mut [Texel; 16]) {
    let diff_or_opaque = (b[3] & 2) != 0;
    let opaque = !punchthrough || diff_or_opaque;
    let msb = ((b[4] as i32) << 8) | b[5] as i32;
    let lsb = ((b[6] as i32) << 8) | b[7] as i32;
    let index_of = |texel: i32| -> usize {
        ((((msb >> texel) & 1) << 1) | ((lsb >> texel) & 1)) as usize
    };

    let r_sum = (b[0] >> 3) as i32 + K_DELTA[(b[0] & 7) as usize];
    let g_sum = (b[1] >> 3) as i32 + K_DELTA[(b[1] & 7) as usize];
    let b_sum = (b[2] >> 3) as i32 + K_DELTA[(b[2] & 7) as usize];
    let individual = !punchthrough && !diff_or_opaque;

    if !individual && (r_sum < 0 || r_sum > 31 || g_sum < 0 || g_sum > 31) {
        // T mode.
        let mut paint = [[0i32; 3]; 4];
        if r_sum < 0 || r_sum > 31 {
            let c1 = [
                expand4(((((b[0] >> 3) & 3) << 2) | (b[0] & 3)) as i32),
                expand4((b[1] >> 4) as i32),
                expand4((b[1] & 15) as i32),
            ];
            let c2 = [
                expand4((b[2] >> 4) as i32),
                expand4((b[2] & 15) as i32),
                expand4((b[3] >> 4) as i32),
            ];
            let d = K_DISTANCE[((((b[3] >> 2) & 3) << 1) | (b[3] & 1)) as usize];
            for c in 0..3 {
                paint[0][c] = c1[c];
                paint[1][c] = c2[c] + d;
                paint[2][c] = c2[c];
                paint[3][c] = c2[c] - d;
            }
        } else {
            let c1 = [
                expand4(((b[0] >> 3) & 15) as i32),
                expand4((((b[0] & 7) as i32) << 1) | (((b[1] >> 4) & 1) as i32)),
                expand4(
                    ((b[1] & 8) as i32)
                        | (((b[1] & 3) as i32) << 1)
                        | ((b[2] >> 7) as i32),
                ),
            ];
            let c2 = [
                expand4(((b[2] >> 3) & 15) as i32),
                expand4((((b[2] & 7) as i32) << 1) | ((b[3] >> 7) as i32)),
                expand4(((b[3] >> 3) & 15) as i32),
            ];
            let v1 = (c1[0] << 16) | (c1[1] << 8) | c1[2];
            let v2 = (c2[0] << 16) | (c2[1] << 8) | c2[2];
            let d = K_DISTANCE[(((b[3] & 4) | ((b[3] & 1) << 1)) as usize)
                | usize::from(v1 >= v2)];
            for c in 0..3 {
                paint[0][c] = c1[c] + d;
                paint[1][c] = c1[c] - d;
                paint[2][c] = c2[c] + d;
                paint[3][c] = c2[c] - d;
            }
        }
        let paint32: [Texel; 4] = [
            [clamp255(paint[0][0]), clamp255(paint[0][1]), clamp255(paint[0][2]), 255],
            [clamp255(paint[1][0]), clamp255(paint[1][1]), clamp255(paint[1][2]), 255],
            [clamp255(paint[2][0]), clamp255(paint[2][1]), clamp255(paint[2][2]), 255],
            [clamp255(paint[3][0]), clamp255(paint[3][1]), clamp255(paint[3][2]), 255],
        ];
        for texel in 0..16 {
            let index = index_of(texel as i32);
            out[texel] = if !opaque && index == 2 {
                [0, 0, 0, 0]
            } else {
                paint32[index]
            };
        }
        return;
    }

    if !individual && (b_sum < 0 || b_sum > 31) {
        // H mode.
        let ro = expand6(((b[0] >> 1) & 63) as i32);
        let go = expand7((((b[0] & 1) as i32) << 6) | (((b[1] >> 1) & 63) as i32));
        let bo = expand6(
            (((b[1] & 1) as i32) << 5)
                | ((b[2] & 0x18) as i32)
                | (((b[2] & 3) as i32) << 1)
                | ((b[3] >> 7) as i32),
        );
        let rh = expand6((((b[3] & 0x7c) >> 1) | (b[3] & 1)) as i32);
        let gh = expand7((b[4] >> 1) as i32);
        let bh = expand6(((((b[4] & 1) as i32) << 5) | ((b[5] >> 3) as i32)) as i32);
        let rv = expand6(((((b[5] & 7) as i32) << 3) | ((b[6] >> 5) as i32)) as i32);
        let gv = expand7(((((b[6] & 31) as i32) << 2) | ((b[7] >> 6) as i32)) as i32);
        let bv = expand6((b[7] & 63) as i32);
        for x in 0..4i32 {
            for y in 0..4i32 {
                let r = (x * (rh - ro) + y * (rv - ro) + 4 * ro + 2) >> 2;
                let g = (x * (gh - go) + y * (gv - go) + 4 * go + 2) >> 2;
                let bl = (x * (bh - bo) + y * (bv - bo) + 4 * bo + 2) >> 2;
                out[(x * 4 + y) as usize] = [clamp255(r), clamp255(g), clamp255(bl), 255];
            }
        }
        return;
    }

    let mut base = [[0i32; 3]; 2];
    for c in 0..3 {
        if individual {
            base[0][c] = expand4((b[c] >> 4) as i32);
            base[1][c] = expand4((b[c] & 15) as i32);
        } else {
            base[0][c] = expand5((b[c] >> 3) as i32);
            base[1][c] = expand5((b[c] >> 3) as i32 + K_DELTA[(b[c] & 7) as usize]);
        }
    }
    let tables = [(b[3] >> 5) as usize, ((b[3] >> 2) & 7) as usize];
    let flipped = (b[3] & 1) != 0;

    let mut palette = [[[0u8; 4]; 4]; 2];
    for sub in 0..2 {
        for idx in 0..4 {
            let modifier = if !opaque && idx == 0 {
                0
            } else {
                ETC1_MODIFIERS[tables[sub]][idx]
            };
            palette[sub][idx] = [
                clamp255(base[sub][0] + modifier),
                clamp255(base[sub][1] + modifier),
                clamp255(base[sub][2] + modifier),
                255,
            ];
        }
    }

    for x in 0..4i32 {
        for y in 0..4i32 {
            let texel = x * 4 + y;
            let index = index_of(texel);
            if !opaque && index == 2 {
                out[texel as usize] = [0, 0, 0, 0];
                continue;
            }
            let sub = if flipped {
                usize::from(y >= 2)
            } else {
                usize::from(x >= 2)
            };
            out[texel as usize] = palette[sub][index];
        }
    }
}

/// The 48 index bits of an EAC block, taken from bytes 2..8.
fn read_eac_bits(block: &[u8]) -> u64 {
    let mut bits = 0u64;
    for byte in &block[2..8] {
        bits = (bits << 8) | *byte as u64;
    }
    bits
}

fn eac_index(bits: u64, texel: i32) -> usize {
    ((bits >> (45 - texel * 3)) & 7) as usize
}

/// One EAC R11 tap, expanded to the 16-bit value the client samples.
fn decode_r11_value(block: &[u8], multiplier: i32, modifier: i32, signed: bool) -> u16 {
    let scaled = if multiplier != 0 {
        modifier * multiplier * 8
    } else {
        modifier
    };
    if !signed {
        let value = (block[0] as i32 * 8 + 4 + scaled).clamp(0, 2047);
        return ((value << 5) | (value >> 6)) as u16;
    }
    let base = (block[0] as i8 as i32).max(-127);
    let value = (base * 8 + scaled).clamp(-1023, 1023);
    let magnitude = value.abs();
    let expanded = (magnitude << 5) | (magnitude >> 5);
    if value < 0 {
        (-expanded) as i16 as u16
    } else {
        expanded as u16
    }
}

/// Decodes a whole image into its destination layout.
pub fn decode_image(
    format: EtcFormat,
    source: &[u8],
    width: u32,
    height: u32,
    destination: &mut [u8],
) -> Result<(), DecodeError> {
    let layout = Layout::new(format, source, width, height, destination)?;
    decode_band(&layout, source, 0, layout.blocks_high, destination);
    Ok(())
}

/// Decodes the same image across `threads` scoped threads, one band of block
/// rows each. The bands never share destination bytes, so no lock is needed and
/// no thread can leave another waiting.
pub fn decode_image_parallel(
    format: EtcFormat,
    source: &[u8],
    width: u32,
    height: u32,
    destination: &mut [u8],
    threads: usize,
) -> Result<(), DecodeError> {
    let layout = Layout::new(format, source, width, height, destination)?;
    let threads = threads.clamp(1, layout.blocks_high.max(1));
    if threads == 1 {
        decode_band(&layout, source, 0, layout.blocks_high, destination);
        return Ok(());
    }
    let band_rows = layout.blocks_high.div_ceil(threads);
    let texel_rows = band_rows * 4;
    let band_bytes = layout.stride * texel_rows;
    std::thread::scope(|scope| {
        for (index, band) in destination.chunks_mut(band_bytes).enumerate() {
            let first = index * band_rows;
            if first >= layout.blocks_high {
                break;
            }
            let count = band_rows.min(layout.blocks_high - first);
            let layout = &layout;
            scope.spawn(move || decode_band(layout, source, first, count, band));
        }
    });
    Ok(())
}

/// Geometry shared by the sequential and parallel entry points.
struct Layout {
    format: EtcFormat,
    blocks_wide: usize,
    blocks_high: usize,
    block_bytes: usize,
    texel_bytes: usize,
    stride: usize,
    width: u32,
    height: u32,
}

impl Layout {
    fn new(
        format: EtcFormat,
        source: &[u8],
        width: u32,
        height: u32,
        destination: &[u8],
    ) -> Result<Self, DecodeError> {
        if width == 0 || height == 0 {
            return Err(DecodeError::InvalidArgument);
        }
        let blocks_wide = width.div_ceil(4) as usize;
        let blocks_high = height.div_ceil(4) as usize;
        let blocks = blocks_wide
            .checked_mul(blocks_high)
            .ok_or(DecodeError::InvalidArgument)?;
        let block_bytes = format.block_bytes();
        if source.len() < blocks * block_bytes {
            return Err(DecodeError::BufferTooSmall);
        }
        let texel_bytes = format.texel_bytes();
        let needed = (width as usize) * (height as usize) * texel_bytes;
        if destination.len() < needed {
            return Err(DecodeError::BufferTooSmall);
        }
        Ok(Self {
            format,
            blocks_wide,
            blocks_high,
            block_bytes,
            texel_bytes,
            stride: (width as usize) * texel_bytes,
            width,
            height,
        })
    }
}

/// Decodes block rows `[first, first + count)` into `destination`, which starts
/// at the texel row those blocks begin on.
fn decode_band(
    layout: &Layout,
    source: &[u8],
    first: usize,
    count: usize,
    destination: &mut [u8],
) {
    let format = layout.format;
    let width = layout.width as usize;
    let height = layout.height as usize;
    let stride = layout.stride;
    let block_bytes = layout.block_bytes;
    let texel_bytes = layout.texel_bytes;
    let blocks_wide = layout.blocks_wide;

    let mut cols = [[0u8; 4]; 16];
    for by in first..first + count {
        for bx in 0..blocks_wide {
            let offset = (by * blocks_wide + bx) * block_bytes;
            let block = &source[offset..offset + block_bytes];
            let row_base = (by - first) * 4;

            if format.is_rgba() {
                if format == EtcFormat::Rgba8 {
                    decode_rgb_block(&block[8..16], false, &mut cols);
                    let alpha_bits = read_eac_bits(block);
                    let alpha_base = block[0] as i32;
                    let alpha_multiplier = (block[1] >> 4) as i32;
                    let table = &EAC_MODIFIERS[(block[1] & 15) as usize];
                    let mut alpha_palette = [0u8; 8];
                    for (tap, entry) in alpha_palette.iter_mut().enumerate() {
                        *entry = clamp255(alpha_base + table[tap] * alpha_multiplier);
                    }
                    for (texel, col) in cols.iter_mut().enumerate() {
                        col[3] = alpha_palette[eac_index(alpha_bits, texel as i32)];
                    }
                } else {
                    decode_rgb_block(block, format == EtcFormat::Rgb8A1, &mut cols);
                }
                for y in 0..4usize {
                    let py = by * 4 + y;
                    if py >= height {
                        break;
                    }
                    for x in 0..4usize {
                        let px = bx * 4 + x;
                        if px >= width {
                            break;
                        }
                        let dst = (row_base + y) * stride + px * 4;
                        destination[dst..dst + 4].copy_from_slice(&cols[x * 4 + y]);
                    }
                }
            } else {
                let channels = texel_bytes / 2;
                let mut channel_bits = [0u64; 2];
                let mut channel_palette = [[0u16; 8]; 2];
                for channel in 0..channels {
                    let cb = &block[channel * 8..channel * 8 + 8];
                    channel_bits[channel] = read_eac_bits(cb);
                    let multiplier = (cb[1] >> 4) as i32;
                    let table = &EAC_MODIFIERS[(cb[1] & 15) as usize];
                    for tap in 0..8 {
                        channel_palette[channel][tap] =
                            decode_r11_value(cb, multiplier, table[tap], format.is_signed());
                    }
                }
                for y in 0..4usize {
                    let py = by * 4 + y;
                    if py >= height {
                        break;
                    }
                    for x in 0..4usize {
                        let px = bx * 4 + x;
                        if px >= width {
                            break;
                        }
                        let texel = (x * 4 + y) as i32;
                        let dst = (row_base + y) * stride + px * texel_bytes;
                        for channel in 0..channels {
                            let value = channel_palette[channel]
                                [eac_index(channel_bits[channel], texel)];
                            destination[dst + channel * 2] = value as u8;
                            destination[dst + channel * 2 + 1] = (value >> 8) as u8;
                        }
                    }
                }
            }
        }
    }
}

/// Decodes block rows `[first_block_row, first_block_row + block_row_count)`
/// of the image `decode_image` describes. `destination` is the whole image, as
/// in the C++ reference: bands write the texel rows their blocks cover and
/// never touch another band's bytes, so they can run on separate threads.
pub fn decode_block_rows(
    format: EtcFormat,
    source: &[u8],
    width: u32,
    height: u32,
    first_block_row: u32,
    block_row_count: u32,
    destination: &mut [u8],
) -> Result<(), DecodeError> {
    let layout = Layout::new(format, source, width, height, destination)?;
    let first = first_block_row as usize;
    let count = block_row_count as usize;
    if first + count > layout.blocks_high {
        return Err(DecodeError::InvalidArgument);
    }
    if count == 0 {
        return Ok(());
    }
    let start = first * 4 * layout.stride;
    decode_band(&layout, source, first, count, &mut destination[start..]);
    Ok(())
}

#[no_mangle]
pub extern "C" fn mapleblox_etc2_abi_version() -> u32 {
    ABI_VERSION
}

/// C ABI used by the runtime. Returns 0 on success, a negative code otherwise.
///
/// # Safety
/// `source` must point to `source_len` readable bytes and `destination` to
/// `destination_len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn mapleblox_etc2_decode_image(
    format: u32,
    source: *const u8,
    source_len: usize,
    width: u32,
    height: u32,
    destination: *mut u8,
    destination_len: usize,
) -> i32 {
    let Some(format) = EtcFormat::from_raw(format) else {
        return ERR_INVALID_ARGUMENT;
    };
    if source.is_null() || destination.is_null() {
        return ERR_INVALID_ARGUMENT;
    }
    let source = unsafe { std::slice::from_raw_parts(source, source_len) };
    let destination = unsafe { std::slice::from_raw_parts_mut(destination, destination_len) };
    match decode_image(format, source, width, height, destination) {
        Ok(()) => ERR_OK,
        Err(error) => error.code(),
    }
}

/// As `mapleblox_etc2_decode_image`, across `threads` scoped threads.
///
/// # Safety
/// As `mapleblox_etc2_decode_image`.
#[no_mangle]
pub unsafe extern "C" fn mapleblox_etc2_decode_image_parallel(
    format: u32,
    source: *const u8,
    source_len: usize,
    width: u32,
    height: u32,
    destination: *mut u8,
    destination_len: usize,
    threads: u32,
) -> i32 {
    let Some(format) = EtcFormat::from_raw(format) else {
        return ERR_INVALID_ARGUMENT;
    };
    if source.is_null() || destination.is_null() {
        return ERR_INVALID_ARGUMENT;
    }
    let source = unsafe { std::slice::from_raw_parts(source, source_len) };
    let destination = unsafe { std::slice::from_raw_parts_mut(destination, destination_len) };
    match decode_image_parallel(
        format,
        source,
        width,
        height,
        destination,
        threads as usize,
    ) {
        Ok(()) => ERR_OK,
        Err(error) => error.code(),
    }
}

/// As `mapleblox_etc2_decode_image`, for one band of block rows. `destination`
/// is the whole image, as in the C++ reference.
///
/// # Safety
/// As `mapleblox_etc2_decode_image`.
#[no_mangle]
pub unsafe extern "C" fn mapleblox_etc2_decode_block_rows(
    format: u32,
    source: *const u8,
    source_len: usize,
    width: u32,
    height: u32,
    first_block_row: u32,
    block_row_count: u32,
    destination: *mut u8,
    destination_len: usize,
) -> i32 {
    let Some(format) = EtcFormat::from_raw(format) else {
        return ERR_INVALID_ARGUMENT;
    };
    if source.is_null() || destination.is_null() {
        return ERR_INVALID_ARGUMENT;
    }
    let source = unsafe { std::slice::from_raw_parts(source, source_len) };
    let destination = unsafe { std::slice::from_raw_parts_mut(destination, destination_len) };
    match decode_block_rows(
        format,
        source,
        width,
        height,
        first_block_row,
        block_row_count,
        destination,
    ) {
        Ok(()) => ERR_OK,
        Err(error) => error.code(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One individual-mode block in the real ETC1 layout: bytes 0-2 carry the
    /// base colours as nibbles per subblock, so 0xF0 lights subblock 0 (235 after
    /// the table-0 modifier) and leaves subblock 1 dark. Every selector is zero.
    const WHITE_BLACK: [u8; 8] = [0xF0, 0xF0, 0xF0, 0x00, 0x00, 0x00, 0x00, 0x00];

    #[test]
    fn decodes_individual_block() {
        let mut cols = [[0u8; 4]; 16];
        decode_rgb_block(&WHITE_BLACK, false, &mut cols);
        for x in 0..4 {
            for y in 0..4 {
                let pixel = cols[x * 4 + y];
                if x < 2 {
                    assert_eq!(pixel, [255, 255, 255, 255], "left half at {x},{y}");
                } else {
                    assert_eq!(pixel, [2, 2, 2, 255], "right half at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn flips_subblocks_horizontally() {
        let mut block = WHITE_BLACK;
        block[3] |= 0x01;
        let mut cols = [[0u8; 4]; 16];
        decode_rgb_block(&block, false, &mut cols);
        for x in 0..4 {
            for y in 0..4 {
                let expected = if y < 2 { 255 } else { 2 };
                assert_eq!(cols[x * 4 + y][0], expected, "at {x},{y}");
            }
        }
    }

    #[test]
    fn decodes_a_full_image_row_major() {
        let mut source = Vec::new();
        source.extend_from_slice(&WHITE_BLACK);
        source.extend_from_slice(&WHITE_BLACK);
        let mut destination = vec![0u8; 5 * 3 * 4];
        decode_image(EtcFormat::Rgb8, &source, 5, 3, &mut destination).expect("decode");
        // Texel (4,2) comes from the second block, left half: white.
        let index = (2 * 5 + 4) * 4;
        assert_eq!(&destination[index..index + 4], &[255, 255, 255, 255]);
    }

    #[test]
    fn rejects_short_buffers() {
        let mut destination = vec![0u8; 4 * 4 * 4];
        assert_eq!(
            decode_image(EtcFormat::Rgb8, &[0u8; 4], 4, 4, &mut destination),
            Err(DecodeError::BufferTooSmall)
        );
        assert_eq!(
            decode_image(EtcFormat::Rgb8, &[0u8; 8], 4, 4, &mut destination[..4]),
            Err(DecodeError::BufferTooSmall)
        );
    }

    #[test]
    fn abi_reports_errors_instead_of_writing() {
        let mut destination = vec![0u8; 4 * 4 * 4];
        let source = [0u8; 8];
        let status = unsafe {
            mapleblox_etc2_decode_image(
                EtcFormat::Rgb8 as u32,
                source.as_ptr(),
                source.len(),
                4,
                4,
                destination.as_mut_ptr(),
                destination.len(),
            )
        };
        assert_eq!(status, ERR_OK);
        let status = unsafe {
            mapleblox_etc2_decode_image(
                99,
                source.as_ptr(),
                source.len(),
                4,
                4,
                destination.as_mut_ptr(),
                destination.len(),
            )
        };
        assert_eq!(status, ERR_INVALID_ARGUMENT);
    }

    /// Compares every byte against the C++ reference output in `tests/vectors.bin`.
    ///
    /// The file is a sequence of cases: format (u32 LE), width, height, source
    /// length, source bytes, destination length, destination bytes.
    #[test]
    fn matches_the_cpp_reference() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors.bin");
        let Ok(data) = std::fs::read(path) else {
            eprintln!("vectors.bin missing; run tests/reference/generate.cc");
            return;
        };
        let mut cursor = 0usize;
        let read_u32 = |cursor: &mut usize| -> u32 {
            let value = u32::from_le_bytes(data[*cursor..*cursor + 4].try_into().unwrap());
            *cursor += 4;
            value
        };
        let mut cases = 0usize;
        while cursor < data.len() {
            let format = read_u32(&mut cursor);
            let width = read_u32(&mut cursor);
            let height = read_u32(&mut cursor);
            let source_len = read_u32(&mut cursor) as usize;
            let source = &data[cursor..cursor + source_len];
            cursor += source_len;
            let destination_len = read_u32(&mut cursor) as usize;
            let expected = &data[cursor..cursor + destination_len];
            cursor += destination_len;

            let mut actual = vec![0u8; destination_len];
            let format = EtcFormat::from_raw(format).expect("known format");
            decode_image(format, source, width, height, &mut actual)
                .expect("reference case decodes");
            assert_eq!(
                actual, expected,
                "case {cases}: format {format:?} {width}x{height}"
            );
            cases += 1;
        }
        assert!(cases > 0, "no cases in vectors.bin");
        eprintln!("compared {cases} cases against the C++ reference");
    }

    /// Bands of block rows must reproduce the whole image byte for byte, which
    /// is what lets the runtime split one upload across worker threads.
    #[test]
    fn blocks_row_bands_match_the_whole_image() {
        const FORMATS: [EtcFormat; 7] = [
            EtcFormat::Rgb8,
            EtcFormat::Rgb8A1,
            EtcFormat::Rgba8,
            EtcFormat::EacR11,
            EtcFormat::EacR11Signed,
            EtcFormat::EacRg11,
            EtcFormat::EacRg11Signed,
        ];
        let mut state = 0x9E37_79B9u32;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        for format in FORMATS {
            for (width, height) in [(37u32, 23u32), (4, 4), (1, 9), (64, 5)] {
                let blocks = width.div_ceil(4) * height.div_ceil(4);
                let source: Vec<u8> = (0..blocks * format.block_bytes() as u32)
                    .map(|_| next() as u8)
                    .collect();
                let texels = (width * height) as usize * format.texel_bytes();
                let mut whole = vec![0u8; texels];
                decode_image(format, &source, width, height, &mut whole)
                    .expect("whole image decodes");

                let mut banded = vec![0u8; texels];
                let blocks_high = height.div_ceil(4);
                let mut first = 0;
                while first < blocks_high {
                    let count = 2.min(blocks_high - first);
                    let status = unsafe {
                        mapleblox_etc2_decode_block_rows(
                            format as u32,
                            source.as_ptr(),
                            source.len(),
                            width,
                            height,
                            first,
                            count,
                            banded.as_mut_ptr(),
                            banded.len(),
                        )
                    };
                    assert_eq!(status, ERR_OK, "{format:?} {width}x{height} band {first}");
                    first += count;
                }
                assert_eq!(
                    banded, whole,
                    "{format:?} {width}x{height} decoded in bands"
                );
            }
        }
    }
}