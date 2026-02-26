use crate::layout::PROBE_VADDR;
use crate::utils::gpt::{GPTHeader, GPTPartition};
use crate::utils::mbr::MBR;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use glenda::arch::mem::PGSIZE;
use glenda::cap::{CapPtr, Endpoint, Reply};
use glenda::client::{DeviceClient, InitClient, ProcessClient, ResourceClient};
use glenda::error::Error;
use glenda::interface::ProcessService;
use glenda::interface::device::DeviceService;
use glenda::io::uring::IoUringServer;
use glenda::ipc::Badge;
use glenda::protocol::device::LogicDeviceType;
use glenda::utils::align::align_up;
use glenda::utils::manager::{CSpaceManager, CSpaceService};
use glenda_drivers::client::block::BlockClient;
use glenda_drivers::client::{RingParams, ShmParams};
use glenda_drivers::interface::{BlockDriver, DriverClient};
use serde::{Deserialize, Serialize};

mod buffer;
mod proxy;
mod server;
pub mod sniffer;
mod volume;

use alloc::collections::VecDeque;
pub use buffer::RequestContext;
pub use proxy::{PartitionMetadata, PartitionProxy};

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
    pub client_shms: BTreeMap<usize, ShmParams>,
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
            client_shms: BTreeMap::new(),
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
    ) -> Vec<(glenda::protocol::device::LogicDeviceDesc, u64, u64)>
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
                                        dev_type: LogicDeviceType::Volume,
                                        parent_name: parent_desc.name.clone(),
                                        badge: None,
                                    };
                                    results.push((
                                        desc,
                                        part.first_lba,
                                        (part.last_lba - part.first_lba + 1),
                                    ));
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
                            dev_type: LogicDeviceType::Volume,
                            parent_name: parent_desc.name.clone(),
                            badge: None,
                        };
                        results.push((desc, entry.start_lba as u64, entry.sectors_count as u64));
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
            dev_type: Some(LogicDeviceType::Block),
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
            if let LogicDeviceType::Block = desc.dev_type {
                if !self.probed_hardware.contains(&hw_id) {
                    let hw_slot = self.cspace.alloc(self.res_client)?;
                    let hw_ep = self.device_client.alloc_logic(
                        Badge::null(),
                        LogicDeviceType::Block,
                        &name,
                        hw_slot,
                    )?;
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

        // Setup ring for AIO probing
        let ring_vaddr = self.next_ring_vaddr.fetch_add(PGSIZE, Ordering::SeqCst);
        let ring_slot = self.cspace.alloc(self.res_client)?;

        let (shm_frame, shm_va, shm_sz, shm_pa) =
            self.shm_frame.as_ref().ok_or(Error::NotInitialized)?;
        let mut client = BlockClient::new(
            hardware_ep,
            self.res_client,
            RingParams {
                sq_entries: 4,
                cq_entries: 4,
                notify_ep: self.endpoint,
                recv_slot: ring_slot,
                vaddr: ring_vaddr,
                size: PGSIZE,
            },
            ShmParams {
                frame: *shm_frame,
                vaddr: *shm_va,
                paddr: *shm_pa as u64,
                size: *shm_sz,
                recv_slot: CapPtr::null(),
            },
        );

        client.connect()?;
        let block_size = client.block_size() as usize;
        let mut buf = [0u8; 4096]; // Buffer for at least one block
        client.read_blocks(0, 1, &mut buf[..block_size])?;
        log!("Read first block of device {} for partition probing", desc.name);
        let mut partitions =
            self.probe_partitions(&desc, &buf[..block_size], block_size, |lba, target| {
                let count = align_up(target.len(), block_size) / block_size;
                client.read_blocks(lba, count as u32, target)
            });

        // Fallback: If no partition table found, treat the whole device as one partition
        if partitions.is_empty() {
            partitions.push((
                glenda::protocol::device::LogicDeviceDesc {
                    name: alloc::format!("{}p0", desc.name),
                    dev_type: LogicDeviceType::Volume,
                    parent_name: desc.name.clone(),
                    badge: None,
                },
                0,
                client.total_sectors(),
            ));
        }

        for (p_desc, first_lba, num_blocks) in partitions {
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
                num_blocks,
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
                                error!("Failed to spawn driver {}: {:?}", driver.name, e);
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

    pub fn handle_notify_sync(&mut self) -> Result<(), Error> {
        self.sync_devices()
    }
}
