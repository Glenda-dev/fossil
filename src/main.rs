#![no_std]
#![allow(dead_code)]
#![no_main]

mod utils;
#[macro_use]
extern crate glenda;
extern crate alloc;

mod layout;
mod manager;
mod server;

use glenda::cap::{CSPACE_CAP, CapType, Endpoint, MONITOR_CAP, RECV_SLOT, REPLY_SLOT};
use glenda::client::ResourceClient;
use glenda::interface::{ResourceService, SystemService};
use glenda::protocol::resource::{DEVICE_ENDPOINT, ResourceType};
use glenda::utils::manager::{CSpaceManager, CSpaceService};
use manager::FossilManager;
use server::FossilServer;

#[unsafe(no_mangle)]
fn main() {
    glenda::console::init_logging("Fossil");
    log!("Starting Fossil Partition Manager...");

    let mut res_client = ResourceClient::new(MONITOR_CAP);
    let mut cspace = CSpaceManager::new(CSPACE_CAP, 64);

    let ep_slot = cspace.alloc(&mut res_client).expect("Fossil: Failed to alloc endpoint slot");
    res_client
        .alloc(glenda::ipc::Badge::null(), CapType::Endpoint, 0, ep_slot)
        .expect("Fossil: Failed to create endpoint cap");
    let ep = Endpoint::from(ep_slot);

    let device_ep_slot =
        cspace.alloc(&mut res_client).expect("Fossil: Failed to alloc device_ep slot");
    res_client
        .get_cap(
            glenda::ipc::Badge::null(),
            ResourceType::Endpoint,
            DEVICE_ENDPOINT,
            device_ep_slot,
        )
        .expect("Fossil: Failed to get Unicorn endpoint");
    let device_ep = Endpoint::from(device_ep_slot);

    let manager = FossilManager::new(device_ep);

    log!("Starting server loop...");
    let mut server = FossilServer::new(ep, manager, &mut res_client, &mut cspace, device_ep);
    server.init().expect("Fossil: Init failed");
    server.listen(ep, REPLY_SLOT, RECV_SLOT).expect("Fossil: Failed to listen");
    server.run().expect("Fossil: Server failed");
}
