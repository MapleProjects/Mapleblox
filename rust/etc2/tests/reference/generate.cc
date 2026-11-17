// Renders the C++ reference decoder over deterministic pseudo-random blocks so
// the Rust port can be compared byte for byte.
//
//   g++ -O2 -std=c++17 -I include rust/etc2/tests/reference/generate.cc \
//       src/graphics/etc2_decoder.cc -o /tmp/etc2-generate -lpthread
//   /tmp/etc2-generate rust/etc2/tests/vectors.bin
//
// The output is a sequence of cases: format (u32 LE), width, height, source
// length, source bytes, destination length, destination bytes.

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <random>
#include <vector>

#include "mocktail/graphics/etc2_decoder.h"

namespace {

void Emit(std::FILE* file, mocktail::graphics::EtcFormat format, std::uint32_t width,
          std::uint32_t height, const std::vector<std::uint8_t>& source) {
  const std::size_t texel_bytes =
      mocktail::graphics::EtcDecodedTexelBytes(format);
  std::vector<std::uint8_t> destination(
      static_cast<std::size_t>(width) * height * texel_bytes);
  if (!mocktail::graphics::DecodeEtcImage(
          format, source.data(), source.size(), width, height,
          destination.data(), destination.size())) {
    std::fprintf(stderr, "reference decode failed\n");
    std::exit(1);
  }
  const std::uint32_t fields[4] = {static_cast<std::uint32_t>(format), width,
                                   height,
                                   static_cast<std::uint32_t>(source.size())};
  std::fwrite(fields, sizeof(fields), 1, file);
  std::fwrite(source.data(), 1, source.size(), file);
  const std::uint32_t destination_len =
      static_cast<std::uint32_t>(destination.size());
  std::fwrite(&destination_len, sizeof(destination_len), 1, file);
  std::fwrite(destination.data(), 1, destination.size(), file);
}

}  // namespace

int main(int argc, char** argv) {
  const char* path = argc > 1 ? argv[1] : "vectors.bin";
  std::FILE* file = std::fopen(path, "wb");
  if (file == nullptr) {
    std::perror(path);
    return 1;
  }
  std::mt19937 rng(0x5EED1234u);
  const mocktail::graphics::EtcFormat formats[] = {
      mocktail::graphics::EtcFormat::kEtc2Rgb8,
      mocktail::graphics::EtcFormat::kEtc2Rgb8A1,
      mocktail::graphics::EtcFormat::kEtc2Rgba8,
      mocktail::graphics::EtcFormat::kEacR11,
      mocktail::graphics::EtcFormat::kEacR11Signed,
      mocktail::graphics::EtcFormat::kEacRg11,
      mocktail::graphics::EtcFormat::kEacRg11Signed,
  };
  const std::uint32_t sizes[][2] = {{4, 4},   {8, 4},   {4, 8},  {8, 8},
                                    {12, 20}, {20, 12}, {64, 64}};
  std::size_t cases = 0;
  for (const auto format : formats) {
    for (const auto& size : sizes) {
      const std::uint32_t width = size[0];
      const std::uint32_t height = size[1];
      const std::size_t blocks =
          static_cast<std::size_t>((width + 3) / 4) * ((height + 3) / 4);
      std::vector<std::uint8_t> source(
          blocks * mocktail::graphics::EtcBlockBytes(format));
      for (auto& byte : source) {
        byte = static_cast<std::uint8_t>(rng() & 0xFF);
      }
      Emit(file, format, width, height, source);
      ++cases;
    }
  }
  std::fclose(file);
  std::fprintf(stderr, "wrote %zu cases to %s\n", cases, path);
  return 0;
}