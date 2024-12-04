use bytes::Bytes;
use futures::TryFutureExt;
use graft_core::lsn::LSN;
use graft_core::VolumeId;
use graft_proto::{
    common::v1::SegmentInfo,
    pagestore::v1::{
        PageAtOffset, ReadPagesRequest, ReadPagesResponse, WritePagesRequest, WritePagesResponse,
    },
};
use reqwest::Url;

use crate::builder::ClientBuildErr;
use crate::builder::ClientBuilder;
use crate::request::prost_request;
use crate::ClientErr;

pub struct PagestoreClient {
    pub(crate) endpoint: Url,
    pub(crate) http: reqwest::Client,
}

impl TryFrom<ClientBuilder> for PagestoreClient {
    type Error = ClientBuildErr;

    fn try_from(builder: ClientBuilder) -> Result<Self, Self::Error> {
        let endpoint = builder.endpoint.join("pagestore/v1/")?;
        let http = builder.http()?;
        Ok(Self { endpoint, http })
    }
}

impl PagestoreClient {
    pub async fn read_pages(
        &self,
        vid: &VolumeId,
        lsn: LSN,
        offsets: Bytes,
    ) -> Result<Vec<PageAtOffset>, ClientErr> {
        let url = self.endpoint.join("read_pages").unwrap();
        let req = ReadPagesRequest {
            vid: vid.copy_to_bytes(),
            lsn: lsn.into(),
            offsets,
        };
        prost_request::<_, ReadPagesResponse>(&self.http, url, req)
            .map_ok(|r| r.pages)
            .await
    }

    pub async fn write_pages(
        &self,
        vid: &VolumeId,
        pages: Vec<PageAtOffset>,
    ) -> Result<Vec<SegmentInfo>, ClientErr> {
        let url = self.endpoint.join("write_pages").unwrap();
        let req = WritePagesRequest { vid: vid.copy_to_bytes(), pages };
        prost_request::<_, WritePagesResponse>(&self.http, url, req)
            .map_ok(|r| r.segments)
            .await
    }
}
