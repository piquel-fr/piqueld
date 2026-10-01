# ADR 0002: Cargo builds the embedded dashboard

Status: accepted, 2026-10-01. Has a planned follow-up: see
[Next step: artifact dependencies](#next-step-artifact-dependencies).

## Context

The dashboard is a Leptos client-side app compiled to WebAssembly and embedded
in the daemon by `apps/piqueld/build.rs`. Building it used to need four tools
outside Cargo:

- **Trunk**, to run the wasm build, wasm-bindgen, and wasm-opt, and to rewrite
  `index.html`;
- **the wasm-bindgen CLI**, which must exactly match the `wasm-bindgen` crate
  version, so Nix and CI each pinned it separately;
- **binaryen's `wasm-opt`**;
- **Tailwind**.

Each tool needed its own pin in Nix, CI, and the docs, and each could break on
its own. In October 2026, Trunk 0.21 stopped compiling with GCC 16, both via
`cargo install` and in nixpkgs. It vendors a C library that uses a removed
compiler option.

Tailwind contributed almost nothing. None of the dashboard's 97 classes is a
Tailwind utility. Our stylesheets passed through it unchanged, so its only
output was its preflight reset.

## Decision

The daemon build script builds the bundle with Cargo alone (see
`apps/piqueld/build/dashboard.rs`):

1. **Compile.** A nested Cargo compiles `piqueld-ui` for
   `wasm32-unknown-unknown` with the `dashboard` profile (`opt-level = "z"`,
   one codegen unit). It uses a sibling target directory, so it can't deadlock
   on the outer build's lock.
2. **Bind.** `wasm-bindgen-cli-support`, the library behind the CLI, generates
   the bindings in-process. It's pinned next to `wasm-bindgen`, so Cargo
   enforces the version match.
3. **Style.** The stylesheets in `apps/piqueld-ui/styles/` are concatenated in
   cascade order. `reset.css` is the Tailwind preflight, vendored.
4. **Shell.** The script writes a one-line loader module, fills the
   placeholders in `index.html` with content-hashed names, and embeds every
   file.

The shell has no inline scripts, so the Content-Security-Policy is a constant.
The only prerequisite is the wasm32 target, which `rust-toolchain.toml`
installs.

Measured on the dashboard at the time of the change:

| Build | wasm | gzipped |
| --- | --- | --- |
| Trunk, release profile + `wasm-opt -Oz` | 5,031 KB | 1,567 KB |
| `dashboard` profile, no wasm-opt | 2,897 KB | 793 KB |
| `dashboard` profile + `wasm-opt -Oz` | 2,497 KB | 903 KB |

wasm-opt made the gzipped module larger, so it was dropped rather than
replaced. Screenshots of every dashboard page, at desktop and mobile widths,
matched pixel for pixel before and after, apart from live timestamps.

Alternatives considered:

- **Keep Trunk.** This keeps the four external pins and their breakage.
- **The `wasm-opt` crate.** It compiles binaryen's C++, and it doesn't help
  gzipped size here.
- **Server rendering without WebAssembly (Topcoat).** Its runtime needs
  `'unsafe-eval'`, and the UI would run inside the daemon instead of using the
  public API.

## Next step: artifact dependencies

The remaining workaround is the nested Cargo invocation. Cargo can't yet
declare "build this crate for another target" as a dependency.
[Artifact dependencies](https://github.com/rust-lang/cargo/issues/9096)
(RFC 3028, `-Z bindeps`) will. Once they reach stable Cargo, replace the nested
build with a build dependency along these lines:

```toml
# apps/piqueld/Cargo.toml
[build-dependencies]
piqueld-ui = { path = "../piqueld-ui", artifact = "bin", target = "wasm32-unknown-unknown", optional = true }
```

The build script then reads the module path from Cargo's
`CARGO_BIN_FILE_PIQUELD_UI_piqueld-ui` environment variable. That deletes:

- `Dashboard::compile` and its environment scrubbing;
- `nested_target_dir` and the `target-ui` directory;
- the manual `rerun-if-changed` tracking, because Cargo tracks the dependency
  itself.

When doing this, check how the stabilised feature selects the profile for
artifact dependencies. The size settings in `[profile.dashboard]` may need to
become a `[profile.*.package.piqueld-ui]` override.

Revisit this when issue #9096 closes, or when `bindeps` is listed as stable in
the Cargo changelog.
