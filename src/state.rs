use dashmap::DashMap;
use sonic_rs::{JsonValueTrait, Value};

pub struct SharedState {
    kv: DashMap<String, Value>,
}

impl SharedState {
    pub fn new() -> Self {
        Self { kv: DashMap::new() }
    }

    pub fn set(&self, key: String, value: Value) {
        self.kv.insert(key, value);
    }

    pub fn get_ref(&self, key: &str) -> Option<dashmap::mapref::one::Ref<'_, String, Value>> {
        self.kv.get(key)
    }

    pub fn remove(&self, key: &str) -> Option<Value> {
        self.kv.remove(key).map(|(_, v)| v)
    }

    pub fn increment(&self, key: &str, delta: i64) -> i64 {
        let mut entry = self.kv.entry(key.to_string()).or_insert(Value::new_i64(0));
        let current = entry.as_i64().unwrap_or_default();
        let new_val = current + delta;
        *entry = Value::new_i64(new_val);
        new_val
    }

    pub fn keys(&self) -> Vec<String> {
        self.kv.iter().map(|entry| entry.key().clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn test_basic_set_get() {
        let state = SharedState::new();
        state.set("key1".to_string(), Value::new_i64(100));
        assert_eq!(state.get_ref("key1").and_then(|v| v.as_i64()), Some(100));
    }

    #[test]
    fn test_heavy_atomic_contention() {
        let state = Arc::new(SharedState::new());
        let thread_count = 100;
        let iterations = 10000;
        let barrier = Arc::new(Barrier::new(thread_count));
        let mut handles = Vec::new();

        for _ in 0..thread_count {
            let s = Arc::clone(&state);
            let b = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                b.wait();
                for _ in 0..iterations {
                    s.increment("global_counter", 1);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            state.get_ref("global_counter").and_then(|v| v.as_i64()),
            Some((thread_count * iterations) as i64)
        );
    }

    #[test]
    fn test_massive_key_churn() {
        let state = SharedState::new();
        let count = 100000; // 1M is too slow for normal cargo test, using 100k
        for i in 0..count {
            state.set(format!("key_{}", i), Value::new_i64(i as i64));
        }

        for i in 0..count {
            assert_eq!(
                state
                    .get_ref(&format!("key_{}", i))
                    .and_then(|v| v.as_i64()),
                Some(i as i64)
            );
        }
    }
}
