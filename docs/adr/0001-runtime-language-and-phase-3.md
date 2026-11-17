# ADR 0001: keep the runtime in C++, limit Rust to policy and tooling

- Status: accepted
- Date: 2026-11-16
- Deciders: Mapleblox maintainers

## Context

The fork was asked to consider migrating the runtime to Rust for speed, and to
replace the ETC2 texture transcoder with a Rust implementation.

The runtime is 134,964 lines of C and C++ across `src/` and `include/`, split
as runtime 36,674, graphics 21,062, update 10,911, jnivm 8,654, ui 7,851,
legacy 7,516, compat 7,253, window 6,828 and audio 5,816. It implements a
binary-compatible Android ABI: a bionic libc shim, an ELF linker, a JNI VM and
a Vulkan interposition layer.

A 20-second `perf record` of a running client, sorted by DSO, measured where
the CPU actually goes:

| Share | Module |
| --- | --- |
| 61.83% | `libroblox.so`, the guest engine |
| 21.82% | unresolved, mostly kernel and unstripped guest code |
| 6.61% | `libc.so.6` |
| 3.42% | `libSDL3.so` |
| 3.07% | the Mapleblox runtime binary |
| 2.74% | `vdso` |

Inside the runtime, the hottest symbol is
`mocktail_bionic_pthread_getspecific` at 2.10% of total CPU, followed by
`mocktail_bionic_sysconf` at 0.05%. The ETC2 path reported 41 KiB of textures
on the GPU and 0 KiB of blocks kept on the CPU during the same run.

## Decision

1. The runtime stays in C++. A full Rust port would have to reproduce a
   byte-exact ABI for the shim, linker, JNI VM and Vulkan interposition, and
   the best case is optimising the 3.07% the runtime owns. Rust does not make
   equivalent algorithms run faster.
2. The ETC2 transcoder stays on the GPU. Its CPU side is below measurement
   noise, so a Rust CPU transcoder would be slower, not faster.
3. Rust is used where it is measured or structural value: the memory and
   performance policy crate (`rust/policy`, linked over a C ABI) and the
   diagnostics tool (`rust/tool`).

## Consequences

- The runtime keeps its ABI guarantees and the fork stays mergeable with
  upstream.
- Performance work targets the guest, the graphics backend and the host
  policy, not the language of the shim.
- Future hot paths may move to Rust only with a measurement that shows the
  runtime's share of CPU growing and a prototype that wins under `perf`.