// Decode throughput of the Rust port, for comparison with the C++ reference.
//
//   cargo run --release --example bench
//
// Decodes a 1024x1024 ETC2 RGB image (65536 blocks) repeatedly and reports
// megatexels per second, both single-threaded and with one thread per core.

use std::time::Instant;

use mapleblox_etc2::{decode_image, decode_image_parallel, EtcFormat};

const WIDTH: u32 = 1024;
const HEIGHT: u32 = 1024;
const ROUNDS: usize = 40;

fn measure(label: &str, parallel: Option<usize>, source: &[u8], destination: &mut [u8]) {
    let run = |destination: &mut [u8]| match parallel {
        None => decode_image(EtcFormat::Rgb8, source, WIDTH, HEIGHT, destination),
        Some(threads) => {
            decode_image_parallel(EtcFormat::Rgb8, source, WIDTH, HEIGHT, destination, threads)
        }
    };
    run(destination).expect("decode");
    let start = Instant::now();
    for _ in 0..ROUNDS {
        run(destination).expect("decode");
    }
    let elapsed = start.elapsed();
    let texels = (WIDTH as f64) * (HEIGHT as f64) * ROUNDS as f64;
    let per_round = elapsed.as_secs_f64() / ROUNDS as f64;
    println!(
        "{label:<8} {:.2} ms per 1024x1024 image, {:.1} Mtexel/s, {:.1} MiB/s decoded",
        per_round * 1000.0,
        texels / elapsed.as_secs_f64() / 1e6,
        (texels * 4.0) / elapsed.as_secs_f64() / (1024.0 * 1024.0)
    );
}

fn main() {
    let cores = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(4);
    let blocks = ((WIDTH as usize).div_ceil(4)) * ((HEIGHT as usize).div_ceil(4));
    let mut source = vec![0u8; blocks * 8];
    let mut state = 0x1234_5678u32;
    for byte in source.iter_mut() {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *byte = (state >> 24) as u8;
    }
    let mut destination = vec![0u8; (WIDTH as usize) * (HEIGHT as usize) * 4];

    measure("rust", None, &source, &mut destination);
    measure("rust-mt", Some(cores), &source, &mut destination);
}