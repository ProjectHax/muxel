//! A small worker set per batch and one admission limit shared by all batches.

use gpui::BackgroundExecutor;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};

const MAX_PROBES: usize = 4;

struct Permits {
    sender: async_channel::Sender<()>,
    receiver: async_channel::Receiver<()>,
}

fn permits() -> &'static Permits {
    static PERMITS: OnceLock<Permits> = OnceLock::new();
    PERMITS.get_or_init(|| {
        let (sender, receiver) = async_channel::bounded(MAX_PROBES);
        for _ in 0..MAX_PROBES {
            sender
                .try_send(())
                .expect("initial probe admission capacity");
        }
        Permits { sender, receiver }
    })
}

struct Permit(async_channel::Sender<()>);

impl Drop for Permit {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

/// Run every job once, returning results in worker order. At most four probes
/// are admitted across overlapping calls. Dropping this future cancels its
/// workers and returns any held permits. Probe I/O must provide its own bound.
pub(super) async fn collect<J, R, F, Fut>(
    executor: BackgroundExecutor,
    jobs: Vec<J>,
    probe: F,
) -> Vec<R>
where
    J: Send + 'static,
    R: Send + 'static,
    F: Fn(J) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = R> + Send + 'static,
{
    let worker_count = jobs.len().min(MAX_PROBES);
    let jobs = Arc::new(Mutex::new(VecDeque::from(jobs)));
    let probe = Arc::new(probe);
    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let (jobs, probe) = (Arc::clone(&jobs), Arc::clone(&probe));
        workers.push(executor.spawn(async move {
            let mut results = Vec::new();
            loop {
                let job = jobs
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .pop_front();
                let Some(job) = job else {
                    break;
                };
                let permits = permits();
                permits
                    .receiver
                    .recv()
                    .await
                    .expect("probe admission remains open");
                let _permit = Permit(permits.sender.clone());
                results.push(probe(job).await);
            }
            results
        }));
    }
    let mut results = Vec::new();
    for worker in workers {
        results.extend(worker.await);
    }
    results
}

#[cfg(test)]
mod tests {
    use super::collect;
    use gpui::TestAppContext;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn overlapping_batches_probe_concurrently_under_one_global_limit_and_return_each_job_once() {
        // Serialize with app tests that can also dispatch production probes.
        let _root = muxel_store::test_support::set_root(
            std::env::temp_dir().join(format!("muxel-close-probes-{}", uuid::Uuid::new_v4())),
        );
        let cx = TestAppContext::single();
        let executor = cx.executor();
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let (entered, observed) = async_channel::unbounded();
        let (release, waiting) = async_channel::unbounded();
        let (finished, results) = async_channel::unbounded();
        struct Active(Arc<AtomicUsize>);
        impl Drop for Active {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let start_batch = |jobs: Vec<usize>| {
            let (active, maximum, entered, waiting, finished, executor) = (
                active.clone(),
                maximum.clone(),
                entered.clone(),
                waiting.clone(),
                finished.clone(),
                executor.clone(),
            );
            let worker_executor = executor.clone();
            executor.spawn(async move {
                let result = collect(worker_executor, jobs, move |job| {
                    let (active, maximum, entered, waiting) = (
                        active.clone(),
                        maximum.clone(),
                        entered.clone(),
                        waiting.clone(),
                    );
                    async move {
                        let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                        let _active = Active(active.clone());
                        maximum.fetch_max(count, Ordering::SeqCst);
                        entered.send(job).await.unwrap();
                        waiting.recv().await.unwrap();
                        // Errors are results too: the helper must not omit jobs.
                        if job == 5 { Err(job) } else { Ok(job) }
                    }
                })
                .await;
                finished.send(result).await.unwrap();
            })
        };
        let first = start_batch(vec![0, 1]);
        cx.run_until_parked();
        assert_eq!(
            active.load(Ordering::SeqCst),
            2,
            "positive control: probes overlap"
        );
        let second = start_batch(vec![2, 3, 4, 5, 6, 7]);
        cx.run_until_parked();
        assert_eq!(active.load(Ordering::SeqCst), 4);
        assert_eq!(
            observed.len(),
            4,
            "second batch exceeded the shared admission limit"
        );
        for _ in 0..8 {
            release.try_send(()).unwrap();
        }
        cx.run_until_parked();
        assert!(first.is_ready() && second.is_ready());
        let mut values = Vec::new();
        for _ in 0..2 {
            values.extend(results.try_recv().unwrap());
        }
        assert_eq!(values.iter().filter(|result| result.is_err()).count(), 1);
        let mut ids: Vec<_> = values
            .into_iter()
            .map(|result| result.unwrap_or_else(|id| id))
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, (0..8).collect::<Vec<_>>());
        assert_eq!(observed.len(), 8);
        assert_eq!(maximum.load(Ordering::SeqCst), 4);
        assert_eq!(active.load(Ordering::SeqCst), 0);

        // Cancellation must return globally held permits, not strand the next
        // batch behind four futures that no longer exist.
        let cancelled = start_batch(vec![8, 9, 10, 11]);
        cx.run_until_parked();
        assert_eq!(active.load(Ordering::SeqCst), 4);
        drop(cancelled);
        cx.run_until_parked();
        assert_eq!(active.load(Ordering::SeqCst), 0);
        let after_cancel = start_batch(vec![12, 13, 14, 15]);
        cx.run_until_parked();
        assert_eq!(active.load(Ordering::SeqCst), 4);
        for _ in 0..4 {
            release.try_send(()).unwrap();
        }
        cx.run_until_parked();
        assert!(after_cancel.is_ready());
        assert_eq!(results.try_recv().unwrap().len(), 4);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }
}
