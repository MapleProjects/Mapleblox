//! Mapleblox diagnostics.
//!
//! Three commands keep the runtime honest after a change:
//!   budget  the number the runtime will publish as video memory
//!   check   validate the FFlag override file before it breaks a launch
//!   bench   live CPU cost of the running client, without perf

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, Instant};

const MIB: u64 = 1024 * 1024;
const TICK_SECONDS: f64 = 0.01; // USER_HZ is 100 on Linux.

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("budget") => budget(),
        Some("check") => check(args.get(1).map(String::as_str)),
        Some("bench") => {
            let pid = args.get(1).and_then(|value| value.parse::<i32>().ok());
            let seconds = args
                .get(2)
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(10);
            bench(pid, seconds)
        }
        Some("help") | Some("--help") | Some("-h") | None => {
            println!("mapleblox-tool budget | check [file] | bench [pid] [seconds]");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("mapleblox-tool: unknown command '{other}'");
            ExitCode::FAILURE
        }
    }
}

fn host_memory_bytes() -> u64 {
    let Ok(text) = fs::read_to_string("/proc/meminfo") else {
        return 0;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kibibytes: u64 = rest
                .split_whitespace()
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            return kibibytes * 1024;
        }
    }
    0
}

fn budget() -> ExitCode {
    let video = mapleblox_policy::detect_video_memory_bytes();
    let host = host_memory_bytes();
    let texture = mapleblox_policy::texture_budget(video, host);
    let published = mapleblox_policy::fflag_video_memory_bytes(texture);
    println!("video memory   {:>8} MiB", video / MIB);
    println!("host memory    {:>8} MiB", host / MIB);
    println!("texture budget {:>8} MiB", texture / MIB);
    println!("published FInt {:>8} MiB ({} bytes)", published / MIB, published);
    if published == i32::MAX as u64 {
        println!("note: the FFlag is a signed 32-bit integer, so 2 GiB is the ceiling");
    }
    ExitCode::SUCCESS
}

fn default_fflags_path() -> PathBuf {
    if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        if !config_home.is_empty() {
            return Path::new(&config_home).join("mocktail/fflags.json");
        }
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".config/mocktail/fflags.json")
}

fn check(path: Option<&str>) -> ExitCode {
    let path = path.map(PathBuf::from).unwrap_or_else(default_fflags_path);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("mapleblox-tool: cannot read {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("mapleblox-tool: {} is not valid JSON: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let Some(object) = value.as_object() else {
        eprintln!("mapleblox-tool: {} must contain a JSON object", path.display());
        return ExitCode::FAILURE;
    };
    let mut problems = 0;
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (key, value) in object {
        let kind = if key.starts_with("DFFlag") {
            "DFFlag"
        } else if key.starts_with("FFlag") {
            "FFlag"
        } else if key.starts_with("DFInt") {
            "DFInt"
        } else if key.starts_with("FInt") {
            "FInt"
        } else if key.starts_with("DFString") {
            "DFString"
        } else if key.starts_with("FString") {
            "FString"
        } else {
            eprintln!("  ! {key}: unknown flag family");
            problems += 1;
            continue;
        };
        *counts.entry(kind).or_default() += 1;
        let valid = match kind {
            "FFlag" | "DFFlag" => value.is_boolean(),
            "FInt" | "DFInt" => value
                .as_i64()
                .map(|number| (i32::MIN as i64..=i32::MAX as i64).contains(&number))
                .unwrap_or(false),
            _ => value.is_string(),
        };
        if !valid {
            eprintln!("  ! {key}: wrong type for {kind}");
            problems += 1;
        }
    }
    let summary: Vec<String> = counts
        .iter()
        .map(|(kind, count)| format!("{count} {kind}"))
        .collect();
    println!("{}: {} flags ({})", path.display(), object.len(), summary.join(", "));
    if problems == 0 {
        println!("OK");
        ExitCode::SUCCESS
    } else {
        eprintln!("{problems} problem(s)");
        ExitCode::FAILURE
    }
}

struct CpuSample {
    ticks: u64,
    threads: usize,
}

fn read_cpu(pid: i32) -> Option<CpuSample> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = text.rfind(')')?;
    let fields: Vec<&str> = text[close + 2..].split_whitespace().collect();
    // After the command field, state is field 3, so utime is index 11 and
    // stime index 12 in the full line; here the slice starts at field 3.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let threads = fs::read_dir(format!("/proc/{pid}/task"))
        .ok()?
        .count();
    Some(CpuSample {
        ticks: utime + stime,
        threads,
    })
}

fn find_client() -> Option<i32> {
    let mut best: Option<(i32, usize)> = None;
    for entry in fs::read_dir("/proc").ok()? {
        let Ok(entry) = entry else { continue };
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(comm) = fs::read_to_string(format!("/proc/{pid}/comm")) else {
            continue;
        };
        let comm = comm.trim();
        if comm != "Main" && comm != "mocktail" {
            continue;
        }
        let threads = fs::read_dir(format!("/proc/{pid}/task"))
            .map(|entries| entries.count())
            .unwrap_or(0);
        if best.map(|(_, best_threads)| threads > best_threads).unwrap_or(true) {
            best = Some((pid, threads));
        }
    }
    best.map(|(pid, _)| pid)
}

fn bench(pid: Option<i32>, seconds: u64) -> ExitCode {
    let Some(pid) = pid.or_else(find_client) else {
        eprintln!("mapleblox-tool: no Roblox client running");
        return ExitCode::FAILURE;
    };
    let Some(start) = read_cpu(pid) else {
        eprintln!("mapleblox-tool: cannot read /proc/{pid}");
        return ExitCode::FAILURE;
    };
    println!("sampling pid {pid} for {seconds}s");
    let begin = Instant::now();
    sleep(Duration::from_secs(seconds.max(1)));
    let Some(end) = read_cpu(pid) else {
        eprintln!("mapleblox-tool: process exited");
        return ExitCode::FAILURE;
    };
    let wall = begin.elapsed().as_secs_f64();
    let cpu_seconds = (end.ticks.saturating_sub(start.ticks)) as f64 * TICK_SECONDS;
    let cores = cpu_seconds / wall;
    println!("threads        {}", end.threads);
    println!("cpu seconds    {cpu_seconds:.2}");
    println!("cpu per wall   {cores:.2} cores");
    println!(
        "headroom       {:.0}% of 16 logical CPUs",
        (cores / 16.0) * 100.0
    );
    ExitCode::SUCCESS
}