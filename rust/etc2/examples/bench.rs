// Decode and pack throughput of the Rust port, for comparison with the C++
// reference.
//
//   cargo run --release --example bench
//
// Decodes a 1024x1024 ETC2 RGB image (65536 blocks) repeatedly and reports
// megatexels per second, both single-threaded and with one thread per core.
// Then packs the decoded texels to BC7 the same way, which is what the CPU
// packer spends its time on.

use std::time::Instant;

use mapleblox_etc2::{decode_image, decode_image_parallel, encode_bc7_block_rows, EtcFormat};

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

fn measure_pack(label: &str, threads: Option<usize>, rgba: &[u8], destination: &mut [u8]) {
    let source = rgba.to_vec();
    let bands = threads.unwrap_or(1);
    let blocks_high = ((HEIGHT as usize).div_ceil(4)) as u32;
    let rows_per_band = blocks_high.div_ceil(bands as u32);
    let run = |destination: &mut [u8]| {
        if let Some(threads) = threads {
            let blocks_wide = WIDTH.div_ceil(4) as usize;
            let band_bytes = blocks_wide * rows_per_band as usize * 16;
            std::thread::scope(|scope| {
                let mut chunks = destination.chunks_mut(band_bytes).enumerate();
                let mut bands = Vec::new();
                let mut first = 0u32;
                while first < blocks_high {
                    let count = rows_per_band.min(blocks_high - first);
                    let chunk = chunks.next().expect("band").1;
                    let source = &source;
                    bands.push(scope.spawn(move || {
                        encode_bc7_block_rows(
                            source,
                            WIDTH,
                            HEIGHT,
                            WIDTH as usize * 4,
                            first,
                            count,
                            chunk,
                        )
                        .expect("pack")
                    }));
                    first += count;
                }
                for band in bands {
                    band.join().expect("thread");
                }
            });
            let _ = threads;
        } else {
            encode_bc7_block_rows(
                &source,
                WIDTH,
                HEIGHT,
                WIDTH as usize * 4,
                0,
                blocks_high,
                destination,
            )
            .expect("pack");
        }
    };
    run(destination);
    let start = Instant::now();
    for _ in 0..ROUNDS {
        run(destination);
    }
    let elapsed = start.elapsed();
    let texels = (WIDTH as f64) * (HEIGHT as f64) * ROUNDS as f64;
    let per_round = elapsed.as_secs_f64() / ROUNDS as f64;
    println!(
        "{label:<8} {:.2} ms per 1024x1024 image, {:.1} Mtexel/s, {:.1} MiB/s packed",
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

    let mut packed = vec![0u8; blocks * 16];
    measure_pack("bc7", None, &destination, &mut packed);
    measure_pack("bc7-mt", Some(cores), &destination, &mut packed);
}