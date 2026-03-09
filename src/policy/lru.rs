use super::ReplacementPolicy;
use crate::fossil::CacheBlock;

pub struct LruPolicy;

impl ReplacementPolicy for LruPolicy {
    fn on_access(&mut self, _block_idx: usize) {}

    fn select_evict(&self, blocks: &[CacheBlock]) -> usize {
        let mut lru_idx = 0;
        let mut min_access = usize::MAX;

        for (i, block) in blocks.iter().enumerate() {
            if block.last_access < min_access {
                min_access = block.last_access;
                lru_idx = i;
            }
        }
        lru_idx
    }
}
