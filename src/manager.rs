use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use glenda::cap::Endpoint;
use glenda::error::Error;
use glenda::ipc::Badge;
use glenda::mem::io_uring::IoUringSqe;
use glenda::protocol::device::{LogicDeviceDesc, LogicDeviceType};
use serde::{Deserialize, Serialize};

use crate::utils::gpt::{GPTHeader, GPTPartition};
use crate::utils::mbr::MBR;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionMetadata {
    pub parent: u64,
    pub start_lba: u64,
    pub num_blocks: u64,
    pub block_size: u64,
}

#[derive(Clone)]
pub struct PartitionProxy {
    pub meta: PartitionMetadata,
}

impl PartitionProxy {
    pub fn new(meta: PartitionMetadata, _endpoint: Endpoint, _name: String) -> Self {
        Self { meta }
    }

    pub fn translate_sqe(&self, sqe: &mut IoUringSqe) {
        sqe.off += self.meta.start_lba * 512;
    }
}

pub struct FossilManager {
    partitions: BTreeMap<usize, PartitionProxy>,
}

impl FossilManager {
    pub fn new(_device_ep: Endpoint) -> Self {
        Self { partitions: BTreeMap::new() }
    }

    pub fn register_proxy(&mut self, badge: usize, proxy: PartitionProxy) {
        self.partitions.insert(badge, proxy);
    }

    pub fn get_partition(&self, badge: Badge) -> Option<&PartitionProxy> {
        self.partitions.get(&(badge.bits()))
    }

    pub fn probe<F>(
        &mut self,
        _parent: usize,
        parent_desc: &LogicDeviceDesc,
        sector0: &[u8],
        num_blocks: u64,
        mut read_fn: F,
    ) -> Vec<(LogicDeviceDesc, u64)>
    where
        F: FnMut(u64, &mut [u8]) -> Result<(), Error>,
    {
        let mut results = Vec::new();

        if sector0.starts_with(b"070701") {
            log!("Initrd/CPIO detected on disk.");
            let mut desc = parent_desc.clone();
            desc.name = String::from("initrd");
            desc.dev_type = LogicDeviceType::Block(num_blocks);
            results.push((desc, 0));
            return results;
        }

        if let Some(mbr) = MBR::parse(sector0) {
            if mbr.is_protective_gpt() {
                log!("GPT detected. Parsing GPT entries...");
                let mut header_buf = [0u8; 512];
                if let Ok(_) = read_fn(1, &mut header_buf) {
                    if let Some(header) = GPTHeader::parse(&header_buf) {
                        let mut entries_buf = [0u8; 128 * 128];
                        let entries_len = header.num_partition_entries as usize
                            * header.partition_entry_size as usize;
                        let entries_blk_count = (entries_len + 511) / 512;

                        let mut offset = 0;
                        for i in 0..entries_blk_count {
                            let mut chunk = [0u8; 512];
                            if read_fn(header.partition_entry_lba + i as u64, &mut chunk).is_ok() {
                                let end = (offset + 512).min(entries_buf.len());
                                entries_buf[offset..end].copy_from_slice(&chunk[..end - offset]);
                                offset = end;
                            }
                        }

                        let gpt_parts = GPTPartition::parse_entries(
                            &entries_buf[..entries_len],
                            header.num_partition_entries,
                            header.partition_entry_size,
                        );
                        for (i, part) in gpt_parts.iter().enumerate() {
                            if part.is_active() {
                                let mut desc = parent_desc.clone();
                                desc.name = String::from(alloc::format!("p{}", i + 1));
                                desc.dev_type =
                                    LogicDeviceType::Block(part.last_lba - part.first_lba + 1);
                                results.push((desc, part.first_lba));
                            }
                        }
                    }
                }
            } else {
                log!("MBR detected. Parsing partitions...");
                for (i, part_opt) in mbr.partitions.iter().enumerate() {
                    if let Some(part) = part_opt {
                        if part.part_type != 0 {
                            let mut desc = parent_desc.clone();
                            desc.name = String::from(alloc::format!("p{}", i + 1));
                            desc.dev_type = LogicDeviceType::Block(part.sectors_count as u64);
                            results.push((desc, part.start_lba as u64));
                        }
                    }
                }
            }
        }

        results
    }
}
