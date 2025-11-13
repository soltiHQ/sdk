use std::sync::Arc;

// TODO: change to 'SupervisorConfig' after: https://github.com/soltiHQ/taskvisor/issues/47
use taskvisor::{Config as SupervisorConfig, ControllerConfig, Supervisor};
use tracing::{debug, info, instrument};

use crate::{error::CoreError, map::to_controller_spec, router::RunnerRouter};

pub struct SupervisorApi {
    sup: Arc<Supervisor>,
    router: RunnerRouter,
}

impl SupervisorApi {
    // #[instrument(level = "info", skip(router))]
    pub async fn new_default(
        router: RunnerRouter,
        subscribers: Vec<Arc<dyn taskvisor::Subscribe>>,
    ) -> Result<Self, CoreError> {
        let sup = Supervisor::builder(SupervisorConfig::default())
            .with_controller(ControllerConfig::default())
            .with_subscribers(subscribers)
            .build();

        // 🔧 Запускаем цикл супервайзера в фоне (как в примере taskvisor)
        let runner = Arc::clone(&sup);
        tokio::spawn(async move {
            let _ = runner.run(Vec::new()).await;
        });

        // 🔧 Дождаться готовности (в твоей версии метод есть — ты же уже вызывал его ранее)
        sup.wait_ready().await;

        info!("supervisor is ready");
        Ok(Self { sup, router })
    }

    pub fn supervisor(&self) -> Arc<Supervisor> {
        Arc::clone(&self.sup)
    }

    #[instrument(level = "debug", skip(self, spec), fields(slot = %spec.slot, kind = ?spec.kind))]
    pub async fn submit(&self, spec: &tno_model::CreateSpec) -> Result<(), CoreError> {
        // 1) Собираем TaskSpec
        let task = self.router.build(spec)?;
        let tspec = crate::map::to_task_spec(task, spec);

        // 2) Admission строго через helper (как в примере taskvisor)
        use taskvisor::ControllerSpec as CS;
        let cspec = match spec.admission {
            tno_model::AdmissionStrategy::Queue => CS::queue(tspec),
            tno_model::AdmissionStrategy::Replace => CS::replace(tspec),
            tno_model::AdmissionStrategy::DropIfRunning => CS::drop_if_running(tspec),
        };

        debug!("submitting via controller");
        // 3) Сабмит ОДНИМ аргументом — ControllerSpec (у тебя такая сигнатура и есть)
        self.sup
            .submit(cspec)
            .await
            .map_err(|e| CoreError::Supervisor(e.to_string()))
    }
}
