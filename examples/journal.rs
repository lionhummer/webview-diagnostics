use webview_diagnostics::*;
fn main() -> Result<()> {
    let mut journal = Journal::in_memory(Limits::default())?;
    journal.set_capture("demo", OpenedBy::Person, true, now_ms())?;
    let seq = journal.record(Record {
        page: "demo".into(),
        key: "log-1".into(),
        at: now_ms(),
        entry: Entry::Console {
            level: Level::Info,
            text: "Hello from an embedded page".into(),
            source: None,
            line: None,
            column: None,
        },
        detail: None,
    })?;
    println!(
        "{}",
        serde_json::to_string_pretty(&journal.open_entry("demo", seq)?)?
    );
    Ok(())
}
