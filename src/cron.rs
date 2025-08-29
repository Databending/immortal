use futures::TryStreamExt;
use immortal_lib::{
    immortal::{client_start_workflow_options_version, CallVersion, NotifyVersion},
    Client,
};
use k8s_openapi::api::core::v1::ConfigMap;
use serde::{Deserialize, Serialize};
use simd_json::json;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;
use tokio_cron_scheduler::{Job, JobScheduler, JobSchedulerError};
use uuid::Uuid;

use kube::{
    api::{Api, PostParams, ResourceExt},
    runtime::{watcher, WatchStreamExt},
    Client as KubeClient,
};

#[derive(Deserialize, Serialize, PartialEq)]
struct CronSpec {
    id: Uuid,
    schedule: String,
    job: CronJob,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
enum CronJob {
    Workflow(immortal_lib::immortal::client_start_workflow_options_version::Version),
    Call(CallVersion),
    Notification(NotifyVersion),
}

struct CronManager {
    sched: JobScheduler,
    // key (stable id from CM) -> scheduler job id
    installed: HashMap<Uuid, Uuid>,
    installed2: HashMap<Uuid, CronSpec>,
    immortal_client: Arc<Mutex<Client>>,
}
impl CronManager {
    async fn new() -> Result<Self, JobSchedulerError> {
        let immortal_client: Arc<Mutex<Client>> = Arc::new(Mutex::new(
            Client::connect(
                std::env::var("IMMORTAL_URL").unwrap_or("http://localhost:10000".to_string()),
            )
            .await
            .unwrap(),
        ));
        let sched = JobScheduler::new().await?;
        // Important: start the scheduler loop
        sched.start().await?;
        Ok(Self {
            sched,
            installed: HashMap::new(),
            installed2: HashMap::new(),
            immortal_client,
        })
    }
    /// Reconcile from desired map (from ConfigMap) to the running scheduler state.
    async fn reconcile(&mut self, desired: Vec<CronSpec>) -> Result<(), JobSchedulerError> {
        // 1) Remove jobs no longer desired
        for (key, job_id) in self.installed.clone() {
            if !desired.iter().find(|f| f.id == key).is_some() {
                let _ = self.sched.remove(&job_id).await;
                self.installed.remove(&key);
                self.installed2.remove(&key);
            }
        }

        // 2) Add/update desired jobs
        for spec in desired {
            // If exists and schedule+payload unchanged, skip. Otherwise re-add (simplest).

            match self.installed2.get(&spec.id) {
                Some(old_spec) => {
                    if *old_spec != spec {
                        if let Some(old) = self.installed.remove(&spec.id) {
                            let _ = self.sched.remove(&old).await;
                            self.installed2.remove(&spec.id);
                        }
                    }
                }
                None => {}
            }

            let immortal_client = Arc::clone(&self.immortal_client);
            let job_payload = spec.job.clone();
            let schedule = spec.schedule.clone();

            let job = Job::new_async(schedule.as_str(), move |_uuid, _l| {
                let immortal_client = Arc::clone(&immortal_client);
                let job_payload = job_payload.clone();
                Box::pin(async move {
                    match job_payload {
                        CronJob::Workflow(workflow_options) => match workflow_options {
                            client_start_workflow_options_version::Version::V1(v1) => {
                                let mut cli = immortal_client.lock().await;
                                if let Err(e) = cli
                                    .start_workflow_v1(v1.input, &v1.workflow_type, &v1.task_queue)
                                    .await
                                {
                                    eprintln!("[cron workflow] error: {e:#?}");
                                }
                            }
                        },
                        CronJob::Call(call) => {
                            // TODO: invoke your call path
                            let _ = call; /* implement */
                        }
                        CronJob::Notification(notify) => {
                            // TODO: invoke your notify path
                            let _ = notify; /* implement */
                        }
                    }
                })
            })?;

            let id = self.sched.add(job).await?;
            self.installed.insert(spec.id.clone(), id);
            self.installed2.insert(spec.id.clone(), spec);
        }

        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "version", content = "spec")]
enum CronConfig {
    V1(CronConfigV1),
}

#[derive(Deserialize, Serialize)]
struct CronConfigV1 {
    pub crons: Vec<CronSpec>,
}

pub async fn start_watcher() -> anyhow::Result<()> {
    let client = KubeClient::try_default().await?;
    let ns = std::env::var("K8S_NAMESPACE").unwrap_or("default".to_string());
    let api = Api::<ConfigMap>::namespaced(client, &ns);

    if api.get_opt("immortal-cron").await?.is_none() {
        let default_cron_config = CronConfig::V1(CronConfigV1 { crons: vec![] });

        let data: ConfigMap = simd_json::serde::from_owned_value(json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
          "name": "immortal-cron",
          "namespace": ns,
          "labels": {
            "app": "immortal"
          }
        },
        "immutable": false,
        "data": {
          "config": simd_json::to_string(&default_cron_config)?
        }
              }))?;
        api.create(&PostParams::default(), &data).await?;
    }

    let use_watchlist = std::env::var("WATCHLIST")
        .map(|s| s == "1")
        .unwrap_or(false);
    let mut wc = if use_watchlist {
        // requires WatchList feature gate on 1.27 or later
        watcher::Config::default().streaming_lists()
    } else {
        watcher::Config::default()
    };

    // restrict to the immortal-cron configmap only
    wc = wc.fields(&format!(
        "metadata.name=immortal-cron,metadata.namespace={}",
        ns
    ));
    let cron_manager = Arc::new(Mutex::new(CronManager::new().await?));

    watcher(api, wc)
        .applied_objects()
        .default_backoff()
        .try_for_each(|cm| {
            let cron_manager_arc = cron_manager.clone();
            async move {
                println!("Saw ConfigMap update: {}", cm.name_any());

                if let Some(data) = cm.data {
                    if let Some(config) = data.get("config") {
                        let mut cron_manager = cron_manager_arc.lock().await;

                        let mut x = config.as_bytes().to_owned();
                        let parsed_config: CronConfig = simd_json::serde::from_slice(&mut x).unwrap();
                        cron_manager
                            .reconcile(match parsed_config {
                                CronConfig::V1(v1) => v1.crons
                                
                            })
                            .await
                            .unwrap();
                    }
                }

                // info!("saw {}", p.name_any());
                // if let Some(unready_reason) = pod_unready(&p) {
                // warn!("{}", unready_reason);
                // }
                Ok(())
            }
        })
        .await?;
    Ok(())
}
