//! BC7 and BC4/BC5 encoders.
//!
//! The runtime keeps emulated ETC2 textures in a BC host image so a 4x4 block
//! costs 16 bytes instead of the 64 an RGBA8 image needs. The GPU encoder that
//! used to fill those blocks is the path this crate replaces, so the packing
//! happens here, on the CPU, right after the decode.
//!
//! Only BC7 mode 6 is emitted: one subset, RGBA endpoints at 7 bits plus a
//! shared p-bit, and 4-bit indices. It is the only mode that carries alpha
//! without a second index set, and its 8-bit effective endpoints are close
//! enough to BC1 that the difference is not visible on the textures this
//! runtime sees. Modes with partitions would buy a little more quality for a
//! lot more code.
//!
//! The decoders below exist so the tests can measure the error of what the
//! encoders produce; nothing in the runtime calls them.

/// Interpolation weights of a 4-bit BC7 index, in 1/64ths.
const WEIGHTS4: [u32; 16] = [
    0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64,
];

/// Interpolation weights of a 3-bit BC4 index, in 1/7ths. Index 0 and 1 are the
/// endpoints themselves, so index `i` beyond that sits at `(i - 1) / 7`.
const WEIGHTS3: [u32; 8] = [0, 1, 1, 2, 3, 4, 5, 6];

/// Blends two 8-bit endpoints with a 4-bit index.
fn blend4(low: u8, high: u8, index: u32) -> u8 {
    let weight = WEIGHTS4[index as usize];
    let value = (64 - weight) * low as u32 + weight * high as u32 + 32;
    (value >> 6) as u8
}

/// Blends two 8-bit endpoints with a 3-bit index, as BC4 does: index 0 is the
/// low endpoint, index 1 the high one, and the rest spread between them.
fn blend3(low: u8, high: u8, index: u32) -> u8 {
    if index == 0 {
        return low;
    }
    if index == 1 {
        return high;
    }
    let weight = WEIGHTS3[index as usize];
    let value = (7 - weight) * low as u32 + weight * high as u32 + 3;
    (value / 7) as u8
}

/// The sixteen levels a pair of endpoints blends to. Building them once per
/// candidate turns every texel measurement into a lookup, which is what keeps
/// the search away from sixteen blends per texel.
fn build_levels(low: [u8; 4], high: [u8; 4]) -> [[u8; 4]; 16] {
    let mut levels = [[0u8; 4]; 16];
    for (index, level) in levels.iter_mut().enumerate() {
        for channel in 0..4 {
            level[channel] = blend4(low[channel], high[channel], index as u32);
        }
    }
    levels
}

/// Squared error of one texel against a level of the pair.
fn level_error(levels: &[[u8; 4]; 16], index: u32, texel: &[u8]) -> u32 {
    let level = &levels[index as usize];
    let mut error = 0;
    for channel in 0..4 {
        let difference = level[channel] as i32 - texel[channel] as i32;
        error += (difference * difference) as u32;
    }
    error
}

/// Index of the weight nearest to a projection in 1/64ths, so the search for
/// the closest level is a table read instead of sixteen blends.
const NEAREST_WEIGHT: [u8; 65] = build_nearest_weight();

const fn build_nearest_weight() -> [u8; 65] {
    let mut table = [0u8; 65];
    let mut scaled = 0;
    while scaled < 65 {
        let mut best = 0u8;
        let mut best_distance = i32::MAX;
        let mut index = 0;
        while index < 16 {
            let distance = WEIGHTS4[index] as i32 - scaled as i32;
            let distance = if distance < 0 { -distance } else { distance };
            if distance < best_distance {
                best_distance = distance;
                best = index as u8;
            }
            index += 1;
        }
        table[scaled] = best;
        scaled += 1;
    }
    table
}

/// Rounds an endpoint to the 8-bit values mode 6 can represent: a 7-bit field
/// plus one p-bit shared by all four channels, so every channel keeps the same
/// parity.
fn quantize_endpoint(endpoint: [f32; 4], parity: u8) -> [u8; 4] {
    let mut out = [0u8; 4];
    for channel in 0..4 {
        let clamped = endpoint[channel].clamp(0.0, 255.0).round() as i32;
        out[channel] = ((clamped as u8) & 0xFE) | parity;
    }
    out
}

/// Least-squares endpoints for a set of texels and their indices, in the
/// 1/64ths space the blend uses.
fn fit_endpoints(texels: &[[u8; 4]], indices: &[u32; 16]) -> ([f32; 4], [f32; 4]) {
    let mut sum_aa = 0.0f64;
    let mut sum_ab = 0.0f64;
    let mut sum_bb = 0.0f64;
    let mut sum_ac = [0.0f64; 4];
    let mut sum_bc = [0.0f64; 4];
    for (texel, &index) in texels.iter().zip(indices.iter()) {
        let weight = WEIGHTS4[index as usize] as f64 / 64.0;
        let a = 1.0 - weight;
        let b = weight;
        sum_aa += a * a;
        sum_ab += a * b;
        sum_bb += b * b;
        for channel in 0..4 {
            let value = texel[channel] as f64;
            sum_ac[channel] += a * value;
            sum_bc[channel] += b * value;
        }
    }
    let determinant = sum_aa * sum_bb - sum_ab * sum_ab;
    let mut low = [0.0f32; 4];
    let mut high = [0.0f32; 4];
    if determinant.abs() < 1e-6 {
        // Every texel landed on the same weight, so the line is not determined:
        // keep the endpoints where the caller put them.
        return (low, high);
    }
    for channel in 0..4 {
        let low_value = (sum_ac[channel] * sum_bb - sum_bc[channel] * sum_ab) / determinant;
        let high_value = (sum_bc[channel] * sum_aa - sum_ac[channel] * sum_ab) / determinant;
        low[channel] = low_value as f32;
        high[channel] = high_value as f32;
    }
    (low, high)
}

/// Total error of a block for a pair of endpoints and its indices.
fn block_error(texels: &[[u8; 4]], low: [u8; 4], high: [u8; 4], indices: &[u32; 16]) -> u64 {
    let levels = build_levels(low, high);
    let mut total = 0u64;
    for (texel, &index) in texels.iter().zip(indices.iter()) {
        total += level_error(&levels, index, texel) as u64;
    }
    total
}

/// Picks the indices of every texel for a pair of endpoints.
///
/// The level is found by projecting the texel onto the line the endpoints
/// span, which is the choice a least-squares fit expects, and the two
/// neighbouring levels are then measured exactly so the discrete pick is never
/// worse than the projection. Sixteen blends per texel, as an exhaustive
/// search would need, is what made packing cost ten times the decode.
fn assign_indices(texels: &[[u8; 4]; 16], low: [u8; 4], high: [u8; 4]) -> [u32; 16] {
    let mut direction = [0i32; 4];
    let mut length = 0i32;
    for channel in 0..4 {
        direction[channel] = high[channel] as i32 - low[channel] as i32;
        length += direction[channel] * direction[channel];
    }
    let mut indices = [0u32; 16];
    if length == 0 {
        return indices;
    }
    let levels = build_levels(low, high);
    // Scaling by a reciprocal keeps the one integer division per candidate
    // instead of one per texel, which was the bulk of the packing cost.
    let reciprocal = (((1u64 << 24) + (length as u64) / 2) / length as u64) as i64;
    for (slot, texel) in indices.iter_mut().zip(texels.iter()) {
        let mut projection = 0i64;
        for channel in 0..4 {
            projection +=
                (texel[channel] as i64 - low[channel] as i64) * direction[channel] as i64;
        }
        let scaled = ((projection * 64 * reciprocal) >> 24).clamp(0, 64);
        let nearest = NEAREST_WEIGHT[scaled as usize] as u32;
        let mut best = nearest;
        let mut best_error = level_error(&levels, nearest, texel);
        for candidate in nearest.saturating_sub(1)..=(nearest + 1).min(15) {
            let error = level_error(&levels, candidate, texel);
            if error < best_error {
                best_error = error;
                best = candidate;
            }
        }
        *slot = best;
    }
    indices
}

/// The p-bit of an endpoint: the parity that leaves the four channels closest
/// to the value being quantized. Both parities round every channel down to the
/// even neighbour, so the choice only moves the odd channels by one level, and
/// picking it here costs one pass instead of the four the search used to try.
fn best_parity(endpoint: [f32; 4]) -> u8 {
    let mut even_error = 0.0f32;
    let mut odd_error = 0.0f32;
    for channel in 0..4 {
        let value = endpoint[channel].clamp(0.0, 255.0);
        let even = (value.round() as i32) & !1;
        let below = even as f32 - value;
        let above = (even + 1) as f32 - value;
        even_error += below * below;
        odd_error += above * above;
    }
    if odd_error < even_error {
        1
    } else {
        0
    }
}

/// Encodes one 4x4 RGBA8 block, 64 bytes, into a BC7 mode 6 block.
pub fn encode_block(rgba: &[u8]) -> [u8; 16] {
    let mut texels = [[0u8; 4]; 16];
    for (slot, texel) in texels.iter_mut().enumerate() {
        texel.copy_from_slice(&rgba[slot * 4..slot * 4 + 4]);
    }

    // Two starting guesses: the bounding box of the block, and the pair of
    // texels farthest apart. The second one follows a gradient better, the
    // first one keeps a flat block from collapsing onto one texel.
    let mut low_box = [255u8; 4];
    let mut high_box = [0u8; 4];
    for texel in texels.iter() {
        for channel in 0..4 {
            low_box[channel] = low_box[channel].min(texel[channel]);
            high_box[channel] = high_box[channel].max(texel[channel]);
        }
    }
    let mut mean = [0.0f32; 4];
    for texel in texels.iter() {
        for channel in 0..4 {
            mean[channel] += texel[channel] as f32 / 16.0;
        }
    }
    let distance = |texel: &[u8; 4], from: &[f32; 4]| -> f32 {
        let mut total = 0.0;
        for channel in 0..4 {
            let difference = texel[channel] as f32 - from[channel];
            total += difference * difference;
        }
        total
    };
    let far = *texels
        .iter()
        .max_by(|a, b| {
            distance(a, &mean)
                .partial_cmp(&distance(b, &mean))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(&texels[0]);
    let far_point = [
        far[0] as f32,
        far[1] as f32,
        far[2] as f32,
        far[3] as f32,
    ];
    let opposite = *texels
        .iter()
        .max_by(|a, b| {
            distance(a, &far_point)
                .partial_cmp(&distance(b, &far_point))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(&texels[0]);

    let mut best_error = u64::MAX;
    let mut best_low = low_box;
    let mut best_high = high_box;
    let mut best_indices = [0u32; 16];

    let starts: [([f32; 4], [f32; 4]); 2] = [
        (
            [
                low_box[0] as f32,
                low_box[1] as f32,
                low_box[2] as f32,
                low_box[3] as f32,
            ],
            [
                high_box[0] as f32,
                high_box[1] as f32,
                high_box[2] as f32,
                high_box[3] as f32,
            ],
        ),
        (
            [
                opposite[0] as f32,
                opposite[1] as f32,
                opposite[2] as f32,
                opposite[3] as f32,
            ],
            far_point,
        ),
    ];

    // Both starts are measured once and only the better one is refined, which
    // costs four candidates instead of six and lands on the same endpoints for
    // the textures this runtime sees.
    for (start_low, start_high) in starts {
        // The p-bit is shared by the four channels of an endpoint, so each
        // endpoint takes the parity that fits it best.
        let quantized_low = quantize_endpoint(start_low, best_parity(start_low));
        let quantized_high = quantize_endpoint(start_high, best_parity(start_high));
        let indices = assign_indices(&texels, quantized_low, quantized_high);
        let error = block_error(&texels, quantized_low, quantized_high, &indices);
        if error < best_error {
            best_error = error;
            best_low = quantized_low;
            best_high = quantized_high;
            best_indices = indices;
        }
    }
    for _ in 0..2 {
        let (fitted_low, fitted_high) = fit_endpoints(&texels, &best_indices);
        if fitted_low == [0.0; 4] && fitted_high == [0.0; 4] {
            break;
        }
        let quantized_low = quantize_endpoint(fitted_low, best_parity(fitted_low));
        let quantized_high = quantize_endpoint(fitted_high, best_parity(fitted_high));
        let indices = assign_indices(&texels, quantized_low, quantized_high);
        let error = block_error(&texels, quantized_low, quantized_high, &indices);
        if error < best_error {
            best_error = error;
            best_low = quantized_low;
            best_high = quantized_high;
            best_indices = indices;
        }
    }

    // Mode 6 stores 63 bits of index data: the first texel is the anchor and
    // keeps only three bits of its index. Swapping the endpoints inverts every
    // index, which brings that one into range without touching the others.
    if best_indices[0] >= 8 {
        std::mem::swap(&mut best_low, &mut best_high);
        for index in best_indices.iter_mut() {
            *index = 15 - *index;
        }
    }

    let mut block = [0u8; 16];
    let mut writer = BitWriter::new(&mut block);
    writer.write(0x40, 7); // mode 6: six clear bits then the marker
    for channel in 0..4 {
        writer.write((best_low[channel] >> 1) as u32, 7);
        writer.write((best_high[channel] >> 1) as u32, 7);
    }
    writer.write((best_low[0] & 1) as u32, 1);
    writer.write((best_high[0] & 1) as u32, 1);
    writer.write(best_indices[0] & 0x7, 3);
    for index in best_indices.iter().skip(1) {
        writer.write(*index, 4);
    }
    block
}

/// Decodes a BC7 mode 6 block back to 4x4 RGBA8. Only mode 6 is understood,
/// which is all this crate emits; the tests use it to measure the error.
pub fn decode_block(block: &[u8; 16]) -> Option<[u8; 64]> {
    let mut reader = BitReader::new(block);
    if reader.read(7) != 0x40 {
        return None;
    }
    let mut low = [0u8; 4];
    let mut high = [0u8; 4];
    for channel in 0..4 {
        low[channel] = (reader.read(7) as u8) << 1;
        high[channel] = (reader.read(7) as u8) << 1;
    }
    let low_parity = reader.read(1) as u8;
    let high_parity = reader.read(1) as u8;
    for channel in 0..4 {
        low[channel] |= low_parity;
        high[channel] |= high_parity;
    }
    let mut indices = [0u32; 16];
    indices[0] = reader.read(3);
    for index in indices.iter_mut().skip(1) {
        *index = reader.read(4);
    }
    let mut out = [0u8; 64];
    for (slot, &index) in indices.iter().enumerate() {
        for channel in 0..4 {
            out[slot * 4 + channel] = blend4(low[channel], high[channel], index);
        }
    }
    Some(out)
}

/// Encodes 16 single-channel values into a BC4 block, using the eight-value
/// mode so every index keeps three bits.
pub fn encode_bc4(values: &[u8]) -> [u8; 8] {
    let mut low = 255u8;
    let mut high = 0u8;
    for &value in values.iter().take(16) {
        low = low.min(value);
        high = high.max(value);
    }
    // The eight-value mode needs the first endpoint above the second.
    if low == high {
        if low == 0 {
            high = 1;
        } else {
            low = high - 1;
        }
    }
    let mut indices = [0u32; 16];
    for (slot, &value) in indices.iter_mut().zip(values.iter().take(16)) {
        let mut best = 0;
        let mut best_error = u32::MAX;
        for index in 0..8u32 {
            let error = (blend3(low, high, index) as i32 - value as i32).unsigned_abs();
            if error < best_error {
                best_error = error;
                best = index;
            }
        }
        *slot = best;
    }
    let mut block = [0u8; 8];
    block[0] = low;
    block[1] = high;
    let mut writer = BitWriter::new(&mut block[2..]);
    for index in indices.iter() {
        writer.write(*index, 3);
    }
    block
}

/// Decodes a BC4 block back to 16 values, for the tests.
pub fn decode_bc4(block: &[u8; 8]) -> [u8; 16] {
    let low = block[0];
    let high = block[1];
    let mut reader = BitReader::new(&block[2..]);
    let mut out = [0u8; 16];
    for slot in out.iter_mut() {
        let index = reader.read(3);
        *slot = blend3(low, high, index);
    }
    out
}

/// Writes fields least significant bit first, the order BC blocks use.
struct BitWriter<'a> {
    bytes: &'a mut [u8],
    bit: usize,
}

impl<'a> BitWriter<'a> {
    fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes, bit: 0 }
    }

    fn write(&mut self, value: u32, count: u32) {
        for offset in 0..count {
            let set = (value >> offset) & 1;
            if set != 0 {
                let byte = self.bit / 8;
                let within = self.bit % 8;
                if byte < self.bytes.len() {
                    self.bytes[byte] |= 1 << within;
                }
            }
            self.bit += 1;
        }
    }
}

/// Reads fields least significant bit first.
struct BitReader<'a> {
    bytes: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit: 0 }
    }

    fn read(&mut self, count: u32) -> u32 {
        let mut value = 0u32;
        for offset in 0..count {
            let byte = self.bit / 8;
            let within = self.bit % 8;
            let set = if byte < self.bytes.len() {
                (self.bytes[byte] >> within) & 1
            } else {
                0
            };
            value |= (set as u32) << offset;
            self.bit += 1;
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_of(texels: &[[u8; 4]; 16]) -> Vec<u8> {
        let mut rgba = Vec::with_capacity(64);
        for texel in texels.iter() {
            rgba.extend_from_slice(texel);
        }
        rgba
    }

    fn mean_squared_error(original: &[u8], decoded: &[u8]) -> f64 {
        let mut total = 0.0;
        for (a, b) in original.iter().zip(decoded.iter()) {
            let difference = *a as f64 - *b as f64;
            total += difference * difference;
        }
        total / original.len() as f64
    }

    #[test]
    fn a_flat_block_round_trips_exactly() {
        // Mode 6 shares one p-bit between the four channels of an endpoint, so
        // a colour only survives untouched when its channels agree on parity.
        let texels = [[36u8, 120, 200, 254]; 16];
        let rgba = block_of(&texels);
        let block = encode_block(&rgba);
        let decoded = decode_block(&block).expect("mode 6");
        assert_eq!(&decoded[..], &rgba[..]);
    }

    #[test]
    fn a_flat_block_with_mixed_parity_loses_one_level() {
        let texels = [[37u8, 120, 200, 255]; 16];
        let rgba = block_of(&texels);
        let block = encode_block(&rgba);
        let decoded = decode_block(&block).expect("mode 6");
        for (original, restored) in rgba.iter().zip(decoded.iter()) {
            let difference = (*original as i32 - *restored as i32).abs();
            assert!(difference <= 1, "{original} became {restored}");
        }
    }

    #[test]
    fn a_gradient_stays_within_a_few_levels() {
        let mut texels = [[0u8; 4]; 16];
        for (slot, texel) in texels.iter_mut().enumerate() {
            let value = (slot * 17) as u8;
            *texel = [value, value, value, 255];
        }
        let rgba = block_of(&texels);
        let block = encode_block(&rgba);
        let decoded = decode_block(&block).expect("mode 6");
        let error = mean_squared_error(&rgba, &decoded);
        assert!(error < 4.0, "mean squared error {error} is too high");
    }

    #[test]
    fn alpha_is_carried_by_the_same_endpoints() {
        let mut texels = [[200u8, 40, 40, 0]; 16];
        for (slot, texel) in texels.iter_mut().enumerate() {
            texel[3] = (slot * 17) as u8;
        }
        let rgba = block_of(&texels);
        let block = encode_block(&rgba);
        let decoded = decode_block(&block).expect("mode 6");
        let error = mean_squared_error(&rgba, &decoded);
        assert!(error < 8.0, "mean squared error {error} is too high");
    }

    #[test]
    fn the_anchor_index_never_needs_its_top_bit() {
        // The first texel is the anchor and only stores three index bits, so a
        // block whose far end sits on texel 0 would otherwise want index 15
        // there, which mode 6 cannot hold.
        let mut texels = [[0u8, 0, 0, 255]; 16];
        texels[0] = [255, 255, 255, 255];
        let rgba = block_of(&texels);
        let block = encode_block(&rgba);
        let decoded = decode_block(&block).expect("mode 6");
        assert_eq!(&decoded[0..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn bc4_round_trips_a_ramp() {
        // Eight levels over the whole range, so a step of 36 is as close as a
        // block can land on an arbitrary value.
        let mut values = [0u8; 16];
        for (slot, value) in values.iter_mut().enumerate() {
            *value = (slot / 2 * 36) as u8;
        }
        let block = encode_bc4(&values);
        let decoded = decode_bc4(&block);
        for (original, restored) in values.iter().zip(decoded.iter()) {
            let difference = (*original as i32 - *restored as i32).abs();
            assert!(difference <= 2, "{original} became {restored}");
        }
    }

    #[test]
    fn bc4_stays_within_half_a_step_of_any_value() {
        let values: [u8; 16] = [0, 17, 34, 51, 68, 85, 102, 119, 136, 153, 170, 187, 204, 221, 238, 255];
        let block = encode_bc4(&values);
        let decoded = decode_bc4(&block);
        for (original, restored) in values.iter().zip(decoded.iter()) {
            let difference = (*original as i32 - *restored as i32).abs();
            assert!(difference <= 19, "{original} became {restored}");
        }
    }

    #[test]
    fn bc4_handles_a_flat_block() {
        let values = [77u8; 16];
        let block = encode_bc4(&values);
        let decoded = decode_bc4(&block);
        for restored in decoded.iter() {
            assert_eq!(*restored, 77);
        }
    }
}

