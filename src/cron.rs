use futures::TryStreamExt;
use immortal_lib::{immortal::{call_version, client_start_workflow_options_version}, Client};
use k8s_openapi::api::core::v1::ConfigMap;
use serde::{Deserialize, Serialize};
use simd_json::json;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;
use tokio_cron_scheduler::{Job, JobScheduler, JobSchedulerError};
use uuid::Uuid;

use kube::{
    api::{Api, Patch, PatchParams, PostParams, ResourceExt},
    runtime::{watcher, WatchStreamExt},
    Client as KubeClient,
};

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct CronSpec {
    pub id: Uuid,
    pub label: String,
    pub description: Option<String>,
    pub status: CronStatus,
    pub schedule: String,
    pub job: CronJob,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
#[serde(tag = "type", content = "spec")]
pub enum CronStatus {
    Running,
    Paused(Option<String>),
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Debug)]
#[serde(tag = "type", content = "spec")]
pub enum CronJob {
    Workflow(immortal_lib::immortal::client_start_workflow_options_version::Version),
    Call(immortal_lib::immortal::call_version::Version),
    Notification(immortal_lib::immortal::notify_version::Version),
}

pub struct CronManager {
    sched: JobScheduler,
    // key (stable id from CM) -> scheduler job id
    installed: HashMap<Uuid, Uuid>,
    pub installed2: HashMap<Uuid, CronSpec>,
    pub installed3: Vec<CronSpec>,
    kube_client: KubeClient,
    // immortal_client: Arc<Mutex<Client>>,
}

impl std::fmt::Debug for CronManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // grab a tiny sample of keys so Debug stays short and doesn't require Debug on values
        let mut sample_installed_keys: Vec<_> = self.installed.keys().cloned().take(3).collect();
        let mut sample_installed2_keys: Vec<_> = self.installed2.keys().cloned().take(3).collect();

        // sort for stable output (optional)
        sample_installed_keys.sort();
        sample_installed2_keys.sort();

        f.debug_struct("CronManager")
            .field("installed_len", &self.installed.len())
            .field("installed_keys_sample", &sample_installed_keys)
            .field("installed2_len", &self.installed2.len())
            .field("installed2_keys_sample", &sample_installed2_keys)
            .finish()
    }
}

impl CronManager {
    pub async fn new(kube_client: KubeClient) -> Result<Self, JobSchedulerError> {
        let sched = JobScheduler::new().await?;
        // Important: start the scheduler loop
        sched.start().await?;
        Ok(Self {
            sched,
            installed: HashMap::new(),
            installed2: HashMap::new(),
            installed3: Vec::new(),
            kube_client,
            // immortal_client,
        })
    }
    async fn get_config_kube(&self) -> anyhow::Result<CronConfig> {
        let ns = std::env::var("K8S_NAMESPACE").unwrap_or("default".to_string());
        let api = Api::<ConfigMap>::namespaced(self.kube_client.clone(), &ns);
        let cm: ConfigMap = api.get("immortal-cron").await?;

        if let Some(data) = cm.data {
            if let Some(config) = data.get("config") {
                let mut x = config.as_bytes().to_owned();
                let parsed_config: CronConfig = simd_json::serde::from_slice(&mut x)?;
                return Ok(parsed_config);
            }
        }
        Err(anyhow::anyhow!("Couldn't find config"))
    }
    async fn save_config_kube(&self, config: CronConfig) -> anyhow::Result<()> {
        let ns = std::env::var("K8S_NAMESPACE").unwrap_or("default".to_string());
        let api = Api::<ConfigMap>::namespaced(self.kube_client.clone(), &ns);
        let patch: ConfigMap = simd_json::serde::from_owned_value(json!({
        "metadata": {
          "name": "immortal-cron",
          "namespace": ns,
          "labels": {
            "app": "immortal"
          }
        },
        "immutable": false,
        "data": {
          "config": simd_json::to_string(&config)?
        }
              }))?;
        let params = PatchParams::apply("immortal").force();
        let patch = Patch::Apply(&patch);
        let _cm: ConfigMap = api.patch("immortal-cron", &params, &patch).await?;
        Ok(())
    }
    pub async fn create_cron(&self, new_cron: CronSpec) -> anyhow::Result<()> {
        let mut current_config = self.get_config_kube().await?;
        match current_config {
            CronConfig::V1(ref mut v1) => v1.crons.push(new_cron),
        }
        println!("{:#?}", current_config);
        self.save_config_kube(current_config).await?;
        Ok(())
    }
    pub async fn delete_cron(&self, cron_id: Uuid) -> anyhow::Result<()> {
        let mut current_config = self.get_config_kube().await?;
        match current_config {
            CronConfig::V1(ref mut v1) => {
                if let Some(index) = v1.crons.iter().position(|f| f.id == cron_id) {
                    v1.crons.remove(index);
                }
            }
        }
        self.save_config_kube(current_config).await?;
        Ok(())
    }
    pub async fn update_cron(&self, updated_cron: CronSpec) -> anyhow::Result<()> {
        let mut current_config = self.get_config_kube().await?;
        match current_config {
            CronConfig::V1(ref mut v1) => {
                if let Some(index) = v1.crons.iter().position(|f| f.id == updated_cron.id) {
                    v1.crons.remove(index);
                    v1.crons.push(updated_cron);
                }
            }
        }
        self.save_config_kube(current_config).await?;
        Ok(())
    }
    pub async fn update_cron_status(&self, id: Uuid, status: CronStatus) -> anyhow::Result<()> {
        let mut current_config = self.get_config_kube().await?;
        match current_config {
            CronConfig::V1(ref mut v1) => {
                if let Some(index) = v1.crons.iter().position(|f| f.id == id) {
                    let cron = v1.crons.get_mut(index).unwrap();
                    cron.status = status.clone();
                }
            }
        }
        self.save_config_kube(current_config).await?;
        Ok(())
    }
    /// Reconcile from desired map (from ConfigMap) to the running scheduler state.
    async fn reconcile(&mut self, desired: Vec<CronSpec>) -> Result<(), JobSchedulerError> {
        let desired_filtered: Vec<_> = desired
            .iter()
            .filter(|f| f.status == CronStatus::Running)
            .map(|f| f.clone())
            .collect();

        // 1) Remove jobs no longer desired
        for (key, job_id) in self.installed.clone() {
            if !desired_filtered.iter().find(|f| f.id == key).is_some() {
                let _ = self.sched.remove(&job_id).await;
                self.installed.remove(&key);
                self.installed2.remove(&key);
            }
        }

        // 2) Add/update desired jobs
        for spec in desired_filtered {
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
            let immortal_client: Arc<Mutex<Client>> = Arc::new(Mutex::new(
                Client::connect(
                    std::env::var("IMMORTAL_URL").unwrap_or("http://localhost:10000".to_string()),
                )
                .await
                .unwrap(),
            ));
            // let immortal_client = Arc::clone(&self.immortal_client);
            let job_payload = spec.job.clone();
            let schedule = spec.schedule.clone();

            let job = Job::new_async("0 ".to_owned() + schedule.as_str(), move |_uuid, _l| {
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
                        CronJob::Call(call_options) => match call_options {
                           call_version::Version::V1(v1) => {
                                let mut cli = immortal_client.lock().await;
                                if let Err(e) = cli
                                    .call_async_v1(v1.input, &v1.call_type, &v1.task_queue)
                                    .await
                                {
                                    println!("[cron workflow] error: {e:#?}");
                                }
                           } 
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

        {
            self.installed3 = desired.clone();
        }

        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(tag = "version", content = "spec")]
enum CronConfig {
    V1(CronConfigV1),
}

#[derive(Deserialize, Serialize, Debug)]
struct CronConfigV1 {
    pub crons: Vec<CronSpec>,
}

pub async fn start_watcher(cron_manager: Arc<Mutex<CronManager>>) -> anyhow::Result<()> {
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
                        let parsed_config: CronConfig =
                            simd_json::serde::from_slice(&mut x).unwrap();
                        cron_manager
                            .reconcile(match parsed_config {
                                CronConfig::V1(v1) => v1.crons,
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
