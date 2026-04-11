use super::FossilServer;
use glenda::cap::{CSPACE_CAP, CapPtr, Endpoint, Rights};
use glenda::client::DeviceClient;
use glenda::error::Error;
use glenda::interface::{CSpaceService, DeviceService, VolumeService};
use glenda::ipc::{Badge, MsgFlags, MsgTag, UTCB};
use glenda::protocol;
use glenda::protocol::init::ServiceState;

impl<'a> FossilServer<'a> {
    fn mint_partition_endpoint(&mut self, partition_badge: usize) -> Result<Endpoint, Error> {
        let slot = self.cspace.alloc(self.res_client)?;
        CSPACE_CAP.mint_self(
            self.ipc.endpoint.cap(),
            slot,
            Badge::new(partition_badge),
            Rights::ALL,
        )?;
        Ok(Endpoint::from(slot))
    }

    fn queue_mount_reply(&mut self, partition_badge: usize) -> Result<(), Error> {
        let reply_slot = self.cspace.alloc(self.res_client)?;
        CSPACE_CAP.transfer_self(self.ipc.reply.cap(), reply_slot)?;
        self.pending_mount_replies.entry(partition_badge).or_default().push_back(reply_slot);
        Ok(())
    }

    fn wake_mount_waiters(
        &mut self,
        partition_badge: usize,
        state: ServiceState,
    ) -> Result<(), Error> {
        let Some(waiters) = self.pending_mount_replies.remove(&partition_badge) else {
            return Ok(());
        };

        for reply_slot in waiters {
            let mut utcb = unsafe { UTCB::new() };
            utcb.clear();

            match state {
                ServiceState::Running => {
                    let Some(fs_ep) = self.partition_fs_endpoints.get(&partition_badge) else {
                        utcb.set_msg_tag(MsgTag::err());
                        utcb.set_mr(0, Error::NotFound as usize);
                        let _ = glenda::cap::Reply::from(reply_slot).reply(&mut utcb);
                        let _ = CSPACE_CAP.delete(reply_slot);
                        continue;
                    };
                    let slot = self.cspace.alloc(self.res_client)?;
                    CSPACE_CAP.copy_self(fs_ep.cap(), slot, Rights::ALL)?;
                    utcb.set_cap_transfer(slot);
                    utcb.set_msg_tag(MsgTag::new(
                        protocol::GENERIC_PROTO,
                        protocol::generic::REPLY,
                        MsgFlags::OK | MsgFlags::HAS_CAP,
                    ));
                }
                ServiceState::Failed => {
                    utcb.set_msg_tag(MsgTag::err());
                    utcb.set_mr(0, Error::DeviceError as usize);
                }
                ServiceState::Stopped | ServiceState::Exited => {
                    utcb.set_msg_tag(MsgTag::err());
                    utcb.set_mr(0, Error::NotFound as usize);
                }
                ServiceState::Starting => {
                    // Still not ready: put it back and skip wake.
                    self.pending_mount_replies
                        .entry(partition_badge)
                        .or_default()
                        .push_back(reply_slot);
                    continue;
                }
            }

            let _ = glenda::cap::Reply::from(reply_slot).reply(&mut utcb);
            let _ = CSPACE_CAP.delete(reply_slot);
        }

        Ok(())
    }

    pub(crate) fn handle_mount_partition_request(&mut self, utcb: &mut UTCB) -> Result<(), Error> {
        let partition_name = unsafe { utcb.read_str()? };
        log!("Mount requested for partition: {}", partition_name);

        let partition_badge = *self.name_to_badge.get(&partition_name).ok_or(Error::NotFound)?;
        let fs_type = self.partitions.get(&partition_badge).ok_or(Error::NotFound)?.fs_type;
        log!("Partition {} found (fs_type={:?})", partition_name, fs_type);

        self.ensure_partition_fs_driver(partition_badge, &partition_name, fs_type)?;

        if let Some(fs_ep) = self.partition_fs_endpoints.get(&partition_badge) {
            let slot = self.cspace.alloc(self.res_client)?;
            CSPACE_CAP.copy_self(fs_ep.cap(), slot, Rights::ALL)?;
            utcb.set_cap_transfer(slot);
            utcb.set_msg_tag(MsgTag::new(
                protocol::GENERIC_PROTO,
                protocol::generic::REPLY,
                MsgFlags::OK | MsgFlags::HAS_CAP,
            ));
            return Ok(());
        }

        self.queue_mount_reply(partition_badge)?;
        Err(Error::Success)
    }
}

impl<'a> VolumeService for FossilServer<'a> {
    fn get_device(&mut self, badge: Badge, _recv: CapPtr) -> Result<Endpoint, Error> {
        let badge_bits = badge.bits();
        let partition_badge = if let Some(mapped) = self.driver_to_partition.get(&badge_bits) {
            *mapped
        } else if self.partitions.contains_key(&badge_bits) {
            badge_bits
        } else {
            return Err(Error::NotFound);
        };

        self.mint_partition_endpoint(partition_badge)
    }

    fn probe_device(&mut self, _badge: Badge, device_name: &str) -> Result<(), Error> {
        log!("Explicit probe requested for device: {}", device_name);
        let mut dev_client = DeviceClient::new(glenda::cap::MONITOR_CAP);
        let query = glenda::protocol::device::DeviceQuery {
            name: Some(device_name.into()),
            compatible: alloc::vec![],
            dev_type: None,
        };
        let names = dev_client.query(Badge::null(), query)?;
        if names.is_empty() {
            return Err(Error::NotFound);
        }
        self.sync_devices()
    }

    fn report_state(
        &mut self,
        badge: Badge,
        state: ServiceState,
        endpoint: Option<CapPtr>,
    ) -> Result<(), Error> {
        let driver_pid = badge.bits();
        let partition_badge = *self.driver_to_partition.get(&driver_pid).ok_or(Error::NotFound)?;

        self.partition_driver_states.insert(partition_badge, state);

        if state == ServiceState::Running {
            let endpoint_cap = endpoint.ok_or(Error::InvalidCapability)?;
            let slot = self.cspace.alloc(self.res_client)?;
            CSPACE_CAP.transfer_self(endpoint_cap, slot)?;
            self.partition_fs_endpoints.insert(partition_badge, Endpoint::from(slot));
        }

        self.wake_mount_waiters(partition_badge, state)?;

        Ok(())
    }

    fn mount_partition(
        &mut self,
        _badge: Badge,
        partition_name: &str,
        _recv: CapPtr,
    ) -> Result<Endpoint, Error> {
        log!("Mount requested for partition: {}", partition_name);
        let partition_badge = *self.name_to_badge.get(partition_name).ok_or(Error::NotFound)?;
        let proxy = self.partitions.get(&partition_badge).ok_or(Error::NotFound)?;
        log!("Partition {} found (fs_type={:?})", partition_name, proxy.fs_type);

        self.ensure_partition_fs_driver(partition_badge, partition_name, proxy.fs_type)?;

        let fs_ep = self.partition_fs_endpoints.get(&partition_badge).ok_or(Error::NotFound)?;
        let slot = self.cspace.alloc(self.res_client)?;
        CSPACE_CAP.copy_self(fs_ep.cap(), slot, Rights::ALL)?;
        Ok(Endpoint::from(slot))
    }
}
