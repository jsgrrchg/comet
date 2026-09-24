//! Process-wide admission for chat data traffic. Registry/device control links
//! deliberately remain outside this small budget. Permits cover the resource's
//! entire lifetime, including response bodies and socket teardown.
use crate::SyncError;
use std::sync::{Arc, OnceLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Clone, Copy)]
pub enum Priority {
    Interactive,
    Background,
}

pub struct Budget {
    sockets: Arc<Semaphore>,
    dials: Arc<Semaphore>,
    http: Arc<Semaphore>,
    background_http: Arc<Semaphore>,
    waiters: Arc<Semaphore>,
    limits: [usize; 3],
}

pub struct Permit {
    _resource: OwnedSemaphorePermit,
    _background: Option<OwnedSemaphorePermit>,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetStats {
    pub sockets: usize,
    pub socket_limit: usize,
    pub dials: usize,
    pub dial_limit: usize,
    pub http: usize,
    pub http_limit: usize,
    pub waiting: usize,
}

impl Budget {
    pub fn new(sockets: usize, dials: usize, http: usize) -> Arc<Self> {
        assert!(sockets > 0 && dials > 0 && http > 0);
        Arc::new(Self {
            sockets: Arc::new(Semaphore::new(sockets)),
            dials: Arc::new(Semaphore::new(dials)),
            http: Arc::new(Semaphore::new(http)),
            background_http: Arc::new(Semaphore::new(http.saturating_sub(2).max(1))),
            waiters: Arc::new(Semaphore::new(128)),
            limits: [sockets, dials, http],
        })
    }

    async fn acquire(
        &self,
        resource: &Arc<Semaphore>,
        background: bool,
    ) -> Result<Permit, SyncError> {
        // Never allow callers to build an unbounded semaphore wait queue.
        let _waiting =
            self.waiters.clone().try_acquire_owned().map_err(|_| {
                SyncError::TemporarilyUnavailable("sync admission queue full".into())
            })?;
        let background = if background {
            Some(
                self.background_http
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| SyncError::Closed)?,
            )
        } else {
            None
        };
        let resource = resource
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| SyncError::Closed)?;
        Ok(Permit {
            _resource: resource,
            _background: background,
        })
    }

    pub async fn socket(&self) -> Result<Permit, SyncError> {
        self.acquire(&self.sockets, false).await
    }
    pub async fn dial(&self) -> Result<Permit, SyncError> {
        self.acquire(&self.dials, false).await
    }
    pub async fn http(&self, priority: Priority) -> Result<Permit, SyncError> {
        self.acquire(&self.http, matches!(priority, Priority::Background))
            .await
    }
    pub fn stats(&self) -> BudgetStats {
        BudgetStats {
            sockets: self.limits[0] - self.sockets.available_permits(),
            socket_limit: self.limits[0],
            dials: self.limits[1] - self.dials.available_permits(),
            dial_limit: self.limits[1],
            http: self.limits[2] - self.http.available_permits(),
            http_limit: self.limits[2],
            waiting: 128 - self.waiters.available_permits(),
        }
    }
}

pub fn shared() -> &'static Arc<Budget> {
    static BUDGET: OnceLock<Arc<Budget>> = OnceLock::new();
    BUDGET.get_or_init(|| Budget::new(24, 4, 8))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancellation_releases_capacity_and_background_leaves_interactive_room() {
        let budget = Budget::new(1, 1, 3);
        let background = budget.http(Priority::Background).await.unwrap();
        let waiting = tokio::spawn({
            let budget = budget.clone();
            async move { budget.http(Priority::Background).await }
        });
        tokio::task::yield_now().await;
        let interactive = budget.http(Priority::Interactive).await.unwrap();
        assert_eq!(budget.stats().http, 2);
        waiting.abort();
        let _ = waiting.await;
        drop((background, interactive));
        assert_eq!(budget.stats().http, 0);
        assert_eq!(budget.stats().waiting, 0);
        let socket = budget.socket().await.unwrap();
        let waiting = tokio::spawn({
            let budget = budget.clone();
            async move { budget.socket().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        drop(socket);
        drop(waiting.await.unwrap().unwrap());
        assert_eq!(budget.stats().sockets, 0);
    }
}
