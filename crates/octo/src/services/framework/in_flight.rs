//! One running fetch per key, shared by everyone who asks for that key while it runs: the
//! `ConcurrentDictionary<key, Lazy<Task<T>>>` pattern the C# services used for single-flight.
//!
//! The fetch runs on its own tokio task, so it finishes (and writes whatever cache it writes)
//! even when every caller has stopped waiting, as a C# task ran on without its awaiters. The
//! key is removed when the fetch ends, whatever its outcome.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use parking_lot::Mutex;

type Flights<V> = Arc<Mutex<HashMap<String, Shared<BoxFuture<'static, V>>>>>;

pub struct InFlight<V> {
    flights: Flights<V>,
}

impl<V> Default for InFlight<V> {
    fn default() -> Self {
        Self {
            flights: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

/// Removes the key when the fetch is done with, normally or by panic.
struct RemoveOnDrop<V> {
    flights: Flights<V>,
    key: String,
}

impl<V> Drop for RemoveOnDrop<V> {
    fn drop(&mut self) {
        self.flights.lock().remove(&self.key);
    }
}

impl<V: Clone + Default + Send + Sync + 'static> InFlight<V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// The running fetch for `key`, or `fetch` started for it. A fetch that panics answers
    /// the default value.
    pub fn run<F>(&self, key: &str, fetch: F) -> Shared<BoxFuture<'static, V>>
    where
        F: Future<Output = V> + Send + 'static,
    {
        let mut flights = self.flights.lock();
        if let Some(running) = flights.get(key) {
            return running.clone();
        }
        let guard = RemoveOnDrop {
            flights: Arc::clone(&self.flights),
            key: key.to_string(),
        };
        // Spawned while the lock is held, so the fetch cannot finish (and remove its key)
        // before the key is there.
        let task = tokio::spawn(async move {
            let _guard = guard;
            fetch.await
        });
        let shared = task.map(Result::unwrap_or_default).boxed().shared();
        flights.insert(key.to_string(), shared.clone());
        shared
    }

    /// Forget every running fetch, as clearing the dictionary did. The fetches run on.
    pub fn clear(&self) {
        self.flights.lock().clear();
    }
}
