//! Mapleblox memory and performance policy.
//!
//! Pure functions that decide how much video memory the Android guest may
//! believe it owns. The C++ runtime keeps the ABI and the startup wiring; the
//! policy lives here so it can be unit tested without booting a Roblox client.

use std::process::Command;

const MIB: u64 = 1024 * 1024;
/// Below this the guest default is already a reasonable share of the host.
const MINIMUM_HOST_BYTES: u64 = 2048 * MIB;
const MINIMUM_BUDGET_BYTES: u64 = 256 * MIB;
/// Ceiling for the RAM heuristic. A board never uses it.
const MAXIMUM_HOST_BUDGET_BYTES: u64 = 1536 * MIB;
/// The desktop compositor and the driver keep a slice of the board.
const VIDEO_RESERVE_BYTES: u64 = 512 * MIB;
/// Roblox reads this value through an FInt, which is a signed 32-bit integer.
const FFLAG_CEILING_BYTES: u64 = i32::MAX as u64;

/// Texture budget for a host that reports no dedicated memory.
pub fn host_texture_budget(host_bytes: u64) -> u64 {
    if host_bytes < MINIMUM_HOST_BYTES {
        return 0;
    }
    (host_bytes / 8).clamp(MINIMUM_BUDGET_BYTES, MAXIMUM_HOST_BUDGET_BYTES)
}

/// Texture budget for a host with a known amount of dedicated video memory.
///
/// The board wins over the system RAM heuristic, and a reserve is kept for the
/// compositor and the driver. Without a board the heuristic takes over.
pub fn texture_budget(video_bytes: u64, host_bytes: u64) -> u64 {
    if video_bytes > VIDEO_RESERVE_BYTES {
        return (video_bytes - VIDEO_RESERVE_BYTES).max(MINIMUM_BUDGET_BYTES);
    }
    host_texture_budget(host_bytes)
}

/// The value that may be published through FIntRenderForceVideoMemorySize.
pub fn fflag_video_memory_bytes(budget_bytes: u64) -> u64 {
    budget_bytes.min(FFLAG_CEILING_BYTES)
}

/// Total dedicated video memory of the first board, or 0 when unknown.
pub fn detect_video_memory_bytes() -> u64 {
    let output = match Command::new("nvidia-smi")
        .args([
            "--query-gpu=memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return 0,
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let first = match text.lines().next() {
        Some(line) => line.trim(),
        None => return 0,
    };
    match first.parse::<u64>() {
        Ok(mebibytes) if mebibytes > 0 => mebibytes * MIB,
        _ => 0,
    }
}

#[no_mangle]
pub extern "C" fn mapleblox_texture_budget_bytes(video_bytes: u64, host_bytes: u64) -> u64 {
    texture_budget(video_bytes, host_bytes)
}

#[no_mangle]
pub extern "C" fn mapleblox_fflag_video_memory_bytes(budget_bytes: u64) -> u64 {
    fflag_video_memory_bytes(budget_bytes)
}

#[no_mangle]
pub extern "C" fn mapleblox_detect_video_memory_bytes() -> u64 {
    detect_video_memory_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_hosts_keep_the_guest_default() {
        assert_eq!(texture_budget(0, 0), 0);
        assert_eq!(texture_budget(0, 1024 * MIB), 0);
        assert_eq!(texture_budget(0, 2047 * MIB), 0);
    }

    #[test]
    fn the_host_heuristic_still_scales_and_clamps() {
        assert_eq!(texture_budget(0, 3800 * MIB), 475 * MIB);
        assert_eq!(texture_budget(0, 2048 * MIB), 256 * MIB);
        assert_eq!(texture_budget(0, 8192 * MIB), 1024 * MIB);
        assert_eq!(texture_budget(0, 65536 * MIB), 1536 * MIB);
    }

    #[test]
    fn the_board_wins_over_the_host_heuristic() {
        assert_eq!(texture_budget(4096 * MIB, 3800 * MIB), 3584 * MIB);
        assert_eq!(texture_budget(8192 * MIB, 65536 * MIB), 7680 * MIB);
    }

    #[test]
    fn a_tiny_board_falls_back_without_going_low() {
        // 520 MiB of board leaves 8 MiB, below the floor, so the floor applies.
        assert_eq!(texture_budget(520 * MIB, 8192 * MIB), 256 * MIB);
        // A 512 MiB board cannot cover the reserve, so the host decides.
        assert_eq!(texture_budget(512 * MIB, 8192 * MIB), 1024 * MIB);
    }

    #[test]
    fn the_fflag_never_overflows_a_signed_int() {
        assert_eq!(fflag_video_memory_bytes(3584 * MIB), FFLAG_CEILING_BYTES);
        assert_eq!(fflag_video_memory_bytes(1024 * MIB), 1024 * MIB);
        assert_eq!(fflag_video_memory_bytes(0), 0);
    }
}