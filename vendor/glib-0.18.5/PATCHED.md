# Patched glib 0.18.5

Vendored copy of glib 0.18.5 from crates.io with one backported fix.

Advisory: GHSA-wrw7-89jp-8q8g / RUSTSEC-2024-0429 (unsoundness in
`Iterator`/`DoubleEndedIterator` for `glib::VariantStrIter`; fixed upstream in glib 0.20.0).

Backported: gtk-rs-core PR #1343 (https://github.com/gtk-rs/gtk-rs-core/pull/1343).
In `src/variant_iter.rs`, `VariantStrIter::impl_get` now declares `let mut p` and passes
`&mut p` (not `&p`) as the out-argument of the variadic `g_variant_get_child`.

Reason: wry 0.57.1 pins webkit2gtk =2.0.2 / gtk ^0.18, so glib cannot be upgraded to >=0.20.
Remove this patch once the stack moves to gtk-rs 0.20+. LICENSE and COPYRIGHT are unchanged.
