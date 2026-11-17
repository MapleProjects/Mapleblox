#ifndef MOCKTAIL_GRAPHICS_ETC2_RUST_DECODER_H_
#define MOCKTAIL_GRAPHICS_ETC2_RUST_DECODER_H_

#include <cstddef>
#include <cstdint>

#include "mocktail/graphics/etc2_decoder.h"

namespace mocktail::graphics {

// Whether this binary links the Rust decoder and MOCKTAIL_ETC2_RUST_DECODE
// leaves it on. Read once: every upload asks.
bool RustEtc2DecoderEnabled();

// Same contract as DecodeEtcImageBlockRows, decoded by the Rust port. False
// when the Rust decoder is not compiled in, is turned off, or declines the
// call, so the caller can fall back to the C++ decoder.
bool RustDecodeEtcImageBlockRows(EtcFormat format, const std::uint8_t* source,
                                 std::size_t source_bytes, std::uint32_t width,
                                 std::uint32_t height,
                                 std::uint32_t first_block_row,
                                 std::uint32_t block_row_count,
                                 std::uint8_t* destination,
                                 std::size_t destination_bytes);

// Whether the CPU packer that turns decoded texels into BC7 blocks is on. It
// needs the Rust decoder, and MOCKTAIL_ETC2_BC_PACK turns it on; off by
// default so the RGBA8 path stays the one that ships until it is measured.
bool RustBc7PackerEnabled();

// Packs `block_row_count` block rows of `width` RGBA8 texels, `source_stride`
// bytes apart, into BC7 blocks of 16 bytes, starting at `first_block_row`.
// `destination` holds only the band, `blocks_wide * block_row_count` blocks, so
// the bands of one image can be packed on separate threads. False when the
// packer is off or declines, so the caller can leave the image decoded.
bool RustPackBc7BlockRows(const std::uint8_t* rgba, std::uint32_t width,
                          std::uint32_t height, std::size_t source_stride,
                          std::uint32_t first_block_row,
                          std::uint32_t block_row_count,
                          std::uint8_t* destination,
                          std::size_t destination_bytes);

}  // namespace mocktail::graphics

#endif  // MOCKTAIL_GRAPHICS_ETC2_RUST_DECODER_H_
