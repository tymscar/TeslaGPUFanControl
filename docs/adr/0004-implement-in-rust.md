# Implement the daemon in Rust

The daemon is implemented in Rust, using the `nvml-wrapper` crate for NVML access and `libloading` (transitively, via `nvml-wrapper-sys`) for runtime `dlopen` of `libnvidia-ml.so.1`.

## Why Rust

Two language-level properties map directly to ADRs already on file and become *compiler-enforced* invariants rather than convention:

- **`Result<T, E>` enforces ADR-0001's "no fault may be silently swallowed."** Every fallible call returns a `Result`; ignoring it requires an explicit `let _ = ...` that is grep-able and reviewable. In Go or C the equivalent guarantee is at best linter-enforced.
- **Newtypes (`Celsius(i16)`, `Pct(u8)`, `Pwm(u8)`) prevent unit-confusion bugs at compile time.** The conversion `pwm = (pct * 255) / 100` is a known bug magnet in this domain; making the three units distinct types removes that surface entirely.

## Why not Go

Go was the obvious alternative and would have given easier OSS contributor onboarding. It was rejected on a single concrete deployment issue: `NVIDIA/go-nvml` requires `CGO_ENABLED=1`. CGO compromises the single-static-binary deploy story this project depends on:

- CGO + musl-static is painful and underdocumented.
- CGO + glibc-static has unresolved runtime issues (`NVIDIA/go-nvml` issue #62).
- The pure-Go alternative is `purego` (self-described as beta) with no maintained NVML binding on top of it — writing our own FFI on a beta library is the worst of both worlds.

Rust's `nvml-wrapper` `dlopen`s `libnvidia-ml.so.1` at runtime via `libloading` and produces a clean static binary against `x86_64-unknown-linux-musl`. This was the deciding factor.

## Why not Python

Rejected on deployment shape. Interpreter dependency on remote unattended boxes, plus signal-handler quirks during long C-extension calls (NVML), plus no compile-time safety on a root-privileged daemon. Three small problems that compound.

## Why not C (the original choice)

Re-evaluated and rejected. The PLAN.md justification ("compiled binary, dlopen, sysfs writes, signal handling") is met by Rust without the memory-safety surface area of a hand-written C daemon running as root with `/dev/watchdog` open.

## Consequences

- Build system is Cargo; release builds target `x86_64-unknown-linux-musl` for the static-binary property.
- Project layout is the standard Cargo crate layout (`src/main.rs`, `src/<module>.rs`, `Cargo.toml`), not the C-style `src/*.c + src/*.h` originally planned.
- NVML access goes through `nvml-wrapper`'s safe API; we do not roll our own FFI shim. (We could, but `nvml-wrapper` already does this correctly with proper error mapping.)
- `libnvidia-ml.so.1` is loaded at runtime, never linked. Same model as `nvidia-smi`. The proprietary NVIDIA driver provides this library; no other system dependency on NVIDIA tooling.
