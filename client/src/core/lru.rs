//! A small least-recently-used map with a budget in bytes, for decoded pictures and soundpad
//! clips. Entries that cost nothing (a picture still loading) stay until replaced or removed.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;

pub struct Lru<K, V> {
    entries: HashMap<K, Entry<V>>,
    budget: usize,
    used: usize,
    tick: u64,
}

struct Entry<V> {
    value: V,
    cost: usize,
    used_at: u64,
}

impl<K: Eq + Hash + Clone, V> Lru<K, V> {
    pub fn new(budget: usize) -> Lru<K, V> {
        Lru { entries: HashMap::new(), budget, used: 0, tick: 0 }
    }

    /// The value, marked as just used.
    pub fn get<Q: Eq + Hash + ?Sized>(&mut self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        self.tick += 1;
        let e = self.entries.get_mut(key)?;
        e.used_at = self.tick;
        Some(&e.value)
    }

    /// Adds or replaces a value and returns whatever had to go to stay within the budget,
    /// including the replaced value. The new entry itself is never evicted.
    pub fn insert(&mut self, key: K, value: V, cost: usize) -> Vec<V> {
        self.tick += 1;
        let mut out = Vec::new();
        if let Some(old) = self.entries.insert(key.clone(), Entry { value, cost, used_at: self.tick }) {
            self.used -= old.cost;
            out.push(old.value);
        }
        self.used += cost;
        while self.used > self.budget {
            let victim =
                self.entries.iter().filter(|(k, e)| e.cost > 0 && **k != key).min_by_key(|(_, e)| e.used_at).map(|(k, _)| k.clone());
            let Some(victim) = victim else { break };
            if let Some(v) = self.remove(&victim) {
                out.push(v);
            }
        }
        out
    }

    pub fn remove<Q: Eq + Hash + ?Sized>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        let e = self.entries.remove(key)?;
        self.used -= e.cost;
        Some(e.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl<K, V> Lru<K, V> {
        fn used(&self) -> usize {
            self.used
        }
    }

    #[test]
    fn evicts_the_least_recently_used_first() {
        let mut lru = Lru::new(10);
        assert!(lru.insert("a", 1, 4).is_empty());
        assert!(lru.insert("b", 2, 4).is_empty());
        lru.get(&"a");
        assert_eq!(lru.insert("c", 3, 4), vec![2]);
        assert_eq!(lru.get(&"a"), Some(&1));
        assert_eq!(lru.get(&"b"), None);
        assert_eq!(lru.used(), 8);
    }

    #[test]
    fn replacing_hands_back_the_old_value_and_keeps_the_count() {
        let mut lru = Lru::new(10);
        lru.insert("a", 1, 0);
        assert_eq!(lru.insert("a", 2, 6), vec![1]);
        assert_eq!(lru.used(), 6);
        assert_eq!(lru.remove(&"a"), Some(2));
        assert_eq!(lru.used(), 0);
    }

    #[test]
    fn free_entries_and_the_newest_one_stay() {
        let mut lru = Lru::new(4);
        lru.insert("loading", 0, 0);
        assert!(lru.insert("huge", 1, 100).is_empty());
        assert_eq!(lru.get(&"loading"), Some(&0));
        assert_eq!(lru.insert("next", 2, 1), vec![1]);
    }
}
