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

}  // namespace mocktail::graphics

#endif  // MOCKTAIL_GRAPHICS_ETC2_RUST_DECODER_H_
