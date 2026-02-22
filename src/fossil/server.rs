use super::FossilServer;
use glenda::cap::{CapPtr, Endpoint, Reply, Rights};
use glenda::error::Error;
use glenda::interface::device::DeviceService;
use glenda::interface::{InitService, SystemService};
use glenda::ipc::server::{handle_call, handle_cap_call, handle_notify};
use glenda::ipc::{Badge, MsgTag, UTCB};
use glenda::protocol::device::{HookTarget, LogicDeviceType};
use glenda::protocol::init::ServiceState;
use glenda::utils::manager::CSpaceService;
use glenda_drivers::protocol::{BLOCK_PROTO, block};

impl<'a> SystemService for FossilServer<'a> {
    fn init(&mut self) -> Result<(), Error> {
        // Scan for existing RawBlock devices
        self.sync_devices()?;

        // Register hook for future devices
        log!("Hooked to Unicorn for block devices");
        let target = HookTarget::Type(LogicDeviceType::RawBlock(0));
        let slot = self.cspace.alloc(self.res_client)?;
        self.cspace.root().mint(self.endpoint.cap(), slot, Badge::new(0xf0511), Rights::ALL)?;
        self.device_client.hook(Badge::null(), target, slot)?;
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
            let mut utcb = unsafe { UTCB::new() };
            utcb.set_reply_window(self.reply.cap());
            utcb.set_recv_window(self.recv);

            if let Err(e) = self.endpoint.recv(&mut utcb) {
                log!("Recv error: {:?}", e);
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
                    log!(
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
            (glenda::protocol::KERNEL_PROTO, glenda::protocol::kernel::NOTIFY) => |s: &mut Self, u: &mut UTCB| {
                handle_notify(u, |u| {
                    let b = u.get_badge();
                    if b.bits() == 0xf0511 {
                        let _ = s.sync_devices();
                    } else if b.bits() & 0x80000000 != 0 {
                        let hw_id = (b.bits() & 0x7fffffff) as u64;
                        if let Some(bclient) = s.device_clients.get(&(hw_id as usize)) {
                            if let Some(ring) = bclient.ring() {
                                while let Some(cqe) = ring.peek_completion() {
                                    for (_, ring) in s.client_rings.iter() {
                                        let _ = ring.complete(cqe.user_data, cqe.res);
                                    }
                                }
                            }
                        }
                    }
                    Ok(())
                })?;
                Err(Error::Success)
            },
            (glenda::protocol::DEVICE_PROTO, glenda::protocol::device::NOTIFY_HOOK) => |s: &mut Self, u: &mut UTCB| {
                handle_notify(u, |u| {
                    let badge = u.get_badge();
                    if badge.bits() == 0xf0511 {
                        let res = s.sync_devices();
                        if let Err(e) = res {
                            log!("Device sync failed: {:?}", e);
                        }
                    }
                    Ok(())
                })?;
                Err(Error::Success)
            },
            (BLOCK_PROTO, block::NOTIFY_IO) => |s: &mut Self, u: &mut UTCB| {
                handle_notify(u, |u| {
                    let badge = u.get_badge();
                    if badge.bits() & 0x80000000 != 0 {
                        // Hardware IO completion
                        let hw_id = (badge.bits() & 0x7fffffff) as u64;
                        if let Some(bclient) = s.device_clients.get(&(hw_id as usize)) {
                            if let Some(ring) = bclient.ring() {
                                while let Some(cqe) = ring.peek_completion() {
                                    log!("Hardware IO completed: user_data={:#x}, res={}", cqe.user_data, cqe.res);
                                    for (_, ring) in s.client_rings.iter() {
                                        let _ = ring.complete(cqe.user_data, cqe.res);
                                    }
                                }
                            }
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
            (BLOCK_PROTO, block::NOTIFY_SQ) => |s: &mut Self, u: &mut UTCB| {
                let badge = u.get_badge();
                handle_notify(u, |u| s.handle_notify_sq(u, badge))?;
                Err(Error::Success)
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
