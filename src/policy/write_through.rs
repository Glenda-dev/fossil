use super::WritePolicy;
use crate::fossil::CacheBlock;

pub struct WriteThrough;

impl WritePolicy for WriteThrough {
    fn should_write_through(&self, _block: &CacheBlock) -> bool {
        true
    }
    fn needs_flush_on_evict(&self, _block: &CacheBlock) -> bool {
        false
    }
}
