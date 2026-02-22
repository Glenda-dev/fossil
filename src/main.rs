#![no_std]
#![allow(dead_code)]
#![no_main]

mod utils;
#[macro_use]
extern crate glenda;
extern crate alloc;

mod fossil;
mod layout;

use crate::fossil::FossilServer;
use crate::layout::{DEVICE_CAP, DEVICE_SLOT, INIT_CAP, INIT_SLOT};
use glenda::cap::{
    CSPACE_CAP, CapType, ENDPOINT_SLOT, Endpoint, MONITOR_CAP, RECV_SLOT, REPLY_SLOT,
};
use glenda::client::{DeviceClient, InitClient, ResourceClient};
use glenda::interface::{ResourceService, SystemService};
use glenda::ipc::Badge;
use glenda::protocol::resource::{DEVICE_ENDPOINT, INIT_ENDPOINT, ResourceType};
use glenda::utils::manager::CSpaceManager;

#[unsafe(no_mangle)]
fn main() {
    glenda::console::init_logging("Fossil");
    log!("Starting Fossil Partition Manager...");

    let mut res_client = ResourceClient::new(MONITOR_CAP);
    res_client
        .get_cap(Badge::null(), ResourceType::Endpoint, INIT_ENDPOINT, INIT_SLOT)
        .expect("Fossil: Failed to get init endpoint cap");
    let mut init_client = InitClient::new(INIT_CAP);
    let mut cspace = CSpaceManager::new(CSPACE_CAP, 16);

    res_client
        .alloc(Badge::null(), CapType::Endpoint, 0, ENDPOINT_SLOT)
        .expect("Fossil: Failed to create endpoint cap");
    let ep = Endpoint::from(ENDPOINT_SLOT);

    res_client
        .get_cap(Badge::null(), ResourceType::Endpoint, DEVICE_ENDPOINT, DEVICE_SLOT)
        .expect("Fossil: Failed to get device endpoint cap");
    let mut dev_client = DeviceClient::new(DEVICE_CAP);

    log!("Starting server loop...");
    let mut server =
        FossilServer::new(ep, &mut res_client, &mut cspace, &mut dev_client, &mut init_client);
    server.init().expect("Fossil: Init failed");
    server.listen(ep, REPLY_SLOT, RECV_SLOT).expect("Fossil: Failed to listen");
    server.run().expect("Fossil: Server failed");
}
