use super::{FileSystemType, FsProbeReader};

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct BiosParameterBlock {
    _jmp_boot: [u8; 3],
    _oem_name: [u8; 8],
    byts_per_sec: u16,
    sec_per_clus: u8,
    rsvd_sec_cnt: u16,
    num_fats: u8,
    root_ent_cnt: u16,
    tot_sec_16: u16,
    _media: u8,
    fat_sz_16: u16,
    _sec_per_trk: u16,
    _num_heads: u16,
    _hidd_sec: u32,
    tot_sec_32: u32,
}

pub fn probe_fat_exfat(read_offset: &mut FsProbeReader<'_>) -> Option<FileSystemType> {
    let mut boot_sector = [0u8; 512];
    if read_offset(0, &mut boot_sector).is_err() {
        return None;
    }

    let oem_name = &boot_sector[3..11];
    if oem_name == b"EXFAT   " {
        return Some(FileSystemType::ExFat);
    }

    if boot_sector[510] != 0x55 || boot_sector[511] != 0xAA {
        return None;
    }

    let bpb =
        unsafe { core::ptr::read_unaligned(boot_sector.as_ptr() as *const BiosParameterBlock) };

    let bytes_per_sec = if bpb.byts_per_sec == 0 { 512 } else { bpb.byts_per_sec };
    let fat_sz_32 =
        u32::from_le_bytes([boot_sector[36], boot_sector[37], boot_sector[38], boot_sector[39]]);
    let fat_sz = if bpb.fat_sz_16 != 0 { bpb.fat_sz_16 as u32 } else { fat_sz_32 };
    let tot_sec = if bpb.tot_sec_16 != 0 { bpb.tot_sec_16 as u32 } else { bpb.tot_sec_32 };

    let root_dir_sectors =
        ((bpb.root_ent_cnt as u32 * 32) + (bytes_per_sec as u32 - 1)) / bytes_per_sec as u32;

    let data_sec = tot_sec
        .saturating_sub(bpb.rsvd_sec_cnt as u32)
        .saturating_sub(bpb.num_fats as u32 * fat_sz)
        .saturating_sub(root_dir_sectors);

    let count_of_clusters =
        if bpb.sec_per_clus != 0 { data_sec / bpb.sec_per_clus as u32 } else { 0 };

    if count_of_clusters < 4085 {
        None
    } else if count_of_clusters < 65525 {
        Some(FileSystemType::Fat16)
    } else {
        Some(FileSystemType::Fat32)
    }
}
