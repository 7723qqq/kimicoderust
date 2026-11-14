use std::future::Future;

use futures_util::{StreamExt, stream};

pub const TOWER_FILE_IO_CONCURRENCY: usize = 8;
pub const TOWER_GIT_CONCURRENCY: usize = 4;

pub async fn map_with_concurrency<T, R, F, Fut>(
    items: Vec<T>,
    concurrency: usize,
    mapper: F,
) -> Vec<R>
where
    F: Fn(usize, T) -> Fut,
    Fut: Future<Output = R>,
{
    if items.is_empty() {
        return Vec::new();
    }
    let workers = concurrency.max(1).min(items.len());
    stream::iter(items.into_iter().enumerate())
        .map(|(index, item)| mapper(index, item))
        .buffered(workers)
        .collect()
        .await
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use super::*;

    async fn assert_order_and_limit(concurrency: usize) {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let items = (0..32).collect::<Vec<_>>();
        let results = map_with_concurrency(items, concurrency, |index, _item| {
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            async move {
                let running = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(running, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis((7 - index % 5) as u64)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                index
            }
        })
        .await;

        assert_eq!(results, (0..32).collect::<Vec<_>>());
        assert_eq!(maximum.load(Ordering::SeqCst), concurrency);
    }

    #[tokio::test]
    async fn file_io_is_ordered_and_bounded_to_eight() {
        assert_order_and_limit(TOWER_FILE_IO_CONCURRENCY).await;
    }

    #[tokio::test]
    async fn git_work_is_ordered_and_bounded_to_four() {
        assert_order_and_limit(TOWER_GIT_CONCURRENCY).await;
    }
}
