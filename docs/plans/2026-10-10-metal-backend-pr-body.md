# Metal backend (ADR 0004) — `zunesha::Device` on macOS

Implements the complete Metal backend per `docs/plans/2026-10-08-metal-backend-requirements.md`, absorbing Borsalino `src/metal.rs` (v0.7.0) as the reference and improving on its two known gaps (ignored `MemoryStrategy`; unpooled init/create paths). Verified end-to-end on Apple M5 Max; unblocks Borsalino-Metal's staged migration (Phase 3) and precedes Goldenweek-Metal per substrate-first sequencing.

## What lands

- **`metal` feature** (`dep:objc`, macOS): `MetalDevice` — ADR-0004 queue reinterpretation (one MTLCommandQueue services compute+graphics, transfer `None`), `MTLCopyAllDevices` device-hint matching, Shared and Private+staging storage modes (honouring `MemoryStrategy` — the improvement over Borsalino), mandatory `create_device_buffer` override, escape hatches (`raw_device` / `raw_buffer` / `command_queue`), pools on every objc-minting path
- **18 Metal GPU tests** on real hardware (35-test suite total under `ZUNESHA_REQUIRE_METAL=1`), incl. the R5.6 pool-drain retention regression, consumer-style blit via raw handles, concurrency regressions
- **macOS CI**: self-hosted `test-macos` job (hantaro fleet recipe) with `ZUNESHA_REQUIRE_METAL=1` — fails hard when Metal is unreachable
- **ADR 0004** + ROADMAP/architecture/book/installation updates; examples `device-info-metal`, `buffer-roundtrip-metal` (with non-macOS fallback builds); CHANGELOG `[Unreleased]`

## Review loop (gpt-6.1-sol moments, 3 rounds, converged)

- **r1** — 3×P1 + 1×P2, every finding verified against its reproduction then fixed (`5d69f63`): typed readback over `contents()` mappings (UB for over-aligned `Pod`) → `collect_aligned` byte-copy; per-device staging lock raceable via the MTLDevice singleton → per-buffer locks + foreign-device rejection; allocation overflow (debug panic / release wrap) → checked math; Linux `--features metal --all-targets` → fallback `main`s
- **r2** — 1×P2 (ADR drift) + 1×P3 (pre-existing vulkan examples, same class) fixed (`861ca59`); full feature × target matrix builds `--all-targets`
- **r3** — converged: zero P1/P2; both P3s adopted (`11d025a`)

## Notes for reviewers

- The alignment floor is 16: Metal exposes no queryable value — adopted as documented conservative floor (ADR 0004 evidence trail)
- Known follow-up (out of scope, recorded in CHANGELOG): `vulkan.rs` carries the same readback-cast and uninit-overflow patterns structurally — separate PR on Linux hardware
- `MTLCreateSystemDefaultDevice` returns a per-GPU singleton; cross-instance reads serialize on the buffer's lock, genuinely foreign-MTLDevice reads are rejected (ADR 0004 §4)
