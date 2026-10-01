# GLib 0.18.5 security backport

Base: the published crates.io `glib` 0.18.5 archive, checksum
`233daaf6e83ae6a12a52055f568f9d7cf4671dabb78ff9560ab6da230ce00ee5`.
The archive checksum was verified against the original Cargo.lock at import.

The only upstream source change is the two-line fix in `src/variant_iter.rs`
from [gtk-rs/gtk-rs-core commit b5a4071e439bef2b5eea76c3aa25e5ae84839e34](https://github.com/gtk-rs/gtk-rs-core/commit/b5a4071e439bef2b5eea76c3aa25e5ae84839e34)
([PR #1343](https://github.com/gtk-rs/gtk-rs-core/pull/1343)). It makes `p`
mutable and passes `&mut p` to `g_variant_get_child`, fixing
[RUSTSEC-2024-0429](https://rustsec.org/advisories/RUSTSEC-2024-0429.html).

Tauri's GTK3 dependencies still require the GLib 0.18 API. The crate keeps its
upstream version; a root `[patch.crates-io]` selects this copy for the entire
dependency graph. No advisory is suppressed. Cargo audit may continue to report
the version-based advisory even though this exact source defect is patched.

`src-tauri/tests/glib_variant_iter.rs` exercises all five affected iterator
methods with optimizations in Linux CI. Debug-only coverage is insufficient
because compiler optimizations expose the invalid immutable-pointer write.

Remove this fork and the Cargo patch when the GTK dependency chain supports an
upstream release containing the fix (GLib >= 0.20). Preserve upstream licensing.
