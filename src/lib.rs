#![doc = include_str!("../README.md")]
mod journal;
mod model;
pub use journal::*;
pub use model::*;
#[cfg(all(feature = "webkit", target_os = "macos"))]
mod capture_bridge;
#[cfg(all(feature = "webkit", target_os = "macos"))]
pub mod webkit;
pub const TEXT_MAX: usize = 8192;
pub const URL_MAX: usize = 4096;
pub const HEADER_MAX: usize = 4096;
pub const BODY_MAX: usize = 65536;
pub const HIDDEN: &str = "[hidden]";
pub fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}
