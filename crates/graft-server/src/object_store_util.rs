use std::{path::PathBuf, sync::Arc};

use object_store::{
    ObjectStore, aws::S3ConditionalPut, local::LocalFileSystem, memory::InMemory, path::Path,
    prefix::PrefixStore,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ObjectStoreConfig {
    /// In memory object store
    #[default]
    Memory,

    /// On disk object store
    Fs { root: PathBuf },

    /// S3 compatible object store
    /// Can load most config and secrets from environment variables
    /// See `object_store::aws::builder::AmazonS3Builder` for env variable names
    S3Compatible {
        bucket: String,
        prefix: Option<String>,
    },
}

impl ObjectStoreConfig {
    pub fn build(self) -> object_store::Result<Arc<dyn ObjectStore>> {
        match self {
            ObjectStoreConfig::Memory => Ok(Arc::new(InMemory::new())),
            ObjectStoreConfig::Fs { root } => Ok(Arc::new(LocalFileSystem::new_with_prefix(root)?)),
            ObjectStoreConfig::S3Compatible { bucket, prefix } => {
                let store = object_store::aws::AmazonS3Builder::from_env()
                    .with_allow_http(true)
                    .with_bucket_name(bucket)
                    .with_conditional_put(S3ConditionalPut::ETagMatch)
                    .build()?;
                if let Some(prefix) = prefix {
                    let prefix = Path::parse(prefix)?;
                    Ok(Arc::new(PrefixStore::new(store, prefix)))
                } else {
                    Ok(Arc::new(store))
                }
            }
        }
    }
}
