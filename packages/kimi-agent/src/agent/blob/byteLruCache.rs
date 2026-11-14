use indexmap::IndexMap;

pub struct ByteLruCache {
    map: IndexMap<String, Vec<u8>>,
    current_bytes: usize,
    max_bytes: usize,
}

impl ByteLruCache {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            map: IndexMap::new(),
            current_bytes: 0,
            max_bytes,
        }
    }

    pub fn get(&mut self, key: &str) -> Option<Vec<u8>> {
        let value = self.map.shift_remove(key)?;
        self.map.insert(key.to_string(), value.clone());
        Some(value)
    }

    pub fn set(&mut self, key: &str, value: Vec<u8>) {
        let size = value.len();
        self.remove(key);
        if size > self.max_bytes {
            return;
        }
        while !self.map.is_empty() && self.current_bytes + size > self.max_bytes {
            self.evict_oldest();
        }
        self.current_bytes += size;
        self.map.insert(key.to_string(), value);
    }

    fn evict_oldest(&mut self) {
        if let Some((key, _)) = self.map.first() {
            let key = key.clone();
            self.remove(&key);
        }
    }

    fn remove(&mut self, key: &str) {
        if let Some(value) = self.map.shift_remove(key) {
            self.current_bytes = self.current_bytes.saturating_sub(value.len());
        }
    }
}
