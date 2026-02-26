use super::FossilServer;
use glenda::cap::{CapPtr, Endpoint, Reply, Rights};
use glenda::error::Error;
use glenda::interface::{
    DeviceService, InitService, MemoryService, ResourceService, SystemService, VolumeService,
};
use glenda::ipc::server::{handle_call, handle_cap_call, handle_notify};
use glenda::ipc::{Badge, MsgTag, UTCB};
use glenda::protocol::VOLUME_PROTO;
use glenda::protocol::device::{HookTarget, LogicDeviceType};
use glenda::protocol::init::ServiceState;
use glenda::utils::manager::CSpaceService;
use glenda_drivers::protocol::{BLOCK_PROTO, block};

impl<'a> SystemService for FossilServer<'a> {
    fn init(&mut self) -> Result<(), Error> {
        // Load Filesystem Driver Config (fs.json)
        log!("Loading fs.json...");
        let config_slot = self.cspace.alloc(self.res_client)?;
        match self.res_client.get_config(Badge::null(), "fs.json", config_slot) {
            Ok((frame, size)) => {
                let vaddr = self
                    .next_ring_vaddr
                    .fetch_add((size + 4095) & !4095, core::sync::atomic::Ordering::SeqCst);
                if let Ok(_) = self.res_client.mmap(Badge::null(), frame, vaddr, size) {
                    let data = unsafe { core::slice::from_raw_parts(vaddr as *const u8, size) };
                    if let Ok(config) = serde_json::from_slice::<super::FSConfig>(data) {
                        log!("Loaded {} FS drivers from config", config.filesystems.len());
                        self.fs_config = Some(config);
                    }
                }
                // Clean up the frame cap from CSpace after use to avoid clutter
                let _ = self.cspace.root().delete(config_slot);
            }
            Err(_) => {
                log!("No fs.json found or failed to load");
                let _ = self.cspace.root().delete(config_slot);
            }
        }

        // Initialize Global Buffer Cache SHM
        let shm_size = self.fs_config.as_ref().map(|c| c.buffer_size).unwrap_or(2 * 1024 * 1024);
        log!("Initializing Buffer Cache ({} bytes)...", shm_size);
        let shm_pages = shm_size / 4096;
        let shm_slot = self.cspace.alloc(self.res_client)?;
        // Use dma_alloc to get physically contiguous (if needed) or just frames.
        // Assuming dma_alloc allocates contiguous memory which is good for drivers.
        let (shm_paddr, shm_frame) =
            self.res_client.dma_alloc(Badge::null(), shm_pages, shm_slot)?;
        let shm_vaddr =
            self.next_ring_vaddr.fetch_add(shm_size, core::sync::atomic::Ordering::SeqCst);
        self.res_client.mmap(Badge::null(), shm_frame.clone(), shm_vaddr, shm_size)?;

        self.buffer_cache = super::buffer::BufferCache::new(shm_vaddr, shm_size, 4096);
        self.shm_frame = Some((shm_frame, shm_vaddr, shm_size, shm_paddr));

        // Register hook for future devices

        log!("Hooked to Unicorn for block devices");
        let target = HookTarget::Type(LogicDeviceType::RawBlock(0));
        let slot = self.cspace.alloc(self.res_client)?;
        self.cspace.root().mint(self.endpoint.cap(), slot, Badge::null(), Rights::ALL)?;
        self.device_client.hook(Badge::null(), target, slot)?;

        // Register Volume endpoint with Warren
        log!("Registering Volume Service...");
        self.res_client
            .register_cap(
                Badge::null(),
                glenda::protocol::resource::ResourceType::Endpoint,
                glenda::protocol::resource::VOLUME_ENDPOINT,
                self.endpoint.cap(),
            )
            .ok();

        Ok(())
    }

    fn listen(&mut self, ep: Endpoint, reply: CapPtr, recv: CapPtr) -> Result<(), Error> {
        self.endpoint = ep;
        self.reply = Reply::from(reply);
        self.recv = recv;
        Ok(())
    }

    fn run(&mut self) -> Result<(), Error> {
        self.init_client.report_service(Badge::null(), ServiceState::Running)?;
        self.running = true;

        while self.running {
            // Process any pending device probes first
            if let Err(e) = self.process_pending_probes() {
                log!("Probe failed: {:?}", e);
            }

            let mut utcb = unsafe { UTCB::new() };
            utcb.clear();

            // FIXME: 执行清理
            if !self.recv.is_null() {
                let _ = self.cspace.root().delete(self.recv);
            }

            utcb.set_reply_window(self.reply.cap());
            utcb.set_recv_window(self.recv);

            if let Err(e) = self.endpoint.recv(&mut utcb) {
                error!("Recv error: {:?}", e);
                continue;
            }

            match self.dispatch(&mut utcb) {
                Ok(()) => {
                    let _ = self.reply(&mut utcb);
                }
                Err(Error::Success) => {
                    // Handled notification, skip reply
                }
                Err(e) => {
                    let badge = utcb.get_badge();
                    let tag = utcb.get_msg_tag();
                    error!(
                        "Dispatch error: {:?} badge={}, proto={:#x}, label={:#x}",
                        e,
                        badge,
                        tag.proto(),
                        tag.label()
                    );
                    utcb.set_msg_tag(MsgTag::err());
                    utcb.set_mr(0, e as usize);
                    let _ = self.reply(&mut utcb);
                }
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, utcb: &mut UTCB) -> Result<(), Error> {
        glenda::ipc_dispatch! {
            self, utcb,
            (VOLUME_PROTO, glenda::protocol::volume::GET_DEVICE) => |s: &mut Self, u: &mut UTCB| {
                 handle_cap_call(u, |u| {
                    let badge_bits = u.get_mr(0);
                    let requested_badge = if badge_bits == 0 { u.get_badge() } else { Badge::new(badge_bits) };
                    let recv = u.get_recv_window();
                    let ep = s.get_device(requested_badge, recv)?;
                    Ok(ep.cap())
                })
            },
            (VOLUME_PROTO, glenda::protocol::volume::PROBE_DEVICE) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u| {
                    let device_name = unsafe { u.read_str()? };
                    s.probe_device(u.get_badge(), &device_name)?;
                    Ok(())
                })
            },
            (VOLUME_PROTO, glenda::protocol::volume::MOUNT_PARTITION) => |s: &mut Self, u: &mut UTCB| {
               handle_call(u, |u| {
                    let mut reader = unsafe { u.get_buffer_reader() };
                    let partition_name = reader.read_str()?;
                    let mount_point = reader.read_str()?;
                    s.mount_partition(u.get_badge(), &partition_name, &mount_point)?;
                    Ok(())
                })
            },
            (glenda::protocol::KERNEL_PROTO, glenda::protocol::kernel::NOTIFY) => |s: &mut Self, u: &mut UTCB| {
                handle_notify(u, |u| {
                    let badge = u.get_badge();
                    let bits = badge.bits();

                    // Determine flags
                    let is_cq = bits & glenda::io::uring::NOTIFY_IO_URING_CQ != 0;
                    let is_sq = bits & glenda::io::uring::NOTIFY_IO_URING_SQ != 0;
                    let is_hook = bits & glenda::protocol::device::NOTIFY_HOOK != 0;

                    // 1. Check for device synchronization notifications
                    if is_hook {
                        if let Err(e) = s.handle_notify_sync() {
                            log!("Sync failed: {:?}", e);
                        }
                    }

                    // 2. Check for hardware IO completion notifications
                    if is_cq {
                        if let Err(e) = s.handle_notify_cq() {
                            log!("Hardware notify failed: {:?}", e);
                        }
                    }

                    // 3. Check for client submission notifications
                    if is_sq {
                        if let Err(e) = s.handle_notify_sq() {
                            log!("Client notify failed: {:?}", e);
                        }
                    }

                    Ok(())
                })?;
                Err(Error::Success)
            },
            (BLOCK_PROTO, block::GET_CAPACITY) => |s: &mut Self, u: &mut UTCB| {
                let badge = u.get_badge();
                handle_call(u, |_| {
                    if let Some(partition) = s.partitions.get(&badge.bits()) {
                        Ok(partition.meta.num_blocks as usize)
                    } else {
                        Err(Error::NotFound)
                    }
                })
            },
            (BLOCK_PROTO, block::GET_BLOCK_SIZE) => |_, u: &mut UTCB| {
                handle_call(u, |_| Ok(4096usize))
            },
            (BLOCK_PROTO, block::SETUP_RING) => |s: &mut Self, u: &mut UTCB| {
                let badge = u.get_badge();
                handle_cap_call(u, |u| s.handle_setup_ring(u, badge))
            },
            (BLOCK_PROTO, block::SETUP_BUFFER) => |s: &mut Self, u: &mut UTCB| {
                handle_cap_call(u, |u| s.handle_request_buffer(u))
            },
            (_, _) => |_, u: &mut UTCB| {
                let tag = u.get_msg_tag();
                let badge = u.get_badge();
                error!(
                    "Unhandled message (proto={:#x}, label={:#x}, badge={:?})",
                    tag.proto(),
                    tag.label(),
                    badge
                );
                Err(Error::InvalidMethod)
            }
        }
    }

    fn reply(&mut self, utcb: &mut UTCB) -> Result<(), Error> {
        self.reply.reply(utcb)
    }

    fn stop(&mut self) {
        self.running = false;
    }
}
