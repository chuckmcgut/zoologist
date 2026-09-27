# rusty_h264-decoder 0.16.0, patched

A copy of [rusty_h264-decoder](https://crates.io/crates/rusty_h264-decoder) 0.16.0 (BSD-2-Clause,
see `LICENSE`; upstream: https://github.com/remade-with-rust/rusty_h264), used through
`[patch.crates-io]` in the workspace `Cargo.toml`. Only `src/` and the manifest are copied (no
tests or examples).

One change, in `src/mb16.rs` (search for "Zoologist patch"): the single-threaded CABAC path for
P macroblocks without residual took a new `Box` for each job instead of one from
`edc_nores_pool`. After each flush those boxes were returned to the pool, which was never drawn
from on that path, so it grew by one box per such macroblock: about 2 MB/s of memory on a
1280×720 High-profile camera of a mostly still scene. The fix uses `take_nores_job`, like the
other paths.

Remove this copy (and the patch entry) once a release fixes it upstream.
