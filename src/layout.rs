pub const PROBE_VADDR: usize = 0x5000_0000;
pub const RING_ENTRIES: u32 = 4;

use glenda::cap::{CapPtr, Endpoint};

pub const INIT_SLOT: CapPtr = CapPtr::from(14);
pub const DEVICE_SLOT: CapPtr = CapPtr::from(15);
pub const INIT_CAP: Endpoint = Endpoint::from(INIT_SLOT);
pub const DEVICE_CAP: Endpoint = Endpoint::from(DEVICE_SLOT);
