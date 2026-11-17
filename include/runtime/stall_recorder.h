#ifndef MOCKTAIL_RUNTIME_STALL_RECORDER_H_
#define MOCKTAIL_RUNTIME_STALL_RECORDER_H_

#include <cstdint>

namespace mocktail {
namespace runtime {

// Mapleblox diagnostics: the presenter calls this on every presented frame.
// A gap larger than the configured threshold is written as one JSON line with
// the host state and the recent runtime events, so a freeze can be explained
// after the fact. Threshold: MAPLEBLOX_STALL_MS, default 250.
void RecordFramePresented(std::uint64_t monotonic_ns, std::int64_t frame) noexcept;

// Records one step of the graphics flow. `duration_ms` is 0 for a marker and
// the elapsed time for a completed operation. The last events are attached to
// the next stall record.
void RecordGraphicsEvent(const char* category, const char* detail,
                         double duration_ms) noexcept;

// Path of the JSONL file, for the diagnostics tool and for the log.
const char* StallRecordPath() noexcept;

}  // namespace runtime
}  // namespace mocktail

#endif  // MOCKTAIL_RUNTIME_STALL_RECORDER_H_