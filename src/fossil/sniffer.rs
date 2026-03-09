// use core::mem;
// use core::slice;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSystemType {
    Fat16,
    Fat32,
    ExFat,
    Ext2,
    Ext3,
    Ext4,
    InitrdFS,
    Unknown,
}

// Minimal struct definitions to avoid dependencies
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct BiosParameterBlock {
    jmp_boot: [u8; 3],
    oem_name: [u8; 8],
    byts_per_sec: u16,
    sec_per_clus: u8,
    rsvd_sec_cnt: u16,
    num_fats: u8,
    root_ent_cnt: u16,
    tot_sec_16: u16,
    media: u8,
    fat_sz_16: u16,
    sec_per_trk: u16,
    num_heads: u16,
    hidd_sec: u32,
    tot_sec_32: u32,
    // ... we don't need the rest for simple detection
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct SuperBlock {
    s_inodes_count: u32,         // 0x0
    s_blocks_count_lo: u32,      // 0x4
    s_r_blocks_count_lo: u32,    // 0x8
    s_free_blocks_count_lo: u32, // 0xC
    s_free_inodes_count: u32,    // 0x10
    s_first_data_block: u32,     // 0x14
    s_log_block_size: u32,       // 0x18
    s_log_cluster_size: u32,     // 0x1C
    s_blocks_per_group: u32,     // 0x20
    s_clusters_per_group: u32,   // 0x24
    s_inodes_per_group: u32,     // 0x28
    s_mtime: u32,                // 0x2C
    s_wtime: u32,                // 0x30
    s_mnt_count: u16,            // 0x34
    s_max_mnt_count: u16,        // 0x36
    s_magic: u16,                // 0x38
    s_state: u16,                // 0x3A
    s_errors: u16,               // 0x3C
    s_minor_rev_level: u16,      // 0x3E
    s_lastcheck: u32,            // 0x40
    s_checkinterval: u32,        // 0x44
    s_creator_os: u32,           // 0x48
    s_rev_level: u32,            // 0x4C
    s_def_resuid: u16,           // 0x50
    s_def_resgid: u16,           // 0x52
    s_first_ino: u32,            // 0x54
    s_inode_size: u16,           // 0x58
    s_block_group_nr: u16,       // 0x5A
    s_feature_compat: u32,       // 0x5C
    s_feature_incompat: u32,     // 0x60
    s_feature_ro_compat: u32,    // 0x64
}

const EXT4_SUPER_MAGIC: u16 = 0xEF53;
const EXT4_FEATURE_COMPAT_HAS_JOURNAL: u32 = 0x0004;
const EXT4_FEATURE_INCOMPAT_EXTENTS: u32 = 0x0040;

/// Detects the filesystem type on a device using the provided block reader.
/// The reader should support reading byte offsets.
pub fn detect_fs<F, E>(mut read_offset: F) -> FileSystemType
where
    F: FnMut(usize, &mut [u8]) -> Result<(), E>,
{
    // 1. Check for FAT / ExFAT at offset 0
    let mut boot_sector = [0u8; 512];
    if read_offset(0, &mut boot_sector).is_ok() {
        // ExFAT Check
        let oem_name = &boot_sector[3..11];
        if oem_name == b"EXFAT   " {
            return FileSystemType::ExFat;
        }

        // FAT Signature Check
        if boot_sector[510] == 0x55 && boot_sector[511] == 0xAA {
            // Parse BPB
            let bpb = unsafe {
                core::ptr::read_unaligned(boot_sector.as_ptr() as *const BiosParameterBlock)
            };

            let bytes_per_sec = if bpb.byts_per_sec == 0 { 512 } else { bpb.byts_per_sec };
            let root_ent_cnt = bpb.root_ent_cnt;
            // Handle FAT32 vs FAT16 fields
            // fat_sz_16 is at offset 22 (u16), accessible via bpb.fat_sz_16
            let fat_sz_16 = bpb.fat_sz_16;

            // tot_sec_16 is at offset 19 (u16), accessible via bpb.tot_sec_16
            let tot_sec_16 = bpb.tot_sec_16;

            // fat_sz_32 is at offset 36 (u32).
            // Our BiosParameterBlock ends at offset 36 (size 36 bytes).
            // So we need to read it from the raw buffer.
            let fat_sz_32 = u32::from_le_bytes([
                boot_sector[36],
                boot_sector[37],
                boot_sector[38],
                boot_sector[39],
            ]);

            // tot_sec_32 is at offset 32 (u32), accessible via bpb.tot_sec_32
            let tot_sec_32 = bpb.tot_sec_32;

            let fat_sz = if fat_sz_16 != 0 { fat_sz_16 as u32 } else { fat_sz_32 };
            let tot_sec = if tot_sec_16 != 0 { tot_sec_16 as u32 } else { tot_sec_32 };

            let root_dir_sectors =
                ((root_ent_cnt as u32 * 32) + (bytes_per_sec as u32 - 1)) / bytes_per_sec as u32;

            let data_sec = tot_sec
                .saturating_sub(bpb.rsvd_sec_cnt as u32)
                .saturating_sub(bpb.num_fats as u32 * fat_sz)
                .saturating_sub(root_dir_sectors);

            let count_of_clusters =
                if bpb.sec_per_clus != 0 { data_sec / bpb.sec_per_clus as u32 } else { 0 };

            if count_of_clusters < 4085 {
                // FAT12 - Not explicitly supported by enum, but technically FAT
                // Treat as Unknown or maybe logic falls through?
                // The request only asked for FAT16/FAT32/ExFAT.
            } else if count_of_clusters < 65525 {
                return FileSystemType::Fat16;
            } else {
                return FileSystemType::Fat32;
            }
        }
    }

    // 2. Check for Ext2/3/4 at offset 1024
    let mut sb_buf = [0u8; 1024]; // Superblock is 1024 bytes
    // Just read the first part enough for struct
    if read_offset(1024, &mut sb_buf).is_ok() {
        // We only need to check the magic and feature flags.
        // We can cast the buffer.
        let sb = unsafe { core::ptr::read_unaligned(sb_buf.as_ptr() as *const SuperBlock) };

        if sb.s_magic == EXT4_SUPER_MAGIC {
            if (sb.s_feature_incompat & EXT4_FEATURE_INCOMPAT_EXTENTS) != 0 {
                return FileSystemType::Ext4;
            } else if (sb.s_feature_compat & EXT4_FEATURE_COMPAT_HAS_JOURNAL) != 0 {
                return FileSystemType::Ext3;
            } else {
                return FileSystemType::Ext2;
            }
        }
    }

    // 3. Check for Initrd
    let mut initrd_header = [0u8; 4];
    if read_offset(0, &mut initrd_header).is_ok() {
        let magic = u32::from_le_bytes(initrd_header);
        if magic == 0x99999999 {
            return FileSystemType::InitrdFS;
        }
    }

    FileSystemType::Unknown
}
