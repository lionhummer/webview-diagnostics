use webview_diagnostics::*;
fn entry(text: &str) -> Entry {
    Entry::Console {
        level: Level::Log,
        text: text.into(),
        source: None,
        line: None,
        column: None,
    }
}
fn record(page: &str, key: &str, text: &str) -> Record {
    Record {
        page: page.into(),
        key: key.into(),
        at: 1,
        entry: entry(text),
        detail: None,
    }
}
fn journal() -> Journal {
    Journal::in_memory(Limits::default()).unwrap()
}
#[test]
fn reading_does_not_start_capture_and_agent_cannot_disable_person() {
    let mut j = journal();
    assert!(j.query("a", &Query::default()).unwrap().is_empty());
    assert!(matches!(
        j.record(record("a", "1", "x")),
        Err(Error::Disabled)
    ));
    j.set_capture("a", OpenedBy::Person, true, 1).unwrap();
    assert!(matches!(
        j.set_capture("a", OpenedBy::Agent, false, 2),
        Err(Error::Ownership)
    ));
    j.set_capture("a", OpenedBy::Agent, true, 2).unwrap();
    assert_eq!(j.owner("a").unwrap(), Some(OpenedBy::Person));
    j.set_capture("a", OpenedBy::Person, false, 3).unwrap();
    assert_eq!(j.owner("a").unwrap(), None);
}
#[test]
fn updates_keep_sequence_and_reads_and_clears_are_page_scoped() {
    let mut j = journal();
    for p in ["a", "b"] {
        j.set_capture(p, OpenedBy::Person, true, 0).unwrap();
    }
    let seq = j.record(record("a", "same", "initial")).unwrap();
    assert_eq!(j.record(record("a", "same", "updated")).unwrap(), seq);
    j.record(record("b", "same", "other")).unwrap();
    assert!(j.open_entry("b", seq).unwrap().is_none());
    assert_eq!(j.query("a", &Query::default()).unwrap().len(), 1);
    j.clear("a").unwrap();
    assert_eq!(j.query("b", &Query::default()).unwrap().len(), 1);
}
#[test]
fn record_batch_rolls_back_if_any_page_is_disabled() {
    let mut j = journal();
    j.set_capture("a", OpenedBy::Person, true, 0).unwrap();
    assert!(matches!(
        j.record_many(vec![
            record("a", "1", "allowed"),
            record("b", "1", "disabled")
        ]),
        Err(Error::Disabled)
    ));
    assert!(j.query("a", &Query::default()).unwrap().is_empty());
}
#[test]
fn count_and_detail_retention_are_independent() {
    let mut j = Journal::in_memory(Limits {
        entries_per_page: 3,
        detail_bytes_per_page: 100,
        ..Limits::default()
    })
    .unwrap();
    j.set_capture("a", OpenedBy::Person, true, 0).unwrap();
    let ids = (0..10)
        .map(|i| {
            let mut r = record("a", &i.to_string(), &i.to_string());
            r.detail = Some(Detail {
                stack: Some("x".repeat(70)),
                ..Default::default()
            });
            j.record(r).unwrap()
        })
        .collect::<Vec<_>>();
    let rows = j.query("a", &Query::default()).unwrap();
    assert_eq!(rows.iter().map(|r| r.seq).collect::<Vec<_>>(), ids[7..]);
    let details = rows
        .iter()
        .filter_map(|r| j.open_entry("a", r.seq).unwrap().unwrap().detail)
        .collect::<Vec<_>>();
    assert_eq!(details.len(), 1);
    assert!(serde_json::to_vec(&details[0]).unwrap().len() <= 100);
}
#[test]
fn structured_credentials_are_removed_before_storage() {
    let mut j = journal();
    j.set_capture("a", OpenedBy::Person, true, 0).unwrap();
    let mut r = record("a", "1", "safe console text");
    r.detail = Some(Detail {
        request_headers: vec![Header {
            name: "Authorization".into(),
            value: "secret-value".into(),
            hidden: false,
        }],
        request_body: Some(Body {
            text: r#"{"nested":{"api_key":"secret-value"},"safe":1}"#.into(),
            mime_type: Some("application/json".into()),
            truncated: false,
        }),
        args: vec![serde_json::json!({"unrestricted":"secret-value"})],
        messages: vec![stream_message(
            0,
            false,
            Some(r#"{"token":"secret-value"}"#),
            None,
            None,
            false,
        )],
        ..Default::default()
    });
    let seq = j.record(r).unwrap();
    let stored = serde_json::to_string(&j.open_entry("a", seq).unwrap()).unwrap();
    assert!(!stored.contains("secret-value"));
    assert!(stored.contains("safe"));
}
#[test]
fn unicode_limits_and_malformed_json_redaction() {
    assert_eq!(clip("αβγ", 2), "αβ");
    let b = body(r#"{"password":"sensitive""#, Some("application/json"), true);
    assert!(!b.text.contains("sensitive"));
    assert!(b.truncated);
    let b = body(
        "password=sensitive&name=alice",
        Some("application/x-www-form-urlencoded"),
        false,
    );
    assert!(!b.text.contains("sensitive"));
    assert!(b.text.contains("alice"));
    assert!(header("X-API-KEY", "sensitive").hidden);
}
#[test]
fn expired_agent_capture_does_not_expire_person_capture() {
    let mut j = journal();
    j.set_capture("agent", OpenedBy::Agent, true, 0).unwrap();
    j.set_capture("person", OpenedBy::Person, true, 0).unwrap();
    j.touch_agent("agent", 10).unwrap();
    assert_eq!(j.expire_agents(5).unwrap(), 0);
    assert_eq!(j.expire_agents(11).unwrap(), 1);
    assert_eq!(j.owner("person").unwrap(), Some(OpenedBy::Person));
    assert_eq!(j.prune_closed(5).unwrap(), 0);
    assert_eq!(j.prune_closed(11).unwrap(), 1);
}
#[test]
fn page_capacity_and_zero_limits_are_explicit() {
    assert!(matches!(
        Journal::in_memory(Limits {
            max_pages: 0,
            ..Default::default()
        }),
        Err(Error::Invalid)
    ));
    let mut j = Journal::in_memory(Limits {
        max_pages: 1,
        ..Default::default()
    })
    .unwrap();
    j.set_capture("a", OpenedBy::Person, true, 0).unwrap();
    assert!(matches!(
        j.set_capture("b", OpenedBy::Person, true, 0),
        Err(Error::Capacity)
    ));
}
#[test]
fn stream_retains_newest_messages_and_counts_discarded() {
    let mut d = Detail::default();
    let m = (0..300)
        .map(|i| stream_message(i, false, Some("x"), None, None, false))
        .collect();
    assert_eq!(add_messages(&mut d, m, 2), 302);
    assert_eq!(d.messages.len(), 200);
    assert_eq!(d.messages[0].at, 100);
    assert_eq!(d.messages_dropped, 102);
}
#[test]
fn known_url_credentials_are_hidden_in_the_summary() {
    let mut j = journal();
    j.set_capture("a", OpenedBy::Person, true, 0).unwrap();
    let r = Record {
        page: "a".into(),
        key: "1".into(),
        at: 0,
        entry: Entry::Network {
            method: "GET".into(),
            url: "https://user:sensitive@example.test/?token=sensitive&safe=yes#sensitive".into(),
            resource_type: "fetch".into(),
            status: Some(200),
            mime_type: None,
            from_cache: false,
            started_at: 0,
            ended_at: Some(1),
            error: None,
            redirected_to: None,
            size: None,
            messages: None,
        },
        detail: None,
    };
    let seq = j.record(r).unwrap();
    let stored = serde_json::to_string(&j.open_entry("a", seq).unwrap()).unwrap();
    assert!(!stored.contains("sensitive"));
    assert!(stored.contains("safe=yes"));
}
