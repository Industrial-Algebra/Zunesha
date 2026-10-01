# Design Refusals

Zunesha follows IA Design Principle 4 (Architectural Refusal) — v0.1 says no
on purpose:

| # | Refusal | Rationale |
|---|---|---|
| 1 | **No shaders, no pipelines.** | Shader compilation stays in Borsalino (compute) and Goldenweek (graphics). Zunesha never imports `naga`. |
| 2 | **No windowing, no presentation.** | Zunesha owns the device, not a surface. Presentation is Goldenweek's concern. |
| 3 | **No graphics queue requirement.** | Compute-only hardware (GB10, headless GPUs) must run Borsalino unimpeded. Queues are capability-driven. |
| 4 | **No `wgpu` dependency.** | Hand-roll Metal/Vulkan FFI — Borsalino/Goldenweek lineage. |

Each refusal is a scope boundary that keeps the substrate small and its
consumers honest: the moment Zunesha grows a shader compiler, it stops being
the thing both consumers can stand on.
