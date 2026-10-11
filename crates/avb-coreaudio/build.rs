// SPDX-License-Identifier: MIT OR Apache-2.0

//! Link the plug-in as a Mach-O **bundle** (`MH_BUNDLE`), not a dylib.
//!
//! CoreAudio's `coreaudiod` loads a HAL plug-in with `CFBundle`, which only
//! accepts an `MH_BUNDLE` executable. Rust's `cdylib` crate-type emits an
//! `MH_DYLIB`, which CFBundle silently refuses (the plug-in never appears and
//! nothing is logged). `cargo:rustc-link-arg` applies the flag to THIS crate's
//! final artifact only — not to dependencies or build scripts — so it does not
//! break `libc`'s build script the way a blanket `RUSTFLAGS=-bundle` does.
//!
//! `-bundle` and `-dylib` conflict, so we also need to stop the cdylib from
//! adding `-dylib`. On Apple's linker, passing `-bundle` last wins; if the
//! toolchain still emits `-dylib` first we additionally pass
//! `-Wl,-bundle` is not needed because rustc invokes `cc`/`ld` directly.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // Scope to the cdylib: apply these ONLY to the plug-in artifact. A plain
        // `rustc-link-arg` also hit the `cargo test` harness, linking it as a
        // bundle too, so the unit-test binary failed with "cannot execute
        // binary file" and this crate's tests never ran.
        //
        // Emit a bundle, not a dynamic library.
        println!("cargo:rustc-link-arg-cdylib=-Wl,-bundle");
        // Undefined symbols (CoreFoundation etc.) are resolved by the host at
        // load time, matching how coreaudiod loads the plug-in.
        println!("cargo:rustc-link-arg-cdylib=-Wl,-undefined,dynamic_lookup");
    }
}
