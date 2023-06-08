use eyre::Result;
use serde::{Deserialize, Serialize};

use crate::record::store::Store;
use crate::settings::Settings;

const KV_VERSION: &str = "v0";
const KV_TAG: &str = "kv";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KvRecord {
    pub key: String,
    pub value: String,
}

impl KvRecord {
    pub fn serialize(&self) -> Result<Vec<u8>> {
        let buf = rmp_serde::to_vec(self)?;

        Ok(buf)
    }
}

pub struct KvStore;

impl KvStore {
    // will want to init the actual kv store when that is done
    pub fn new() -> KvStore {
        KvStore {}
    }

    pub async fn set(&self, store: &mut impl Store, key: &str, value: &str) -> Result<()> {
        let host_id = Settings::host_id().expect("failed to get host_id");

        let record = KvRecord {
            key: key.to_string(),
            value: value.to_string(),
        };

        let bytes = record.serialize()?;

        let len = store.len(host_id.as_str(), KV_TAG).await?;

        let parent = if len > 0 {
            Some(store.last(host_id.as_str(), KV_TAG).await?.id)
        } else {
            None
        };

        let record = atuin_common::record::Record::new(
            host_id,
            KV_VERSION.to_string(),
            KV_TAG.to_string(),
            parent,
            bytes,
        );

        store.push(record).await?;

        Ok(())
    }

    // TODO: setup an actual kv store, rebuild func, and do not pass the main store in here as
    // well.
    pub async fn get(&self, store: &impl Store, key: &str) -> Result<Option<KvRecord>> {
        // TODO: don't load this from disk so much
        let host_id = Settings::host_id().expect("failed to get host_id");

        // Currently, this is O(n). When we have an actual KV store, it can be better
        // Just a poc for now!

        // iterate records to find the value we want
        // start at the end, so we get the most recent version
        let mut record = store.last(host_id.as_str(), KV_TAG).await?;
        let kv: KvRecord = rmp_serde::from_slice(&record.data)?;

        if kv.key == key {
            return Ok(Some(kv));
        }

        while let Some(parent) = record.parent {
            record = store.get(parent.as_str()).await?;
            let kv: KvRecord = rmp_serde::from_slice(&record.data)?;

            if kv.key == key {
                return Ok(Some(kv));
            }
        }

        // if we get here, then... we didn't find the record with that key :(
        return Ok(None);
    }
}
