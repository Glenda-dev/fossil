use super::{FileSystemType, FsProbeReader};

const EXT4_SUPER_MAGIC: u16 = 0xEF53;

const EXT_SUPERBLOCK_OFFSET: usize = 1024;
const EXT_SUPERBLOCK_PROBE_SIZE: usize = 104;

const EXT_SB_MAGIC_OFFSET: usize = 0x38;
const EXT_SB_FEATURE_COMPAT_OFFSET: usize = 0x5C;
const EXT_SB_FEATURE_INCOMPAT_OFFSET: usize = 0x60;
const EXT_SB_FEATURE_RO_COMPAT_OFFSET: usize = 0x64;

const EXT4_FEATURE_COMPAT_HAS_JOURNAL: u32 = 0x0004;

const EXT4_FEATURE_INCOMPAT_EXTENTS: u32 = 0x0040;
const EXT4_FEATURE_INCOMPAT_64BIT: u32 = 0x0080;
const EXT4_FEATURE_INCOMPAT_MMP: u32 = 0x0100;
const EXT4_FEATURE_INCOMPAT_FLEX_BG: u32 = 0x0200;
const EXT4_FEATURE_INCOMPAT_EA_INODE: u32 = 0x0400;
const EXT4_FEATURE_INCOMPAT_DIRDATA: u32 = 0x1000;
const EXT4_FEATURE_INCOMPAT_CSUM_SEED: u32 = 0x2000;
const EXT4_FEATURE_INCOMPAT_LARGEDIR: u32 = 0x4000;
const EXT4_FEATURE_INCOMPAT_INLINE_DATA: u32 = 0x8000;
const EXT4_FEATURE_INCOMPAT_ENCRYPT: u32 = 0x10000;
const EXT4_FEATURE_INCOMPAT_CASEFOLD: u32 = 0x40000;

const EXT4_FEATURE_RO_COMPAT_HUGE_FILE: u32 = 0x0008;
const EXT4_FEATURE_RO_COMPAT_DIR_NLINK: u32 = 0x0020;
const EXT4_FEATURE_RO_COMPAT_EXTRA_ISIZE: u32 = 0x0040;
const EXT4_FEATURE_RO_COMPAT_QUOTA: u32 = 0x0100;
const EXT4_FEATURE_RO_COMPAT_BIGALLOC: u32 = 0x0200;
const EXT4_FEATURE_RO_COMPAT_METADATA_CSUM: u32 = 0x0400;
const EXT4_FEATURE_RO_COMPAT_PROJECT: u32 = 0x2000;
const EXT4_FEATURE_RO_COMPAT_VERITY: u32 = 0x8000;
const EXT4_FEATURE_RO_COMPAT_ORPHAN_PRESENT: u32 = 0x10000;

const EXT4_FEATURE_INCOMPAT_MASK: u32 = EXT4_FEATURE_INCOMPAT_EXTENTS
    | EXT4_FEATURE_INCOMPAT_64BIT
    | EXT4_FEATURE_INCOMPAT_MMP
    | EXT4_FEATURE_INCOMPAT_FLEX_BG
    | EXT4_FEATURE_INCOMPAT_EA_INODE
    | EXT4_FEATURE_INCOMPAT_DIRDATA
    | EXT4_FEATURE_INCOMPAT_CSUM_SEED
    | EXT4_FEATURE_INCOMPAT_LARGEDIR
    | EXT4_FEATURE_INCOMPAT_INLINE_DATA
    | EXT4_FEATURE_INCOMPAT_ENCRYPT
    | EXT4_FEATURE_INCOMPAT_CASEFOLD;

const EXT4_FEATURE_RO_COMPAT_MASK: u32 = EXT4_FEATURE_RO_COMPAT_HUGE_FILE
    | EXT4_FEATURE_RO_COMPAT_DIR_NLINK
    | EXT4_FEATURE_RO_COMPAT_EXTRA_ISIZE
    | EXT4_FEATURE_RO_COMPAT_QUOTA
    | EXT4_FEATURE_RO_COMPAT_BIGALLOC
    | EXT4_FEATURE_RO_COMPAT_METADATA_CSUM
    | EXT4_FEATURE_RO_COMPAT_PROJECT
    | EXT4_FEATURE_RO_COMPAT_VERITY
    | EXT4_FEATURE_RO_COMPAT_ORPHAN_PRESENT;

#[inline]
fn read_le_u16(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([buf[offset], buf[offset + 1]])
}

#[inline]
fn read_le_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([buf[offset], buf[offset + 1], buf[offset + 2], buf[offset + 3]])
}

/// extfs 探测：识别 ext2/ext3/ext4。
pub fn probe_extfs(read_offset: &mut FsProbeReader<'_>) -> Option<FileSystemType> {
    let mut sb_buf = [0u8; EXT_SUPERBLOCK_PROBE_SIZE];
    match read_offset(EXT_SUPERBLOCK_OFFSET, &mut sb_buf) {
        Ok(_) => {}
        Err(e) => {
            error!("Failed to read superblock for extfs probe: {:?}", e);
            return None;
        }
    }
    let magic = read_le_u16(&sb_buf, EXT_SB_MAGIC_OFFSET);
    if magic != EXT4_SUPER_MAGIC {
        return None;
    }

    let compat = read_le_u32(&sb_buf, EXT_SB_FEATURE_COMPAT_OFFSET);
    let incompat = read_le_u32(&sb_buf, EXT_SB_FEATURE_INCOMPAT_OFFSET);
    let ro_compat = read_le_u32(&sb_buf, EXT_SB_FEATURE_RO_COMPAT_OFFSET);

    let has_ext4_incompat = (incompat & EXT4_FEATURE_INCOMPAT_MASK) != 0;
    let has_ext4_ro_compat = (ro_compat & EXT4_FEATURE_RO_COMPAT_MASK) != 0;

    if has_ext4_incompat || has_ext4_ro_compat {
        Some(FileSystemType::Ext4)
    } else if (compat & EXT4_FEATURE_COMPAT_HAS_JOURNAL) != 0 {
        Some(FileSystemType::Ext3)
    } else {
        Some(FileSystemType::Ext2)
    }
}
