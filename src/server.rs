use immortal_lib::common::Payloads;
use regex::Regex;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

use immortal_lib::immortal::immortal_worker_action_v1::Action as WorkerAction;
// use redis::RedisError;
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
// use history::Status as HistoryStatus;
// use history::{ActivityHistory, ActivityRun, History, WorkflowHistory};
use history::{ActivityHistory, History, Status as HistoryStatus, WorkflowHistory};
// use immortal_lib::common;
use immortal_lib::common::Payload;
use immortal_lib::immortal::call_result_version;
use immortal_lib::immortal::call_version;
use immortal_lib::immortal::notify_version;
use immortal_lib::immortal::CallResultV1;
use immortal_lib::immortal::CallResultVersion;
use immortal_lib::immortal::CallVersion;
use immortal_lib::immortal::ClientStartWorkflowOptionsV1;
use immortal_lib::immortal::NotifyVersion;
use immortal_lib::immortal::StartNotificationOptionsV1;
use immortal_worker_lib::models::ActivitySchema;
use immortal_worker_lib::models::CallSchema;
use immortal_worker_lib::models::WfSchema;
use redis::streams::StreamMaxlen;
use redis::AsyncCommands;
// use bb8_redis::redis::AsyncCommands;
use bb8_redis::{bb8, RedisConnectionManager};
use immortal_lib::immortal::immortal_server::{Immortal, ImmortalServer};
use immortal_lib::immortal::{
    activity_result_version, immortal_server_action_v1, immortal_server_action_version,
    immortal_worker_action_version, request_start_activity_options_version,
    workflow_result_version, ActivityResultV1, ActivityResultVersion,
    ClientStartWorkflowOptionsVersion, ClientStartWorkflowResponse, ImmortalServerActionVersion,
    ImmortalWorkerActionV1, ImmortalWorkerActionVersion, RequestStartActivityOptionsVersion,
    WorkflowResultVersion,
};
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
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use tracing::error;
use uuid::Uuid;

use crate::cron::start_watcher;
use crate::cron::CronManager;

use crate::history_metadata::{ActivityHistoryMetadata, WorkflowHistoryMetadata};
// use crate::history3::ActivityRunHistoryMetadata;
use crate::metrics::IdentifiableMetrics;
use crate::service::CallOptions;
use crate::service::ImmortalService;
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
pub mod history_metadata;
pub mod metrics;
pub mod models;
pub mod service;
pub mod state;
pub mod utils;
pub mod ws;

fn matches_any(patterns: &[String], input: &str) -> bool {
    for pattern in patterns {
        let re = Regex::new(pattern).expect("Invalid regex pattern");
        if re.is_match(input) {
            return true;
        }
    }
    false
}

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
    WorkflowCompleted(Uuid, WorkflowHistoryMetadata),
    WorkflowResult(Uuid, WorkflowHistoryMetadata),
    WorkflowFailed(Uuid, WorkflowHistoryMetadata),
    // WorkflowCancelled,
    ActivityRunStarted(Uuid, ActivityHistory),
    ActivityRunCompleted(Uuid, ActivityHistoryMetadata),
    ActivityRunFailed(Uuid, ActivityHistory),
    WorkerAdded(String),
    WorkerRemoved(Uuid),
    // ActivityRunCancelled,
}
#[derive(Debug)]
pub struct RegisteredWorker {
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
    instance_id: Uuid,
}

pub enum WorkflowStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
enum KillState {
    Healthy,
    Suspected {
        first_seen: DateTime<Utc>,
        attempts: u32,
    },
    Orphaned {
        first_seen: DateTime<Utc>,
    }, // Finalized,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunningProperties<T> {
    start: DateTime<Utc>,
    timeout: DateTime<Utc>,
    kill_state: KillState,
    heartbeat_timeout: Duration,
    // in seconds
    max_duration: Duration,
    worker_id: String,
    worker_instance_id: Uuid,
    additional_properties: T,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallProperties {
    pub last_heartbeat: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivityProperties {
    pub workflow_id: String,
    pub latest_run_id: String,
    pub index: usize,
    pub last_heartbeat: DateTime<Utc>,
    pub scheduled: DateTime<Utc>,
    pub latest_run_start: Option<DateTime<Utc>>,
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
                                error!("Failed to send workflow notification: {:?}", e);
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

        let worker_instance_id;
        let mut worker_details = worker_details
            .clone()
            .ok_or(tonic::Status::invalid_argument(
                "Worker details never provided",
            ))?;
        {
            // sometimes immortal freezes here, not sure why
            println!("waiting to receive workers write handle");
            let workers = self.workers.read().await;

            worker_instance_id = Uuid::parse_str(&worker_details.instance_id)
                .map_err(|_e| Status::invalid_argument("Worker instance id must be UUID v4."))?;
            let worker_instance_ids = workers.iter().map(|f| f.0.clone()).collect::<Vec<_>>();

            if worker_instance_ids.contains(&worker_instance_id) {
                return Err(tonic::Status::invalid_argument(
                    "Instance ID already registered.",
                ));
            }
        }
        let worker_id2 = worker_instance_id.clone();
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
                                    worker_id: worker_id2.to_string(),
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
                                    immortal_lib::immortal::Level::Info => "info",
                                    immortal_lib::immortal::Level::Warn => "warn",
                                    immortal_lib::immortal::Level::Error => "error",
                                    immortal_lib::immortal::Level::Debug => "debug",
                                    immortal_lib::immortal::Level::Trace => "trace",
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
                                            Err(e) => error!("Error serializing JSON: {}", e),
                                        },
                                        Err(e) => error!("Error parsing metadata: {}", e),
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
                                                &key,
                                                StreamMaxlen::Approx(1000),
                                                "*",
                                                &items,
                                            )
                                            .await
                                        {
                                            error!("Error appending to logs: {}", e);
                                        }
                                        // TODO: don't ignore this
                                        if let Err(e) = con.expire::<&str, ()>(&key, 259_200).await
                                        {
                                            error!("Error setting exp for logs: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        error!("Error getting Redis connection: {}", e);
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

            // let worker_ids = workers.iter().map(|f| f.0.clone()).collect::<Vec<_>>();
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
                worker_instance_id,
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
                    instance_id: worker_instance_id.clone(),
                },
            );
            // 1) WE FIRST NEED TO CHECK IF THE WORKFLOW EXISTS IN THE DATABASE
            // 2) WE NEED TO SEE IF THE WORKFLOW HAS BEEN SETTLED
            // 2.1) IF THE WORKFLOW HAS BEEN SETTLED. SEND A KILL COMMAND
            // 2.2) IF THE WORKFLOW IS STILL RUNNING BUT IS AT A NEW EPOCH. SEND KILL COMMAND
            // 2.3) IF WORKFLOW IS STILL RUNNING AND EPOCH MATCHES. ADD TO RUNNING_WORKFLOWS LIST
            // WE SHOULD ALSO TRACK/SEND IF THE WORKFLOW/ACTIVITY HAS BEEN COMPLETED. THIS CAN BE
            // DONE SAFELY AS WHEN A WORKER IS DISCONNECTED. NO NEW WF/ACTIVITIES ARE ADDED TO IT'S
            // QUEUE SO IT WILL NOT PILE UP WITH HANGING RESULTS.
            // this should also be done in a separate tokio task to avoid blocking the main thread
            let running_workflows = worker_details.running_workflows;
            let running_activities = worker_details.running_activities;
            let mut con = self.redis_pool.get().await.unwrap();

            let now = Utc::now();
            {
                for running_activity in running_activities {
                    let mut guard = self.running_activities.write().await;
                    let activity_id = running_activity.activity_id;
                    let workflow_id = running_activity.workflow_id;
                    let activity_run_id = running_activity.activity_run_id;
                    let activity_metadata =
                        ActivityHistoryMetadata::get_opt(&mut con, &workflow_id, &activity_id)
                            .await
                            .unwrap();

                    if let Some(activity_metadata) = activity_metadata {
                        let latest_run_id = activity_metadata
                            .runs
                            .get(activity_metadata.runs.len() - 1)
                            .unwrap()
                            .run_id
                            .to_string();
                        if latest_run_id == activity_run_id {
                            guard.insert(
                                activity_id,
                                Box::new((
                                    worker_instance_id.clone(),
                                    vec![],
                                    // probably turn this into a vec
                                    RunningProperties {
                                        start: activity_metadata.start_time.clone(),
                                        worker_id: worker_details.worker_id.clone(),
                                        worker_instance_id: worker_instance_id.clone(),
                                        max_duration: Duration::seconds(30),
                                        heartbeat_timeout: Duration::seconds(30),
                                        timeout: activity_metadata.start_time
                                            + Duration::seconds(30),
                                        kill_state: KillState::Healthy,
                                        additional_properties: ActivityProperties {
                                            workflow_id: workflow_id.clone(),
                                            latest_run_id: latest_run_id,
                                            index: activity_metadata.index,
                                            last_heartbeat: now.clone(),
                                            // THIS IS INCORRECT
                                            scheduled: now,
                                            latest_run_start: None,
                                        },
                                    },
                                )),
                            );
                        }
                    }
                }
            }

            for running_workflow in running_workflows {
                let wf =
                    WorkflowHistoryMetadata::get_opt(&mut con, &running_workflow.workflow_id, true)
                        .await
                        .unwrap();
                if let Some(wf) = wf {
                    if matches!(wf.status, HistoryStatus::Completed) {
                        // IGNORE
                    } else if running_workflow.epoch != wf.epoch {
                        let _ = self.kill_workflow(&running_workflow.workflow_id).await;
                        // KILL AND IGNORE
                    } else {
                        // FINAL ADD WF BACK TO RUNNING WORKFLOWS

                        let mut guard = self.running_workflows.write().await;
                        guard.insert(
                            running_workflow.workflow_id,
                            Box::new((
                                worker_instance_id.clone(),
                                RunningProperties {
                                    start: wf.start_time.clone(),
                                    worker_id: worker_details.worker_id.clone(),
                                    worker_instance_id: worker_instance_id.clone(),
                                    max_duration: Duration::seconds(30),
                                    heartbeat_timeout: Duration::seconds(30),
                                    timeout: wf.start_time + Duration::seconds(30),
                                    kill_state: KillState::Healthy,
                                    additional_properties: ClientStartWorkflowOptionsV1 {
                                        workflow_id: Some(wf.workflow_id.clone()),
                                        workflow_type: wf.workflow_type.clone(),
                                        workflow_version: "V1".to_string(),
                                        // WE NEED TO FIGURE THIS ONE OUT
                                        input: match wf.args.len() {
                                            0 => None,
                                            _ => Some(Payloads {
                                                payloads: wf
                                                    .args
                                                    .into_iter()
                                                    .map(|f| Payload {
                                                        data: f.data.unwrap(),
                                                        metadata: f.metadata.unwrap_or_default(),
                                                    })
                                                    .collect(),
                                            }),
                                        },
                                        task_queue: wf.task_queue.clone(),
                                    },
                                },
                            )),
                        );
                    }
                } else {
                    let _ = self.kill_workflow(&running_workflow.workflow_id).await;
                    // KILL
                }
            }
        }

        let workers = Arc::clone(&self.workers);
        self.call_notify.notify_one();
        self.workflow_notify.notify_one();
        self.activity_notify.notify_one();
        let _ = self
            .notification_tx
            .send(Notification::WorkerAdded(worker_details.worker_id.clone()));

        let notification_tx = self.notification_tx.clone();
        let running_workflows = self.running_workflows.clone();
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
                let x = workers.remove(&worker_instance_id);
                let _ =
                    notification_tx.send(Notification::WorkerRemoved(worker_instance_id.clone()));

                let now = Utc::now();
                // we need to find the running workflows and set them to orphaned

                let mut running_workflows = running_workflows.write().await;

                for (wf_id, running_workflow) in running_workflows.iter_mut() {
                    if running_workflow.0 == worker_instance_id {
                        running_workflow.1.kill_state = KillState::Orphaned { first_seen: now };

                        println!(
                            "wf {wf_id} set to orphaned because {} == {}",
                            running_workflow.0, worker_instance_id
                        )
                    } else {
                        println!(
                            "wf {wf_id} not set to orphaned because {} != {}",
                            running_workflow.0, worker_instance_id
                        )
                    }
                }

                if let Some(x) = x {
                    println!("Stream ended and removed {:?}", x.worker_id);
                } else {
                    error!("Stream ended NO WORKER REMOVED {}", worker_instance_id);
                    println!("Stream ended NO WORKER REMOVED {}", worker_instance_id);
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
                .start_workflow_internal(workflow_options, Some(tx))
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
                            let workflow_output = self
                                .history
                                .get_workflow_output(&result.workflow_id)
                                .await
                                .unwrap();
                            return Ok(Response::new(WorkflowResultVersion {
                                version: Some(workflow_result_version::Version::V1(
                                    immortal_lib::immortal::WorkflowResultV1 {
                                        workflow_id: result.workflow_id.clone(),
                                        worker_instance_id: result.owner.clone().unwrap().instance_id.to_string(),
                                        epoch: result.epoch,
                                        worker_id: result.worker_id.clone().unwrap(),
                                        status: match result.status {
                                            HistoryStatus::Completed => Some(immortal_lib::immortal::workflow_result_v1::Status::Completed(immortal_lib::immortal::Success {
                                                result: workflow_output.map(|data| Payload {
                                                    data,
                                                    metadata: result.output.clone().unwrap().metadata.unwrap().clone()
                                                })
                                            })),
                                            HistoryStatus::Failed => Some(immortal_lib::immortal::workflow_result_v1::Status::Failed(immortal_lib::immortal::Failure{
                                                failure: workflow_output.map(|mut data| simd_json::from_slice::<immortal_lib::failure::Failure>(&mut data).unwrap())
                                            })),
                                            _ => None

                                        },
                                    },
                                )),
                            }));
                            // TODO
                            // return Err(tonic::Status::unimplemented(format!("lol")));
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
            .start_workflow_internal(workflow_options, None)
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
                self.completed_activity_inner(activity_result).await?;
                // self.com
                // Remove the activity from the running map
                // I think what is happening is that tx is being dropped
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

    // THE FOLLOWING IS UPDATED AND STORED IN REDIS
    // 1) END TIME
    // 2) STATUS
    // 3) OUTPUT

    async fn completed_workflow(
        &self,
        request: Request<WorkflowResultVersion>,
    ) -> Result<Response<()>, Status> {
        let workflow_version = request.into_inner();
        // let mut inform_worker = true;
        match workflow_version.version {
            Some(workflow_result_version::Version::V1(workflow_result)) => {
                // give it a time to let activities sync with redis
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                println!("COMPLETD WORKFLOW");
                self.completed_workflow_inner(workflow_result).await?;
                // self.com
                // Remove the activity from the running map
                // I think what is happening is that tx is being dropped
            }

            None => {
                return Err(Status::invalid_argument("Missing workflow result version"));
            }
        }

        // println!("finished");
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
                    let activity_index = self
                        .history
                        .get_workflow_activity_len(&activity_options.workflow_id)
                        .await
                        .map_err(|e| {
                            Status::internal(format!(
                                "Couldn't fetch workflow history activity length {}",
                                e.to_string()
                            ))
                        })?;

                    match activity_queues.get_mut(&activity_options.task_queue) {
                        Some(queue) => {
                            queue.push_back(Box::new((
                                activity_options.clone(),
                                vec![tx],
                                now,
                                activity_index,
                                None,
                            )));
                        }
                        None => {
                            let mut queue = VecDeque::new();
                            queue.push_back(Box::new((
                                activity_options.clone(),
                                vec![tx],
                                now,
                                activity_index,
                                None,
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
                    Err(e) =>  {
                        println!("{:#?}", e);
                        Err(Status::internal("Activity failed"))
                    }
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
    let _guard = sentry::init(("https://961020bbea942af57e22b66f1825355b@o4510499360014336.ingest.us.sentry.io/4510540688588800", sentry::ClientOptions {
    release: sentry::release_name!(),
    // Capture user IPs and potentially sensitive headers when using HTTP server integrations
    // see https://docs.sentry.io/platforms/rust/data-management/data-collected for more info
    send_default_pii: true,
    ..Default::default()
  }));
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(sentry::integrations::tracing::layer())
        .init();
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
    let immortal_service = Arc::new(ImmortalService {
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
        running_workflows: Arc::new(RwLock::new(HashMap::new())),
        running_calls: Arc::new(RwLock::new(HashMap::new())),
    });

    // let _ = immortal_service.orphaned_workflows().await;

    immortal_service.clone().resurrect();
    immortal_service.workflow_queue_thread();
    immortal_service.activity_queue_thread();
    immortal_service.call_queue_thread();
    immortal_service.clone().watchdog();
    let svc = ImmortalServer::new((*immortal_service).clone());
    // let (health_reporter, health_service) = tonic_health::server::health_reporter();
    // health_reporter
    //     .set_serving::<ImmortalServer<ImmortalService>>()
    //     .await;

    immortal_service.history.sync_workflow_index().await?;
    // let immortal_service = Arc::new(immortal_service);
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
