# Local libtailscale-sys patch

Minimal build sources from crates.io `libtailscale-sys` 0.2.2 (Rust wrapper:
<https://github.com/messense/libtailscale-rs>, embedded Go/C library:
<https://github.com/tailscale/libtailscale>). The Rust bindings, build script,
Go module versions, and C API are unchanged. Unused examples and platform
wrappers are omitted. Rust wrapper code is MIT-licensed; Go/C code is covered
by `libtailscale/LICENSE` (BSD-3-Clause).

The upstream listener records peers against the **sent** descriptor, but
`SCM_RIGHTS` can assign a different descriptor in `accept`. It also returns
bracketed IPv6 strings that the Rust wrapper cannot parse.

The patch sends the 16-byte peer IP with the descriptor, then registers that
IP against the **received** descriptor before returning from `accept`. The C
accept function delegates to Go so the descriptor and peer stay together.
`net.IP.String` supplies unbracketed IPv6 and ordinary IPv4 literals. Partial
metadata reads and closed listeners return errors without exposing a partial
connection. Connection bytes are untouched.

`just test-tailnet-bridge` exercises both address families with distinct sent
and received descriptors, concurrent live peers, unchanged connection bytes,
partial metadata, and listener EOF. It runs as part of `just test` and CI.

Remove the Cargo patch and this directory once a released upstream version
fixes both issues. If Go module versions change, update the libtailscale Go
module fetch in `flake.nix` and its fixed-output hash too.
