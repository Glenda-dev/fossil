use alloc::string::String;
use alloc::vec::Vec;
use glenda::client::ResourceClient;
use glenda::error::Error;
use glenda::interface::CSpaceService;
use glenda::interface::ResourceService;
use glenda::ipc::Badge;
use glenda::mem::Perms;
use glenda::mem::pool::MemoryPool;
use glenda::utils::manager::{CSpaceManager, VSpaceManager};
use serde::{Deserialize, Serialize};

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
    #[serde(default)]
    pub replacement_policy: String,
    #[serde(default)]
    pub write_policy: String,
    pub filesystems: Vec<FSDriverConfig>,
}

impl FSConfig {
    pub fn load(
        res_client: &mut ResourceClient,
        cspace: &mut CSpaceManager,
        vspace: &mut VSpaceManager,
        mem_pool: &mut MemoryPool,
    ) -> Result<Self, Error> {
        let config_slot = cspace.alloc(res_client)?;
        let result = match res_client.get_config(Badge::null(), "fs.json", config_slot) {
            Ok((frame, size)) => {
                let shm = mem_pool.map_shm(vspace, cspace, res_client, frame, size, Perms::READ)?;
                let data =
                    unsafe { core::slice::from_raw_parts(shm.vaddr() as *const u8, shm.size()) };
                match serde_json::from_slice::<Self>(data) {
                    Ok(config) => Ok(config),
                    Err(_) => Err(Error::InvalidConfig),
                }
            }
            Err(e) => Err(e),
        };
        result
    }
}
