//! The writer: one exclusive OS lock, duplicate check before add, and the plan
//! lifecycle. A refusal writes nothing.
use crate::{similarity, validate, LoreError, Result, Store};
use fs2::FileExt;
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};

/// At most this many records a duplicate scan reads.
const MAX_SCAN: usize = 200_000;

#[derive(Clone, Debug, Default)]
pub struct Duplicates {
    /// A record this one repeats (Jaccard >= 0.5), if any.
    pub same: Option<String>,
}

/// A held writer lock. Dropping it releases the lock.
pub struct Writer {
    lock: File,
}

impl Writer {
    /// Takes `.lore.lock`, waiting up to 30 s; the holder is recorded in it.
    pub fn acquire(root: &Path, who: &str) -> Result<Self> {
        std::fs::create_dir_all(root).map_err(|error| LoreError::new(format!("{}: {error}", root.display())))?;
        let path = root.join(".lore.lock");
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)
            .map_err(|error| LoreError::new(format!("{}: {error}", path.display())))?;
        let started = Instant::now();
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => break,
                Err(_) if started.elapsed() < Duration::from_secs(30) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => {
                    let holder = read_holder(&mut file).unwrap_or_default();
                    return Err(LoreError::new(format!(
                        "another writer holds the store ({holder}); nothing was written")));
                }
            }
        }
        let holder = json!({"who": who, "since": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())});
        file.set_len(0).ok();
        file.seek(SeekFrom::Start(0)).ok();
        file.write_all(holder.to_string().as_bytes()).ok();
        file.flush().ok();
        Ok(Self { lock: file })
    }

    /// Refuses a record whose story repeats one already in `records`.
    pub fn duplicate(records: &[Value], record: &Value) -> Duplicates {
        let mut same = None;
        for existing in records.iter().take(MAX_SCAN) {
            if existing.get("kind") == record.get("kind") && similarity(existing, record) >= 0.5 {
                same = existing.get("id").and_then(Value::as_str).map(str::to_owned);
                if same.is_some() { break; }
            }
        }
        Duplicates { same }
    }

    /// Adds a validated record; duplicates and invalid records are refused.
    pub fn add(&self, store: &Store, relative: &str, record: Value, areas: &[String], new: bool) -> Result<()> {
        let errors = validate(&record, areas, None);
        if !errors.is_empty() { return Err(LoreError::with("refused, nothing was written", errors)); }
        let records = store.read(relative).unwrap_or_default();
        if !new {
            let duplicate = Self::duplicate(&records, &record);
            if let Some(same) = duplicate.same {
                return Err(LoreError::new(format!("the record repeats {same}; pass --new to add it anyway")));
            }
        }
        store.add(relative, record)
    }

    /// Moves one item through its lifecycle. A closed status needs its fields;
    /// done/dropped/superseded may only reopen with a reason.
    pub fn item_status(&self, store: &Store, plan: &str, id: &str, status: &str, fields: &serde_json::Map<String, Value>, areas: &[String]) -> Result<()> {
        let relative = format!("lore/plans/{plan}.jsonl");
        let mut records = store.read(&relative)?;
        let Some(position) = records.iter().position(|record| record.get("id").and_then(Value::as_str) == Some(id)) else {
            return Err(LoreError::new(format!("no item {id} in plan {plan}")));
        };
        let old = records[position].get("status").and_then(Value::as_str).unwrap_or_default().to_owned();
        let allowed = matches!(status, "done" | "dropped" | "superseded" | "deferred" | "open");
        if !allowed { return Err(LoreError::new(format!("unknown item status {status}"))); }
        if old == "open" || old == "deferred" {
            // free to any status
        } else if status == "open" && fields.get("reason").and_then(Value::as_str).is_some_and(|r| !r.is_empty()) {
            // reopening needs a reason
        } else {
            return Err(LoreError::new(format!("item {id} is {old}; only reopen with a reason")));
        }
        let record = &mut records[position];
        for key in ["commits", "reason", "decided_by", "superseded_by", "outcome"] {
            record.as_object_mut().map(|map| map.remove(key));
        }
        record["status"] = json!(status);
        for (key, value) in fields { record[key] = value.clone(); }
        let errors = validate(record, areas, None);
        if !errors.is_empty() { return Err(LoreError::with("refused, nothing was written", errors)); }
        store.write(&relative, &records)
    }    /// A plan closes only when none of its items is open or deferred.
    pub fn close_plan(store: &Store, plan: &str, outcome: &str, areas: &[String]) -> Result<()> {
        let relative = format!("lore/plans/{plan}.jsonl");
        let mut records = store.read(&relative)?;
        let open = records.iter().skip(1).any(|item| {
            matches!(item.get("status").and_then(Value::as_str), Some("open" | "deferred"))
        });
        if open { return Err(LoreError::new(format!("plan {plan} still has open items"))); }
        let Some(plan_record) = records.first_mut() else { return Err(LoreError::new(format!("plan {plan} is missing"))); };
        plan_record["status"] = json!("closed");
        plan_record["outcome"] = json!(outcome);
        plan_record["updated"] = json!(plan_record.get("updated").cloned().unwrap_or(json!("")));
        let errors = validate(plan_record, areas, None);
        if !errors.is_empty() { return Err(LoreError::with("refused, nothing was written", errors)); }
        store.write(&relative, &records)
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.lock.set_len(0);
        let _ = fs2::FileExt::unlock(&self.lock);
    }
}

fn read_holder(file: &mut File) -> Option<String> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut text).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    Some(format!("{} since {}", value["who"].as_str().unwrap_or("unknown"), value["since"].as_u64().unwrap_or(0)))
}
