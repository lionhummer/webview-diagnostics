# Verification for 0.1.0

Ten journal regression tests cover ownership, page-scoped access and clear, transactional rollback, count/detail retention, structured redaction, Unicode/truncated bodies, agent expiry, page capacity, stream truncation and known URL credentials. Four macOS adapter unit tests cover request/timing correlation, stream association and diagnostic reconstruction.

The real macOS smoke example verifies page console capture plus a synthetic loopback HTTP request. Locally verified on macOS 26.5.2; the resource-load delegate was available. CI additionally checks macOS and Linux with Rust 1.88/stable. This is not exhaustive validation of private WebKit APIs across operating-system updates.

Private selector availability is checked dynamically. Worker console output, images/media bodies, active frame-header probes, a full debugger, and adapters for other operating systems are outside 0.1.0. No universal credential-removal, memory allocator cap, filesystem quota or performance claim is made.
