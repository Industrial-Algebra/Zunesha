# Zunesha — Agent Operating Guide

## Gitflow (Non-Negotiable)

Zunesha follows IA gitflow as defined in the
[`ia-gitflow`](https://github.com/Industrial-Algebra/ia-toolkit/blob/main/skills/ia-gitflow/SKILL.md)
skill. Read it before touching branches.

> The gitflow structure is live: `develop` exists, `main` and `develop` are
> protected (PRs only, required CI checks, admins included), and CI runs on
> every PR. All feature work goes through `develop` via PRs. The repo is
> mirrored to the IA Forgejo (king-ghidorah) via dual-push on `origin`.

### Branch Model

```
feature/* ──PR──▶ develop ──release PR──▶ main ──tag v*──▶ publish
                     ▲                                        │
                     └──────── backmerge (merge commit) ──────┘
```

### Hard Rules

1. **Never push directly to `main` or `develop`.** Both are protected.
   No direct pushes — not "just a CI fix", not "a one-liner", not "it's faster".
   Branch it, PR it, let CI run. This is enforced by GitHub branch protection.

2. **Every release to `main` is followed by a `main → develop` backmerge**
   using a merge commit (never squash). This is the last step of releasing,
   not an optional chore.

3. **Release-only commits (version bump, changelog dating) live on a
   `release/*` branch**, not on `develop` or `main`.

### What went wrong elsewhere (do not repeat)

- **Direct pushes to main**: sibling projects bypassed review during CI
  emergencies. Branch protection now prevents this mechanically.
- **Silent `develop` recreation**: if `develop` is ever missing,
  **investigate why before recreating it** (check `delete_branch_on_merge`,
  recent deletions, etc.).
- **Skipped backmerges**: release PRs merged without backmerging `main`
  to `develop` cause the branches to diverge in history.

## Coding Standards

Follow the
[`ia-coding-standards`](https://github.com/Industrial-Algebra/ia-toolkit/blob/main/skills/ia-coding-standards/SKILL.md)
skill: TDD (test first), phantom types, `Result` not panic, exhaustive matching,
feature gates additive only, every public item documented.

## Project-Specific Conventions

- **Shared device substrate for Borsalino (compute) and Goldenweek (graphics).**
  Zunesha owns device/queues/memory-strategy/buffer-allocation. It does NOT own
  pipelines, shaders, dispatch, or presentation — those stay in the consumers.
- **Never import `naga`.** Shader compilation is the consumers' job. If you find
  yourself reaching for naga here, the code belongs in Borsalino or Goldenweek.
- **Capability-driven queues, never graphics-required.** Borsalino targets
  compute-only hardware (NVIDIA Grace Blackwell GB10 / DGX Spark — Justin has
  two). `Queues { compute (always), graphics: Option, transfer: Option }`. The
  baseline `init()` must not regress Borsalino-on-GB10.
- **Buffer ownership.** `zunesha::Buffer` is the shared primitive; the
  consumers' `GpuBuffer` types wrap it. Do not introduce a second, divergent
  buffer abstraction.
- **Unified GC safety at the device layer.** The epoch tracker lives here and
  observes *every* dispatch (compute + graphics) through the device. Consumers
  increment/decrement it; they do not run their own.
- **Structural correctness, not numerical.** Verification effort targets valid
  device/buffer state. Numerical exactness is Borsalino's concern.
- **Cross-crate proof agreement (IA P3).** When the `verify` feature lands,
  Zunesha owns the device/buffer-layer obligation bundles; Borsalino/Goldenweek
  per-kernel bundles cite them by `Origin`. Governed by an ADR.

## Naming

Zunesha = the giant immortal elephant that carries Zou on its back (One Piece) —
the foundational carrier. Part of the IA One-Piece GPU naming family:
**Borsalino** (Admiral Kizaru, compute), **Goldenweek** (Miss Goldenweek /
Marianne, graphics), **Zunesha** (device substrate). Briefly named *Rayleigh*
(2026-08-12); renamed when `rayleigh` was found taken on crates.io.

## License

Apache-2.0. See `LICENSE` and `CONTRIBUTING.md`.
