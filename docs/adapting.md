# Host adaptation

## Tauri or another Rust desktop shell

Resolve the view in the host, on its main thread. Enable its journal page explicitly, then attach a short callback. Forward `Record`s into a bounded queue using a nonblocking operation. A dedicated worker owns `Journal` and drains records in batches through `record_many`; it can send serialized summary events back to the UI. If the queue is full, increment a visible dropped-record counter rather than blocking WebKit.

A React panel can query summaries, fetch `open_entry(page, seq)` for details, filter using `Query`, and clear its page. Implement authenticated page access in your command/tool handlers. Rust should retain authority over journal access; do not expose a raw database or unscoped query endpoint.

## Other browser engines

Translate engine events into `Entry`, `Detail`, and `Record`; retain a stable key while updating a request. Call `record_many` with bounded transactions. Chromium, Linux WebKitGTK and Windows WebView2 adapters are not supplied in 0.1.0. Their callbacks must report what they actually capture rather than inventing parity with the macOS adapter.

## Lifecycle and privacy

Use a distinct page identity per live view. On account/environment changes, detach views and destroy or explicitly clear the appropriate journals. File encryption and tenant separation are host responsibilities. Coordinate agent expiration with native detachment; `expire_agents` changes journal admission only.

Do not collect real browsing sessions for fixtures. Synthetic HTTP responses and generated console values are sufficient for deterministic tests. Keep native sinks non-reentrant and never synchronously write SQLite on the main thread.
