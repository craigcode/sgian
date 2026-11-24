# Dependency warning review for 0.2.0

Reviewed 2026-11-23 against the current RustSec advisory database. The four
unignored informational warnings from the earlier native acceptance were
reviewed individually; no new audit ignore was added.

| Dependency / advisory | Disposition |
| --- | --- |
| `rustix` 1.1.4 — [RUSTSEC-2026-0486](https://raw.githubusercontent.com/RustSec/advisory-db/main/crates/rustix/RUSTSEC-2026-0486.md) | Updated to **1.1.6**, the patched release for the zero-length extended-attribute buffer issue. |
| `tokio` 1.52.3 — [RUSTSEC-2026-0386](https://raw.githubusercontent.com/RustSec/advisory-db/main/crates/tokio/RUSTSEC-2026-0386.md) | Updated to **1.53.3**, a patched release for the `AsyncFd` safe-constructor unsoundness. |
| `pin-utils` 0.1.0 — [RUSTSEC-2026-0441](https://raw.githubusercontent.com/RustSec/advisory-db/main/crates/pin-utils/RUSTSEC-2026-0441.md) | **Removed** from the lockfile by disabling unused Nix defaults in the existing `portable-pty` fork. The fork uses only `fs` (`access`) and `term` (`tcgetattr` / `Termios`); Nix's unused `aio` feature introduced `pin-utils`. See the [fork note](../../src-tauri/vendor/portable-pty/SGIAN-PATCH.md). |
| `chrono` 0.4.44 — [RUSTSEC-2026-0543](https://raw.githubusercontent.com/RustSec/advisory-db/main/crates/chrono/RUSTSEC-2026-0543.md) | **Retained as an inactive optional lockfile dependency**, with the warning visible. RustSec lists no patched release. It is optional in `serde_with` 3.24.0, reached through Tauri's utilities; Sgian does not enable the `chrono_0_4` feature. |

## Chrono evidence and removal condition

Both commands below report `warning: nothing to print` and exit successfully,
including all target platforms and all features declared by Sgian:

```sh
cargo tree --locked --manifest-path src-tauri/Cargo.toml --target all -i chrono -e features
cargo tree --locked --manifest-path src-tauri/Cargo.toml --target all --all-features -i chrono -e features
```

`serde_with` defaults to `std` and `macros`. Its weak `chrono_0_4?/clock` and
`chrono_0_4?/std` feature references do not activate its optional Chrono
dependency. Chrono's presence in `Cargo.lock` is not evidence that it is built
into Sgian. Recheck these commands whenever framework dependencies or features
change; enabling Chrono requires a fresh review. Remove this entry when the
upstream optional dependency leaves the lockfile. Do not add a blanket audit
ignore or substitute a date library inside a third-party framework merely to
hide an inactive advisory.

## Audit policy and validation

Run the audit from the repository root so it reads `.cargo/audit.toml`:

```sh
cargo audit --file src-tauri/Cargo.lock
```

The result after these changes is **zero unignored vulnerabilities and one
informational warning**, for Chrono. `pin-utils` is absent from the lockfile.
The existing two exceptions are unchanged: the Linux-only unmaintained
`proc-macro-error` chain and the vendored GLib version whose iterator fix is
backported and regression-tested. Their removal conditions are in
[the audit configuration](../../.cargo/audit.toml).

Cargo-audit fails for unignored vulnerabilities, but informational warnings
do not fail it by default. Previous documentation overstated that policy and
is corrected in this version. Weekly CI continues to surface newly reported
advisories for review.

The version PR must pass the Rust, frontend, native protocol and packaging
checks on all four CI jobs before merge. Physical Windows acceptance and
production signing remain separate release gates.
