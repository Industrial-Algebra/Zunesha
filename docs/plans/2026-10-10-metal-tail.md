# Plan Contract — Metal Tail: Examples, CI macOS Job, Docs Tables

- **Unit:** metal-tail
- **Depends on:** `feat/metal-backend` HEAD (Metal backend complete: 31 tests green)
- **Followed by:** ADR 0004 (authored in parallel — do NOT touch `docs/adr/`)
- **Branch:** `feat/metal-backend` (stay on it; no git commands)

## Files

- CREATE `examples/device-info-metal.rs`
- CREATE `examples/buffer-roundtrip-metal.rs`
- EDIT `Cargo.toml` — append two `[[example]]` blocks
- EDIT `.github/workflows/ci.yml` — insert the `test-macos` job
- EDIT `docs/architecture.md` — four exact replacements
- EDIT `docs/ROADMAP.md` — three exact replacements
- EDIT `book/src/guide/installation.md` — two exact replacements
- EDIT `book/src/design/architecture.md` — same four replacements as `docs/architecture.md` (the book mirrors the doc; apply each replacement wherever the old text appears)
- EDIT `book/src/getting-started.md` — three exact replacements
- EDIT `book/src/examples/device-info.md` — append command block
- EDIT `book/src/examples/buffer-roundtrip.md` — append command block

## Contracts

### `examples/device-info-metal.rs` (full content)

```rust
// Copyright (C) 2026 Industrial Algebra
// SPDX-License-Identifier: Apache-2.0

// Mirrors the cfg of `zunesha::metal` (macOS + metal feature).
#![cfg(all(feature = "metal", target_os = "macos"))]

//! Print what the Metal substrate resolved on this Mac: the ADR-0004
//! queue shape, limits, memory strategy, and effective buffer placement.
//!
//! ```sh
//! cargo run --features metal --example device-info-metal
//! ```

use std::process::ExitCode;

use zunesha::{Device, InitRequest};

fn main() -> ExitCode {
    let mut request = InitRequest::prefer_graphics();
    if let Ok(hint) = std::env::var("ZUNESHA_TEST_DEVICE") {
        if !hint.is_empty() {
            request = request.with_device_hint(hint);
        }
    }
    let device = match zunesha::init_with(request) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "device-info-metal: no Metal device ({e}) — nothing to report on this host"
            );
            return ExitCode::SUCCESS;
        }
    };

    let queues = device.queues();
    println!("queues (ADR 0004 reinterpretation):");
    println!("  compute : the owned MTLCommandQueue handle");
    if queues.graphics.is_some() {
        println!("  graphics: Some — the same MTLCommandQueue (every Metal device renders)");
    } else {
        println!("  graphics: absent");
    }
    if queues.transfer.is_some() {
        println!("  transfer: Some");
    } else {
        println!("  transfer: None (blits ride the same queue)");
    }

    println!(
        "limits: min_storage_buffer_offset_alignment = {}",
        device.limits().min_storage_buffer_offset_alignment
    );
    println!("memory strategy requested: {:?}", device.memory_strategy());
    println!(
        "effective buffer placement: {:?}",
        device.buffer_placement()
    );
    ExitCode::SUCCESS
}
```

### `examples/buffer-roundtrip-metal.rs` (full content)

```rust
// Copyright (C) 2026 Industrial Algebra
// SPDX-License-Identifier: Apache-2.0

// Mirrors the cfg of `zunesha::metal` (macOS + metal feature).
#![cfg(all(feature = "metal", target_os = "macos"))]

//! Round-trip data through buffers on both storage modes — the Metal
//! analogue of `examples/buffer-roundtrip.rs`: Shared (unified) writes
//! and reads the direct mapping; Private + staging exercises the blit
//! path in both directions. Transparent to the consumer.
//!
//! ```sh
//! cargo run --features metal --example buffer-roundtrip-metal
//! ```

use std::process::ExitCode;

use zunesha::{Device, MemoryStrategy};

fn main() -> ExitCode {
    // A pattern whose corruption is easy to spot by eye.
    let data: Vec<u8> = (0..64_u8)
        .map(|i| i.wrapping_mul(7).wrapping_add(3))
        .collect();

    for strategy in [MemoryStrategy::Unified, MemoryStrategy::DeviceLocal] {
        let device = match zunesha::init_with_strategy(strategy) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "buffer-roundtrip-metal: no Metal device ({e}) — skipping {strategy:?}"
                );
                return ExitCode::SUCCESS;
            }
        };
        let buffer = match device.create_buffer(&data) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("buffer-roundtrip-metal: create_buffer({strategy:?}) failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        let read: Vec<u8> = match device.read_buffer(&buffer) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("buffer-roundtrip-metal: read_buffer({strategy:?}) failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        // The read may be padded to the buffer's aligned size; the prefix
        // must match exactly.
        if read.len() < data.len() || &read[..data.len()] != data.as_slice() {
            eprintln!(
                "buffer-roundtrip-metal: {strategy:?} verification FAILED — first bytes {:?}",
                &read[..data.len().min(read.len())]
            );
            return ExitCode::FAILURE;
        }
        println!(
            "buffer-roundtrip-metal: {:?} ✓ (placement {:?}, {} bytes verified)",
            strategy,
            device.buffer_placement(),
            data.len()
        );
    }
    ExitCode::SUCCESS
}
```

### `Cargo.toml` — append after the existing `buffer-roundtrip` `[[example]]` block

```toml

[[example]]
name = "device-info-metal"
required-features = ["metal"]

[[example]]
name = "buffer-roundtrip-metal"
required-features = ["metal"]
```

### `.github/workflows/ci.yml` — insert this job between the `test` job and the `doc` job (same indentation as `test:` / `doc:` — two spaces)

```yaml
  test-macos:
    name: Test (macOS Apple Silicon, self-hosted)
    runs-on: [self-hosted, macOS, ARM64]
    timeout-minutes: 45
    # Public repo + self-hosted runner: same-repo heads only (agent
    # branches), never fork PRs. See Holmberg docs/ci-runners.md.
    if: |
      github.event.action != 'labeled' &&
      (github.event_name != 'pull_request' || github.event.pull_request.head.repo.full_name == github.repository)
    env:
      # The dedicated hardware job must never pass without touching a real
      # Metal device: device-init failures hard-fail instead of skipping.
      ZUNESHA_REQUIRE_METAL: "1"
    steps:
      - uses: actions/checkout@v4
      # Self-hosted launchd agents inherit a bare PATH with no nix
      # profiles — same fleet recipe as Borsalino's macOS job (see
      # Holmberg docs/ci-runners.md for the full rationale).
      - name: Fleet PATH (nix profiles)
        run: |
          echo "$HOME/.nix-profile/bin:/run/current-system/sw/bin" >> "$GITHUB_PATH"
          echo "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=/usr/bin/cc" >> "$GITHUB_ENV"
      - uses: dtolnay/rust-toolchain@nightly
      - uses: Swatinem/rust-cache@v2
      - name: Run tests (metal)
        run: cargo test --features metal
```

### `docs/architecture.md` (and the mirrored `book/src/design/architecture.md`) — exact replacements

**R1** — old:

```
below it is a backend (today: Vulkan via raw `ash`).
```

new:

```
below it is a backend (Vulkan via raw `ash` on Linux/Windows, Metal via raw
`objc` on macOS).
```

**R2** — old (inside the layer diagram):

```
                ┌────────────┴────────────┐
                │   Vulkan (ash, raw FFI) │   ← backends (Metal planned)
                └─────────────────────────┘
```

new:

```
                ┌────────────┴────────────┐
                │  Vulkan (ash) │ Metal (objc)  │   ← backends
                └─────────────────────────┘
```

**R3** — old:

```
- ⏳ Metal backend (raw `objc`) — planned, mirroring Borsalino's backend split.
```

new:

```
- ✅ Metal backend (`src/metal.rs`): ADR-0004 queue reinterpretation (the
  owned MTLCommandQueue services compute + graphics; transfer is `None`),
  Shared and Private + staging storage modes, escape hatches (`raw_device`,
  `raw_buffer`, the queue handle). Verified on Apple M5 Max.
```

**R4** — old:

```
- ⏳ Borsalino refactor — a separate session consumes Zunesha as its device
  layer (handoff recorded in agent memory).
```

new:

```
- 🟨 Borsalino migration — the Vulkan path already consumes Zunesha 0.1.1
  from crates.io (Borsalino develop, staged plan); its Metal path migrates
  onto this backend next (the escape hatches are its Phase 3 seam).
```

### `docs/ROADMAP.md` — exact replacements

**R1** — old:

```
## Pending — committed sequence

- [ ] **Borsalino migration** — Borsalino consumes Zunesha, deletes its own
```

new:

```
## Pending — committed sequence

- [x] **Metal backend** (`objc`, macOS) — landed on `feat/metal-backend`:
      ADR-0004 queue reinterpretation, Shared + Private/staging storage
      modes, escape hatches, `ZUNESHA_REQUIRE_METAL` CI. Verified on
      Apple M5 Max; ships with the next release. Unblocks Borsalino-Metal's
      staged migration.
- [ ] **Borsalino migration** — Borsalino consumes Zunesha, deletes its own
```

**R2** — old:

```
- [ ] **Metal backend** (`objc`, macOS) — substrate-first sequencing applies:
      nothing consumes it until Goldenweek-Metal exists.
- [ ] **Cross-queue synchronization** (candidate ADR 0004) — queue-family
```

new:

```
- [ ] **Cross-queue synchronization** (candidate ADR 0005) — queue-family
```

**R3** — old:

```
> Not scheduled. Each lands only with a concrete consumer need, per ADR
> 0001's reversibility clause.
```

new:

```
> Not scheduled. Each lands only with a concrete consumer need, per ADR
> 0001's reversibility clause. (The Metal backend moved out of this
> section — see Pending above.)
```

### `book/src/guide/installation.md` — exact replacements

**R1** — old:

```
| Metal | macOS (Apple Silicon) | `metal` | 🚧 post-v0.1 — raw `objc_msgSend` FFI |
```

new:

```
| Metal | macOS (Apple Silicon) | `metal` | ✅ Shared + Private/staging (ADR 0004) |
```

**R2** — the sentence containing "Metal" near line 15 ("…the complete **Vulkan** backend, and a `NoDeviceStub`. Metal" + its continuation) — replace the sentence pair with:

```
epoch tracker, the complete **Vulkan** backend, and a `NoDeviceStub`. The
Metal backend (ADR 0004) completes the substrate on macOS.
```

Read the surrounding text first and keep it coherent; the intent is: Metal is no longer "post-v0.1 pending" — it is implemented.

### `book/src/getting-started.md` — exact replacements

**R1** — old:

```
# zunesha = { version = "0.1", features = ["metal"] }  # macOS (post-v0.1)
```

new:

```
# zunesha = { version = "0.1", features = ["metal"] }  # macOS
```

**R2** — old:

```
cargo run --features vulkan --example device-info
cargo run --features vulkan --example buffer-roundtrip
```

new:

```
cargo run --features vulkan --example device-info
cargo run --features vulkan --example buffer-roundtrip
# macOS:
cargo run --features metal --example device-info-metal
cargo run --features metal --example buffer-roundtrip-metal
```

**R3** — anywhere the page says Metal is not yet available / pending, update the wording to "implemented (ADR 0004)". Read the page; keep edits minimal and truthful.

### `book/src/examples/device-info.md` — append at the end

````
On macOS, the Metal twin prints the ADR-0004 queue shape:

```sh
cargo run --features metal --example device-info-metal
```
````

### `book/src/examples/buffer-roundtrip.md` — append at the end

````
On macOS, the Metal twin exercises both storage modes (Shared and
Private + staging):

```sh
cargo run --features metal --example buffer-roundtrip-metal
```
````

## Tests

No new unit tests (the backend's 31-test suite covers behavior). The
examples ARE the test surface for this unit — they must build and their
`#![cfg]` gates must keep no-backend builds compiling.

Assertions:
1. `cargo build --features metal --examples` succeeds, zero warnings.
2. Default-feature build (`cargo build --examples`) still succeeds — the
   metal examples compile to empty under no-backend cfg (their gate makes
   them no-ops, exactly like the vulkan examples on macOS).

## Constraints

- No new crates or dependencies.
- Apache-2.0 SPDX headers on both new example files (as in the Contracts).
- Files out of bounds: `src/**`, `docs/adr/**`, `docs/plans/**`,
  `AGENTS.md`, `CHANGELOG.md` (release-time), `Cargo.lock` excepted if
  cargo touches it.
- Do NOT run any git command. Leave changes in the working tree.
- When prose in this plan and a Contract block disagree, the Contract wins.

## Completion

Run and fix until all green (pipe through grep/tail — never paste full
logs; tool timeouts are SECONDS, cargo <= 600):

```bash
cargo build --features metal --examples 2>&1 | grep -E "error|warning" | tail -5
cargo build --examples 2>&1 | grep -E "error|warning" | tail -5
cargo test --features metal 2>&1 | grep -E "test result|FAILED|error" | tail -5
cargo fmt --all --check && echo FMT-OK
cargo clippy --features metal --all-targets -- -D warnings 2>&1 | tail -3
```

Note: `cargo test` may skip Metal tests if no device — that is fine; the
build steps are the gate here. Do NOT set `ZUNESHA_REQUIRE_METAL`.

## Out of scope

- ADR 0004 itself (parallel unit).
- CHANGELOG entry (release-time).
- The `metal` module source (complete; do not modify).
- Goldenweek-side docs.
