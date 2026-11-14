//! The map from order id to resting order. Only orders resting in the book are in it.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::mem;
use std::ops::Index;

use crate::types::{OrderId, Price, Qty, Side};

#[derive(Clone, Copy, Debug)]
pub struct RestingOrder {
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
}

/// Iteration order of this map never reaches the output: every query walks the levels instead.
pub type OrderMap = IdMap<RestingOrder>;

/// An open-addressing map from order id to `V`, with each key stored next to its value.
///
/// The standard map keeps a separate array of control bytes, so a lookup in a table that is not
/// in cache touches two cache lines: the control bytes, then the slot. Here a lookup reads the
/// slot at the id's home position and walks forward until it finds the id or an empty slot,
/// which at a load of at most one half is usually within the first cache line it touches.
/// Removal shifts the following entries back instead of leaving tombstones, so a long run of
/// adds and deletes (a trading day) never degrades the table.
///
/// It has only the operations the book uses, with the standard map's signatures, and no
/// iteration outside the microbenchmarks' `internals` feature, so the book cannot depend on slot
/// order. It never shrinks, like the standard map.
/// shortcut: the load limit is one half, which costs about twice the slots of the standard map's
/// seven eighths; if memory per book matters more than probe length, raise it with Robin Hood
/// ordering so misses stay short.
#[derive(Debug)]
pub struct IdMap<V> {
    /// Empty, or a power of two in length with at most half the slots full.
    slots: Vec<Option<(OrderId, V)>>,
    len: usize,
    /// Keys the hash so that ids chosen by an outside party cannot be aimed at one run of slots
    /// without learning it. Drawn from the standard library's per-process random keys.
    seed: u64,
}

impl<V> Default for IdMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> IdMap<V> {
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            len: 0,
            seed: RandomState::new().build_hasher().finish(),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn contains_key(&self, id: &OrderId) -> bool {
        self.find(*id).is_some()
    }

    pub fn get(&self, id: &OrderId) -> Option<&V> {
        let i = self.find(*id)?;
        self.slots[i].as_ref().map(|(_, value)| value)
    }

    pub fn get_mut(&mut self, id: &OrderId) -> Option<&mut V> {
        let i = self.find(*id)?;
        self.slots[i].as_mut().map(|(_, value)| value)
    }

    /// Inserts or overwrites, returning the old value as the standard map does.
    pub fn insert(&mut self, id: OrderId, value: V) -> Option<V> {
        if (self.len + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let mask = self.slots.len() - 1;
        let mut i = self.home(id);
        loop {
            let slot = &mut self.slots[i];
            match slot {
                Some((key, old)) if *key == id => return Some(mem::replace(old, value)),
                Some(_) => i = (i + 1) & mask,
                None => {
                    *slot = Some((id, value));
                    self.len += 1;
                    return None;
                }
            }
        }
    }

    pub fn remove(&mut self, id: &OrderId) -> Option<V> {
        let mut hole = self.find(*id)?;
        let (_, value) = self.slots[hole].take()?;
        self.len -= 1;
        // Backward-shift deletion: move each later entry of the run into the hole when the hole
        // lies between its home and its slot, so every entry stays reachable from its home.
        let mask = self.slots.len() - 1;
        let mut j = (hole + 1) & mask;
        while let Some((key, _)) = &self.slots[j] {
            let home = self.home(*key);
            if j.wrapping_sub(home) & mask >= j.wrapping_sub(hole) & mask {
                self.slots[hole] = self.slots[j].take();
                hole = j;
            }
            j = (j + 1) & mask;
        }
        Some(value)
    }

    /// The values in slot order, with the standard map's signature, only for the microbenchmark's
    /// check of the resting shares after a pass (`internals`); the book never iterates the map.
    #[cfg(feature = "internals")]
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.slots.iter().flatten().map(|(_, value)| value)
    }

    /// The slot holding `id`. Terminates because at least half the slots are empty.
    fn find(&self, id: OrderId) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut i = self.home(id);
        loop {
            match &self.slots[i] {
                Some((key, _)) if *key == id => return Some(i),
                Some(_) => i = (i + 1) & mask,
                None => return None,
            }
        }
    }

    /// Only called with a non-empty table.
    fn home(&self, id: OrderId) -> usize {
        // Truncating to usize keeps the low bits, which is all the mask uses.
        (mix(id ^ self.seed) as usize) & (self.slots.len() - 1)
    }

    fn grow(&mut self) {
        let capacity = (self.slots.len() * 2).max(8);
        let mut fresh = Vec::new();
        fresh.resize_with(capacity, || None);
        let old = mem::replace(&mut self.slots, fresh);
        let mask = capacity - 1;
        for (id, value) in old.into_iter().flatten() {
            let mut i = self.home(id);
            while self.slots[i].is_some() {
                i = (i + 1) & mask;
            }
            self.slots[i] = Some((id, value));
        }
    }
}

impl<V> Index<&OrderId> for IdMap<V> {
    type Output = V;

    fn index(&self, id: &OrderId) -> &V {
        self.get(id).expect("id is in the map")
    }
}

/// MurmurHash3's 64-bit finalizer: a bijection on `u64` in which every output bit depends on
/// every input bit. The slot comes from the low bits, so a plain multiply would put ids that
/// differ only in high bits (or all multiples of a power of two) in one run of slots; this does
/// not. Sequential, sparse increasing, clustered and high-bit-only ids all spread; see the tests.
fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^ (x >> 33)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::{IdMap, mix};

    const SEEDS: [u64; 3] = [0, 0x9e37_79b9_7f4a_7c15, u64::MAX];

    /// Checks that the 1024 ids hash to distinct values and that their low 10 bits (the slot in
    /// a 1024-slot table) use a fair share of slots.
    fn check_spread(ids: &[u64]) {
        assert_eq!(ids.len(), 1024);
        for seed in SEEDS {
            let hashes: Vec<u64> = ids.iter().map(|&id| mix(id ^ seed)).collect();
            let distinct: HashSet<u64> = hashes.iter().copied().collect();
            assert_eq!(distinct.len(), ids.len(), "seed {seed:#x}: hashes collide");
            // 1024 random keys in 1024 slots fill about 1 - 1/e = 63% of them.
            let homes: HashSet<u64> = hashes.iter().map(|h| h & 1023).collect();
            assert!(
                homes.len() > 560,
                "seed {seed:#x}: {} of 1024 slots used",
                homes.len()
            );
        }
    }

    #[test]
    fn hash_spreads_every_id_pattern() {
        check_spread(&(0..1024).collect::<Vec<_>>());
        check_spread(&(0..1024).map(|i| 1_000_000 + i * 37).collect::<Vec<_>>());
        check_spread(&(0..1024).map(|i| i * 1024).collect::<Vec<_>>());
        check_spread(&(0..1024).map(|i| i << 54).collect::<Vec<_>>());
        check_spread(&(0..1024).map(|i| (i << 40) | 7).collect::<Vec<_>>());
        let mut ends: Vec<u64> = (0..512).collect();
        ends.extend((0..512).map(|i| u64::MAX - i));
        check_spread(&ends);
    }

    /// Every entry is reachable from its home and the count matches.
    fn check_table(map: &IdMap<u32>) {
        let full: Vec<u64> = map.slots.iter().flatten().map(|&(id, _)| id).collect();
        assert_eq!(full.len(), map.len);
        assert!(map.slots.is_empty() || map.len * 2 <= map.slots.len());
        for id in full {
            assert!(map.contains_key(&id), "id {id} unreachable from its home");
        }
    }

    /// Applies a pseudo-random mix of inserts, overwrites and removes over `ids` to the map and
    /// to the standard map, comparing every result and the whole content as it goes.
    fn differential(ids: &[u64], steps: usize) {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut map = IdMap::new();
        let mut want = HashMap::new();
        for step in 0..steps {
            let id = ids[(next() % ids.len() as u64) as usize];
            let value = step as u32;
            if next() % 3 == 0 {
                assert_eq!(map.remove(&id), want.remove(&id), "remove {id}");
            } else {
                assert_eq!(map.insert(id, value), want.insert(id, value), "insert {id}");
            }
            assert_eq!(map.len(), want.len());
            if step % 64 == 0 {
                check_table(&map);
                for &id in ids {
                    assert_eq!(map.get(&id), want.get(&id), "get {id}");
                }
            }
        }
        check_table(&map);
        for &id in ids {
            assert_eq!(map.get(&id), want.get(&id), "get {id}");
        }
    }

    #[test]
    fn matches_the_standard_map_on_small_tables() {
        // A pool of 12 keys keeps the table at 8 to 32 slots, where runs often wrap around.
        let small: Vec<u64> = (0..12).collect();
        differential(&small, 5_000);
        let ends = [0, 1, 2, u64::MAX, u64::MAX - 1, 1 << 63, 1 << 32, 7 << 40];
        differential(&ends, 5_000);
    }

    #[test]
    fn matches_the_standard_map_on_id_patterns() {
        differential(&(0..2_000).collect::<Vec<_>>(), 20_000);
        differential(&(0..2_000).map(|i| i << 52).collect::<Vec<_>>(), 20_000);
        differential(
            &(0..2_000)
                .map(|i| (1 << 40) | (i * 4096))
                .collect::<Vec<_>>(),
            20_000,
        );
    }

    #[test]
    fn get_mut_and_index_see_the_same_value() {
        let mut map = IdMap::new();
        assert_eq!(map.get_mut(&5), None);
        map.insert(5, 10);
        *map.get_mut(&5).unwrap() += 1;
        assert_eq!(map[&5], 11);
        assert!(!map.contains_key(&6));
    }

    #[test]
    fn delete_heavy_churn_keeps_the_table_small() {
        // A day of adds and deletes with increasing ids and a bounded live set.
        let mut map = IdMap::new();
        for id in 0..100_000u64 {
            map.insert(id, 0u32);
            if id >= 100 {
                assert_eq!(map.remove(&(id - 100)), Some(0));
            }
        }
        assert_eq!(map.len(), 100);
        assert!(map.slots.len() <= 256);
        check_table(&map);
    }
}
