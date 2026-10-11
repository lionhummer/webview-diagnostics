pub use crate::{Body, add_messages, body, header, now_ms, stream_message};
use crate::{Detail, Entry, Record};
use std::{cell::RefCell, collections::HashMap, rc::Rc};
type Sink = Rc<dyn Fn(Record)>;
thread_local! {static SINKS:RefCell<HashMap<String,Sink>>=RefCell::new(HashMap::new());}
pub fn register(page: &str, sink: Sink) {
    SINKS.with(|s| {
        s.borrow_mut().insert(page.into(), sink);
    });
}
pub fn unregister(page: &str) {
    SINKS.with(|s| {
        s.borrow_mut().remove(page);
    });
}
pub fn put(page: &str, key: String, entry: Entry, detail: Option<Detail>) {
    let sink = SINKS.with(|s| s.borrow().get(page).cloned());
    if let Some(sink) = sink {
        sink(Record {
            page: page.into(),
            key,
            at: now_ms(),
            entry,
            detail,
        });
    }
}
pub fn console_key() -> String {
    format!("console:{}", uuid::Uuid::now_v7())
}
pub fn socket_key() -> String {
    format!("socket:{}", uuid::Uuid::now_v7())
}
pub fn network_key(id: u64, hop: u32) -> String {
    format!("network:{}:{id}:{hop}", run_id())
}
fn run_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| uuid::Uuid::now_v7().to_string())
}
pub fn textual(mime: &str) -> bool {
    mime.starts_with("text/")
        || ["json", "javascript", "xml", "x-www-form-urlencoded"]
            .iter()
            .any(|s| mime.contains(s))
}
