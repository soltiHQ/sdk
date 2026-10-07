use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use taskvisor::TaskFn;

use super::*;

#[derive(Clone)]
struct TestClock {
    start: tokio::time::Instant,
    origin: DateTime<Utc>,
    offset_ms: Arc<AtomicI64>,
}
impl TestClock {
    fn new() -> Self {
        Self {
            start: tokio::time::Instant::now(),
            origin: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            offset_ms: Arc::new(AtomicI64::new(0)),
        }
    }
    fn adjust(&self, milliseconds: i64) {
        self.offset_ms.fetch_add(milliseconds, Ordering::SeqCst);
    }
}
impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        self.origin
            + chrono::Duration::from_std(self.start.elapsed()).unwrap()
            + chrono::Duration::milliseconds(self.offset_ms.load(Ordering::SeqCst))
    }
}

fn cron(job: TaskRef, timeout: Duration) -> CronTask {
    CronTask::new(
        CronSchedule::parse("* * * * * *", "UTC").unwrap(),
        job,
        timeout,
    )
    .unwrap()
}
async fn settle() {
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
}
async fn advance(duration: Duration) {
    tokio::time::advance(duration).await;
    settle().await;
}

#[test]
fn zero_and_unrepresentable_durations_are_rejected() {
    let job = TaskFn::arc(|_| async { Ok(()) });
    let schedule = CronSchedule::parse("* * * * *", "UTC").unwrap();
    assert!(matches!(
        CronTask::new(schedule.clone(), job.clone(), Duration::ZERO),
        Err(CronError::InvalidDuration("invocation_timeout"))
    ));
    assert!(matches!(
        CronTask::new(schedule, job.clone(), Duration::MAX),
        Err(CronError::InvalidDuration("invocation_timeout"))
    ));
    assert!(matches!(
        cron(job, Duration::from_secs(1)).with_max_lateness(Duration::ZERO),
        Err(CronError::InvalidDuration("max_lateness"))
    ));
}

#[tokio::test(start_paused = true)]
async fn cancelled_registration_never_constructs_the_job() {
    let calls = Arc::new(AtomicUsize::new(0));
    let job = TaskFn::arc({
        let calls = calls.clone();
        move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        }
    });
    let result = cron(job, Duration::from_secs(1))
        .run(TaskContext::detached_cancelled(), TestClock::new())
        .await;
    assert!(matches!(result, Err(TaskError::Canceled)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn waiting_cancellation_does_not_wait_for_the_calendar() {
    let calls = Arc::new(AtomicUsize::new(0));
    let job = TaskFn::arc({
        let calls = calls.clone();
        move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        }
    });
    let task = cron(job, Duration::from_secs(1));
    let ctx = TaskContext::detached();
    let cancellation = ctx.cancellation_token();
    let runner = tokio::spawn(async move { task.run(ctx, TestClock::new()).await });
    settle().await;
    cancellation.cancel();
    assert!(matches!(runner.await.unwrap(), Err(TaskError::Canceled)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

struct PendingProbe {
    ctx: TaskContext,
    cancelled_on_drop: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}
impl Future for PendingProbe {
    type Output = Result<(), TaskError>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}
impl Drop for PendingProbe {
    fn drop(&mut self) {
        self.cancelled_on_drop
            .store(self.ctx.is_cancelled(), Ordering::SeqCst);
        self.dropped.store(true, Ordering::SeqCst);
    }
}
fn pending_probe() -> (TaskRef, Arc<AtomicBool>, Arc<AtomicBool>, Arc<AtomicUsize>) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let job = TaskFn::arc({
        let cancelled = cancelled.clone();
        let dropped = dropped.clone();
        let calls = calls.clone();
        move |ctx| {
            calls.fetch_add(1, Ordering::SeqCst);
            PendingProbe {
                ctx,
                cancelled_on_drop: cancelled.clone(),
                dropped: dropped.clone(),
            }
        }
    });
    (job, cancelled, dropped, calls)
}

#[tokio::test(start_paused = true)]
async fn invocation_timeout_signals_child_before_dropping_job() {
    let (job, cancelled, dropped, calls) = pending_probe();
    let task = cron(job, Duration::from_millis(250));
    let runner =
        tokio::spawn(async move { task.run(TaskContext::detached(), TestClock::new()).await });
    settle().await;
    advance(Duration::from_secs(1)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    advance(Duration::from_millis(250)).await;
    assert!(
        matches!(runner.await.unwrap(), Err(TaskError::Timeout { timeout, .. }) if timeout == Duration::from_millis(250))
    );
    assert!(cancelled.load(Ordering::SeqCst));
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn active_cancellation_drops_job_and_returns_original_category() {
    let (job, cancelled, dropped, calls) = pending_probe();
    let task = cron(job, Duration::from_secs(10));
    let ctx = TaskContext::detached();
    let token = ctx.cancellation_token();
    let runner = tokio::spawn(async move { task.run(ctx, TestClock::new()).await });
    settle().await;
    advance(Duration::from_secs(1)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    token.cancel();
    assert!(matches!(runner.await.unwrap(), Err(TaskError::Canceled)));
    assert!(cancelled.load(Ordering::SeqCst));
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn parent_cancellation_allows_asynchronous_job_cleanup() {
    let cleaned = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let job = TaskFn::arc({
        let cleaned = cleaned.clone();
        let calls = calls.clone();
        move |ctx: TaskContext| {
            let cleaned = cleaned.clone();
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                ctx.cancelled().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                cleaned.store(true, Ordering::SeqCst);
                Err(TaskError::Canceled)
            }
        }
    });
    let task = cron(job, Duration::from_secs(10));
    let ctx = TaskContext::detached();
    let token = ctx.cancellation_token();
    let runner = tokio::spawn(async move { task.run(ctx, TestClock::new()).await });
    settle().await;
    advance(Duration::from_secs(1)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    token.cancel();
    settle().await;
    assert!(
        !runner.is_finished(),
        "wrapper must retain and poll cooperative cleanup"
    );
    advance(Duration::from_millis(50)).await;
    assert!(matches!(runner.await.unwrap(), Err(TaskError::Canceled)));
    assert!(cleaned.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn invocation_failure_is_not_swallowed_or_retried_inside_cron() {
    let calls = Arc::new(AtomicUsize::new(0));
    let job = TaskFn::arc({
        let calls = calls.clone();
        move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(TaskError::fatal("permanent")) }
        }
    });
    let task = cron(job, Duration::from_secs(1));
    let runner =
        tokio::spawn(async move { task.run(TaskContext::detached(), TestClock::new()).await });
    settle().await;
    advance(Duration::from_secs(1)).await;
    assert!(
        matches!(runner.await.unwrap(), Err(TaskError::Fatal { reason, .. }) if reason == "permanent")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn long_jobs_are_serial_and_skip_occurrences_while_running() {
    let calls = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    struct Active(Arc<AtomicUsize>);
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let job = TaskFn::arc({
        let calls = calls.clone();
        let active = active.clone();
        move |_| {
            let calls = calls.clone();
            let active = active.clone();
            async move {
                assert_eq!(active.fetch_add(1, Ordering::SeqCst), 0);
                let _active = Active(active);
                calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(3)).await;
                Ok(())
            }
        }
    });
    let ctx = TaskContext::detached();
    let token = ctx.cancellation_token();
    let task = cron(job, Duration::from_secs(10));
    let runner = tokio::spawn(async move { task.run(ctx, TestClock::new()).await });
    settle().await;
    advance(Duration::from_secs(1)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    advance(Duration::from_secs(3)).await;
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    advance(Duration::from_secs(1)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    token.cancel();
    assert!(matches!(runner.await.unwrap(), Err(TaskError::Canceled)));
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn clock_adjustments_skip_stale_occurrences_without_duplicates() {
    let calls = Arc::new(AtomicUsize::new(0));
    let job = TaskFn::arc({
        let calls = calls.clone();
        move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        }
    });
    let task = cron(job, Duration::from_secs(10))
        .with_max_lateness(Duration::from_secs(1))
        .unwrap();
    let clock = TestClock::new();
    let adjusted = clock.clone();
    let ctx = TaskContext::detached();
    let token = ctx.cancellation_token();
    let runner = tokio::spawn(async move { task.run(ctx, clock).await });
    settle().await;
    adjusted.adjust(120_000);
    advance(Duration::from_secs(1)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "stale occurrence is skipped"
    );
    advance(Duration::from_secs(1)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    adjusted.adjust(-2_000);
    advance(Duration::from_secs(2)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "clock rollback cannot repeat the elapsed occurrence"
    );
    advance(Duration::from_secs(1)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    token.cancel();
    assert!(matches!(runner.await.unwrap(), Err(TaskError::Canceled)));
}

#[tokio::test(start_paused = true)]
async fn aborting_the_wrapper_releases_its_owned_job_future() {
    let (job, _, dropped, calls) = pending_probe();
    let task = cron(job, Duration::from_secs(10));
    let runner =
        tokio::spawn(async move { task.run(TaskContext::detached(), TestClock::new()).await });
    settle().await;
    advance(Duration::from_secs(1)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    runner.abort();
    assert!(runner.await.unwrap_err().is_cancelled());
    assert!(dropped.load(Ordering::SeqCst));
    advance(Duration::from_secs(10)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
