use crate::model::{Answer, Message};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const MAX_ENTRIES: usize = 256;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(24 * 60 * 60);

struct Entry {
    owner: String,
    response: Value,
    history: History,
    created: Instant,
    bytes: usize,
}

#[derive(Clone, Serialize)]
pub enum History {
    Canonical(Vec<Message>),
    Native { items: Vec<Value>, upstream: String },
}

#[derive(Default)]
struct Entries {
    by_id: HashMap<String, Entry>,
    order: VecDeque<String>,
    bytes: usize,
}

#[derive(Default)]
pub struct ResponseStore {
    entries: Mutex<Entries>,
}

pub struct PendingResponse {
    pub store: Arc<ResponseStore>,
    pub owner: String,
    pub messages: Vec<Message>,
    pub previous_id: Value,
    pub enabled: bool,
}

impl PendingResponse {
    pub fn finish(self, response: &mut Value, answer: &Answer) -> Result<(), ()> {
        response["previous_response_id"] = self.previous_id;
        response["store"] = json!(self.enabled);
        if self.enabled {
            let mut messages = self.messages;
            messages.push(Message {
                role: "assistant".into(),
                content: json!(answer.content),
                calls: answer.calls.clone(),
                ..Default::default()
            });
            self.store
                .insert(self.owner, response.clone(), History::Canonical(messages))?;
        }
        Ok(())
    }
}

pub struct PendingNativeResponse {
    pub store: Arc<ResponseStore>,
    pub owner: String,
    pub items: Vec<Value>,
    pub upstream: String,
    pub previous_id: Value,
    pub enabled: bool,
}

impl PendingNativeResponse {
    pub fn decorate(&self, response: &mut Value) {
        response["previous_response_id"] = self.previous_id.clone();
        response["store"] = json!(self.enabled);
    }

    pub fn finish(mut self, response: &mut Value) -> Result<(), ()> {
        self.decorate(response);
        let output = response["output"].as_array().ok_or(())?;
        self.items.extend(output.iter().cloned());
        if self.enabled {
            self.store.insert(
                self.owner,
                response.clone(),
                History::Native {
                    items: self.items,
                    upstream: self.upstream,
                },
            )?;
        }
        Ok(())
    }
}

impl ResponseStore {
    pub fn new() -> Arc<Self> {
        let store = Arc::new(Self::default());
        let weak = Arc::downgrade(&store);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.tick().await;
            loop {
                interval.tick().await;
                let Some(store) = weak.upgrade() else {
                    break;
                };
                Self::expire(&mut store.entries.lock().unwrap());
            }
        });
        store
    }

    fn expire(entries: &mut Entries) {
        while let Some(id) = entries.order.front() {
            if entries
                .by_id
                .get(id)
                .is_some_and(|entry| entry.created.elapsed() < TTL)
            {
                break;
            }
            let id = entries.order.pop_front().unwrap();
            if let Some(entry) = entries.by_id.remove(&id) {
                entries.bytes = entries.bytes.saturating_sub(entry.bytes);
            }
        }
    }

    pub fn get(&self, id: &str, owner: &str) -> Option<(Value, History)> {
        let mut entries = self.entries.lock().unwrap();
        Self::expire(&mut entries);
        entries
            .by_id
            .get(id)
            .filter(|entry| entry.owner == owner)
            .map(|entry| (entry.response.clone(), entry.history.clone()))
    }

    pub fn delete(&self, id: &str, owner: &str) -> bool {
        let mut entries = self.entries.lock().unwrap();
        Self::expire(&mut entries);
        if !entries
            .by_id
            .get(id)
            .is_some_and(|entry| entry.owner == owner)
        {
            return false;
        }
        if let Some(entry) = entries.by_id.remove(id) {
            entries.bytes = entries.bytes.saturating_sub(entry.bytes);
        }
        entries.order.retain(|key| key != id);
        true
    }

    fn insert(&self, owner: String, response: Value, history: History) -> Result<(), ()> {
        let id = response["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or(())?
            .to_owned();
        let size = serde_json::to_vec(&history)
            .map_err(|_| ())?
            .len()
            .saturating_add(serde_json::to_vec(&response).map_err(|_| ())?.len());
        if size > MAX_ENTRY_BYTES {
            return Err(());
        }
        let mut entries = self.entries.lock().unwrap();
        Self::expire(&mut entries);
        if entries.by_id.contains_key(&id) {
            return Err(());
        }
        while entries.by_id.len() >= MAX_ENTRIES || entries.bytes.saturating_add(size) > MAX_BYTES {
            let old = entries.order.pop_front().ok_or(())?;
            if let Some(entry) = entries.by_id.remove(&old) {
                entries.bytes = entries.bytes.saturating_sub(entry.bytes);
            }
        }
        entries.bytes += size;
        entries.order.push_back(id.clone());
        entries.by_id.insert(
            id,
            Entry {
                owner,
                response,
                history,
                created: Instant::now(),
                bytes: size,
            },
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_expires_and_capacity_is_bounded() {
        let store = ResponseStore::default();
        for index in 0..=MAX_ENTRIES {
            let response = json!({"id":format!("resp_{index}"),"output":[]});
            store
                .insert("owner".into(), response, History::Canonical(Vec::new()))
                .unwrap();
        }
        assert!(store.get("resp_0", "owner").is_none());
        assert!(store.get(&format!("resp_{MAX_ENTRIES}"), "owner").is_some());
        {
            let mut entries = store.entries.lock().unwrap();
            let first = entries.order.front().unwrap().clone();
            entries.by_id.get_mut(&first).unwrap().created =
                Instant::now() - TTL - Duration::from_secs(1);
        }
        assert!(store.get("resp_1", "owner").is_none());
    }
}
