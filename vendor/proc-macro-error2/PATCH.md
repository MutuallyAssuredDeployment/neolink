# Local Rust compatibility patch

Source: `proc-macro-error2` 2.0.1 from crates.io.

Upstream: https://github.com/GnomedDev/proc-macro-error-2

Published source revision: `9b129234ed8f50d3ff8da09041dcbd980b63fc8c`.
The original MIT and Apache-2.0 licenses are included.

## Change

In `src/lib.rs`, change `extern crate proc_macro;` to
`pub extern crate proc_macro;`. The crate publicly re-exports `proc_macro`
through its helper module, so the original private declaration triggers
Rust warning E0365 and will become a compiler error.

This is the correction suggested by the Rust compiler for
https://github.com/rust-lang/rust/issues/127909.

The root `[patch.crates-io]` selects this local package for the entire
workspace. Cargo.lock records the local source. The Cargo registry cache
is not modified. All other upstream source files remain unchanged.

## Removal

Once a compatible upstream release contains the correction:

1. Remove the root `proc-macro-error2` patch and its workspace exclusion.
2. Update Cargo.lock to the corrected upstream package.
3. Run the Neolink build, tests, and future-incompatibility report.
4. Remove this vendor directory after those checks pass.
