//! The map from order id to resting order. Only orders resting in the book are in it.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

use crate::types::{OrderId, Price, Qty, Side};

#[derive(Clone, Copy, Debug)]
pub struct RestingOrder {
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
    pub slot: u32,
}

pub type OrderMap = HashMap<OrderId, RestingOrder, IdHashBuilder>;

#[derive(Clone, Copy, Debug)]
pub struct IdHashBuilder {
    seed: u64,
}

impl Default for IdHashBuilder {
    fn default() -> Self {
        Self {
            seed: RandomState::new().build_hasher().finish(),
        }
    }
}

impl BuildHasher for IdHashBuilder {
    type Hasher = IdHasher;

    fn build_hasher(&self) -> IdHasher {
        IdHasher { hash: self.seed }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct IdHasher {
    hash: u64,
}

impl Hasher for IdHasher {
    fn write_u64(&mut self, id: u64) {
        self.hash = mix(self.hash ^ id);
    }

    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_le_bytes(word));
        }
    }

    fn finish(&self) -> u64 {
        self.hash
    }
}

fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^ (x >> 33)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::hash::{BuildHasher, Hasher};

    use super::{IdHashBuilder, IdHasher};

    const SEEDS: [u64; 3] = [0, 0x9e37_79b9_7f4a_7c15, u64::MAX];

    fn hash(seed: u64, id: u64) -> u64 {
        let mut hasher = IdHashBuilder { seed }.build_hasher();
        hasher.write_u64(id);
        hasher.finish()
    }

    fn check_spread(ids: &[u64]) {
        assert_eq!(ids.len(), 1024);
        for seed in SEEDS {
            let hashes: Vec<u64> = ids.iter().map(|&id| hash(seed, id)).collect();
            let distinct: HashSet<u64> = hashes.iter().copied().collect();
            assert_eq!(distinct.len(), ids.len(), "seed {seed:#x}: hashes collide");
            let buckets: HashSet<u64> = hashes.iter().map(|h| h & 1023).collect();
            assert!(
                buckets.len() > 560,
                "seed {seed:#x}: {} of 1024 buckets used",
                buckets.len()
            );
            let tags: HashSet<u64> = hashes.iter().map(|h| h >> 57).collect();
            assert!(
                tags.len() > 110,
                "seed {seed:#x}: {} of 128 tags",
                tags.len()
            );
        }
    }

    #[test]
    fn sequential_ids_spread() {
        check_spread(&(0..1024).collect::<Vec<_>>());
    }

    #[test]
    fn sparse_increasing_ids_spread() {
        check_spread(&(0..1024).map(|i| 1_000_000 + i * 37).collect::<Vec<_>>());
        check_spread(&(0..1024).map(|i| i * 1024).collect::<Vec<_>>());
    }

    #[test]
    fn high_bit_only_ids_spread() {
        check_spread(&(0..1024).map(|i| i << 54).collect::<Vec<_>>());
        check_spread(&(0..1024).map(|i| (i << 40) | 7).collect::<Vec<_>>());
    }

    #[test]
    fn extreme_ids_spread() {
        let mut ids: Vec<u64> = (0..512).collect();
        ids.extend((0..512).map(|i| u64::MAX - i));
        check_spread(&ids);
        let ends = [0, 1, u64::MAX, u64::MAX - 1, 1 << 63];
        for seed in SEEDS {
            let hashes: HashSet<u64> = ends.iter().map(|&id| hash(seed, id)).collect();
            assert_eq!(hashes.len(), ends.len());
        }
    }

    #[test]
    fn seed_changes_the_hash() {
        assert_ne!(hash(1, 42), hash(2, 42));
        assert_ne!(IdHashBuilder::default().seed, IdHashBuilder::default().seed);
    }

    #[test]
    fn write_matches_write_u64_for_eight_bytes() {
        let mut a = IdHasher { hash: 5 };
        a.write(&42u64.to_le_bytes());
        let mut b = IdHasher { hash: 5 };
        b.write_u64(42);
        assert_eq!(a.finish(), b.finish());
    }
}
