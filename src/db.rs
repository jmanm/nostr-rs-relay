//! Event persistence and querying
use crate::error::Result;
use crate::event::Event;
use crate::subscription::Subscription;
use hex;
use log::*;
use rusqlite::params;
use rusqlite::Connection;
use rusqlite::OpenFlags;
use std::path::Path;
use tokio::task;

/// Database file
const DB_FILE: &str = "nostr.db";

/// Schema definition
const INIT_SQL: &str = r##"
-- Database settings
PRAGMA encoding = "UTF-8";
PRAGMA journal_mode=WAL;
PRAGMA main.synchronous=NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA application_id = 1654008667;
PRAGMA user_version = 1;
pragma mmap_size = 536870912; -- 512MB of mmap

-- Event Table
CREATE TABLE IF NOT EXISTS event (
id INTEGER PRIMARY KEY,
event_hash BLOB NOT NULL, -- 4-byte hash
first_seen INTEGER NOT NULL, -- when the event was first seen (not authored!) (seconds since 1970)
created_at INTEGER NOT NULL, -- when the event was authored
author BLOB NOT NULL, -- author pubkey
kind INTEGER NOT NULL, -- event kind
content TEXT NOT NULL -- serialized json of event object
);

-- Event Indexes
CREATE UNIQUE INDEX IF NOT EXISTS event_hash_index ON event(event_hash);
CREATE INDEX IF NOT EXISTS created_at_index ON event(created_at);
CREATE INDEX IF NOT EXISTS author_index ON event(author);
CREATE INDEX IF NOT EXISTS kind_index ON event(kind);

-- Event References Table
CREATE TABLE IF NOT EXISTS event_ref (
id INTEGER PRIMARY KEY,
event_id INTEGER NOT NULL, -- an event ID that contains an #e tag.
referenced_event BLOB NOT NULL, -- the event that is referenced.
FOREIGN KEY(event_id) REFERENCES event(id) ON UPDATE CASCADE ON DELETE CASCADE
);

-- Event References Index
CREATE INDEX IF NOT EXISTS event_ref_index ON event_ref(referenced_event);

-- Pubkey References Table
CREATE TABLE IF NOT EXISTS pubkey_ref (
id INTEGER PRIMARY KEY,
event_id INTEGER NOT NULL, -- an event ID that contains an #p tag.
referenced_pubkey BLOB NOT NULL, -- the pubkey that is referenced.
FOREIGN KEY(event_id) REFERENCES event(id) ON UPDATE RESTRICT ON DELETE CASCADE
);

-- Pubkey References Index
CREATE INDEX IF NOT EXISTS pubkey_ref_index ON pubkey_ref(referenced_pubkey);
"##;

/// Spawn a database writer that persists events to the SQLite store.
pub async fn db_writer(
    mut event_rx: tokio::sync::mpsc::Receiver<Event>,
    bcast_tx: tokio::sync::broadcast::Sender<Event>,
) -> tokio::task::JoinHandle<Result<()>> {
    task::spawn_blocking(move || {
        let mut conn = Connection::open_with_flags(
            Path::new(DB_FILE),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        info!("opened database for writing");
        // TODO: determine if we need to execute the init script.
        // TODO: check database app id / version before proceeding.
        match conn.execute_batch(INIT_SQL) {
            Ok(()) => info!("database pragma/schema initialized and ready"),
            Err(err) => error!("update failed: {}", err),
        }
        loop {
            // call blocking read on channel
            let next_event = event_rx.blocking_recv();
            // if the channel has closed, we will never get work
            if next_event.is_none() {
                break;
            }
            let event = next_event.unwrap();
            match write_event(&mut conn, &event) {
                Ok(updated) => {
                    if updated == 0 {
                        info!("nothing inserted (dupe?)");
                    } else {
                        info!("persisted event: {}", event.get_event_id_prefix());
                        // send this out to all clients
                        bcast_tx.send(event.clone()).ok();
                    }
                }
                Err(err) => {
                    warn!("event insert failed: {}", err);
                }
            }
        }
        conn.close().ok();
        info!("database connection closed");
        Ok(())
    })
}

/// Persist an event to the database.
pub fn write_event(conn: &mut Connection, e: &Event) -> Result<usize> {
    // start transaction
    let tx = conn.transaction()?;
    // get relevant fields from event and convert to blobs.
    let id_blob = hex::decode(&e.id).ok();
    let pubkey_blob = hex::decode(&e.pubkey).ok();
    let event_str = serde_json::to_string(&e).ok();
    // ignore if the event hash is a duplicate.
    let ins_count = tx.execute(
        "INSERT OR IGNORE INTO event (event_hash, created_at, kind, author, content, first_seen) VALUES (?1, ?2, ?3, ?4, ?5, strftime('%s','now'));",
        params![id_blob, e.created_at, e.kind, pubkey_blob, event_str]
    )?;
    if ins_count == 0 {
        // if the event was a duplicate, no need to insert event or
        // pubkey references.
        return Ok(ins_count);
    }
    // remember primary key of the event most recently inserted.
    let ev_id = tx.last_insert_rowid();
    // add all event tags into the event_ref table
    let etags = e.get_event_tags();
    if !etags.is_empty() {
        for etag in etags.iter() {
            tx.execute(
                "INSERT OR IGNORE INTO event_ref (event_id, referenced_event) VALUES (?1, ?2)",
                params![ev_id, hex::decode(&etag).ok()],
            )?;
        }
    }
    // add all event tags into the pubkey_ref table
    let ptags = e.get_pubkey_tags();
    if !ptags.is_empty() {
        for ptag in ptags.iter() {
            tx.execute(
                "INSERT OR IGNORE INTO pubkey_ref (event_id, referenced_pubkey) VALUES (?1, ?2)",
                params![ev_id, hex::decode(&ptag).ok()],
            )?;
        }
    }
    tx.commit()?;
    Ok(ins_count)
}

/// Event resulting from a specific subscription request
#[derive(PartialEq, Debug, Clone)]
pub struct QueryResult {
    /// Subscription identifier
    pub sub_id: String,
    /// Serialized event
    pub event: String,
}

/// Check if a string contains only hex characters.
fn is_hex(s: &str) -> bool {
    s.chars().all(|x| char::is_ascii_hexdigit(&x))
}

/// Create a dynamic SQL query string from a subscription.
fn query_from_sub(sub: &Subscription) -> String {
    // build a dynamic SQL query.  all user-input is either an integer
    // (sqli-safe), or a string that is filtered to only contain
    // hexadecimal characters.
    let mut query =
        "SELECT DISTINCT(e.content) FROM event e LEFT JOIN event_ref er ON e.id=er.event_id LEFT JOIN pubkey_ref pr ON e.id=pr.event_id "
            .to_owned();
    // for every filter in the subscription, generate a where clause
    let mut filter_clauses: Vec<String> = Vec::new();
    for f in sub.filters.iter() {
        // individual filter components
        let mut filter_components: Vec<String> = Vec::new();
        // Query for "authors"
        if f.authors.is_some() {
            let authors_escaped: Vec<String> = f
                .authors
                .as_ref()
                .unwrap()
                .iter()
                .filter(|&x| is_hex(x))
                .map(|x| format!("x'{}'", x))
                .collect();
            let authors_clause = format!("author IN ({})", authors_escaped.join(", "));
            filter_components.push(authors_clause);
        }
        // Query for Kind
        if f.kind.is_some() {
            // kind is number, no escaping needed
            let kind_clause = format!("kind = {}", f.kind.unwrap());
            filter_components.push(kind_clause);
        }
        // Query for event
        if f.id.is_some() {
            let id_str = f.id.as_ref().unwrap();
            if is_hex(id_str) {
                let id_clause = format!("event_hash = x'{}'", id_str);
                filter_components.push(id_clause);
            }
        }
        // Query for referenced event
        if f.event.is_some() {
            let ev_str = f.event.as_ref().unwrap();
            if is_hex(ev_str) {
                let ev_clause = format!("referenced_event = x'{}'", ev_str);
                filter_components.push(ev_clause);
            }
        }
        // Query for referenced pet name pubkey
        if f.pubkey.is_some() {
            let pet_str = f.pubkey.as_ref().unwrap();
            if is_hex(pet_str) {
                let pet_clause = format!("referenced_pubkey = x'{}'", pet_str);
                filter_components.push(pet_clause);
            }
        }
        // Query for timestamp
        if f.since.is_some() {
            let created_clause = format!("created_at > {}", f.since.unwrap());
            filter_components.push(created_clause);
        }
        // combine all clauses, and add to filter_clauses
        if !filter_components.is_empty() {
            let mut fc = "( ".to_owned();
            fc.push_str(&filter_components.join(" AND "));
            fc.push_str(" )");
            filter_clauses.push(fc);
        } else {
            // if there are no filter clauses, we should return everything
            filter_clauses.push(" 1=1 ".to_owned());
        }
    }

    // combine all filters with OR clauses, if any exist
    if !filter_clauses.is_empty() {
        query.push_str(" WHERE ");
        query.push_str(&filter_clauses.join(" OR "));
    }
    debug!("query string: {}", query);
    query
}

/// Perform a database query using a subscription.
///
/// The [`Subscription`] is converted into a SQL query.  Each result
/// is published on the `query_tx` channel as it is returned.  If a
/// message becomes available on the `abandon_query_rx` channel, the
/// query is immediately aborted.
pub async fn db_query(
    sub: Subscription,
    query_tx: tokio::sync::mpsc::Sender<QueryResult>,
    mut abandon_query_rx: tokio::sync::oneshot::Receiver<()>,
) {
    task::spawn_blocking(move || {
        let conn =
            Connection::open_with_flags(Path::new(DB_FILE), OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        debug!("opened database for reading");
        debug!("going to query for: {:?}", sub);
        // generate SQL query
        let q = query_from_sub(&sub);
        // execute the query
        let mut stmt = conn.prepare(&q).unwrap();
        let mut event_rows = stmt.query([]).unwrap();
        while let Some(row) = event_rows.next().unwrap() {
            // check if this is still active (we could do this every N rows)
            if abandon_query_rx.try_recv().is_ok() {
                debug!("query aborted");
                return;
            }
            // TODO: check before unwrapping
            let event_json = row.get(0).unwrap();
            query_tx
                .blocking_send(QueryResult {
                    sub_id: sub.get_id(),
                    event: event_json,
                })
                .ok();
        }
        debug!("query completed");
    });
}