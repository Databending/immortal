use immortal_lib::common::Payloads;
use immortal_lib::immortal::ActivityCache;
use socketioxide::SocketIo;
#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

// #[cfg(not(feature = "server"))]
// compile_error!("This binary crate requires `--features server`.");
// easy break: run and didn't instantly die
use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;

use kube::Client as KubeClient;
// pub mod immortal {
//     tonic::include_proto!("immortal");
// }
use axum;
use dotenvy::dotenv;
use history::Status as HistoryStatus;
use history::{ActivityHistory, ActivityRun, History, WorkflowHistory};
// use immortal_lib::common;
use immortal_lib::common::Payload;
use immortal_lib::failure;
use immortal_lib::immortal;
use immortal_lib::immortal::call_result_version;
use immortal_lib::immortal::call_version;
use immortal_lib::immortal::notify_version;
use immortal_lib::immortal::CallResultV1;
use immortal_lib::immortal::CallResultVersion;
use immortal_lib::immortal::CallVersion;
use immortal_lib::immortal::ClientStartWorkflowOptionsV1;
use immortal_lib::immortal::NotifyVersion;
use immortal_lib::immortal::RequestStartActivityOptionsV1;
use immortal_lib::immortal::StartNotificationOptionsV1;
use immortal_worker_lib::models::ActivitySchema;
use immortal_worker_lib::models::CallSchema;
use immortal_worker_lib::models::WfSchema;
use rand::Rng;
use redis::streams::StreamMaxlen;
use redis::AsyncCommands;
// use bb8_redis::redis::AsyncCommands;
use bb8_redis::{bb8, RedisConnectionManager};
use immortal::immortal_server::{Immortal, ImmortalServer};
use immortal::immortal_worker_action_v1::Action as WorkerAction;
use immortal::{
    activity_result_version, client_start_workflow_options_version, immortal_server_action_v1,
    immortal_server_action_version, immortal_worker_action_version,
    request_start_activity_options_version, workflow_result_v1, workflow_result_version,
    ActivityResultV1, ActivityResultVersion, ClientStartWorkflowOptionsVersion,
    ClientStartWorkflowResponse, ImmortalServerActionVersion, ImmortalWorkerActionV1,
    ImmortalWorkerActionVersion, RequestStartActivityOptionsVersion, StartWorkflowOptionsV1,
    WorkflowResultVersion,
};
use regex::Regex;
use simd_json::OwnedValue;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::watch;
use tokio::sync::{broadcast, Notify, RwLock};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tonic::transport::Server;
// use tonic_health::server::HealthReporter;
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use tracing::error;
// use tracing_subscriber::FmtSubscriber;
use uuid::Uuid;
// use immortal::immortal_s
// use immortal::im::{Server, ServerServer};
// use routeguide::route_guide_server::{RouteGuide, RouteGuideServer};
// use routeguide::worker_action::Action;
// use routeguide::{Feature, Point, Rectangle, RouteNote, RouteSummary};

use crate::cron::start_watcher;
use crate::cron::CronManager;
use crate::metrics::IdentifiableMetrics;
use crate::state::AppState;
use crate::state::JwtPublicBytes;
use crate::ws::on_connect;
use crate::ws::WsState;
use serde::Serialize;
use std::path::Path;
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
pub mod api;
pub mod cron;
pub mod error;
pub mod history;
pub mod metrics;
pub mod models;
pub mod state;
pub mod utils;
pub mod ws;

pub async fn get_file_as_byte_vec() -> JwtPublicBytes {
    let filename = Path::new("jwt.pem").to_str().unwrap();
    let mut f = tokio::fs::File::open(&filename)
        .await
        .expect("no file found");
    let metadata = tokio::fs::metadata(&filename)
        .await
        .expect("unable to read metadata");
    let mut buffer = vec![0; metadata.len() as usize];
    f.read(&mut buffer).await.expect("buffer overflow");

    JwtPublicBytes(buffer)
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", content = "spec")]
pub enum Notification {
    // ActivityStarted(Uuid, ActivityHistory),
    // ActivityCompleted(Uuid, ActivityHistory),
    // ActivityFailed(Uuid, ActivityHistory),
    // ActivityCancelled,
    WorkflowStarted(Uuid, WorkflowHistory),
    WorkflowCompleted(Uuid, WorkflowHistory),
    WorkflowResult(Uuid, WorkflowResultVersion),
    WorkflowFailed(Uuid, WorkflowHistory),
    // WorkflowCancelled,
    ActivityRunStarted(Uuid, ActivityHistory),
    ActivityRunCompleted(Uuid, ActivityHistory),
    ActivityRunFailed(Uuid, ActivityHistory),
    WorkerAdded(String),
    WorkerRemoved(String),
    // ActivityRunCancelled,
}
#[derive(Debug)]
struct RegisteredWorker {
    worker_id: String,
    registered_on: DateTime<Utc>,
    task_queue: String,
    // metrics_stream: broadcast::Sender<Metrics>,
    _incoming: JoinHandle<()>,
    tx: Sender<Result<ImmortalWorkerActionVersion, Status>>,
    registered_workflows: HashMap<String, WfSchema>,
    registered_activities: HashMap<String, ActivitySchema>,
    _registered_calls: HashMap<String, CallSchema>,
    activity_capacity: i32,
    max_activity_capacity: i32,
    workflow_capacity: i32,
    max_workflow_capacity: i32,
}

// #[derive(Debug, Clone)]
// struct LogStream {
//     stream_id: String,
//     tags: HashMap<String, String>,
// }
//
// #[derive(Debug, Clone)]
// enum LogStreamUpdate {
//     Add(LogStream),
//     Remove(String),
// }

pub enum WorkflowStatus {
    Running,
    Completed,
    Failed,
}
//
// #[derive(Debug, Clone)]
// struct CallQueue(
//     Arc<
//         Mutex<
//             HashMap<
//                 String,
//                 VecDeque<(
//                     String,
//                     CallOptions,
//                     tokio::sync::broadcast::Sender<CallResultV1>,
//                 )>,
//             >,
//         >,
//     >,
// );
//
// impl CallQueue {
//     async fn get_queue(
//         &self,
//     ) -> tokio::sync::MutexGuard<
//         '_,
//         HashMap<
//             String,
//             VecDeque<(
//                 String,
//                 CallOptions,
//                 tokio::sync::broadcast::Sender<CallResultV1>,
//             )>,
//         >,
//     > {
//         self.0.lock().await
//     }
// }

#[derive(Debug, Clone, Serialize, PartialEq)]
struct CallOptions {
    call_type: String,
    input: Option<Payload>,
    task_queue: String,
}

#[derive(Debug, Clone, Serialize)]
struct RunningProperties<T> {
    start: DateTime<Utc>,
    timeout: DateTime<Utc>,
    heartbeat_timeout: Duration,
    // in seconds
    max_duration: Duration,
    worker_id: String,
    additional_properties: T,
}

#[derive(Debug, Clone, Serialize)]
struct CallProperties {
    pub last_heartbeat: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
struct ActivityProperties {
    pub workflow_id: String,
    pub last_heartbeat: DateTime<Utc>,
    pub scheduled: DateTime<Utc>,
    pub latest_run_start: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct ImmortalService {
    redis_pool: bb8::Pool<RedisConnectionManager>,
    cron_manager: Arc<Mutex<CronManager>>,
    metrics_stream: broadcast::Sender<IdentifiableMetrics>,
    workers: Arc<RwLock<HashMap<String, RegisteredWorker>>>,
    // log_streams: (
    //     broadcast::Sender<LogStreamUpdate>,
    //     Arc<Mutex<HashMap<String, LogStream>>>,
    // ),
    history: History,

    notification_tx: Arc<tokio::sync::broadcast::Sender<Notification>>,
    notification_rx: Arc<tokio::sync::broadcast::Receiver<Notification>>,

    call_notify: Arc<Notify>,
    running_calls: Arc<
        RwLock<
            HashMap<
                String,
                Box<(
                    Option<tokio::sync::mpsc::Sender<CallResultV1>>,
                    RunningProperties<CallProperties>,
                )>,
            >,
        >,
    >,
    orphaned_activities: Arc<RwLock<HashMap<String, tokio::sync::oneshot::Sender<bool>>>>,
    orphaned_workflows: Arc<RwLock<HashMap<String, tokio::sync::oneshot::Sender<bool>>>>,
    activity_notify: Arc<Notify>,
    running_activities: Arc<
        RwLock<
            HashMap<
                String,
                Box<(
                    // worker id
                    String,
                    tokio::sync::oneshot::Sender<ActivityResultV1>,
                    RunningProperties<ActivityProperties>,
                )>,
            >,
        >,
    >,
    call_queue: Arc<
        Mutex<
            HashMap<
                String,
                VecDeque<
                    Box<(
                        String,
                        CallOptions,
                        Option<tokio::sync::mpsc::Sender<CallResultV1>>,
                    )>,
                >,
            >,
        >,
    >,
    workflow_notify: Arc<Notify>,
    workflow_queue: Arc<
        Mutex<
            HashMap<
                String,
                VecDeque<
                    Box<(
                        String,
                        ClientStartWorkflowOptionsV1,
                        Option<watch::Sender<i32>>,
                        Option<Vec<ActivityCache>>,
                    )>,
                >,
            >,
        >,
    >,
    activity_queue: Arc<
        Mutex<
            HashMap<
                String,
                VecDeque<
                    Box<(
                        String,
                        RequestStartActivityOptionsV1,
                        tokio::sync::oneshot::Sender<ActivityResultV1>,
                        DateTime<Utc>,
                    )>,
                >,
            >,
        >,
    >,
}

enum AdjustCapacity {
    Workflow,
    Activity,
}

async fn fail_entire_workflow(
    history: &History,
    v1: &mut WorkflowHistory,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    for activity in &mut v1.activities {
        let mut changed = false;
        for run in &mut activity.runs {
            if matches!(run.status, HistoryStatus::Running) {
                run.status = HistoryStatus::Failed("Orphaned".to_string());
                run.end_time = Some(now);
                changed = true;
            }
        }
        if changed {
            let activity_clone = activity.clone();
            history
                .update_activity(&v1.workflow_id, activity_clone)
                .await?;
        }
    }
    v1.status = HistoryStatus::Failed("Orphaned".to_string());
    v1.end_time = Some(now);
    history.update_workflow(&v1.workflow_id, v1.clone()).await?;
    Ok(())
}

async fn rehydrate_activity_if_absent(
    running_activities: &Arc<
        RwLock<
            HashMap<
                String,
                Box<(
                    String,
                    tokio::sync::oneshot::Sender<ActivityResultV1>,
                    RunningProperties<ActivityProperties>,
                )>,
            >,
        >,
    >,
    workers: &Arc<RwLock<HashMap<String, RegisteredWorker>>>,
    worker_id: &str,
    workflow_id: &str,
    activity: &ActivityHistory,
    last_run_start: Option<DateTime<Utc>>,
) {
    let mut running = running_activities.write().await;
    if running.contains_key(&activity.activity_id) {
        return;
    }
    let (tx, _rx) = tokio::sync::oneshot::channel::<ActivityResultV1>();
    let now = Utc::now();
    running.insert(
        activity.activity_id.clone(),
        Box::new((
            worker_id.to_string(),
            tx,
            RunningProperties {
                start: last_run_start.unwrap_or(now),
                timeout: now + Duration::seconds(30),
                max_duration: Duration::seconds(30),
                worker_id: worker_id.to_string(),
                heartbeat_timeout: Duration::seconds(30),
                additional_properties: ActivityProperties {
                    workflow_id: workflow_id.to_string(),
                    last_heartbeat: now,
                    scheduled: last_run_start.unwrap_or(now),
                    latest_run_start: last_run_start,
                },
            },
        )),
    );

    // decrement activity capacity (best-effort)
    ImmortalService::adjust_capacity(
        Arc::clone(workers),
        worker_id.to_string(),
        -1,
        AdjustCapacity::Activity,
    )
    .await;
}

// --- Activity retry policy (tweak as needed) ---
const ACTIVITY_MAX_ATTEMPTS: usize = 3;
const ACTIVITY_BACKOFF_BASE_MS: u64 = 1_000; // 1s
const ACTIVITY_BACKOFF_FACTOR: u64 = 2;
const ACTIVITY_BACKOFF_JITTER_MS: u64 = 250;

// Build RequestStartActivityOptionsV1 from history for a retry.
// We infer task_queue from the workflow (common case). If your activities
// use a different queue, swap this to your preferred source.
async fn build_retry_activity_options(
    history: &History,
    workflow_id: &str,
    activity: &history::ActivityHistory,
) -> anyhow::Result<immortal::RequestStartActivityOptionsV1> {
    // use immortal_lib::common::Payload;

    // Grab the workflow to infer its task_queue
    let wf = history
        .get_workflow(workflow_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("workflow not found for retry"))?;

    // WHY THE FUCK WAS I DOING THIS???????
    // Serialize the activity input (it’s stored as OwnedValue in history)
    // let activity_input = if let Some(v) = &activity.input {
    //     let mut tmp = simd_json::to_vec(v)?;
    //     Some(Payload {
    //         data: std::mem::take(&mut tmp),
    //         ..Default::default()
    //     })
    // } else {
    //     None
    // };

    Ok(RequestStartActivityOptionsV1 {
        workflow_id: workflow_id.to_string(),
        activity_id: activity.activity_id.clone(),
        activity_type: activity.activity_type.clone(),
        activity_input: activity.input.clone(),
        // Use the same queue as the workflow by default
        task_queue: wf.task_queue.unwrap_or_else(|| "default".to_string()),
        // Reasonable defaults; override if you persist these in history
        heartbeat_timeout: None,
        schedule_to_start_timeout: None,
        ..Default::default()
    })
}

// Schedule a retry with exponential backoff (+ jitter)
impl ImmortalService {
    fn schedule_activity_retry(
        &self,
        options: immortal::RequestStartActivityOptionsV1,
        next_run_id: String,
        attempt_index: usize, // 0-based (0 => first retry)
        tx: tokio::sync::oneshot::Sender<ActivityResultV1>,
    ) {
        let backoff_ms = ACTIVITY_BACKOFF_BASE_MS
            .saturating_mul(ACTIVITY_BACKOFF_FACTOR.saturating_pow(attempt_index as u32));
        let jitter = rand::rng().random_range(0..=ACTIVITY_BACKOFF_JITTER_MS);
        let delay = std::time::Duration::from_millis(backoff_ms + jitter);

        let activity_queue = Arc::clone(&self.activity_queue);
        let activity_notify = Arc::clone(&self.activity_notify);

        tokio::spawn(async move {
            tokio::time::sleep(delay).await;

            // Enqueue back onto the activity queue
            let now = chrono::Utc::now();
            {
                let mut queues = activity_queue.lock().await;
                match queues.get_mut(&options.task_queue) {
                    Some(q) => {
                        q.push_back(Box::new((next_run_id.clone(), options.clone(), tx, now)))
                    }
                    None => {
                        let mut q = std::collections::VecDeque::new();
                        q.push_back(Box::new((next_run_id.clone(), options.clone(), tx, now)));
                        queues.insert(options.task_queue.clone(), q);
                    }
                }
            }
            activity_notify.notify_one();
        });
    }

    async fn adjust_capacity(
        workers: Arc<RwLock<HashMap<String, RegisteredWorker>>>,
        worker_id: String,
        amount: i32,
        capacity: AdjustCapacity,
    ) {
        let mut workers = workers.write().await;
        if let Some(worker) = workers.iter_mut().find(|f| f.1.worker_id == worker_id) {
            match capacity {
                AdjustCapacity::Workflow => worker.1.workflow_capacity += amount,
                AdjustCapacity::Activity => worker.1.activity_capacity += amount,
            }
        }
    }

    // We need to first check
    // 1) Is the workflow still running on the workers
    // 1.1) If it is, we simply add it back to the running queue
    // 1.2) If it is not, we fail the entire workflow and all activities
    // 2) Once we have determined that the workflow is still running. We need to now check with
    //    activity it is stuck on. We can do an activity check as well.
    // 2.1) If the activity is running, we add it back to the queue
    // 2.2) If the activity is no longer running, depending on the retry policy, we either rerun it
    //   or fail everything

    async fn _continue_workflow(&self, workflow_id: &str) -> anyhow::Result<()> {
        if let Some(workflow) = self.history.get_workflow(workflow_id).await? {
            let activities_cache: Vec<_> = workflow
                .activities
                .iter()
                .map(|f| ActivityCache {
                    input: f.input.clone(),
                    output: f.output.as_ref().map(|x| Payload::new(x)),
                    activity_type: f.activity_type.clone(),
                    task_queue: f.task_queue.clone(),
                })
                .collect();
            self.start_workflow_internal(
                ClientStartWorkflowOptionsVersion {
                    version: Some(client_start_workflow_options_version::Version::V1(
                        ClientStartWorkflowOptionsV1 {
                            workflow_type: workflow.workflow_type.clone(),
                            workflow_version: "V1".to_string(),
                            input: Some(Payloads::new(workflow.args.iter().map(|f| f).collect())),
                            task_queue: workflow.task_queue.clone().unwrap(),
                            workflow_id: Some(workflow_id.to_string()),
                        },
                    )),
                },
                None,
                Some(activities_cache),
            )
            .await?;
        }

        Ok(())
    }

    async fn orphaned_workflows(&self) -> anyhow::Result<()> {
        use tokio::time::{timeout, Duration as TokioDuration};

        let running_activities = Arc::clone(&self.running_activities);
        let orphaned_activities = Arc::clone(&self.orphaned_activities);
        let orphaned_workflows = Arc::clone(&self.orphaned_workflows);
        let workers = Arc::clone(&self.workers);
        let history = self.history.clone();

        // scan recent set; tune page as desired
        let workflows = history
            .get_workflows(Some(500), Some(0), None, None, None)
            .await?;

        for wf in workflows {
            let history::WorkflowHistoryVersion::V1(mut v1) = wf;

            if !matches!(v1.status, HistoryStatus::Running) {
                continue;
            }

            let now = Utc::now();
            let Some(worker_id) = v1.worker_id.clone() else {
                // nothing owns it: fail everything
                fail_entire_workflow(&history, &mut v1, now).await?;
                continue;
            };

            // ensure worker exists & matches queue + can run the workflow type
            let (worker_tx, can_run_workflow) = {
                let guard = workers.read().await;
                if let Some(w) = guard.get(&worker_id) {
                    let queue_ok = v1
                        .task_queue
                        .as_ref()
                        .map(|q| q == &w.task_queue)
                        .unwrap_or(true);
                    let wf_ok = w.registered_workflows.contains_key(&v1.workflow_type);
                    (Some(w.tx.clone()), queue_ok && wf_ok)
                } else {
                    (None, false)
                }
            };

            if worker_tx.is_none() || !can_run_workflow {
                fail_entire_workflow(&history, &mut v1, now).await?;
                continue;
            }

            // Ask the worker if this workflow is actually still running.
            let (tx_wf, rx_wf) = oneshot::channel::<bool>();
            {
                let mut map = orphaned_workflows.write().await;
                map.insert(v1.workflow_id.clone(), tx_wf);
            }

            // fire-and-wait (bounded)
            if let Err(_e) = worker_tx
                .as_ref()
                .unwrap()
                .send(Ok(ImmortalWorkerActionVersion {
                    version: Some(immortal_worker_action_version::Version::V1(
                        ImmortalWorkerActionV1 {
                            action: Some(WorkerAction::CheckWorkflow(v1.workflow_id.clone())),
                        },
                    )),
                }))
                .await
            {
                // can’t ask => consider orphaned
                fail_entire_workflow(&history, &mut v1, now).await?;
                continue;
            }

            let running = timeout(TokioDuration::from_secs(2), rx_wf)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(false);

            if !running {
                // worker said "no" (or timed out) => fail everything
                fail_entire_workflow(&history, &mut v1, now).await?;
                continue;
            }

            // Workflow is running: check each Running activity run.
            for activity in &mut v1.activities {
                // Only check the *latest* running run (if any)
                let maybe_latest_running_idx = activity
                    .runs
                    .iter()
                    .rposition(|r| matches!(r.status, HistoryStatus::Running));

                let Some(idx) = maybe_latest_running_idx else {
                    continue;
                };
                // let run_id = activity.runs[idx].run_id.clone();

                // Ensure worker can run this activity on this queue
                let can_resume_activity = {
                    let guard = workers.read().await;
                    if let Some(w) = guard.get(&worker_id) {
                        v1.task_queue
                            .as_ref()
                            .map(|q| &w.task_queue == q)
                            .unwrap_or(true)
                            && w.registered_activities
                                .contains_key(&activity.activity_type)
                    } else {
                        false
                    }
                };

                if !can_resume_activity {
                    // Mark that run failed
                    activity.runs[idx].status = HistoryStatus::Failed("Orphaned".to_string());
                    activity.runs[idx].end_time = Some(now);
                    let cloned = activity.clone();
                    history.update_activity(&v1.workflow_id, cloned).await?;
                    continue;
                }

                // Ask the worker about the activity
                let (tx_act, rx_act) = oneshot::channel::<bool>();
                {
                    let mut map = orphaned_activities.write().await;
                    map.insert(activity.activity_id.clone(), tx_act);
                }

                if let Err(_e) = worker_tx
                    .as_ref()
                    .unwrap()
                    .send(Ok(ImmortalWorkerActionVersion {
                        version: Some(immortal_worker_action_version::Version::V1(
                            ImmortalWorkerActionV1 {
                                action: Some(WorkerAction::CheckActivity(
                                    activity.activity_id.clone(),
                                )),
                            },
                        )),
                    }))
                    .await
                {
                    // treat as not running
                    activity.runs[idx].status = HistoryStatus::Failed("Orphaned".to_string());
                    activity.runs[idx].end_time = Some(now);
                    let cloned = activity.clone();
                    history.update_activity(&v1.workflow_id, cloned).await?;
                    continue;
                }

                let act_running = timeout(TokioDuration::from_secs(2), rx_act)
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or(false);

                if act_running {
                    // rehydrate into memory so watchdog & logs update heartbeats again
                    let last_run_start = activity.runs[idx].start_time;
                    rehydrate_activity_if_absent(
                        &running_activities,
                        &workers,
                        &worker_id,
                        &v1.workflow_id,
                        activity,
                        Some(last_run_start),
                    )
                    .await;
                } else {
                    // mark failed
                    activity.runs[idx].status = HistoryStatus::Failed("Orphaned".to_string());
                    activity.runs[idx].end_time = Some(now);
                    let cloned = activity.clone();
                    history.update_activity(&v1.workflow_id, cloned).await?;
                }
            }

            // If *all* activities ended up not running anymore, it’s safer to fail the workflow too.
            let any_running_left = v1.activities.iter().any(|a| {
                a.runs
                    .iter()
                    .any(|r| matches!(r.status, HistoryStatus::Running))
            });

            if !any_running_left {
                v1.status = HistoryStatus::Failed("Orphaned".to_string());
                v1.end_time = Some(now);
                history.update_workflow(&v1.workflow_id, v1.clone()).await?;
            }
        }

        Ok(())
    }
    async fn kill_workflow(&self, workflow_id: &str) -> anyhow::Result<()> {
        if let Some(workflow) = self.history.get_workflow(workflow_id).await? {
            if let Some(worker_id) = workflow.worker_id {
                {
                    let running_activities_stripped: Vec<_>;
                    {
                        let running_activities = self.running_activities.read().await;

                        running_activities_stripped = running_activities
                            .iter()
                            .map(|f| {
                                (
                                    f.0.clone(),
                                    f.1 .2.additional_properties.workflow_id.clone(),
                                )
                            })
                            .collect();
                    }
                    for (activity_id, w_id) in running_activities_stripped {
                        if w_id == workflow_id {
                            println!("killing activity");
                            self.kill_activity(&activity_id).await?;
                        }
                    }
                }
                {
                    let workers = self.workers.read().await;
                    if let Some(worker) = workers.get(&worker_id) {
                        println!("killing workflow");
                        worker
                            .tx
                            .send(Ok(ImmortalWorkerActionVersion {
                                version: Some(immortal_worker_action_version::Version::V1(
                                    ImmortalWorkerActionV1 {
                                        action: Some(WorkerAction::KillWorkflow(
                                            workflow_id.to_string(),
                                        )),
                                    },
                                )),
                            }))
                            .await?;
                    }
                }
            }
            // let running_workflows = self.execute_workflow
        }
        Ok(())
    }
    async fn kill_activity(&self, activity_id: &str) -> anyhow::Result<()> {
        {
            let running_activities = self.running_activities.read().await;

            if let Some(running_activity) = running_activities.get(activity_id) {
                let workers = self.workers.read().await;
                if let Some(worker) = workers.get(&running_activity.2.worker_id) {
                    worker
                        .tx
                        .send(Ok(ImmortalWorkerActionVersion {
                            version: Some(immortal_worker_action_version::Version::V1(
                                ImmortalWorkerActionV1 {
                                    action: Some(WorkerAction::KillActivity(
                                        activity_id.to_string(),
                                    )),
                                },
                            )),
                        }))
                        .await?;
                }
            }
        }
        Ok(())
    }
    fn watchdog(&self) {
        let running_calls = Arc::clone(&self.running_calls);
        let running_activities = Arc::clone(&self.running_activities);
        let workers = Arc::clone(&self.workers);
        tokio::spawn(async move {
            loop {
                {
                    let mut activities_to_remove = vec![];

                    for (id, running_activity) in running_activities.read().await.iter() {
                        let now = Utc::now();
                        let max_time = running_activity.2.additional_properties.last_heartbeat
                            + running_activity.2.heartbeat_timeout;
                        if now > max_time {
                            let available_worker = {
                                let workers = workers.read().await;
                                workers
                                    .get(&running_activity.2.worker_id)
                                    .map(|worker| (worker.worker_id.clone(), worker.tx.clone()))
                            };
                            if let Some(worker) = available_worker {
                                if let Err(e) = worker
                                    .1
                                    .send(Ok(ImmortalWorkerActionVersion {
                                        version: Some(immortal_worker_action_version::Version::V1(
                                            ImmortalWorkerActionV1 {
                                                action: Some(WorkerAction::KillActivity(
                                                    id.clone(),
                                                )),
                                            },
                                        )),
                                    }))
                                    .await
                                {
                                    println!("{:#?}", e);

                                    //running_calls.write().await.remove(id);
                                }
                                println!("killing activity");
                                activities_to_remove.push(id.clone());
                            }
                            // kill it
                        }
                    }
                    // I will temporarily remove this because I only want to remove activities from
                    // the list once the worker confirmed that it has been killed. This might cause
                    // issues I am not sure yet.
                    // I also need to watch out with this. Because in the case of a deployment, if
                    // we have another worker that joins, tries to grab the same name, the server
                    // will think that this is the same worker, and not the old worker
                    // reconnecting.
                    // if activities_to_remove.len() > 0 {
                    //     let mut running_activities = running_activities.write().await;
                    //     for activity_to_remove in activities_to_remove {
                    //         if let Some(running_activity) =
                    //             running_activities.remove(&activity_to_remove)
                    //         {
                    //             let (_worker_id, tx, props) = *running_activity;
                    //             // this is also weird that this does not work. This should
                    //             // technically remove the activity. Unless the worker cannot find
                    //             // it inside reunning activities
                    //             if let Err(e) = tx.send(ActivityResultV1 {
                    //                 activity_id: activity_to_remove.clone(),
                    //
                    //                 workflow_id: props.additional_properties.workflow_id.clone(),
                    //                 activity_run_id: "0".to_string(),
                    //
                    //                 status: Some(immortal::activity_result_v1::Status::Failed(
                    //                     immortal::Failure {
                    //                         failure: Some(failure::Failure {
                    //                             message: "timeout".to_string(),
                    //                             ..Default::default()
                    //                         }),
                    //                     },
                    //                 )),
                    //             }) {
                    //                 println!("{:#?}", e);
                    //             }
                    //         }
                    //     }
                    // }
                    for (id, running_call) in running_calls.read().await.clone().iter() {
                        let now = Utc::now();
                        let max_time = running_call.1.timeout;
                        if now > max_time {
                            let available_worker = {
                                let workers = workers.read().await;
                                workers
                                    .get(&running_call.1.worker_id)
                                    .map(|worker| (worker.worker_id.clone(), worker.tx.clone()))
                            };
                            if let Some(worker) = available_worker {
                                if let Err(e) = worker
                                    .1
                                    .send(Ok(ImmortalWorkerActionVersion {
                                        version: Some(immortal_worker_action_version::Version::V1(
                                            ImmortalWorkerActionV1 {
                                                action: Some(WorkerAction::KillCall(id.clone())),
                                            },
                                        )),
                                    }))
                                    .await
                                {
                                    println!("{:#?}", e);
                                }

                                running_calls.write().await.remove(id);
                            }
                            // kill it
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });
    }
    pub fn call_queue_thread(&self) {
        let call_queue = Arc::clone(&self.call_queue);
        let running_calls = Arc::clone(&self.running_calls);
        let workers = Arc::clone(&self.workers);
        let notify = self.call_notify.clone();
        tokio::spawn(async move {
            loop {
                notify.notified().await;
                println!("running call queue");
                // Lock once and take a snapshot of queues
                let queues_snapshot: HashMap<String, _> = {
                    let call_queues = call_queue.lock().await;
                    call_queues
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                };

                // min: 1
                // max: 100
                // ratio: for every 10 tasks + 1 worker
                // interval: 10 mins
                println!("got snapshot");
                for (queue_name, queue) in queues_snapshot {
                    if queue.is_empty() {
                        println!("queue is empty so continuing");
                        continue;
                    }

                    // Try to assign one call from this queue
                    for (_index, queued_item) in queue.into_iter().enumerate() {
                        let (call_id, call_options, sender) = *queued_item;
                        //println!("executing {call_id}");
                        // Find eligible workers

                        if let Some(sender) = &sender {
                            if sender.is_closed() {
                                let mut call_queues = call_queue.lock().await;
                                if let Some(queue_vec) = call_queues.get_mut(&queue_name) {
                                    if let Some(pos) =
                                        queue_vec.iter().position(|x| (**x).0 == call_id)
                                    {
                                        queue_vec.remove(pos);
                                    }
                                    if queue_vec.is_empty() {
                                        call_queues.remove(&queue_name);
                                    }
                                }
                            }
                        }

                        let available_workers: Vec<_>;

                        {
                            let workers_guard = workers.read().await;

                            available_workers = workers_guard
                                .iter()
                                .filter(|(_, worker)| worker.task_queue == call_options.task_queue)
                                .map(|(_, worker)| (worker.worker_id.clone(), worker.tx.clone()))
                                .collect();
                        }

                        if available_workers.is_empty() {
                            break;
                        }

                        let chosen_worker_index =
                            rand::rng().random_range(0..available_workers.len());

                        if let Some(worker) = available_workers.get(chosen_worker_index) {
                            // Dispatch call to worker
                            if let Err(e) = worker
                                .1
                                .send(Ok(ImmortalWorkerActionVersion {
                                    version: Some(immortal_worker_action_version::Version::V1(
                                        ImmortalWorkerActionV1 {
                                            action: Some(WorkerAction::StartCall(
                                                immortal::StartCallOptionsV1 {
                                                    call_run_id: "0".to_string(),
                                                    call_id: call_id.clone(),
                                                    call_type: call_options.call_type.clone(),
                                                    call_input: call_options.input,
                                                },
                                            )),
                                        },
                                    )),
                                }))
                                .await
                            {
                                if available_workers.len() > 1 {
                                    notify.notify_one();
                                }
                                // match e.0 {
                                //    Ok(_) => {
                                //
                                //    },
                                //    Err(e) => match e {
                                //        Status
                                //
                                //    }
                                //
                                // }
                                eprintln!("Failed to send call to worker {}: {:#?}", worker.0, e);
                                //TODO: need to go ahead, check the error, and then act accordingly
                                println!("{:#?}", available_workers);
                            } else {
                                // Remove the item from the actual call queue (not the snapshot)
                                {
                                    let mut call_queues = call_queue.lock().await;
                                    if let Some(queue_vec) = call_queues.get_mut(&queue_name) {
                                        if let Some(pos) =
                                            queue_vec.iter().position(|x| (**x).0 == call_id)
                                        {
                                            queue_vec.remove(pos);
                                        }
                                        if queue_vec.is_empty() {
                                            call_queues.remove(&queue_name);
                                        }
                                    }
                                }

                                // Register running call
                                {
                                    let mut running_calls = running_calls.write().await;
                                    let now = Utc::now();
                                    let timeout = now + Duration::seconds(30);
                                    running_calls.insert(
                                        call_id.clone(),
                                        Box::new((
                                            sender,
                                            RunningProperties {
                                                start: now.clone(),
                                                timeout,
                                                max_duration: Duration::seconds(30),
                                                worker_id: worker.0.clone(),
                                                heartbeat_timeout: Duration::seconds(30),
                                                additional_properties: CallProperties {
                                                    last_heartbeat: now,
                                                },
                                            },
                                        )),
                                    );
                                }
                            }

                            //break; // Assign one call per loop per queue
                        } else {
                            println!("no available workers");
                        }
                    }
                }
                println!("finished running loop");
            }
        });
    }
    pub fn activity_queue_thread(&self) {
        let activity_queue = Arc::clone(&self.activity_queue);
        let running_activities = Arc::clone(&self.running_activities);
        let notification_tx = Arc::clone(&self.notification_tx);
        let workers = Arc::clone(&self.workers);
        let history = self.history.clone();

        let notify = self.activity_notify.clone();
        tokio::spawn(async move {
            loop {
                notify.notified().await;
                let mut activity_queues = activity_queue.lock().await;

                for (queue_name, queue) in activity_queues.iter_mut() {
                    if let Some(queued_item) = queue.pop_front() {
                        let (activity_run_id, activity_options, tx, scheduled) = *queued_item;
                        if let Some(schedule_to_start_timeout) =
                            activity_options.schedule_to_start_timeout
                        {
                            let now = Utc::now();
                            let schedule_to_start_timeout: Duration =
                                schedule_to_start_timeout.into();
                            let max_time = scheduled + schedule_to_start_timeout;
                            if now > max_time {
                                if let Err(e) = tx.send(ActivityResultV1 {
                                    activity_id: activity_options.activity_id.clone(),

                                    workflow_id: activity_options.workflow_id.clone(),
                                    activity_run_id: activity_run_id.to_string(),

                                    status: Some(immortal::activity_result_v1::Status::Failed(
                                        immortal::Failure {
                                            failure: Some(failure::Failure {
                                                message: "timeout".to_string(),
                                                ..Default::default()
                                            }),
                                        },
                                    )),
                                }) {
                                    eprintln!("{:#?}", e);
                                }
                                continue;
                            }
                        }
                        let available_workers: Vec<_>;
                        {
                            let workers_guard = workers.read().await;

                            available_workers = workers_guard
                                .iter()
                                .filter(|(_, worker)| {
                                    worker.task_queue == *queue_name
                                        && worker
                                            .registered_activities
                                            .contains_key(&activity_options.activity_type)
                                })
                                .map(|(_, worker)| (worker.worker_id.clone(), worker.tx.clone()))
                                .collect();
                        }

                        if available_workers.is_empty() {
                            queue.push_front(Box::new((
                                activity_run_id,
                                activity_options,
                                tx,
                                scheduled,
                            )));
                            continue;
                        }

                        let random_index = rand::rng().random_range(0..available_workers.len());

                        if let Some(worker) = available_workers.get(random_index) {
                            let now = Utc::now();
                            let duration = Duration::seconds(30);
                            let timeout = now + duration;

                            running_activities.write().await.insert(
                                activity_options.activity_id.clone(),
                                Box::new((
                                    worker.0.clone(),
                                    tx,
                                    RunningProperties {
                                        start: now,
                                        timeout,
                                        max_duration: duration,
                                        worker_id: worker.0.clone(),
                                        heartbeat_timeout: activity_options
                                            .heartbeat_timeout
                                            .map(|f| f.into())
                                            .unwrap_or(Duration::seconds(30)),
                                        additional_properties: ActivityProperties {
                                            workflow_id: activity_options.workflow_id.clone(),
                                            last_heartbeat: now,
                                            scheduled: now.clone(),
                                            latest_run_start: None,
                                        },
                                    },
                                )),
                            );

                            let mut activity_history = ActivityHistory::new(
                                activity_options.activity_type.clone(),
                                activity_options.activity_id.clone(),
                                activity_options
                                    .activity_input
                                    .clone()
                                    .map(|mut i| simd_json::from_slice(&mut i.data).ok())
                                    .unwrap_or_default(),
                                activity_options.task_queue.clone(),
                                activity_options.activity_input.clone(),
                            );
                            activity_history
                                .runs
                                .push(ActivityRun::new("0".to_string()));

                            match history
                                .get_activity(
                                    &activity_options.workflow_id,
                                    &activity_options.activity_id,
                                )
                                .await
                            {
                                Ok(Some(mut existing)) => {
                                    existing
                                        .runs
                                        .push(ActivityRun::new(activity_run_id.clone()));
                                    if let Err(e) = history
                                        .update_activity(
                                            &activity_options.workflow_id,
                                            existing.clone(),
                                        )
                                        .await
                                    {
                                        eprintln!(
                                            "Failed to append run to existing activity: {:?}",
                                            e
                                        );
                                    }
                                    activity_history = existing;
                                }
                                _ => {
                                    // first ever run for this activity id
                                    let mut activity_history = ActivityHistory::new(
                                        activity_options.activity_type.clone(),
                                        activity_options.activity_id.clone(),
                                        activity_options
                                            .activity_input
                                            .clone()
                                            .map(|mut i| simd_json::from_slice(&mut i.data).ok())
                                            .unwrap_or_default(),
                                        activity_options.task_queue.clone(),
                                        activity_options.activity_input.clone(),
                                    );
                                    activity_history
                                        .runs
                                        .push(ActivityRun::new(activity_run_id.clone()));
                                    if let Err(e) = history
                                        .add_activity(
                                            &activity_options.workflow_id,
                                            activity_history,
                                        )
                                        .await
                                    {
                                        eprintln!("Failed to add activity to history: {:?}", e);
                                    }
                                }
                            }
                            println!("added activity history");
                            if let Err(e) = worker
                                .1
                                .send(Ok(ImmortalWorkerActionVersion {
                                    version: Some(immortal_worker_action_version::Version::V1(
                                        ImmortalWorkerActionV1 {
                                            action: Some(WorkerAction::StartActivity(
                                                immortal::StartActivityOptionsV1 {
                                                    activity_id: activity_options
                                                        .activity_id
                                                        .clone(),
                                                    activity_type: activity_options
                                                        .activity_type
                                                        .clone(),
                                                    activity_input: activity_options.activity_input,
                                                    workflow_id: activity_options
                                                        .workflow_id
                                                        .clone(),
                                                    activity_run_id,
                                                },
                                            )),
                                        },
                                    )),
                                }))
                                .await
                            {
                                eprintln!("Failed to send StartActivity to worker: {:#?}", e);
                                continue;
                            }

                            Self::adjust_capacity(
                                Arc::clone(&workers),
                                worker.0.clone(),
                                -1,
                                AdjustCapacity::Activity,
                            )
                            .await;

                            if let Err(e) = notification_tx.send(Notification::ActivityRunStarted(
                                Uuid::parse_str(&activity_options.workflow_id).unwrap_or_default(),
                                activity_history,
                            )) {
                                eprintln!("Failed to send notification: {:?}", e);
                            }
                        } else {
                            queue.push_front(Box::new((
                                activity_run_id,
                                activity_options,
                                tx,
                                scheduled,
                            )));
                        }
                    }
                }

                activity_queues.retain(|_, q| !q.is_empty());
            }
        });
    }
    pub fn workflow_queue_thread(&self) {
        let workflow_queue = Arc::clone(&self.workflow_queue);
        let workers = Arc::clone(&self.workers);
        let notification_tx = Arc::clone(&self.notification_tx);
        let history = self.history.clone();

        let notify = self.workflow_notify.clone();
        tokio::spawn(async move {
            loop {
                notify.notified().await;

                // Snapshot and convert the queue structure
                let queues_snapshot: HashMap<
                    String,
                    Vec<(String, StartWorkflowOptionsV1, Option<watch::Sender<i32>>)>,
                > = {
                    let queue_guard = workflow_queue.lock().await;
                    queue_guard
                        .iter()
                        .filter(|(_, v)| !v.is_empty())
                        .map(|(queue_name, items)| {
                            let converted_items = items
                                .iter()
                                .map(|item| {
                                    let (id, client_opts, sender, cache) = *(item.clone());
                                    (
                                        id.clone(),
                                        StartWorkflowOptionsV1 {
                                            cache: cache.unwrap_or(vec![]),
                                            // this might be incorrect
                                            workflow_id: id,
                                            workflow_type: client_opts.workflow_type,
                                            workflow_version: client_opts.workflow_version,
                                            task_queue: client_opts.task_queue,
                                            input: client_opts.input,
                                        },
                                        sender.clone(),
                                    )
                                })
                                .collect::<Vec<_>>();
                            (queue_name.clone(), converted_items)
                        })
                        .collect()
                };

                for (queue_name, queue) in queues_snapshot {
                    if queue.is_empty() {
                        continue;
                    }

                    for (workflow_id, workflow_options, sender) in queue {
                        let available_workers: Vec<_>;
                        {
                            let workers_guard = workers.read().await;
                            available_workers = workers_guard
                                .iter()
                                .filter(|(_, worker)| {
                                    worker.task_queue == queue_name
                                        && worker
                                            .registered_workflows
                                            .contains_key(&workflow_options.workflow_type)
                                })
                                .map(|(_, worker)| (worker.worker_id.clone(), worker.tx.clone()))
                                .collect();
                        }

                        if available_workers.is_empty() {
                            println!("No available workers for workflow queue {}", queue_name);
                            break;
                        }

                        let chosen_index = rand::rng().random_range(0..available_workers.len());

                        if let Some(worker) = available_workers.get(chosen_index) {
                            // Remove the item from the actual queue
                            {
                                let mut queue_guard = workflow_queue.lock().await;
                                if let Some(vec) = queue_guard.get_mut(&queue_name) {
                                    if let Some(pos) =
                                        vec.iter().position(|x| (**x).0 == workflow_id)
                                    {
                                        vec.remove(pos);
                                    }
                                    if vec.is_empty() {
                                        queue_guard.remove(&queue_name);
                                    }
                                }
                            }
                            if let Some(tx) = sender {
                                if tx.receiver_count() == 0 {
                                    println!("DROPPING WORKFLOW AS RECEVIER NO LONG EXISTS");
                                    continue;
                                }
                            }

                            // Build and store history
                            let workflow_history = WorkflowHistory::new(
                                workflow_options.workflow_type.clone(),
                                workflow_id.clone(),
                                workflow_options
                                    .input
                                    .clone()
                                    .map(|mut i| {
                                        i.payloads
                                            .iter_mut()
                                            .filter_map(|f| simd_json::from_slice(&mut f.data).ok())
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                                queue_name.clone(),
                                worker.0.clone(),
                            );

                            if let Err(e) = history.add_workflow(workflow_history.clone()).await {
                                eprintln!("Failed to add workflow to history: {:?}", e);
                            }

                            // Send to worker
                            if let Err(e) = worker
                                .1
                                .send(Ok(ImmortalWorkerActionVersion {
                                    version: Some(immortal_worker_action_version::Version::V1(
                                        ImmortalWorkerActionV1 {
                                            action: Some(WorkerAction::StartWorkflow(
                                                StartWorkflowOptionsV1 {
                                                    cache: vec![],
                                                    workflow_id: workflow_id.clone(),
                                                    workflow_type: workflow_options
                                                        .workflow_type
                                                        .clone(),
                                                    workflow_version: workflow_options
                                                        .workflow_version
                                                        .clone(),
                                                    task_queue: workflow_options.task_queue.clone(),
                                                    input: workflow_options.input,
                                                },
                                            )),
                                        },
                                    )),
                                }))
                                .await
                            {
                                eprintln!("Failed to send workflow to worker: {:#?}", e);
                                continue;
                            }
                            Self::adjust_capacity(
                                Arc::clone(&workers),
                                worker.0.clone(),
                                -1,
                                AdjustCapacity::Workflow,
                            )
                            .await;

                            if let Err(e) = notification_tx.send(Notification::WorkflowStarted(
                                Uuid::parse_str(&workflow_id).unwrap_or_default(),
                                workflow_history,
                            )) {
                                eprintln!("Failed to send workflow notification: {:?}", e);
                            }

                            //break;
                        }
                    }
                }
                println!("DONE");
            }
        });
    }

    pub async fn start_activity_internal(
        &self,
        call_options: CallVersion,
    ) -> anyhow::Result<CallResultVersion> {
        match call_options.version {
            Some(call_version::Version::V1(call)) => {
                let (tx, mut rx) = mpsc::channel::<CallResultV1>(100);

                {
                    let mut queue = self.call_queue.lock().await;
                    match queue.get_mut(&call.call_type) {
                        Some(queue) => {
                            queue.push_back(Box::new((
                                Uuid::new_v4().to_string(),
                                CallOptions {
                                    call_type: call.call_type.clone(),
                                    input: call.input,
                                    task_queue: call.task_queue.clone(),
                                },
                                Some(tx),
                            )));
                        }
                        None => {
                            let mut queue2 = VecDeque::new();
                            queue2.push_back(Box::new((
                                Uuid::new_v4().to_string(),
                                CallOptions {
                                    call_type: call.call_type.clone(),
                                    input: call.input,
                                    task_queue: call.task_queue.clone(),
                                },
                                Some(tx),
                            )));
                            queue.insert(call.call_type.clone(), queue2);
                        }
                    }
                }

                self.call_notify.notify_one();
                match rx.recv().await {
                    Some(payload) => Ok(CallResultVersion {
                        version: Some(call_result_version::Version::V1(payload)),
                    }),
                    None => Err(anyhow::anyhow!("Call failed")),
                }
            }
            _ => Err(anyhow::anyhow!("unsupported version")),
        }
    }

    pub async fn start_workflow_internal(
        &self,
        workflow_options: ClientStartWorkflowOptionsVersion,
        sender: Option<watch::Sender<i32>>,
        activity_cache: Option<Vec<ActivityCache>>,
    ) -> Result<String, Status> {
        Ok(match workflow_options.version {
            Some(client_start_workflow_options_version::Version::V1(workflow_options)) => {
                let workflow_id = workflow_options
                    .workflow_id
                    .clone()
                    .unwrap_or(Uuid::new_v4().to_string());
                let mut wq = self.workflow_queue.lock().await;

                match wq.get_mut(&workflow_options.task_queue) {
                    Some(queue) => {
                        queue.push_back(Box::new((
                            workflow_id.clone(),
                            workflow_options.clone(),
                            sender,
                            activity_cache,
                        )));
                    }
                    None => {
                        let mut queue = VecDeque::new();
                        queue.push_back(Box::new((
                            workflow_id.clone(),
                            workflow_options.clone(),
                            sender,
                            activity_cache,
                        )));
                        wq.insert(workflow_options.task_queue.clone(), queue);
                    }
                }
                self.workflow_notify.notify_one();
                workflow_id
            }
            _ => {
                return Err(Status::internal("unsupported version"));
            }
        })
    }
}

fn matches_any(patterns: &[String], input: &str) -> bool {
    for pattern in patterns {
        let re = Regex::new(pattern).expect("Invalid regex pattern");
        if re.is_match(input) {
            return true;
        }
    }
    false
}

#[tonic::async_trait]
impl Immortal for ImmortalService {
    type RegisterWorkerStream = ReceiverStream<Result<ImmortalWorkerActionVersion, Status>>;

    async fn call_async(&self, request: Request<CallVersion>) -> Result<Response<()>, Status> {
        match request.into_inner().version {
            Some(call_version::Version::V1(call)) => {
                // let (tx, _) = broadcast::channel::<CallResultV1>(100);

                {
                    let mut queue = self.call_queue.lock().await;
                    match queue.get_mut(&call.call_type) {
                        Some(queue) => {
                            let call_options = CallOptions {
                                call_type: call.call_type.clone(),
                                input: call.input,
                                task_queue: call.task_queue.clone(),
                            };
                            // check if call is already in the queue and if it is stackable
                            if call.stackable.unwrap_or(false) {
                                let existing_calls_in_queue =
                                    queue.iter().map(|f| f.1.clone()).collect::<Vec<_>>();

                                for existing_call_in_queue in existing_calls_in_queue {
                                    if existing_call_in_queue == call_options {
                                        self.call_notify.notify_one();
                                        return Ok(Response::new(()));
                                    }
                                }
                            }

                            queue.push_back(Box::new((
                                Uuid::new_v4().to_string(),
                                call_options,
                                None,
                            )));
                        }
                        None => {
                            let mut queue2 = VecDeque::new();
                            queue2.push_back(Box::new((
                                Uuid::new_v4().to_string(),
                                CallOptions {
                                    call_type: call.call_type.clone(),
                                    input: call.input,
                                    task_queue: call.task_queue.clone(),
                                },
                                None,
                            )));
                            queue.insert(call.call_type.clone(), queue2);
                        }
                    }
                }

                self.call_notify.notify_one();
                Ok(Response::new(()))
                // queue.get_mut(&call.call_type).unwrap().push_back((
                //     Uuid::new_v4().to_string(),
                //     call.clone(),
                //     tx,
                // ));
                // match rx.recv().await {
                //     Ok(payload) => Ok(Response::new(CallResultVersion {
                //         version: Some(call_result_version::Version::V1(payload)),
                //     })),
                //     Err(_) => Err(Status::internal("Call failed")),
                // }
            }
            _ => Err(Status::internal("unsupported version")),
        }
    }
    async fn call(
        &self,
        request: Request<CallVersion>,
    ) -> Result<Response<CallResultVersion>, Status> {
        match request.into_inner().version {
            Some(call_version::Version::V1(call)) => {
                let mut rx = {
                    let mut queue = self.call_queue.lock().await;
                    match queue.get_mut(&call.call_type) {
                        Some(queue) => {
                            let (tx, rx) = mpsc::channel::<CallResultV1>(10);
                            // need to switch from mpsc to broadcast
                            // let existing_calls_in_queue =
                            //     queue.iter().map(|f| f.1.clone()).collect::<Vec<_>>();
                            let call_options = CallOptions {
                                call_type: call.call_type.clone(),
                                input: call.input,
                                task_queue: call.task_queue.clone(),
                            };
                            // for existing_call_in_queue in existing_calls_in_queue {
                            //     if existing_call_in_queue == call_options {
                            //         self.call_notify.notify_one();
                            //         // return Response::new(());
                            //     }
                            // }
                            queue.push_back(Box::new((
                                Uuid::new_v4().to_string(),
                                call_options,
                                Some(tx),
                            )));
                            rx
                        }
                        None => {
                            let (tx, rx) = mpsc::channel::<CallResultV1>(10);
                            let mut queue2 = VecDeque::new();
                            queue2.push_back(Box::new((
                                Uuid::new_v4().to_string(),
                                CallOptions {
                                    call_type: call.call_type.clone(),
                                    input: call.input,
                                    task_queue: call.task_queue.clone(),
                                },
                                Some(tx),
                            )));
                            queue.insert(call.call_type.clone(), queue2);
                            rx
                        }
                    }
                };

                self.call_notify.notify_one();
                // queue.get_mut(&call.call_type).unwrap().push_back((
                //     Uuid::new_v4().to_string(),
                //     call.clone(),
                //     tx,
                // ));
                match rx.recv().await {
                    Some(payload) => Ok(Response::new(CallResultVersion {
                        version: Some(call_result_version::Version::V1(payload)),
                    })),
                    None => Err(Status::internal("Call failed")),
                }
            }
            _ => Err(Status::internal("unsupported version")),
        }
    }
    async fn notify(&self, request: Request<NotifyVersion>) -> Result<Response<()>, Status> {
        {
            if let Some(version) = request.into_inner().version {
                match version {
                    notify_version::Version::V1(v1) => {
                        let workers = self.workers.read().await;
                        let workers_to_notify = workers
                            .iter()
                            .filter(|(_, worker)| matches_any(&v1.task_queues, &worker.task_queue))
                            .map(|(_, worker)| worker)
                            .collect::<Vec<_>>();
                        for worker in workers_to_notify {
                            if let Err(e) = worker
                                .tx
                                .send(Ok(ImmortalWorkerActionVersion {
                                    version: Some(immortal_worker_action_version::Version::V1(
                                        ImmortalWorkerActionV1 {
                                            action: Some(WorkerAction::Notify(
                                                StartNotificationOptionsV1 {
                                                    notification_id: Uuid::new_v4().to_string(),
                                                    notification_type: v1.notify_type.clone(),
                                                    notification_input: v1.input.clone(),
                                                },
                                            )),
                                        },
                                    )),
                                }))
                                .await
                            {
                                eprintln!("Failed to send workflow notification: {:?}", e);
                            }
                        }
                    }
                }
            }
        }
        Ok(Response::new(()))
    }
    async fn register_worker(
        &self,
        request: Request<Streaming<ImmortalServerActionVersion>>,
    ) -> Result<Response<Self::RegisterWorkerStream>, Status> {
        println!("received worker register call");
        let mut stream = request.into_inner();
        let mut worker_details = None;
        if let Some(Ok(action)) = stream.next().await {
            match action.version {
                Some(immortal_server_action_version::Version::V1(x)) => match x.action {
                    Some(immortal_server_action_v1::Action::RegisterWorker(wd)) => {
                        worker_details = Some(wd);
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        println!("received worker details");
        let redis_pool = self.redis_pool.clone(); // clone the pool handle before spawning

        let metrics_stream = self.metrics_stream.clone();

        let mut worker_id;
        let mut worker_details = worker_details
            .clone()
            .ok_or(tonic::Status::invalid_argument(
                "Worker details never provided",
            ))?;
        {
            // sometimes immortal freezes here, not sure why
            println!("waiting to receive workers write handle");
            let workers = self.workers.read().await;

            worker_id = worker_details.worker_id.clone();
            let worker_ids = workers.iter().map(|f| f.0.clone()).collect::<Vec<_>>();

            if worker_ids.contains(&worker_details.worker_id) {
                worker_details.worker_id =
                    format!("{}-{}", worker_details.worker_id, Uuid::new_v4());
                worker_id = worker_details.worker_id.clone();
            }
        }
        let worker_id2 = worker_id.clone();
        let running_activities = Arc::clone(&self.running_activities);
        let orphaned_workflows = Arc::clone(&self.orphaned_workflows);
        let orphaned_activities = Arc::clone(&self.orphaned_activities);
        // let (metrics_stream, _) = broadcast::channel(10);
        // let metrics_stream_sender = metrics_stream.clone();
        let handle = tokio::spawn(async move {
            while let Some(Ok(action)) = stream.next().await {
                match action.version {
                    Some(immortal_server_action_version::Version::V1(x)) => match x.action {
                        Some(immortal_server_action_v1::Action::Metrics(metrics)) => {
                            metrics_stream
                                .send(IdentifiableMetrics {
                                    worker_id: worker_id2.clone(),
                                    cpu_pct: metrics.cput_pct,
                                    mem_used: metrics.mem_used,
                                    mem_total: metrics.mem_total,
                                })
                                .ok();
                        }
                        Some(immortal_server_action_v1::Action::CheckActivity(check_activity)) => {
                            // Expect the worker to send back { activity_id, running } (bool)
                            // If your proto uses different field names, adjust here.
                            let running = check_activity.running;
                            let id = check_activity.activity_id.clone();
                            let mut map = orphaned_activities.write().await;
                            if let Some(sender) = map.remove(&id) {
                                let _ = sender.send(running);
                            }
                        }
                        Some(immortal_server_action_v1::Action::CheckWorkflow(check_workflow)) => {
                            // Expect the worker to send back { workflow_id, running } (bool)
                            let running = check_workflow.running;
                            let id = check_workflow.workflow_id.clone();
                            let mut map = orphaned_workflows.write().await;
                            if let Some(sender) = map.remove(&id) {
                                let _ = sender.send(running);
                            }
                        }
                        Some(immortal_server_action_v1::Action::LogEvent(mut log)) => {
                            if let Some(when_dt) = DateTime::from_timestamp(log.when, 0) {
                                let when = when_dt.to_string();

                                let level = match log.level() {
                                    immortal::Level::Info => "info",
                                    immortal::Level::Warn => "warn",
                                    immortal::Level::Error => "error",
                                    immortal::Level::Debug => "debug",
                                    immortal::Level::Trace => "trace",
                                }
                                .to_string();
                                let mut items = vec![
                                    ("message", &log.message),
                                    ("when", &when),
                                    ("level", &level),
                                ];
                                let metadata;
                                if let Some(ref mut x) = &mut log.metadata {
                                    match simd_json::from_slice::<OwnedValue>(x) {
                                        Ok(json_data) => match simd_json::to_string(&json_data) {
                                            Ok(meta_str) => {
                                                metadata = meta_str;
                                                items.push(("metadata", &metadata));
                                            }
                                            Err(e) => eprintln!("Error serializing JSON: {}", e),
                                        },
                                        Err(e) => eprintln!("Error parsing metadata: {}", e),
                                    }
                                }
                                match log.activity_id.as_ref() {
                                    Some(activity_id) => {
                                        let mut running_activities =
                                            running_activities.write().await;
                                        if let Some(running_activity) =
                                            running_activities.get_mut(activity_id)
                                        {
                                            running_activity
                                                .2
                                                .additional_properties
                                                .last_heartbeat = when_dt.clone();
                                        }
                                        items.push(("activity_id", activity_id));
                                    }
                                    None => {}
                                }
                                match log.activity_run_id.as_ref() {
                                    Some(activity_run_id) => {
                                        items.push(("activity_run_id", activity_run_id));
                                    }
                                    None => {}
                                }

                                match redis_pool.get().await {
                                    Ok(mut con) => {
                                        let key = format!("immortal:logs:{}", log.workflow_id);
                                        if let Err(e) = con
                                            .xadd_maxlen::<_, &str, &str, _, ()>(
                                                key,
                                                StreamMaxlen::Approx(1000),
                                                "*",
                                                &items,
                                            )
                                            .await
                                        {
                                            eprintln!("Error appending to logs: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("Error getting Redis connection: {}", e);
                                    }
                                }
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            println!("incoming Stream ended");
        });

        let (tx, rx) = mpsc::channel(100);

        {
            let mut workers = self.workers.write().await;

            let worker_ids = workers.iter().map(|f| f.0.clone()).collect::<Vec<_>>();
            // println!("WORKER IDS: {:#?}", worker_ids);
            let registered_workflows = worker_details
                .registered_workflows
                .iter_mut()
                .map(|x| {
                    (
                        x.workflow_type.clone(),
                        WfSchema {
                            args: simd_json::from_slice(&mut x.args).unwrap(),
                            output: simd_json::from_slice(&mut x.output).unwrap(),
                        },
                    )
                })
                .collect();
            let registered_activities = worker_details
                .registered_activities
                .iter_mut()
                .map(|x| {
                    (
                        x.activity_type.clone(),
                        ActivitySchema {
                            args: simd_json::from_slice(&mut x.args).unwrap(),
                            output: simd_json::from_slice(&mut x.output).unwrap(),
                        },
                    )
                })
                .collect();
            let registered_calls = worker_details
                .registered_calls
                .iter_mut()
                .map(|x| {
                    (
                        x.call_type.clone(),
                        CallSchema {
                            args: simd_json::from_slice(&mut x.args).unwrap(),
                            output: simd_json::from_slice(&mut x.output).unwrap(),
                        },
                    )
                })
                .collect();
            workers.insert(
                worker_details.worker_id.clone(),
                RegisteredWorker {
                    // metrics_stream,
                    registered_on: Utc::now(),
                    activity_capacity: worker_details.activity_capacity,
                    task_queue: worker_details.task_queue,
                    workflow_capacity: worker_details.workflow_capacity,
                    _incoming: handle,
                    tx: tx.clone(),
                    worker_id: worker_details.worker_id.clone(),
                    registered_workflows,
                    registered_activities,
                    _registered_calls: registered_calls,
                    max_activity_capacity: worker_details.activity_capacity,
                    max_workflow_capacity: worker_details.workflow_capacity,
                },
            );
        }

        let workers = Arc::clone(&self.workers);
        self.call_notify.notify_one();
        self.workflow_notify.notify_one();
        self.activity_notify.notify_one();
        let _ = self
            .notification_tx
            .send(Notification::WorkerAdded(worker_details.worker_id.clone()));

        let notification_tx = self.notification_tx.clone();
        // let features = self.features.clone();

        tokio::spawn(async move {
            loop {
                let action = ImmortalWorkerActionVersion {
                    version: Some(immortal_worker_action_version::Version::V1(
                        ImmortalWorkerActionV1 {
                            action: Some(WorkerAction::Heartbeat(0)),
                        },
                    )),
                };
                match tx.send(Ok(action)).await {
                    Ok(_) => {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                    Err(_) => {
                        break;
                    }
                }
            }

            {
                let mut workers = workers.write().await;
                let x = workers.remove(&worker_id);
                let _ = notification_tx.send(Notification::WorkerRemoved(worker_id.clone()));
                if let Some(x) = x {
                    println!("Stream ended and removed {:?}", x.worker_id);
                } else {
                    error!("Stream ended NO WORKER REMOVED {}", worker_id);
                    println!("Stream ended NO WORKER REMOVED {}", worker_id);
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn execute_workflow(
        &self,
        request: Request<ClientStartWorkflowOptionsVersion>,
    ) -> Result<Response<WorkflowResultVersion>, Status> {
        let workflow_options = request.into_inner();
        let mut rx = self.notification_rx.resubscribe();
        let (tx, _rx) = watch::channel::<i32>(0);
        let workflow_id = Uuid::parse_str(
            &self
                .start_workflow_internal(workflow_options, Some(tx), None)
                .await?,
        )
        .map_err(|e| tonic::Status::invalid_argument(format!("Invalid UUID: {}", e)))?;
        println!("executed workflow {workflow_id}");
        loop {
            match &rx.recv().await {
                Ok(x) => match x {
                    Notification::WorkflowResult(id, result) => {
                        if *id == workflow_id {
                            println!("workflow completed");
                            return Ok(Response::new(result.clone()));
                        }
                    }
                    _ => {}
                },
                Err(_) => {}
            }
        }
    }
    async fn start_workflow(
        &self,
        request: Request<ClientStartWorkflowOptionsVersion>,
    ) -> Result<Response<ClientStartWorkflowResponse>, Status> {
        let workflow_options = request.into_inner();
        let workflow_id = self
            .start_workflow_internal(workflow_options, None, None)
            .await?;

        println!("started workflow: {workflow_id}");
        Ok(Response::new(ClientStartWorkflowResponse { workflow_id }))
    }
    async fn completed_activity(
        &self,
        request: Request<ActivityResultVersion>,
    ) -> Result<Response<()>, Status> {
        let activity_version = request.into_inner();
        // let mut inform_worker = true;
        match activity_version.version {
            Some(activity_result_version::Version::V1(activity_result)) => {
                // Remove the activity from the running map
                // I think what is happening is that tx is being dropped
                let (worker_id, tx, _) = match self
                    .running_activities
                    .write()
                    .await
                    .remove(&activity_result.activity_id)
                {
                    Some(entry) => *entry,
                    None => return Err(Status::not_found("Activity not found")),
                };

                println!("workflow_id {:?}", activity_result.workflow_id);
                println!("activity_id {:?}", activity_result.activity_id);

                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                // Fetch and update activity history
                let activity_opt = self
                    .history
                    .get_activity(&activity_result.workflow_id, &activity_result.activity_id)
                    .await
                    .map_err(|e| {
                        eprintln!("Error fetching activity: {:?}", e);
                        Status::internal("Failed to fetch activity history")
                    })?;

                let mut activity = match activity_opt {
                    Some(a) => a,
                    None => {
                        eprintln!("Activity history not found for completed activity");
                        return Err(Status::not_found("Activity history not found"));
                    }
                };

                // {
                let run = match activity
                    .runs
                    .iter_mut()
                    .find(|f| f.run_id == activity_result.activity_run_id)
                {
                    Some(r) => r,
                    None => {
                        eprintln!(
                            "Run ID {} not found in activity history",
                            activity_result.activity_run_id
                        );
                        return Err(Status::not_found(format!(
                            "Run ID {} not found in activity history",
                            activity_result.activity_run_id
                        )));
                    }
                };

                run.end_time = Some(chrono::Utc::now());

                // println!("{:?}", activity_result.status);

                let mut failed = false;
                match activity_result.status.clone() {
                    Some(immortal::activity_result_v1::Status::Completed(x)) => {
                        match x.result {
                            Some(mut result_data) => {
                                run.status = HistoryStatus::Completed(
                                    simd_json::to_owned_value(&mut result_data.data).unwrap(),
                                );
                            }
                            None => {
                                run.status = HistoryStatus::Failed("Missing result payload".into());
                            }
                        }
                        if let Ok(id) = Uuid::parse_str(&activity_result.workflow_id) {
                            if let Err(e) = self
                                .notification_tx
                                .send(Notification::ActivityRunCompleted(id, activity.clone()))
                            {
                                error!("Error sending ActivityRunCompleted notification: {:?}", e);
                            }
                        }
                    }

                    Some(immortal::activity_result_v1::Status::Failed(x)) => {
                        failed = true;
                        // Mark this run failed in history
                        run.status = HistoryStatus::Failed(format!("{:#?}", x));

                        // Count attempts so far (all runs for this activity)

                        // Send a completion/failed notification for this run (kept as-is)
                        if let Ok(id) = Uuid::parse_str(&activity_result.workflow_id) {
                            if let Err(e) = self
                                .notification_tx
                                .send(Notification::ActivityRunCompleted(id, activity.clone()))
                            {
                                eprintln!(
                                    "Error sending ActivityRunCompleted notification: {:?}",
                                    e
                                );
                            }
                        }
                    }

                    Some(immortal::activity_result_v1::Status::Cancelled(x)) => {
                        run.status = HistoryStatus::Failed(format!("{:#?}", x));
                        // (No retry on cancelled by default; tweak if you want)
                    }

                    None => {
                        run.status = HistoryStatus::Failed("Missing status field".into());
                    }
                }
                // let _ = self.notification_tx.send(Notification::ActivityRunCompleted(
                //     Uuid::parse_str(&workflow_id).unwrap(),
                //     activity.clone(),
                // ));
                // I THINK IT'S THIS MOTHERFUCKER RIGHT HERE OVERWRITING MY ACTIVITY DATA

                let attempts = activity.runs.len(); // includes this failed run
                if let Err(e) = self
                    .history
                    .update_activity(&activity_result.workflow_id, activity.clone())
                    .await
                {
                    println!("error");
                    eprintln!("Failed to update activity history: {:?}", e);
                    return Err(Status::internal("Failed to update activity history"));
                } 

                // I TRIED MOVING THIS HERE SO THAT WE FIRST UPDATE THE RUN WITH A FAILED
                // STATUS AND THEN. IF WE CAN RUN AGAIN. WE WRITE A NEW ATTEMPT
                // I MIGHT HAVE TO CREATE A SYSTEM WHERE WE ALWAYS FETCH THE LATEST REDIS DATA
                // Decide retry
                if failed && attempts < ACTIVITY_MAX_ATTEMPTS {
                    // inform_worker = false;
                    // Persist the failed run before we schedule the retry

                    // Build next options from history
                    match build_retry_activity_options(
                        &self.history,
                        &activity_result.workflow_id,
                        &activity,
                    )
                    .await
                    {
                        Ok(next_opts) => {
                            // Next run_id is attempts as string (previous runs are 0..attempts-1)
                            let next_run_id = attempts.to_string();

                            // Schedule retry with exponential backoff
                            self.schedule_activity_retry(
                                next_opts,
                                next_run_id,
                                attempts - 1, // attempt_index: 0-based for first retry
                                tx,
                            );

                            // (Optional) You can emit a small notification here if you want:
                            // let _ = self.notification_tx.send(Notification::ActivityRunStarted(...));
                        }
                        Err(err) => {
                            eprintln!("Could not build retry options: {:?}", err);
                            // fall through: no retry ⇒ final failure (we already marked run as failed)
                        }
                    }
                } else {
                    // }

                    // if inform_worker {
                    println!("INFORMING WORKER");
                    // Update the worker's activity capacity
                    let mut workers = self.workers.write().await;
                    //this is the root of my problems
                    if let Some(worker) = workers.get_mut(&worker_id) {
                        if worker.activity_capacity < worker.max_activity_capacity {
                            worker.activity_capacity += 1;
                        }

                        // THIS IS WHERE WE TELL THE WORKER THE RESULT OF THE ACTIVITY
                        // IN THIS CASE. WHEN WE RETRY THE ACTIVITY. WE DON'T WANT TO INFORM THE WORKER
                        // JUST YET. I WILL ADD A BOOL CALLED INFORM_WORKER
                        if let Err(e) = tx.send(activity_result) {
                            eprintln!("Failed to send activity result: {:?}", e);
                        }
                    } else {
                        eprintln!("Worker {} not found", worker_id);
                    }
                }

                println!("finished");
            }

            None => {
                return Err(Status::invalid_argument("Missing activity result version"));
            }
        }

        Ok(Response::new(()))
    }
    async fn completed_call(
        &self,
        request: Request<CallResultVersion>,
    ) -> Result<Response<()>, Status> {
        let call_version = request.into_inner();
        match call_version.version {
            Some(call_result_version::Version::V1(call_result)) => {
                let mut running_calls = self.running_calls.write().await;
                // let tx = running_activities.get(&activity_result.activity_id).unwrap();
                match running_calls.remove(&call_result.call_id) {
                    Some(tx) => {
                        if let Some(tx) = tx.0 {
                            // need to watch out for this as it can increase past max

                            match tx.send(call_result.clone()).await {
                                Ok(_) => {}
                                Err(e) => println!("{:#?}", e),
                            }
                        }
                    }
                    None => {
                        return Err(Status::not_found("Activity not found"));
                    }
                }
            }
            _ => {}
        }

        Ok(Response::new(()))
    }
    async fn completed_workflow(
        &self,
        request: Request<WorkflowResultVersion>,
    ) -> Result<Response<()>, Status> {
        let mut workflow_version = request.into_inner();

        let Some(workflow_result_version::Version::V1(ref mut workflow_result)) =
            workflow_version.version
        else {
            return Err(Status::invalid_argument("Missing workflow result version"));
        };

        // give it a time to let activities sync with redis
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        // Fetch workflow history
        let workflow_opt = self
            .history
            .get_workflow(&workflow_result.workflow_id)
            .await
            .map_err(|e| {
                error!("Failed to get workflow history: {:?}", e);
                Status::internal("Failed to get workflow history")
            })?;

        let mut workflow = match workflow_opt {
            Some(wf) => wf,
            None => {
                error!(
                    "Workflow history not found: {}",
                    workflow_result.workflow_id
                );
                return Err(Status::not_found("Workflow history not found"));
            }
        };

        // Update end time
        workflow.end_time = Some(chrono::Utc::now());

        let worker_id = workflow_result.worker_id.clone();
        let workflow_id = workflow_result.workflow_id.clone();
        // Set status and send specific notification
        match &mut workflow_result.status {
            Some(workflow_result_v1::Status::Completed(x)) => {
                match &mut x.result {
                    Some(ref mut result_data) => {
                        let mut x = result_data.data.clone();
                        workflow.status =
                            HistoryStatus::Completed(simd_json::to_owned_value(&mut x).unwrap());
                        // match serde_json::from_slice(&result_data.data) {
                        //     Ok(deserialized) => {
                        //         workflow.status = HistoryStatus::Completed(deserialized);
                        //     }
                        //     Err(e) => {
                        //         println!("Failed to parse result: {:?}", e);
                        //         error!("Failed to deserialize workflow result: {:?}", e);
                        //         workflow.status =
                        //             HistoryStatus::Failed("Invalid result format".into());
                        //     }
                        // }
                    }
                    None => {
                        workflow.status = HistoryStatus::Failed("Missing result payload".into());
                    }
                }

                if let Ok(uuid) = Uuid::parse_str(&workflow_result.workflow_id) {
                    if let Err(e) = self
                        .notification_tx
                        .send(Notification::WorkflowCompleted(uuid, workflow.clone()))
                    {
                        error!("Error sending WorkflowCompleted notification: {:?}", e);
                    }
                }
            }

            Some(workflow_result_v1::Status::Failed(x)) => {
                workflow.status = HistoryStatus::Failed(format!("{:#?}", x));

                if let Ok(uuid) = Uuid::parse_str(&workflow_result.workflow_id) {
                    if let Err(e) = self
                        .notification_tx
                        .send(Notification::WorkflowFailed(uuid, workflow.clone()))
                    {
                        error!("Error sending WorkflowFailed notification: {:?}", e);
                    }
                }
            }

            Some(workflow_result_v1::Status::Cancelled(x)) => {
                workflow.status = HistoryStatus::Failed(format!("{:#?}", x));
            }

            None => {
                workflow.status = HistoryStatus::Failed("Missing status field".into());
            }
        }

        // Increase worker capacity if found
        {
            let mut workers = self.workers.write().await;
            if let Some(worker) = workers.get_mut(&worker_id) {
                if worker.workflow_capacity < worker.max_workflow_capacity {
                    worker.workflow_capacity += 1;
                }
            } else {
                error!(
                    "Worker {} not found when marking workflow complete",
                    workflow_result.worker_id
                );
            }
        }

        let _ = self.notification_tx.send(Notification::WorkflowCompleted(
            Uuid::parse_str(&workflow_id).unwrap(),
            workflow.clone(),
        ));
        // Save updated workflow
        self.history
            .update_workflow(&workflow_id, workflow)
            .await
            .map_err(|e| {
                error!("Failed to update workflow history: {:?}", e);
                Status::internal("Failed to update workflow history")
            })?;

        // Notify workflow result
        match Uuid::parse_str(&workflow_result.workflow_id) {
            Ok(uuid) => {
                if let Err(e) = self
                    .notification_tx
                    .send(Notification::WorkflowResult(uuid, workflow_version))
                {
                    error!("Error sending WorkflowResult notification: {:?}", e);
                }
            }
            Err(e) => {
                error!(
                    "Invalid UUID in workflow_id (result notification): {} ({:?})",
                    workflow_result.workflow_id, e
                );
            }
        }
        println!("finished");
        Ok(Response::new(()))
    }
    async fn start_activity(
        &self,
        request: Request<RequestStartActivityOptionsVersion>,
    ) -> Result<Response<ActivityResultVersion>, Status> {
        let activity_version = request.into_inner();
        match &activity_version.version {
            Some(request_start_activity_options_version::Version::V1(activity_options)) => {
                let (tx, rx) = oneshot::channel::<ActivityResultV1>();
                {
                    let now = Utc::now();
                    let mut activity_queues = self.activity_queue.lock().await;
                    match activity_queues.get_mut(&activity_options.task_queue) {
                        Some(queue) => {
                            queue.push_back(Box::new((
                                "0".to_string(),
                                activity_options.clone(),
                                tx,
                                now,
                            )));
                        }
                        None => {
                            let mut queue = VecDeque::new();
                            queue.push_back(Box::new((
                                "0".to_string(),
                                activity_options.clone(),
                                tx,
                                now,
                            )));
                            activity_queues.insert(activity_options.task_queue.clone(), queue);
                        }
                    }
                    self.activity_notify.notify_one();
                }

                match rx.await {
                    Ok(payload) => Ok(Response::new(ActivityResultVersion {
                        version: Some(activity_result_version::Version::V1(payload)),
                    })),
                    Err(_) => Err(Status::internal("Activity failed")),
                }
            }
            None => Err(Status::internal("unsupported version")),
        }
    }
}
//
// async fn service_status(reporter: HealthReporter) {
//     reporter
//         .set_serving::<ImmortalServer<ImmortalService>>()
//         .await;
// }

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();
    // tracing::subscriber::set_global_default(FmtSubscriber::default())?;
    let addr = "0.0.0.0:10000".parse().unwrap();

    let redis_username = std::env::var("REDIS_USERNAME").unwrap_or("".to_string());
    let redis_password = std::env::var("REDIS_PASSWORD").unwrap_or("pine5apple".to_string());
    let redis_host = std::env::var("REDIS_HOST").unwrap_or("127.0.0.1".to_string());
    let redis_port = std::env::var("REDIS_PORT").unwrap_or("30379".to_string());
    let redis_url = format!("redis://{redis_username}:{redis_password}@{redis_host}:{redis_port}/");
    println!("redis_url = {:?}", redis_url);
    // let (tx, _rx) = broadcast::channel(100);

    // let log_streams = Arc::new(Mutex::new(HashMap::new()));
    let manager = RedisConnectionManager::new(redis_url).unwrap();
    let pool = bb8::Pool::builder().build(manager).await.unwrap();

    let kube_client = KubeClient::try_default().await?;
    let cron_manager = Arc::new(Mutex::new(CronManager::new(kube_client).await?));
    let (stream_tx, _) = broadcast::channel::<IdentifiableMetrics>(1024);
    let (notification_tx, notification_rx) = broadcast::channel(100);
    let immortal_service = ImmortalService {
        cron_manager: Arc::clone(&cron_manager),
        metrics_stream: stream_tx.clone(),
        notification_tx: Arc::new(notification_tx.clone()),
        notification_rx: Arc::new(notification_rx),
        orphaned_activities: Arc::new(RwLock::new(HashMap::new())),
        orphaned_workflows: Arc::new(RwLock::new(HashMap::new())),
        workflow_queue: Arc::new(Mutex::new(HashMap::new())),
        workflow_notify: Arc::new(Notify::new()),
        activity_notify: Arc::new(Notify::new()),
        call_notify: Arc::new(Notify::new()),
        activity_queue: Arc::new(Mutex::new(HashMap::new())),
        call_queue: Arc::new(Mutex::new(HashMap::new())),
        redis_pool: pool.clone(),
        history: History::new(&pool),
        // log_streams: (tx.clone(), Arc::clone(&log_streams)),
        workers: Arc::new(RwLock::new(HashMap::new())),
        running_activities: Arc::new(RwLock::new(HashMap::new())),
        running_calls: Arc::new(RwLock::new(HashMap::new())),
    };

    let _ = immortal_service.orphaned_workflows().await;

    immortal_service.workflow_queue_thread();
    immortal_service.activity_queue_thread();
    immortal_service.call_queue_thread();
    immortal_service.watchdog();
    let svc = ImmortalServer::new(immortal_service.clone());
    // let (health_reporter, health_service) = tonic_health::server::health_reporter();
    // health_reporter
    //     .set_serving::<ImmortalServer<ImmortalService>>()
    //     .await;

    immortal_service.history.sync_workflow_index().await?;
    let immortal_service = Arc::new(immortal_service);
    // tokio::spawn(service_status(health_reporter.clone()));
    let use_tokio_console = std::env::var("ENABLE_TOKIO_CONSOLE").unwrap_or("false".to_string());
    if use_tokio_console.as_str() == "true" {
        console_subscriber::ConsoleLayer::builder()
            // set how long the console will retain data from completed tasks
            // set the address the server is bound to
            .server_addr(([0, 0, 0, 0], 6669))
            // ... other configurations ...
            .init();
    }

    {
        // let (_latest_tx, _latest_rx) = watch::channel(IdentifiableMetrics {
        //     // ts_ms: 0,
        //     cpu_pct: 0.0,
        //     mem_used: 0,
        //     mem_total: 0,
        //     worker_id: "server".to_string(),
        // });

        // optional history buffer (e.g., last 120 samples)
        let history = Arc::new(RwLock::new(VecDeque::with_capacity(120)));

        // spawn sampler
        tokio::spawn(metrics::server_sampler(stream_tx.clone(), history.clone()));
        let cors = CorsLayer::very_permissive();

        let (layer, io) = SocketIo::builder()
            // .with_state(tx.clone())
            // .with_state(log_streams)
            .with_state(pool.clone())
            .with_state(notification_tx)
            .with_state(stream_tx)
            .with_state(WsState::default())
            .with_state(Arc::clone(&immortal_service))
            .build_layer();
        io.ns("/api", on_connect);
        let app = axum::Router::new()
            .nest("/api", api::router())
            .layer(cors)
            .layer(
                ServiceBuilder::new()
                    .layer(CorsLayer::permissive())
                    .layer(layer),
            )
            .with_state(AppState {
                redis: pool,
                without_validation_arguments: (),
                pub_key: get_file_as_byte_vec().await,
                immortal_service,
            });

        let listener = tokio::net::TcpListener::bind("0.0.0.0:3001").await.unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
    }
    let enable_cron = std::env::var("ENABLE_CRON")
        .unwrap_or_else(|_| "true".into())
        .to_lowercase()
        == "true";

    if enable_cron {
        tokio::spawn(async move {
            loop {
                println!("watcher started");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if let Err(e) = start_watcher(Arc::clone(&cron_manager)).await {
                    println!("{:#?}", e)
                }
            }
        });
    }

    Server::builder()
        .add_service(svc)
        // .add_service(health_service)
        .serve(addr)
        .await?;

    Ok(())
}
