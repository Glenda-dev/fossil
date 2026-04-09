use glenda::error::Error;

pub mod extfs;
pub mod fatfs;
pub mod initrdfs;

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

pub type FsProbeReader<'a> = dyn FnMut(usize, &mut [u8]) -> Result<(), Error> + 'a;
pub type FsProbeFn = for<'a> fn(&mut FsProbeReader<'a>) -> Option<FileSystemType>;

#[derive(Debug, Clone, Copy)]
pub struct FsProber {
    pub name: &'static str,
    pub probe: FsProbeFn,
}

/// 类 Linux 的文件系统探测注册表：按顺序探测，首个匹配即返回。
pub const FILESYSTEM_PROBERS: &[FsProber] = &[
    FsProber { name: "fatfs/exfat", probe: fatfs::probe_fat_exfat },
    FsProber { name: "extfs", probe: extfs::probe_extfs },
    FsProber { name: "initrdfs", probe: initrdfs::probe_initrdfs },
];

/// Detect FS by trying all registered probers in order.
pub fn detect_fs_registered<F>(mut read_offset: F) -> FileSystemType
where
    F: FnMut(usize, &mut [u8]) -> Result<(), Error>,
{
    let reader: &mut FsProbeReader<'_> = &mut read_offset;

    for prober in FILESYSTEM_PROBERS {
        if let Some(found) = (prober.probe)(reader) {
            return found;
        }
    }

    FileSystemType::Unknown
}
