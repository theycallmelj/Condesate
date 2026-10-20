//! Shared storage abstraction.
//!
//! In OS terms this is the swarm's "filesystem" / shared memory: every harness
//! gets an `Arc<dyn Storage>` handle to the *same* backend, so they can leave
//! notes for each other, checkpoint transcripts, publish results, etc.
//!
//! Swap `InMemoryStorage` for Redis, sqlite, S3, a vector DB, ... by writing a
//! new impl of this trait. Nothing else changes.

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[async_trait]
pub trait Storage: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<String>>;
    async fn set(&self, key: &str, value: &str) -> Result<()>;
    /// Return all keys beginning with `prefix` (use "" for everything).
    async fn keys(&self, prefix: &str) -> Result<Vec<String>>;
}

/// Trivial concurrent key/value store. Good enough to demonstrate sharing.
#[derive(Default)]
pub struct InMemoryStorage {
    inner: RwLock<HashMap<String, String>>,
}

impl InMemoryStorage {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[async_trait]
impl Storage for InMemoryStorage {
    async fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.inner.read().await.get(key).cloned())
    }

    async fn set(&self, key: &str, value: &str) -> Result<()> {
        self.inner.write().await.insert(key.to_string(), value.to_string());
        Ok(())
    }

    async fn keys(&self, prefix: &str) -> Result<Vec<String>> {
        Ok(self
            .inner
            .read()
            .await
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_get_roundtrip() {
        let s = InMemoryStorage::new();
        assert_eq!(s.get("missing").await.unwrap(), None);
        s.set("a", "1").await.unwrap();
        assert_eq!(s.get("a").await.unwrap().as_deref(), Some("1"));
        // overwrite
        s.set("a", "2").await.unwrap();
        assert_eq!(s.get("a").await.unwrap().as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn keys_filters_by_prefix() {
        let s = InMemoryStorage::new();
        s.set("proj/x", "1").await.unwrap();
        s.set("proj/y", "2").await.unwrap();
        s.set("other", "3").await.unwrap();
        let mut ks = s.keys("proj/").await.unwrap();
        ks.sort();
        assert_eq!(ks, vec!["proj/x".to_string(), "proj/y".to_string()]);
        assert_eq!(s.keys("").await.unwrap().len(), 3);
    }
}
