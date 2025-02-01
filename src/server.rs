use chrono::DateTime;
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
use tokio::sync::broadcast;
use tokio::sync::mpsc::Sender;
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
pub struct ImmortalService {
    redis_pool: bb8::Pool<RedisConnectionManager>,
    workers: Arc<Mutex<HashMap<String, RegisteredWorker>>>,
    // log_streams: (
    //     broadcast::Sender<LogStreamUpdate>,
    //     Arc<Mutex<HashMap<String, LogStream>>>,
    // ),
    history: History,

    notification_tx: Arc<tokio::sync::broadcast::Sender<Notification>>,
    notification_rx: Arc<tokio::sync::broadcast::Receiver<Notification>>,

    running_calls: Arc<Mutex<HashMap<String, tokio::sync::broadcast::Sender<CallResultV1>>>>,
    running_activities: Arc<
        Mutex<
            HashMap<
                String,
                (
                    Option<String>,
                    tokio::sync::oneshot::Sender<ActivityResultV1>,
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
    workflow_queue: Arc<Mutex<HashMap<String, VecDeque<(String, ClientStartWorkflowOptionsV1)>>>>,
    activity_queue: Arc<Mutex<HashMap<String, VecDeque<(String, RequestStartActivityOptionsV1)>>>>,
}

impl ImmortalService {
    pub fn call_queue_thread(&self) {
        let call_queue = Arc::clone(&self.call_queue);
        let running_calls = Arc::clone(&self.running_calls);
        let workers = Arc::clone(&self.workers);
        tokio::spawn(async move {
            loop {
                let queues;
                // let activity_queues;
                // {
                {
                    let call_queues = call_queue.lock().await;

                    queues = call_queues
                        .iter()
                        .map(|(f, c)| (f.to_string(), c.clone()))
                        .collect::<HashMap<_, _>>()
                        .clone();
                }
                // }

                for (queue_name, queue) in queues {
                    let mut workers = workers.lock().await;
                    for (i, x) in queue.iter().enumerate() {
                        let mut workers_filtered = workers
                            .iter_mut()
                            .filter(|(_, worker)| worker.task_queue == *x.1.task_queue)
                            // .filter(|(_, worker)| {
                            //     worker
                            //         .registered_activities
                            //         .contains_key(&x.1.call_type)
                            // })
                            .map(|(_, worker)| worker)
                            .collect::<Vec<_>>();
                        if workers_filtered.len() == 0 {
                            continue;
                        }
                        let random = {
                            let mut rng = rand::thread_rng();
                            rng.gen_range(0..workers_filtered.len())
                        };
                        match workers_filtered.get_mut(random) {
                            Some(worker) => {
                                let (call_id, call_options, sender) = {
                                    let mut queues = call_queue.lock().await;
                                    let q = queues.get_mut(&queue_name).unwrap();
                                    let y = q.remove(i).unwrap();
                                    if q.len() == 0 {
                                        queues.remove(&queue_name);
                                    }
                                    y
                                };

                                {
                                    let mut running_calls = running_calls.lock().await;
                                    running_calls.insert(call_id.clone(), sender);
                                }
                                // let workers = self.workers.lock().await;
                                // let worker = workers.get(worker_id).unwrap().clone();

                                worker
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
                                    .unwrap();
                            }
                            None => {}
                        }
                        break;
                    }
                }
                // wait 500ms
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });
    }
    pub fn activity_queue_thread(&self) {
        let activity_queue = Arc::clone(&self.activity_queue);
        let running_activities = Arc::clone(&self.running_activities);
        let notification_tx = Arc::clone(&self.notification_tx);
        let workers = Arc::clone(&self.workers);
        let history = self.history.clone();
        tokio::spawn(async move {
            loop {
                let queues;
                // let activity_queues;
                {
                    let activity_queues = (activity_queue.lock().await).clone();
                    queues = activity_queues
                        .iter()
                        .filter(|(_, v)| v.len() > 0)
                        .map(|(f, c)| (f.to_string(), c.clone()))
                        .collect::<HashMap<_, _>>()
                        .clone();
                }

                for (queue_name, queue) in queues {
                    let mut workers = workers.lock().await;
                    for (i, x) in queue.iter().enumerate() {
                        let mut workers_filtered = workers
                            .iter_mut()
                            .filter(|(_, worker)| worker.task_queue == *queue_name)
                            .filter(|(_, worker)| worker.activity_capacity > 0)
                            .filter(|(_, worker)| {
                                worker
                                    .registered_activities
                                    .contains_key(&x.1.activity_type)
                            })
                            .map(|(_, worker)| worker)
                            .collect::<Vec<_>>();
                        if workers_filtered.len() == 0 {
                            continue;
                        }
                        let random = {
                            let mut rng = rand::thread_rng();
                            rng.gen_range(0..workers_filtered.len())
                        };
                        match workers_filtered.get_mut(random) {
                            Some(worker) => {
                                let (activity_run_id, activity_options) = {
                                    let mut queues = activity_queue.lock().await;
                                    let q = queues.get_mut(&queue_name).unwrap();
                                    q.remove(i).unwrap()
                                };

                                {
                                    let mut running_activities = running_activities.lock().await;
                                    running_activities
                                        .get_mut(&activity_options.activity_id)
                                        .unwrap()
                                        .0 = Some(worker.worker_id.clone());
                                }
                                // let workers = self.workers.lock().await;
                                // let worker = workers.get(worker_id).unwrap().clone();

                                worker
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
                                    .unwrap();
                                worker.activity_capacity -= 1;

                                let mut activity_history = ActivityHistory::new(
                                    activity_options.activity_type.clone(),
                                    activity_options.activity_id.clone(),
                                );
                                activity_history
                                    .runs
                                    .push(ActivityRun::new("0".to_string()));
                                history
                                    .add_activity(
                                        &activity_options.workflow_id,
                                        activity_history.clone(),
                                    )
                                    .await
                                    .unwrap();
                                notification_tx
                                    .send(Notification::ActivityRunStarted(
                                        Uuid::parse_str(&activity_options.workflow_id).unwrap(),
                                        activity_history,
                                    ))
                                    .unwrap();
                            }
                            None => {}
                        }
                        break;
                    }
                }
                // wait 500ms
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });
    }
    pub fn workflow_queue_thread(&self) {
        let workflow_queue = Arc::clone(&self.workflow_queue);
        let workers = Arc::clone(&self.workers);
        let notification_tx = Arc::clone(&self.notification_tx);
        let history = self.history.clone();

        tokio::spawn(async move {
            loop {
                let queues;
                {
                    let workflow_queues = (workflow_queue.lock().await).clone();

                    queues = workflow_queues
                        .iter()
                        .filter(|(_, v)| v.len() > 0)
                        .map(|(f, c)| (f.to_string(), c.clone()))
                        .collect::<HashMap<_, _>>()
                        .clone();
                }

                for (queue_name, queue) in queues {
                    let mut workers = workers.lock().await;
                    for (i, x) in queue.iter().enumerate() {
                        let mut workers_filtered = workers
                            .iter_mut()
                            .filter(|(_, worker)| worker.task_queue == *queue_name)
                            .filter(|(_, worker)| worker.workflow_capacity > 0)
                            .filter(|(_, worker)| {
                                worker.registered_workflows.contains_key(&x.1.workflow_type)
                            })
                            .map(|(_, worker)| worker)
                            .collect::<Vec<_>>();
                        if workers_filtered.len() == 0 {
                            continue;
                        }
                        let random = {
                            let mut rng = rand::thread_rng();
                            rng.gen_range(0..workers_filtered.len())
                        };
                        match workers_filtered.get_mut(random) {
                            Some(worker) => {
                                let (workflow_id, workflow_options) = {
                                    let mut queues = workflow_queue.lock().await;
                                    let q = queues.get_mut(&queue_name).unwrap();

                                    q.remove(i).unwrap()
                                };
                                // let workers = self.workers.lock().await;
                                // let worker = workers.get(worker_id).unwrap().clone();

                                let workflow_history = WorkflowHistory::new(
                                    workflow_options.workflow_type.clone(),
                                    workflow_id.clone(),
                                    workflow_options
                                        .input
                                        .as_ref()
                                        .unwrap()
                                        .payloads
                                        .iter()
                                        .map(|f| serde_json::from_slice(&f.data).unwrap())
                                        .collect::<Vec<_>>(),
                                );
                                history
                                    .add_workflow(workflow_history.clone())
                                    .await
                                    .unwrap();
                                worker
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
                                                        task_queue: workflow_options
                                                            .task_queue
                                                            .clone(),

                                                        input: workflow_options.input.clone(),
                                                    },
                                                )),
                                            },
                                        )),
                                    }))
                                    .await
                                    .unwrap();
                                worker.workflow_capacity -= 1;

                                notification_tx
                                    .send(Notification::WorkflowStarted(
                                        Uuid::parse_str(&workflow_history.workflow_id).unwrap(),
                                        workflow_history,
                                    ))
                                    .unwrap();
                            }
                            None => {}
                        }
                    }
                }

                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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
            match request.into_inner().version.unwrap() {
                notify_version::Version::V1(v1) => {
                    let workers = self.workers.lock().await;
                    let workers_to_notify = workers
                        .iter()
                        .filter(|(_, worker)| matches_any(&v1.task_queues, &worker.task_queue))
                        .map(|(_, worker)| worker)
                        .collect::<Vec<_>>();
                    for worker in workers_to_notify {
                        worker
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
                            .unwrap();
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
        let mut con = self.redis_pool.get().await.unwrap().clone();
        let mut stream = request.into_inner();
        let mut worker_details = None;
        if let Ok(action) = stream.next().await.unwrap() {
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
        let handle = tokio::spawn(async move {
            while let Some(Ok(action)) = stream.next().await {
                println!("Action = {:?}", action);
                match action.version {
                    Some(immortal_server_action_version::Version::V1(x)) => match x.action {
                        Some(immortal_server_action_v1::Action::LogEvent(log)) => {
                            let when = DateTime::from_timestamp(log.when, 0).unwrap().to_string();
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
                            match log.metadata.as_ref() {
                                Some(x) => {
                                    let json_data: Value = serde_json::from_slice(&x).unwrap();
                                    metadata = serde_json::to_string(&json_data).unwrap();
                                    items.push(("metadata", &metadata));
                                }
                                None => {}
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

                            let _: () = con
                                .xadd_maxlen(
                                    format!("immortal:logs:{}", log.workflow_id),
                                    StreamMaxlen::Approx(1000),
                                    "*",
                                    &items,
                                )
                                .await
                                .unwrap();
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            println!("incoming Stream ended");
        });
        // worker_details_version.next().await
        let mut workers = self.workers.lock().await;

        let (tx, rx) = mpsc::channel(4);
        let mut worker_details = worker_details.unwrap();

        println!("{:#?}", worker_details.worker_id);

        let worker_ids = workers.iter().map(|f| f.0.clone()).collect::<Vec<_>>();

        if worker_ids.contains(&worker_details.worker_id) {
            worker_details.worker_id = format!("{}-{}", worker_details.worker_id, Uuid::new_v4());
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
        let workers = Arc::clone(&self.workers);
        println!("workers = {:?}", workers);
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
                let mut workers = workers.lock().await;
                workers.remove(&worker_details.worker_id);
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
            Uuid::parse_str(&self.start_workflow_internal(workflow_options).await?).unwrap();
        println!("started workflow {workflow_id}");
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
                let mut running_activities = self.running_activities.lock().await;
                // let tx = running_activities.get(&activity_result.activity_id).unwrap();
                match running_activities.remove(&activity_result.activity_id) {
                    Some((worker_id, tx)) => {
                        let mut workers = self.workers.lock().await;
                        let worker = workers.get_mut(&worker_id.unwrap()).unwrap();
                        // need to watch out for this as it can increase past max
                        if worker.activity_capacity < worker.max_activity_capacity {
                            worker.activity_capacity += 1;
                        }
                        tx.send(activity_result.clone()).unwrap();
                    }
                    None => {
                        return Err(Status::not_found("Activity not found"));
                    }
                }
                let mut activity = self
                    .history
                    .get_activity(&activity_result.workflow_id, &activity_result.activity_id)
                    .await
                    .unwrap()
                    .unwrap();
                let run = activity
                    .runs
                    .iter_mut()
                    .find(|f| f.run_id == activity_result.activity_run_id)
                    .unwrap();
                run.end_time = Some(
                    DateTime::from_timestamp(chrono::Utc::now().timestamp(), 0)
                        .unwrap()
                        .naive_utc(),
                );
                match activity_result.status.unwrap() {
                    immortal::activity_result_v1::Status::Completed(x) => {
                        run.status = HistoryStatus::Completed(
                            serde_json::from_slice(&x.result.unwrap().data).unwrap(),
                        );

                        match self
                            .notification_tx
                            .send(Notification::ActivityRunCompleted(
                                Uuid::parse_str(&activity_result.workflow_id).unwrap(),
                                activity.clone(),
                            )) {
                            Ok(_) => {}
                            Err(e) => error!("Error sending notification {:#?}", e),
                        }
                    }
                    immortal::activity_result_v1::Status::Failed(x) => {
                        run.status = HistoryStatus::Failed(format!("{:#?}", x));
                        match self.notification_tx.send(Notification::ActivityRunFailed(
                            Uuid::parse_str(&activity_result.workflow_id).unwrap(),
                            activity.clone(),
                        )) {
                            Ok(_) => {}
                            Err(e) => error!("Error sending notification {:#?}", e),
                        }
                    }
                    immortal::activity_result_v1::Status::Cancelled(x) => {
                        run.status = HistoryStatus::Failed(format!("{:#?}", x));
                    }
                }
                self.history
                    .update_activity(&activity_result.workflow_id, activity)
                    .await
                    .unwrap();
            }
            _ => {}
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
                let mut running_calls = self.running_calls.lock().await;
                // let tx = running_activities.get(&activity_result.activity_id).unwrap();
                match running_calls.remove(&call_result.call_id) {
                    Some(tx) => {
                        // need to watch out for this as it can increase past max

                        match tx.send(call_result.clone()) {
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
        match &workflow_version.version {
            Some(workflow_result_version::Version::V1(workflow_result)) => {
                let mut workflow = self
                    .history
                    .get_workflow(&workflow_result.workflow_id)
                    .await
                    .unwrap()
                    .unwrap();

                workflow.end_time = Some(
                    DateTime::from_timestamp(chrono::Utc::now().timestamp(), 0)
                        .unwrap()
                        .naive_utc(),
                );
                self.notification_tx
                    .send(Notification::WorkflowResult(
                        Uuid::parse_str(&workflow_result.workflow_id).unwrap(),
                        workflow_version.clone(),
                    ))
                    .unwrap();
                match workflow_result.status.clone().unwrap() {
                    workflow_result_v1::Status::Completed(x) => {
                        workflow.status = HistoryStatus::Completed(
                            serde_json::from_slice(&x.result.unwrap().data).unwrap(),
                        );
                        self.notification_tx
                            .send(Notification::WorkflowCompleted(
                                Uuid::parse_str(&workflow_result.workflow_id).unwrap(),
                                workflow.clone(),
                            ))
                            .unwrap();
                    }
                    workflow_result_v1::Status::Failed(x) => {
                        workflow.status = HistoryStatus::Failed(format!("{:#?}", x));
                        self.notification_tx
                            .send(Notification::WorkflowFailed(
                                Uuid::parse_str(&workflow_result.workflow_id).unwrap(),
                                workflow.clone(),
                            ))
                            .unwrap();
                    }
                    workflow_result_v1::Status::Cancelled(x) => {
                        workflow.status = HistoryStatus::Failed(format!("{:#?}", x));
                    }
                }

                {
                    let mut workers = self.workers.lock().await;
                    let worker = workers.get_mut(&workflow_result.worker_id).unwrap();
                    if worker.workflow_capacity < worker.max_workflow_capacity {
                        worker.workflow_capacity += 1;
                    }
                }

                self.history
                    .update_workflow(&workflow_result.workflow_id, workflow)
                    .await
                    .unwrap();
            }
            _ => {}
        }

        Ok(Response::new(()))
    }
    async fn start_activity(
        &self,
        request: Request<RequestStartActivityOptionsVersion>,
    ) -> Result<Response<ActivityResultVersion>, Status> {
        let activity_version = request.into_inner();
        match &activity_version.version {
            Some(request_start_activity_options_version::Version::V1(activity_options)) => {
                {
                    let mut activity_queues = self.activity_queue.lock().await;
                    match activity_queues.get_mut(&activity_options.task_queue) {
                        Some(queue) => {
                            queue.push_back(("0".to_string(), activity_options.clone()));
                        }
                        None => {
                            let mut queue = VecDeque::new();
                            queue.push_back(("0".to_string(), activity_options.clone()));
                            activity_queues.insert(activity_options.task_queue.clone(), queue);
                        }
                    }
                }

                let (tx, rx) = oneshot::channel::<ActivityResultV1>();
                {
                    let mut running_activities = self.running_activities.lock().await;

                    running_activities.insert(activity_options.activity_id.clone(), (None, tx));
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
                        s2.emit("history-update", serde_json::to_value(z).unwrap())
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
            let temp: String = serde_json::from_value(data.clone()).unwrap();
            ack.bin(bin);
            let s2 = socket.clone();
            let handle = tokio::spawn(async move {
                let mut last_id = "0-0".to_string();
                loop {
                    let opts = StreamReadOptions::default().block(500);

                    let srr: StreamReadReply = con
                        .xread_options(
                            &[format!("immortal:logs:{temp}")],
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
        activity_queue: Arc::new(Mutex::new(HashMap::new())),
        call_queue: Arc::new(Mutex::new(HashMap::new())),
        redis_pool: pool.clone(),
        history: History::new(&pool),
        // log_streams: (tx.clone(), Arc::clone(&log_streams)),
        workers: Arc::new(Mutex::new(HashMap::new())),
        running_activities: Arc::new(Mutex::new(HashMap::new())),
        running_calls: Arc::new(Mutex::new(HashMap::new())),
    };

    immortal_service.workflow_queue_thread();
    immortal_service.activity_queue_thread();
    immortal_service.call_queue_thread();
    let svc = ImmortalServer::new(immortal_service.clone());
    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<ImmortalServer<ImmortalService>>()
        .await;

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

    let con = pool.get().await.unwrap();
    Server::builder()
        .add_service(svc)
        .add_service(health_service)
        .serve(addr)
        .await?;

    Ok(())
}
