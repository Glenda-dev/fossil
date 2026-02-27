pub const PROBE_VADDR: usize = glenda::mem::SERVICE_POOL_VA;
pub const RING_ENTRIES: u32 = 4;

use glenda::cap::{CapPtr, Endpoint};

pub const INIT_SLOT: CapPtr = CapPtr::from(14);
pub const DEVICE_SLOT: CapPtr = CapPtr::from(15);
pub const INIT_CAP: Endpoint = Endpoint::from(INIT_SLOT);
pub const DEVICE_CAP: Endpoint = Endpoint::from(DEVICE_SLOT);
