use crate::{
    Body, Detail, Entry, OpenedBy, OpenedEntry, StoredEntry, TEXT_MAX, URL_MAX, body, clip, header,
    stream_message,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid page, key or journal limits")]
    Invalid,
    #[error("capture is disabled for this page")]
    Disabled,
    #[error("an agent cannot disable capture opened by a person")]
    Ownership,
    #[error("page capacity exhausted")]
    Capacity,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Debug)]
pub struct Limits {
    pub entries_per_page: usize,
    pub detail_bytes_per_page: usize,
    pub max_pages: usize,
    pub max_batch: usize,
    /// Console previews may contain arbitrary values. Opt in explicitly.
    pub keep_console_arguments: bool,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            entries_per_page: 5000,
            detail_bytes_per_page: 16 * 1024 * 1024,
            max_pages: 512,
            max_batch: 2000,
            keep_console_arguments: false,
        }
    }
}
pub struct Record {
    pub page: String,
    pub key: String,
    pub at: i64,
    pub entry: Entry,
    pub detail: Option<Detail>,
}
#[derive(Default)]
pub struct Query {
    pub after: i64,
    pub limit: usize,
    pub text: Option<String>,
    pub network_only: bool,
}
/// Synchronous, single-connection journal. Run writes on a bounded host worker.
pub struct Journal {
    conn: Connection,
    limits: Limits,
}
impl Journal {
    pub fn open(path: impl AsRef<Path>, limits: Limits) -> Result<Self> {
        Self::from_connection(Connection::open(path)?, limits)
    }
    pub fn in_memory(limits: Limits) -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?, limits)
    }
    fn from_connection(conn: Connection, limits: Limits) -> Result<Self> {
        if [
            limits.entries_per_page,
            limits.detail_bytes_per_page,
            limits.max_pages,
            limits.max_batch,
        ]
        .contains(&0)
            || [
                limits.entries_per_page,
                limits.detail_bytes_per_page,
                limits.max_pages,
                limits.max_batch,
            ]
            .iter()
            .any(|v| *v > i64::MAX as usize)
        {
            return Err(Error::Invalid);
        }
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
          CREATE TABLE IF NOT EXISTS diagnostics_pages_v1(page TEXT PRIMARY KEY,owner TEXT, touched INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS diagnostics_entries_v1(seq INTEGER PRIMARY KEY AUTOINCREMENT,page TEXT NOT NULL REFERENCES diagnostics_pages_v1(page) ON DELETE CASCADE,key TEXT NOT NULL,at INTEGER NOT NULL,entry TEXT NOT NULL,detail TEXT,bytes INTEGER NOT NULL,UNIQUE(page,key));
          CREATE INDEX IF NOT EXISTS diagnostics_page_seq_v1 ON diagnostics_entries_v1(page,seq);")?;
        Ok(Self { conn, limits })
    }
    pub fn owner(&self, page: &str) -> Result<Option<OpenedBy>> {
        let owner: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT owner FROM diagnostics_pages_v1 WHERE page=?",
                [page],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match owner.flatten().as_deref() {
            Some("person") => Some(OpenedBy::Person),
            Some("agent") => Some(OpenedBy::Agent),
            _ => None,
        })
    }
    pub fn set_capture(
        &mut self,
        page: &str,
        actor: OpenedBy,
        enabled: bool,
        at: i64,
    ) -> Result<()> {
        if page.trim().is_empty() || page.len() > 512 {
            return Err(Error::Invalid);
        }
        let tx = self.conn.transaction()?;
        let held: Option<Option<String>> = tx
            .query_row(
                "SELECT owner FROM diagnostics_pages_v1 WHERE page=?",
                [page],
                |r| r.get(0),
            )
            .optional()?;
        if actor == OpenedBy::Agent && held.as_ref().and_then(|o| o.as_deref()) == Some("person") {
            if !enabled {
                return Err(Error::Ownership);
            }
            return Ok(());
        }
        if held.is_none() {
            let count: i64 =
                tx.query_row("SELECT COUNT(*) FROM diagnostics_pages_v1", [], |r| {
                    r.get(0)
                })?;
            if count >= self.limits.max_pages as i64 {
                return Err(Error::Capacity);
            }
        }
        let owner = enabled.then_some(match actor {
            OpenedBy::Person => "person",
            OpenedBy::Agent => "agent",
        });
        tx.execute("INSERT INTO diagnostics_pages_v1(page,owner,touched) VALUES(?,?,?) ON CONFLICT(page) DO UPDATE SET owner=excluded.owner,touched=excluded.touched",params![page,owner,at])?;
        tx.commit()?;
        Ok(())
    }
    pub fn expire_agents(&mut self, before: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE diagnostics_pages_v1 SET owner=NULL WHERE owner='agent' AND touched<?",
            [before],
        )?)
    }
    pub fn touch_agent(&mut self, page: &str, at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE diagnostics_pages_v1 SET touched=? WHERE page=? AND owner='agent'",
            params![at, page],
        )?;
        Ok(())
    }
    pub fn prune_closed(&mut self, before: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM diagnostics_pages_v1 WHERE owner IS NULL AND touched<?",
            [before],
        )?)
    }
    pub fn record(&mut self, record: Record) -> Result<i64> {
        self.record_many(vec![record])?
            .into_iter()
            .next()
            .ok_or(Error::Invalid)
    }
    /// All records commit together, or none do. Redaction runs before SQL.
    pub fn record_many(&mut self, records: Vec<Record>) -> Result<Vec<i64>> {
        if records.len() > self.limits.max_batch {
            return Err(Error::Invalid);
        }
        let mut prepared = vec![];
        for r in records {
            if r.key.trim().is_empty()
                || r.key.len() > 512
                || r.page.trim().is_empty()
                || r.page.len() > 512
            {
                return Err(Error::Invalid);
            }
            let mut entry = r.entry;
            sanitize_entry(&mut entry);
            let detail = r
                .detail
                .map(|mut d| {
                    sanitize_detail(&mut d, self.limits.keep_console_arguments);
                    serde_json::to_string(&d)
                })
                .transpose()?;
            let bytes = detail.as_ref().map_or(0, String::len);
            prepared.push((
                r.page,
                r.key,
                r.at,
                serde_json::to_string(&entry)?,
                detail,
                bytes,
            ));
        }
        let tx = self.conn.transaction()?;
        let mut pages = std::collections::HashSet::new();
        let mut seqs = vec![];
        for (page, key, at, entry, detail, bytes) in prepared {
            let owner: Option<Option<String>> = tx
                .query_row(
                    "SELECT owner FROM diagnostics_pages_v1 WHERE page=?",
                    [&page],
                    |r| r.get(0),
                )
                .optional()?;
            if owner.flatten().is_none() {
                return Err(Error::Disabled);
            }
            let seq=tx.query_row("INSERT INTO diagnostics_entries_v1(page,key,at,entry,detail,bytes) VALUES(?,?,?,?,?,?) ON CONFLICT(page,key) DO UPDATE SET at=excluded.at,entry=excluded.entry,detail=COALESCE(excluded.detail,diagnostics_entries_v1.detail),bytes=CASE WHEN excluded.detail IS NULL THEN diagnostics_entries_v1.bytes ELSE excluded.bytes END RETURNING seq",params![page,key,at,entry,detail,bytes as i64],|r|r.get(0))?;
            seqs.push(seq);
            pages.insert(page);
        }
        for page in pages {
            tx.execute("DELETE FROM diagnostics_entries_v1 WHERE page=? AND seq NOT IN (SELECT seq FROM diagnostics_entries_v1 WHERE page=? ORDER BY seq DESC LIMIT ?)",params![page,page,self.limits.entries_per_page as i64])?;
            let mut total: i64 = tx.query_row(
                "SELECT COALESCE(SUM(bytes),0) FROM diagnostics_entries_v1 WHERE page=?",
                [&page],
                |r| r.get(0),
            )?;
            while total > self.limits.detail_bytes_per_page as i64 {
                let (seq,bytes):(i64,i64)=tx.query_row("SELECT seq,bytes FROM diagnostics_entries_v1 WHERE page=? AND detail IS NOT NULL ORDER BY seq LIMIT 1",[&page],|r|Ok((r.get(0)?,r.get(1)?)))?;
                tx.execute(
                    "UPDATE diagnostics_entries_v1 SET detail=NULL,bytes=0 WHERE seq=?",
                    [seq],
                )?;
                total -= bytes;
            }
        }
        tx.commit()?;
        Ok(seqs)
    }
    pub fn query(&self, page: &str, query: &Query) -> Result<Vec<StoredEntry>> {
        let limit = if query.limit == 0 {
            100
        } else {
            query.limit.min(self.limits.entries_per_page)
        };
        let mut stmt = self.conn.prepare(
            "SELECT seq,at,entry FROM diagnostics_entries_v1 WHERE page=? AND seq>? ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![page, query.after], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut result = vec![];
        for row in rows {
            let (seq, at, raw) = row?;
            let entry: Entry = serde_json::from_str(&raw)?;
            if query.network_only && !matches!(&entry, Entry::Network { .. }) {
                continue;
            }
            if query
                .text
                .as_ref()
                .is_some_and(|t| !raw.to_lowercase().contains(&t.to_lowercase()))
            {
                continue;
            }
            result.push(StoredEntry { seq, at, entry });
            if result.len() >= limit {
                break;
            }
        }
        Ok(result)
    }
    pub fn open_entry(&self, page: &str, seq: i64) -> Result<Option<OpenedEntry>> {
        let row: Option<(i64, String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT at,entry,detail FROM diagnostics_entries_v1 WHERE page=? AND seq=?",
                params![page, seq],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(|(at, entry, detail)| {
            Ok(OpenedEntry {
                entry: StoredEntry {
                    seq,
                    at,
                    entry: serde_json::from_str(&entry)?,
                },
                detail: detail.map(|s| serde_json::from_str(&s)).transpose()?,
            })
        })
        .transpose()
    }
    pub fn clear(&mut self, page: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM diagnostics_entries_v1 WHERE page=?", [page])?;
        Ok(())
    }
}
fn safe_url(raw: &str) -> String {
    let Ok(mut url) = url::Url::parse(raw) else {
        return "[unparseable URL]".into();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);
    let fields = url
        .query_pairs()
        .map(|(k, v)| {
            let hidden = header(&k, &v).hidden;
            (
                k.to_string(),
                if hidden {
                    "[hidden]".into()
                } else {
                    v.into_owned()
                },
            )
        })
        .collect::<Vec<_>>();
    if !fields.is_empty() {
        url.query_pairs_mut().clear().extend_pairs(fields);
    }
    clip(url.as_str(), URL_MAX)
}
fn sanitize_entry(e: &mut Entry) {
    match e {
        Entry::Console { text, source, .. } => {
            *text = clip(text, TEXT_MAX);
            *source = source.as_ref().map(|s| safe_url(s));
        }
        Entry::Network {
            url,
            method,
            resource_type,
            error,
            redirected_to,
            mime_type,
            ..
        } => {
            *url = safe_url(url);
            *method = clip(method, 32);
            *resource_type = clip(resource_type, 128);
            *error = error.as_ref().map(|s| clip(s, TEXT_MAX));
            *redirected_to = redirected_to.as_ref().map(|s| safe_url(s));
            *mime_type = mime_type.as_ref().map(|s| clip(s, 256));
        }
    }
}
fn sanitize_detail(d: &mut Detail, args: bool) {
    if !args {
        d.args.clear()
    }
    d.stack = d.stack.as_ref().map(|s| clip(s, TEXT_MAX));
    for h in d
        .request_headers
        .iter_mut()
        .chain(d.response_headers.iter_mut())
    {
        *h = header(&clip(&h.name, 256), &h.value)
    }
    for b in [&mut d.request_body, &mut d.response_body]
        .into_iter()
        .flatten()
    {
        let Body {
            text,
            mime_type,
            truncated,
        } = b.clone();
        *b = body(&text, mime_type.as_deref(), truncated);
    }
    let messages = std::mem::take(&mut d.messages)
        .into_iter()
        .map(|m| {
            stream_message(
                m.at,
                m.sent,
                if m.binary.is_some() {
                    None
                } else {
                    Some(&m.text)
                },
                m.event,
                m.binary,
                m.truncated,
            )
        })
        .collect();
    crate::add_messages(d, messages, 0);
}
