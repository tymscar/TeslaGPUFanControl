# License: Apache-2.0

The project is licensed under Apache-2.0. The original PLAN.md draft specified GPL-2.0; we re-evaluated and chose Apache-2.0 for three reasons specific to this project.

1. **NVIDIA library compatibility.** The daemon `dlopen`s `libnvidia-ml.so.1`, which is proprietary. The FSF interprets dynamic linking as creating a derivative work, which makes "GPL + proprietary lib via dlopen" a 15-year-old unresolved gray area. Apache-2.0 sidesteps the question entirely.
2. **Rust ecosystem convention.** Every direct dependency (`nvml-wrapper`, `nix`, `clap`, `tracing`, `signal-hook`, `thiserror`) is Apache-2.0/MIT dual. A GPL daemon would be the only non-permissive node in the dependency graph, which surprises contributors.
3. **Copyleft offers no concrete protection in this scope.** A hypothetical closed-source fork of a system daemon you install on your own box does not harm users in the way copyleft is designed to prevent.

Apache-2.0 over MIT specifically for the explicit patent grant, which has zero downside and a small upside for hardware-control tooling.

## Consequences

- `LICENSE` file in repo root contains the Apache-2.0 text verbatim.
- `Cargo.toml` declares `license = "Apache-2.0"`.
- Source files include the standard Apache-2.0 SPDX header (`// SPDX-License-Identifier: Apache-2.0`) at the top of each `.rs` file.
- Contributors are not required to sign a CLA; the Apache-2.0 license itself grants the necessary rights for incorporation.
