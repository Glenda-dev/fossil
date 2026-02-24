use crate::layout::PROBE_VADDR;
use crate::utils::gpt::{GPTHeader, GPTPartition};
use crate::utils::mbr::MBR;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use glenda::arch::mem::PGSIZE;
use glenda::cap::{CapPtr, Endpoint, Frame, Reply, Rights};
use glenda::client::{DeviceClient, InitClient, ProcessClient, ResourceClient};
use glenda::error::Error;
use glenda::interface::device::DeviceService;
use glenda::interface::{MemoryService, ProcessService, ResourceService};
use glenda::io::uring::{IoUringBuffer as IoUring, IoUringServer};
use glenda::ipc::Badge;
use glenda::protocol::device::LogicDeviceType;
use glenda::utils::manager::{CSpaceManager, CSpaceService};
use glenda_drivers::client::block::BlockClient;
use glenda_drivers::interface::BlockDriver;
use serde::{Deserialize, Serialize};

mod buffer;
mod server;
pub mod sniffer;
mod volume;

use alloc::collections::VecDeque;
pub use buffer::RequestContext;

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

    pub fn translate_sqe(&self, sqe: &mut glenda::io::uring::IoUringSqe) {
        sqe.off += self.meta.start_lba * 512;
    }
}

pub struct FossilServer<'a> {
    pub endpoint: Endpoint,
    pub reply: Reply,
    pub recv: CapPtr,
    pub res_client: &'a mut ResourceClient,
    pub device_client: &'a mut DeviceClient,
    pub process_client: &'a mut ProcessClient,
    pub cspace: &'a mut CSpaceManager,
    pub init_client: &'a mut InitClient,

    pub partitions: BTreeMap<usize, PartitionProxy>,
    pub name_to_badge: BTreeMap<String, usize>,
    pub client_rings: BTreeMap<usize, IoUringServer>,
    pub device_clients: BTreeMap<usize, BlockClient>,
    pub probed_hardware: BTreeSet<u64>,

    pub inflight_requests: BTreeMap<u64, RequestContext>,

    pub fs_config: Option<FSConfig>,
    pub driver_to_partition: BTreeMap<usize, usize>,
    pub next_partition_badge: AtomicUsize,
    pub next_ring_vaddr: AtomicUsize,
    pub running: bool,

    pub pending_devices: VecDeque<String>,

    // Block Cache
    pub buffer_cache: buffer::BufferCache,
    pub shm_frame: Option<(glenda::cap::Frame, usize, usize, usize)>, // Frame, vaddr, size, paddr
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FSDriverConfig {
    pub name: String,
    pub binary: String,
    pub compatible: Vec<String>,
    #[serde(default)]
    pub autostart: bool,
}

fn default_buffer_size() -> usize {
    2 * 1024 * 1024
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FSConfig {
    #[serde(default = "default_buffer_size")]
    pub buffer_size: usize,
    pub filesystems: Vec<FSDriverConfig>,
}

impl<'a> FossilServer<'a> {
    pub fn new(
        endpoint: Endpoint,
        res_client: &'a mut ResourceClient,
        process_client: &'a mut ProcessClient,
        cspace: &'a mut CSpaceManager,
        device_client: &'a mut DeviceClient,
        init_client: &'a mut InitClient,
    ) -> Self {
        Self {
            endpoint,
            reply: Reply::from(CapPtr::null()),
            recv: CapPtr::null(),
            res_client,
            process_client,
            device_client,
            init_client,
            cspace,
            partitions: BTreeMap::new(),
            name_to_badge: BTreeMap::new(),
            client_rings: BTreeMap::new(),
            device_clients: BTreeMap::new(),
            probed_hardware: BTreeSet::new(),
            inflight_requests: BTreeMap::new(),
            fs_config: None,
            driver_to_partition: BTreeMap::new(),
            next_partition_badge: AtomicUsize::new(0x1000),
            next_ring_vaddr: AtomicUsize::new(PROBE_VADDR + PGSIZE * 512),
            running: false,
            pending_devices: VecDeque::new(),
            buffer_cache: buffer::BufferCache::new(0, 0, 4096),
            shm_frame: None,
        }
    }

    fn probe_partitions<F>(
        &self,
        parent_desc: &glenda::protocol::device::LogicDeviceDesc,
        sector0: &[u8],
        block_size: usize,
        mut read_fn: F,
    ) -> Vec<(glenda::protocol::device::LogicDeviceDesc, u64)>
    where
        F: FnMut(u64, &mut [u8]) -> Result<(), Error>,
    {
        let mut results = Vec::new();

        // GPT header is at offset 512.
        // If block_size is 512, it's LBA 1.
        // If block_size is 4096, it's inside LBA 0 (which we already have in sector0).
        let gpt_header_buf = if block_size == 512 {
            let mut buf = [0u8; 512];
            if read_fn(1, &mut buf).is_ok() { Some(buf) } else { None }
        } else if block_size >= 1024 {
            let mut buf = [0u8; 512];
            buf.copy_from_slice(&sector0[512..1024]);
            Some(buf)
        } else {
            None
        };

        if let Some(mbr) = MBR::parse(sector0) {
            if mbr.is_protective_gpt() {
                if let Some(header_buf) = gpt_header_buf {
                    if let Some(header) = GPTHeader::parse(&header_buf) {
                        let mut entries_buf = [0u8; 128 * 128];
                        let entries_len = header.num_partition_entries as usize
                            * header.partition_entry_size as usize;
                        let entries_blk_count = (entries_len + block_size - 1) / block_size;
                        if let Ok(_) = read_fn(
                            header.partition_entry_lba,
                            &mut entries_buf[..entries_blk_count * block_size],
                        ) {
                            let gpt_parts = GPTPartition::parse_entries(
                                &entries_buf[..entries_len],
                                header.num_partition_entries,
                                header.partition_entry_size,
                            );
                            let mut part_index = 0;
                            for part in gpt_parts.iter() {
                                if part.is_active() {
                                    let name =
                                        alloc::format!("{}p{}", parent_desc.name, part_index);
                                    let desc = glenda::protocol::device::LogicDeviceDesc {
                                        name: name.clone(),
                                        dev_type: LogicDeviceType::Block(
                                            part.last_lba - part.first_lba + 1,
                                        ),
                                        parent_name: parent_desc.name.clone(),
                                        badge: None,
                                    };
                                    results.push((desc, part.first_lba));
                                    part_index += 1;
                                }
                            }
                        }
                    }
                }
            } else {
                let mut p_index = 0;
                for i in 0..4 {
                    if let Some(entry) = &mbr.partitions[i] {
                        let desc = glenda::protocol::device::LogicDeviceDesc {
                            name: alloc::format!("{}p{}", parent_desc.name, p_index),
                            dev_type: LogicDeviceType::Block(entry.sectors_count as u64),
                            parent_name: parent_desc.name.clone(),
                            badge: None,
                        };
                        results.push((desc, entry.start_lba as u64));
                        p_index += 1;
                    }
                }
            }
        }
        results
    }

    pub fn sync_devices(&mut self) -> Result<(), Error> {
        log!("Syncing logical devices from Unicorn...");
        let query = glenda::protocol::device::DeviceQuery {
            name: None,
            compatible: alloc::vec![],
            dev_type: Some(1), // Type 1 = RawBlock
        };
        let names = self.device_client.query(Badge::null(), query)?;
        for name in names {
            if !self.pending_devices.contains(&name) {
                self.pending_devices.push_back(name);
            }
        }
        Ok(())
    }

    pub fn process_pending_probes(&mut self) -> Result<(), Error> {
        while let Some(name) = self.pending_devices.pop_front() {
            let (hw_id, desc) = self.device_client.get_logic_desc(Badge::null(), &name)?;
            if let LogicDeviceType::RawBlock(_) = desc.dev_type {
                if !self.probed_hardware.contains(&hw_id) {
                    let hw_slot = self.cspace.alloc(self.res_client)?;
                    let hw_ep = self.device_client.alloc_logic(Badge::null(), 1, &name, hw_slot)?;
                    self.probe(hw_id, desc, hw_ep)?;
                }
            }
        }
        Ok(())
    }

    pub fn probe(
        &mut self,
        hardware_id: u64,
        desc: glenda::protocol::device::LogicDeviceDesc,
        hardware_ep: Endpoint,
    ) -> Result<(), Error> {
        log!("Probing device {} (hw_id={:x})", desc.name, hardware_id);
        let mut client = BlockClient::new(hardware_ep);
        client.init()?;

        // Setup ring for AIO probing
        let ring_vaddr = self.next_ring_vaddr.fetch_add(PGSIZE, Ordering::SeqCst);
        // Removed separate data_vaddr allocation as we use global SHM

        let ring_slot = self.cspace.alloc(self.res_client)?;
        // Use minted endpoint with badge identifying the hardware for notifications
        let hw_notify_slot = self.cspace.alloc(self.res_client)?;
        self.cspace.root().mint(
            self.endpoint.cap(),
            hw_notify_slot,
            Badge::new(hardware_id as usize | 0x80000000),
            Rights::ALL,
        )?;
        let hw_notify_ep = Endpoint::from(hw_notify_slot);

        // Use driver-provided ring buffer instead of allocating our own
        BlockDriver::setup_ring(&mut client, 4, 4, hw_notify_ep, self.recv)?;

        // Move the received frame cap to our managed slot
        self.cspace.root().move_cap(self.recv, ring_slot)?;
        let ring_frame = Frame::from(ring_slot);
        self.res_client.mmap(Badge::null(), ring_frame.clone(), ring_vaddr, PGSIZE)?;

        // Removed data_slot allocation as we use global SHM

        let ring_buf =
            unsafe { glenda::io::uring::IoUringBuffer::new(ring_vaddr as *mut u8, PGSIZE, 4, 4) };
        let ring = glenda::io::uring::IoUringClient::new(ring_buf);
        client.set_ring(ring);

        if let Some((shm_frame, shm_vaddr, shm_size, shm_paddr)) = &self.shm_frame {
            BlockDriver::setup_shm(
                &mut client,
                shm_frame.clone(),
                *shm_vaddr,
                *shm_paddr as u64,
                *shm_size,
            )?;
        } else {
            // Fallback for standalone/testing or explicit non-buffered?
            // But we require buffer cache now.
            return Err(Error::NotInitialized);
        }

        let block_size = client.block_size() as usize;
        let mut buf = [0u8; 4096]; // Buffer for at least one block
        client.read_blocks(0, 1, &mut buf[..block_size])?;

        let mut partitions =
            self.probe_partitions(&desc, &buf[..block_size], block_size, |lba, target| {
                let count = (target.len() + block_size - 1) / block_size;
                client.read_blocks(lba, count as u32, target)
            });

        // Fallback: If no partition table found, treat the whole device as one partition
        if partitions.is_empty() {
            partitions.push((
                glenda::protocol::device::LogicDeviceDesc {
                    name: alloc::format!("{}p0", desc.name),
                    dev_type: LogicDeviceType::Block(client.total_sectors()),
                    parent_name: desc.name.clone(),
                    badge: None,
                },
                0,
            ));
        }

        for (p_desc, first_lba) in partitions {
            let badge = Badge::new(self.next_partition_badge.fetch_add(1, Ordering::SeqCst));
            log!(
                "Found partition {} at LBA {:x}, assigning badge {:x}",
                p_desc.name,
                first_lba,
                badge.bits()
            );

            let meta = PartitionMetadata {
                parent: hardware_id,
                start_lba: first_lba,
                num_blocks: match p_desc.dev_type {
                    LogicDeviceType::Block(n) => n,
                    _ => 0,
                },
                block_size: client.block_size(),
            };

            // Sniff FS
            let fs_type = sniffer::detect_fs(|offset, target| {
                let sector = first_lba + offset / block_size as u64;
                let offset_in_sector = offset % block_size as u64;

                if offset_in_sector == 0 && target.len() >= block_size {
                    let count = (target.len() + block_size - 1) / block_size;
                    client.read_blocks(sector, count as u32, target)
                } else {
                    let mut temp = [0u8; 4096];
                    client.read_blocks(sector, 1, &mut temp[..block_size])?;
                    let copy_len =
                        core::cmp::min(target.len(), block_size - offset_in_sector as usize);
                    target[..copy_len].copy_from_slice(
                        &temp[offset_in_sector as usize..offset_in_sector as usize + copy_len],
                    );
                    Ok(())
                }
            });
            log!("Partition {} FS type: {:?}", p_desc.name, fs_type);

            let proxy =
                PartitionProxy::new(meta, hardware_ep, p_desc.name.clone(), fs_type, Badge::null());
            self.partitions.insert(badge.bits(), proxy);
            self.name_to_badge.insert(p_desc.name.clone(), badge.bits());

            // Auto-spawn if configured
            if let Some(ref config) = self.fs_config {
                let fs_type_lower = alloc::format!("{:?}", fs_type).to_lowercase();
                for driver in &config.filesystems {
                    if driver.autostart && driver.compatible.contains(&fs_type_lower) {
                        log!("Autostarting driver {} for {}", driver.name, p_desc.name);
                        match self.process_client.spawn(badge, &driver.binary) {
                            Ok(pid) => {
                                self.driver_to_partition.insert(pid, badge.bits());
                            }
                            Err(e) => {
                                error!("Fossil: Failed to spawn driver {}: {:?}", driver.name, e);
                            }
                        }
                    }
                }
            }
        }

        self.device_clients.insert(hardware_id as usize, client);
        self.probed_hardware.insert(hardware_id);
        Ok(())
    }

    pub fn handle_request_buffer(&mut self, utcb: &mut glenda::ipc::UTCB) -> Result<CapPtr, Error> {
        if let Some((frame, vaddr, size, paddr)) = &self.shm_frame {
            utcb.set_mr(0, *vaddr);
            utcb.set_mr(1, *size);
            utcb.set_mr(2, *paddr);
            Ok(frame.cap())
        } else {
            Err(Error::NotInitialized)
        }
    }

    fn handle_setup_ring(
        &mut self,
        utcb: &mut glenda::ipc::UTCB,
        badge: Badge,
    ) -> Result<CapPtr, Error> {
        let sq_entries = utcb.get_mr(0) as u32;
        let cq_entries = utcb.get_mr(1) as u32;

        log!("setup_ring(sq={}, cq={}, badge={:?})", sq_entries, cq_entries, badge);

        if self.client_rings.contains_key(&badge.bits()) {
            return Err(Error::AlreadyExists);
        }
        let notify_slot = self.cspace.alloc(self.res_client).map_err(|_| Error::OutOfMemory)?;
        self.cspace.root().move_cap(self.recv, notify_slot)?;
        let notify_ep = Endpoint::from(notify_slot);

        // Calculate ring size: 64 (header) + sq_entries * 64 + cq_entries * 16
        let ring_size = 64 + (sq_entries as usize * 64) + (cq_entries as usize * 16);
        let pages = (ring_size + PGSIZE - 1) / PGSIZE;
        let actual_size = pages * PGSIZE;

        let slot = self.cspace.alloc(self.res_client)?;
        let frame_cap =
            self.res_client.alloc(Badge::null(), glenda::cap::CapType::Frame, pages, slot)?;
        let frame = Frame::from(frame_cap);

        let vaddr = self.next_ring_vaddr.fetch_add(actual_size, Ordering::SeqCst);
        self.res_client.mmap(Badge::null(), frame, vaddr, actual_size)?;
        let ring = unsafe { IoUring::new(vaddr as *mut u8, actual_size, sq_entries, cq_entries) };
        let mut server = IoUringServer::new(ring);
        server.set_client_notify(notify_ep);
        self.client_rings.insert(badge.bits(), server);
        Ok(frame_cap)
    }

    fn handle_notify_sq(
        &mut self,
        _utcb: &mut glenda::ipc::UTCB,
        badge: Badge,
    ) -> Result<(), Error> {
        let ring = self.client_rings.get_mut(&badge.bits()).ok_or(Error::NotInitialized)?;
        let proxy = self.partitions.get(&badge.bits()).ok_or(Error::NotFound)?;
        let hw_id = proxy.meta.parent as usize;
        let hw_client = self.device_clients.get(&hw_id).ok_or(Error::NotFound)?;
        let hw_ring = hw_client.ring().ok_or(Error::NotInitialized)?;

        while let Some(mut sqe) = ring.ring.pop_sqe() {
            proxy.translate_sqe(&mut sqe);

            let block_size = hw_client.block_size();
            let hw_user_data = self.next_partition_badge.fetch_add(1, Ordering::Relaxed) as u64
                | 0x8000000000000000;

            let client_user_data = sqe.user_data;
            sqe.user_data = hw_user_data;

            if buffer::IOBufferManager::is_aligned(&sqe, block_size) {
                let ctx = buffer::RequestContext {
                    client_badge: badge.bits(),
                    client_user_data,
                    buffer_info: None,
                };
                self.inflight_requests.insert(hw_user_data, ctx);
                hw_ring.submit(sqe)?;
            } else {
                // Handle unaligned IO via Buffer Cache
                let aligned_offset = sqe.off & !(block_size as u64 - 1);
                let end_offset =
                    (sqe.off + sqe.len as u64 + block_size as u64 - 1) & !(block_size as u64 - 1);
                let aligned_len = (end_offset - aligned_offset) as u32; // Assuming fits in u32

                let sector_idx = aligned_offset / block_size as u64;
                let cache_res = self.buffer_cache.access_block(hw_id, sector_idx);

                let buffer_info = buffer::BufferInfo {
                    original_addr: sqe.addr,
                    original_len: sqe.len,
                    original_offset: sqe.off,
                    aligned_offset,
                    aligned_len,
                    is_write: sqe.opcode == glenda::io::uring::IOURING_OP_WRITE,
                    cache_idx: Some(cache_res.block_idx),
                };

                let ctx = buffer::RequestContext {
                    client_badge: badge.bits(),
                    client_user_data,
                    buffer_info: Some(buffer_info),
                };

                sqe.addr = cache_res.buf_vaddr as u64;
                sqe.off = aligned_offset;
                sqe.len = aligned_len;

                self.inflight_requests.insert(hw_user_data, ctx);

                if !cache_res.is_hit && !buffer_info.is_write {
                    hw_ring.submit(sqe)?;
                } else if buffer_info.is_write {
                    // TODO: Handle write buffering/lazy write
                    hw_ring.submit(sqe)?;
                } else {
                    // Hit on Read, simulate complete or just re-read for now.
                    // Ideally we just send completion to client ring immediately.
                    // But structure of loop expects async completion.
                    // We can submit a NOP or just complete?
                    // Submit read anyway to verify hardware for now.
                    hw_ring.submit(sqe)?;
                }
            }
        }

        Ok(())
    }
}
