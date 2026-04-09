pub mod gpt;
pub mod mbr;

use alloc::vec::Vec;
use glenda::error::Error;

#[derive(Debug, Clone, Copy)]
pub struct PartitionRange {
    pub start_lba: usize,
    pub num_blocks: usize,
}

pub struct ProbeContext<'a> {
    pub sector0: &'a [u8],
    pub block_size: usize,
}

pub type PartProbeReader<'a> = dyn FnMut(usize, &mut [u8]) -> Result<(), Error> + 'a;
pub type PartProbeFn =
    for<'a, 'b> fn(&ProbeContext<'a>, &mut PartProbeReader<'b>) -> Option<Vec<PartitionRange>>;

#[derive(Debug, Clone, Copy)]
pub struct PartProber {
    pub name: &'static str,
    pub probe: PartProbeFn,
}

/// 类 Linux 的分区表探测注册表：按顺序尝试，每个探测器返回 Some 即认定匹配。
pub const PARTITION_TABLE_PROBERS: &[PartProber] = &[
    PartProber { name: "gpt", probe: gpt::probe_gpt },
    PartProber { name: "mbr", probe: mbr::probe_mbr },
];

pub fn detect_partitions_registered<F>(
    sector0: &[u8],
    block_size: usize,
    mut read_lba: F,
) -> Vec<PartitionRange>
where
    F: FnMut(usize, &mut [u8]) -> Result<(), Error>,
{
    let ctx = ProbeContext { sector0, block_size };
    let reader: &mut PartProbeReader<'_> = &mut read_lba;

    for prober in PARTITION_TABLE_PROBERS {
        if let Some(parts) = (prober.probe)(&ctx, reader)
            && !parts.is_empty()
        {
            return parts;
        }
    }

    Vec::new()
}
