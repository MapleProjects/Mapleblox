#include "runtime/stall_recorder.h"

#include <atomic>
#include <algorithm>
#include <cerrno>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <ctime>
#include <filesystem>
#include <map>
#include <mutex>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include <dirent.h>
#include <time.h>

namespace mocktail {
namespace runtime {
namespace {

constexpr std::uint64_t kDefaultThresholdMs = 250;
constexpr std::uint64_t kNanosecondsPerMillisecond = 1000000ULL;
constexpr long kMaximumRecordBytes = 8L * 1024L * 1024L;
constexpr std::size_t kEventCapacity = 24;
constexpr std::size_t kCategoryBytes = 24;
constexpr std::size_t kDetailBytes = 96;
constexpr std::size_t kThreadReportCount = 5;
constexpr int kSamplerPeriodMs = 200;

struct GraphicsEvent {
  std::uint64_t monotonic_ns = 0;
  double duration_ms = 0.0;
  char category[kCategoryBytes] = {};
  char detail[kDetailBytes] = {};
};

struct ThreadUsage {
  std::string name;
  std::uint64_t ticks = 0;
};

std::atomic<std::uint64_t> g_last_present_ns{0};
std::atomic<std::uint64_t> g_threshold_ns{0};
std::mutex g_mutex;
std::string g_record_path;
bool g_path_resolved = false;
GraphicsEvent g_events[kEventCapacity];
std::size_t g_event_count = 0;
std::map<int, ThreadUsage> g_thread_baseline;
bool g_sampler_started = false;

std::uint64_t NowNanoseconds() noexcept {
  struct timespec now = {};
  clock_gettime(CLOCK_MONOTONIC, &now);
  return static_cast<std::uint64_t>(now.tv_sec) * 1000000000ULL +
         static_cast<std::uint64_t>(now.tv_nsec);
}

// USER_HZ is 100 on Linux, so one tick is 10 ms.
std::map<int, ThreadUsage> ReadThreadUsage() noexcept {
  std::map<int, ThreadUsage> usage;
  DIR* tasks = opendir("/proc/self/task");
  if (tasks == nullptr) {
    return usage;
  }
  while (dirent* entry = readdir(tasks)) {
    const int tid = std::atoi(entry->d_name);
    if (tid <= 0) {
      continue;
    }
    char path[96] = {};
    std::snprintf(path, sizeof(path), "/proc/self/task/%d/stat", tid);
    FILE* stat_file = std::fopen(path, "r");
    if (stat_file == nullptr) {
      continue;
    }
    char line[1024] = {};
    const std::size_t read = std::fread(line, 1, sizeof(line) - 1, stat_file);
    std::fclose(stat_file);
    line[read] = '\0';
    const char* close = std::strrchr(line, ')');
    if (close == nullptr) {
      continue;
    }
    // After the command, state is field 3; utime and stime are fields 14 and 15,
    // which are indexes 11 and 12 in the slice that starts at state.
    char state = 0;
    long long fields[13] = {};
    const int parsed =
        std::sscanf(close + 1,
                    " %c %lld %lld %lld %lld %lld %lld %lld %lld %lld %lld"
                    " %lld %lld",
                    &state, &fields[0], &fields[1], &fields[2], &fields[3],
                    &fields[4], &fields[5], &fields[6], &fields[7],
                    &fields[8], &fields[9], &fields[10], &fields[11]);
    if (parsed < 13) {
      continue;
    }
    ThreadUsage thread;
    char comm_path[96] = {};
    std::snprintf(comm_path, sizeof(comm_path), "/proc/self/task/%d/comm", tid);
    FILE* comm_file = std::fopen(comm_path, "r");
    if (comm_file != nullptr) {
      char comm[32] = {};
      const std::size_t comm_read =
          std::fread(comm, 1, sizeof(comm) - 1, comm_file);
      std::fclose(comm_file);
      comm[comm_read] = '\0';
      for (char* cursor = comm; *cursor != '\0'; ++cursor) {
        if (*cursor == '\n') {
          *cursor = '\0';
          break;
        }
      }
      thread.name = comm;
    }
    thread.ticks = static_cast<std::uint64_t>(fields[10] + fields[11]);
    usage[tid] = std::move(thread);
  }
  closedir(tasks);
  return usage;
}

void SamplerLoop() noexcept {
  for (;;) {
    std::this_thread::sleep_for(std::chrono::milliseconds(kSamplerPeriodMs));
    std::map<int, ThreadUsage> sample = ReadThreadUsage();
    std::lock_guard<std::mutex> lock(g_mutex);
    g_thread_baseline = std::move(sample);
  }
}

void StartSamplerLocked() {
  if (g_sampler_started) {
    return;
  }
  g_sampler_started = true;
  std::thread(SamplerLoop).detach();
}

std::uint64_t ThresholdNanoseconds() noexcept {
  std::uint64_t threshold = g_threshold_ns.load(std::memory_order_relaxed);
  if (threshold != 0) {
    return threshold;
  }
  std::uint64_t milliseconds = kDefaultThresholdMs;
  if (const char* configured = std::getenv("MAPLEBLOX_STALL_MS")) {
    const unsigned long long parsed = std::strtoull(configured, nullptr, 10);
    if (parsed > 0 && parsed < 60000ULL) {
      milliseconds = parsed;
    }
  }
  threshold = milliseconds * kNanosecondsPerMillisecond;
  g_threshold_ns.store(threshold, std::memory_order_relaxed);
  return threshold;
}

std::string StateRoot() {
  const char* xdg = std::getenv("XDG_STATE_HOME");
  if (xdg != nullptr && xdg[0] != '\0') {
    return std::string(xdg) + "/mocktail";
  }
  const char* home = std::getenv("HOME");
  if (home == nullptr || home[0] == '\0') {
    return "/tmp/mocktail-state";
  }
  return std::string(home) + "/.local/state/mocktail";
}

const std::string& RecordPath() {
  if (!g_path_resolved) {
    g_record_path = StateRoot() + "/diagnostics/stalls.jsonl";
    g_path_resolved = true;
  }
  return g_record_path;
}

std::uint64_t ReadFirstNumber(const char* path) noexcept {
  FILE* file = std::fopen(path, "r");
  if (file == nullptr) {
    return 0;
  }
  char buffer[256] = {};
  const std::size_t read = std::fread(buffer, 1, sizeof(buffer) - 1, file);
  std::fclose(file);
  buffer[read] = '\0';
  for (std::size_t index = 0; index < read; ++index) {
    if (buffer[index] >= '0' && buffer[index] <= '9') {
      return std::strtoull(buffer + index, nullptr, 10);
    }
  }
  return 0;
}

std::uint64_t ReadStatusValue(const char* key) noexcept {
  FILE* file = std::fopen("/proc/self/status", "r");
  if (file == nullptr) {
    return 0;
  }
  char line[256] = {};
  const std::size_t key_length = std::strlen(key);
  std::uint64_t value = 0;
  while (std::fgets(line, sizeof(line), file) != nullptr) {
    if (std::strncmp(line, key, key_length) == 0) {
      value = std::strtoull(line + key_length, nullptr, 10);
      break;
    }
  }
  std::fclose(file);
  return value;
}

std::uint64_t ReadPackageTemperatureMillicelsius() noexcept {
  for (int zone = 0; zone < 16; ++zone) {
    char type_path[96] = {};
    std::snprintf(type_path, sizeof(type_path),
                  "/sys/class/thermal/thermal_zone%d/type", zone);
    FILE* type_file = std::fopen(type_path, "r");
    if (type_file == nullptr) {
      continue;
    }
    char type[64] = {};
    const std::size_t read = std::fread(type, 1, sizeof(type) - 1, type_file);
    std::fclose(type_file);
    type[read] = '\0';
    if (std::strstr(type, "x86_pkg_temp") == nullptr) {
      continue;
    }
    char temp_path[96] = {};
    std::snprintf(temp_path, sizeof(temp_path),
                  "/sys/class/thermal/thermal_zone%d/temp", zone);
    return ReadFirstNumber(temp_path);
  }
  return 0;
}

void AppendEscaped(std::string* out, const char* text) {
  for (const char* cursor = text; *cursor != '\0'; ++cursor) {
    const char value = *cursor;
    if (value == '"' || value == '\\') {
      out->push_back('\\');
      out->push_back(value);
    } else if (value == '\n') {
      out->append("\\n");
    } else if (static_cast<unsigned char>(value) >= 32) {
      out->push_back(value);
    }
  }
}

void RotateIfNeeded(const std::string& path) {
  std::error_code error;
  const auto size = std::filesystem::file_size(path, error);
  if (error || size < static_cast<std::uintmax_t>(kMaximumRecordBytes)) {
    return;
  }
  std::filesystem::rename(path, path + ".1", error);
}

void AppendFlow(std::string* line, std::uint64_t stall_ns) {
  line->append("\"flow\":[");
  const std::size_t first =
      g_event_count > kEventCapacity ? g_event_count - kEventCapacity : 0;
  bool first_entry = true;
  for (std::size_t index = first; index < g_event_count; ++index) {
    const GraphicsEvent& event = g_events[index % kEventCapacity];
    if (!first_entry) {
      line->push_back(',');
    }
    first_entry = false;
    const double offset_ms =
        (static_cast<double>(event.monotonic_ns) -
         static_cast<double>(stall_ns)) /
        1e6;
    char numbers[64] = {};
    std::snprintf(numbers, sizeof(numbers), "{\"at_ms\":%.1f,\"dur_ms\":%.1f",
                  offset_ms, event.duration_ms);
    line->append(numbers);
    line->append(",\"cat\":\"");
    AppendEscaped(line, event.category);
    line->append("\",\"detail\":\"");
    AppendEscaped(line, event.detail);
    line->append("\"}");
  }
  line->push_back(']');
}

// Per-thread CPU consumed since the last sample. Separates a guest that is
// computing during the freeze from one that is blocked on the driver.
void AppendThreads(std::string* line,
                   const std::map<int, ThreadUsage>& before) {
  const std::map<int, ThreadUsage> after = ReadThreadUsage();
  std::vector<std::pair<double, std::string>> deltas;
  for (const auto& [tid, usage] : after) {
    const auto found = before.find(tid);
    if (found == before.end() || usage.ticks < found->second.ticks) {
      continue;
    }
    const double milliseconds =
        static_cast<double>(usage.ticks - found->second.ticks) * 10.0;
    if (milliseconds >= 5.0) {
      deltas.emplace_back(milliseconds, usage.name);
    }
  }
  std::sort(deltas.begin(), deltas.end(),
            [](const auto& left, const auto& right) {
              return left.first > right.first;
            });
  line->append("\"threads\":[");
  const std::size_t limit =
      deltas.size() < kThreadReportCount ? deltas.size() : kThreadReportCount;
  for (std::size_t index = 0; index < limit; ++index) {
    if (index != 0) {
      line->push_back(',');
    }
    char numbers[64] = {};
    std::snprintf(numbers, sizeof(numbers), "{\"ms\":%.0f,\"name\":\"",
                  deltas[index].first);
    line->append(numbers);
    AppendEscaped(line, deltas[index].second.c_str());
    line->append("\"}");
  }
  line->push_back(']');
}

void AppendRecord(std::uint64_t gap_ns, std::int64_t frame) noexcept {
  const std::string& path = RecordPath();
  std::error_code error;
  std::filesystem::create_directories(
      std::filesystem::path(path).parent_path(), error);

  char timestamp[32] = {};
  const std::time_t now = std::time(nullptr);
  std::tm local = {};
  localtime_r(&now, &local);
  std::strftime(timestamp, sizeof(timestamp), "%Y-%m-%dT%H:%M:%S", &local);

  const double gap_ms = static_cast<double>(gap_ns) / 1e6;
  const std::uint64_t rss_kb = ReadStatusValue("VmRSS:");
  const std::uint64_t threads = ReadStatusValue("Threads:");
  const std::uint64_t frequency_khz =
      ReadFirstNumber("/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq");
  const std::uint64_t temperature_mc = ReadPackageTemperatureMillicelsius();

  // The baseline is copied before the writer lock: AppendThreads must not take
  // the same mutex again.
  std::map<int, ThreadUsage> baseline;
  {
    std::lock_guard<std::mutex> lock(g_mutex);
    baseline = g_thread_baseline;
  }

  std::lock_guard<std::mutex> lock(g_mutex);
  std::string line;
  line.reserve(2048);
  char header[256] = {};
  std::snprintf(header, sizeof(header),
                "{\"t\":\"%s\",\"gap_ms\":%.1f,\"frame\":%lld,\"rss_mb\":%llu,"
                "\"thread_count\":%llu,\"freq_mhz\":%llu,\"temp_c\":%.1f,",
                timestamp, gap_ms, static_cast<long long>(frame),
                static_cast<unsigned long long>(rss_kb / 1024U),
                static_cast<unsigned long long>(threads),
                static_cast<unsigned long long>(frequency_khz / 1000U),
                static_cast<double>(temperature_mc) / 1000.0);
  line.append(header);
  AppendThreads(&line, baseline);
  line.push_back(',');
  const std::uint64_t stall_ns = NowNanoseconds();
  AppendFlow(&line, stall_ns);
  line.append("}\n");

  RotateIfNeeded(path);
  FILE* file = std::fopen(path.c_str(), "a");
  if (file == nullptr) {
    return;
  }
  std::fwrite(line.data(), 1, line.size(), file);
  std::fclose(file);
}

}  // namespace

const char* StallRecordPath() noexcept { return RecordPath().c_str(); }

void RecordGraphicsEvent(const char* category, const char* detail,
                         double duration_ms) noexcept {
  if (category == nullptr) {
    return;
  }
  std::lock_guard<std::mutex> lock(g_mutex);
  GraphicsEvent& event = g_events[g_event_count % kEventCapacity];
  event.monotonic_ns = NowNanoseconds();
  event.duration_ms = duration_ms;
  std::snprintf(event.category, sizeof(event.category), "%s", category);
  std::snprintf(event.detail, sizeof(event.detail), "%s",
                detail == nullptr ? "" : detail);
  ++g_event_count;
}

void RecordFramePresented(std::uint64_t monotonic_ns,
                          std::int64_t frame) noexcept {
  {
    std::lock_guard<std::mutex> lock(g_mutex);
    StartSamplerLocked();
  }
  const std::uint64_t previous =
      g_last_present_ns.exchange(monotonic_ns, std::memory_order_relaxed);
  if (previous == 0 || monotonic_ns <= previous) {
    return;
  }
  const std::uint64_t gap_ns = monotonic_ns - previous;
  if (gap_ns < ThresholdNanoseconds()) {
    return;
  }
  std::fprintf(stderr, "  [stall] %.0f ms without a frame (frame %lld)\n",
               static_cast<double>(gap_ns) / 1e6,
               static_cast<long long>(frame));
  AppendRecord(gap_ns, frame);
}

}  // namespace runtime
}  // namespace mocktail