# WebView Diagnostics

**An embedded-browser diagnostics journal with bounded SQLite storage and optional native macOS capture.**

Capture console messages, errors, requests, response details, and stream messages. Keep summaries small, read details when needed, and expose the same journal to a person or an agent. The core runs without Tauri or a browser runtime.

Rust 1.88+. MIT OR Apache-2.0. The optional `webkit` feature targets macOS and uses public and private WebKit APIs; treat its platform adapter as experimental.

## Quick start

```toml
[dependencies]
webview-diagnostics = "0.1"
```

```rust
use webview_diagnostics::*;
let mut journal = Journal::in_memory(Limits::default())?;
journal.set_capture("page-1", OpenedBy::Person, true, now_ms())?;
let sequence = journal.record(Record {
    page: "page-1".into(), key: "console-1".into(), at: now_ms(),
    entry: Entry::Console {
        level: Level::Info, text: "Connected".into(), source: None,
        line: None, column: None,
    }, detail: None,
})?;
let opened = journal.open_entry("page-1", sequence)?;
assert!(opened.is_some());
# Ok::<(), webview_diagnostics::Error>(())
```

For durable storage use `Journal::open("diagnostics.sqlite", Limits::default())`. A journal uses one synchronous SQLite connection. Run it on a dedicated host worker with a bounded queue; never perform database IO inside a native main-thread capture callback.

## Native WebKit capture

Enable `features = ["webkit"]`. Pass your existing `WKWebView` and an `Rc<dyn Fn(Record)>` callback to `webkit::attach`. The callback runs on the main thread. Keep it short, use `try_send` into a bounded worker queue, count dropped records, and never re-enter attachment from it.

`attach` returns `CaptureCapabilities { native_network }`. Missing private resource-delegate support is reported explicitly; console capture can still run. Call `detach` to stop capture and `unloaded` before discarding a view. Each simultaneously active view must have a unique page ID and its own user-content controller. Do not overwrite an existing resource-load delegate owned by another integration.

```rust,ignore
let capabilities = webview_diagnostics::webkit::attach(
    main_thread_marker, &web_view, "page-1", sink,
);
// At shutdown, on the main thread:
webview_diagnostics::webkit::unloaded(main_thread_marker, &web_view, "page-1");
```

The capture adapter has no application-global view lookup, credentials, HTTP client, Tauri commands, or automatic frame-header probes. The host owns those integrations. `archived_body` optionally reads a textual resource from the supplied view; poll it on the main thread with Tokio's time driver enabled.

## What is captured

| Source | Capture | Boundary |
| --- | --- | --- |
| Native resource delegate | Requests, response headers/status, redirects, failures | Private selector; no network list when unavailable |
| Page instrumentation | Console, uncaught errors, resource timing | Page can observe or tamper with its own instrumentation; workers' console output is absent |
| Page fetch/XHR instrumentation | Text response bodies | Truncated; does not provide every native resource body |
| WebSocket/EventSource/fetch streams | Bounded messages and dropped counts | Page instrumentation; binary content is omitted |
| Web archive lookup | Text document/script/style resource | Availability depends on WebKit; no image/media bodies |

The package does not open Safari's inspector, provide a debugger or DOM editor, or implement Chromium's protocol. It supplies a capture-and-query layer for embedded views.

## Journal guarantees

- Capture is off until explicitly enabled; reading never enables it.
- An agent cannot disable a console opened by a person.
- `expire_agents` expires only agent-owned capture; the host schedules it and calls native `detach` for expired pages.
- Stable page/key updates preserve entry sequence. A page cannot read another page's entry by sequence.
- `record_many` is transactional: an invalid/disabled member rejects the whole batch.
- Entry count and detail byte retention are independent. Old details can disappear while their summaries remain.
- `clear(page)` clears only that page. `prune_closed` removes only disabled pages older than the cutoff.

The journal does not authenticate callers. Put page authorization in the host boundary before exposing queries to tools.

## Limits and redaction

| Limit | Default |
| --- | --- |
| Summaries per page | 5,000 |
| Serialized detail bytes per page | 16 MiB |
| Retained page identities | 512 |
| Records per transaction | 2,000 |
| Console arguments | Omitted; opt in through `keep_console_arguments` |
| Body text | 65,536 Unicode characters |
| Stream messages | Latest 200; text limited to 2,048 characters |

Known credential header values and secret-looking fields in JSON/form bodies are redacted before journal storage. Known URL credential query parameters, userinfo and fragments are removed from summaries. Truncated JSON uses a best-effort field redactor. Free-form console text, URL paths, stacks, custom field names and opted-in previews can still contain sensitive information. These heuristics are not a universal secret detector. Add application-specific filtering in the sink or omit those fields.

Text limits count characters; the detail budget counts serialized UTF-8 bytes. Disk file size, SQLite WAL size, caller allocations and host queues are separate from retained detail payloads. Page capacity exhaustion returns `Capacity`; it never silently removes active pages.

## Examples and checks

```sh
cargo run --example journal
# macOS: actual WKWebView console and local HTTP request capture
cargo run --features webkit --example webkit_smoke
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps
cargo package --locked
```

The native smoke test binds an ephemeral loopback server, loads synthetic HTML, verifies console output and an actual native request when the capability is available, then detaches. It opens no external website and needs no credentials. CI checks the portable core on Linux and the native adapter on macOS.

See [design](docs/design.md), [adaptation](docs/adapting.md), and [verification](docs/verification.md). The native callback API and platform compatibility may evolve in minor releases before 1.0.
