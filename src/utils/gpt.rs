use alloc::string::String;
use alloc::vec::Vec;
use core::convert::TryInto;

pub struct GPTHeader {
    pub current_lba: u64,
    pub backup_lba: u64,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    pub partition_entry_lba: u64,
    pub num_partition_entries: u32,
    pub partition_entry_size: u32,
}

#[derive(Debug, Clone)]
pub struct GPTPartition {
    pub type_guid: [u8; 16],
    pub unique_guid: [u8; 16],
    pub first_lba: u64,
    pub last_lba: u64,
    pub attributes: u64,
    pub name: String,
}

impl GPTHeader {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < 92 {
            return None;
        }

        // Signature "EFI PART"
        if &buf[0..8] != b"EFI PART" {
            return None;
        }

        let current_lba = u64::from_le_bytes(buf[24..32].try_into().unwrap());
        let backup_lba = u64::from_le_bytes(buf[32..40].try_into().unwrap());
        let first_usable_lba = u64::from_le_bytes(buf[40..48].try_into().unwrap());
        let last_usable_lba = u64::from_le_bytes(buf[48..56].try_into().unwrap());
        let partition_entry_lba = u64::from_le_bytes(buf[72..80].try_into().unwrap());
        let num_partition_entries = u32::from_le_bytes(buf[80..84].try_into().unwrap());
        let partition_entry_size = u32::from_le_bytes(buf[84..88].try_into().unwrap());

        Some(GPTHeader {
            current_lba,
            backup_lba,
            first_usable_lba,
            last_usable_lba,
            partition_entry_lba,
            num_partition_entries,
            partition_entry_size,
        })
    }
}

impl GPTPartition {
    pub fn parse_entries(buf: &[u8], num: u32, size: u32) -> Vec<Self> {
        let mut entries = Vec::new();
        for i in 0..num {
            let offset = (i * size) as usize;
            if offset + (size as usize) > buf.len() {
                break;
            }
            let entry_buf = &buf[offset..offset + (size as usize)];

            let type_guid: [u8; 16] = entry_buf[0..16].try_into().unwrap_or([0; 16]);
            let unique_guid: [u8; 16] = entry_buf[16..32].try_into().unwrap_or([0; 16]);

            // Check if partition is empty (all zeroes in type guid)
            if type_guid.iter().all(|&b| b == 0) {
                continue;
            }

            let first_lba = u64::from_le_bytes(entry_buf[32..40].try_into().unwrap_or([0; 8]));
            let last_lba = u64::from_le_bytes(entry_buf[40..48].try_into().unwrap_or([0; 8]));
            let attributes = u64::from_le_bytes(entry_buf[48..56].try_into().unwrap_or([0; 8]));

            // Extract partition name (UTF-16LE, 72 bytes)
            let mut name = String::new();
            let name_bytes = &entry_buf[56..128];
            for i in 0..36 {
                let bytes = &name_bytes[i * 2..i * 2 + 2];
                let c = u16::from_le_bytes([bytes[0], bytes[1]]);
                if c == 0 {
                    break;
                }
                if let Some(ch) = char::from_u32(c as u32) {
                    name.push(ch);
                }
            }

            entries.push(GPTPartition {
                type_guid,
                unique_guid,
                first_lba,
                last_lba,
                attributes,
                name: name.clone(), // Ensure it's not a reference
            });
        }
        entries
    }

    pub fn is_active(&self) -> bool {
        self.type_guid.iter().any(|&b| b != 0)
    }
}
