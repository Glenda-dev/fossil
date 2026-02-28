use crate::fossil::CacheBlock;

pub trait ReplacementPolicy: Send + Sync {
    fn on_access(&mut self, block_idx: usize);
    fn select_evict(&self, blocks: &[CacheBlock]) -> usize;
}

pub trait WritePolicy: Send + Sync {
    /// decide whether to write through to disk immediately
    fn should_write_through(&self, block: &CacheBlock) -> bool;
    /// decide whether we need to flush this block before eviction
    fn needs_flush_on_evict(&self, block: &CacheBlock) -> bool;
}

pub mod lru;
pub mod write_through;

pub use lru::LruPolicy;
pub use write_through::WriteThrough;
