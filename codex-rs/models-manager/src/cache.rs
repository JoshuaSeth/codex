use chrono::DateTime;
use chrono::Utc;
use codex_protocol::openai_models::ModelInfo;
use codex_utils_path::write_atomically;
use serde::Deserialize;
use serde::Serialize;
use std::io;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::Duration;
use tokio::fs;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;
use tokio::task;
use tracing::error;
use tracing::info;

/// Manages loading and saving of models cache to disk.
#[derive(Debug)]
pub(crate) struct ModelsCacheManager {
    cache_path: PathBuf,
    cache_ttl: Duration,
    write_lock: Semaphore,
}

impl ModelsCacheManager {
    /// Create a new cache manager with the given path and TTL.
    pub(crate) fn new(cache_path: PathBuf, cache_ttl: Duration) -> Self {
        Self {
            cache_path,
            cache_ttl,
            write_lock: Semaphore::new(1),
        }
    }

    /// Attempt to load a fresh cache entry. Returns `None` if the cache doesn't exist or is stale.
    pub(crate) async fn load_fresh(&self, expected_version: &str) -> Option<ModelsCache> {
        info!(
                cache_path = %self.cache_path.display(),
                expected_version,
            "models cache: attempting load_fresh"
        );
        let cache = match self.load().await {
            Ok(cache) => cache?,
            Err(err) => {
                error!("failed to load models cache: {err}");
                return None;
            }
        };
        info!(
            cache_path = %self.cache_path.display(),
            cached_version = ?cache.client_version,
            fetched_at = %cache.fetched_at,
            "models cache: loaded cache file"
        );
        if cache.client_version.as_deref() != Some(expected_version) {
            info!(
                cache_path = %self.cache_path.display(),
                expected_version,
                cached_version = ?cache.client_version,
                "models cache: cache version mismatch"
            );
            return None;
        }
        if !cache.is_fresh(self.cache_ttl) {
            info!(
                cache_path = %self.cache_path.display(),
                cache_ttl_secs = self.cache_ttl.as_secs(),
                fetched_at = %cache.fetched_at,
                "models cache: cache is stale"
            );
            return None;
        }
        info!(
            cache_path = %self.cache_path.display(),
            cache_ttl_secs = self.cache_ttl.as_secs(),
            "models cache: cache hit"
        );
        Some(cache)
    }

    /// Persist the cache to disk, creating parent directories as needed.
    pub(crate) async fn persist_cache(
        &self,
        models: &[ModelInfo],
        etag: Option<String>,
        client_version: String,
    ) {
        let cache = ModelsCache {
            fetched_at: Utc::now(),
            etag,
            client_version: Some(client_version),
            models: models.to_vec(),
        };
        let _write_permit = self.acquire_write_permit().await;
        if let Err(err) = self.save_internal(&cache).await {
            error!("failed to write models cache: {err}");
        }
    }

    /// Renew the cache TTL by updating the fetched_at timestamp to now.
    pub(crate) async fn renew_cache_ttl(&self) -> io::Result<()> {
        let _write_permit = self.acquire_write_permit().await;
        let mut cache = match self.load().await? {
            Some(cache) => cache,
            None => return Err(io::Error::new(ErrorKind::NotFound, "cache not found")),
        };
        cache.fetched_at = Utc::now();
        self.save_internal(&cache).await
    }

    async fn load(&self) -> io::Result<Option<ModelsCache>> {
        match fs::read(&self.cache_path).await {
            Ok(contents) => {
                let cache = serde_json::from_slice(&contents)
                    .map_err(|err| io::Error::new(ErrorKind::InvalidData, err.to_string()))?;
                Ok(Some(cache))
            }
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    async fn save_internal(&self, cache: &ModelsCache) -> io::Result<()> {
        if let Some(parent) = self.cache_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_string_pretty(cache)
            .map_err(|err| io::Error::new(ErrorKind::InvalidData, err.to_string()))?;
        let cache_path = self.cache_path.clone();
        task::spawn_blocking(move || write_atomically(&cache_path, &json))
            .await
            .map_err(io::Error::other)?
    }

    async fn acquire_write_permit(&self) -> SemaphorePermit<'_> {
        self.write_lock
            .acquire()
            .await
            .unwrap_or_else(|_| unreachable!())
    }

    #[cfg(test)]
    /// Set the cache TTL.
    pub(crate) fn set_ttl(&mut self, ttl: Duration) {
        self.cache_ttl = ttl;
    }

    #[cfg(test)]
    /// Manipulate cache file for testing. Allows setting a custom fetched_at timestamp.
    pub(crate) async fn manipulate_cache_for_test<F>(&self, f: F) -> io::Result<()>
    where
        F: FnOnce(&mut DateTime<Utc>),
    {
        let _write_permit = self.acquire_write_permit().await;
        let mut cache = match self.load().await? {
            Some(cache) => cache,
            None => return Err(io::Error::new(ErrorKind::NotFound, "cache not found")),
        };
        f(&mut cache.fetched_at);
        self.save_internal(&cache).await
    }

    #[cfg(test)]
    /// Mutate the full cache contents for testing.
    pub(crate) async fn mutate_cache_for_test<F>(&self, f: F) -> io::Result<()>
    where
        F: FnOnce(&mut ModelsCache),
    {
        let _write_permit = self.acquire_write_permit().await;
        let mut cache = match self.load().await? {
            Some(cache) => cache,
            None => return Err(io::Error::new(ErrorKind::NotFound, "cache not found")),
        };
        f(&mut cache);
        self.save_internal(&cache).await
    }
}

/// Serialized snapshot of models and metadata cached on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ModelsCache {
    pub(crate) fetched_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_version: Option<String>,
    pub(crate) models: Vec<ModelInfo>,
}

impl ModelsCache {
    /// Returns `true` when the cache entry has not exceeded the configured TTL.
    fn is_fresh(&self, ttl: Duration) -> bool {
        if ttl.is_zero() {
            return false;
        }
        let Ok(ttl_duration) = chrono::Duration::from_std(ttl) else {
            return false;
        };
        let age = Utc::now().signed_duration_since(self.fetched_at);
        age <= ttl_duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[tokio::test]
    async fn cache_write_atomically_replaces_existing_file() {
        let temp_dir = tempdir().expect("create temp dir");
        let cache_path = temp_dir.path().join("models_cache.json");
        let manager = ModelsCacheManager::new(cache_path.clone(), Duration::from_secs(60));
        manager
            .persist_cache(
                &[],
                Some("original-etag".to_string()),
                "test-client".to_string(),
            )
            .await;
        let mut original_file = std::fs::File::open(&cache_path).expect("open original cache");

        manager
            .persist_cache(
                &[],
                Some("replacement-etag".to_string()),
                "test-client".to_string(),
            )
            .await;

        let mut original_contents = String::new();
        original_file
            .read_to_string(&mut original_contents)
            .expect("read original cache handle");
        let original_cache: ModelsCache =
            serde_json::from_str(&original_contents).expect("parse original cache handle");
        assert_eq!(original_cache.etag.as_deref(), Some("original-etag"));

        let current_cache = manager
            .load()
            .await
            .expect("load current cache")
            .expect("current cache exists");
        assert_eq!(current_cache.etag.as_deref(), Some("replacement-etag"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_cache_updates_remain_valid_json() {
        let temp_dir = tempdir().expect("create temp dir");
        let cache_path = temp_dir.path().join("models_cache.json");
        let manager = Arc::new(ModelsCacheManager::new(
            cache_path.clone(),
            Duration::from_secs(60),
        ));
        manager
            .persist_cache(
                &[],
                Some("initial-etag".to_string()),
                "test-client".to_string(),
            )
            .await;

        let mut tasks = Vec::new();
        for writer_id in 0..8 {
            let manager = Arc::clone(&manager);
            tasks.push(tokio::spawn(async move {
                for update_id in 0..20 {
                    let etag = format!(
                        "writer-{writer_id}-update-{update_id}-{}",
                        "x".repeat(64_000)
                    );
                    manager
                        .persist_cache(&[], Some(etag), "test-client".to_string())
                        .await;
                    manager.renew_cache_ttl().await.expect("renew cache TTL");
                }
            }));
        }
        for _ in 0..4 {
            let cache_path = cache_path.clone();
            tasks.push(tokio::spawn(async move {
                for _ in 0..400 {
                    let contents = fs::read(&cache_path).await.expect("read cache");
                    serde_json::from_slice::<ModelsCache>(&contents)
                        .expect("cache remains valid JSON");
                    task::yield_now().await;
                }
            }));
        }

        for task in tasks {
            task.await.expect("cache task completes");
        }
    }
}
