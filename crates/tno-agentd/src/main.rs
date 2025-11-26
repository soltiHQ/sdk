use std::sync::Arc;
use std::time::Duration;
use tno_core::{RunnerRouter, SupervisorApi};
use tno_exec::prelude::*;
//use tno_exec::proc::ProcConfig;
use tno_model::{
    AdmissionStrategy, BackoffStrategy, CreateSpec, JitterStrategy, RestartStrategy, TaskKind,
};
use tno_observe::*;
use tracing::info;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    // 1) Логгер (text по умолчанию; уровень берётся из cfg.level или RUST_LOG)
    let mut cfg = LoggerConfig::default();
    cfg.level = tno_observe::LoggerLevel::new("debug")?;

    init_logger(&cfg)?;
    info!("after log");

    let journal = Arc::new(Subscriber::default()) as Arc<dyn taskvisor::Subscribe>;
    let subscribers: Vec<Arc<dyn taskvisor::Subscribe>> =
        vec![Arc::new(Subscriber::default()) as Arc<dyn taskvisor::Subscribe>];
    // 2) Готовим runner: "ls /tmp"
    // let runner = ProcRunner::new(ProcConfig {
    //     program: "top".into(),
    //     args: vec![],
    //     env: vec![], // можно добавить пары ("KEY".into(), "VALUE".into())
    //     cwd: None,   // можно задать рабочую директорию
    //     fail_on_non_zero: true,
    // });
    info!("after runner");

    // 3) Регистрируем runner в роутере
    let mut router = RunnerRouter::new();
    // router.register(Arc::new(runner));

    // 4) Поднимаем SupervisorApi
    let api = SupervisorApi::new(
        taskvisor::SupervisorConfig::default(),
        taskvisor::ControllerConfig::default(),
        subscribers,
        router,
    )
    .await?;

    // 5) Спека на задачу (TaskKind::Exec)
    // let spec = CreateSpec {
    //     slot: "demo".into(),
    //     kind: TaskKind::Exec,
    //     admission: AdmissionStrategy::Replace,
    //     restart: RestartStrategy::Always { interval_ms: None },
    //     backoff: BackoffStrategy {
    //         first_ms: 5000, // базовая задержка после фейла
    //         max_ms: 30_000,
    //         factor: 2.0,
    //         jitter: JitterStrategy::Full, // или None, чтобы не получать 0
    //     },
    //     timeout_ms: 3_000, // 10s, чтобы демо не зависало
    // };

    // 6) Сабмитим
    //api.submit(&spec).await?;
    let (task, spec) = tno_observe::timezone_sync();
    let policy = tno_core::TaskPolicy::from_spec(&spec);

    api.submit_with_task(task, &policy).await?;

    //
    // Небольшая пауза, чтобы увидеть вывод команды в логах/консоли процесса
    // (taskvisor выполняет задачу асинхронно)
    tokio::time::sleep(Duration::from_secs(10)).await;

    Ok(())
}
