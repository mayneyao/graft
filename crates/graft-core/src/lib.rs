pub mod byte_unit;
pub mod gid;
pub mod hash_table;
pub mod lsn;
pub mod page;
pub mod page_count;
pub mod page_index;
pub mod zerocopy_ext;

pub use gid::{ClientId, SegmentId, VolumeId};
pub use page_count::PageCount;
pub use page_index::PageIdx;

#[cfg(any(test, feature = "testutil"))]
pub mod testutil;

static_assertions::assert_cfg!(
    target_endian = "little",
    "Graft currently only supports little-endian systems"
);
