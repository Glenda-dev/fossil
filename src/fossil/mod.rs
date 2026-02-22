use crate::layout::PROBE_VADDR;
use crate::utils::gpt::{GPTHeader, GPTPartition};
use crate::utils::mbr::MBR;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use glenda::arch::mem::PGSIZE;
use glenda::cap::{CapPtr, Endpoint, Frame, Reply, Rights};
use glenda::client::{DeviceClient, InitClient, ResourceClient};
use glenda::error::Error;
use glenda::interface::device::DeviceService;
use glenda::interface::{MemoryService, ResourceService};
use glenda::ipc::{Badge, UTCB};
use glenda::mem::io_uring::IoUringSqe;
use glenda::mem::shm::SharedMemory;
use glenda::protocol::device::{self, LogicDeviceDesc, LogicDeviceType};
use glenda::utils::manager::{CSpaceManager, CSpaceService};
use glenda_drivers::client::block::BlockClient;
use glenda_drivers::interface::BlockDriver;
use glenda_drivers::io_uring::{IoRing, IoRingServer};
use serde::{Deserialize, Serialize};

mod server;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionMetadata {
    pub parent: u64,
    pub start_lba: u64,
    pub num_blocks: u64,
    pub block_size: u64,
}

#[derive(Clone)]
pub struct PartitionProxy {
    pub meta: PartitionMetadata,
}

impl PartitionProxy {
    pub fn new(meta: PartitionMetadata, _endpoint: Endpoint, _name: String) -> Self {
        Self { meta }
    }

    pub fn translate_sqe(&self, sqe: &mut IoUringSqe) {
        sqe.off += self.meta.start_lba * 512; // MBR/GPT LBA is usually 512B
    }
}

pub struct FossilServer<'a> {
    endpoint: Endpoint,
    reply: Reply,
    recv: CapPtr,
    res_client: &'a mut ResourceClient,
    device_client: &'a mut DeviceClient,
    cspace: &'a mut CSpaceManager,
    init_client: &'a mut InitClient,

    // badge -> PartitionProxy
    partitions: BTreeMap<usize, PartitionProxy>,
    // next_badge_bits -> IoRingServer
    client_rings: BTreeMap<usize, IoRingServer>,
    // hardware_badge_bits -> BlockClient
    device_clients: BTreeMap<usize, BlockClient>,
    probed_hardware: BTreeSet<u64>,

    next_partition_badge: AtomicUsize,
    next_ring_vaddr: AtomicUsize,
    running: bool,
}

impl<'a> FossilServer<'a> {
    pub fn new(
        endpoint: Endpoint,
        res_client: &'a mut ResourceClient,
        cspace: &'a mut CSpaceManager,
        device_client: &'a mut DeviceClient,
        init_client: &'a mut InitClient,
    ) -> Self {
        Self {
            endpoint,
            reply: Reply::from(CapPtr::null()),
            recv: CapPtr::null(),
            res_client,
            device_client,
            init_client,
            cspace,
            partitions: BTreeMap::new(),
            client_rings: BTreeMap::new(),
            device_clients: BTreeMap::new(),
            probed_hardware: BTreeSet::new(),
            next_partition_badge: AtomicUsize::new(0x1000),
            // Skip 2MB to avoid collision with Initrd (usually mapped at 0x5000_0000)
            next_ring_vaddr: AtomicUsize::new(PROBE_VADDR + PGSIZE * 512),
            running: false,
        }
    }

    fn probe_partitions<F>(
        &self,
        parent_desc: &LogicDeviceDesc,
        sector0: &[u8],
        mut read_fn: F,
    ) -> Vec<(LogicDeviceDesc, u64)>
    where
        F: FnMut(u64, &mut [u8]) -> Result<(), Error>,
    {
        let mut results = Vec::new();

        if let Some(mbr) = MBR::parse(sector0) {
            if mbr.is_protective_gpt() {
                log!("GPT detected. Parsing GPT entries...");
                let mut header_buf = [0u8; 512];
                if let Ok(_) = read_fn(1, &mut header_buf) {
                    if let Some(header) = GPTHeader::parse(&header_buf) {
                        let mut entries_buf = [0u8; 128 * 128];
                        let entries_len = header.num_partition_entries as usize
                            * header.partition_entry_size as usize;
                        let entries_blk_count = (entries_len + 511) / 512;

                        let mut offset = 0;
                        for i in 0..entries_blk_count {
                            let mut chunk = [0u8; 512];
                            if read_fn(header.partition_entry_lba + i as u64, &mut chunk).is_ok() {
                                let end = (offset + 512).min(entries_buf.len());
                                entries_buf[offset..end].copy_from_slice(&chunk[..end - offset]);
                                offset = end;
                            }
                        }

                        let gpt_parts = GPTPartition::parse_entries(
                            &entries_buf[..entries_len],
                            header.num_partition_entries,
                            header.partition_entry_size,
                        );
                        for (i, part) in gpt_parts.iter().enumerate() {
                            if part.is_active() {
                                let mut desc = parent_desc.clone();
                                desc.name = String::from(alloc::format!("part{}", i + 1));
                                desc.dev_type =
                                    LogicDeviceType::Block(part.last_lba - part.first_lba + 1);
                                results.push((desc, part.first_lba));
                            }
                        }
                    }
                }
            } else {
                log!("MBR detected. Parsing partitions...");
                for (i, part_opt) in mbr.partitions.iter().enumerate() {
                    if let Some(part) = part_opt {
                        if part.part_type != 0 {
                            let mut desc = parent_desc.clone();
                            desc.name = String::from(alloc::format!("p{}", i + 1));
                            desc.dev_type = LogicDeviceType::Block(part.sectors_count as u64);
                            results.push((desc, part.start_lba as u64));
                        }
                    }
                }
            }
        }
        results
    }

    fn sync_devices(&mut self) -> Result<(), Error> {
        log!("Syncing logical devices from Unicorn...");
        let query = device::DeviceQuery {
            name: None,
            compatible: alloc::vec![],
            dev_type: Some(1), // 1 for RawBlock
        };
        let names = self.device_client.query(Badge::null(), query)?;
        log!("Queried devices: {:?}", names);

        for name in names {
            let (hw_id, desc) = self.device_client.get_logic_desc(Badge::null(), &name)?;
            if let LogicDeviceType::RawBlock(_) = desc.dev_type {
                if !self.probed_hardware.contains(&hw_id) {
                    log!("Found new RawBlock device: {}, id={}", name, hw_id);
                    let hardware_ep =
                        self.device_client.alloc_logic(Badge::new(0xf0511), 1, &name)?;
                    self.probe_device(hw_id, desc, hardware_ep.cap())?;
                }
            }
        }
        Ok(())
    }

    fn probe_device(
        &mut self,
        hw_id: u64,
        desc: device::LogicDeviceDesc,
        hardware_ep: CapPtr,
    ) -> Result<(), Error> {
        if self.probed_hardware.contains(&hw_id) {
            return Ok(());
        }
        self.probed_hardware.insert(hw_id);

        if let LogicDeviceType::RawBlock(capacity_bytes) = desc.dev_type {
            log!(
                "Probing RawBlock: {} (capacity_bytes={}, id={:x?})",
                desc.name,
                capacity_bytes,
                hw_id
            );

            let notify_slot = self.cspace.alloc(self.res_client)?;
            let notify_badge = hw_id as usize | 0x80000000; // Use high bit to distinguish from partitions
            self.cspace.root().mint(
                self.endpoint.cap(),
                notify_slot,
                Badge::new(notify_badge),
                Rights::ALL,
            )?;
            let notify_ep = Endpoint::from(notify_slot);
            let ring_frame_slot = self.cspace.alloc(self.res_client)?;
            let mut bclient = BlockClient::new(Endpoint::from(hardware_ep));
            let ring_frame = bclient.setup_ring(4, 4, notify_ep, ring_frame_slot)?;
            // Always skip at least 16 pages (64KB) to avoid collision with multi-page driver frames
            let vaddr = self.next_ring_vaddr.fetch_add(PGSIZE * 16, Ordering::SeqCst);
            self.res_client.mmap(Badge::null(), ring_frame, vaddr, PGSIZE)?;
            let ring = IoRing::new(SharedMemory::from_frame(ring_frame, vaddr, PGSIZE), 4, 4)?;
            bclient.set_ring(glenda_drivers::io_uring::IoRingClient::new(ring));

            // Setup SHM buffer for zero-copy IO
            let shm_frame_slot = self.cspace.alloc(self.res_client)?;
            let shm_pages = 16; // 64KB
            let shm_size = shm_pages * PGSIZE;
            let (shm_paddr, shm_frame) =
                self.res_client.dma_alloc(Badge::null(), shm_pages, shm_frame_slot)?;

            // Use another 16-page aligned region for SHM
            let shm_vaddr = self.next_ring_vaddr.fetch_add(shm_size, Ordering::SeqCst);
            self.res_client.mmap(Badge::null(), shm_frame, shm_vaddr, shm_size)?;
            bclient.setup_shm(shm_frame, shm_vaddr, shm_paddr as u64, shm_size)?;
            log!(
                "SHM buffer setup at vaddr={:#x}, paddr={:#x}, size={}",
                shm_vaddr,
                shm_paddr,
                shm_size
            );

            // Read sector 0 for device
            let mut sector_buf = [0u8; 4096];
            match bclient.read_blocks(0, 1, 4096, &mut sector_buf) {
                Ok(_) => {
                    log!(
                        "Read sector 0 (4KB) for device {} success, first 16 bytes: {:02x?}",
                        desc.name,
                        &sector_buf[..16]
                    );
                }
                Err(e) => {
                    error!("Read sector 0 (4KB) for device {} FAILED: {:?}", desc.name, e);
                }
            }
            let mut partitions = self.probe_partitions(&desc, &sector_buf[..512], |lba, buf| {
                bclient.read_at(lba * 512, buf.len() as u32, buf)
            });
            log!("Probed {} partitions for device {}", partitions.len(), desc.name);
            if partitions.is_empty() {
                let part_desc = device::LogicDeviceDesc {
                    name: alloc::format!("{}", desc.name),
                    dev_type: LogicDeviceType::Block(capacity_bytes / 4096),
                    parent_name: alloc::format!("{}", desc.name),
                    badge: None,
                };
                partitions.push((part_desc, 0));
            }

            for (mut part_desc, start_lba) in partitions {
                let local_badge = self.next_partition_badge.fetch_add(1, Ordering::SeqCst);
                log!(
                    "Registering partition {} (start_lba={}, badge={:x?})",
                    part_desc.name,
                    start_lba,
                    local_badge
                );

                let slot = self.cspace.alloc(self.res_client)?;
                self.cspace.root().mint(
                    self.endpoint.cap(),
                    slot,
                    Badge::new(local_badge),
                    Rights::ALL,
                )?;

                part_desc.badge = None;
                self.device_client.register_logic(Badge::null(), part_desc.clone(), slot)?;

                if let LogicDeviceType::Block(num_blocks) = part_desc.dev_type {
                    let meta = PartitionMetadata {
                        parent: hw_id,
                        start_lba,
                        num_blocks,
                        block_size: 4096,
                    };
                    let proxy = PartitionProxy::new(
                        meta,
                        Endpoint::from(hardware_ep),
                        part_desc.name.clone(),
                    );
                    self.partitions.insert(local_badge, proxy);
                }
            }

            self.device_clients.insert(hw_id as usize, bclient);
        }
        Ok(())
    }

    fn handle_setup_ring(&mut self, utcb: &mut UTCB, badge: Badge) -> Result<CapPtr, Error> {
        let sq_entries = utcb.get_mr(0) as u32;
        let cq_entries = utcb.get_mr(1) as u32;

        if self.client_rings.contains_key(&badge.bits()) {
            return Err(Error::AlreadyExists);
        }

        // Notification endpoint is in self.recv (from the client's setup_ring call)
        let notify_slot = self.cspace.alloc(self.res_client).map_err(|_| Error::OutOfMemory)?;
        self.cspace.root().move_cap(self.recv, notify_slot)?;
        let notify_ep = Endpoint::from(notify_slot);

        let slot = self.cspace.alloc(self.res_client)?;
        let frame_cap =
            self.res_client.alloc(Badge::null(), glenda::cap::CapType::Frame, 0, slot)?;
        let frame = Frame::from(frame_cap);

        let vaddr = self.next_ring_vaddr.fetch_add(PGSIZE * 16, Ordering::SeqCst);
        self.res_client.mmap(Badge::null(), frame, vaddr, PGSIZE)?;

        let ring =
            IoRing::new(SharedMemory::from_frame(frame, vaddr, PGSIZE), sq_entries, cq_entries)?;
        let mut server = IoRingServer::new(ring);
        server.set_client_notify(notify_ep);
        self.client_rings.insert(badge.bits(), server);

        Ok(frame_cap)
    }

    fn handle_notify_sq(&mut self, _utcb: &mut UTCB, badge: Badge) -> Result<(), Error> {
        let ring = self.client_rings.get(&badge.bits()).ok_or(Error::NotInitialized)?;
        let proxy = self.partitions.get(&badge.bits()).ok_or(Error::NotFound)?;
        let hw_client =
            self.device_clients.get(&(proxy.meta.parent as usize)).ok_or(Error::NotFound)?;
        let hw_ring = hw_client.ring().ok_or(Error::NotInitialized)?;

        let buffer = ring.ring().buffer();
        while let Some(mut sqe) = buffer.pop_sqe() {
            proxy.translate_sqe(&mut sqe);
            hw_ring.submit(sqe)?;
        }
        Ok(())
    }
}
