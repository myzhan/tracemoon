//! Symbol caching for performance optimization

use std::collections::HashMap;

/// LRU-style cache for symbol resolution results
pub struct SymbolCache {
    /// Address -> Symbol name mapping
    cache: HashMap<usize, String>,
    /// Maximum cache size
    max_size: usize,
    /// Access order for LRU eviction (simplified - just track insertions)
    order: Vec<usize>,
}

impl SymbolCache {
    /// Create a new symbol cache with the given maximum size
    pub fn new(max_size: usize) -> Self {
        Self {
            cache: HashMap::with_capacity(max_size),
            max_size,
            order: Vec::with_capacity(max_size),
        }
    }

    /// Get a cached symbol name
    pub fn get(&self, addr: usize) -> Option<&String> {
        self.cache.get(&addr)
    }

    /// Insert a symbol into the cache
    pub fn insert(&mut self, addr: usize, name: String) {
        if self.cache.contains_key(&addr) {
            // Already exists, just update
            self.cache.insert(addr, name);
            return;
        }
        
        // Evict if at capacity
        if self.cache.len() >= self.max_size && !self.order.is_empty() {
            // Simple FIFO eviction for simplicity
            let old_addr = self.order.remove(0);
            self.cache.remove(&old_addr);
        }
        
        self.cache.insert(addr, name);
        self.order.push(addr);
    }

    /// Clear the cache
    pub fn clear(&mut self) {
        self.cache.clear();
        self.order.clear();
    }

    /// Get the number of cached entries
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Check if cache is empty
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
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
