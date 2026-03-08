use crate::fossil::sniffer;
use crate::fossil::{FossilServer, buffer};
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use glenda::cap::{CSPACE_CAP, CapPtr, Endpoint};
use glenda::drivers::client::ShmParams;
use glenda::error::Error;
use glenda::interface::CSpaceService;
use glenda::io::uring::{
    IOURING_OP_WRITE, IoUringBuffer as IoUring, IoUringServer, IoUringSqe as SQE,
};
use glenda::ipc::{Badge, UTCB};
use glenda::mem::pool::ShmType;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionMetadata {
    pub parent: u64,
    pub start_lba: u64,
    pub num_blocks: u64,
    pub block_size: u32,
}

pub struct PartitionProxy {
    pub meta: PartitionMetadata,
    pub hardware_ep: Endpoint,
    pub name: String,
    pub fs_type: sniffer::FileSystemType,
    pub hardware_badge: Badge,
}

impl PartitionProxy {
    pub fn new(
        meta: PartitionMetadata,
        hardware_ep: Endpoint,
        name: String,
        fs_type: sniffer::FileSystemType,
        hardware_badge: Badge,
    ) -> Self {
        Self { meta, hardware_ep, name, fs_type, hardware_badge }
    }

    pub fn translate_sqe(&self, sqe: &mut SQE) {
        sqe.off += self.meta.start_lba * 512;
    }
}

impl<'a> FossilServer<'a> {
    pub fn handle_acquire_shm(&mut self, utcb: &mut UTCB) -> Result<CapPtr, Error> {
        if let Some(shm) = &self.global_shm {
            utcb.set_mr(0, shm.vaddr());
            utcb.set_mr(1, shm.size());
            // DO NOT set MR2 - physical address is hidden from VolumeClient
            Ok(shm.frame().cap())
        } else {
            Err(Error::NotInitialized)
        }
    }

    pub fn handle_register_shm(&mut self, utcb: &mut UTCB, badge: Badge) -> Result<(), Error> {
        let client_vaddr = utcb.get_mr(0);
        let client_size = utcb.get_mr(1);

        if let Some(shm) = &self.global_shm {
            // For VolumeClient (like InitrdFS), we explicitly hide the physical address
            // to ensure isolation. The proxy translates addresses to Fossil's view.
            self.client_shms.insert(
                badge.bits(),
                ShmParams {
                    frame: shm.frame().clone(),
                    vaddr: client_vaddr,
                    paddr: 0, // Shielded for non-hardware clients
                    size: client_size,
                    recv_slot: CapPtr::null(),
                },
            );
            Ok(())
        } else {
            Err(Error::NotInitialized)
        }
    }

    pub fn handle_setup_ring(&mut self, utcb: &mut UTCB, badge: Badge) -> Result<CapPtr, Error> {
        let sq_entries = utcb.get_mr(0) as u32;
        let cq_entries = utcb.get_mr(1) as u32;

        if self.client_rings.contains_key(&badge.bits()) {
            return Err(Error::AlreadyExists);
        }
        let notify_slot = self.cspace.alloc(self.res_client).map_err(|_| Error::OutOfMemory)?;
        CSPACE_CAP.move_cap(self.recv, notify_slot)?;
        let notify_ep = Endpoint::from(notify_slot);

        let ring_size = 64 + (sq_entries as usize * 64) + (cq_entries as usize * 16);

        let slot = self.cspace.alloc(self.res_client)?;
        let shm = self.mem_pool.alloc_shm(
            self.vspace,
            self.cspace,
            self.res_client,
            ring_size,
            ShmType::Regular,
            slot,
        )?;

        let ring =
            unsafe { IoUring::new(shm.vaddr() as *mut u8, shm.size(), sq_entries, cq_entries) };
        let mut server = IoUringServer::new(ring);
        server.set_client_notify(notify_ep);
        self.client_rings.insert(badge.bits(), server);
        Ok(shm.frame().cap())
    }

    pub fn handle_notify_sq(&mut self) -> Result<(), Error> {
        let rings_to_process: Vec<usize> = self.client_rings.keys().cloned().collect();

        for badge_bits in rings_to_process {
            let proxy = self.partitions.get(&badge_bits).ok_or(Error::NotFound)?;
            let hw_id = proxy.meta.parent as usize;
            let hw_client = self.device_clients.get(&hw_id).ok_or(Error::NotFound)?;
            let hw_ring = hw_client.ring().ok_or(Error::NotInitialized)?;

            let ring = self.client_rings.get_mut(&badge_bits).ok_or(Error::NotInitialized)?;

            while let Some(mut sqe) = ring.ring.pop_sqe() {
                proxy.translate_sqe(&mut sqe);

                if let Some(shm) = self.client_shms.get(&badge_bits) {
                    let addr = sqe.addr as usize;
                    if addr >= shm.vaddr && addr < shm.vaddr + shm.size {
                        let offset = addr - shm.vaddr;
                        if let Some(gshm) = &self.global_shm {
                            sqe.addr = (gshm.vaddr() + offset) as u64;
                        }
                    }
                }

                let hw_user_data = self.next_partition_badge.fetch_add(1, Ordering::Relaxed) as u64
                    | 0x8000000000000000;

                let client_user_data = sqe.user_data;
                sqe.user_data = hw_user_data;

                if buffer::IOBufferManager::is_aligned(&sqe, 4096) {
                    let ctx = buffer::RequestContext {
                        client_badge: badge_bits,
                        client_user_data,
                        buffer_info: None,
                    };
                    self.inflight_requests.insert(hw_user_data, ctx);
                    hw_ring.submit(sqe)?;
                } else {
                    let aligned_offset = sqe.off & !7u64; // Aligned to 4KB (8 sectors)
                    let end_offset = (sqe.off + (sqe.len as u64 + 511) / 512 + 7) & !7u64;
                    let aligned_len_sectors = (end_offset - aligned_offset) as u32;

                    let cache_res = self.buffer_cache.access_block(hw_id, aligned_offset);
                    let is_rmw = sqe.opcode == IOURING_OP_WRITE
                        && (sqe.off != aligned_offset || sqe.len != aligned_len_sectors * 512);

                    let buffer_info = buffer::BufferInfo {
                        original_addr: sqe.addr,
                        original_len: sqe.len,
                        original_offset: sqe.off,
                        aligned_offset,
                        aligned_len: aligned_len_sectors * 512,
                        is_write: sqe.opcode == IOURING_OP_WRITE,
                        is_rmw,
                        cache_idx: Some(cache_res.block_idx),
                    };

                    let ctx = buffer::RequestContext {
                        client_badge: badge_bits,
                        client_user_data,
                        buffer_info: Some(buffer_info),
                    };

                    sqe.addr = cache_res.buf_vaddr as u64;
                    sqe.off = aligned_offset;
                    sqe.len = aligned_len_sectors * 512;

                    self.inflight_requests.insert(hw_user_data, ctx);

                    if !cache_res.is_hit || is_rmw {
                        // For 512B devices, if not hit, we read the whole 4KB block.
                        // If it's an unaligned write (RMW), we MUST read first.
                        sqe.opcode = glenda::io::uring::IOURING_OP_READ;
                        hw_ring.submit(sqe)?;
                    } else if buffer_info.is_write {
                        // Aligned write
                        // Check Write Policy
                        if self.buffer_cache.should_write_through(cache_res.block_idx) {
                            hw_ring.submit(sqe)?;
                        } else {
                            // Write-back Logic: just mark dirty and complete to client
                            self.buffer_cache.mark_dirty(cache_res.block_idx);
                            let res = buffer_info.original_len as i32;
                            self.inflight_requests.remove(&hw_user_data);
                            ring.complete(client_user_data, res).ok();
                        }
                    } else {
                        // Cache hit for read, complete immediately.
                        let res = buffer_info.original_len as i32;
                        // Copy to client buffer
                        if let Some(shm) = self.client_shms.get(&badge_bits) {
                            if let Some(gshm) = &self.global_shm {
                                let src_ptr = (cache_res.buf_vaddr
                                    + (buffer_info.original_offset - aligned_offset) as usize)
                                    as *const u8;
                                let dst_offset = buffer_info.original_addr as usize - shm.vaddr;
                                let dst_ptr = (gshm.vaddr() + dst_offset) as *mut u8;
                                unsafe {
                                    core::ptr::copy_nonoverlapping(
                                        src_ptr,
                                        dst_ptr,
                                        buffer_info.original_len as usize,
                                    );
                                }
                            }
                        }
                        self.inflight_requests.remove(&hw_user_data);
                        ring.complete(client_user_data, res).ok();
                    }
                }
            }
        }
        Ok(())
    }

    pub fn handle_notify_cq(&mut self) -> Result<(), Error> {
        let hardware_ids: Vec<usize> = self.device_clients.keys().cloned().collect();

        for hw_id in hardware_ids {
            if let Some(bclient) = self.device_clients.get(&hw_id) {
                if let Some(ring) = bclient.ring() {
                    while let Some(cqe) = ring.pop_completion() {
                        if let Some(ctx) = self.inflight_requests.remove(&cqe.user_data) {
                            let mut res = cqe.res;
                            if let Some(buf_info) = ctx.buffer_info {
                                if cqe.res >= 0 {
                                    res = buf_info.original_len as i32;

                                    // If this request was into cache, and it's a read, copy to client.
                                    if !buf_info.is_write {
                                        // Update Cache State
                                        if let Some(cidx) = buf_info.cache_idx {
                                            self.buffer_cache.mark_valid(cidx);

                                            // Copy to client buffer
                                            if let Some(shm) =
                                                self.client_shms.get(&ctx.client_badge)
                                            {
                                                if let Some(gshm) = &self.global_shm {
                                                    let aligned_offset = buf_info.aligned_offset;
                                                    let original_offset = buf_info.original_offset;

                                                    // cache_res vaddr is base + offset
                                                    let cache_vaddr =
                                                        self.buffer_cache.get_base_vaddr()
                                                            + (cidx * 4096);
                                                    let src_ptr = (cache_vaddr
                                                        + (original_offset - aligned_offset)
                                                            as usize)
                                                        as *const u8;
                                                    let dst_offset =
                                                        buf_info.original_addr as usize - shm.vaddr;
                                                    let dst_ptr =
                                                        (gshm.vaddr() + dst_offset) as *mut u8;
                                                    unsafe {
                                                        core::ptr::copy_nonoverlapping(
                                                            src_ptr,
                                                            dst_ptr,
                                                            buf_info.original_len as usize,
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    } else if buf_info.is_rmw {
                                        // Completed reading the block for RMW
                                        if let Some(cidx) = buf_info.cache_idx {
                                            self.buffer_cache.mark_valid(cidx);

                                            // 1. Modify the cached block with original write data
                                            if let Some(shm) =
                                                self.client_shms.get(&ctx.client_badge)
                                            {
                                                if let Some(gshm) = &self.global_shm {
                                                    let aligned_offset = buf_info.aligned_offset;
                                                    let original_offset = buf_info.original_offset;

                                                    let cache_vaddr =
                                                        self.buffer_cache.get_base_vaddr()
                                                            + (cidx * 4096);
                                                    let dst_ptr = (cache_vaddr
                                                        + (original_offset - aligned_offset)
                                                            as usize)
                                                        as *mut u8;
                                                    let src_offset =
                                                        buf_info.original_addr as usize - shm.vaddr;
                                                    let src_ptr =
                                                        (gshm.vaddr() + src_offset) as *const u8;
                                                    unsafe {
                                                        core::ptr::copy_nonoverlapping(
                                                            src_ptr,
                                                            dst_ptr,
                                                            buf_info.original_len as usize,
                                                        );
                                                    }
                                                }
                                            }

                                            // 2. Write modification back to disk (Based on Write Policy)
                                            if self.buffer_cache.should_write_through(cidx) {
                                                let parent_hw_id = if let Some(p) =
                                                    self.partitions.get(&ctx.client_badge)
                                                {
                                                    p.meta.parent as usize
                                                } else {
                                                    0
                                                };

                                                if let Some(hw_client) =
                                                    self.device_clients.get(&parent_hw_id)
                                                {
                                                    if let Some(hw_ring) = hw_client.ring() {
                                                        let hw_user_data = self
                                                            .next_partition_badge
                                                            .fetch_add(1, Ordering::Relaxed)
                                                            as u64
                                                            | 0x8000000000000000;

                                                        let mut sqe =
                                                            glenda::io::uring::IoUringSqe::default(
                                                            );
                                                        sqe.opcode =
                                                            glenda::io::uring::IOURING_OP_WRITE;
                                                        sqe.addr =
                                                            (self.buffer_cache.get_base_vaddr()
                                                                + cidx * 4096)
                                                                as u64;
                                                        sqe.off = buf_info.aligned_offset;
                                                        sqe.len = buf_info.aligned_len;
                                                        sqe.user_data = hw_user_data;

                                                        let mut new_buf_info = buf_info;
                                                        new_buf_info.is_rmw = false; // Final write phase

                                                        let new_ctx = buffer::RequestContext {
                                                            client_badge: ctx.client_badge,
                                                            client_user_data: ctx.client_user_data,
                                                            buffer_info: Some(new_buf_info),
                                                        };
                                                        self.inflight_requests
                                                            .insert(hw_user_data, new_ctx);
                                                        hw_ring.submit(sqe).ok();
                                                        continue; // Don't complete to client yet
                                                    }
                                                }
                                            } else {
                                                // Write-back: just mark dirty and complete
                                                self.buffer_cache.mark_dirty(cidx);
                                            }
                                        }
                                    }
                                }
                            }

                            if let Some(client_ring) = self.client_rings.get_mut(&ctx.client_badge)
                            {
                                let _ = client_ring.complete(ctx.client_user_data, res);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
