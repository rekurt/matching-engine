/// Slab key type — u32 supports ~4 billion entries per book.
pub type SlabKey = u32;

/// A slab entry: either occupied with a value, or vacant with a pointer to the next free slot.
enum Entry<T> {
    Occupied(T),
    Vacant { next_free: Option<SlabKey> },
}

/// Pre-allocated slab allocator for order nodes.
/// Once capacity is allocated, insert/remove never touch the global allocator.
/// Free slots are recycled through an intrusive free list.
pub struct Slab<T> {
    entries: Vec<Entry<T>>,
    free_head: Option<SlabKey>,
    len: usize,
}

impl<T> Slab<T> {
    /// Create a new slab with the given pre-allocated capacity.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            entries: Vec::with_capacity(cap),
            free_head: None,
            len: 0,
        }
    }

    /// Number of occupied entries.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Insert a value and return its slab key.
    pub fn insert(&mut self, val: T) -> SlabKey {
        self.len += 1;
        if let Some(key) = self.free_head {
            // Reuse a free slot.
            let idx = key as usize;
            match &self.entries[idx] {
                Entry::Vacant { next_free } => {
                    self.free_head = *next_free;
                }
                Entry::Occupied(_) => unreachable!("free_head points to occupied slot"),
            }
            self.entries[idx] = Entry::Occupied(val);
            key
        } else {
            // Append new slot.
            let key = self.entries.len() as SlabKey;
            self.entries.push(Entry::Occupied(val));
            key
        }
    }

    /// Remove a value by key and return it.
    /// Panics if the key is invalid or already vacant.
    pub fn remove(&mut self, key: SlabKey) -> T {
        let idx = key as usize;
        // Check before mutating to give a clear panic message
        match &self.entries[idx] {
            Entry::Vacant { .. } => panic!("removing vacant slab entry at key {key}"),
            Entry::Occupied(_) => {}
        }
        let entry = std::mem::replace(
            &mut self.entries[idx],
            Entry::Vacant {
                next_free: self.free_head,
            },
        );
        self.free_head = Some(key);
        self.len -= 1;
        match entry {
            Entry::Occupied(val) => val,
            Entry::Vacant { .. } => unreachable!(),
        }
    }

    /// Get a reference to a value by key.
    #[inline]
    pub fn get(&self, key: SlabKey) -> Option<&T> {
        match self.entries.get(key as usize)? {
            Entry::Occupied(val) => Some(val),
            Entry::Vacant { .. } => None,
        }
    }

    /// Get a mutable reference to a value by key.
    #[inline]
    pub fn get_mut(&mut self, key: SlabKey) -> Option<&mut T> {
        match self.entries.get_mut(key as usize)? {
            Entry::Occupied(val) => Some(val),
            Entry::Vacant { .. } => None,
        }
    }

    /// Iterate over all occupied entries.
    pub fn iter(&self) -> impl Iterator<Item = (SlabKey, &T)> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| match entry {
                Entry::Occupied(val) => Some((i as SlabKey, val)),
                Entry::Vacant { .. } => None,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_and_get() {
        let mut slab = Slab::with_capacity(4);
        let k0 = slab.insert(10);
        let k1 = slab.insert(20);
        let k2 = slab.insert(30);
        assert_eq!(slab.len(), 3);
        assert_eq!(slab.get(k0), Some(&10));
        assert_eq!(slab.get(k1), Some(&20));
        assert_eq!(slab.get(k2), Some(&30));
    }

    #[test]
    fn test_remove_and_reuse() {
        let mut slab = Slab::with_capacity(4);
        let _k0 = slab.insert("a");
        let k1 = slab.insert("b");
        let _k2 = slab.insert("c");

        let removed = slab.remove(k1);
        assert_eq!(removed, "b");
        assert_eq!(slab.len(), 2);
        assert_eq!(slab.get(k1), None);

        // Next insert should reuse slot k1.
        let k3 = slab.insert("d");
        assert_eq!(k3, k1);
        assert_eq!(slab.get(k3), Some(&"d"));
    }

    #[test]
    fn test_remove_all_and_reinsert() {
        let mut slab = Slab::with_capacity(2);
        let k0 = slab.insert(1);
        let k1 = slab.insert(2);
        slab.remove(k0);
        slab.remove(k1);
        assert_eq!(slab.len(), 0);
        assert!(slab.is_empty());

        let k2 = slab.insert(3);
        assert_eq!(slab.len(), 1);
        assert_eq!(slab.get(k2), Some(&3));
    }

    #[test]
    fn test_get_mut() {
        let mut slab = Slab::with_capacity(2);
        let k = slab.insert(42);
        *slab.get_mut(k).unwrap() = 99;
        assert_eq!(slab.get(k), Some(&99));
    }

    #[test]
    fn test_iter() {
        let mut slab = Slab::with_capacity(4);
        slab.insert(10);
        let k1 = slab.insert(20);
        slab.insert(30);
        slab.remove(k1);

        let items: Vec<_> = slab.iter().map(|(_, v)| *v).collect();
        assert_eq!(items, vec![10, 30]);
    }

    #[test]
    #[should_panic(expected = "removing vacant slab entry")]
    fn test_double_remove_panics() {
        let mut slab = Slab::with_capacity(2);
        let k = slab.insert(1);
        slab.remove(k);
        slab.remove(k); // should panic
    }
}
