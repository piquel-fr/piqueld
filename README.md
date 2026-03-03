# piqueld

Daemon &amp; CLI to manage my infrastructure.

## Protocol

Daemon-CLI communication uses a simple length-prefixed message framing:
each message is preceded by a 4-byte little-endian `u32` indicating the
payload size in bytes, followed by the payload itself.

Payloads are JSON-serialized commands and responses, produced via `serde_json`.
For the full list of supported message types, see `piquel-core/src/ipc/message.rs`.
