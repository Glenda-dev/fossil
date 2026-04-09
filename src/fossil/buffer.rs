use crate::policy::{LruPolicy, ReplacementPolicy, WritePolicy, WriteThrough};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use glenda::io::uring::IoUringSqe;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockState {
    Invalid,
    Valid,
    Dirty,
    Filling,
}

#[derive(Debug, Clone)]
pub struct CacheBlock {
    pub device_id: usize,
    pub sector_idx: usize,
    pub state: BlockState,
    pub buf_offset: usize,  // Offset in the global buffer
    pub last_access: usize, // For LRU
}

pub struct BufferCache {
    base_vaddr: usize,
    total_size: usize,
    block_size: usize,
    num_blocks: usize,
    blocks: Vec<CacheBlock>,
    access_counter: AtomicUsize,
    // Map (device_id, sector_idx) -> block_index
    lookup: BTreeMap<(usize, usize), usize>,
    free_blocks: Vec<usize>,
    policy: Box<dyn ReplacementPolicy>,
    write_policy: Box<dyn WritePolicy>,
}

#[derive(Debug, Clone)]
pub struct CacheResult {
    pub buf_vaddr: usize,
    pub buf_offset: usize,
    pub block_idx: usize,
    pub is_hit: bool,
}

impl BufferCache {
    pub fn new(base_vaddr: usize, total_size: usize, block_size: usize) -> Self {
        let num_blocks = if block_size > 0 { total_size / block_size } else { 0 };
        let mut blocks = Vec::with_capacity(num_blocks);
        let mut free_blocks = Vec::with_capacity(num_blocks);

        for i in 0..num_blocks {
            blocks.push(CacheBlock {
                device_id: 0,
                sector_idx: 0,
                state: BlockState::Invalid,
                buf_offset: i * block_size,
                last_access: 0,
            });
            free_blocks.push(i);
        }

        Self {
            base_vaddr,
            total_size,
            block_size,
            num_blocks,
            blocks,
            access_counter: AtomicUsize::new(0),
            lookup: BTreeMap::new(),
            free_blocks,
            policy: Box::new(LruPolicy),
            write_policy: Box::new(WriteThrough),
        }
    }

    pub fn set_policy(&mut self, policy: Box<dyn ReplacementPolicy>) {
        self.policy = policy;
    }

    pub fn set_write_policy(&mut self, policy: Box<dyn WritePolicy>) {
        self.write_policy = policy;
    }

    pub fn should_write_through(&self, idx: usize) -> bool {
        self.write_policy.should_write_through(&self.blocks[idx])
    }

    pub fn needs_flush_on_evict(&self, idx: usize) -> bool {
        self.write_policy.needs_flush_on_evict(&self.blocks[idx])
    }

    pub fn get_block_size(&self) -> usize {
        self.block_size
    }

    pub fn get_base_vaddr(&self) -> usize {
        self.base_vaddr
    }

    /// Access a block. If present, returns index and update LRU.
    /// If absent, evict LRU or use free block, return index.
    pub fn access_block(
        &mut self,
        device_id: usize,
        sector_idx: usize,
        sectors_per_cache_block: usize,
    ) -> CacheResult {
        let access_time = self.access_counter.fetch_add(1, Ordering::SeqCst) as usize;

        let sectors_per_cache_block = core::cmp::max(1, sectors_per_cache_block);
        let cache_sector_idx = (sector_idx / sectors_per_cache_block) * sectors_per_cache_block;

        if let Some(&idx) = self.lookup.get(&(device_id, cache_sector_idx)) {
            // Cache Hit
            let block = &mut self.blocks[idx];
            block.last_access = access_time;
            self.policy.on_access(idx);
            return CacheResult {
                buf_vaddr: self.base_vaddr + block.buf_offset,
                buf_offset: block.buf_offset,
                block_idx: idx,
                is_hit: true,
            };
        }

        // Cache Miss
        let idx = if let Some(free_idx) = self.free_blocks.pop() {
            free_idx
        } else {
            // Use the replacement policy to select a block to evict
            let lru_idx = self.policy.select_evict(&self.blocks);

            // TODO: Handle dirty pages (write back before eviction)
            // For now, assume direct write policy so no dirty blocks in cache effectively.
            if self.blocks[lru_idx].state == BlockState::Dirty {
                // TODO: Initiating write back would be async here...
                // Ideally return a flag saying "Needs Flush"
            }

            // Remove old mapping
            let old_dev = self.blocks[lru_idx].device_id;
            let old_sec = self.blocks[lru_idx].sector_idx;
            self.lookup.remove(&(old_dev, old_sec));

            lru_idx
        };

        // Initialize new block
        let block = &mut self.blocks[idx];
        block.device_id = device_id;
        block.sector_idx = cache_sector_idx;
        block.state = BlockState::Invalid; // Caller must fill it
        block.last_access = access_time;

        self.lookup.insert((device_id, cache_sector_idx), idx);
        self.policy.on_access(idx);

        CacheResult {
            buf_vaddr: self.base_vaddr + block.buf_offset,
            buf_offset: block.buf_offset,
            block_idx: idx,
            is_hit: false,
        }
    }

    pub fn mark_valid(&mut self, idx: usize) {
        if idx < self.blocks.len() {
            self.blocks[idx].state = BlockState::Valid;
        }
    }

    pub fn mark_dirty(&mut self, idx: usize) {
        if idx < self.blocks.len() {
            self.blocks[idx].state = BlockState::Dirty;
        }
    }

    pub fn invalidate(&mut self, idx: usize) {
        if idx < self.blocks.len() {
            let block = &mut self.blocks[idx];
            block.state = BlockState::Invalid;
            // self.lookup.remove(&(block.device_id, block.sector_idx)); // Already removed from logic? No.
            // If explicit invalidation:
            // self.lookup.remove ...
            // self.free_blocks.push ...
        }
    }
}

pub struct IOBufferManager;

impl IOBufferManager {
    /// Checks if the request is aligned to the block_size.
    pub fn is_aligned(sqe: &IoUringSqe, block_size: u32) -> bool {
        (sqe.off % block_size as usize == 0) && (sqe.len % block_size == 0)
    }

    // Moved check_alignment_and_create_context logic to Server
}

#[derive(Debug, Clone, Copy)]
pub struct BufferInfo {
    pub original_addr: usize,
    pub original_len: u32,
    pub original_offset: usize,
    pub aligned_offset: usize,
    pub aligned_len: u32,
    pub is_write: bool,           // true if write op
    pub is_rmw: bool,             // true if needs RMW
    pub cache_idx: Option<usize>, // Track associated cache block
}

pub struct RequestContext {
    pub client_badge: usize,
    pub client_user_data: usize,
    pub buffer_info: Option<BufferInfo>,
}
