// easy break: run and didn't instantly die

use chrono::DateTime;
use chrono::Duration;
use chrono::NaiveDateTime;
use chrono::Utc;
// pub mod immortal {
//     tonic::include_proto!("immortal");
// }
use ::immortal::common;
use ::immortal::common::Payload;
use ::immortal::failure;
use ::immortal::immortal;
use ::immortal::immortal::call_result_version;
use ::immortal::immortal::call_version;
use ::immortal::immortal::notify_version;
use ::immortal::immortal::CallResultV1;
use ::immortal::immortal::CallResultVersion;
use ::immortal::immortal::CallVersion;
use ::immortal::immortal::ClientStartWorkflowOptionsV1;
use ::immortal::immortal::NotifyVersion;
use ::immortal::immortal::RequestStartActivityOptionsV1;
use ::immortal::immortal::StartNotificationOptionsV1;
use ::immortal::models::history::Status as HistoryStatus;
use ::immortal::models::history::{ActivityHistory, ActivityRun, History, WorkflowHistory};
use ::immortal::models::ActivitySchema;
use ::immortal::models::CallSchema;
use ::immortal::models::WfSchema;
use axum;
use bb8_redis::bb8::Pool;
use dotenvy::dotenv;
use rand::Rng;
use redis::streams::{StreamId, StreamKey, StreamMaxlen, StreamReadOptions, StreamReadReply};
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
use serde_json::Value;
use socketioxide::extract::{AckSender, Bin, State};
use socketioxide::socket::DisconnectReason;
use socketioxide::{
    extract::{Data, SocketRef},
    SocketIo,
};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::{broadcast, Notify, RwLock};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tonic::transport::Server;
use tonic_health::server::HealthReporter;
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use tracing::{error, info};
use tracing_subscriber::FmtSubscriber;
use uuid::Uuid;
// use immortal::immortal_s
// use immortal::im::{Server, ServerServer};
// use routeguide::route_guide_server::{RouteGuide, RouteGuideServer};
// use routeguide::worker_action::Action;
// use routeguide::{Feature, Point, Rectangle, RouteNote, RouteSummary};

use serde::Serialize;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
pub mod api;
pub mod models;

#[derive(Clone, Debug, Serialize)]
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
    // ActivityRunCancelled,
}
#[derive(Debug)]
struct RegisteredWorker {
    worker_id: String,

    task_queue: String,
    incoming: JoinHandle<()>,
    tx: Sender<Result<ImmortalWorkerActionVersion, Status>>,
    registered_workflows: HashMap<String, WfSchema>,
    registered_activities: HashMap<String, ActivitySchema>,
    registered_calls: HashMap<String, CallSchema>,
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

#[derive(Debug, Clone)]
struct CallOptions {
    call_type: String,
    input: Option<Payload>,
    task_queue: String,
}

#[derive(Debug, Clone)]
struct RunningProperties<T> {
    start: NaiveDateTime,
    timeout: NaiveDateTime,
    // in seconds
    max_duration: Duration,
    worker_id: String,
    additional_properties: T,
}

#[derive(Debug, Clone)]
struct CallProperties {}

#[derive(Debug, Clone)]
struct ActivityProperties {
    pub workflow_id: String,
}

#[derive(Debug, Clone)]
pub struct ImmortalService {
    redis_pool: bb8::Pool<RedisConnectionManager>,
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
                (
                    tokio::sync::broadcast::Sender<CallResultV1>,
                    RunningProperties<CallProperties>,
                ),
            >,
        >,
    >,

    activity_notify: Arc<Notify>,
    running_activities: Arc<
        RwLock<
            HashMap<
                String,
                (
                    // worker id
                    String,
                    tokio::sync::oneshot::Sender<ActivityResultV1>,
                    RunningProperties<ActivityProperties>,
                ),
            >,
        >,
    >,
    call_queue: Arc<
        Mutex<
            HashMap<
                String,
                VecDeque<(
                    String,
                    CallOptions,
                    tokio::sync::broadcast::Sender<CallResultV1>,
                )>,
            >,
        >,
    >,

    workflow_notify: Arc<Notify>,
    workflow_queue: Arc<Mutex<HashMap<String, VecDeque<(String, ClientStartWorkflowOptionsV1)>>>>,
    activity_queue: Arc<
        Mutex<
            HashMap<
                String,
                VecDeque<(
                    String,
                    RequestStartActivityOptionsV1,
                    tokio::sync::oneshot::Sender<ActivityResultV1>,
                )>,
            >,
        >,
    >,
}

impl ImmortalService {
    fn watchdog(&self) {
        let running_calls = Arc::clone(&self.running_calls);
        let running_activities = Arc::clone(&self.running_activities);
        let workers = Arc::clone(&self.workers);
        tokio::spawn(async move {
            loop {
                {
                    let mut activities_to_remove = vec![];

                    for (id, running_activity) in running_activities.read().await.iter() {
                        let now = Utc::now().naive_utc();
                        let max_time = running_activity.2.timeout;
                        if now > max_time {
                            let workers = workers.read().await;
                            if let Some(worker) = workers.get(&running_activity.2.worker_id) {
                                if let Err(e) = worker
                                    .tx
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
                    if activities_to_remove.len() > 0 {
                        let mut running_activities = running_activities.write().await;
                        for activity_to_remove in activities_to_remove {
                            if let Some((_worker_id, tx, props)) =
                                running_activities.remove(&activity_to_remove)
                            {
                                if let Err(e) = tx.send(ActivityResultV1 {
                                    activity_id: activity_to_remove.clone(),

                                    workflow_id: props.additional_properties.workflow_id.clone(),
                                    activity_run_id: "0".to_string(),

                                    status: Some(immortal::activity_result_v1::Status::Failed(
                                        immortal::Failure {
                                            failure: Some(failure::Failure {
                                                message: "timeout".to_string(),
                                                ..Default::default()
                                            }),
                                        },
                                    )),
                                }) {
                                    println!("{:#?}", e);
                                }
                            }
                        }
                    }
                    for (id, running_call) in running_calls.read().await.clone().iter() {
                        let now = Utc::now().naive_utc();
                        let max_time = running_call.1.timeout;
                        if now > max_time {
                            let workers = workers.read().await;
                            if let Some(worker) = workers.get(&running_call.1.worker_id) {
                                if let Err(e) = worker
                                    .tx
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

                for (queue_name, queue) in queues_snapshot {
                    if queue.is_empty() {
                        continue;
                    }

                    let mut workers_guard = workers.write().await;

                    // Try to assign one call from this queue
                    for (_index, (call_id, call_options, sender)) in queue.into_iter().enumerate() {
                        // Find eligible workers
                        let mut available_workers: Vec<_> = workers_guard
                            .iter_mut()
                            .filter(|(_, worker)| worker.task_queue == call_options.task_queue)
                            .map(|(_, worker)| worker)
                            .collect();

                        if available_workers.is_empty() {
                            break;
                        }

                        let chosen_worker_index =
                            rand::thread_rng().gen_range(0..available_workers.len());

                        if let Some(worker) = available_workers.get_mut(chosen_worker_index) {
                            // Dispatch call to worker
                            if let Err(e) = worker
                                .tx
                                .send(Ok(ImmortalWorkerActionVersion {
                                    version: Some(immortal_worker_action_version::Version::V1(
                                        ImmortalWorkerActionV1 {
                                            action: Some(WorkerAction::StartCall(
                                                immortal::StartCallOptionsV1 {
                                                    call_run_id: "0".to_string(),
                                                    call_id: call_id.clone(),
                                                    call_type: call_options.call_type.clone(),
                                                    call_input: call_options.input.clone(),
                                                },
                                            )),
                                        },
                                    )),
                                }))
                                .await
                            {
                                eprintln!(
                                    "Failed to send call to worker {}: {:?}",
                                    worker.worker_id, e
                                );
                            } else {
                                // Remove the item from the actual call queue (not the snapshot)
                                {
                                    let mut call_queues = call_queue.lock().await;
                                    if let Some(queue_vec) = call_queues.get_mut(&queue_name) {
                                        if let Some(pos) =
                                            queue_vec.iter().position(|(id, _, _)| *id == call_id)
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
                                    let now = Utc::now().naive_utc();
                                    let timeout = now + Duration::seconds(30);
                                    running_calls.insert(
                                        call_id.clone(),
                                        (
                                            sender,
                                            RunningProperties {
                                                start: now,
                                                timeout,
                                                max_duration: Duration::seconds(30),
                                                worker_id: worker.worker_id.clone(),
                                                additional_properties: CallProperties {},
                                            },
                                        ),
                                    );
                                }
                            }

                            //break; // Assign one call per loop per queue
                        }
                    }
                }
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
                    if let Some((activity_run_id, activity_options, tx)) = queue.pop_front() {
                        let mut workers_guard = workers.write().await;

                        let mut available_workers: Vec<_> = workers_guard
                            .iter_mut()
                            .filter(|(_, worker)| {
                                worker.task_queue == *queue_name
                                    && worker
                                        .registered_activities
                                        .contains_key(&activity_options.activity_type)
                            })
                            .map(|(_, worker)| worker)
                            .collect();

                        if available_workers.is_empty() {
                            queue.push_front((activity_run_id, activity_options, tx));
                            continue;
                        }

                        let random_index = rand::thread_rng().gen_range(0..available_workers.len());

                        if let Some(worker) = available_workers.get_mut(random_index) {
                            let now = Utc::now().naive_utc();
                            let duration = Duration::seconds(5);
                            let timeout = now + duration;

                            running_activities.write().await.insert(
                                activity_options.activity_id.clone(),
                                (
                                    worker.worker_id.clone(),
                                    tx,
                                    RunningProperties {
                                        start: now,
                                        timeout,
                                        max_duration: duration,
                                        worker_id: worker.worker_id.clone(),
                                        additional_properties: ActivityProperties {
                                            workflow_id: activity_options.workflow_id.clone(),
                                        },
                                    },
                                ),
                            );

                            if let Err(e) = worker
                                .tx
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
                                                    activity_input: activity_options
                                                        .activity_input
                                                        .clone(),
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
                                eprintln!("Failed to send StartActivity to worker: {:?}", e);
                                continue;
                            }

                            worker.activity_capacity -= 1;

                            let mut activity_history = ActivityHistory::new(
                                activity_options.activity_type.clone(),
                                activity_options.activity_id.clone(),
                            );
                            activity_history
                                .runs
                                .push(ActivityRun::new("0".to_string()));

                            if let Err(e) = history
                                .add_activity(
                                    &activity_options.workflow_id,
                                    activity_history.clone(),
                                )
                                .await
                            {
                                eprintln!("Failed to add activity to history: {:?}", e);
                            }

                            if let Err(e) = notification_tx.send(Notification::ActivityRunStarted(
                                Uuid::parse_str(&activity_options.workflow_id).unwrap_or_default(),
                                activity_history,
                            )) {
                                eprintln!("Failed to send notification: {:?}", e);
                            }
                        } else {
                            queue.push_front((activity_run_id, activity_options, tx));
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
                let queues_snapshot: HashMap<String, Vec<(String, StartWorkflowOptionsV1)>> = {
                    let queue_guard = workflow_queue.lock().await;
                    queue_guard
                        .iter()
                        .filter(|(_, v)| !v.is_empty())
                        .map(|(queue_name, items)| {
                            let converted_items = items
                                .iter()
                                .map(|(id, client_opts)| {
                                    (
                                        id.clone(),
                                        StartWorkflowOptionsV1 {
                                            // this might be incorrect
                                            workflow_id: id.clone(),
                                            workflow_type: client_opts.workflow_type.clone(),
                                            workflow_version: client_opts.workflow_version.clone(),
                                            task_queue: client_opts.task_queue.clone(),
                                            input: client_opts.input.clone(),
                                        },
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

                    let mut workers_guard = workers.write().await;

                    for (workflow_id, workflow_options) in queue {
                        let mut available_workers: Vec<_> = workers_guard
                            .iter_mut()
                            .filter(|(_, worker)| {
                                worker.task_queue == queue_name
                                    && worker
                                        .registered_workflows
                                        .contains_key(&workflow_options.workflow_type)
                            })
                            .map(|(_, worker)| worker)
                            .collect();

                        if available_workers.is_empty() {
                            println!("No available workers for workflow queue {}", queue_name);
                            break;
                        }

                        let chosen_index = rand::thread_rng().gen_range(0..available_workers.len());

                        if let Some(worker) = available_workers.get_mut(chosen_index) {
                            // Remove the item from the actual queue
                            {
                                let mut queue_guard = workflow_queue.lock().await;
                                if let Some(vec) = queue_guard.get_mut(&queue_name) {
                                    if let Some(pos) =
                                        vec.iter().position(|(id, _)| *id == workflow_id)
                                    {
                                        vec.remove(pos);
                                    }
                                    if vec.is_empty() {
                                        queue_guard.remove(&queue_name);
                                    }
                                }
                            }

                            // Build and store history
                            let workflow_history = WorkflowHistory::new(
                                workflow_options.workflow_type.clone(),
                                workflow_id.clone(),
                                workflow_options
                                    .input
                                    .as_ref()
                                    .map(|i| {
                                        i.payloads
                                            .iter()
                                            .filter_map(|f| serde_json::from_slice(&f.data).ok())
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                            );

                            if let Err(e) = history.add_workflow(workflow_history.clone()).await {
                                eprintln!("Failed to add workflow to history: {:?}", e);
                            }

                            // Send to worker
                            if let Err(e) = worker
                                .tx
                                .send(Ok(ImmortalWorkerActionVersion {
                                    version: Some(immortal_worker_action_version::Version::V1(
                                        ImmortalWorkerActionV1 {
                                            action: Some(WorkerAction::StartWorkflow(
                                                StartWorkflowOptionsV1 {
                                                    workflow_id: workflow_id.clone(),
                                                    workflow_type: workflow_options
                                                        .workflow_type
                                                        .clone(),
                                                    workflow_version: workflow_options
                                                        .workflow_version
                                                        .clone(),
                                                    task_queue: workflow_options.task_queue.clone(),
                                                    input: workflow_options.input.clone(),
                                                },
                                            )),
                                        },
                                    )),
                                }))
                                .await
                            {
                                eprintln!("Failed to send workflow to worker: {:?}", e);
                                continue;
                            }

                            worker.workflow_capacity -= 1;

                            if let Err(e) = notification_tx.send(Notification::WorkflowStarted(
                                Uuid::parse_str(&workflow_id).unwrap_or_default(),
                                workflow_history,
                            )) {
                                eprintln!("Failed to send workflow notification: {:?}", e);
                            }

                            break;
                        }
                    }
                }
            }
        });
    }

    pub async fn start_workflow_internal(
        &self,
        workflow_options: ClientStartWorkflowOptionsVersion,
    ) -> Result<String, Status> {
        println!("starting workflow");
        Ok(match workflow_options.version {
            Some(client_start_workflow_options_version::Version::V1(workflow_options)) => {
                let workflow_id = workflow_options
                    .workflow_id
                    .clone()
                    .unwrap_or(Uuid::new_v4().to_string());
                println!("waiting for queue lock");
                let mut wq = self.workflow_queue.lock().await;

                println!("obtained queue lock");
                match wq.get_mut(&workflow_options.task_queue) {
                    Some(queue) => {
                        queue.push_back((workflow_id.clone(), workflow_options.clone()));
                    }
                    None => {
                        let mut queue = VecDeque::new();
                        queue.push_back((workflow_id.clone(), workflow_options.clone()));
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
    async fn call(
        &self,
        request: Request<CallVersion>,
    ) -> Result<Response<CallResultVersion>, Status> {
        {
            match request.into_inner().version {
                Some(call_version::Version::V1(call)) => {
                    let (tx, mut rx) = broadcast::channel::<CallResultV1>(100);

                    {
                        let mut queue = self.call_queue.lock().await;
                        match queue.get_mut(&call.call_type) {
                            Some(queue) => {
                                queue.push_back((
                                    Uuid::new_v4().to_string(),
                                    CallOptions {
                                        call_type: call.call_type.clone(),
                                        input: call.input.clone(),
                                        task_queue: call.task_queue.clone(),
                                    },
                                    tx,
                                ));
                            }
                            None => {
                                let mut queue2 = VecDeque::new();
                                queue2.push_back((
                                    Uuid::new_v4().to_string(),
                                    CallOptions {
                                        call_type: call.call_type.clone(),
                                        input: call.input.clone(),
                                        task_queue: call.task_queue.clone(),
                                    },
                                    tx,
                                ));
                                queue.insert(call.call_type.clone(), queue2);
                            }
                        }
                    }

                    self.call_notify.notify_one();
                    // queue.get_mut(&call.call_type).unwrap().push_back((
                    //     Uuid::new_v4().to_string(),
                    //     call.clone(),
                    //     tx,
                    // ));
                    match rx.recv().await {
                        Ok(payload) => Ok(Response::new(CallResultVersion {
                            version: Some(call_result_version::Version::V1(payload)),
                        })),
                        Err(_) => Err(Status::internal("Call failed")),
                    }
                }
                _ => Err(Status::internal("unsupported version")),
            }
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
        let redis_pool = self.redis_pool.clone(); // clone the pool handle before spawning

        let handle = tokio::spawn(async move {
            while let Some(Ok(action)) = stream.next().await {
                match action.version {
                    Some(immortal_server_action_version::Version::V1(x)) => match x.action {
                        Some(immortal_server_action_v1::Action::LogEvent(log)) => {
                            if let Some(when) = DateTime::from_timestamp(log.when, 0) {
                                let when = when.to_string();
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
                                if let Some(x) = log.metadata.as_ref() {
                                    match serde_json::from_slice::<Value>(x) {
                                        Ok(json_data) => match serde_json::to_string(&json_data) {
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

        let (tx, rx) = mpsc::channel(4);
        let worker_id;
        {
            let mut workers = self.workers.write().await;

            let mut worker_details = worker_details.ok_or(tonic::Status::invalid_argument(
                "Worker details never provided",
            ))?;

            println!("{:#?}", worker_details.worker_id);

            worker_id = worker_details.worker_id.clone();
            let worker_ids = workers.iter().map(|f| f.0.clone()).collect::<Vec<_>>();

            if worker_ids.contains(&worker_details.worker_id) {
                worker_details.worker_id =
                    format!("{}-{}", worker_details.worker_id, Uuid::new_v4());
            }

            println!("{:#?}", worker_ids);
            let registered_workflows = worker_details
                .registered_workflows
                .iter()
                .map(|x| {
                    (
                        x.workflow_type.clone(),
                        WfSchema {
                            args: serde_json::from_slice(&x.args).unwrap(),
                            output: serde_json::from_slice(&x.output).unwrap(),
                        },
                    )
                })
                .collect();
            let registered_activities = worker_details
                .registered_activities
                .iter()
                .map(|x| {
                    (
                        x.activity_type.clone(),
                        ActivitySchema {
                            args: serde_json::from_slice(&x.args).unwrap(),
                            output: serde_json::from_slice(&x.output).unwrap(),
                        },
                    )
                })
                .collect();
            let registered_calls = worker_details
                .registered_calls
                .iter()
                .map(|x| {
                    (
                        x.call_type.clone(),
                        CallSchema {
                            args: serde_json::from_slice(&x.args).unwrap(),
                            output: serde_json::from_slice(&x.output).unwrap(),
                        },
                    )
                })
                .collect();
            workers.insert(
                worker_details.worker_id.clone(),
                RegisteredWorker {
                    activity_capacity: worker_details.activity_capacity,
                    task_queue: worker_details.task_queue,
                    workflow_capacity: worker_details.workflow_capacity,
                    incoming: handle,
                    tx: tx.clone(),
                    worker_id: worker_details.worker_id.clone(),
                    registered_workflows,
                    registered_activities,
                    registered_calls,
                    max_activity_capacity: worker_details.activity_capacity,
                    max_workflow_capacity: worker_details.workflow_capacity,
                },
            );
        }

        let workers = Arc::clone(&self.workers);
        println!("workers = {:?}", workers);
        self.call_notify.notify_one();
        self.workflow_notify.notify_one();
        self.activity_notify.notify_one();
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
                workers.remove(&worker_id);
            }

            println!("Stream ended");
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn execute_workflow(
        &self,
        request: Request<ClientStartWorkflowOptionsVersion>,
    ) -> Result<Response<WorkflowResultVersion>, Status> {
        let workflow_options = request.into_inner();
        let mut rx = self.notification_rx.resubscribe();
        let workflow_id =
            Uuid::parse_str(&self.start_workflow_internal(workflow_options).await?)
                .map_err(|e| tonic::Status::invalid_argument(format!("Invalid UUID: {}", e)))?;
        println!("executed workflow {workflow_id}");
        loop {
            match &rx.recv().await {
                Ok(x) => {
                    println!("workflow completed");
                    match x {
                        Notification::WorkflowResult(id, result) => {
                            if *id == workflow_id {
                                return Ok(Response::new(result.clone()));
                            }
                        }
                        _ => {}
                    }
                }
                Err(_) => {}
            }
        }
    }
    async fn start_workflow(
        &self,
        request: Request<ClientStartWorkflowOptionsVersion>,
    ) -> Result<Response<ClientStartWorkflowResponse>, Status> {
        let workflow_options = request.into_inner();
        let workflow_id = self.start_workflow_internal(workflow_options).await?;

        println!("started workflow: {workflow_id}");
        Ok(Response::new(ClientStartWorkflowResponse { workflow_id }))
    }
    async fn completed_activity(
        &self,
        request: Request<ActivityResultVersion>,
    ) -> Result<Response<()>, Status> {
        let activity_version = request.into_inner();

        match activity_version.version {
            Some(activity_result_version::Version::V1(activity_result)) => {
                // Remove the activity from the running map
                let (worker_id, tx, _) = match self
                    .running_activities
                    .write()
                    .await
                    .remove(&activity_result.activity_id)
                {
                    Some(entry) => entry,
                    None => return Err(Status::not_found("Activity not found")),
                };

                // Update the worker's activity capacity
                let mut workers = self.workers.write().await;
                if let Some(worker) = workers.get_mut(&worker_id) {
                    if worker.activity_capacity < worker.max_activity_capacity {
                        worker.activity_capacity += 1;
                    }

                    if let Err(e) = tx.send(activity_result.clone()) {
                        error!("Failed to send activity result: {:?}", e);
                    }
                } else {
                    error!("Worker {} not found", worker_id);
                }

                // Fetch and update activity history
                let activity_opt = self
                    .history
                    .get_activity(&activity_result.workflow_id, &activity_result.activity_id)
                    .await
                    .map_err(|e| {
                        error!("Error fetching activity: {:?}", e);
                        Status::internal("Failed to fetch activity history")
                    })?;

                let mut activity = match activity_opt {
                    Some(a) => a,
                    None => {
                        error!("Activity history not found for completed activity");
                        return Err(Status::not_found("Activity history not found"));
                    }
                };

                let run = match activity
                    .runs
                    .iter_mut()
                    .find(|f| f.run_id == activity_result.activity_run_id)
                {
                    Some(r) => r,
                    None => {
                        error!(
                            "Run ID {} not found in activity history",
                            activity_result.activity_run_id
                        );
                        return Err(Status::not_found("Run ID not found in activity history"));
                    }
                };

                run.end_time = Some(chrono::Utc::now().naive_utc());

                match activity_result.status {
                    Some(immortal::activity_result_v1::Status::Completed(x)) => {
                        match x.result {
                            Some(result_data) => match serde_json::from_slice(&result_data.data) {
                                Ok(result_value) => {
                                    run.status = HistoryStatus::Completed(result_value);
                                }
                                Err(e) => {
                                    error!("Failed to parse result: {:?}", e);
                                    run.status =
                                        HistoryStatus::Failed("Invalid result format".into());
                                }
                            },
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
                        } else {
                            error!(
                                "Invalid UUID in workflow_id: {}",
                                activity_result.workflow_id
                            );
                        }
                    }

                    Some(immortal::activity_result_v1::Status::Failed(x)) => {
                        run.status = HistoryStatus::Failed(format!("{:#?}", x));
                        if let Ok(id) = Uuid::parse_str(&activity_result.workflow_id) {
                            if let Err(e) = self
                                .notification_tx
                                .send(Notification::ActivityRunCompleted(id, activity.clone()))
                            {
                                error!("Error sending ActivityRunCompleted notification: {:?}", e);
                            }
                        } else {
                            error!(
                                "Invalid UUID in workflow_id: {}",
                                activity_result.workflow_id
                            );
                        }
                    }

                    Some(immortal::activity_result_v1::Status::Cancelled(x)) => {
                        run.status = HistoryStatus::Failed(format!("{:#?}", x));
                        // No notification in this case?
                    }

                    None => {
                        run.status = HistoryStatus::Failed("Missing status field".into());
                    }
                }

                if let Err(e) = self
                    .history
                    .update_activity(&activity_result.workflow_id, activity)
                    .await
                {
                    error!("Failed to update activity history: {:?}", e);
                    return Err(Status::internal("Failed to update activity history"));
                }
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
                        // need to watch out for this as it can increase past max

                        match tx.0.send(call_result.clone()) {
                            Ok(_) => {}
                            Err(e) => println!("{:#?}", e),
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
        let workflow_version = request.into_inner();

        let Some(workflow_result_version::Version::V1(workflow_result)) = &workflow_version.version
        else {
            return Err(Status::invalid_argument("Missing workflow result version"));
        };

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
        workflow.end_time = Some(chrono::Utc::now().naive_utc());

        // Notify workflow result
        match Uuid::parse_str(&workflow_result.workflow_id) {
            Ok(uuid) => {
                if let Err(e) = self
                    .notification_tx
                    .send(Notification::WorkflowResult(uuid, workflow_version.clone()))
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

        // Set status and send specific notification
        match &workflow_result.status {
            Some(workflow_result_v1::Status::Completed(x)) => {
                match &x.result {
                    Some(result_data) => match serde_json::from_slice(&result_data.data) {
                        Ok(deserialized) => {
                            workflow.status = HistoryStatus::Completed(deserialized);
                        }
                        Err(e) => {
                            error!("Failed to deserialize workflow result: {:?}", e);
                            workflow.status = HistoryStatus::Failed("Invalid result format".into());
                        }
                    },
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
            if let Some(worker) = workers.get_mut(&workflow_result.worker_id) {
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

        // Save updated workflow
        self.history
            .update_workflow(&workflow_result.workflow_id, workflow)
            .await
            .map_err(|e| {
                error!("Failed to update workflow history: {:?}", e);
                Status::internal("Failed to update workflow history")
            })?;

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
                    let mut activity_queues = self.activity_queue.lock().await;
                    match activity_queues.get_mut(&activity_options.task_queue) {
                        Some(queue) => {
                            queue.push_back(("0".to_string(), activity_options.clone(), tx));
                        }
                        None => {
                            let mut queue = VecDeque::new();
                            queue.push_back(("0".to_string(), activity_options.clone(), tx));
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

async fn service_status(mut reporter: HealthReporter) {
    reporter
        .set_serving::<ImmortalServer<ImmortalService>>()
        .await;
}

async fn on_connect(
    socket: SocketRef,
    Data(_data): Data<Value>,
    pool: State<Pool<RedisConnectionManager>>,
    notification_tx: State<broadcast::Sender<Notification>>,
) {
    info!("Socket.IO connected: {:?} {:?}", socket.ns(), socket.id);
    // println!("Data = {:?}", data);
    // socket.emit("auth", data).ok();
    // let pool = pool.clone();
    let mut con = pool.get().await.unwrap().clone();
    // con.

    // let a: () = con
    //     .xgroup_create_mkstream("immortal::logs", "immortal::logs::group", "$")
    //     .await
    //     .unwrap();

    socket.on(
        "message",
        |socket: SocketRef, Data::<Value>(data), Bin(bin)| {
            info!("Received event: {:?} {:?}", data, bin);
            socket.bin(bin).emit("message-back", data).ok();
        },
    );

    socket.on(
        "message-with-ack",
        |Data::<Value>(data), ack: AckSender, Bin(bin)| {
            info!("Received event: {:?} {:?}", data, bin);
            ack.bin(bin).send(data).ok();
        },
    );

    {
        let tx = notification_tx.clone();

        socket.on(
            "history-notifications",
            |socket: SocketRef, Data::<Value>(data), ack: AckSender, Bin(bin)| async move {
                info!("Received event: {:?} {:?}", data, bin);
                ack.bin(bin).send(data).ok();
                let s2 = socket.clone();
                let mut rx = tx.clone().subscribe();
                let handle = tokio::spawn(async move {
                    while let Ok(z) = rx.recv().await {
                        s2.emit(
                            "history-update",
                            serde_json::to_value(z).unwrap_or(serde_json::json!({})),
                        )
                        .ok();
                    }

                    // info!("Stream ended");
                });

                info!("Stream ended");
                let abort = handle.abort_handle();
                socket.on_disconnect(|_socket: SocketRef, _reason: DisconnectReason| async move {
                    // sink.unsubscribe("immortal::logs").await.unwrap();
                    abort.abort();
                    println!("aborting stream");
                    // handle.abort();
                })
            },
        );
    }

    socket.on(
        "fetch-logs",
        |socket: SocketRef, Data::<Value>(data), ack: AckSender, Bin(bin)| {
            info!("Received event: {:?} {:?}", data, bin);
            let log_id: String = serde_json::from_value(data.clone()).unwrap();
            ack.bin(bin);
            let s2 = socket.clone();
            let handle = tokio::spawn(async move {
                let mut last_id = "0-0".to_string();
                loop {
                    let opts = StreamReadOptions::default().block(500);

                    let srr: StreamReadReply = con
                        .xread_options(
                            &[format!("immortal:logs:{log_id}")],
                            &[last_id.as_str()],
                            &opts,
                        )
                        .await
                        .expect("read");
                    for StreamKey { key: _, ids } in srr.keys {
                        for StreamId { id, map } in ids {
                            last_id = id.clone();
                            let mut parsed_map = serde_json::json!({
                                "id": id.clone()
                            });
                            for (n, s) in map {
                                if let redis::Value::BulkString(bytes) = s {
                                    if n == "metadata" {
                                        parsed_map[n] =
                                            serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                                    } else {
                                        parsed_map[n] =
                                            Value::String(String::from_utf8(bytes).unwrap());
                                    }
                                } else {
                                    panic!("Weird data")
                                }
                            }
                            s2.emit("message-back", parsed_map).ok();
                        }
                    }
                }

                // info!("Stream ended");
            });

            info!("Stream ended");
            let abort = handle.abort_handle();
            socket.on_disconnect(|_socket: SocketRef, _reason: DisconnectReason| async move {
                // sink.unsubscribe("immortal::logs").await.unwrap();
                abort.abort();
                println!("aborting stream");
                // handle.abort();
            })
        },
    );
    // let s2 = socket.clone();

    // let abort = handle.abort_handle();
    socket.on_disconnect(|socket: SocketRef, reason: DisconnectReason| async move {
        // sink.unsubscribe("immortal::logs").await.unwrap();
        // abort.abort();
        println!(
            "Socket {} on ns {} disconnected, reason: {:?}",
            socket.id,
            socket.ns(),
            reason
        );
        // handle.abort();
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();
    tracing::subscriber::set_global_default(FmtSubscriber::default())?;
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

    let (notification_tx, notification_rx) = broadcast::channel(100);
    let immortal_service = ImmortalService {
        notification_tx: Arc::new(notification_tx.clone()),
        notification_rx: Arc::new(notification_rx),
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

    immortal_service.workflow_queue_thread();
    immortal_service.activity_queue_thread();
    immortal_service.call_queue_thread();
    immortal_service.watchdog();
    let svc = ImmortalServer::new(immortal_service.clone());
    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<ImmortalServer<ImmortalService>>()
        .await;

    immortal_service.history.sync_workflow_index().await?;
    tokio::spawn(service_status(health_reporter.clone()));

    {
        let cors = CorsLayer::very_permissive();

        let (layer, io) = SocketIo::builder()
            // .with_state(tx.clone())
            // .with_state(log_streams)
            .with_state(pool.clone())
            .with_state(notification_tx)
            .build_layer();
        io.ns("/", on_connect);
        let app = axum::Router::new()
            .nest("/api", api::router())
            .layer(cors)
            .layer(
                ServiceBuilder::new()
                    .layer(CorsLayer::permissive())
                    .layer(layer),
            )
            .with_state(immortal_service);

        let listener = tokio::net::TcpListener::bind("0.0.0.0:3001").await.unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
    }

    Server::builder()
        .add_service(svc)
        .add_service(health_service)
        .serve(addr)
        .await?;

    Ok(())
}
