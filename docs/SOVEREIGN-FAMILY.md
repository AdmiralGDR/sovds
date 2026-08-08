# The Sovereign Engineering family

A catalogue of sibling projects built to the same internal engineering
charter (the principles 0–V). They share a doctrine — zero-dependency
agnostic cores, hexagonal ports/adapters, make-invalid-states-unrepresentable,
ACID/rollback, bounded no-panic execution, reproducible builds, zero-touch CI — but
differ in domain and language. This document exists so the family stays coherent
and so any one project can be brought to the common format deliberately, as its own
task.

> Scope note: this is a **catalogue + recommendations**, not a change log for those
> projects. Each lives in its own repository; edits to them are separate work.

## The members

| Project | Domain | Language / build | Licence | Maturity |
|---------|--------|------------------|---------|----------|
| **this repo** (`sovds` + Router Brain) | dataset toolkit + off-router network supervisor | Rust, Cargo workspace | MIT OR Apache-2.0 | toolkit extracted; router live |
| **sovmr** | GPU AV1 video re-encoder (zero-copy NVDEC→NVENC, "no FFmpeg") | Rust, Cargo workspace (9 crates) | AGPL-3.0 | v1.0.0 |
| **sovscale** | Vulkan temporal upscaler (DLSS/FSR-style, injects under Proton) | **Zig** (0.16), `build.zig` | AGPL-3.0 | advanced WIP |
| **AetheriumFramework** | universal Minecraft mod meta-loader (load-time bytecode transform) | **Java 21 / GraalVM** (+Kotlin, C++), Gradle | AGPL-3.0 | v1.0.0-SINGULARITY |
| **Финансовая Крепость** (`Finances/`) | personal-finance dashboard with a hard-coded "manifest" ruleset | Python (Streamlit) + C++17 core | — | working MVP, no tests |

## What binds them

The doctrine, expressed in each stack's idiom:

- **`sovmr`** is the cleanest Rust embodiment: a std-only-forever domain core, a
  port/adapter facade (`EncoderProvider`, "swap the engine in one line"), a
  hostile-input gate, ACID atomic-rename outputs, zero-alloc ISOBMFF parsing. It is
  the **structural template** this repo's workspace mirrors.
- **`sovscale`** carries the **canonical text of the principles 0–V** in its the internal engineering charter.
  Zig, not Rust, but the same zero-third-party-dependency law and never-crash-host
  discipline.
- **AetheriumFramework** is philosophically parallel without ever naming the
  set of principles: a literal ACID transaction engine, sealed-types /
  invalid-states-unrepresentable, a load-time `FFM → JNI → pure-Java → disable`
  fallback chain, reproducible stamped jars. JVM, so the overlap is
  architecture/docs, not code.
- **Финансовая Крепость** hard-codes its own "манифест" as enforced business rules
  (safety cushion, withdrawal ban, asset whitelist) and reaches for a C++ core "to
  reduce dependence on third-party libraries" — the same instinct, applied to
  money. Its private data (`finance_tracker.db`, `backups/`) is correctly
  gitignored and must never enter any shared tooling.

## The one real inconsistency: licensing

The family is split:

- `sovmr`, `sovscale`, `AetheriumFramework` → **AGPL-3.0** (strong, viral copyleft).
- this repo (`sovds` + Router Brain) → **MIT OR Apache-2.0** (permissive).

This is **not an accident to fix by making them uniform.** They serve different
purposes:

- `sovds` is a **library meant to be embedded in third-party projects.** A viral
  copyleft would force every integrator to adopt AGPL — the opposite of "easy to
  integrate." Permissive MIT/Apache is the correct, deliberate choice here; keep it.
- `sovmr`/`sovscale`/Aetherium are **end-user applications/frameworks**, where AGPL
  protects the work without blocking a downstream library user. AGPL is defensible
  there.

Recommendation: keep the split, but make it *intentional and documented* (this
section is that record). If a shared **library** is ever extracted from the AGPL
projects for reuse, relicense that library permissively for the same reason `sovds`
is permissive.

## Bringing a project to the common format (per-project, future tasks)

- **`sovmr`** — already conformant; nothing to do beyond keeping it the reference
  Rust layout.
- **`sovscale`** — Zig; conformant to the doctrine. If any pure-logic piece becomes
  reusable, consider extracting it behind a port like `sovds`/`sovmr` do.
- **AetheriumFramework** — JVM; a full port to Rust/Zig would be a rewrite, not a
  migration. The reusable asset is its **architecture and bilingual Diátaxis docs**;
  adopt those doc conventions, don't port the code.
- **Финансовая Крепость** — the clearest gap is **no tests**. Highest-value step:
  add characterization tests around the C++ core (`core_math.cpp`: Monte-Carlo,
  rebalance, inflation) and the manifest-rule enforcement in `repository.py`, then a
  proper build manifest for the C++ side. Keep private data out.
