//! A resizable byte budget shared by the network reader (which spends it)
//! and the decoder side (which gives it back as it consumes data). It caps
//! how much audio sits in memory between the two.

use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;

/// Network chunks are split into pieces no larger than this before spending
/// budget, so a piece never needs more than the smallest allowed capacity.
pub const MAX_PIECE: usize = 16 * 1024;

#[derive(Debug)]
struct Sizes {
    capacity: usize,
    /// Permits still owed after a shrink that couldn't be applied at once
    /// (they were held by buffered data). Paid off from future releases.
    debt: usize,
}

#[derive(Clone, Debug)]
pub struct ByteBudget {
    sem: Arc<Semaphore>,
    sizes: Arc<Mutex<Sizes>>,
}

impl ByteBudget {
    pub fn new(capacity: usize) -> Self {
        Self {
            sem: Arc::new(Semaphore::new(capacity)),
            sizes: Arc::new(Mutex::new(Sizes { capacity, debt: 0 })),
        }
    }

    /// Wait until `n` bytes fit. Returns `false` once the budget is closed
    /// (the consumer went away), which tells the producer to stop.
    pub async fn acquire(&self, n: usize) -> bool {
        match self.sem.acquire_many(n.min(u32::MAX as usize) as u32).await {
            Ok(permit) => {
                permit.forget();
                true
            }
            Err(_) => false,
        }
    }

    /// Give back `n` bytes that left the buffer.
    pub fn release(&self, n: usize) {
        let Ok(mut sizes) = self.sizes.lock() else {
            return;
        };
        let pay = sizes.debt.min(n);
        sizes.debt -= pay;
        self.sem.add_permits(n - pay);
    }

    pub fn capacity(&self) -> usize {
        self.sizes.lock().map(|s| s.capacity).unwrap_or(0)
    }

    /// Bytes currently buffered (spent and not yet released).
    pub fn buffered(&self) -> usize {
        let Ok(sizes) = self.sizes.lock() else {
            return 0;
        };
        (sizes.capacity + sizes.debt).saturating_sub(self.sem.available_permits())
    }

    pub fn resize(&self, capacity: usize) {
        let Ok(mut sizes) = self.sizes.lock() else {
            return;
        };
        if capacity >= sizes.capacity {
            let mut grow = capacity - sizes.capacity;
            let pay = sizes.debt.min(grow);
            sizes.debt -= pay;
            grow -= pay;
            self.sem.add_permits(grow);
        } else {
            let shrink = sizes.capacity - capacity;
            let forgotten = self.sem.forget_permits(shrink);
            sizes.debt += shrink - forgotten;
        }
        sizes.capacity = capacity;
    }

    /// Wake any waiting producer and make all future `acquire`s fail.
    pub fn close(&self) {
        self.sem.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn acquire_blocks_when_full_and_resumes_on_release() {
        let b = ByteBudget::new(100);
        assert!(b.acquire(100).await);
        assert_eq!(b.buffered(), 100);

        let waiting = tokio::time::timeout(Duration::from_millis(50), b.acquire(10)).await;
        assert!(waiting.is_err(), "should wait while full");

        b.release(10);
        assert!(b.acquire(10).await);
    }

    #[tokio::test]
    async fn close_unblocks_producer() {
        let b = ByteBudget::new(10);
        assert!(b.acquire(10).await);
        let b2 = b.clone();
        let producer = tokio::spawn(async move { b2.acquire(5).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        b.close();
        assert!(!producer.await.unwrap(), "closed budget reports false");
    }

    #[tokio::test]
    async fn grow_and_shrink() {
        let b = ByteBudget::new(100);
        b.resize(300);
        assert_eq!(b.capacity(), 300);
        assert!(b.acquire(250).await);
        assert_eq!(b.buffered(), 250);

        // Shrink below what's buffered: excess is owed, not lost.
        b.resize(100);
        assert_eq!(b.buffered(), 250);
        b.release(250);
        assert_eq!(b.buffered(), 0);
        assert!(b.acquire(100).await);
        let over = tokio::time::timeout(Duration::from_millis(30), b.acquire(1)).await;
        assert!(over.is_err(), "capacity is 100 after debt is paid");
    }
}
