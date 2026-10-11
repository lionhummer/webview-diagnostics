//! Capture for a page's developer console (`devtools.rs`), in its WKWebView.
//!
//! - **Network** comes from WebKit's resource-load delegate
//!   (`-[WKWebView _setResourceLoadDelegate:]`, private, macOS 11+): every
//!   load the page makes — documents, frames, scripts, styles, images,
//!   fetches — seen from outside the page, which can neither see nor change
//!   it. With it come the request's headers and body and the response's
//!   headers, as WebKit has them: cookies are added later, in the network
//!   process, and were never here. Where the delegate is missing the console
//!   has no network list.
//! - **Response bodies** are not something the delegate sees. The page script
//!   reads those of the page's `fetch` and `XMLHttpRequest` calls — text
//!   only, the first `BODY_MAX` — and they join the request the delegate saw
//!   at the same address. A document's, script's or stylesheet's is read
//!   from the page when the entry is opened ([`archived_body`]): the main
//!   document as it arrived (`_getMainResourceDataWithCompletionHandler:`,
//!   private), the rest from the page's web archive. Images and media have
//!   none.
//! - **Timing** comes from the page's Resource Timing, joined to the request
//!   at the same address that started closest to it.
//! - **WebSockets and event streams**: the page script wraps `WebSocket` and
//!   `EventSource` and reads `text/event-stream` responses to `fetch`,
//!   passing their messages on in batches. The delegate never sees a
//!   WebSocket, so a socket is an entry of its own.
//! - **Console** has no native source: WKWebView reports nothing a page logs.
//!   A script in the page's own world wraps `console`, listens for uncaught
//!   errors and rejections (a worker's reach the page as errors), elements
//!   that fail to load, Content Security Policy violations and (where WebKit
//!   has them) deprecation reports, and posts to a handler registered in that
//!   world, each message with a preview of its arguments and where it was
//!   logged from. What WebKit tells only its own Web Inspector is rebuilt
//!   here: a failed load logs WebKit's reason (CORS refusals in WebKit's own
//!   words, which the delegate is given), a load the server refused logs its
//!   status, an element that loaded but was refused says why (a script or
//!   stylesheet of the wrong type), a load that never went out is checked for
//!   insecure content, and a frame cancelled before its response is checked
//!   for cancellation. This adapter does not re-request frame headers. What
//!   workers log never reaches the page, so the console does not have it.
//!   The script stays on the page once added (WebKit removes only all of a
//!   page's scripts at once) but does nothing without the handler, which is
//!   there only while the console is on: with it off, a document sees
//!   nothing of either. While on, the page could notice the wrapping, or
//!   post into it, as it can log anything.
//!
//! Both are attached when the console is turned on or a page loads with it
//! on, and come off when it is turned off or the page unloads. What is kept
//! of headers and bodies is `devtools::header` and `devtools::body`.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_foundation::{
    NSData, NSDate, NSDictionary, NSError, NSObjectProtocol, NSString, NSURL, NSURLRequest,
};
use objc2_web_kit::{
    WKContentWorld, WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKUserScript,
    WKUserScriptInjectionTime, WKWebView,
};
use serde::Deserialize;
use serde_json::Value;

use crate::capture_bridge as devtools;
use crate::{BODY_MAX, Detail, Entry, Header, Level, TEXT_MAX, Timing, URL_MAX, clip};

const HANDLER: &str = "wvDiagnosticsDevtools";

/// Console messages kept per page each second; past that they are counted,
/// and the count is logged once the second is over.
const CONSOLE_PER_SECOND: u32 = 300;
/// Response bodies taken per page each second; past that they are let go.
const BODIES_PER_SECOND: u32 = 50;
/// Largest message the page script may post; a larger one is let go.
const MESSAGE_MAX: usize = 512 * 1024;
/// Requests followed at once per page. Past it the oldest are let go: a page
/// holding thousands open is streaming, not loading.
const PENDING_MAX: usize = 2_000;
/// Finished requests a late response body or timing can still join.
const RECENT_MAX: usize = 300;
/// How far apart a request's start and its Resource Timing's may be.
const TIMING_SLACK_MS: i64 = 3_000;
/// Batches of stream messages taken per page each second; past that their
/// messages are counted as let go.
const FRAMES_PER_SECOND: u32 = 20;
/// WebSockets, and streams of the page script, followed at once per page.
const SOCKETS_MAX: usize = 100;
const STREAMS_MAX: usize = 300;
/// How long reading back a body from the page may take.
const ARCHIVE_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest stack kept with a console message.
const STACK_MAX: usize = 8 * 1024;

const SCRIPT: &str = r#"(() => {
  // Looked up on every post: the handler goes when the console is turned off
  // and a new one comes when it is turned on again.
  const handler = () => window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.wvDiagnosticsDevtools;
  const mark = Symbol.for('wvDiagnostics.devtools');
  if (window[mark] || !handler()) return;
  Object.defineProperty(window, mark, { value: true });
  // Taken before the page's own scripts can replace them.
  const setTimer = window.setTimeout.bind(window);
  const clearTimer = window.clearTimeout.bind(window);
  const now = Date.now;
  const MAX = 8192;
  const BODY_MAX = 65536;
  const post = (message) => {
    const to = handler();
    if (!to) return;
    try { to.postMessage(JSON.stringify(message)); } catch (_) {}
  };
  const cut = (text, max = MAX) => (text.length > max ? text.slice(0, max) + '…' : text);
  const nodeName = (node) =>
    node.nodeType === 1
      ? `<${node.tagName.toLowerCase()}${node.id ? '#' + node.id : ''}${typeof node.className === 'string' && node.className ? '.' + node.className.trim().split(/\s+/).join('.') : ''}>`
      : node.nodeName;
  const show = (value) => {
    if (typeof value === 'string') return value;
    if (value instanceof Error) return value.stack ? `${value.name}: ${value.message}\n${value.stack}` : String(value);
    if (typeof value === 'function') return `ƒ ${value.name || 'anonymous'}()`;
    if (value === null || typeof value !== 'object') return String(value);
    if (typeof Node !== 'undefined' && value instanceof Node) return nodeName(value);
    try {
      const seen = new WeakSet();
      const json = JSON.stringify(value, (_, item) => {
        if (typeof item === 'bigint') return `${item}n`;
        if (typeof item === 'function') return `ƒ ${item.name || 'anonymous'}()`;
        if (item && typeof item === 'object') {
          if (seen.has(item)) return '[Circular]';
          seen.add(item);
        }
        return item;
      });
      return json === undefined ? String(value) : cut(json);
    } catch (_) {
      return Object.prototype.toString.call(value);
    }
  };
  // A value as the console's detail shows it: a tree, a few levels deep and
  // a few hundred nodes in all, opened by the person rather than the page.
  const label = (value) => {
    if (Array.isArray(value)) return `Array(${value.length})`;
    if (value instanceof Map) return `Map(${value.size})`;
    if (value instanceof Set) return `Set(${value.size})`;
    if (ArrayBuffer.isView(value)) return `${value.constructor.name}(${value.length})`;
    const name = value.constructor && value.constructor.name;
    return name || 'Object';
  };
  const preview = (value, depth, budget) => {
    if (budget.left-- <= 0) return { t: 'more' };
    switch (typeof value) {
      case 'string': return { t: 'string', v: cut(value, 2000) };
      case 'number': return { t: 'number', v: Object.is(value, -0) ? '-0' : String(value) };
      case 'boolean': return { t: 'boolean', v: String(value) };
      case 'undefined': return { t: 'undefined', v: 'undefined' };
      case 'bigint': return { t: 'bigint', v: `${value}n` };
      case 'symbol': return { t: 'symbol', v: String(value) };
      case 'function': return { t: 'function', v: `ƒ ${value.name || 'anonymous'}()` };
    }
    if (value === null) return { t: 'null', v: 'null' };
    try {
      if (value instanceof Error) {
        return { t: 'error', v: `${value.name}: ${value.message}`, stack: value.stack ? cut(String(value.stack), 4000) : undefined };
      }
      if (typeof Node !== 'undefined' && value instanceof Node) return { t: 'node', v: nodeName(value) };
      if (value instanceof Date) return { t: 'date', v: isNaN(value) ? 'Invalid Date' : value.toISOString() };
      if (value instanceof RegExp) return { t: 'regexp', v: String(value) };
      if (budget.path.has(value)) return { t: 'circular', v: label(value) };
      const kind = Array.isArray(value) || ArrayBuffer.isView(value) ? 'array' : 'object';
      if (depth >= 4) return { t: kind, v: label(value), entries: [], more: 1 };
      budget.path.add(value);
      const entries = [];
      let total = 0;
      const add = (key, item) => {
        total += 1;
        if (entries.length < 100) entries.push([key, preview(item, depth + 1, budget)]);
      };
      if (value instanceof Map) {
        for (const [key, item] of value) add(typeof key === 'string' ? key : show(key), item);
      } else if (value instanceof Set) {
        let i = 0;
        for (const item of value) add(String(i++), item);
      } else if (kind === 'array') {
        total = value.length;
        for (let i = 0; i < Math.min(value.length, 100); i++) entries.push([String(i), preview(value[i], depth + 1, budget)]);
      } else {
        for (const key of Object.keys(value)) {
          let item;
          try { item = value[key]; } catch (_) { item = '[could not be read]'; }
          add(key, item);
        }
      }
      budget.path.delete(value);
      return { t: kind, v: label(value), entries, more: total > entries.length ? total - entries.length : undefined };
    } catch (_) {
      return { t: 'object', v: Object.prototype.toString.call(value) };
    }
  };
  const previews = (args) => {
    const budget = { left: 500, path: new Set() };
    return args.map((arg) => preview(arg, 0, budget));
  };
  // The frames that called console: past this function, `send`, the
  // method's report and its wrapper.
  const callers = () => (new Error().stack || '').split('\n').slice(4);
  const where = (frame) => {
    const at = /([^@\s]+):(\d+):(\d+)$/.exec(frame || '');
    return at ? { source: at[1], line: Number(at[2]), column: Number(at[3]) } : {};
  };
  const send = (level, args) => {
    const frames = callers();
    post({
      type: 'console',
      level,
      text: cut(args.map(show).join(' ')),
      args: previews(args),
      stack: frames.length ? cut(frames.join('\n'), 8000) : undefined,
      ...where(frames[0]),
    });
  };
  // `send` is only ever called from a method's report, so every message
  // skips the same frames to where the page called console.
  const wrap = (name, report) => {
    const original = console[name];
    if (typeof original !== 'function') return;
    console[name] = function (...args) {
      report(args);
      return original.apply(this, args);
    };
  };
  for (const [name, level] of [
    ['log', 'log'], ['info', 'info'], ['warn', 'warn'], ['error', 'error'], ['debug', 'debug'], ['trace', 'debug'],
    ['table', 'log'], ['dir', 'log'], ['dirxml', 'log'], ['group', 'log'], ['groupCollapsed', 'log'],
  ]) {
    wrap(name, (args) => send(level, args.length || !name.startsWith('group') ? args : ['console.group']));
  }
  wrap('assert', ([condition, ...args]) => { if (!condition) send('error', ['Assertion failed:', ...args]); });
  wrap('clear', () => send('info', ['console.clear() was called']));
  const named = (args) => (args.length && args[0] !== undefined ? String(args[0]) : 'default');
  const counts = new Map();
  wrap('count', (args) => {
    const name = named(args);
    const count = (counts.get(name) || 0) + 1;
    counts.set(name, count);
    send('log', [`${name}: ${count}`]);
  });
  wrap('countReset', (args) => {
    const name = named(args);
    if (counts.has(name)) counts.set(name, 0);
    else send('warn', [`Count for '${name}' does not exist`]);
  });
  const timers = new Map();
  const elapsed = (name) => `${name}: ${(performance.now() - timers.get(name)).toFixed(3)}ms`;
  wrap('time', (args) => {
    const name = named(args);
    if (timers.has(name)) send('warn', [`Timer '${name}' already exists`]);
    else timers.set(name, performance.now());
  });
  wrap('timeLog', (args) => {
    const name = named(args);
    if (!timers.has(name)) send('warn', [`Timer '${name}' does not exist`]);
    else send('log', [elapsed(name), ...args.slice(1)]);
  });
  wrap('timeEnd', (args) => {
    const name = named(args);
    if (!timers.has(name)) send('warn', [`Timer '${name}' does not exist`]);
    else { send('log', [elapsed(name)]); timers.delete(name); }
  });
  window.addEventListener('error', (event) => {
    if (!(event instanceof ErrorEvent)) return;
    const stack = event.error && event.error.stack;
    // What WebKit gives the page of an error thrown by another origin's script.
    const muted = event.message === 'Script error.' && !event.filename;
    post({
      type: 'console',
      level: 'error',
      text: muted
        ? 'Script error. A script from another origin threw it; WebKit hides its message, file and line from the page unless the script is loaded with crossorigin and its server sends Access-Control-Allow-Origin.'
        : cut(event.message),
      args: event.error !== undefined ? previews([event.error]) : undefined,
      stack: stack ? cut(String(stack), 8000) : undefined,
      source: event.filename || undefined,
      line: event.lineno || undefined,
      column: event.colno || undefined,
    });
  });
  window.addEventListener('unhandledrejection', (event) => {
    const reason = event.reason;
    post({
      type: 'console',
      level: 'error',
      text: cut('Unhandled promise rejection: ' + show(reason)),
      args: previews([reason]),
      stack: reason && reason.stack ? cut(String(reason.stack), 8000) : undefined,
    });
  });
  // The rejection logged above found a handler after all.
  window.addEventListener('rejectionhandled', (event) => {
    post({ type: 'console', level: 'info', text: cut('Promise rejection handled later: ' + show(event.reason)) });
  });
  // An element's load failing (an image, script, stylesheet, media) fires a
  // plain event that does not bubble: only a listener capturing sees it.
  // Native code says why, from the request it saw, or that none went out.
  window.addEventListener('error', (event) => {
    if (event instanceof ErrorEvent) return;
    const target = event.target;
    if (!target || target === window || target.nodeType !== 1) return;
    const url = target.currentSrc || target.src || target.href || '';
    post({
      type: 'element',
      text: cut(`Failed to load resource: ${nodeName(target)}${url ? ' ' + url : ''}`),
      url: url || undefined,
      page: location.href,
    });
  }, true);
  // What WebKit's own console says when the page's Content Security Policy
  // blocks something; the page hears it as an event.
  document.addEventListener('securitypolicyviolation', (event) => {
    const report = event.disposition === 'report';
    const blocked = event.blockedURI || 'inline';
    post({
      type: 'console',
      level: report ? 'warn' : 'error',
      text: cut(`${report ? '[Report Only] ' : ''}Refused to load ${blocked} because it violates the Content Security Policy directive: "${event.violatedDirective}"`),
      source: event.sourceFile || undefined,
      line: event.lineNumber || undefined,
      column: event.columnNumber || undefined,
    });
  });
  // Deprecation and intervention notices, where this WebKit reports them.
  if (typeof ReportingObserver === 'function') {
    try {
      new ReportingObserver((reports) => {
        for (const report of reports) {
          const body = report.body || {};
          post({
            type: 'console',
            level: 'warn',
            text: cut(`[${report.type === 'deprecation' ? 'Deprecation' : 'Intervention'}] ${body.message || body.id || report.type}`),
            source: body.sourceFile || report.url || undefined,
            line: body.lineNumber || undefined,
            column: body.columnNumber || undefined,
          });
        }
      }, { types: ['deprecation', 'intervention'], buffered: true }).observe();
    } catch (_) {}
  }

  // How long each load's phases took, for the request the native side saw.
  // Another origin's load shows only its total unless it allows more
  // (Timing-Allow-Origin): its phases read as 0.
  if (typeof PerformanceObserver === 'function' && performance.timeOrigin) {
    const span = (from, to) => (from > 0 && to > from ? Math.round((to - from) * 10) / 10 : undefined);
    const timing = (e) => {
      // The document's entry can come before it has loaded, with no duration yet.
      const total = e.duration > 0 ? e.duration : Math.max(0, e.responseEnd - e.startTime);
      const t = { url: e.name, start: performance.timeOrigin + e.startTime, total: Math.round(total * 10) / 10 };
      if (!(e.requestStart > 0)) {
        t.restricted = true;
        return t;
      }
      t.redirect = span(e.redirectStart, e.redirectEnd);
      t.blocked = span(e.fetchStart, e.domainLookupStart || e.connectStart || e.requestStart);
      t.dns = span(e.domainLookupStart, e.domainLookupEnd);
      t.connect = span(e.connectStart, e.secureConnectionStart > 0 ? e.secureConnectionStart : e.connectEnd);
      t.tls = e.secureConnectionStart > 0 ? span(e.secureConnectionStart, e.connectEnd) : undefined;
      t.waiting = span(e.requestStart, e.responseStart);
      t.download = span(e.responseStart, e.responseEnd);
      t.protocol = e.nextHopProtocol || undefined;
      // All three read 0 where the sizes are not shared; 0 sent with a body is the cache.
      if (e.transferSize > 0 || e.decodedBodySize > 0) t.transferSize = e.transferSize;
      t.encodedSize = e.encodedBodySize || undefined;
      t.decodedSize = e.decodedBodySize || undefined;
      return t;
    };
    const report = (list) => {
      if (!handler()) return;
      const timings = list.getEntries().filter((e) => /^https?:/.test(e.name)).slice(0, 200).map(timing);
      if (timings.length) post({ type: 'timing', timings });
    };
    for (const type of ['navigation', 'resource']) {
      try { new PerformanceObserver(report).observe({ type, buffered: true }); } catch (_) {}
    }
  }

  // Messages of WebSockets and event streams, passed on in batches.
  const FRAME_MAX = 2048;
  const FRAMES_PER_BATCH = 100;
  let streams = 0;
  const stream = (kind, method, url) => {
    const id = `${now().toString(36)}.${Math.random().toString(36).slice(2, 8)}.${++streams}`;
    post({ type: 'stream', id, kind, method, url });
    return id;
  };
  const batches = new Map();
  const flush = (id) => {
    const batch = batches.get(id);
    if (!batch) return;
    batches.delete(id);
    clearTimer(batch.timer);
    post({ type: 'frames', id, frames: batch.frames, skipped: batch.skipped });
  };
  const frame = (id, sent, value, event) => {
    if (!handler()) return;
    let batch = batches.get(id);
    if (!batch) {
      batch = { frames: [], skipped: 0, timer: setTimer(() => flush(id), 500) };
      batches.set(id, batch);
    }
    if (batch.frames.length >= FRAMES_PER_BATCH) {
      batch.skipped += 1;
      return;
    }
    const item = { at: now(), sent };
    if (typeof value === 'string') {
      item.text = value.length > FRAME_MAX ? value.slice(0, FRAME_MAX) : value;
      if (value.length > FRAME_MAX) item.truncated = true;
    } else {
      item.size = value ? (value.byteLength !== undefined ? value.byteLength : value.size || 0) : 0;
    }
    if (event && event !== 'message') item.event = event;
    batch.frames.push(item);
  };
  const state = (id, value, extra) => {
    flush(id);
    post({ type: 'stream-state', id, state: value, ...extra });
  };

  const sockets = new WeakMap();
  const NativeSocket = window.WebSocket;
  if (typeof NativeSocket === 'function') {
    window.WebSocket = new Proxy(NativeSocket, {
      construct(target, args, newTarget) {
        const socket = Reflect.construct(target, args, newTarget);
        try {
          if (handler()) {
            const id = stream('websocket', 'GET', socket.url);
            sockets.set(socket, id);
            socket.addEventListener('open', () => state(id, 'open'));
            socket.addEventListener('message', (event) => frame(id, false, event.data));
            socket.addEventListener('error', () => state(id, 'error'));
            socket.addEventListener('close', (event) => state(id, 'closed', { code: event.code, reason: event.reason || undefined }));
          }
        } catch (_) {}
        return socket;
      },
    });
    const send = NativeSocket.prototype.send;
    NativeSocket.prototype.send = function (value) {
      try {
        const id = sockets.get(this);
        if (id) frame(id, true, value);
      } catch (_) {}
      return send.apply(this, arguments);
    };
  }

  // An event source is followed from each time it opens: a reconnect is a
  // request of its own. Named events are heard once the page listens for them.
  const sources = new WeakMap();
  const NativeSource = window.EventSource;
  if (typeof NativeSource === 'function') {
    const listen = NativeSource.prototype.addEventListener;
    const ended = (source, value) => {
      const watched = sources.get(source);
      if (!watched || !watched.id) return;
      state(watched.id, value);
      watched.id = null;
    };
    window.EventSource = new Proxy(NativeSource, {
      construct(target, args, newTarget) {
        const source = Reflect.construct(target, args, newTarget);
        try {
          if (handler()) {
            const watched = { id: null, types: new Set(['message', 'open', 'error']) };
            sources.set(source, watched);
            listen.call(source, 'open', () => { watched.id = stream('eventsource', 'GET', source.url); });
            listen.call(source, 'message', (event) => { if (watched.id) frame(watched.id, false, event.data); });
            listen.call(source, 'error', () => ended(source, source.readyState === 2 ? 'closed' : 'error'));
          }
        } catch (_) {}
        return source;
      },
    });
    NativeSource.prototype.addEventListener = function (type) {
      try {
        const watched = sources.get(this);
        const name = String(type);
        if (watched && !watched.types.has(name)) {
          watched.types.add(name);
          listen.call(this, name, (event) => { if (watched.id) frame(watched.id, false, event.data, name); });
        }
      } catch (_) {}
      return listen.apply(this, arguments);
    };
    const close = NativeSource.prototype.close;
    NativeSource.prototype.close = function () {
      try { ended(this, 'closed'); } catch (_) {}
      return close.apply(this, arguments);
    };
  }

  // The events of a `text/event-stream` response to `fetch`, as they arrive.
  const readEvents = async (id, response) => {
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';
    const event = (block) => {
      let name = 'message';
      const data = [];
      for (const line of block.split(/\r\n|\r|\n/)) {
        if (line.startsWith('data:')) data.push(line.slice(5).replace(/^ /, ''));
        else if (line.startsWith('event:')) name = line.slice(6).trim() || 'message';
      }
      if (data.length) frame(id, false, data.join('\n'), name);
    };
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done || !handler()) break;
        buffer += decoder.decode(value, { stream: true });
        let end;
        while ((end = buffer.search(/\r\n\r\n|\n\n|\r\r/)) >= 0) {
          event(buffer.slice(0, end));
          buffer = buffer.slice(end).replace(/^(\r\n\r\n|\n\n|\r\r)/, '');
        }
        // A stream that never ends an event is not kept whole.
        if (buffer.length > BODY_MAX) buffer = buffer.slice(-FRAME_MAX);
      }
    } catch (_) {
    } finally {
      try { reader.cancel(); } catch (_) {}
      state(id, 'closed');
    }
  };

  // Bodies of the page's own requests, for the request the native side saw.
  const TEXTUAL = /^(text\/|application\/(json|[\w.+-]*\+json|javascript|ecmascript|xml|[\w.+-]*\+xml|x-www-form-urlencoded|graphql))/i;
  const mime = (type) => (type || '').split(';')[0].trim() || undefined;
  const clipBody = (text) =>
    typeof text === 'string' ? { text: text.length > BODY_MAX ? text.slice(0, BODY_MAX) : text, truncated: text.length > BODY_MAX } : undefined;
  const sentBody = (body) => {
    if (typeof body === 'string') return body;
    if (typeof URLSearchParams !== 'undefined' && body instanceof URLSearchParams) return body.toString();
    if (typeof FormData !== 'undefined' && body instanceof FormData) {
      return [...body].map(([key, item]) => `${key}=${typeof item === 'string' ? item : `[file ${item.name || 'blob'}]`}`).join('&');
    }
    if (body instanceof ArrayBuffer || ArrayBuffer.isView(body)) {
      const bytes = body instanceof ArrayBuffer ? new Uint8Array(body) : new Uint8Array(body.buffer, body.byteOffset, body.byteLength);
      try {
        return new TextDecoder('utf-8', { fatal: true }).decode(bytes.subarray(0, BODY_MAX * 4));
      } catch (_) {
        return `[${bytes.length} bytes of binary data]`;
      }
    }
    if (typeof Blob !== 'undefined' && body instanceof Blob) return `[${body.type || 'blob'}, ${body.size} bytes]`;
    if (typeof ReadableStream !== 'undefined' && body instanceof ReadableStream) return '[a stream]';
    return undefined;
  };
  // A request that failed: native code logs why if it went out, or says it never did.
  const failed = (method, url, error) => {
    if (!handler() || (error && error.name === 'AbortError')) return;
    post({ type: 'failed', method, url, message: cut(String((error && error.message) || error || 'failed')), page: location.href });
  };
  const readCapped = async (response) => {
    const type = response.headers.get('content-type') || '';
    if (!TEXTUAL.test(type) || !response.body) return undefined;
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let text = '';
    let truncated = false;
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        text += decoder.decode(value, { stream: true });
        if (text.length > BODY_MAX) {
          truncated = true;
          text = text.slice(0, BODY_MAX);
          break;
        }
      }
    } finally {
      try { reader.cancel(); } catch (_) {}
    }
    if (!truncated) text += decoder.decode();
    return { text, truncated };
  };
  const fetch = window.fetch;
  if (typeof fetch === 'function') {
    const seen = (method, url, requestBody, response) => {
      const type = response.headers.get('content-type') || '';
      const at = response.url || url;
      if (/event-stream/i.test(type)) {
        let copy = null;
        try { copy = response.body ? response.clone() : null; } catch (_) {}
        post({ type: 'body', method, url: at, status: response.status, mimeType: mime(type), requestBody });
        if (copy) readEvents(stream('fetch', method, at), copy);
        return;
      }
      // A replacing stream would be read for as long as it runs.
      const readable = !/multipart\/x-mixed-replace/i.test(type);
      let copy = null;
      try { copy = readable ? response.clone() : null; } catch (_) {}
      (copy ? readCapped(copy) : Promise.resolve(undefined)).then(
        (responseBody) => post({ type: 'body', method, url: at, status: response.status, mimeType: mime(type), requestBody, responseBody }),
        () => {},
      );
    };
    window.fetch = function (input, init) {
      const promise = fetch.apply(this, arguments);
      let method, url, requestBody;
      try {
        const request = typeof Request !== 'undefined' && input instanceof Request ? input : null;
        method = String((init && init.method) || (request && request.method) || 'GET').toUpperCase();
        url = new URL(request ? request.url : String(input), location.href).href;
        requestBody = clipBody(sentBody(init && init.body));
      } catch (_) {
        return promise;
      }
      // The page gets a promise of its own: one it leaves unhandled is still
      // reported, as it would be without the console.
      return promise.then(
        (response) => {
          try { if (handler()) seen(method, url, requestBody, response); } catch (_) {}
          return response;
        },
        (error) => {
          try { failed(method, url, error); } catch (_) {}
          throw error;
        },
      );
    };
  }
  const XHR = typeof XMLHttpRequest !== 'undefined' && XMLHttpRequest.prototype;
  if (XHR) {
    const open = XHR.open;
    const sendXhr = XHR.send;
    XHR.open = function (method, url) {
      try { this[mark] = { method: String(method || 'GET').toUpperCase(), url: new URL(String(url), location.href).href }; } catch (_) {}
      return open.apply(this, arguments);
    };
    XHR.send = function (body) {
      const sent = this[mark];
      if (sent) {
        const requestBody = clipBody(sentBody(body));
        this.addEventListener('error', () => failed(sent.method, sent.url, 'XMLHttpRequest failed'), { once: true });
        this.addEventListener('loadend', () => {
          if (!handler()) return;
          const type = this.getResponseHeader('content-type') || '';
          let responseBody;
          try {
            if ((this.responseType === '' || this.responseType === 'text') && TEXTUAL.test(type)) responseBody = clipBody(this.responseText);
            else if (this.responseType === 'json' && this.response != null) responseBody = clipBody(JSON.stringify(this.response));
          } catch (_) {}
          post({ type: 'body', method: sent.method, url: this.responseURL || sent.url, status: this.status, mimeType: mime(type), requestBody, responseBody });
        }, { once: true });
      }
      return sendXhr.apply(this, arguments);
    };
  }
})();"#;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum Message {
    Console(ConsoleMessage),
    Body(BodyMessage),
    Element(ElementMessage),
    Failed(FailedMessage),
    Timing(TimingMessage),
    Stream(StreamMessage),
    Frames(FramesMessage),
    StreamState(StreamStateMessage),
}

/// An element whose load failed, as the page heard it.
#[derive(Deserialize)]
struct ElementMessage {
    text: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    page: Option<String>,
}

/// A `fetch` or XHR that failed, as the page heard it.
#[derive(Deserialize)]
struct FailedMessage {
    method: String,
    url: String,
    message: String,
    #[serde(default)]
    page: Option<String>,
}

#[derive(Deserialize)]
struct TimingMessage {
    timings: Vec<TimingEntry>,
}

#[derive(Deserialize)]
struct TimingEntry {
    url: String,
    /// When the load started, in ms since the epoch.
    start: f64,
    #[serde(flatten)]
    timing: Timing,
}

/// A WebSocket made, an event source opened, or a `fetch` answered with an
/// event stream; `id` is the page script's.
#[derive(Deserialize)]
struct StreamMessage {
    id: String,
    kind: String,
    method: String,
    url: String,
}

#[derive(Deserialize)]
struct FramesMessage {
    id: String,
    frames: Vec<FrameMessage>,
    #[serde(default)]
    skipped: u32,
}

#[derive(Deserialize)]
struct FrameMessage {
    at: i64,
    #[serde(default)]
    sent: bool,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    truncated: bool,
}

/// `open`, `error` or `closed`.
#[derive(Deserialize)]
struct StreamStateMessage {
    id: String,
    state: String,
    #[serde(default)]
    code: Option<u16>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
struct ConsoleMessage {
    level: Level,
    text: String,
    #[serde(default)]
    args: Vec<Value>,
    #[serde(default)]
    stack: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    column: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BodyMessage {
    method: String,
    url: String,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    request_body: Option<SentBody>,
    #[serde(default)]
    response_body: Option<SentBody>,
}

#[derive(Deserialize)]
struct SentBody {
    text: String,
    #[serde(default)]
    truncated: bool,
}

/// A request between its start and its end, and for a while after, so a
/// response body read by the page script can still join it.
struct Pending {
    key: String,
    hop: u32,
    entry: Entry,
    detail: Detail,
    /// The page script's body joined it.
    bodied: bool,
    /// Its Resource Timing joined it.
    timed: bool,
    /// A stream of the page script follows it.
    streamed: bool,
    /// What the page heard of an element whose load this was, before the
    /// load finished: said once it has.
    element: Option<String>,
}

impl Pending {
    fn new(key: String, hop: u32, entry: Entry, detail: Detail) -> Self {
        Self {
            key,
            hop,
            entry,
            detail,
            bodied: false,
            timed: false,
            streamed: false,
            element: None,
        }
    }

    fn put(&self, tab: &str) {
        devtools::put(
            tab,
            self.key.clone(),
            self.entry.clone(),
            Some(self.detail.clone()),
        );
    }
}

fn network_entry(method: String, url: String, resource_type: &str, started_at: i64) -> Entry {
    Entry::Network {
        method,
        url: clip(&url, URL_MAX),
        resource_type: resource_type.to_string(),
        status: None,
        mime_type: None,
        from_cache: false,
        started_at,
        ended_at: None,
        error: None,
        redirected_to: None,
        size: None,
        messages: None,
    }
}

/// One page's requests, shared by its delegate and its console handler.
#[derive(Default)]
struct Requests {
    pending: HashMap<u64, Pending>,
    recent: VecDeque<Pending>,
    /// WebSockets, which the delegate never sees.
    sockets: VecDeque<Pending>,
    /// The page script's streams, by its id, and the key of their request.
    streams: VecDeque<(String, String)>,
}

fn network_at<'a>(p: &'a Pending, url: &str) -> Option<&'a Entry> {
    matches!(&p.entry, Entry::Network { url: u, .. } if u == url).then_some(&p.entry)
}

impl Requests {
    fn finished(&mut self, p: Pending) {
        if self.recent.len() >= RECENT_MAX {
            self.recent.pop_front();
        }
        self.recent.push_back(p);
    }

    /// The latest request the delegate saw that `matches`: still going, or
    /// lately finished.
    fn latest(&mut self, matches: impl Fn(&Pending) -> bool) -> Option<&mut Pending> {
        if let Some(id) = self
            .pending
            .iter()
            .filter(|(_, p)| matches(p))
            .map(|(id, _)| *id)
            .max()
        {
            return self.pending.get_mut(&id);
        }
        self.recent.iter_mut().rev().find(|p| matches(p))
    }

    /// The latest request at `url` by `method` no body has joined yet.
    fn awaiting_body(&mut self, method: &str, url: &str) -> Option<&mut Pending> {
        self.latest(|p| {
            !p.bodied
                && matches!(&p.entry, Entry::Network { method: m, url: u, .. } if m.eq_ignore_ascii_case(method) && u == url)
        })
    }

    /// The request at `url` that started closest to `start` and has no timing.
    fn awaiting_timing(&mut self, url: &str, start: i64) -> Option<&mut Pending> {
        let distance = |p: &Pending| match network_at(p, url) {
            Some(Entry::Network { started_at, .. }) if !p.timed => Some((started_at - start).abs()),
            _ => None,
        };
        let pending = self
            .pending
            .iter()
            .filter_map(|(id, p)| distance(p).map(|d| (d, *id)))
            .min();
        let recent = self
            .recent
            .iter()
            .enumerate()
            .filter_map(|(i, p)| distance(p).map(|d| (d, i)))
            .min();
        match (pending, recent) {
            (Some((d, id)), r) if d <= TIMING_SLACK_MS && r.is_none_or(|(rd, _)| d <= rd) => {
                self.pending.get_mut(&id)
            }
            (_, Some((d, i))) if d <= TIMING_SLACK_MS => self.recent.get_mut(i),
            _ => None,
        }
    }

    /// The request a stream of the page script of `kind` (`fetch` or
    /// `eventsource`) follows, marked as followed: the oldest still going at
    /// that address with no stream yet, of the same type if there is one, as
    /// streams answer in the order they were asked.
    fn unstreamed(&mut self, kind: &str, method: &str, url: &str) -> Option<String> {
        let candidates = |same_type: bool| {
            self.pending
                .iter()
                .filter(|(_, p)| {
                    !p.streamed
                        && matches!(&p.entry, Entry::Network { method: m, url: u, resource_type, .. }
                            if m.eq_ignore_ascii_case(method) && u == url && (!same_type || resource_type == kind))
                })
                .map(|(id, _)| *id)
                .min()
        };
        let id = candidates(true).or_else(|| candidates(false))?;
        let p = self.pending.get_mut(&id)?;
        p.streamed = true;
        Some(p.key.clone())
    }

    fn by_key(&mut self, key: &str) -> Option<&mut Pending> {
        self.pending
            .values_mut()
            .chain(self.recent.iter_mut())
            .chain(self.sockets.iter_mut())
            .find(|p| p.key == key)
    }

    /// The request a stream of the page script follows.
    fn stream(&mut self, id: &str) -> Option<&mut Pending> {
        let key = self.streams.iter().find(|(i, _)| i == id)?.1.clone();
        self.by_key(&key)
    }

    fn follow(&mut self, id: String, key: String) {
        if self.streams.len() >= STREAMS_MAX {
            self.streams.pop_front();
        }
        self.streams.push_back((id, key));
    }
}

pub struct LoadIvars {
    tab: String,
    requests: Rc<RefCell<Requests>>,
}

pub struct ConsoleIvars {
    tab: String,
    requests: Rc<RefCell<Requests>>,
    /// The delegate sees this page's requests: one it did not see never went out.
    loads: bool,
    second: Cell<Option<Instant>>,
    sent: Cell<u32>,
    bodies: Cell<u32>,
    frames: Cell<u32>,
    dropped: Cell<u32>,
}

define_class!(
    /// WebKit's resource-load delegate for one page. Matched by selector:
    /// the protocol is private.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "wvDiagnosticsDevtoolsResourceLoads"]
    #[ivars = LoadIvars]
    pub struct ResourceLoads;

    impl ResourceLoads {
        #[unsafe(method(webView:resourceLoad:didSendRequest:))]
        fn did_send(&self, _web_view: &AnyObject, info: &AnyObject, request: &NSURLRequest) {
            self.started(info, request, 0);
        }

        #[unsafe(method(webView:resourceLoad:didCompleteWithError:response:))]
        fn did_complete(&self, web_view: &AnyObject, info: &AnyObject, error: Option<&NSError>, response: Option<&AnyObject>) {
            self.completed(web_view, info, error, response);
        }

        #[unsafe(method(webView:resourceLoad:didPerformHTTPRedirection:newRequest:))]
        fn did_redirect(&self, _web_view: &AnyObject, info: &AnyObject, response: &AnyObject, request: &NSURLRequest) {
            let id = load_id(info);
            let mut requests = self.ivars().requests.borrow_mut();
            let hop = requests.pending.remove(&id).map(|mut p| {
                set_response(&mut p, response);
                if let Entry::Network { redirected_to, .. } = &mut p.entry {
                    *redirected_to = Some(clip(&request_url(request).unwrap_or_default(), URL_MAX));
                }
                finish(&mut p.entry, event_time(info), None);
                devtools::put(&self.ivars().tab, p.key.clone(), p.entry.clone(), Some(p.detail.clone()));
                let hop = p.hop + 1;
                requests.finished(p);
                hop
            });
            drop(requests);
            self.started(info, request, hop.unwrap_or(1));
        }

        #[unsafe(method(webView:resourceLoad:didReceiveResponse:))]
        fn did_receive(&self, _web_view: &AnyObject, info: &AnyObject, response: &AnyObject) {
            let mut requests = self.ivars().requests.borrow_mut();
            let Some(p) = requests.pending.get_mut(&load_id(info)) else { return };
            set_response(p, response);
            p.put(&self.ivars().tab);
        }
    }

    unsafe impl NSObjectProtocol for ResourceLoads {}
);

define_class!(
    /// Receives the console script's messages for one page.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "wvDiagnosticsDevtoolsConsole"]
    #[ivars = ConsoleIvars]
    pub struct ConsoleHandler;

    unsafe impl NSObjectProtocol for ConsoleHandler {}

    unsafe impl WKScriptMessageHandler for ConsoleHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
            let body = unsafe { message.body() };
            let Some(text) = body.downcast_ref::<NSString>() else {
                return;
            };
            let text = text.to_string();
            if text.len() > MESSAGE_MAX {
                return;
            }
            match serde_json::from_str::<Message>(&text) {
                Ok(Message::Console(message)) => self.console(message),
                Ok(Message::Body(message)) => self.body(message),
                Ok(Message::Element(message)) => self.element(message),
                Ok(Message::Failed(message)) => self.failed(message),
                Ok(Message::Timing(message)) => self.timing(message),
                Ok(Message::Stream(message)) => self.stream(message),
                Ok(Message::Frames(message)) => self.frames(message),
                Ok(Message::StreamState(message)) => self.stream_state(message),
                Err(_) => {}
            }
        }
    }
);

impl ResourceLoads {
    fn new(mtm: MainThreadMarker, tab: &str, requests: Rc<RefCell<Requests>>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(LoadIvars {
            tab: tab.to_string(),
            requests,
        });
        unsafe { msg_send![super(this), init] }
    }

    fn started(&self, info: &AnyObject, request: &NSURLRequest, hop: u32) {
        let id = load_id(info);
        let url = request_url(request).unwrap_or_else(|| info_url(info));
        let method = request
            .HTTPMethod()
            .map(|m| m.to_string())
            .unwrap_or_else(|| info_method(info));
        let detail = request_detail(request);
        let kind = refined_type(info, &detail.request_headers);
        let p = Pending::new(
            devtools::network_key(id, hop),
            hop,
            network_entry(method, url, kind, event_time(info)),
            detail,
        );
        p.put(&self.ivars().tab);
        let mut requests = self.ivars().requests.borrow_mut();
        if requests.pending.len() >= PENDING_MAX
            && let Some(oldest) = requests.pending.keys().min().copied()
        {
            requests.pending.remove(&oldest);
        }
        requests.pending.insert(id, p);
    }

    fn completed(
        &self,
        web_view: &AnyObject,
        info: &AnyObject,
        error: Option<&NSError>,
        response: Option<&AnyObject>,
    ) {
        let id = load_id(info);
        let tab = &self.ivars().tab;
        let mut requests = self.ivars().requests.borrow_mut();
        // A load answered from the disk cache, or stopped at its CORS
        // preflight, can complete without starting.
        let mut p = requests.pending.remove(&id).unwrap_or_else(|| {
            Pending::new(
                devtools::network_key(id, 0),
                0,
                network_entry(
                    info_method(info),
                    info_url(info),
                    refined_type(info, &[]),
                    event_time(info),
                ),
                Detail::default(),
            )
        });
        if let Some(response) = response {
            set_response(&mut p, response);
        }
        if let Entry::Network { from_cache, .. } = &mut p.entry {
            *from_cache = loaded_from_cache(info);
        }
        finish(&mut p.entry, event_time(info), error.map(error_text));
        p.put(tab);
        let line =
            failure_line(&p).or_else(|| p.element.take().and_then(|text| element_note(&p, &text)));
        let refused_frame = refused_frame(&p);
        requests.finished(p);
        drop(requests);
        if let Some((text, url)) = line {
            put_console(tab, Level::Error, text, Some(url));
        }
        if let Some((url, agent)) = refused_frame
            && let Some(top) = page_url(web_view)
        {
            let _ = (tab, url, top, agent); // No extra network requests: the host owns frame-policy probing.
        }
    }
}

/// A console line written by native code, as WebKit's console would show it.
fn put_console(tab: &str, level: Level, text: String, source: Option<String>) {
    devtools::put(
        tab,
        devtools::console_key(),
        Entry::Console {
            level,
            text: clip(&text, TEXT_MAX),
            source: source.map(|s| clip(&s, URL_MAX)),
            line: None,
            column: None,
        },
        None,
    );
}

/// What WebKit's console says of a finished load that failed, or that the
/// server refused, and its address.
fn failure_line(p: &Pending) -> Option<(String, String)> {
    let Entry::Network {
        url, status, error, ..
    } = &p.entry
    else {
        return None;
    };
    match (error.as_deref(), status) {
        (Some("Cancelled"), _) => None,
        (Some(error), _) => Some((format!("Failed to load resource: {error}"), url.clone())),
        (None, Some(code)) if *code >= 400 => {
            let reason = http::StatusCode::from_u16(*code)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or("");
            Some((
                format!(
                    "Failed to load resource: the server responded with a status of {code} ({reason})"
                ),
                url.clone(),
            ))
        }
        _ => None,
    }
}

/// A frame's document cancelled before any response reached the page — how
/// WebKit refuses to frame one — with the user agent it was asked for with.
fn refused_frame(p: &Pending) -> Option<(String, Option<String>)> {
    let Entry::Network {
        url,
        resource_type,
        status,
        error,
        ..
    } = &p.entry
    else {
        return None;
    };
    (resource_type == "iframe" && status.is_none() && error.as_deref() == Some("Cancelled")).then(
        || {
            let agent = p
                .detail
                .request_headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("user-agent"))
                .map(|h| h.value.clone());
            (url.clone(), agent)
        },
    )
}

/// Why an element that loaded could not be used, in WebKit's words where it
/// has them; `None` when the load itself failed, which its own line says.
fn element_note(p: &Pending, text: &str) -> Option<(String, String)> {
    let Entry::Network {
        url,
        resource_type,
        status,
        error,
        mime_type,
        ..
    } = &p.entry
    else {
        return None;
    };
    if error.is_some() || status.is_some_and(|s| s >= 400) {
        return None;
    }
    let header = |name: &str| {
        p.detail
            .response_headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.trim().to_ascii_lowercase())
    };
    let nosniff = header("x-content-type-options").as_deref() == Some("nosniff");
    let mime = mime_type.as_deref().unwrap_or("").to_ascii_lowercase();
    let note = match resource_type.as_str() {
        "script" if nosniff && !is_script_mime(&mime) => format!(
            "Refused to execute {url} as script because \"X-Content-Type-Options: nosniff\" was given and its Content-Type is not a script MIME type."
        ),
        "script" if refused_script_mime(&mime) => {
            format!("Refused to execute {url} as script because {mime} is not a script MIME type.")
        }
        "stylesheet" if mime != "text/css" => format!(
            "Did not parse stylesheet at '{url}' because non CSS MIME types are not allowed{}.",
            if nosniff {
                " when 'X-Content-Type-Options: nosniff' is given"
            } else {
                " in strict mode"
            }
        ),
        _ => format!(
            "{text} — it arrived ({}{}) but could not be used.",
            status
                .map(|s| s.to_string())
                .unwrap_or_else(|| "no status".into()),
            mime_type
                .as_deref()
                .map(|m| format!(", {m}"))
                .unwrap_or_default()
        ),
    };
    Some((note, url.clone()))
}

fn is_script_mime(mime: &str) -> bool {
    matches!(
        mime,
        "text/javascript"
            | "application/javascript"
            | "application/x-javascript"
            | "application/ecmascript"
            | "text/ecmascript"
            | "module"
    ) || mime.ends_with("+javascript")
}

/// Types WebKit refuses to run as a script even without `nosniff`.
fn refused_script_mime(mime: &str) -> bool {
    mime.starts_with("image/")
        || mime.starts_with("audio/")
        || mime.starts_with("video/")
        || mime == "text/csv"
}

/// An `http:` load asked for by an `https:` page: blocked as insecure.
fn insecure(page: Option<&str>, url: &str) -> Option<String> {
    let page = page?;
    (page.starts_with("https:") && url.starts_with("http:")).then(|| {
        format!("[blocked] The page at {page} requested insecure content from {url}. This content was blocked and must be served over HTTPS.")
    })
}

fn page_url(web_view: &AnyObject) -> Option<String> {
    let url: Option<Retained<NSURL>> = unsafe { msg_send![web_view, URL] };
    url.and_then(|u| u.absoluteString()).map(|s| s.to_string())
}

impl ConsoleHandler {
    fn new(
        mtm: MainThreadMarker,
        tab: &str,
        requests: Rc<RefCell<Requests>>,
        loads: bool,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ConsoleIvars {
            tab: tab.to_string(),
            requests,
            loads,
            second: Cell::new(None),
            sent: Cell::new(0),
            bodies: Cell::new(0),
            frames: Cell::new(0),
            dropped: Cell::new(0),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn console(&self, message: ConsoleMessage) {
        self.new_second();
        let ivars = self.ivars();
        if ivars.sent.get() >= CONSOLE_PER_SECOND {
            ivars.dropped.set(ivars.dropped.get() + 1);
            return;
        }
        ivars.sent.set(ivars.sent.get() + 1);
        let detail = Detail {
            args: message.args,
            stack: message.stack.map(|s| clip(&s, STACK_MAX)),
            ..Detail::default()
        };
        devtools::put(
            &ivars.tab,
            devtools::console_key(),
            Entry::Console {
                level: message.level,
                text: clip(&message.text, TEXT_MAX),
                source: message.source.map(|s| clip(&s, URL_MAX)),
                line: message.line,
                column: message.column,
            },
            Some(detail),
        );
    }

    /// A body the page script read joins the request it belongs to.
    fn body(&self, message: BodyMessage) {
        self.new_second();
        let ivars = self.ivars();
        if ivars.bodies.get() >= BODIES_PER_SECOND {
            return;
        }
        ivars.bodies.set(ivars.bodies.get() + 1);
        let mut requests = ivars.requests.borrow_mut();
        let Some(p) = requests.awaiting_body(&message.method, &message.url) else {
            return;
        };
        p.bodied = true;
        if p.detail.request_body.is_none() {
            let sent_type = p
                .detail
                .request_headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-type"));
            p.detail.request_body = message
                .request_body
                .map(|b| devtools::body(&b.text, sent_type.map(|h| h.value.as_str()), b.truncated));
        }
        p.detail.response_body = message
            .response_body
            .map(|b| devtools::body(&b.text, message.mime_type.as_deref(), b.truncated));
        p.put(&ivars.tab);
    }

    /// A line of the console's own, held to the same rate as the page's.
    fn log(&self, text: String, source: Option<String>) {
        self.console(ConsoleMessage {
            level: Level::Error,
            text,
            args: Vec::new(),
            stack: None,
            source,
            line: None,
            column: None,
        });
    }

    /// An element's load failed: say why from the request the delegate saw,
    /// once it has finished, or that none went out.
    fn element(&self, message: ElementMessage) {
        let ElementMessage { text, url, page } = message;
        let Some(url) = url.filter(|u| u.starts_with("http:") || u.starts_with("https:")) else {
            return self.log(text, None);
        };
        if !self.ivars().loads {
            return self.log(text, Some(url));
        }
        let mut requests = self.ivars().requests.borrow_mut();
        let line = match requests.latest(|p| network_at(p, &url).is_some()) {
            Some(p) if matches!(p.entry, Entry::Network { ended_at: None, .. }) => {
                p.element = Some(text);
                return;
            }
            Some(p) => element_note(p, &text).map(|(note, _)| note),
            None => Some(insecure(page.as_deref(), &url).unwrap_or_else(|| {
                format!("{text} — it was blocked before it was requested, by a content blocker or the page's Content Security Policy.")
            })),
        };
        drop(requests);
        if let Some(line) = line {
            self.log(line, Some(url));
        }
    }

    /// A `fetch` or XHR failed. One that went out has its reason logged as it
    /// finished; one that never did is said here.
    fn failed(&self, message: FailedMessage) {
        let FailedMessage {
            method,
            url,
            message,
            page,
        } = message;
        if !self.ivars().loads || !(url.starts_with("http:") || url.starts_with("https:")) {
            return;
        }
        let went_out = self
            .ivars()
            .requests
            .borrow_mut()
            .latest(|p| matches!(&p.entry, Entry::Network { method: m, url: u, .. } if m.eq_ignore_ascii_case(&method) && *u == url))
            .is_some();
        if went_out {
            return;
        }
        let line = insecure(page.as_deref(), &url).unwrap_or_else(|| {
            format!("{method} {url} failed before it was sent ({message}): a content blocker or the page's Content Security Policy may have stopped it.")
        });
        self.log(line, Some(url));
    }

    /// The page's Resource Timing joins the requests it measured.
    fn timing(&self, message: TimingMessage) {
        let mut requests = self.ivars().requests.borrow_mut();
        for TimingEntry { url, start, timing } in message.timings {
            let Some(p) = requests.awaiting_timing(&url, start as i64) else {
                continue;
            };
            p.timed = true;
            if let Entry::Network { size, .. } = &mut p.entry {
                *size = timing.encoded_size;
            }
            p.detail.timing = Some(timing);
            p.put(&self.ivars().tab);
        }
    }

    /// A stream begins: a WebSocket as an entry of its own, an event stream
    /// with the request the delegate saw for it.
    fn stream(&self, message: StreamMessage) {
        let StreamMessage {
            id,
            kind,
            method,
            url,
        } = message;
        let tab = &self.ivars().tab;
        let mut requests = self.ivars().requests.borrow_mut();
        if kind != "websocket"
            && let Some(key) = requests.unstreamed(&kind, &method, &url)
        {
            requests.follow(id, key);
            return;
        }
        let resource_type = match kind.as_str() {
            "websocket" => "websocket",
            "fetch" => "fetch",
            _ => "eventsource",
        };
        let p = Pending::new(
            devtools::socket_key(),
            0,
            network_entry(method, url, resource_type, devtools::now_ms()),
            Detail::default(),
        );
        p.put(tab);
        requests.follow(id, p.key.clone());
        if requests.sockets.len() >= SOCKETS_MAX {
            requests.sockets.pop_front();
        }
        requests.sockets.push_back(p);
    }

    /// A batch of a stream's messages.
    fn frames(&self, message: FramesMessage) {
        self.new_second();
        let ivars = self.ivars();
        let over = ivars.frames.get() >= FRAMES_PER_SECOND;
        ivars.frames.set(ivars.frames.get() + 1);
        let mut requests = ivars.requests.borrow_mut();
        let Some(p) = requests.stream(&message.id) else {
            return;
        };
        let (messages, skipped) = if over {
            (Vec::new(), message.skipped + message.frames.len() as u32)
        } else {
            let messages = message
                .frames
                .into_iter()
                .map(|f| {
                    devtools::stream_message(
                        f.at,
                        f.sent,
                        f.text.as_deref(),
                        f.event,
                        f.size,
                        f.truncated,
                    )
                })
                .collect();
            (messages, message.skipped)
        };
        let total = devtools::add_messages(&mut p.detail, messages, skipped);
        if let Entry::Network { messages, .. } = &mut p.entry {
            *messages = Some(total);
        }
        if !over {
            p.put(&ivars.tab);
        }
    }

    /// A stream opened, failed or closed. Only a WebSocket's entry is the
    /// stream's own to update; the delegate finishes an event stream's.
    fn stream_state(&self, message: StreamStateMessage) {
        let tab = &self.ivars().tab;
        let mut requests = self.ivars().requests.borrow_mut();
        let Some(p) = requests.stream(&message.id) else {
            return;
        };
        let Entry::Network {
            url,
            resource_type,
            status,
            ended_at,
            error,
            ..
        } = &mut p.entry
        else {
            return;
        };
        if resource_type != "websocket" {
            // An event stream with no request the delegate saw ends here.
            if p.key.starts_with("s:") && message.state != "open" && ended_at.is_none() {
                *ended_at = Some(devtools::now_ms());
                p.put(tab);
            }
            return;
        }
        let failed_at = match message.state.as_str() {
            "open" => {
                *status = Some(101);
                None
            }
            "error" => {
                *error = Some("The WebSocket connection failed".into());
                ended_at.get_or_insert_with(devtools::now_ms);
                Some(url.clone())
            }
            _ => {
                ended_at.get_or_insert_with(devtools::now_ms);
                if error.is_none()
                    && let Some(code) = message.code.filter(|c| !matches!(c, 1000 | 1001 | 1005))
                {
                    *error = Some(match message.reason.filter(|r| !r.is_empty()) {
                        Some(reason) => {
                            format!("Closed with code {code}: {}", clip(&reason, 200))
                        }
                        None => format!("Closed with code {code}"),
                    });
                }
                None
            }
        };
        p.put(tab);
        drop(requests);
        if let Some(url) = failed_at {
            self.log(
                format!("WebSocket connection to '{url}' failed."),
                Some(url),
            );
        }
    }

    /// Opening a new second records how many messages the last one dropped.
    fn new_second(&self) {
        let ivars = self.ivars();
        let now = Instant::now();
        if ivars
            .second
            .get()
            .is_some_and(|start| now.duration_since(start) < Duration::from_secs(1))
        {
            return;
        }
        let dropped = ivars.dropped.replace(0);
        if dropped > 0 {
            devtools::put(
                &ivars.tab,
                devtools::console_key(),
                Entry::Console {
                    level: Level::Warn,
                    text: format!(
                        "{dropped} console messages not kept (more than {CONSOLE_PER_SECOND} a second)"
                    ),
                    source: None,
                    line: None,
                    column: None,
                },
                None,
            );
        }
        ivars.second.set(Some(now));
        ivars.sent.set(0);
        ivars.bodies.set(0);
        ivars.frames.set(0);
    }
}

/// What is attached to a page while its console is on.
struct Attached {
    loads: Option<Retained<ResourceLoads>>,
    _console: Retained<ConsoleHandler>,
}

thread_local! {
    /// WebKit holds the load delegate weakly; this keeps it.
    static ATTACHED: RefCell<HashMap<String, Attached>> = RefCell::new(HashMap::new());
    /// Pages whose view has the console script.
    static SCRIPTED: RefCell<std::collections::HashSet<String>> = RefCell::new(Default::default());
}

/// Capability detected by selector availability. Missing network support is explicit.
#[derive(Clone, Copy, Debug)]
pub struct CaptureCapabilities {
    pub native_network: bool,
}
/// Attach only on the main thread. The sink must return promptly and must not
/// re-enter attach/detach. Use a bounded host queue for storage.
pub fn attach(
    mtm: MainThreadMarker,
    view: &WKWebView,
    tab: &str,
    sink: std::rc::Rc<dyn Fn(crate::Record)>,
) -> CaptureCapabilities {
    devtools::register(tab, sink);
    if let Some(supported) = ATTACHED.with(|a| a.borrow().get(tab).map(|a| a.loads.is_some())) {
        return CaptureCapabilities {
            native_network: supported,
        };
    }
    let requests = Rc::new(RefCell::new(Requests::default()));
    let supported: bool =
        unsafe { msg_send![view, respondsToSelector: sel!(_setResourceLoadDelegate:)] };
    let loads = supported.then(|| {
        let loads = ResourceLoads::new(mtm, tab, requests.clone());
        let _: () = unsafe { msg_send![view, _setResourceLoadDelegate: &*loads] };
        loads
    });
    if !supported {
        log::warn!(
            "[browser-devtools] this WebKit has no resource-load delegate; the console lists no requests"
        );
    }
    let console = ConsoleHandler::new(mtm, tab, requests, supported);
    let world = unsafe { WKContentWorld::pageWorld(mtm) };
    let source = NSString::from_str(SCRIPT);
    unsafe {
        let controller = view.configuration().userContentController();
        controller.addScriptMessageHandler_contentWorld_name(
            ProtocolObject::from_ref(&*console),
            &world,
            &NSString::from_str(HANDLER),
        );
        if SCRIPTED.with(|s| s.borrow_mut().insert(tab.to_string())) {
            let script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly_inContentWorld(
                WKUserScript::alloc(mtm),
                &source,
                WKUserScriptInjectionTime::AtDocumentStart,
                false,
                &world,
            );
            controller.addUserScript(&script);
        }
        // The document already showing, from now on; its frames from their next load.
        view.evaluateJavaScript_inFrame_inContentWorld_completionHandler(
            &source, None, &world, None,
        );
    }
    ATTACHED.with(|a| {
        a.borrow_mut().insert(
            tab.to_string(),
            Attached {
                loads,
                _console: console,
            },
        )
    });
    CaptureCapabilities {
        native_network: supported,
    }
}

pub fn detach(mtm: MainThreadMarker, view: &WKWebView, tab: &str) {
    devtools::unregister(tab);
    let Some(attached) = ATTACHED.with(|a| a.borrow_mut().remove(tab)) else {
        return;
    };
    if attached.loads.is_some() {
        let _: () =
            unsafe { msg_send![view, _setResourceLoadDelegate: Option::<&AnyObject>::None] };
    }
    let world = unsafe { WKContentWorld::pageWorld(mtm) };
    unsafe {
        view.configuration()
            .userContentController()
            .removeScriptMessageHandlerForName_contentWorld(&NSString::from_str(HANDLER), &world);
    }
}

/// The page's view is gone, and its script with it.
pub fn unloaded(mtm: MainThreadMarker, view: &WKWebView, tab: &str) {
    detach(mtm, view, tab);
    SCRIPTED.with(|s| s.borrow_mut().remove(tab));
}

// =============================================================================
// Reading WebKit's load info, requests and responses
// =============================================================================

fn responds(object: &AnyObject, selector: objc2::runtime::Sel) -> bool {
    unsafe { msg_send![object, respondsToSelector: selector] }
}

fn load_id(info: &AnyObject) -> u64 {
    unsafe { msg_send![info, resourceLoadID] }
}

fn request_url(request: &NSURLRequest) -> Option<String> {
    request
        .URL()
        .and_then(|u| u.absoluteString())
        .map(|s| s.to_string())
}

fn info_url(info: &AnyObject) -> String {
    if !responds(info, sel!(originalURL)) {
        return String::new();
    }
    let url: Option<Retained<NSURL>> = unsafe { msg_send![info, originalURL] };
    url.and_then(|u| u.absoluteString())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn info_method(info: &AnyObject) -> String {
    if !responds(info, sel!(originalHTTPMethod)) {
        return "GET".into();
    }
    let method: Option<Retained<NSString>> = unsafe { msg_send![info, originalHTTPMethod] };
    method
        .map(|m| m.to_string())
        .unwrap_or_else(|| "GET".into())
}

/// When WebKit saw the event, in ms since the epoch; now where it does not say.
fn event_time(info: &AnyObject) -> i64 {
    if responds(info, sel!(eventTimestamp)) {
        let date: Option<Retained<NSDate>> = unsafe { msg_send![info, eventTimestamp] };
        if let Some(date) = date {
            return (date.timeIntervalSince1970() * 1000.0) as i64;
        }
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn loaded_from_cache(info: &AnyObject) -> bool {
    if !responds(info, sel!(loadedFromCache)) {
        return false;
    }
    let cached: Bool = unsafe { msg_send![info, loadedFromCache] };
    cached.as_bool()
}

/// `_WKResourceLoadInfoResourceType`, as the console names it.
fn resource_type(info: &AnyObject) -> &'static str {
    if !responds(info, sel!(resourceType)) {
        return "other";
    }
    let kind: isize = unsafe { msg_send![info, resourceType] };
    match kind {
        0 => "manifest",
        1 => "beacon",
        2 => "csp-report",
        3 => "document",
        4 => "image",
        5 => "fetch",
        6 => "font",
        7 => "media",
        8 => "object",
        9 => "ping",
        10 => "script",
        11 => "stylesheet",
        12 => "xhr",
        13 => "xslt",
        _ => "other",
    }
}

/// The resource type, told apart further than WebKit's: a frame's document,
/// and loads it calls `other` that the request says more of (`Sec-Fetch-Dest`
/// for a worker's script, `Accept` for an event source).
fn refined_type(info: &AnyObject, request_headers: &[Header]) -> &'static str {
    let header = |name: &str| {
        request_headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.to_ascii_lowercase())
    };
    match resource_type(info) {
        "document" if in_frame(info) => "iframe",
        "other" => match header("sec-fetch-dest").as_deref() {
            Some("worker") => "worker",
            Some("sharedworker") => "sharedworker",
            Some("serviceworker") => "serviceworker",
            _ if header("accept").is_some_and(|a| a.contains("text/event-stream")) => "eventsource",
            _ => "other",
        },
        kind => kind,
    }
}

/// The load is for a frame inside the page, not the page itself.
fn in_frame(info: &AnyObject) -> bool {
    if !responds(info, sel!(parentFrame)) {
        return false;
    }
    let parent: Option<Retained<AnyObject>> = unsafe { msg_send![info, parentFrame] };
    parent.is_some()
}

/// A document's, script's or stylesheet's body, read back from the page in
/// `tab` as text: the main document (`document`, with its MIME type) as it
/// arrived, the rest from the page's web archive. `None` when the page is not
/// loaded, no longer has it, or it is not text.
pub async fn archived_body(
    view: &WKWebView,
    url: &str,
    document: Option<&str>,
) -> Option<devtools::Body> {
    let (document, mime) = (document.is_some(), document.map(str::to_string));
    let address = url.to_string();
    let rx = {
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<Vec<u8>>>();
        let view = Some(view);
        let main = document
            && view
                .as_ref()
                .and_then(|v| unsafe { v.URL() })
                .and_then(|u| u.absoluteString())
                .is_some_and(|u| same_document(&u.to_string(), &address));
        let selector = if main {
            sel!(_getMainResourceDataWithCompletionHandler:)
        } else {
            sel!(createWebArchiveDataWithCompletionHandler:)
        };
        let Some(view) = view.filter(|v| (main || !document) && responds(v, selector)) else {
            let _ = tx.send(None);
            return None;
        };
        let tx = std::sync::Mutex::new(Some(tx));
        let block = RcBlock::new(move |data: *mut NSData, _error: *mut NSError| {
            let bytes = unsafe { data.as_ref() }.map(|d| d.to_vec());
            if let Some(tx) = tx.lock().ok().and_then(|mut t| t.take()) {
                let _ = tx.send(bytes);
            }
        });
        if main {
            let _: () =
                unsafe { msg_send![view, _getMainResourceDataWithCompletionHandler: &*block] };
        } else {
            let _: () =
                unsafe { msg_send![view, createWebArchiveDataWithCompletionHandler: &*block] };
        }
        rx
    };
    let bytes = tokio::time::timeout(ARCHIVE_TIMEOUT, rx)
        .await
        .ok()?
        .ok()??;
    let url = url.to_string();
    if document {
        return Some(text_body(&bytes, None, mime.as_deref()));
    }
    tokio::task::spawn_blocking(move || archived_resource(&bytes, &url))
        .await
        .ok()?
}

fn same_document(a: &str, b: &str) -> bool {
    let strip = |u: &str| u.split('#').next().unwrap_or(u).to_string();
    strip(a) == strip(b)
}

/// A subresource of a web archive by its address, in the page or any of its
/// frames. A frame's own document is its markup now, not what arrived, and
/// is not looked at.
fn archived_resource(bytes: &[u8], url: &str) -> Option<devtools::Body> {
    let archive = plist::Value::from_reader(std::io::Cursor::new(bytes)).ok()?;
    let resource = find_subresource(archive.as_dictionary()?, url)?;
    let field = |name: &str| resource.get(name).and_then(|v| v.as_string());
    let mime = field("WebResourceMIMEType");
    if !mime.is_some_and(devtools::textual) {
        return None;
    }
    let data = resource.get("WebResourceData")?.as_data()?;
    Some(text_body(data, field("WebResourceTextEncodingName"), mime))
}

fn find_subresource<'a>(
    archive: &'a plist::Dictionary,
    url: &str,
) -> Option<&'a plist::Dictionary> {
    let list = |name: &str| {
        archive
            .get(name)
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_dictionary())
    };
    list("WebSubresources")
        .find(|r| r.get("WebResourceURL").and_then(|v| v.as_string()) == Some(url))
        .or_else(|| list("WebSubframeArchives").find_map(|frame| find_subresource(frame, url)))
}

/// Bytes the page kept as a body: decoded by the encoding WebKit named
/// (UTF-8 unless it is a Latin one), cut to `BODY_MAX`.
fn text_body(bytes: &[u8], encoding: Option<&str>, mime: Option<&str>) -> devtools::Body {
    let latin = encoding.is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "iso-8859-1" | "latin1" | "windows-1252" | "us-ascii"
        )
    });
    let text: String = if latin {
        bytes.iter().map(|&b| b as char).collect()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    let truncated = text.chars().count() > BODY_MAX;
    let kept: String = text.chars().take(BODY_MAX).collect();
    devtools::body(&kept, mime, truncated)
}

/// A header dictionary as kept, by name.
fn headers_of(dict: &NSDictionary<AnyObject, AnyObject>) -> Vec<Header> {
    let mut headers: Vec<Header> = dict
        .allKeys()
        .iter()
        .filter_map(|key| {
            let name = key.downcast_ref::<NSString>()?.to_string();
            let value = dict.objectForKey(&key)?;
            let value = value
                .downcast_ref::<NSString>()
                .map(|v| v.to_string())
                .unwrap_or_default();
            Some(devtools::header(&name, &value))
        })
        .collect();
    headers.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });
    headers
}

/// What a request carries as it goes out: its headers, and its body when it
/// is in memory (not a stream or a file).
fn request_detail(request: &NSURLRequest) -> Detail {
    let headers: Option<Retained<NSDictionary<AnyObject, AnyObject>>> =
        unsafe { msg_send![request, allHTTPHeaderFields] };
    let request_headers = headers.map(|h| headers_of(&h)).unwrap_or_default();
    let request_body = request.HTTPBody().filter(|d| d.length() > 0).map(|data| {
        let bytes = data.to_vec();
        let content_type = request_headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("content-type"));
        match std::str::from_utf8(&bytes) {
            Ok(text) => devtools::body(
                &text.chars().take(BODY_MAX + 1).collect::<String>(),
                content_type.map(|h| h.value.as_str()),
                false,
            ),
            Err(_) => devtools::Body {
                text: format!("{} bytes of binary data", bytes.len()),
                mime_type: content_type.map(|h| h.value.clone()),
                truncated: false,
            },
        }
    });
    Detail {
        request_headers,
        request_body,
        ..Detail::default()
    }
}

fn set_response(p: &mut Pending, response: &AnyObject) {
    let Entry::Network {
        status, mime_type, ..
    } = &mut p.entry
    else {
        return;
    };
    if responds(response, sel!(statusCode)) {
        let code: isize = unsafe { msg_send![response, statusCode] };
        *status = u16::try_from(code).ok().filter(|c| *c > 0);
    }
    if responds(response, sel!(MIMEType)) {
        let mime: Option<Retained<NSString>> = unsafe { msg_send![response, MIMEType] };
        *mime_type = mime.map(|m| m.to_string());
    }
    if responds(response, sel!(allHeaderFields)) {
        let headers: Option<Retained<NSDictionary<AnyObject, AnyObject>>> =
            unsafe { msg_send![response, allHeaderFields] };
        if let Some(headers) = headers {
            p.detail.response_headers = headers_of(&headers);
        }
    }
}

fn finish(entry: &mut Entry, at: i64, failure: Option<String>) {
    if let Entry::Network {
        ended_at, error, ..
    } = entry
    {
        *ended_at = Some(at);
        if failure.is_some() {
            *error = failure;
        }
    }
}

fn error_text(error: &NSError) -> String {
    // NSURLErrorCancelled: the page or a navigation stopped it.
    if error.code() == -999 {
        return "Cancelled".into();
    }
    error.localizedDescription().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: u64, kind: &str, url: &str, started_at: i64) -> Pending {
        Pending::new(
            devtools::network_key(id, 0),
            0,
            network_entry("GET".into(), url.into(), kind, started_at),
            Detail::default(),
        )
    }

    fn answered(
        mut p: Pending,
        status: Option<u16>,
        error: Option<&str>,
        mime: &str,
        headers: &[(&str, &str)],
    ) -> Pending {
        if let Entry::Network {
            status: s,
            error: e,
            mime_type,
            ended_at,
            ..
        } = &mut p.entry
        {
            *s = status;
            *e = error.map(str::to_string);
            *mime_type = Some(mime.into());
            *ended_at = Some(1);
        }
        p.detail.response_headers = headers
            .iter()
            .map(|(n, v)| devtools::header(n, v))
            .collect();
        p
    }

    #[test]
    fn timing_joins_the_request_that_started_closest() {
        let mut requests = Requests::default();
        requests.finished(request(1, "script", "https://x.test/a.js", 1_000));
        requests
            .pending
            .insert(2, request(2, "script", "https://x.test/a.js", 5_000));
        let p = requests
            .awaiting_timing("https://x.test/a.js", 4_990)
            .unwrap();
        assert_eq!(p.key, devtools::network_key(2, 0));
        p.timed = true;
        let p = requests
            .awaiting_timing("https://x.test/a.js", 1_002)
            .unwrap();
        assert_eq!(p.key, devtools::network_key(1, 0));
        p.timed = true;
        assert!(
            requests
                .awaiting_timing("https://x.test/a.js", 1_000)
                .is_none()
        );
        assert!(
            requests
                .awaiting_timing("https://x.test/b.js", 5_000)
                .is_none()
        );
        requests
            .pending
            .insert(3, request(3, "script", "https://x.test/c.js", 0));
        assert!(
            requests
                .awaiting_timing("https://x.test/c.js", 10_000)
                .is_none()
        );
    }

    #[test]
    fn streams_follow_requests_in_the_order_they_were_asked() {
        let mut requests = Requests::default();
        let url = "https://x.test/sse";
        requests.pending.insert(4, request(4, "fetch", url, 0));
        requests
            .pending
            .insert(5, request(5, "eventsource", url, 0));
        requests.pending.insert(6, request(6, "fetch", url, 0));
        assert_eq!(
            requests.unstreamed("eventsource", "GET", url),
            Some(devtools::network_key(5, 0))
        );
        assert_eq!(
            requests.unstreamed("fetch", "GET", url),
            Some(devtools::network_key(4, 0))
        );
        assert_eq!(
            requests.unstreamed("eventsource", "GET", url),
            Some(devtools::network_key(6, 0))
        );
        assert_eq!(requests.unstreamed("fetch", "GET", url), None);

        requests.follow("a".into(), devtools::network_key(4, 0));
        assert_eq!(
            requests.stream("a").map(|p| p.key.clone()),
            Some(devtools::network_key(4, 0))
        );
        assert!(requests.stream("b").is_none());
    }

    #[test]
    fn a_failed_load_reads_as_in_webkit() {
        let cors = answered(
            request(1, "fetch", "https://api.test/x", 0),
            None,
            Some(
                "Origin https://app.test is not allowed by Access-Control-Allow-Origin. Status code: 200",
            ),
            "",
            &[],
        );
        assert_eq!(
            failure_line(&cors).map(|(t, _)| t).as_deref(),
            Some(
                "Failed to load resource: Origin https://app.test is not allowed by Access-Control-Allow-Origin. Status code: 200"
            )
        );
        let missing = answered(
            request(2, "image", "https://x.test/a.png", 0),
            Some(404),
            None,
            "text/html",
            &[],
        );
        assert_eq!(
            failure_line(&missing).map(|(t, _)| t).as_deref(),
            Some("Failed to load resource: the server responded with a status of 404 (Not Found)")
        );
        let cancelled = answered(
            request(3, "iframe", "https://bank.test/", 0),
            None,
            Some("Cancelled"),
            "",
            &[],
        );
        assert!(failure_line(&cancelled).is_none());
        assert_eq!(
            refused_frame(&cancelled).map(|(u, _)| u).as_deref(),
            Some("https://bank.test/")
        );
        assert!(refused_frame(&missing).is_none());
    }

    #[test]
    fn an_element_that_loaded_but_was_refused_says_why() {
        let text = "Failed to load resource: <script> https://cdn.test/a.js";
        let nosniff = answered(
            request(1, "script", "https://cdn.test/a.js", 0),
            Some(200),
            None,
            "text/plain",
            &[("X-Content-Type-Options", "nosniff")],
        );
        assert!(element_note(&nosniff, text).unwrap().0.starts_with(
            "Refused to execute https://cdn.test/a.js as script because \"X-Content-Type-Options: nosniff\""
        ));
        let style = answered(
            request(2, "stylesheet", "https://cdn.test/a.css", 0),
            Some(200),
            None,
            "text/html",
            &[],
        );
        assert!(
            element_note(&style, text)
                .unwrap()
                .0
                .contains("non CSS MIME types are not allowed in strict mode")
        );
        let image = answered(
            request(3, "image", "https://x.test/a.png", 0),
            Some(200),
            None,
            "image/png",
            &[],
        );
        assert!(
            element_note(
                &image,
                "Failed to load resource: <img> https://x.test/a.png"
            )
            .unwrap()
            .0
            .ends_with("it arrived (200, image/png) but could not be used.")
        );
        let failed = answered(
            request(4, "image", "https://x.test/b.png", 0),
            Some(404),
            None,
            "text/html",
            &[],
        );
        assert!(element_note(&failed, text).is_none());

        assert!(
            insecure(Some("https://app.test/"), "http://cdn.test/a.js")
                .is_some_and(|t| t.starts_with("[blocked]"))
        );
        assert!(insecure(Some("http://app.test/"), "http://cdn.test/a.js").is_none());
    }
}
