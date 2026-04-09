use super::{FileSystemType, FsProbeReader};

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct SuperBlock {
    _s_inodes_count: u32,
    _s_blocks_count_lo: u32,
    _s_r_blocks_count_lo: u32,
    _s_free_blocks_count_lo: u32,
    _s_free_inodes_count: u32,
    _s_first_data_block: u32,
    _s_log_block_size: u32,
    _s_log_cluster_size: u32,
    _s_blocks_per_group: u32,
    _s_clusters_per_group: u32,
    _s_inodes_per_group: u32,
    _s_mtime: u32,
    _s_wtime: u32,
    _s_mnt_count: u16,
    _s_max_mnt_count: u16,
    s_magic: u16,
    _s_state: u16,
    _s_errors: u16,
    _s_minor_rev_level: u16,
    _s_lastcheck: u32,
    _s_checkinterval: u32,
    _s_creator_os: u32,
    _s_rev_level: u32,
    _s_def_resuid: u16,
    _s_def_resgid: u16,
    _s_first_ino: u32,
    _s_inode_size: u16,
    _s_block_group_nr: u16,
    s_feature_compat: u32,
    s_feature_incompat: u32,
    _s_feature_ro_compat: u32,
}

const EXT4_SUPER_MAGIC: u16 = 0xEF53;
const EXT4_FEATURE_COMPAT_HAS_JOURNAL: u32 = 0x0004;
const EXT4_FEATURE_INCOMPAT_EXTENTS: u32 = 0x0040;

/// extfs 探测：识别 ext2/ext3/ext4。
pub fn probe_extfs(read_offset: &mut FsProbeReader<'_>) -> Option<FileSystemType> {
    let mut sb_buf = [0u8; 1024];
    if read_offset(1024, &mut sb_buf).is_err() {
        return None;
    }

    let sb = unsafe { core::ptr::read_unaligned(sb_buf.as_ptr() as *const SuperBlock) };

    if sb.s_magic != EXT4_SUPER_MAGIC {
        return None;
    }

    if (sb.s_feature_incompat & EXT4_FEATURE_INCOMPAT_EXTENTS) != 0 {
        Some(FileSystemType::Ext4)
    } else if (sb.s_feature_compat & EXT4_FEATURE_COMPAT_HAS_JOURNAL) != 0 {
        Some(FileSystemType::Ext3)
    } else {
        Some(FileSystemType::Ext2)
    }
}
