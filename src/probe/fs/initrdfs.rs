use super::{FileSystemType, FsProbeReader};

pub fn probe_initrdfs(read_offset: &mut FsProbeReader<'_>) -> Option<FileSystemType> {
    let mut initrd_header = [0u8; 4];
    if read_offset(0, &mut initrd_header).is_err() {
        return None;
    }

    let magic = u32::from_le_bytes(initrd_header);
    if magic == 0x9999_9999 { Some(FileSystemType::InitrdFS) } else { None }
}
