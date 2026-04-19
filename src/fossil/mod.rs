use crate::FSConfig;
use crate::layout::PROBE_VADDR;
use crate::probe::{fs, part};
use alloc::collections::VecDeque;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use glenda::arch::mem::PGSIZE;
use glenda::cap::{CapPtr, Endpoint, Page, Reply};
use glenda::client::{DeviceClient, InitClient, ProcessClient, ResourceClient};
use glenda::drivers::client::block::BlockClient;
use glenda::drivers::client::{RingParams, ShmParams};
use glenda::drivers::interface::{BlockDriver, DriverClient};
use glenda::error::Error;
use glenda::interface::CSpaceService;
use glenda::interface::ProcessService;
use glenda::interface::device::DeviceService;
use glenda::io::uring::IoUringServer;
use glenda::ipc::Badge;
use glenda::mem::Perms;
use glenda::mem::pool::{MemoryPool, ShmType};
use glenda::mem::shm::SharedMemory;
use glenda::protocol::device::LogicDeviceType;
use glenda::protocol::init::ServiceState;
use glenda::utils::manager::{CSpaceManager, VSpaceManager};

mod buffer;
mod proxy;
mod server;
mod volume;

pub use buffer::{CacheBlock, RequestContext};
pub use proxy::{PartitionMetadata, PartitionProxy};

#[derive(Debug, Default)]
pub struct FossilResourceLedger {
    pub mount_waiters_queued: usize,
    pub mount_waiters_woken: usize,
    pub partition_endpoint_mints: usize,
    pub mount_reply_slots_live: BTreeSet<CapPtr>,
}

pub struct FossilIpc {
    pub endpoint: Endpoint,
    pub reply: Reply,
    pub recv: CapPtr,
    pub running: bool,
}

pub struct FossilServer<'a> {
    pub ipc: FossilIpc,
    pub res_client: &'a mut ResourceClient,
    pub device_client: &'a mut DeviceClient,
    pub process_client: &'a mut ProcessClient,
    pub cspace: &'a mut CSpaceManager,
    pub vspace: &'a mut VSpaceManager,
    pub init_client: &'a mut InitClient,

    pub partitions: BTreeMap<usize, PartitionProxy>,
    pub name_to_badge: BTreeMap<String, usize>,
    pub partition_fs_endpoints: BTreeMap<usize, Endpoint>,
    pub partition_driver_pids: BTreeMap<usize, usize>,
    pub partition_driver_states: BTreeMap<usize, ServiceState>,
    pub client_rings: BTreeMap<usize, IoUringServer>,
    pub client_shms: BTreeMap<usize, ShmParams>,
    pub device_clients: BTreeMap<usize, BlockClient>,
    pub probed_hardware: BTreeSet<usize>,

    pub inflight_requests: BTreeMap<usize, RequestContext>,

    pub fs_config: Option<FSConfig>,
    pub driver_to_partition: BTreeMap<usize, usize>,
    pub next_partition_badge: AtomicUsize,
    pub mem_pool: MemoryPool,
    pub pending_devices: VecDeque<String>,
    pub pending_mount_replies: BTreeMap<usize, VecDeque<CapPtr>>,

    // Block Cache
    pub buffer_cache: buffer::BufferCache,
    pub global_shm: Option<SharedMemory>,
    pub resource_ledger: FossilResourceLedger,
}

impl<'a> FossilServer<'a> {
    pub fn new(
        endpoint: Endpoint,
        res_client: &'a mut ResourceClient,
        process_client: &'a mut ProcessClient,
        cspace: &'a mut CSpaceManager,
        vspace: &'a mut VSpaceManager,
        device_client: &'a mut DeviceClient,
        init_client: &'a mut InitClient,
    ) -> Self {
        Self {
            ipc: FossilIpc {
                endpoint,
                reply: Reply::from(CapPtr::null()),
                recv: CapPtr::null(),
                running: false,
            },
            res_client,
            process_client,
            device_client,
            init_client,
            cspace,
            vspace,
            partitions: BTreeMap::new(),
            name_to_badge: BTreeMap::new(),
            partition_fs_endpoints: BTreeMap::new(),
            partition_driver_pids: BTreeMap::new(),
            partition_driver_states: BTreeMap::new(),
            client_rings: BTreeMap::new(),
            device_clients: BTreeMap::new(),
            probed_hardware: BTreeSet::new(),
            inflight_requests: BTreeMap::new(),
            client_shms: BTreeMap::new(),
            fs_config: None,
            driver_to_partition: BTreeMap::new(),
            next_partition_badge: AtomicUsize::new(0x1000),
            mem_pool: MemoryPool::new(PROBE_VADDR + PGSIZE * 512),
            pending_devices: VecDeque::new(),
            pending_mount_replies: BTreeMap::new(),
            buffer_cache: buffer::BufferCache::new(0, 0, 4096),
            global_shm: None,
            resource_ledger: FossilResourceLedger::default(),
        }
    }

    pub(crate) fn ledger_mount_waiter_queued(&mut self, slot: CapPtr) {
        self.resource_ledger.mount_waiters_queued =
            self.resource_ledger.mount_waiters_queued.saturating_add(1);
        self.resource_ledger.mount_reply_slots_live.insert(slot);
    }

    pub(crate) fn ledger_mount_waiter_woken(&mut self, slot: CapPtr) {
        self.resource_ledger.mount_waiters_woken =
            self.resource_ledger.mount_waiters_woken.saturating_add(1);
        self.resource_ledger.mount_reply_slots_live.remove(&slot);
    }

    pub(crate) fn ledger_partition_ep_minted(&mut self) {
        self.resource_ledger.partition_endpoint_mints =
            self.resource_ledger.partition_endpoint_mints.saturating_add(1);
    }

    pub(crate) fn log_resource_ledger(&self, _reason: &str) {}

    fn fs_type_compatible_key(fs_type: fs::FileSystemType) -> Option<&'static str> {
        match fs_type {
            fs::FileSystemType::Fat16 => Some("fat16"),
            fs::FileSystemType::Fat32 => Some("fat32"),
            fs::FileSystemType::ExFat => Some("exfat"),
            fs::FileSystemType::Ext2 => Some("ext2"),
            fs::FileSystemType::Ext3 => Some("ext3"),
            fs::FileSystemType::Ext4 => Some("ext4"),
            fs::FileSystemType::InitrdFS => Some("initrdfs"),
            fs::FileSystemType::Unknown => None,
        }
    }

    fn select_fs_driver_binary(&self, fs_type: fs::FileSystemType) -> Option<String> {
        let key = Self::fs_type_compatible_key(fs_type)?;
        let cfg = self.fs_config.as_ref()?;

        for driver in &cfg.filesystems {
            if driver.compatible.iter().any(|c| c == key) {
                return Some(driver.binary.clone());
            }
        }

        None
    }

    pub(crate) fn ensure_partition_fs_driver(
        &mut self,
        partition_badge: usize,
        partition_name: &str,
        fs_type: fs::FileSystemType,
    ) -> Result<(), Error> {
        let binary = self.select_fs_driver_binary(fs_type).ok_or(Error::NotFound)?;
        if !self.partition_driver_pids.contains_key(&partition_badge) {
            log!("Spawning FS driver {} for partition {}", binary, partition_name);

            let pid = self.process_client.spawn(Badge::null(), &binary)?;
            self.driver_to_partition.insert(pid, partition_badge);
            self.partition_driver_pids.insert(partition_badge, pid);
            self.partition_driver_states.insert(partition_badge, ServiceState::Starting);
        }

        Ok(())
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
                    self.probe(hw_id, &name, desc, hw_ep)?;
                }
            }
        }
        Ok(())
    }

    fn read_block(
        &mut self,
        client: &BlockClient,
        hw_id: usize,
        lba: usize,
        target: &mut [u8],
    ) -> Result<(), Error> {
        let block_size = client.block_size() as usize;
        let cache_block_size = self.buffer_cache.get_block_size();
        let sectors_per_cache_block = core::cmp::max(1, cache_block_size / block_size);
        let cache_base_lba = (lba / sectors_per_cache_block) * sectors_per_cache_block;
        let lba_in_cache = lba - cache_base_lba;
        let byte_offset = lba_in_cache * block_size;

        let res = self.buffer_cache.access_block(hw_id, lba, sectors_per_cache_block);
        let cache_vaddr = res.buf_vaddr;

        if !res.is_hit {
            // Fill the whole cache block window from device.
            let cache_slice = unsafe {
                core::slice::from_raw_parts_mut(
                    cache_vaddr as *mut u8,
                    sectors_per_cache_block * block_size,
                )
            };
            for i in 0..sectors_per_cache_block {
                let start = i * block_size;
                let end = start + block_size;
                client.read_blocks(cache_base_lba + i, 1, &mut cache_slice[start..end])?;
            }
            self.buffer_cache.mark_valid(res.block_idx);
        }

        let cache_slice = unsafe {
            core::slice::from_raw_parts(
                cache_vaddr as *const u8,
                sectors_per_cache_block * block_size,
            )
        };
        let copy_len = core::cmp::min(target.len(), block_size);
        target[..copy_len].copy_from_slice(&cache_slice[byte_offset..byte_offset + copy_len]);
        Ok(())
    }

    fn probe(
        &mut self,
        hardware_id: usize,
        logical_name: &str,
        desc: glenda::protocol::device::LogicDeviceDesc,
        hardware_ep: Endpoint,
    ) -> Result<(), Error> {
        log!("Probing device {} (driver={}) (ID: {})", logical_name, desc.name, hardware_id);
        let gshm = self.global_shm.as_ref().ok_or(Error::NotInitialized)?;
        let ring_slot = self.cspace.alloc(self.res_client)?;
        let ring_vaddr = self.mem_pool.reserve(PGSIZE);

        let client = {
            if let Some(existing) = self.device_clients.get(&(hardware_id as usize)) {
                existing.clone()
            } else {
                let mut c = BlockClient::new(
                    hardware_ep,
                    self.res_client,
                    RingParams {
                        sq_entries: 4,
                        cq_entries: 4,
                        notify_ep: self.ipc.endpoint,
                        recv_slot: ring_slot,
                        vaddr: ring_vaddr,
                        size: PGSIZE,
                    },
                    ShmParams {
                        frame: gshm.frame(),
                        vaddr: gshm.vaddr(),
                        paddr: gshm.paddr(),
                        size: gshm.size(),
                        recv_slot: CapPtr::null(),
                    },
                );
                c.connect(self.vspace, self.cspace)?;
                // Add the received ring frame to MemoryPool for tracking
                self.mem_pool.map_shm(
                    self.vspace,
                    self.cspace,
                    self.res_client,
                    Page::from(ring_slot),
                    PGSIZE,
                    Perms::READ | Perms::WRITE,
                )?;
                c
            }
        };

        log!("Read first block of device {} for partition probing", desc.name);
        let block_size = client.block_size() as usize;
        let hw_id_usize = hardware_id as usize;
        let mut first_block = [0u8; 4096];
        self.read_block(&client, hw_id_usize, 0, &mut first_block[..block_size])?;

        let detected_parts = {
            let server_ptr = core::cell::UnsafeCell::new(&mut *self);
            part::detect_partitions_registered(
                &first_block[..block_size],
                block_size,
                |lba, target| {
                    let s_mut = unsafe { &mut *server_ptr.get() };
                    let mut copied = 0usize;
                    let mut cur_lba = lba;

                    while copied < target.len() {
                        if (target.len() - copied) >= block_size {
                            s_mut.read_block(
                                &client,
                                hw_id_usize,
                                cur_lba,
                                &mut target[copied..copied + block_size],
                            )?;
                            copied += block_size;
                            cur_lba += 1;
                        } else {
                            let mut temp = alloc::vec![0u8; block_size];
                            s_mut.read_block(&client, hw_id_usize, cur_lba, &mut temp)?;
                            let copy_len = target.len() - copied;
                            target[copied..copied + copy_len].copy_from_slice(&temp[..copy_len]);
                            copied += copy_len;
                        }
                    }
                    Ok(())
                },
            )
        };

        let mut partitions = Vec::new();
        for (part_index, range) in detected_parts.into_iter().enumerate() {
            let p_desc = glenda::protocol::device::LogicDeviceDesc {
                name: alloc::format!("{}p{}", logical_name, part_index),
                dev_type: LogicDeviceType::Volume,
                parent_name: String::from(logical_name),
                badge: None,
            };
            partitions.push((p_desc, range.start_lba, range.num_blocks));
        }

        // Fallback: If no partition table found, treat the whole device as one partition
        if partitions.is_empty() {
            partitions.push((
                glenda::protocol::device::LogicDeviceDesc {
                    name: alloc::format!("{}p0", logical_name),
                    dev_type: LogicDeviceType::Volume,
                    parent_name: String::from(logical_name),
                    badge: None,
                },
                0,
                client.total_sectors() as usize,
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
            let fs_type = {
                let s_ptr = core::cell::UnsafeCell::new(&mut *self);
                fs::detect_fs_registered(|offset, target| {
                    let s = unsafe { &mut *s_ptr.get() };
                    let mut copied = 0usize;
                    while copied < target.len() {
                        let cur_offset = offset + copied;
                        let sector = first_lba + cur_offset / block_size as usize;
                        let offset_in_sector = cur_offset % block_size as usize;

                        if offset_in_sector == 0 && (target.len() - copied) >= block_size {
                            s.read_block(
                                &client,
                                hw_id_usize,
                                sector,
                                &mut target[copied..copied + block_size],
                            )?;
                            copied += block_size;
                        } else {
                            let mut temp = [0u8; 4096];
                            s.read_block(&client, hw_id_usize, sector, &mut temp[..block_size])?;
                            let copy_len = core::cmp::min(
                                target.len() - copied,
                                block_size - offset_in_sector,
                            );
                            target[copied..copied + copy_len].copy_from_slice(
                                &temp[offset_in_sector..offset_in_sector + copy_len],
                            );
                            copied += copy_len;
                        }
                    }
                    Ok(())
                })
            };
            log!("Partition {} FS type: {:?}", p_desc.name, fs_type);

            let proxy =
                PartitionProxy::new(meta, hardware_ep, p_desc.name.clone(), fs_type, Badge::null());
            self.partitions.insert(badge.bits(), proxy);
            self.name_to_badge.insert(p_desc.name.clone(), badge.bits());

            if let Err(e) = self.ensure_partition_fs_driver(badge.bits(), &p_desc.name, fs_type) {
                warn!(
                    "Failed to bind FS driver for partition {} ({:?}): {:?}",
                    p_desc.name, fs_type, e
                );
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
