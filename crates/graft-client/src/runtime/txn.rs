use culprit::{Result, ResultExt};
use std::sync::Arc;

use graft_core::{
    page::{Page, EMPTY_PAGE},
    page_offset::PageOffset,
    VolumeId,
};

use crate::ClientErr;

use super::storage::{memtable::Memtable, page::PageValue, snapshot::Snapshot, Storage};

#[derive(Clone, Debug)]
pub struct ReadTxn {
    vid: VolumeId,
    snapshot: Option<Snapshot>,
    storage: Arc<Storage>,
}

impl ReadTxn {
    pub(crate) fn new(vid: VolumeId, snapshot: Option<Snapshot>, storage: Arc<Storage>) -> Self {
        Self { vid, snapshot, storage }
    }

    /// Return the volume ID for this transaction
    pub fn vid(&self) -> &VolumeId {
        &self.vid
    }

    /// Return the snapshot for this transaction
    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    /// Read a page from the snapshot
    pub fn read(&self, offset: PageOffset) -> Result<Page, ClientErr> {
        if let Some(snapshot) = &self.snapshot {
            match self
                .storage
                .read(&self.vid, offset, snapshot.lsn())
                .or_into_ctx()?
            {
                PageValue::Available(page) => Ok(page),
                PageValue::Pending => todo!("download page from remote"),
            }
        } else {
            Ok(EMPTY_PAGE)
        }
    }

    // Upgrade this read transaction into a write transaction.
    pub fn upgrade(self) -> Result<WriteTxn, ClientErr> {
        Ok(WriteTxn::new(self))
    }
}

#[derive(Debug)]
pub struct WriteTxn {
    read_txn: ReadTxn,
    memtable: Memtable,
}

impl WriteTxn {
    pub fn new(read_txn: ReadTxn) -> Self {
        Self { read_txn, memtable: Default::default() }
    }

    /// Returns the volume ID for this transaction
    pub fn vid(&self) -> &VolumeId {
        self.read_txn.vid()
    }

    /// Returns the snapshot backing this transaction
    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.read_txn.snapshot()
    }

    /// Read a page; supports read your own writes (RYOW)
    pub fn read(&self, offset: PageOffset) -> Result<Page, ClientErr> {
        if let Some(page) = self.memtable.get(offset) {
            return Ok(page.clone());
        }
        self.read_txn.read(offset)
    }

    /// Write a page
    pub fn write(&mut self, offset: PageOffset, page: Page) {
        self.memtable.insert(offset, page);
    }

    /// Commit the transaction
    pub fn commit(self) -> Result<ReadTxn, ClientErr> {
        let Self { read_txn, memtable } = self;
        let ReadTxn { vid, snapshot, storage } = read_txn;
        let snapshot = storage.commit(&vid, snapshot, memtable).or_into_ctx()?;
        Ok(ReadTxn::new(vid, Some(snapshot), storage))
    }
}
