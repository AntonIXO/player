## 2024-06-13 - Bounds checks in `Vec::push` for hot audio loops
**Learning:** In highly trafficked audio processing loops (like `Packer::pack`), calling `Vec::push` repeatedly incurs bounds checks that LLVM cannot always elide, even when `Vec::reserve` is used up front. Using `Vec::extend` with iterators allows LLVM to compute the necessary length and safely write elements without per-element checks, yielding a ~30-40% speedup in packing time.
**Action:** Prefer `extend` with iterators (like `.map()` or `.flat_map()`) over explicit `for` loops calling `.push()` when collecting fixed-size transformations in hot loops.

## 2024-06-14 - Pre-allocation and chunks_exact in slice processing loops
**Learning:** For loops over complex structures where `extend` + `flat_map` is not easily applicable due to state mutations (like DSD to DoP marker toggling in `DopPacker::pack`), pre-allocating using `reserve` combined with `.chunks_exact()` and an explicit loop avoids `Vec` bounds checks entirely and yields ~20-30% performance improvements. A manual `while` loop accessing elements iteratively misses chunk optimization logic.
**Action:** Prefer `chunks_exact()` instead of `while` loops with slice indexing `[i..i+frame_in]`, and always combine with exact `.reserve()` to avoid reallocation overhead.

## 2024-06-15 - Fast byte-wise mutations over `extend_from_slice` in DoP packing loops
**Learning:** `extend_from_slice` calls within tight inner loops building PCM frames per channel can incur overhead from iterator adapters, bounds checking or slice copying that LLVM struggles to fully elide when sizes are dynamic or branching exists per channel. Doing a single `resize` of the destination buffer and directly assigning bytes via a `chunks_exact_mut` iterator is significantly faster (~30% reduction in pack time) because LLVM sees the non-overlapping slice and avoids bounds checks per element.
**Action:** When manually assembling output byte structures across multiple channels from parallel structures, pre-`resize` the destination and iterate over it with `chunks_exact_mut` rather than pushing or extending slice by slice.
