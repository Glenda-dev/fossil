use super::FossilServer;
use glenda::cap::{CSPACE_CAP, CapPtr, Endpoint, Rights};
use glenda::client::DeviceClient;
use glenda::error::Error;
use glenda::interface::{CSpaceService, DeviceService, VolumeService};
use glenda::ipc::Badge;

impl<'a> VolumeService for FossilServer<'a> {
    fn get_device(&mut self, badge: Badge, _recv: CapPtr) -> Result<Endpoint, Error> {
        let partition_badge = self.driver_to_partition.get(&badge.bits()).ok_or(Error::NotFound)?;
        let slot = self.cspace.alloc(self.res_client)?;
        CSPACE_CAP.mint(self.endpoint.cap(), slot, Badge::new(*partition_badge), Rights::ALL)?;
        Ok(Endpoint::from(slot))
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

    fn mount_partition(
        &mut self,
        _badge: Badge,
        partition_name: &str,
        mount_point: &str,
    ) -> Result<(), Error> {
        log!("Mount requested: {} -> {}", partition_name, mount_point);
        let partition_badge = self.name_to_badge.get(partition_name).ok_or(Error::NotFound)?;
        let proxy = self.partitions.get(partition_badge).ok_or(Error::NotFound)?;
        log!("Partition {} found (fs_type={:?})", partition_name, proxy.fs_type);
        Ok(())
    }
}
