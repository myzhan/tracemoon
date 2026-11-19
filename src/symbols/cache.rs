//! Symbol caching for performance optimization

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};

/// Bounded (FIFO-evicting) cache for symbol resolution results.
///
/// Eviction is first-in-first-out, not LRU: hits do not reorder entries. That is
/// deliberate — resolved addresses stay hot for the whole run, so the cheaper policy is
/// enough, and the name says what the code actually does.
pub struct SymbolCache {
    /// Address -> Symbol name mapping
    cache: HashMap<usize, String>,
    /// Maximum cache size
    max_size: usize,
    /// Insertion order, used to pick the oldest entry to evict.
    order: VecDeque<usize>,
}

impl SymbolCache {
    /// Create a new symbol cache with the given maximum size
    pub fn new(max_size: usize) -> Self {
        Self {
            cache: HashMap::with_capacity(max_size),
            max_size,
            order: VecDeque::with_capacity(max_size),
        }
    }

    /// Get a cached symbol name
    pub fn get(&self, addr: usize) -> Option<&String> {
        self.cache.get(&addr)
    }

    /// Insert a symbol into the cache
    pub fn insert(&mut self, addr: usize, name: String) {
        // Already cached: update in place and keep its place in the eviction queue.
        if let Entry::Occupied(mut entry) = self.cache.entry(addr) {
            entry.insert(name);
            return;
        }

        if self.cache.len() >= self.max_size {
            if let Some(oldest) = self.order.pop_front() {
                self.cache.remove(&oldest);
            }
        }

        self.cache.insert(addr, name);
        self.order.push_back(addr);
    }

    /// Get the number of cached entries
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.cache.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_basic() {
        let mut cache = SymbolCache::new(100);

        cache.insert(0x1000, "func1".to_string());
        cache.insert(0x2000, "func2".to_string());

        assert_eq!(cache.get(0x1000), Some(&"func1".to_string()));
        assert_eq!(cache.get(0x2000), Some(&"func2".to_string()));
        assert_eq!(cache.get(0x3000), None);
    }

    #[test]
    fn test_cache_eviction() {
        let mut cache = SymbolCache::new(2);

        cache.insert(0x1000, "func1".to_string());
        cache.insert(0x2000, "func2".to_string());
        cache.insert(0x3000, "func3".to_string());

        // First entry should be evicted
        assert_eq!(cache.get(0x1000), None);
        assert_eq!(cache.get(0x2000), Some(&"func2".to_string()));
        assert_eq!(cache.get(0x3000), Some(&"func3".to_string()));
    }

    #[test]
    fn test_cache_update() {
        let mut cache = SymbolCache::new(100);

        cache.insert(0x1000, "func1".to_string());
        cache.insert(0x1000, "func1_updated".to_string());

        assert_eq!(cache.get(0x1000), Some(&"func1_updated".to_string()));
        assert_eq!(cache.len(), 1);
    }
}
