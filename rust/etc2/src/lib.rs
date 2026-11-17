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
//! Status: the ETC1/ETC2 RGB block in individual mode is implemented and tested.
//! Differential mode and the ETC2 extra modes (T, H, planar) and EAC blocks
//! report [`ERR_UNSUPPORTED`] instead of guessing, until they are ported and
//! checked byte for byte against `src/graphics/etc2_decoder.cc`.

pub const ABI_VERSION: u32 = 1;

pub const ERR_OK: i32 = 0;
pub const ERR_INVALID_ARGUMENT: i32 = -1;
pub const ERR_BUFFER_TOO_SMALL: i32 = -2;
pub const ERR_UNSUPPORTED: i32 = -3;

/// Block layouts the runtime can ask for, kept in step with `EtcFormat`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum EtcFormat {
    /// ETC1 or ETC2 RGB: 8 bytes per 4x4 block.
    Rgb8 = 0,
    /// ETC2 RGBA: the RGB block plus an EAC alpha block, 16 bytes.
    Rgba8 = 1,
    /// EAC R11: one channel, 8 bytes per 4x4 block.
    R11 = 2,
    /// EAC RG11: two channels, 16 bytes per 4x4 block.
    Rg11 = 3,
}

impl EtcFormat {
    fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Rgb8),
            1 => Some(Self::Rgba8),
            2 => Some(Self::R11),
            3 => Some(Self::Rg11),
            _ => None,
        }
    }

    /// Bytes one 4x4 block occupies in this format.
    fn block_bytes(self) -> usize {
        match self {
            Self::Rgb8 | Self::R11 => 8,
            Self::Rgba8 | Self::Rg11 => 16,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeError {
    InvalidArgument,
    BufferTooSmall,
    Unsupported,
}

impl DecodeError {
    fn code(self) -> i32 {
        match self {
            Self::InvalidArgument => ERR_INVALID_ARGUMENT,
            Self::BufferTooSmall => ERR_BUFFER_TOO_SMALL,
            Self::Unsupported => ERR_UNSUPPORTED,
        }
    }
}

/// ETC1/ETC2 modifier tables, indexed by the 3-bit table codeword.
const MODIFIERS: [[i32; 4]; 8] = [
    [2, 8, -2, -8],
    [5, 17, -5, -17],
    [9, 29, -9, -29],
    [13, 42, -13, -42],
    [18, 60, -18, -60],
    [24, 80, -24, -80],
    [33, 106, -33, -106],
    [47, 183, -47, -183],
];

fn expand4(value: u32) -> i32 {
    let nibble = (value & 0xF) as i32;
    (nibble << 4) | nibble
}

fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// Decodes one ETC1/ETC2 RGB block.
///
/// Only the individual mode is implemented: it is the ETC1 layout shared by
/// every ETC2 RGB block whose differential bit is clear. Differential mode can
/// carry the ETC2-only modes (T, H, planar), so it returns `None` rather than
/// risk decoding a block as something it is not.
fn decode_rgb_block(block: &[u8]) -> Option<[[u8; 4]; 16]> {
    debug_assert!(block.len() >= 8);
    let high = u32::from_be_bytes([block[0], block[1], block[2], block[3]]);
    let low = u32::from_be_bytes([block[4], block[5], block[6], block[7]]);

    let differential = (high >> 31) & 1 != 0;
    if differential {
        return None;
    }
    let flip = (high >> 30) & 1 != 0;
    let table1 = ((high >> 27) & 0x7) as usize;
    let table2 = ((high >> 24) & 0x7) as usize;

    let base1 = [expand4(high >> 20), expand4(high >> 12), expand4(high >> 4)];
    let base2 = [expand4(high >> 16), expand4(high >> 8), expand4(high)];

    let msb_plane = (low >> 16) & 0xFFFF;
    let lsb_plane = low & 0xFFFF;
    let mut out = [[0u8; 4]; 16];
    for y in 0..4u32 {
        for x in 0..4u32 {
            let index = y * 4 + x;
            let selector = (((msb_plane >> index) & 1) << 1) | ((lsb_plane >> index) & 1);
            let second = if flip { y >= 2 } else { x >= 2 };
            let (base, table) = if second {
                (base2, MODIFIERS[table2])
            } else {
                (base1, MODIFIERS[table1])
            };
            let delta = table[selector as usize];
            let pixel = &mut out[index as usize];
            pixel[0] = clamp_u8(base[0] + delta);
            pixel[1] = clamp_u8(base[1] + delta);
            pixel[2] = clamp_u8(base[2] + delta);
            pixel[3] = 255;
        }
    }
    Some(out)
}

/// Decodes a whole image into RGBA8.
///
/// `width` and `height` are the texture dimensions; blocks past the edge of a
/// texture that is not a multiple of four are written and simply cover the
/// padding, which is what the upload expects.
pub fn decode_image(
    format: EtcFormat,
    source: &[u8],
    width: u32,
    height: u32,
    destination: &mut [u8],
) -> Result<(), DecodeError> {
    if width == 0 || height == 0 {
        return Err(DecodeError::InvalidArgument);
    }
    let blocks_x = width.div_ceil(4) as usize;
    let blocks_y = height.div_ceil(4) as usize;
    let blocks = blocks_x
        .checked_mul(blocks_y)
        .ok_or(DecodeError::InvalidArgument)?;
    let block_bytes = format.block_bytes();
    if source.len() < blocks * block_bytes {
        return Err(DecodeError::BufferTooSmall);
    }
    let needed = (width as usize) * (height as usize) * 4;
    if destination.len() < needed {
        return Err(DecodeError::BufferTooSmall);
    }
    let stride = (width as usize) * 4;

    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            let offset = (by * blocks_x + bx) * block_bytes;
            let pixels = match format {
                EtcFormat::Rgb8 => decode_rgb_block(&source[offset..offset + 8])
                    .ok_or(DecodeError::Unsupported)?,
                EtcFormat::Rgba8 => {
                    let mut pixels = decode_rgb_block(&source[offset..offset + 8])
                        .ok_or(DecodeError::Unsupported)?;
                    let alpha = decode_alpha_block(&source[offset + 8..offset + 16])
                        .ok_or(DecodeError::Unsupported)?;
                    for (pixel, value) in pixels.iter_mut().zip(alpha) {
                        pixel[3] = value;
                    }
                    pixels
                }
                EtcFormat::R11 | EtcFormat::Rg11 => return Err(DecodeError::Unsupported),
            };
            for y in 0..4usize {
                let py = by * 4 + y;
                if py >= height as usize {
                    break;
                }
                for x in 0..4usize {
                    let px = bx * 4 + x;
                    if px >= width as usize {
                        break;
                    }
                    let dst = py * stride + px * 4;
                    destination[dst..dst + 4].copy_from_slice(&pixels[y * 4 + x]);
                }
            }
        }
    }
    Ok(())
}

/// EAC alpha (ETC2 RGBA). Not ported yet.
fn decode_alpha_block(_block: &[u8]) -> Option<[u8; 16]> {
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One individual-mode block: white on the left half, black on the right,
    /// every selector zero, so the table-0 modifier (+2) shows on both sides.
    const WHITE_BLACK: [u8; 8] = [0x00, 0xF0, 0xF0, 0xF0, 0x00, 0x00, 0x00, 0x00];

    #[test]
    fn decodes_individual_block() {
        let pixels = decode_rgb_block(&WHITE_BLACK).expect("individual mode");
        for y in 0..4 {
            for x in 0..4 {
                let pixel = pixels[y * 4 + x];
                if x < 2 {
                    assert_eq!(&pixel, &[255, 255, 255, 255], "left half at {x},{y}");
                } else {
                    assert_eq!(&pixel, &[2, 2, 2, 255], "right half at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn flips_subblocks_horizontally() {
        // Same colours but the flip bit set: top half white, bottom half black.
        let mut block = WHITE_BLACK;
        block[0] |= 0x40;
        let pixels = decode_rgb_block(&block).expect("individual mode");
        for y in 0..4 {
            for x in 0..4 {
                let expected = if y < 2 { 255 } else { 2 };
                assert_eq!(pixels[y * 4 + x][0], expected, "at {x},{y}");
            }
        }
    }

    #[test]
    fn selectors_pick_the_table_entries() {
        // Individual block, both halves mid grey, selector 3 on every pixel.
        let mut block = [0u8; 8];
        block[0] = 0x00;
        block[1] = 0x77; // r1 = 7, r2 = 7
        block[2] = 0x77; // g1 = 7, g2 = 7
        block[3] = 0x77; // b1 = 7, b2 = 7
        // MSB plane and LSB plane all ones -> selector 3.
        for byte in &mut block[4..] {
            *byte = 0xFF;
        }
        let pixels = decode_rgb_block(&block).expect("individual mode");
        // 0x77 base expands to 119; table 0 selector 3 is -8.
        assert_eq!(pixels[0], [111, 111, 111, 255]);
    }

    #[test]
    fn differential_mode_is_reported_not_guessed() {
        let mut block = WHITE_BLACK;
        block[0] |= 0x80;
        assert!(decode_rgb_block(&block).is_none());
    }

    #[test]
    fn decodes_a_full_image_and_pads_the_edges() {
        // 5x3 image: two block columns, one block row.
        let mut source = Vec::new();
        source.extend_from_slice(&WHITE_BLACK);
        source.extend_from_slice(&WHITE_BLACK);
        let mut destination = vec![0u8; 5 * 3 * 4];
        decode_image(EtcFormat::Rgb8, &source, 5, 3, &mut destination).expect("decode");
        // Pixel 4,2 comes from the second block, left half: white.
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
}
