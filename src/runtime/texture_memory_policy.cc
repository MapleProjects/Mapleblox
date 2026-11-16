#include "runtime/texture_memory_policy.h"

#define JSON_NOEXCEPTION 1
#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <fstream>
#include <limits>
#include <nlohmann/json.hpp>
#include <string>

namespace mocktail {
namespace runtime {
namespace {

constexpr std::uint64_t kMebibyte = 1024U * 1024U;
// Below this the guest default is already a reasonable share of the host.
constexpr std::uint64_t kMinimumHostMemoryBytes = 2048U * kMebibyte;
constexpr std::uint64_t kMinimumBudgetBytes = 256U * kMebibyte;
constexpr std::uint64_t kMaximumBudgetBytes = 1536U * kMebibyte;
// The desktop compositor and the driver keep a slice of the board for
// themselves, so the guest never receives the whole thing.
constexpr std::uint64_t kVideoMemoryReserveBytes = 512U * kMebibyte;
constexpr std::string_view kVideoMemoryOverride =
    "FIntRenderForceVideoMemorySize";

// Mapleblox: dedicated video memory beats the system RAM heuristic. The
// driver does not export it through procfs on 600-series releases, so the
// only reliable source is nvidia-smi. The query runs once during startup.
std::uint64_t DetectDedicatedVideoMemoryBytes() {
  FILE* pipe = popen(
      "nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits"
      " 2>/dev/null",
      "r");
  if (pipe == nullptr) {
    return 0;
  }
  unsigned long long mebibytes = 0;
  const int fields = std::fscanf(pipe, "%llu", &mebibytes);
  pclose(pipe);
  if (fields != 1 || mebibytes == 0) {
    return 0;
  }
  return static_cast<std::uint64_t>(mebibytes) * kMebibyte;
}

}  // namespace

std::uint64_t DetectHostMemoryBytes() {
  std::ifstream input("/proc/meminfo");
  std::string key;
  while (input >> key) {
    if (key != "MemTotal:") {
      input.ignore(std::numeric_limits<std::streamsize>::max(), '\n');
      continue;
    }
    std::uint64_t kibibytes = 0;
    if (!(input >> kibibytes)) {
      break;
    }
    if (kibibytes > std::numeric_limits<std::uint64_t>::max() / 1024U) {
      break;
    }
    return kibibytes * 1024U;
  }
  return 0;
}

std::uint64_t CalculateTextureMemoryBudgetBytesForVideo(
    std::uint64_t video_memory_bytes, std::uint64_t host_memory_bytes) {
  if (video_memory_bytes > kVideoMemoryReserveBytes) {
    return std::max(kMinimumBudgetBytes,
                    video_memory_bytes - kVideoMemoryReserveBytes);
  }
  if (host_memory_bytes < kMinimumHostMemoryBytes) {
    return 0;
  }
  return std::clamp(host_memory_bytes / 8U, kMinimumBudgetBytes,
                    kMaximumBudgetBytes);
}

std::uint64_t CalculateTextureMemoryBudgetBytes(
    std::uint64_t host_memory_bytes) {
  return CalculateTextureMemoryBudgetBytesForVideo(
      DetectDedicatedVideoMemoryBytes(), host_memory_bytes);
}

bool MergeTextureMemoryClientSettingsOverrides(std::uint64_t budget_bytes,
                                               std::string_view base_json,
                                               std::string* merged_json,
                                               std::string* error) {
  if (merged_json == nullptr) {
    if (error != nullptr) {
      *error = "texture memory client-settings output is required";
    }
    return false;
  }
  nlohmann::json overrides = nlohmann::json::parse(
      base_json.empty() ? "{}" : base_json, nullptr, false, true);
  if (overrides.is_discarded() || !overrides.is_object()) {
    if (error != nullptr) {
      *error = "texture memory client-settings input must be a JSON object";
    }
    return false;
  }
  const std::string key(kVideoMemoryOverride);
  if (budget_bytes != 0 && !overrides.contains(key)) {
    // The override is a Roblox FInt, so it cannot carry more than INT32_MAX.
    const std::uint64_t clamped = std::min<std::uint64_t>(
        budget_bytes,
        static_cast<std::uint64_t>(std::numeric_limits<std::int32_t>::max()));
    overrides[key] = std::to_string(clamped);
  }
  *merged_json = overrides.dump();
  return true;
}

}  // namespace runtime
}  // namespace mocktail
