# Rspack allocative adapters

Based on the published `rspack-allocative` 0.3.5 crate (Meta's allocative, MIT OR Apache-2.0). The original source headers and licenses are retained. Local changes add adapters needed by Rspack and correct collection accounting. Debug (`release-debug`) and profiling binding builds include it automatically; other builds must opt in. Snapshot collection requires `RSPACK_ALLOCATIVE_DIR` at runtime.

Only adapters used by Rspack are retained. Unused upstream adapters, size-only visitors, and SVG/golden test helpers are removed. Rspack regression tests live in the dedicated `rspack_allocative_testing` crate:

```sh
cargo test -p rspack_allocative_testing --features allocative \
  --config 'target."cfg(all())".rustflags=["--cfg=allocative"]'
```

Ordinary structs whose types satisfy `'static` use the pinned nightly's `type_info` reflection and need no derive, registration, or per-type implementation. `Visit` provides an automatic traversal entry point for sized types. At compile time it first looks for an explicit `Allocative` adapter; otherwise it discovers struct/tuple fields and their offsets. Runtime traversal calls these precomputed visitors through the existing `Visitor`, preserving the byte tree and collection accounting.

```text
FlameGraphBuilder::visit_root             src/flamegraph.rs
  Visit::visit_memory                     src/reflection.rs
       ↓
  explicit Allocative adapter exists?     src/impls/
    yes → container API / guarded access / custom field policy
    no  → reflected struct fields → Visit for each field
       ↓
  Visitor → shared deduplication → folded byte tree
```

Keep derives for enums (which need to match the active variant) and types with `allocative(skip)` or custom `allocative(visit = ...)` field policies. Also retain typed adapters for fields with nonliteral array lengths: the pinned reflection compiler does not consistently normalize their const expressions, which can cause duplicate codegen symbols. The vtable query uses the same compiler intrinsic as `trait_info_of`, avoiding that wrapper's premature array layout query. Explicit adapters always take precedence over structural reflection. For generic derives, use `allocative(bound = "T: allocative::Visit")` (plus the type's existing bounds) without requiring every payload to implement `Allocative`; traversal now accepts `Visit`. Unsized payloads need a `Visit` implementation or a dynamic supertrait bridge. `MaybeAllocative`, `SourceAllocative`, and `StorageAllocative` provide that bridge in profiling builds, so concrete implementations need no derive either. Callbacks with inaccessible captures retain explicit opaque visitors.

Reflection never follows borrowed references or guesses ownership through raw pointers. Unsupported enums/unions, inaccessible raw-pointer storage, unaligned packed fields, and bare `UnsafeCell` emit coverage warnings rather than dereferencing unknown data. Existing lock adapters acquire a guard before visiting their contents. `ManuallyDrop` is opaque because its payload may already have been destroyed. Automatic reflection requires `T: 'static` (the type cannot contain temporary borrows; the value itself can be local). This prevents lifetime-erased metadata from selecting a static-only adapter for a borrowed value. Type identities remain internal to traversal and are not exported to `Any` or a type registry. Temporary borrowed helper values must use an explicit typed `Allocative::visit` call instead of automatic reflection. Each descriptor covers at most 256 immediate fields; larger types need an explicit adapter. No descriptor is rebuilt at snapshot time.

Use the repository's pinned `nightly-2026-04-16`. Reflection APIs are experimental; upgrading the toolchain requires rerunning the ownership tests, including dynamic dispatch and adapter selection. Do not replace a missing owning implementation with `size_of` or `skip`; check the snapshot's coverage warnings.

The older Dylint toolchain uses a non-reflective compatibility implementation when checking the adapter crate with snapshots disabled. It cannot build Rspack memory snapshots; enable profiling only with the workspace toolchain.

`FlameGraphBuilder::with_shared_ownership()` attributes shared payload to the first owner and terminates strong cycles. That owner may vary with hash iteration order. Compare represented totals or stable populations rather than assuming a shared allocation always appears under the same path.

The byte tree includes capacity where the container exposes it. Private hash-table/node metadata, allocator overhead and inaccessible external state remain outside complete accounting. It is an ownership estimate, not RSS, peak usage or an allocation-event trace. The compiler snapshot writes these limits and coverage warnings alongside the folded data. Counts are measured separately from the current module graph; folded byte weights never imply object counts.
