# Dual Rust/C API across shared libraries

A **reference implementation of a design pattern**: how to ship a family of Rust libraries that each expose both a first-class Rust API and an ergonomic C API, where a type defined by one library is used by another, and the libraries may be versioned and upgraded independently.

**Start here:** run `./run.sh`, then read [ARCHITECTURE.md](ARCHITECTURE.md) — the pattern, its rationale, the rules that make it sound, and a checklist for applying it to a new component.

## The problem the pattern solves

Rust's defaults work against this shape. Crates are statically linked and `#[repr(Rust)]` layouts are unstable, so when two `cdylib`s both depend on a type's crate, **each embeds its own copy of the code and its own frozen idea of the layout**. The copies bloat the artifacts and silently diverge as versions drift — a type created by one library and read by another is then two different types that happen to share a name.

The pattern's answer is a four-crate split per component plus a feature-selected backend, so that:

- each type's implementation has exactly **one home**, with no duplication across `.so` files;
- pure-Rust consumers still get a **zero-cost, statically linked** first-class Rust API;
- every component exposes a **hand-designed C API** — opaque handles, ownership rules, error codes — rather than a generated one;
- shared libraries **drift in version independently**, as long as an explicitly frozen, append-only ABI contract holds.

## Why it is a reference rather than a write-up

Every structural claim here is executable. The repository is the proof, not an illustration of it:

- the one-implementation-home property is checked with `nm`, and the **naive duplicated build is reproducible on demand** for contrast;
- version drift is proven by rebuilding one library with changed internals and showing consumer binaries **byte-identical and still passing**;
- zero-copy is proven by **pointer identity** on every surface — C, Python, Swift, and an independent `mmap` of an exported DMA-BUF fd on real hardware;
- the aliasing model is checked under **Miri, in both Stacked and Tree Borrows**, including a deliberately-failing diagnostic that documents why a contract exists;
- the C header is regenerated and **re-proved on every run** (pedantic C11 + C++17 compile, derived constant check, `_Static_assert` layout goldens).

Where a design turned out to be wrong, or an idea did not survive contact with the platform, [ARCHITECTURE.md](ARCHITECTURE.md) records that rather than the tidied-up version — those notes are usually the useful part.

## Applying it to your own component

The pattern is component-shaped, not project-shaped: see [Replicating the pattern for a new component X](ARCHITECTURE.md#replicating-the-pattern-for-a-new-component-x) for the step-by-step. `A` and `B` in this repo are deliberately trivial in behaviour — the point is the boundaries between them, so the mechanics stay legible.

## Prerequisites

- Rust (stable) + a C compiler. Everything in "core" below builds with just that.
- Optional per surface: `cbindgen` (header generation), Python 3 (a venv is created automatically; numpy is installed into it), `cargo-zigbuild` + `zig` (Linux/Windows cross-builds), Android NDK (Android builds and the PE/ELF inspection tools), Xcode (Swift test), .NET 8 SDK (C# test), ssh access to an `imx95-pro` board (on-target DMA-BUF test).
- The BoltFFI crates (`ab-bolt`, `a-mod`, `b-mod`) depend on a **sibling fork checkout via a machine-local absolute path** (`~/Software/Mobile/boltffi`, branch `edgefirst`) and require its CLI installed from the *same commit* (`cargo install --path .../boltffi_cli`). This is deliberate (CLI/crate commit lock) but not portable as committed — adjust the path in those three `Cargo.toml`s on another machine.

## Crate map

| Crate | Role |
| ------- | ------ |
| **Core pattern** | |
| `crates/a` | First-class Rust API of A; backend feature switch: `static` (implementation compiled in), `dynamic` (wrapper over liba's C ABI), `vtable` (dispatch through a runtime-installed function table) |
| `crates/a-abi` | Layout-only contracts: `#[repr(C)]` structs, status codes, callback types, capsule/vtable records — append-only evolution rules |
| `crates/a-ffi` | Declarations-only `extern "C"` bindings to liba (the Rust "header") |
| `crates/a-nostd-probe` | cdylib: a `no_std` size probe (not part of the architecture) — measures the floor a Rust cdylib can reach without libstd |
| `crates/a-rt` | rlib: quiet panic hook + thread-local last-error, dependency-free so every cdylib (including ones that must not embed the implementation) can install its own |
| `crates/a-cshim` | rlib: the C-ABI logic (bodies + panic shield + last-error) over the static backend, shared by `a-capi` and the self-contained `a-py` |
| `crates/a-capi` | cdylib → **liba.so**: the exported symbol surface and contract docs (cbindgen's source); bodies forward to `a-cshim` |
| `crates/b` | First-class Rust API of B, written once against `a` — backend-agnostic |
| `crates/b-capi` | cdylib → **libb.so**: consumes A through liba's C ABI (default); `--no-default-features --features static` builds the naive self-contained contrast case |
| `crates/app` | Pure-Rust static binary: one copy of A, no shared-library dependency |
| **Python** | |
| `crates/a-py` | PyO3 module `a`: pyclasses, PyCapsule `_C_API`, buffer protocol, asyncio |
| `crates/b-py` | PyO3 module `b`: consumes `a.A` via the capsule; abi3 |
| **Mobile (BoltFFI)** | |
| `crates/ab-bolt` | One Swift/Kotlin module over A+B (static backend on Apple, dynamic on Android) |
| **.NET (BoltFFI)** | |
| `crates/a-mod`, `crates/b-mod` | TWO independent C# modules sharing A via raw u64 handles over liba |

## Scripts

| Script | What it does |
| -------- | -------------- |
| `run.sh` | Core build + C harness + Rust app + header generation + ABI conformance — **the entry point** |
| `drift.sh` | Rebuild ONLY liba.so with changed internals; prove consumers pass untouched |
| `build-py.sh` | Python modules, both deployment variants (shared-liba and capsule-vtable), full test battery |
| `build-wheels.sh` | maturin wheels for the self-contained variant, installed into a fresh venv and re-tested from there |
| `gen-header.sh` | cbindgen header + constants/layout/pedantic C11+C++17 assertions |
| `size.sh` | The size ladder: four rungs measured, each rung's cost demonstrated |
| `package.sh` | cargo-c release packaging: versioned install, pkg-config, and a consumer built against the staged tree |
| `check-abi.sh` | Every `a-ffi` declaration must be exported by liba (name-level) |
| `miri.sh` | Scenario battery under Miri, both Stacked and Tree Borrows, plus the capsule aliasing diagnostic |
| `cross-test.sh` | zigbuild aarch64-linux + deploy/run the harness on the imx95-pro board (DMA-BUF) |
| (in `crates/ab-bolt`) `boltffi build` + `swiftc swifttest/Test.swift` | Swift/Kotlin module build + Swift validation (see ARCHITECTURE.md Mobile section) |
| `csharp-test.sh` | Two-module C# validation on the host |
| `cross-win.sh` | Windows win-x64 + win-arm64 DLLs (zig), filtered import libraries, PE verification |
| `win-import.sh` | Filtered Windows import library from the canonical a-ffi declarations |

## Test programs (executable documentation)

| Program | Proves |
| --------- | -------- |
| `harness/main.c` | The C contract end-to-end, compiled against the shipped `include/liba.h` |
| `crates/app/src/main.rs` | The pure-Rust path (static linking, futures with a 20-line executor) |
| `py/test_ab.py` | Python: capsule sharing, buffer protocol/PEP 3118, asyncio, numpy zero-copy |
| `crates/ab-bolt/swifttest/Test.swift` | Swift: classes, throws, async/await, AsyncStream, IOSurface zero-copy |
| `cstest/Program.cs` | C#: two modules, raw-handle sharing, Tasks, typed exceptions |

## Validated results (qualitative; commands in ARCHITECTURE.md)

- One implementation home: consumers carry only undefined `a_*` imports (`nm -u`), and the naive duplication case is reproducible on demand: `cargo build --manifest-path crates/b-capi/Cargo.toml --no-default-features --features static`.
- Version drift: liba.so rebuilt with changed internals; every consumer binary byte-identical and passing — C, Python, and the frozen-window fast path alike.
- Zero-copy by pointer identity, on every surface: C (`b_data_ptr`), Python (`__array_interface__`), Swift (`IOSurfaceLookup`), plus independent mmap of exported DMA-BUF fds on the imx95-pro board.
- Async/streaming/teardown, leak-checked (`leaks --atExit`: zero), with destroy-is-full-teardown semantics.
