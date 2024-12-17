use std::{
    fmt::Debug,
    io,
    path::Path,
    sync::{Arc, Mutex},
};

use bytes::Bytes;
use commit::CommitKey;
use fjall::{KvSeparationOptions, PartitionCreateOptions};
use graft_core::{
    byte_unit::ByteUnit,
    lsn::LSN,
    page::{PageSizeErr, EMPTY_PAGE},
    page_offset::PageOffset,
    zerocopy_err::ZerocopyErr,
    VolumeId,
};
use memtable::Memtable;
use page::{PageKey, PageValue};
use snapshot::{Snapshot, SnapshotKey, SnapshotKind, SnapshotKindMask, SnapshotSet};
use splinter::Splinter;
use tokio::sync::{futures::Notified, Notify};
use tryiter::{TryIterator, TryIteratorExt};
use volume::{SyncDirection, VolumeConfig};
use zerocopy::{IntoBytes, TryFromBytes};

pub(crate) mod commit;
pub(crate) mod memtable;
pub(crate) mod page;
pub mod snapshot;
pub mod volume;

#[derive(Debug, thiserror::Error)]
pub enum StorageErr {
    #[error(transparent)]
    FjallErr(#[from] fjall::Error),

    #[error(transparent)]
    IoErr(#[from] io::Error),

    #[error("Corrupt key: {0}")]
    CorruptKey(ZerocopyErr),

    #[error("Corrupt snapshot: {0}")]
    CorruptSnapshot(ZerocopyErr),

    #[error("Corrupt volume config: {0}")]
    CorruptVolumeConfig(ZerocopyErr),

    #[error("Corrupt page: {0}")]
    CorruptPage(#[from] PageSizeErr),

    #[error("Illegal concurrent write to volume {0}")]
    ConcurrentWrite(VolumeId),
}

impl From<lsm_tree::Error> for StorageErr {
    fn from(err: lsm_tree::Error) -> Self {
        StorageErr::FjallErr(err.into())
    }
}

pub struct Storage {
    keyspace: fjall::Keyspace,

    /// Used to store volume configs
    /// maps from VolumeId to VolumeConfig
    volumes: fjall::Partition,

    /// Used to store volume snapshots
    /// maps from (VolumeId, SnapshotKind) to Snapshot
    snapshots: fjall::Partition,

    /// Used to store page contents
    /// maps from (VolumeId, Offset, LSN) to PageValue
    pages: fjall::Partition,

    /// Used to track changes made by local commits.
    /// maps from (VolumeId, LSN) to Splinter of written offsets
    commits: fjall::Partition,

    /// Used to serialize the Volume commit process
    commit_lock: Arc<Mutex<()>>,

    /// Used to notify subscribers of new commits
    commit_notify: Notify,
}

impl Storage {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, StorageErr> {
        Self::open_config(fjall::Config::new(path))
    }

    pub fn open_temporary() -> Result<Self, StorageErr> {
        Self::open_config(fjall::Config::new(tempfile::tempdir()?.into_path()).temporary(true))
    }

    pub fn open_config(config: fjall::Config) -> Result<Self, StorageErr> {
        let keyspace = config.open()?;
        let volumes = keyspace.open_partition("volumes", Default::default())?;
        let snapshots = keyspace.open_partition("snapshots", Default::default())?;
        let pages = keyspace.open_partition(
            "pages",
            PartitionCreateOptions::default().with_kv_separation(KvSeparationOptions::default()),
        )?;
        let commits = keyspace.open_partition(
            "commits",
            PartitionCreateOptions::default().with_kv_separation(KvSeparationOptions::default()),
        )?;
        Ok(Storage {
            keyspace,
            volumes,
            snapshots,
            pages,
            commits,
            commit_lock: Default::default(),
            commit_notify: Default::default(),
        })
    }

    pub fn listen_for_commit(&self) -> Notified<'_> {
        self.commit_notify.notified()
    }

    pub fn add_volume(&self, vid: &VolumeId, config: VolumeConfig) -> Result<(), StorageErr> {
        Ok(self.volumes.insert(vid, config)?)
    }

    pub fn query_volumes(
        &self,
        sync: SyncDirection,
        kind_mask: SnapshotKindMask,
    ) -> impl TryIterator<Ok = (VolumeId, SnapshotSet), Err = StorageErr> + '_ {
        let seqno = self.keyspace.instant();
        let volumes = self.volumes.snapshot_at(seqno).iter().err_into();

        volumes.try_filter_map(move |(vid, config)| {
            let config = VolumeConfig::try_read_from_bytes(&config)
                .map_err(|e| StorageErr::CorruptVolumeConfig(e.into()))?;
            if sync.matches(config.sync()) {
                let vid = VolumeId::try_read_from_bytes(&vid)
                    .map_err(|e| StorageErr::CorruptKey(e.into()))?;
                let set = self.snapshots_with_seqno(seqno, &vid, kind_mask)?;
                Ok(Some((vid, set)))
            } else {
                Ok(None)
            }
        })
    }

    pub fn snapshots(
        &self,
        vid: &VolumeId,
        kind_mask: SnapshotKindMask,
    ) -> Result<SnapshotSet, StorageErr> {
        let seqno = self.keyspace.instant();
        self.snapshots_with_seqno(seqno, vid, kind_mask)
    }

    fn snapshots_with_seqno(
        &self,
        seqno: u64,
        vid: &VolumeId,
        kind_mask: SnapshotKindMask,
    ) -> Result<SnapshotSet, StorageErr> {
        let mut snapshots = self
            .snapshots
            .snapshot_at(seqno)
            .prefix(vid)
            .err_into::<StorageErr>()
            .try_filter_map(move |(k, v)| {
                let key = SnapshotKey::try_read_from_bytes(&k)
                    .map_err(|e| StorageErr::CorruptKey(e.into()))?;
                if kind_mask.contains(key.kind()) {
                    let val = Snapshot::try_read_from_bytes(&v)
                        .map_err(|e| StorageErr::CorruptSnapshot(e.into()))?;
                    Ok(Some((key, val)))
                } else {
                    Ok(None)
                }
            });

        let mut set = SnapshotSet::default();
        while let Some((key, snapshot)) = snapshots.try_next()? {
            assert_eq!(key.vid(), vid);
            set.insert(key.kind(), snapshot);
        }
        Ok(set)
    }

    pub fn snapshot(
        &self,
        vid: &VolumeId,
        kind: SnapshotKind,
    ) -> Result<Option<Snapshot>, StorageErr> {
        let key = snapshot::SnapshotKey::new(vid.clone(), kind);
        if let Some(snapshot) = self.snapshots.get(key)? {
            Ok(Some(
                Snapshot::try_read_from_bytes(&snapshot)
                    .map_err(|e| StorageErr::CorruptSnapshot(e.into()))?,
            ))
        } else {
            Ok(None)
        }
    }

    pub fn read(
        &self,
        vid: &VolumeId,
        offset: PageOffset,
        lsn: LSN,
    ) -> Result<PageValue, StorageErr> {
        let zero = PageKey::new(vid.clone(), offset, LSN::ZERO);
        let key = PageKey::new(vid.clone(), offset, lsn);
        let range = zero..=key;

        // Search for the latest page between LSN(0) and the requested LSN,
        // returning None if no page is found.
        if let Some((_, page)) = self.pages.snapshot().range(range).next_back().transpose()? {
            let bytes: Bytes = page.into();
            Ok(bytes.try_into()?)
        } else {
            Ok(PageValue::Available(EMPTY_PAGE))
        }
    }

    pub fn commit(
        &self,
        vid: &VolumeId,
        snapshot: Option<Snapshot>,
        memtable: Memtable,
    ) -> Result<Snapshot, StorageErr> {
        let mut batch = self.keyspace.batch();
        let read_lsn = snapshot.as_ref().map(|s| s.lsn());
        let mut max_offset = snapshot
            .and_then(|s| s.page_count().last_offset())
            .unwrap_or(PageOffset::ZERO);
        let commit_lsn = read_lsn
            .map(|lsn| lsn.next().expect("lsn overflow"))
            .unwrap_or_default();

        // construct a changed offsets splinter
        let mut offsets = Splinter::default();

        // write out the memtable
        let mut page_key = PageKey::new(vid.clone(), PageOffset::ZERO, commit_lsn);
        for (offset, page) in memtable {
            page_key.set_offset(offset);
            max_offset = max_offset.max(offset);
            offsets.insert(offset.into());
            batch.insert(&self.pages, page_key.as_bytes(), page);
        }

        // write out a new volume snapshot
        let snapshot_key = SnapshotKey::new(vid.clone(), SnapshotKind::Local);
        let snapshot = Snapshot::new(commit_lsn, max_offset.pages());
        batch.insert(&self.snapshots, snapshot_key, snapshot.as_bytes());

        // write out a new commit
        let commit_key = CommitKey::new(vid.clone(), commit_lsn);
        batch.insert(&self.commits, commit_key, offsets.serialize_to_bytes());

        // acquire the commit lock
        let _permit = self.commit_lock.lock().expect("commit lock poisoned");

        // check to see if the read snapshot is the latest local snapshot while
        // holding the commit lock
        let latest = self.snapshot(vid, SnapshotKind::Local)?;
        if latest.map(|l| l.lsn()) != read_lsn {
            return Err(StorageErr::ConcurrentWrite(vid.clone()));
        }

        // commit the changes
        batch.commit()?;

        // notify listeners of the new commit
        self.commit_notify.notify_waiters();

        // return the new snapshot
        Ok(snapshot)
    }
}

impl Debug for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Storage")
            .field("disk usage", &ByteUnit::new(self.keyspace.disk_space()))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use graft_core::page::Page;

    use super::*;

    #[test]
    fn test_query_volumes() {
        let storage = Storage::open_temporary().unwrap();

        let mut memtable = Memtable::default();
        memtable.insert(0.into(), Page::test_filled(0x42));

        let mut vids = [VolumeId::random(), VolumeId::random()];
        vids.sort();

        // first volume has two commits, and is configured to pull
        storage
            .add_volume(&vids[0], VolumeConfig::new(SyncDirection::Pull))
            .unwrap();
        let snapshot = storage.commit(&vids[0], None, memtable.clone()).unwrap();
        storage
            .commit(&vids[0], Some(snapshot), memtable.clone())
            .unwrap();

        // second volume has one commit, and is configured to push
        storage
            .add_volume(&vids[1], VolumeConfig::new(SyncDirection::Push))
            .unwrap();
        storage.commit(&vids[1], None, memtable.clone()).unwrap();

        // ensure that we can query back out the snapshots
        let sync = SyncDirection::Both;
        let mask = SnapshotKindMask::default().with(SnapshotKind::Local);
        let mut iter = storage.query_volumes(sync, mask);

        // check the first volume
        let (vid, set) = iter.try_next().unwrap().unwrap();
        assert_eq!(vid, vids[0]);
        let snapshot = set.get(SnapshotKind::Local).unwrap();
        assert_eq!(snapshot.lsn(), LSN::new(1));
        assert_eq!(snapshot.page_count(), 1);

        // check the second volume
        let (vid, set) = iter.try_next().unwrap().unwrap();
        assert_eq!(vid, vids[1]);
        let snapshot = set.get(SnapshotKind::Local).unwrap();
        assert_eq!(snapshot.lsn(), LSN::new(0));
        assert_eq!(snapshot.page_count(), 1);

        assert!(iter.next().is_none());

        // verify that the sync direction filter works
        let sync = SyncDirection::Push;
        let mask = SnapshotKindMask::default().with(SnapshotKind::Local);
        let mut iter = storage.query_volumes(sync, mask);

        // should be the second volume
        let (vid, set) = iter.try_next().unwrap().unwrap();
        assert_eq!(vid, vids[1]);
        let snapshot = set.get(SnapshotKind::Local).unwrap();
        assert_eq!(snapshot.lsn(), LSN::new(0));
        assert_eq!(snapshot.page_count(), 1);

        assert!(iter.next().is_none());
    }
}
