use core::convert::TryInto;

#[derive(Debug, Clone, Copy)]
pub struct MBRPartition {
    pub part_type: u8,
    pub start_lba: u32,
    pub sectors_count: u32,
    pub is_bootable: bool,
}

pub struct MBR {
    pub partitions: [Option<MBRPartition>; 4],
}

impl MBR {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < 512 {
            return None;
        }

        // Check signature 0x55AA
        if buf[510] != 0x55 || buf[511] != 0xAA {
            return None;
        }

        let mut partitions = [None; 4];
        let table_start = 0x1BE;

        for i in 0..4 {
            let offset = table_start + (i * 16);
            let entry = &buf[offset..offset + 16];

            let status = entry[0];
            let part_type = entry[4];
            let start_lba = u32::from_le_bytes(entry[8..12].try_into().unwrap());
            let sectors_count = u32::from_le_bytes(entry[12..16].try_into().unwrap());

            if part_type != 0 {
                partitions[i] = Some(MBRPartition {
                    part_type,
                    start_lba,
                    sectors_count,
                    is_bootable: (status & 0x80) != 0,
                });
            }
        }

        Some(MBR { partitions })
    }

    pub fn is_protective_gpt(&self) -> bool {
        for p in self.partitions.iter().flatten() {
            if p.part_type == 0xEE {
                return true;
            }
        }
        false
    }
}
