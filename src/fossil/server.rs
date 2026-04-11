use super::FossilServer;
use crate::FSConfig;
use glenda::cap::{CSPACE_CAP, CapPtr, Endpoint, Reply};
use glenda::error::Error;
use glenda::interface::CSpaceService;
use glenda::interface::{
    DeviceService, InitService, ResourceService, SystemService, VolumeService,
};
use glenda::ipc::server::{handle_call, handle_cap_call, handle_notify};
use glenda::ipc::{Badge, MsgFlags, MsgTag, UTCB};
use glenda::protocol::VOLUME_PROTO;
use glenda::protocol::device::{HookTarget, LogicDeviceType};
use glenda::protocol::init::ServiceState;

impl<'a> SystemService for FossilServer<'a> {
    fn init(&mut self) -> Result<(), Error> {
        // Load Filesystem Driver Config (fs.json)
        log!("Loading fs.json...");
        match FSConfig::load(self.res_client, self.cspace, self.vspace, &mut self.mem_pool) {
            Ok(config) => {
                log!("Loaded {} FS drivers from config", config.filesystems.len());
                self.fs_config = Some(config);
            }
            Err(_) => {
                log!("No fs.json found or failed to load");
            }
        }

        // Initialize Global Buffer Cache SHM
        let shm_size = self.fs_config.as_ref().map(|c| c.buffer_size).unwrap_or(2 * 1024 * 1024);
        log!("Initializing Buffer Cache ({} bytes)...", shm_size);

        let shm_slot = self.cspace.alloc(self.res_client)?;
        let shm = self.mem_pool.alloc_shm(
            self.vspace,
            self.cspace,
            self.res_client,
            shm_size,
            super::ShmType::DMA,
            shm_slot,
        )?;

        self.buffer_cache = super::buffer::BufferCache::new(shm.vaddr(), shm.size(), 4096);

        if let Some(ref config) = self.fs_config {
            if config.replacement_policy == "fifo" {
                // Future implementation
            } else if config.replacement_policy == "lru" {
                self.buffer_cache.set_policy(alloc::boxed::Box::new(crate::policy::LruPolicy));
            }

            if config.write_policy == "write-through" {
                self.buffer_cache
                    .set_write_policy(alloc::boxed::Box::new(crate::policy::WriteThrough));
            } else if config.write_policy == "write-back" {
                // Future implementation
            }
        }

        self.global_shm = Some(shm);

        // One-shot initial scan/probe for existing devices before reporting Running.
        self.sync_devices()?;
        self.process_pending_probes()?;

        // Register hook for future devices (hotplug after initial one-shot scan)
        log!("Hooked to Unicorn for block devices");
        let target = HookTarget::Type(LogicDeviceType::Block);
        self.device_client.hook(Badge::null(), target, self.ipc.endpoint.cap())?;

        // Register Volume endpoint with Warren
        log!("Registering Volume Service...");
        self.res_client
            .register_cap(
                Badge::null(),
                glenda::protocol::resource::ResourceType::Endpoint,
                glenda::protocol::resource::VOLUME_ENDPOINT,
                self.ipc.endpoint.cap(),
            )
            .ok();

        self.init_client.report_service(Badge::null(), ServiceState::Running)?;

        Ok(())
    }

    fn listen(&mut self, ep: Endpoint, reply: CapPtr, recv: CapPtr) -> Result<(), Error> {
        self.ipc.endpoint = ep;
        self.ipc.reply = Reply::from(reply);
        self.ipc.recv = recv;
        Ok(())
    }

    fn run(&mut self) -> Result<(), Error> {
        self.ipc.running = true;

        while self.ipc.running {
            // Process any pending device probes first
            if let Err(e) = self.process_pending_probes() {
                log!("Probe failed: {:?}", e);
            }

            let mut utcb = unsafe { UTCB::new() };
            utcb.clear();

            utcb.set_reply_window(self.ipc.reply.cap());
            utcb.set_recv_window(self.ipc.recv);

            if let Err(e) = self.ipc.endpoint.recv(&mut utcb) {
                error!("Recv error: {:?}", e);
                continue;
            }

            match self.dispatch(&mut utcb) {
                Ok(()) => {
                    let _ = self.reply(&mut utcb);
                }
                Err(Error::Success) => {
                    // Handled notification, skip reply
                    let _ = CSPACE_CAP.delete(self.ipc.reply.cap());
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
        let result = glenda::ipc_dispatch! {
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
            (VOLUME_PROTO, glenda::protocol::volume::REPORT_STATE) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u| {
                    let state = ServiceState::from(u.get_mr(0));
                    let endpoint = if u.get_msg_tag().flags().contains(MsgFlags::HAS_CAP) {
                        Some(s.ipc.recv)
                    } else {
                        None
                    };
                    s.report_state(u.get_badge(), state, endpoint)?;
                    Ok(())
                })
            },
            (VOLUME_PROTO, glenda::protocol::volume::MOUNT_PARTITION) => |s: &mut Self, u: &mut UTCB| {
                s.handle_mount_partition_request(u)
            },
            (VOLUME_PROTO, glenda::protocol::volume::GET_INFO) => |s: &mut Self, u: &mut UTCB| {
                let badge = u.get_badge();
                handle_call(u, |u| {
                    if let Some(partition) = s.partitions.get(&badge.bits()) {
                        u.set_mr(0, partition.meta.block_size as usize); // block_size
                        u.set_mr(1, partition.meta.num_blocks as usize); // size in blocks
                        Ok(())
                    } else {
                        Err(Error::NotFound)
                    }
                })
            },
            (VOLUME_PROTO, glenda::protocol::volume::SETUP_RING) => |s: &mut Self, u: &mut UTCB| {
                let badge = u.get_badge();
                handle_cap_call(u, |u| s.handle_setup_ring(u, badge))
            },
            (VOLUME_PROTO, glenda::protocol::volume::ACQUIRE_SHM) => |s: &mut Self, u: &mut UTCB| {
                handle_cap_call(u, |u| s.handle_acquire_shm(u))
            },
            (VOLUME_PROTO, glenda::protocol::volume::REGISTER_SHM) => |s: &mut Self, u: &mut UTCB| {
                let badge = u.get_badge();
                handle_call(u, |u| s.handle_register_shm(u, badge).map(|_| 0usize))
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
                            error!("Sync failed: {:?}", e);
                        }
                    }

                    // 2. Check for hardware IO completion notifications
                    if is_cq {
                        if let Err(e) = s.handle_notify_cq() {
                            error!("Hardware notify failed: {:?}", e);
                        }
                    }

                    // 3. Check for client submission notifications
                    if is_sq {
                        if let Err(e) = s.handle_notify_sq() {
                            error!("Client notify failed: {:?}", e);
                        }
                    }

                    Ok(())
                })
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
        };
        result
    }

    fn reply(&mut self, utcb: &mut UTCB) -> Result<(), Error> {
        self.ipc.reply.reply(utcb)
    }

    fn stop(&mut self) {
        self.ipc.running = false;
    }
}
