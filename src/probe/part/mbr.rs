use super::{PartProbeReader, PartitionRange, ProbeContext};
use crate::utils::mbr::MBR;
use alloc::vec::Vec;

pub fn probe_mbr(
    ctx: &ProbeContext<'_>,
    _read_lba: &mut PartProbeReader<'_>,
) -> Option<Vec<PartitionRange>> {
    let mbr = MBR::parse(ctx.sector0)?;

    // Protective MBR 由 GPT 探测器处理。
    if mbr.is_protective_gpt() {
        return None;
    }

    let mut parts = Vec::new();
    for i in 0..4 {
        if let Some(entry) = &mbr.partitions[i]
            && entry.sectors_count != 0
        {
            parts.push(PartitionRange {
                start_lba: entry.start_lba as usize,
                num_blocks: entry.sectors_count as usize,
            });
        }
    }

    if parts.is_empty() { None } else { Some(parts) }
}
