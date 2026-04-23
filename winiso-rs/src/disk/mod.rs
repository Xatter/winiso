pub mod fat32;
pub mod partition;
pub mod raw_write;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PartitionScheme {
    Mbr,
    Gpt,
}

pub struct PartitionInfo {
    pub offset: u64,
    pub size: u64,
}
