//! The cache manages on disk and mem-mapped segments.
//! The cache needs to respect the following limits:
//!   - Disk space
//!   - Maximum open file descriptors (maximum mmap'ed segments)

use std::{future::Future, io, ops::Deref};

use bytes::Bytes;
use graft_core::guid::SegmentId;

pub trait Cache: Send + Sync {
    type Item<'a>: Deref<Target = [u8]>
    where
        Self: 'a;

    fn put(&self, sid: &SegmentId, data: Bytes) -> impl Future<Output = io::Result<()>> + Send;

    fn get(
        &self,
        sid: &SegmentId,
    ) -> impl Future<Output = io::Result<Option<Self::Item<'_>>>> + Send;
}
