#include "mocktail/graphics/etc2_rust_decoder.h"

#include <cstdio>
#include <cstdlib>
#include <cstring>

#if defined(MAPLEBLOX_HAVE_RUST_ETC2)
extern "C" {
std::uint32_t mapleblox_etc2_abi_version();
std::int32_t mapleblox_etc2_decode_block_rows(
    std::uint32_t format, const std::uint8_t* source, std::size_t source_len,
    std::uint32_t width, std::uint32_t height, std::uint32_t first_block_row,
    std::uint32_t block_row_count, std::uint8_t* destination,
    std::size_t destination_len);
std::int32_t mapleblox_bc7_encode_block_rows(
    const std::uint8_t* rgba, std::size_t rgba_len, std::uint32_t width,
    std::uint32_t height, std::size_t source_stride,
    std::uint32_t first_block_row, std::uint32_t block_row_count,
    std::uint8_t* destination, std::size_t destination_len);
}
#endif

namespace mocktail::graphics {
namespace {

// ABI 5 added the block-row BC7 packer this file calls.
constexpr std::uint32_t kRequiredAbiVersion = 5;

bool RustDecodeRequested() {
#if defined(MAPLEBLOX_HAVE_RUST_ETC2)
  return true;
#else
  return false;
#endif
}

}  // namespace

bool RustEtc2DecoderEnabled() {
  static const bool enabled = [] {
    const char* value = std::getenv("MOCKTAIL_ETC2_RUST_DECODE");
    if (value == nullptr || std::strcmp(value, "0") == 0) {
      return false;
    }
    if (!RustDecodeRequested()) {
      std::fprintf(stderr,
                   "  [vulkan] MOCKTAIL_ETC2_RUST_DECODE is set but this build "
                   "has no Rust decoder; decoding in C++\n");
      return false;
    }
#if defined(MAPLEBLOX_HAVE_RUST_ETC2)
    const std::uint32_t version = mapleblox_etc2_abi_version();
    if (version < kRequiredAbiVersion) {
      std::fprintf(stderr,
                   "  [vulkan] Rust ETC2 decoder reports ABI %u, %u is needed; "
                   "decoding in C++\n",
                   version, kRequiredAbiVersion);
      return false;
    }
    std::fprintf(stderr,
                 "  [vulkan] ETC2 decoding in Rust (ABI %u, "
                 "MOCKTAIL_ETC2_RUST_DECODE)\n",
                 version);
#endif
    return true;
  }();
  return enabled;
}

bool RustDecodeEtcImageBlockRows(EtcFormat format, const std::uint8_t* source,
                                 std::size_t source_bytes, std::uint32_t width,
                                 std::uint32_t height,
                                 std::uint32_t first_block_row,
                                 std::uint32_t block_row_count,
                                 std::uint8_t* destination,
                                 std::size_t destination_bytes) {
#if defined(MAPLEBLOX_HAVE_RUST_ETC2)
  return mapleblox_etc2_decode_block_rows(
             static_cast<std::uint32_t>(format), source, source_bytes, width,
             height, first_block_row, block_row_count, destination,
             destination_bytes) == 0;
#else
  (void)format;
  (void)source;
  (void)source_bytes;
  (void)width;
  (void)height;
  (void)first_block_row;
  (void)block_row_count;
  (void)destination;
  (void)destination_bytes;
  return false;
#endif
}

bool RustBc7PackerEnabled() {
  static const bool enabled = [] {
    if (!RustEtc2DecoderEnabled()) {
      return false;
    }
    const char* value = std::getenv("MOCKTAIL_ETC2_BC_PACK");
    if (value == nullptr || std::strcmp(value, "0") == 0) {
      return false;
    }
    std::fprintf(stderr,
                 "  [vulkan] ETC2 uploads packed to BC7 on the CPU "
                 "(MOCKTAIL_ETC2_BC_PACK)\n");
    return true;
  }();
  return enabled;
}

bool RustPackBc7BlockRows(const std::uint8_t* rgba, std::uint32_t width,
                          std::uint32_t height, std::size_t source_stride,
                          std::uint32_t first_block_row,
                          std::uint32_t block_row_count,
                          std::uint8_t* destination,
                          std::size_t destination_bytes) {
#if defined(MAPLEBLOX_HAVE_RUST_ETC2)
  if (!RustBc7PackerEnabled()) {
    return false;
  }
  const std::size_t rgba_bytes =
      source_stride * (height == 0 ? 0 : height - 1) +
      std::size_t{width} * 4;
  return mapleblox_bc7_encode_block_rows(
             rgba, rgba_bytes, width, height, source_stride, first_block_row,
             block_row_count, destination, destination_bytes) == 0;
#else
  (void)rgba;
  (void)width;
  (void)height;
  (void)source_stride;
  (void)first_block_row;
  (void)block_row_count;
  (void)destination;
  (void)destination_bytes;
  return false;
#endif
}

}  // namespace mocktail::graphics
