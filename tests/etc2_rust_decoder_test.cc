#include "mocktail/graphics/etc2_rust_decoder.h"

#include <gtest/gtest.h>

#include <cstdint>
#include <random>
#include <vector>

#include "mocktail/graphics/etc2_decoder.h"

namespace mocktail::graphics {
namespace {

constexpr EtcFormat kFormats[] = {
    EtcFormat::kEtc2Rgb8,       EtcFormat::kEtc2Rgb8A1,
    EtcFormat::kEtc2Rgba8,      EtcFormat::kEacR11,
    EtcFormat::kEacR11Signed,   EtcFormat::kEacRg11,
    EtcFormat::kEacRg11Signed,
};

std::vector<std::uint8_t> RandomBlocks(EtcFormat format, std::uint32_t width,
                                       std::uint32_t height,
                                       std::uint32_t seed) {
  const std::uint32_t blocks = ((width + 3) / 4) * ((height + 3) / 4);
  std::mt19937 engine(seed);
  std::vector<std::uint8_t> bytes(static_cast<std::size_t>(blocks) *
                                  EtcBlockBytes(format));
  for (std::uint8_t& byte : bytes) {
    byte = static_cast<std::uint8_t>(engine());
  }
  return bytes;
}

bool RustAvailable() {
  static const bool available = [] {
    const EtcFormat format = EtcFormat::kEtc2Rgb8;
    const std::vector<std::uint8_t> source(8, 0);
    std::vector<std::uint8_t> destination(4 * 4 * 4, 0);
    return RustDecodeEtcImageBlockRows(format, source.data(), source.size(), 4,
                                       4, 0, 1, destination.data(),
                                       destination.size());
  }();
  return available;
}

// The Rust port must agree with the C++ decoder byte for byte, or a texture
// drawn by one and compared by the other is a rendering bug waiting to happen.
TEST(Etc2RustDecoderTest, MatchesTheCppReference) {
  if (!RustAvailable()) {
    GTEST_SKIP() << "built without the Rust ETC2 decoder";
  }
  std::uint32_t seed = 1;
  for (const EtcFormat format : kFormats) {
    for (const auto& [width, height] :
         {std::pair<std::uint32_t, std::uint32_t>{37, 23},
          {64, 64}, {4, 4}, {1, 9}, {128, 5}, {5, 128}}) {
      const std::vector<std::uint8_t> source =
          RandomBlocks(format, width, height, seed++);
      const std::size_t texels = static_cast<std::size_t>(width) * height *
                                 EtcDecodedTexelBytes(format);

      std::vector<std::uint8_t> expected(texels, 0);
      ASSERT_TRUE(DecodeEtcImage(format, source.data(), source.size(), width,
                                 height, expected.data(), expected.size()));

      std::vector<std::uint8_t> actual(texels, 0);
      ASSERT_TRUE(RustDecodeEtcImageBlockRows(format, source.data(),
                                              source.size(), width, height, 0,
                                              (height + 3) / 4,
                                              actual.data(), actual.size()));
      EXPECT_EQ(actual, expected) << "format " << static_cast<int>(format)
                                  << " " << width << "x" << height;
    }
  }
}

// Bands written by separate threads must not share a byte, which is what lets
// the runtime split one upload across workers.
TEST(Etc2RustDecoderTest, BlockRowBandsMatchTheWholeImage) {
  if (!RustAvailable()) {
    GTEST_SKIP() << "built without the Rust ETC2 decoder";
  }
  std::uint32_t seed = 500;
  for (const EtcFormat format : kFormats) {
    constexpr std::uint32_t kWidth = 37;
    constexpr std::uint32_t kHeight = 41;
    const std::vector<std::uint8_t> source =
        RandomBlocks(format, kWidth, kHeight, seed++);
    const std::size_t texels = static_cast<std::size_t>(kWidth) * kHeight *
                               EtcDecodedTexelBytes(format);

    std::vector<std::uint8_t> whole(texels, 0);
    ASSERT_TRUE(DecodeEtcImage(format, source.data(), source.size(), kWidth,
                               kHeight, whole.data(), whole.size()));

    std::vector<std::uint8_t> banded(texels, 0);
    const std::uint32_t rows = (kHeight + 3) / 4;
    for (std::uint32_t row = 0; row < rows; row += 3) {
      const std::uint32_t count = std::min<std::uint32_t>(3, rows - row);
      ASSERT_TRUE(RustDecodeEtcImageBlockRows(format, source.data(),
                                              source.size(), kWidth, kHeight,
                                              row, count, banded.data(),
                                              banded.size()))
          << "band at row " << row;
    }
    EXPECT_EQ(banded, whole) << "format " << static_cast<int>(format);
  }
}

TEST(Etc2RustDecoderTest, RejectsBandsOutsideTheImage) {
  if (!RustAvailable()) {
    GTEST_SKIP() << "built without the Rust ETC2 decoder";
  }
  const std::vector<std::uint8_t> source = RandomBlocks(
      EtcFormat::kEtc2Rgb8, 8, 8, 7);
  std::vector<std::uint8_t> destination(8 * 8 * 4, 0);
  EXPECT_FALSE(RustDecodeEtcImageBlockRows(EtcFormat::kEtc2Rgb8, source.data(),
                                           source.size(), 8, 8, 2, 1,
                                           destination.data(),
                                           destination.size()));
}

TEST(Etc2RustDecoderTest, RejectsShortBuffers) {
  if (!RustAvailable()) {
    GTEST_SKIP() << "built without the Rust ETC2 decoder";
  }
  const std::vector<std::uint8_t> source = RandomBlocks(
      EtcFormat::kEtc2Rgb8, 8, 8, 11);
  std::vector<std::uint8_t> destination(16, 0);
  EXPECT_FALSE(RustDecodeEtcImageBlockRows(EtcFormat::kEtc2Rgb8, source.data(),
                                           source.size(), 8, 8, 0, 1,
                                           destination.data(),
                                           destination.size()));
}

}  // namespace
}  // namespace mocktail::graphics
