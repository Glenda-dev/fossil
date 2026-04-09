use super::{PartProbeReader, PartitionRange, ProbeContext};
use crate::utils::gpt::{GPTHeader, GPTPartition};
use crate::utils::mbr::MBR;
use alloc::vec;
use alloc::vec::Vec;

pub fn probe_gpt(
    ctx: &ProbeContext<'_>,
    read_lba: &mut PartProbeReader<'_>,
) -> Option<Vec<PartitionRange>> {
    let mbr = MBR::parse(ctx.sector0)?;
    if !mbr.is_protective_gpt() {
        return None;
    }

    let gpt_header_buf = if ctx.block_size == 512 {
        let mut buf = [0u8; 512];
        if read_lba(1, &mut buf).is_ok() { Some(buf) } else { None }
    } else if ctx.block_size >= 1024 && ctx.sector0.len() >= 1024 {
        let mut buf = [0u8; 512];
        buf.copy_from_slice(&ctx.sector0[512..1024]);
        Some(buf)
    } else {
        None
    }?;

    let header = GPTHeader::parse(&gpt_header_buf)?;
    let entries_len = header.num_partition_entries as usize * header.partition_entry_size as usize;
    if entries_len == 0 {
        return None;
    }

    let entries_blk_count = (entries_len + ctx.block_size - 1) / ctx.block_size;
    let mut entries_buf = vec![0u8; entries_blk_count * ctx.block_size];
    if read_lba(
        header.partition_entry_lba as usize,
        &mut entries_buf[..entries_blk_count * ctx.block_size],
    )
    .is_err()
    {
        return None;
    }

    let gpt_parts = GPTPartition::parse_entries(
        &entries_buf[..entries_len],
        header.num_partition_entries,
        header.partition_entry_size,
    );

    let mut parts = Vec::new();
    for p in &gpt_parts {
        if p.is_active() && p.last_lba >= p.first_lba {
            parts.push(PartitionRange {
                start_lba: p.first_lba as usize,
                num_blocks: (p.last_lba - p.first_lba + 1) as usize,
            });
        }
    }

    if parts.is_empty() { None } else { Some(parts) }
}
