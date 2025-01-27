use std::{
    collections::HashSet,
    fmt::Debug,
    io,
    ops::{RangeBounds, RangeInclusive},
    path::Path,
    sync::Arc,
};

use bytes::Bytes;
use changeset::ChangeSet;
use commit::CommitKey;
use culprit::{Culprit, ResultExt};
use fjall::{KvSeparationOptions, PartitionCreateOptions, Slice};
use graft_core::{
    byte_unit::ByteUnit,
    lsn::{LSNRangeExt, LSN},
    page_count::PageCount,
    page_offset::PageOffset,
    zerocopy_err::ZerocopyErr,
    VolumeId,
};
use graft_proto::{common::v1::GraftErrCode, pagestore::v1::PageAtOffset};
use memtable::Memtable;
use page::{PageKey, PageValue, PageValueConversionErr};
use parking_lot::{Mutex, MutexGuard};
use snapshot::Snapshot;
use splinter::{DecodeErr, Splinter, SplinterRef};
use tryiter::{TryIterator, TryIteratorExt};
use volume_state::{
    SyncDirection, VolumeConfig, VolumeQueryIter, VolumeState, VolumeStateKey, VolumeStateTag,
    VolumeStatus, Watermarks,
};
use zerocopy::IntoBytes;

use crate::ClientErr;

pub mod changeset;
pub(crate) mod commit;
pub(crate) mod memtable;
pub(crate) mod page;
pub mod snapshot;
pub mod volume_state;

type Result<T> = std::result::Result<T, Culprit<StorageErr>>;

#[derive(Debug, thiserror::Error)]
pub enum StorageErr {
    #[error("fjall error: {0}")]
    FjallErr(#[from] fjall::Error),

    #[error("io error: {0}")]
    IoErr(io::ErrorKind),

    #[error("Corrupt key: {0}")]
    CorruptKey(ZerocopyErr),

    #[error("Corrupt snapshot: {0}")]
    CorruptSnapshot(ZerocopyErr),

    #[error("Corrupt volume config: {0}")]
    CorruptVolumeConfig(ZerocopyErr),

    #[error("Volume state {0:?} is corrupt: {1}")]
    CorruptVolumeState(VolumeStateTag, ZerocopyErr),

    #[error("Corrupt page: {0}")]
    CorruptPage(#[from] PageValueConversionErr),

    #[error("Corrupt commit: {0}")]
    CorruptCommit(#[from] DecodeErr),

    #[error("Illegal concurrent write to volume")]
    ConcurrentWrite,

    #[error("Volume needs recovery")]
    VolumeNeedsRecovery,

    #[error(
        "The local Volume state is ahead of the remote state, refusing to accept remote changes"
    )]
    RemoteConflict,
}

impl From<io::Error> for StorageErr {
    fn from(err: io::Error) -> Self {
        StorageErr::IoErr(err.kind())
    }
}

impl From<lsm_tree::Error> for StorageErr {
    fn from(err: lsm_tree::Error) -> Self {
        StorageErr::FjallErr(err.into())
    }
}

pub struct Storage {
    keyspace: fjall::Keyspace,

    /// Used to store volume state broken out by tag.
    /// Keyed by VolumeStateKey.
    ///
    /// {vid}/VolumeStateTag::Config -> VolumeConfig
    /// {vid}/VolumeStateTag::Snapshot -> Snapshot
    /// {vid}/VolumeStateTag::Watermarks -> Watermarks
    volumes: fjall::Partition,

    /// Used to store page contents
    /// maps from (VolumeId, Offset, LSN) to PageValue
    pages: fjall::Partition,

    /// Used to track changes made by local commits.
    /// maps from (VolumeId, LSN) to Splinter of written offsets
    commits: fjall::Partition,

    /// Must be held while performing read+write transactions.
    /// Read-only and write-only transactions don't need to hold the lock as
    /// long as they are safe:
    /// To make read-only txns safe, always use fjall snapshots
    /// To make write-only txns safe, they must be monotonic
    commit_lock: Arc<Mutex<()>>,

    /// Used to notify subscribers of new local commits
    local_changeset: ChangeSet<VolumeId>,

    /// Used to notify subscribers of new remote commits
    remote_changeset: ChangeSet<VolumeId>,
}

impl Storage {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_config(fjall::Config::new(path))
    }

    pub fn open_temporary() -> Result<Self> {
        Self::open_config(fjall::Config::new(tempfile::tempdir()?.into_path()).temporary(true))
    }

    pub fn open_config(config: fjall::Config) -> Result<Self> {
        let keyspace = config.open()?;
        let volumes = keyspace.open_partition("volumes", Default::default())?;
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
            pages,
            commits,
            commit_lock: Default::default(),
            local_changeset: Default::default(),
            remote_changeset: Default::default(),
        })
    }

    /// Access the local commit changeset. This ChangeSet is updated whenever a
    /// Volume receives a local commit.
    pub fn local_changeset(&self) -> &ChangeSet<VolumeId> {
        &self.local_changeset
    }

    /// Access the remote commit changeset. This ChangeSet is updated whenever a
    /// Volume receives a remote commit.
    pub fn remote_changeset(&self) -> &ChangeSet<VolumeId> {
        &self.remote_changeset
    }

    /// Add a new volume to the storage. This function will overwrite any
    /// existing configuration for the volume.
    pub fn set_volume_config(&self, vid: &VolumeId, config: VolumeConfig) -> Result<()> {
        let key = VolumeStateKey::new(vid.clone(), VolumeStateTag::Config);
        Ok(self.volumes.insert(key, config)?)
    }

    fn set_volume_status(&self, vid: &VolumeId, status: VolumeStatus) -> Result<()> {
        let key = VolumeStateKey::new(vid.clone(), VolumeStateTag::Status);
        Ok(self.volumes.insert(key, status)?)
    }

    pub fn get_volume_status(&self, vid: &VolumeId) -> Result<VolumeStatus> {
        let key = VolumeStateKey::new(vid.clone(), VolumeStateTag::Status);
        if let Some(value) = self.volumes.get(key)? {
            Ok(VolumeStatus::from_bytes(&value)?)
        } else {
            Ok(VolumeStatus::Ok)
        }
    }

    pub fn volume_state(&self, vid: &VolumeId) -> Result<VolumeState> {
        let mut state = VolumeState::new(vid.clone());
        let mut iter = self.volumes.snapshot().prefix(vid);
        while let Some((key, value)) = iter.try_next()? {
            let key = VolumeStateKey::ref_from_bytes(&key)?;
            debug_assert_eq!(key.vid(), vid, "vid mismatch");
            state.accumulate(key.tag(), value)?;
        }
        Ok(state)
    }

    pub fn snapshot(&self, vid: &VolumeId) -> Result<Option<Snapshot>> {
        let key = VolumeStateKey::new(vid.clone(), VolumeStateTag::Snapshot);
        if let Some(snapshot) = self.volumes.get(key)? {
            Ok(Some(Snapshot::from_bytes(&snapshot)?))
        } else {
            Ok(None)
        }
    }

    pub fn query_volumes(
        &self,
        sync: SyncDirection,
        vids: Option<HashSet<VolumeId>>,
    ) -> impl TryIterator<Ok = VolumeState, Err = Culprit<StorageErr>> {
        let iter = self.volumes.snapshot().iter().err_into();
        let iter = VolumeQueryIter::new(iter);
        iter.try_filter(move |state| {
            let matches_vid = vids.as_ref().map_or(true, |s| s.contains(state.vid()));
            let matches_dir = state.config().sync().matches(sync);
            Ok(matches_vid && matches_dir)
        })
    }

    /// Returns an iterator of PageValue's at an exact LSN for a volume.
    /// Notably, this function will not return a page at an earlier LSN that is
    /// shadowed by this LSN.
    pub fn query_pages<'a, T>(
        &'a self,
        vid: &'a VolumeId,
        lsn: LSN,
        offsets: &'a SplinterRef<T>,
    ) -> impl TryIterator<Ok = (PageOffset, Option<PageValue>), Err = Culprit<StorageErr>> + 'a
    where
        T: AsRef<[u8]> + 'a,
    {
        offsets
            .iter()
            .map(move |offset| {
                let offset: PageOffset = offset.into();
                let key = PageKey::new(vid.clone(), offset, lsn);
                Ok((offset, self.pages.get(key)?))
            })
            .map_ok(|(offset, page)| {
                if let Some(page) = page {
                    Ok((offset, Some(PageValue::try_from(page).or_into_ctx()?)))
                } else {
                    Ok((offset, None))
                }
            })
    }

    /// Returns the most recent visible page in a volume by LSN at a particular
    /// offset. Notably, this will return a page from an earlier LSN if the page
    /// hasn't changed since then.
    pub fn read(&self, vid: &VolumeId, lsn: LSN, offset: PageOffset) -> Result<(LSN, PageValue)> {
        let first_key = PageKey::new(vid.clone(), offset, LSN::FIRST);
        let key = PageKey::new(vid.clone(), offset, lsn);
        let range = first_key..=key;

        // Search for the latest page between LSN(0) and the requested LSN,
        // returning PageValue::Pending if none found.
        if let Some((key, page)) = self.pages.snapshot().range(range).next_back().transpose()? {
            let lsn = PageKey::ref_from_bytes(&key)?.lsn();
            let bytes: Bytes = page.into();
            Ok((lsn, PageValue::try_from(bytes).or_into_ctx()?))
        } else {
            Ok((lsn, PageValue::Pending))
        }
    }

    pub fn commit(
        &self,
        vid: &VolumeId,
        snapshot: Option<Snapshot>,
        memtable: Memtable,
    ) -> Result<Snapshot> {
        let mut batch = self.keyspace.batch();
        let mut pages = snapshot.as_ref().map_or(PageCount::ZERO, |s| s.pages());
        let read_lsn = snapshot.as_ref().map(|s| s.local());
        let remote_lsn = snapshot.and_then(|s| s.remote());
        let commit_lsn = read_lsn
            .map(|lsn| lsn.next().expect("lsn overflow"))
            .unwrap_or(LSN::FIRST);

        // this Splinter will contain all of the offsets this commit changed
        let mut offsets = Splinter::default();

        // persist the memtable
        let mut page_key = PageKey::new(vid.clone(), PageOffset::ZERO, commit_lsn);
        for (offset, page) in memtable {
            page_key = page_key.with_offset(offset);
            pages = pages.max(offset.pages());
            offsets.insert(offset.into());
            batch.insert(&self.pages, page_key.as_bytes(), PageValue::from(page));
        }

        // persist the new volume snapshot
        let snapshot_key = VolumeStateKey::new(vid.clone(), VolumeStateTag::Snapshot);
        let snapshot = Snapshot::new(commit_lsn, remote_lsn, pages);
        batch.insert(&self.volumes, snapshot_key, snapshot.as_bytes());

        // persist the new commit
        let commit_key = CommitKey::new(vid.clone(), commit_lsn);
        batch.insert(&self.commits, commit_key, offsets.serialize_to_bytes());

        // acquire the commit lock
        let _permit = self.commit_lock.lock();

        // check to see if the read snapshot is the latest local snapshot while
        // holding the commit lock
        let latest = self.snapshot(vid)?;
        if latest.map(|l| l.local()) != read_lsn {
            return Err(Culprit::new_with_note(
                StorageErr::ConcurrentWrite,
                format!("Illegal concurrent write to Volume {vid}"),
            ));
        }

        // commit the changes
        batch.commit()?;

        // notify listeners of the new local commit
        self.local_changeset.mark_changed(&vid);

        // return the new snapshot
        Ok(snapshot)
    }

    /// Replicate a remote commit to local storage.
    pub fn receive_remote_commit(
        &self,
        vid: &VolumeId,
        remote_snapshot: graft_proto::Snapshot,
        changed: SplinterRef<Bytes>,
    ) -> Result<()> {
        self.receive_remote_commit_holding_lock(
            self.commit_lock.lock(),
            vid,
            remote_snapshot,
            changed,
        )
    }

    /// Receive a remote commit into storage; it's only safe to call this
    /// function while holding the commit lock
    fn receive_remote_commit_holding_lock(
        &self,
        _permit: MutexGuard<'_, ()>,
        vid: &VolumeId,
        remote_snapshot: graft_proto::Snapshot,
        changed: SplinterRef<Bytes>,
    ) -> Result<()> {
        // resolve the remote lsn and page count
        let remote_lsn = remote_snapshot.lsn().expect("invalid remote LSN");
        let remote_pages = remote_snapshot.pages();

        log::trace!(
            "volume {:?} received remote commit at LSN {} with {} pages",
            vid,
            remote_lsn,
            remote_pages
        );

        let mut batch = self.keyspace.batch();

        // retrieve the current volume state
        let state = self.volume_state(vid)?;
        let snapshot = state.snapshot();
        let watermarks = state.watermarks();

        // ensure that we can accept this remote commit
        if state.needs_recovery() {
            precept::expect_reachable!(
                "volume needs recovery",
                { "vid": vid, "state": state }
            );

            return Err(Culprit::new_with_note(
                StorageErr::VolumeNeedsRecovery,
                format!("Volume {vid} needs recovery"),
            ));
        }
        if state.has_pending_commits() {
            precept::expect_reachable!(
                "volume has pending commits while receiving remote commit",
                { "vid": vid, "state": state }
            );

            // mark the volume as having a remote conflict
            self.set_volume_status(vid, VolumeStatus::Conflict)?;

            return Err(Culprit::new_with_note(
                StorageErr::RemoteConflict,
                format!("Volume {vid:?} has pending commits, refusing to accept remote changes"),
            ));
        }

        // compute the next local lsn
        let local_lsn = snapshot.map_or(LSN::FIRST, |s| s.local().next().expect("lsn overflow"));

        // persist the new volume snapshot
        batch.insert(
            &self.volumes,
            VolumeStateKey::new(vid.clone(), VolumeStateTag::Snapshot),
            Snapshot::new(local_lsn, Some(remote_lsn), remote_pages),
        );

        // fast forward the sync watermarks to ensure we don't roundtrip this
        // commit back to the server
        batch.insert(
            &self.volumes,
            VolumeStateKey::new(vid.clone(), VolumeStateTag::Watermarks),
            watermarks
                .clone()
                .with_last_sync(local_lsn)
                .with_pending_sync(local_lsn),
        );

        // mark changed pages
        let mut key = PageKey::new(vid.clone(), PageOffset::ZERO, local_lsn);
        let pending = Bytes::from(PageValue::Pending);
        for offset in changed.iter() {
            key = key.with_offset(offset.into());
            batch.insert(&self.pages, key.as_ref(), pending.as_ref());
        }

        batch.commit()?;

        // notify listeners of the new remote commit
        self.remote_changeset.mark_changed(&vid);

        Ok(())
    }

    /// Write a set of pages to storage at a particular vid/lsn
    pub fn receive_pages(&self, vid: &VolumeId, lsn: LSN, pages: Vec<PageAtOffset>) -> Result<()> {
        let mut key = PageKey::new(vid.clone(), PageOffset::ZERO, lsn);
        let mut batch = self.keyspace.batch();
        for page in pages {
            key = key.with_offset(page.offset());
            batch.insert(
                &self.pages,
                key.as_ref(),
                PageValue::try_from(page.data).or_into_ctx()?,
            );
        }
        Ok(batch.commit()?)
    }

    /// Prepare to sync a volume to the remote.
    /// Returns:
    /// - the volume snapshot
    /// - the range of LSNs to sync
    /// - an iterator of commits to sync
    pub fn prepare_sync_to_remote(
        &self,
        vid: &VolumeId,
    ) -> Result<(
        Snapshot,
        RangeInclusive<LSN>,
        impl TryIterator<Ok = (LSN, SplinterRef<Slice>), Err = Culprit<StorageErr>>,
    )> {
        // acquire the commit lock
        let _permit = self.commit_lock.lock();

        // retrieve the current volume state
        let state = self.volume_state(vid)?;

        // fail if the volume needs recovery
        if state.needs_recovery() {
            precept::expect_reachable!(
                "volume needs recovery",
                { "vid": vid, "state": state }
            );
            return Err(Culprit::new_with_note(
                StorageErr::VolumeNeedsRecovery,
                format!("Volume {vid} needs recovery"),
            ));
        }

        // ensure that we only run this job when we actually have commits to sync
        precept::expect_always_or_unreachable!(
            state.has_pending_commits(),
            "the sync push job only runs when we have local commits to push",
            { "vid": vid, "state": state }
        );
        debug_assert!(
            state.has_pending_commits(),
            "the sync push job only runs when we have local commits to push"
        );

        // resolve the snapshot; we can expect it to be available because this
        // function should only run when we have local commits to sync
        let snapshot = state.snapshot().expect("volume snapshot missing").clone();
        let local_lsn = snapshot.local();

        // update pending_sync to the local LSN
        self.volumes.insert(
            VolumeStateKey::new(vid.clone(), VolumeStateTag::Watermarks),
            state.watermarks().clone().with_pending_sync(local_lsn),
        )?;

        // calculate the LSN range of commits to sync
        let start = state
            .watermarks()
            .last_sync()
            .map_or(LSN::FIRST, |s| s.next().expect("LSN overflow"));
        let end = local_lsn;
        let lsns = start..=end;

        // create a commit iterator
        let commit_start = CommitKey::new(vid.clone(), *lsns.start());
        let commit_end = CommitKey::new(vid.clone(), *lsns.end());
        let mut cursor = commit_start.lsn();
        let commits = self
            .commits
            .snapshot()
            .range(commit_start..=commit_end)
            .err_into()
            .map_ok(move |(k, v)| {
                let lsn = CommitKey::ref_from_bytes(&k)?.lsn();

                // detect missing commits
                assert_eq!(lsn, cursor, "missing commit detected");
                cursor = cursor.next().expect("lsn overflow");

                let splinter = SplinterRef::from_bytes(v).or_into_ctx()?;
                Ok((lsn, splinter))
            });

        Ok((snapshot, lsns, commits))
    }

    /// Rollback a failed push operation by setting Watermarks::pending_sync to
    /// Watermarks::last_sync
    pub fn rollback_sync_to_remote(&self, vid: &VolumeId, err: &ClientErr) -> Result<()> {
        // acquire the commit lock
        let _permit = self.commit_lock.lock();

        // rollback the pending_sync watermark
        let key = VolumeStateKey::new(vid.clone(), VolumeStateTag::Watermarks);
        let watermarks = match self.volumes.get(&key)? {
            Some(watermarks) => Watermarks::from_bytes(&watermarks)?,
            None => Watermarks::default(),
        };
        self.volumes
            .insert(key, watermarks.rollback_pending_sync())?;

        // set the volume status based on the error
        if let ClientErr::GraftErr(err) = err {
            if err.code() == GraftErrCode::CommitRejected {
                self.set_volume_status(vid, VolumeStatus::RejectedCommit)?;
            }
        }

        Ok(())
    }

    /// Complete a push operation by updating the volume snapshot, updating
    /// Watermarks::last_sync, and removing all synced commits.
    pub fn complete_sync_to_remote(
        &self,
        vid: &VolumeId,
        sync_start_snapshot: Snapshot,
        remote_snapshot: graft_proto::Snapshot,
        synced_lsns: impl RangeBounds<LSN>,
    ) -> Result<()> {
        // acquire the commit lock and start a new batch
        let _permit = self.commit_lock.lock();
        let mut batch = self.keyspace.batch();

        let state = self.volume_state(vid)?;

        // resolve the snapshot; we can expect it to be available because this
        // function should only run after we have synced a local commit
        let snapshot = state.snapshot().expect("volume snapshot missing");

        let local_lsn = snapshot.local();
        let pages = snapshot.pages();
        let remote_lsn = remote_snapshot.lsn().expect("invalid remote LSN");

        log::trace!(
            "completing sync to remote: pushed local LSN {} to remote LSN {} for volume {:?}",
            sync_start_snapshot.local(),
            remote_lsn,
            state,
        );

        // check invariants
        assert!(
            snapshot.remote() < Some(remote_lsn),
            "remote LSN should be monotonically increasing"
        );
        assert_eq!(
            state.watermarks().pending_sync(),
            Some(sync_start_snapshot.local()),
            "the pending_sync watermark must be equal to the local LSN at the start of the sync"
        );

        // persist the updated remote LSN to the snapshot
        batch.insert(
            &self.volumes,
            VolumeStateKey::new(vid.clone(), VolumeStateTag::Snapshot),
            Snapshot::new(local_lsn, Some(remote_lsn), pages),
        );

        // commit the pending sync
        batch.insert(
            &self.volumes,
            VolumeStateKey::new(vid.clone(), VolumeStateTag::Watermarks),
            state.watermarks().clone().commit_pending_sync(),
        );

        // remove all commits in the synced range
        let mut key = CommitKey::new(vid.clone(), LSN::FIRST);
        for lsn in synced_lsns.iter() {
            key = key.with_lsn(lsn);
            batch.remove(&self.commits, key.as_ref());
        }

        Ok(batch.commit()?)
    }

    /// Reset the volume to the provided remote snapshot.
    /// This will cause all pending commits to be rolled back and the volume
    /// status to be cleared.
    pub fn reset_volume_to_remote(
        &self,
        vid: &VolumeId,
        remote_snapshot: graft_proto::Snapshot,
        changed: SplinterRef<Bytes>,
    ) -> Result<()> {
        // acquire the commit lock and start a new batch
        let permit = self.commit_lock.lock();

        // retrieve the current volume state
        let state = self.volume_state(&vid)?;
        let snapshot = state.snapshot();
        let local_lsn = snapshot.map(|s| s.local());
        let target_lsn = state.watermarks().last_sync();

        if target_lsn == local_lsn {
            // no need to reset, we can just receive the remote commit
            return self.receive_remote_commit_holding_lock(permit, vid, remote_snapshot, changed);
        }

        // invariants
        assert!(
            target_lsn < local_lsn,
            "refusing to reset to a LSN larger than the current LSN; local={:?}, target={:?}",
            local_lsn,
            target_lsn
        );

        log::trace!(
            "resetting volume {:?} from {:?} to {:?}",
            vid,
            local_lsn,
            target_lsn,
        );

        let mut batch = self.keyspace.batch();

        // reset the snapshot
        if let Some(target_lsn) = target_lsn {
            batch.insert(
                &self.volumes,
                VolumeStateKey::new(vid.clone(), VolumeStateTag::Snapshot),
                Snapshot::new(
                    target_lsn,
                    Some(remote_snapshot.lsn().expect("invalid LSN")),
                    remote_snapshot.pages(),
                ),
            );
        } else {
            batch.remove(
                &self.volumes,
                VolumeStateKey::new(vid.clone(), VolumeStateTag::Snapshot),
            );
        }

        // clear the status
        batch.remove(
            &self.volumes,
            VolumeStateKey::new(vid.clone(), VolumeStateTag::Status),
        );

        // rollback the pending_sync watermark
        batch.insert(
            &self.volumes,
            VolumeStateKey::new(vid.clone(), VolumeStateTag::Watermarks),
            state.watermarks().clone().rollback_pending_sync(),
        );

        // remove all pending commits
        let mut commits = self.commits.snapshot().prefix(vid);
        while let Some((key, value)) = commits.try_next().or_into_ctx()? {
            let key = CommitKey::ref_from_bytes(&key)?;
            assert_eq!(
                key.vid(),
                vid,
                "refusing to remove commit from another volume"
            );
            assert!(
                Some(key.lsn()) > target_lsn,
                "invariant violation: no commits should exist at or below target_lsn"
            );
            batch.remove(&self.commits, key.as_ref());

            // remove the commit's offsets
            let splinter = SplinterRef::from_bytes(value).or_into_ctx()?;

            let mut key = PageKey::new(vid.clone(), 0.into(), key.lsn());
            for offset in splinter.iter() {
                key = key.with_offset(offset.into());
                batch.remove(&self.pages, key.as_ref());
            }
        }

        // now that we have reset to the earlier volume state, we can receive
        // the remote commit
        return self.receive_remote_commit_holding_lock(permit, vid, remote_snapshot, changed);
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
            .set_volume_config(&vids[0], VolumeConfig::new(SyncDirection::Pull))
            .unwrap();
        let snapshot = storage.commit(&vids[0], None, memtable.clone()).unwrap();
        storage
            .commit(&vids[0], Some(snapshot), memtable.clone())
            .unwrap();

        // second volume has one commit, and is configured to push
        storage
            .set_volume_config(&vids[1], VolumeConfig::new(SyncDirection::Push))
            .unwrap();
        storage.commit(&vids[1], None, memtable.clone()).unwrap();

        // ensure that we can query back out the snapshots
        let sync = SyncDirection::Both;
        let mut iter = storage.query_volumes(sync, None);

        // check the first volume
        let state = iter.try_next().unwrap().unwrap();
        assert_eq!(state.vid(), &vids[0]);
        assert_eq!(state.config().sync(), SyncDirection::Pull);
        let snapshot = state.snapshot().unwrap();
        assert_eq!(snapshot.local(), LSN::new(2));
        assert_eq!(snapshot.pages(), 1);

        // check the second volume
        let state = iter.try_next().unwrap().unwrap();
        assert_eq!(state.vid(), &vids[1]);
        assert_eq!(state.config().sync(), SyncDirection::Push);
        let snapshot = state.snapshot().unwrap();
        assert_eq!(snapshot.local(), LSN::new(1));
        assert_eq!(snapshot.pages(), 1);

        // iter is empty
        assert!(iter.next().is_none());

        // verify that the sync direction filter works
        let sync = SyncDirection::Push;
        let mut iter = storage.query_volumes(sync, None);

        // should be the second volume
        let state = iter.try_next().unwrap().unwrap();
        assert_eq!(state.vid(), &vids[1]);
        assert_eq!(state.config().sync(), SyncDirection::Push);
        let snapshot = state.snapshot().unwrap();
        assert_eq!(snapshot.local(), LSN::new(1));
        assert_eq!(snapshot.pages(), 1);

        // iter is empty
        assert!(iter.next().is_none());

        // verify that the volume id filter works
        let sync = SyncDirection::Both;
        let vid_set = HashSet::from_iter([vids[0].clone()]);
        let mut iter = storage.query_volumes(sync, Some(vid_set));

        // should be the first volume
        let state = iter.try_next().unwrap().unwrap();
        assert_eq!(state.vid(), &vids[0]);

        // iter is empty
        assert!(iter.next().is_none());
    }
}
