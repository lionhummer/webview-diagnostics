use crate::{BODY_MAX, HEADER_MAX, HIDDEN, clip};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::OnceLock;
/// Who turned a page's console on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OpenedBy {
    Person,
    Agent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Debug,
    Log,
    Info,
    Warn,
    Error,
}

/// One line of the console.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Entry {
    Console {
        level: Level,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        column: Option<u32>,
    },
    /// One request. Updated in place as it gets a response and finishes; a
    /// redirect finishes it and starts another.
    Network {
        method: String,
        url: String,
        /// `document`, `script`, `stylesheet`, `image`, `fetch`, `xhr`, … or `other`.
        resource_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(default)]
        from_cache: bool,
        started_at: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ended_at: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Where a redirect sent it; the next hop is an entry of its own.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        redirected_to: Option<String>,
        /// The response body's bytes as sent (compressed, if it was), from the
        /// page's Resource Timing; absent where the server did not allow it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
        /// Messages a WebSocket or an event stream carried so far.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        messages: Option<u32>,
    },
}

/// What an entry shows when opened.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Detail {
    /// A console message's arguments, as the page script previewed them:
    /// `{ t: "object", entries: [[key, preview]], more }` and so on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<Value>,
    /// Where a console message was logged from, or an error's stack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub request_headers: Vec<Header>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub response_headers: Vec<Header>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_body: Option<Body>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<Body>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<Timing>,
    /// A WebSocket's or an event stream's newest [`MESSAGES_KEPT`] messages,
    /// oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<StreamMessage>,
    /// Messages let go: older than the newest [`MESSAGES_KEPT`], or more than
    /// the page script passes on at once.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub messages_dropped: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// How long a request's phases took, in ms, as the page's Resource Timing
/// measured them. A phase is absent when it did not happen (a connection
/// reused, nothing redirected), and all of them when the request went to
/// another origin that does not send `Timing-Allow-Origin`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Timing {
    pub total: f64,
    /// Only `total` is known: the server did not allow the rest to be seen.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restricted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect: Option<f64>,
    /// Waiting to start: queued behind other requests, or for a cache lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<f64>,
    /// Opening the connection, before TLS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<f64>,
    /// From sending the request to the first byte of the response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download: Option<f64>,
    /// `http/1.1`, `h2`, `h3`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// Bytes over the network, headers included; 0 from cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_size: Option<u64>,
    /// The body as sent, and as decoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoded_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decoded_size: Option<u64>,
}

/// One message a WebSocket sent or received, or one event of an event stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamMessage {
    pub at: i64,
    /// The page sent it; otherwise it arrived.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sent: bool,
    /// Its text, secret-looking JSON fields hidden, at most [`MESSAGE_TEXT_MAX`]
    /// characters; empty for binary.
    pub text: String,
    /// An event stream's event name, when not `message`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    /// A binary message's size in bytes; its content is not kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Messages a stream keeps, and the longest text kept of one.
pub const MESSAGES_KEPT: usize = 200;
pub const MESSAGE_TEXT_MAX: usize = 2 * 1024;

/// A stream message as kept: secret-looking JSON fields hidden, then cut.
pub fn stream_message(
    at: i64,
    sent: bool,
    text: Option<&str>,
    event: Option<String>,
    binary: Option<u64>,
    truncated: bool,
) -> StreamMessage {
    let kept = text
        .map(|t| {
            let trimmed = t.trim_start();
            if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
                return t.to_string();
            }
            match serde_json::from_str::<Value>(t) {
                Ok(mut value) => {
                    redact_json(&mut value);
                    serde_json::to_string(&value).unwrap_or_default()
                }
                Err(_) => redact_json_text(t),
            }
        })
        .unwrap_or_default();
    let cut = kept.chars().count() > MESSAGE_TEXT_MAX;
    StreamMessage {
        at,
        sent,
        text: if cut {
            clip(&kept, MESSAGE_TEXT_MAX)
        } else {
            kept
        },
        event,
        binary: if text.is_some() {
            None
        } else {
            Some(binary.unwrap_or(0))
        },
        truncated: truncated || cut,
    }
}

/// Add `messages` to a stream's detail, keeping the newest [`MESSAGES_KEPT`].
/// Returns how many the stream has carried in all.
pub fn add_messages(detail: &mut Detail, messages: Vec<StreamMessage>, skipped: u32) -> u32 {
    detail.messages.extend(messages);
    detail.messages_dropped = detail.messages_dropped.saturating_add(skipped);
    if detail.messages.len() > MESSAGES_KEPT {
        let over = detail.messages.len() - MESSAGES_KEPT;
        detail.messages.drain(..over);
        detail.messages_dropped = detail.messages_dropped.saturating_add(over as u32);
    }
    (detail.messages.len() as u32).saturating_add(detail.messages_dropped)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Header {
    pub name: String,
    pub value: String,
    /// The value was not kept: it carries a sign-in or a secret.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Body {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Only the first [`BODY_MAX`] characters were kept.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Header names whose values are never kept, and words that mark one.
const HIDDEN_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
];
const SECRET_HEADER_WORDS: &[&str] = &[
    "token",
    "secret",
    "password",
    "session",
    "auth",
    "api-key",
    "apikey",
    "csrf",
    "xsrf",
    "signature",
    "credential",
];

/// A header as kept: its value hidden when it carries a sign-in or a secret.
pub fn header(name: &str, value: &str) -> Header {
    let lower = name.to_ascii_lowercase();
    let hidden = HIDDEN_HEADERS.contains(&lower.as_str())
        || SECRET_HEADER_WORDS.iter().any(|w| lower.contains(w));
    Header {
        name: name.to_string(),
        value: if hidden {
            String::new()
        } else {
            clip(value, HEADER_MAX)
        },
        hidden,
    }
}

/// Field names in a body whose values are never kept: whole words for short
/// ones, which would otherwise match inside ordinary names.
const SECRET_FIELD_WORDS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "authorization",
    "cookie",
    "session",
    "credential",
    "cardnumber",
    "privatekey",
];
const SECRET_FIELD_NAMES: &[&str] = &["pwd", "pin", "otp", "cvc", "cvv", "ssn", "code_verifier"];

fn secret_field(name: &str) -> bool {
    let squashed: String = name
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    SECRET_FIELD_WORDS.iter().any(|w| squashed.contains(w))
        || SECRET_FIELD_NAMES.contains(&name.to_ascii_lowercase().as_str())
}

/// A body as kept: secret-looking fields of JSON and form bodies hidden, then
/// cut to [`BODY_MAX`]. `truncated` says the page script already cut it.
pub fn body(text: &str, mime_type: Option<&str>, truncated: bool) -> Body {
    let redacted = redact_body(text, mime_type);
    let cut = redacted.chars().count() > BODY_MAX;
    Body {
        text: if cut {
            clip(&redacted, BODY_MAX)
        } else {
            redacted
        },
        mime_type: mime_type.map(str::to_string),
        truncated: truncated || cut,
    }
}

fn redact_body(text: &str, mime_type: Option<&str>) -> String {
    let mime = mime_type.unwrap_or("").to_ascii_lowercase();
    let trimmed = text.trim_start();
    if mime.contains("json") || trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(mut value) = serde_json::from_str::<Value>(text) {
            redact_json(&mut value);
            return serde_json::to_string_pretty(&value).unwrap_or_default();
        }
        // Cut short, or not JSON after all: hide `"key": value` pairs by pattern.
        return redact_json_text(text);
    }
    if mime.contains("x-www-form-urlencoded") || (mime.is_empty() && looks_like_form(trimmed)) {
        return url::form_urlencoded::parse(text.as_bytes())
            .map(|(k, v)| {
                let v = if secret_field(&k) { HIDDEN.into() } else { v };
                format!("{k}={v}")
            })
            .collect::<Vec<_>>()
            .join("&");
    }
    text.to_string()
}

fn looks_like_form(text: &str) -> bool {
    !text.is_empty()
        && !text.contains(char::is_whitespace)
        && text.split('&').all(|pair| pair.contains('='))
}

fn redact_json(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, field) in map.iter_mut() {
                if secret_field(key) {
                    *field = Value::String(HIDDEN.into());
                } else {
                    redact_json(field);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_json),
        _ => {}
    }
}

fn redact_json_text(text: &str) -> String {
    static PAIR: OnceLock<Option<regex::Regex>> = OnceLock::new();
    let pair = PAIR.get_or_init(|| {
        regex::Regex::new(r#""([^"\\]{1,64})"\s*:\s*("(?:[^"\\]|\\.)*"?|[^,}\]\s]+)"#).ok()
    });
    let Some(pair) = pair.as_ref() else {
        return HIDDEN.into();
    };
    pair.replace_all(text, |caps: &regex::Captures| {
        if secret_field(&caps[1]) {
            format!("\"{}\": \"{HIDDEN}\"", &caps[1])
        } else {
            caps[0].to_string()
        }
    })
    .into_owned()
}

/// An entry as stored: `seq` orders a page's entries and stays the same when
/// an entry is updated.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredEntry {
    pub seq: i64,
    pub at: i64,
    #[serde(flatten)]
    pub entry: Entry,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DevtoolsEvent {
    pub tab_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// With `enabled`: whether the agent turned it on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<bool>,
    /// Everything before `entries` is gone.
    pub cleared: bool,
    pub entries: Vec<StoredEntry>,
}

/// The console as it opens: whether it is on, and the newest entries, oldest first.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DevtoolsState {
    pub enabled: bool,
    /// Whether the agent turned it on.
    pub agent: bool,
    pub entries: Vec<StoredEntry>,
}

/// An entry opened: its row and what it shows.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenedEntry {
    #[serde(flatten)]
    pub entry: StoredEntry,
    /// `None` once the page's detail budget let it go.
    pub detail: Option<Detail>,
}
