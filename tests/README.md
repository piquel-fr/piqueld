# Cross-cutting tests

Focused integration tests live beside the crate that owns the seam they exercise.
The daemon tests cover a fresh SQLite lifecycle, the real Docker abstraction with
an in-memory fake, and an opt-in isolated Docker qualification. The client tests
cover the loopback TCP and Unix-socket transports.
