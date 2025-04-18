use std::{collections::HashMap, iter::once, sync::Arc};

use culprit::{Result, ResultExt};

use graft_core::{
    PageIdx, VolumeId,
    lsn::LSN,
    page::{EMPTY_PAGE, Page},
};
use splinter_rs::Splinter;

use crate::{ClientErr, ClientPair, oracle::Oracle};

use super::{
    storage::{Storage, page::PageValue, snapshot::Snapshot},
    volume_writer::VolumeWriter,
};

pub trait VolumeRead {
    fn vid(&self) -> &VolumeId;

    /// Retrieve the Volume snapshot backing this reader
    fn snapshot(&self) -> Option<&Snapshot>;

    /// Read a page from the snapshot
    fn read<O: Oracle>(&self, oracle: &mut O, pageidx: PageIdx) -> Result<Page, ClientErr>;
}

#[derive(Debug, Clone)]
pub struct VolumeReader {
    vid: VolumeId,
    snapshot: Option<Snapshot>,
    clients: Arc<ClientPair>,
    storage: Arc<Storage>,
}

impl VolumeReader {
    pub(crate) fn new(
        vid: VolumeId,
        snapshot: Option<Snapshot>,
        clients: Arc<ClientPair>,
        storage: Arc<Storage>,
    ) -> Self {
        Self { vid, snapshot, clients, storage }
    }

    /// Upgrade this reader into a writer
    pub fn upgrade(self) -> VolumeWriter {
        self.into()
    }

    /// decompose this reader into snapshot and storage
    pub(crate) fn into_parts(self) -> (VolumeId, Option<Snapshot>, Arc<ClientPair>, Arc<Storage>) {
        (self.vid, self.snapshot, self.clients, self.storage)
    }

    /// Read a page from the local page cache
    pub fn read_cached(&self, pageidx: PageIdx) -> Result<(Option<LSN>, PageValue), ClientErr> {
        if let Some(snapshot) = self.snapshot() {
            self.storage
                .read(self.vid(), snapshot.local(), pageidx)
                .map(|(lsn, v)| (Some(lsn), v))
                .or_into_ctx()
        } else {
            Ok((None, PageValue::Empty))
        }
    }
}

impl VolumeRead for VolumeReader {
    #[inline]
    fn vid(&self) -> &VolumeId {
        &self.vid
    }

    #[inline]
    fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    fn read<O: Oracle>(&self, oracle: &mut O, pageidx: PageIdx) -> Result<Page, ClientErr> {
        if let Some(snapshot) = self.snapshot() {
            match self
                .storage
                .read(self.vid(), snapshot.local(), pageidx)
                .or_into_ctx()?
            {
                (_, PageValue::Available(page)) => {
                    oracle.observe_cache_hit(pageidx);
                    Ok(page)
                }
                (_, PageValue::Empty) => {
                    oracle.observe_cache_hit(pageidx);
                    Ok(EMPTY_PAGE)
                }
                (_, PageValue::Pending) => {
                    if let Some((remote_lsn, local_lsn)) = snapshot.remote_mapping().splat() {
                        fetch_page(
                            &self.clients,
                            &self.storage,
                            oracle,
                            self.vid(),
                            remote_lsn,
                            local_lsn,
                            pageidx,
                        )
                        .or_into_ctx()
                    } else {
                        Ok(EMPTY_PAGE)
                    }
                }
            }
        } else {
            Ok(EMPTY_PAGE)
        }
    }
}

fn fetch_page<O: Oracle>(
    clients: &ClientPair,
    storage: &Storage,
    oracle: &mut O,
    vid: &VolumeId,
    remote_lsn: LSN,
    local_lsn: LSN,
    pageidx: PageIdx,
) -> Result<Page, ClientErr> {
    let _span = tracing::trace_span!(
        "fetching page from pagestore",
        ?vid,
        %remote_lsn,
        %local_lsn,
        %pageidx,
    )
    .entered();

    // predict future page fetches using the oracle, then eliminate pages we
    // have already fetched while building our update hashmap.
    let mut graft = Splinter::default();
    let mut pages = HashMap::new();
    for idx in once(pageidx).chain(oracle.predict_next(pageidx)) {
        let (lsn, page) = storage.read(vid, local_lsn, idx).or_into_ctx()?;
        if matches!(page, PageValue::Pending) {
            graft.insert(idx.to_u32());
            pages.insert(idx, (lsn, PageValue::Empty));
        }
    }

    // process client results and update the hashmap
    let response = clients
        .pagestore()
        .read_pages(vid, remote_lsn, graft.serialize_to_bytes())?;
    for page in response {
        if let Some(entry) = pages.get_mut(&page.pageidx().or_into_ctx()?) {
            entry.1 = page.page().or_into_ctx()?.into();
        } else {
            tracing::warn!(?vid, %remote_lsn, pageidx=page.pageidx, "unexpected page");
            precept::expect_unreachable!(
                "received unexpected page from pagestore",
                {
                    "vid": vid,
                    "remote_lsn": remote_lsn,
                    "pageidx": page.pageidx,
                }
            );
        }
    }

    let requested_page = pages
        .get(&pageidx)
        .cloned()
        .and_then(|(_, p)| p.try_into_page())
        .expect("requested page not found");

    // update local storage with fetched pages
    storage.receive_pages(vid, pages).or_into_ctx()?;

    // return the requested page
    Ok(requested_page)
}
