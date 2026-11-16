#include "runtime/texture_memory_policy.h"

#define JSON_NOEXCEPTION 1
#include <algorithm>
#include <cstdint>
#include <fstream>
#include <limits>
#include <nlohmann/json.hpp>
#include <string>

// Mapleblox: the numeric policy lives in rust/policy and links into the runtime
// as a static archive. This translation unit owns the ABI and the JSON
// plumbing; the decisions themselves are unit tested on the Rust side.
extern "C" {
std::uint64_t mapleblox_texture_budget_bytes(std::uint64_t video_bytes,
                                             std::uint64_t host_bytes);
std::uint64_t mapleblox_fflag_video_memory_bytes(std::uint64_t budget_bytes);
std::uint64_t mapleblox_detect_video_memory_bytes(void);
}

namespace mocktail {
namespace runtime {
namespace {

constexpr std::string_view kVideoMemoryOverride =
    "FIntRenderForceVideoMemorySize";

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
  return mapleblox_texture_budget_bytes(video_memory_bytes, host_memory_bytes);
}

std::uint64_t CalculateTextureMemoryBudgetBytes(
    std::uint64_t host_memory_bytes) {
  return CalculateTextureMemoryBudgetBytesForVideo(
      mapleblox_detect_video_memory_bytes(), host_memory_bytes);
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
    overrides[key] = std::to_string(
        mapleblox_fflag_video_memory_bytes(budget_bytes));
  }
  *merged_json = overrides.dump();
  return true;
}

}  // namespace runtime
}  // namespace mocktail