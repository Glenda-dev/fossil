use alloc::collections::{BTreeMap, BTreeSet};
use glenda::arch::mem::PGSIZE;
use glenda::cap::{CapPtr, Endpoint, Frame, Reply, Rights, VSPACE_SLOT, VSpace};
use glenda::client::{ResourceClient, device::DeviceClient};
use glenda::error::Error;
use glenda::interface::device::DeviceService;
use glenda::interface::{ResourceService, SystemService};
use glenda::ipc::{Badge, MsgFlags, MsgTag, UTCB};
use glenda::mem::Perms;
use glenda::mem::shm::SharedMemory;
use glenda::protocol::DEVICE_PROTO;
use glenda::protocol::device::{self, DeviceNotification, HookTarget, LogicDeviceType};
use glenda::utils::manager::{CSpaceManager, CSpaceService};

use glenda_drivers::client::block::BlockClient;
use glenda_drivers::interface::BlockDriver;
use glenda_drivers::io_uring::IoRing;
use glenda_drivers::protocol::{BLOCK_PROTO, block};

use crate::layout::PROBE_VADDR;
use crate::manager::{FossilManager, PartitionMetadata, PartitionProxy};
use core::sync::atomic::{AtomicUsize, Ordering};

pub struct FossilServer<'a> {
    endpoint: Endpoint,
    reply: Reply,
    recv: CapPtr,
    manager: FossilManager,
    res_client: &'a mut ResourceClient,
    device_client: DeviceClient,
    cspace: &'a mut CSpaceManager,

    // next_badge_bits -> IoRing
    client_rings: BTreeMap<usize, IoRing>,
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
        manager: FossilManager,
        res_client: &'a mut ResourceClient,
        cspace: &'a mut CSpaceManager,
        device_ep: Endpoint,
    ) -> Self {
        Self {
            endpoint,
            reply: Reply::from(CapPtr::null()),
            recv: CapPtr::null(),
            manager,
            res_client,
            device_client: DeviceClient::new(device_ep),
            cspace,
            client_rings: BTreeMap::new(),
            device_clients: BTreeMap::new(),
            probed_hardware: BTreeSet::new(),
            next_partition_badge: AtomicUsize::new(0x1000),
            next_ring_vaddr: AtomicUsize::new(PROBE_VADDR + PGSIZE * 64),
            running: false,
        }
    }

    fn handle_update(&mut self, utcb: &mut UTCB) -> Result<(), Error> {
        let note: DeviceNotification = unsafe { utcb.read_postcard()? };

        match note {
            DeviceNotification::Registered(hw_badge_bits, desc) => {
                if let LogicDeviceType::RawBlock(_) = desc.dev_type {
                    let hardware_ep = if utcb.get_msg_tag().flags().contains(MsgFlags::HAS_CAP) {
                        utcb.get_recv_window()
                    } else {
                        return Err(Error::InvalidArgs);
                    };

                    self.probe_device(hw_badge_bits, desc, hardware_ep)
                } else {
                    Ok(())
                }
            }
        }
    }

    fn probe_device(
        &mut self,
        hw_badge_bits: u64,
        desc: device::LogicDeviceDesc,
        hardware_ep: CapPtr,
    ) -> Result<(), Error> {
        if self.probed_hardware.contains(&hw_badge_bits) {
            return Ok(());
        }
        self.probed_hardware.insert(hw_badge_bits);

        if let LogicDeviceType::RawBlock(capacity_bytes) = desc.dev_type {
            log!(
                "Probing RawBlock: {} (capacity_bytes={}, badge={:x?})",
                desc.name,
                capacity_bytes,
                hw_badge_bits
            );

            let mut bclient = BlockClient::new(Endpoint::from(hardware_ep));
            let ring_frame = bclient.setup_ring(4, 4)?;

            let vaddr = self.next_ring_vaddr.fetch_add(PGSIZE, Ordering::SeqCst);
            VSpace::from(VSPACE_SLOT).map(ring_frame, vaddr, Perms::READ | Perms::WRITE)?;
            let ring = IoRing::new(SharedMemory::from_frame(ring_frame, vaddr, PGSIZE), 4, 4)?;
            bclient.set_ring(glenda_drivers::io_uring::IoRingClient::new(ring));

            let mut sector_buf = [0u8; 512];
            bclient.read_blocks(0, 1, &mut sector_buf)?;

            let mut partitions = self.manager.probe(
                hw_badge_bits as usize,
                &desc,
                &sector_buf,
                capacity_bytes / 512,
                |lba, buf| bclient.read_blocks(lba, 1, buf),
            );

            if partitions.is_empty() {
                log!("No partitions found for device {}", desc.name);
                let part_desc = device::LogicDeviceDesc {
                    name: alloc::format!("{}", desc.name),
                    dev_type: LogicDeviceType::Block(capacity_bytes / 512),
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
                let _ = self.cspace.root().mint(
                    self.endpoint.cap(),
                    slot,
                    Badge::new(local_badge),
                    Rights::ALL,
                );

                part_desc.badge = Some(local_badge as u64);
                self.device_client.register_logic(Badge::null(), part_desc.clone(), slot)?;

                if let LogicDeviceType::Block(num_blocks) = part_desc.dev_type {
                    let meta = PartitionMetadata {
                        parent: hw_badge_bits,
                        start_lba,
                        num_blocks,
                        block_size: 512,
                    };
                    let proxy = PartitionProxy::new(
                        meta,
                        Endpoint::from(hardware_ep),
                        part_desc.name.clone(),
                    );
                    self.manager.register_proxy(local_badge, proxy);
                }
            }

            self.device_clients.insert(hw_badge_bits as usize, bclient);
        }
        Ok(())
    }

    fn handle_setup_ring(&mut self, utcb: &mut UTCB, badge: Badge) -> Result<CapPtr, Error> {
        let sq_entries = utcb.get_mr(0) as u32;
        let cq_entries = utcb.get_mr(1) as u32;

        if self.client_rings.contains_key(&badge.bits()) {
            return Err(Error::AlreadyExists);
        }

        let slot = self.cspace.alloc(self.res_client)?;
        let frame_cap =
            self.res_client.alloc(Badge::null(), glenda::cap::CapType::Frame, 0, slot)?;
        let frame = Frame::from(frame_cap);

        let vaddr = self.next_ring_vaddr.fetch_add(PGSIZE, Ordering::SeqCst);
        VSpace::from(VSPACE_SLOT).map(frame, vaddr, Perms::READ | Perms::WRITE)?;

        let ring =
            IoRing::new(SharedMemory::from_frame(frame, vaddr, PGSIZE), sq_entries, cq_entries)?;
        self.client_rings.insert(badge.bits(), ring);

        Ok(frame_cap)
    }

    fn handle_notify_sq(&mut self, _utcb: &mut UTCB, badge: Badge) -> Result<(), Error> {
        let ring = self.client_rings.get(&badge.bits()).ok_or(Error::NotInitialized)?;
        let proxy = self.manager.get_partition(badge).ok_or(Error::NotFound)?;
        let hw_client =
            self.device_clients.get(&(proxy.meta.parent as usize)).ok_or(Error::NotFound)?;
        let hw_ring = hw_client.ring().ok_or(Error::NotInitialized)?;

        let buffer = ring.buffer();
        while let Some(mut sqe) = buffer.pop_sqe() {
            proxy.translate_sqe(&mut sqe);
            hw_ring.submit(sqe)?;
        }
        Ok(())
    }
}

impl<'a> SystemService for FossilServer<'a> {
    fn init(&mut self) -> Result<(), Error> {
        // 1. Scan for existing RawBlock devices
        log!("Scanning for existing block devices...");
        let names = self
            .device_client
            .query(Badge::null(), device::DeviceQuery { compatible: alloc::vec::Vec::new() })?;

        for name in names {
            if let Ok((id, desc)) = self.device_client.get_logic_desc(Badge::null(), &name) {
                if let LogicDeviceType::RawBlock(_) = desc.dev_type {
                    if let Ok(ep) = self.device_client.alloc_logic(Badge::null(), 1, &name) {
                        if let Err(e) = self.probe_device(id, desc, ep.cap()) {
                            error!("Failed to probe existing device {}: {:?}", name, e);
                        }
                    }
                }
            }
        }

        // 2. Register hook for future devices
        let target = HookTarget::Type(LogicDeviceType::RawBlock(0));
        self.device_client.hook(Badge::null(), target, self.endpoint.cap())?;
        log!("Hooked to Unicorn for block devices");
        Ok(())
    }

    fn listen(&mut self, ep: Endpoint, reply: CapPtr, recv: CapPtr) -> Result<(), Error> {
        self.endpoint = ep;
        self.reply = Reply::from(reply);
        self.recv = recv;
        Ok(())
    }

    fn run(&mut self) -> Result<(), Error> {
        self.running = true;
        while self.running {
            let mut utcb = unsafe { UTCB::new() };
            utcb.clear();
            utcb.set_reply_window(self.reply.cap());
            utcb.set_recv_window(self.recv);

            if let Err(e) = self.endpoint.recv(&mut utcb) {
                log!("recv error: {:?}", e);
                continue;
            }

            if let Err(e) = self.dispatch(&mut utcb) {
                log!("dispatch error: {:?}", e);
                utcb.set_msg_tag(MsgTag::err());
                utcb.set_mr(0, e as usize);
            }

            let _ = self.reply(&mut utcb);
        }
        Ok(())
    }

    fn dispatch(&mut self, utcb: &mut UTCB) -> Result<(), Error> {
        let tag = utcb.get_msg_tag();
        let proto = tag.proto();
        let label = tag.label();
        let badge = utcb.get_badge();

        match (proto, label) {
            (DEVICE_PROTO, device::UPDATE) => self.handle_update(utcb),
            (BLOCK_PROTO, block::GET_CAPACITY) => {
                if let Some(partition) = self.manager.get_partition(badge) {
                    utcb.set_mr(0, partition.meta.num_blocks as usize);
                    Ok(())
                } else {
                    Err(Error::NotFound)
                }
            }
            (BLOCK_PROTO, block::SETUP_RING) => {
                let frame = self.handle_setup_ring(utcb, badge)?;
                utcb.set_msg_tag(MsgTag::new(0, 0, MsgFlags::OK | MsgFlags::HAS_CAP));
                utcb.set_cap_transfer(frame);
                Ok(())
            }
            (BLOCK_PROTO, block::NOTIFY_SQ) => self.handle_notify_sq(utcb, badge),
            _ => Err(Error::InvalidMethod),
        }
    }

    fn reply(&mut self, utcb: &mut UTCB) -> Result<(), Error> {
        self.reply.reply(utcb)
    }

    fn stop(&mut self) {
        self.running = false;
    }
}
