# Design

The host owns web views, authentication, authorization and thread scheduling. The portable journal owns redaction and bounded local persistence. Optional capture translates native/page events into records without depending on an application framework.

Capture is explicit and page-scoped. Reading never enables it. Person-owned capture cannot be disabled or replaced by an agent. Agent-owned capture can expire independently; the host invokes expiry and closed-page pruning as part of its lifecycle.

The journal uses a global monotonically increasing sequence and stable per-page record keys. Updating an existing key preserves its sequence. Missing incoming detail preserves previous detail. Reads, opens and clears are page-scoped. Batches sanitize before entering a transaction and either commit completely or fail.

Entry count retention and retained detail-byte limits operate separately. Dropping old details keeps their summary entries until count eviction. Active-page and batch limits reject explicitly. Payload limits do not bound SQLite file size, WAL size or all transient allocations. Calls are synchronous: move journal I/O to a worker when needed.

Credential redaction happens before persistence for recognized structured headers, URL credentials/query fields and supported bodies. Console prose, arbitrary paths and unknown secret formats require host policy; there is no universal secret detector. Argument retention defaults off. Hosts must authorize access to the journal and protect the database file.

The macOS adapter registers a page-scoped sink on the main thread. The host supplies bounded dispatch to its worker, detaches before disposal and reports dropped events. Private WebKit resource delegates are optional and experimental; page instrumentation remains a separate capture source. Correlation handles timing proximity and redirect hops but does not claim complete browser traffic coverage.
