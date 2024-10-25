// pub mod immortal {
//     tonic::include_proto!("immortal");
// }
use ::immortal::common;
use ::immortal::failure;
use ::immortal::immortal;
use ::immortal::models::history::Status as HistoryStatus;
use ::immortal::models::history::{
    ActivityHistory, ActivityRun, History, WorkflowHistory, WorkflowRun,
};
use axum;
use axum::http::Method;
use bb8_redis::bb8::Pool;
use chrono::NaiveDateTime;
use redis::streams::{StreamId, StreamKey, StreamMaxlen, StreamReadOptions, StreamReadReply};
use redis::AsyncCommands;
// use bb8_redis::redis::AsyncCommands;
use bb8_redis::{bb8, RedisConnectionManager};
use redis::Client;

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
use serde_json::Value;
use socketioxide::extract::{AckSender, Bin, State};
use socketioxide::socket::DisconnectReason;
use socketioxide::{
    extract::{Data, SocketRef},
    SocketIo,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tonic::transport::Server;
use tonic_health::server::HealthReporter;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;
use tracing_subscriber::FmtSubscriber;
use uuid::Uuid;
// use immortal::immortal_s
// use immortal::im::{Server, ServerServer};
// use routeguide::route_guide_server::{RouteGuide, RouteGuideServer};
// use routeguide::worker_action::Action;
// use routeguide::{Feature, Point, Rectangle, RouteNote, RouteSummary};

use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
pub mod api;
pub mod models;

#[derive(Debug)]
struct RegisteredWorker {
    worker_id: String,

    incoming: JoinHandle<()>,
    tx: Sender<Result<ImmortalWorkerActionVersion, Status>>,
    registered_workflows: Vec<String>,
    registered_activities: Vec<String>,
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
pub struct ImmortalService {
    redis_pool: bb8::Pool<RedisConnectionManager>,
    workers: Arc<Mutex<HashMap<String, RegisteredWorker>>>,
    // log_streams: (
    //     broadcast::Sender<LogStreamUpdate>,
    //     Arc<Mutex<HashMap<String, LogStream>>>,
    // ),
    history: Arc<Mutex<History>>,
    running_activities: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<ActivityResultV1>>>>,
}

impl ImmortalService {
    pub async fn start_workflow_internal(
        &self,
        workflow_options: ClientStartWorkflowOptionsVersion,
    ) -> Result<(String, String), Status> {
        Ok(match workflow_options.version {
            Some(client_start_workflow_options_version::Version::V1(workflow_options)) => {
                let workers = self.workers.lock().await;
                let worker = match workers.iter().find(|(_, worker)| {
                    worker
                        .registered_workflows
                        .contains(&workflow_options.workflow_type)
                }) {
                    Some((_, worker)) => worker,
                    None => {
                        return Err(Status::not_found("Worker not found"));
                    }
                };
                let mut history = self.history.lock().await;
                let workflow_id = workflow_options
                    .workflow_id
                    .clone()
                    .unwrap_or(Uuid::new_v4().to_string());

                let mut workflow_history = WorkflowHistory::new(
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
                workflow_history.add_run(WorkflowRun::new("0".to_string()));
                history.add_workflow(workflow_history);
                worker
                    .tx
                    .send(Ok(ImmortalWorkerActionVersion {
                        version: Some(immortal_worker_action_version::Version::V1(
                            ImmortalWorkerActionV1 {
                                action: Some(WorkerAction::StartWorkflow(StartWorkflowOptionsV1 {
                                    workflow_id: workflow_id.clone(),
                                    workflow_type: workflow_options.workflow_type.clone(),
                                    workflow_version: workflow_options.workflow_version.clone(),
                                    task_queue: workflow_options.task_queue.clone(),

                                    input: workflow_options.input.clone(),
                                    workflow_run_id: "0".to_string(),
                                })),
                            },
                        )),
                    }))
                    .await
                    .unwrap();
                (workflow_id, "0".to_string())
            }
            _ => {
                return Err(Status::internal("unsupported version"));
            }
        })
    }
}

#[tonic::async_trait]
impl Immortal for ImmortalService {
    type RegisterWorkerStream = ReceiverStream<Result<ImmortalWorkerActionVersion, Status>>;
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
                            let when = NaiveDateTime::from_timestamp(log.when, 0).to_string();
                            let mut items = vec![
                                ("workflow_id", &log.workflow_id),
                                ("workflow_run_id", &log.workflow_run_id),
                                ("message", &log.message),
                                ("when", &when),
                            ];
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
                            println!("Log = {:?}", log);
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
        let worker_details = worker_details.unwrap();

        workers.insert(
            worker_details.worker_id.clone(),
            RegisteredWorker {
                incoming: handle,
                tx: tx.clone(),
                worker_id: worker_details.worker_id.clone(),
                registered_workflows: worker_details.registered_workflows.clone(),
                registered_activities: worker_details.registered_activities.clone(),
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

    async fn start_workflow(
        &self,
        request: Request<ClientStartWorkflowOptionsVersion>,
    ) -> Result<Response<ClientStartWorkflowResponse>, Status> {
        let workflow_options = request.into_inner();
        let (workflow_id, workflow_run_id) = self.start_workflow_internal(workflow_options).await?;

        Ok(Response::new(ClientStartWorkflowResponse {
            workflow_id,
            workflow_run_id,
        }))
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
                    Some(tx) => {
                        tx.send(activity_result.clone()).unwrap();
                    }
                    None => {
                        return Err(Status::not_found("Activity not found"));
                    }
                }
                let mut history = self.history.lock().await;
                let run = history
                    .get_activity_mut(&activity_result.activity_id)
                    .unwrap()
                    .get_run_mut(&activity_result.activity_run_id)
                    .unwrap();
                run.end_time = Some(NaiveDateTime::from_timestamp(
                    chrono::Utc::now().timestamp(),
                    0,
                ));
                match activity_result.status.unwrap() {
                    immortal::activity_result_v1::Status::Completed(x) => {
                        run.status = HistoryStatus::Completed(
                            serde_json::from_slice(&x.result.unwrap().data).unwrap(),
                        );
                    }
                    immortal::activity_result_v1::Status::Failed(x) => {
                        run.status = HistoryStatus::Failed(format!("{:#?}", x));
                    }
                    immortal::activity_result_v1::Status::Cancelled(x) => {
                        run.status = HistoryStatus::Failed(format!("{:#?}", x));
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
        match workflow_version.version {
            Some(workflow_result_version::Version::V1(workflow_result)) => {
                let mut history = self.history.lock().await;
                let run = history
                    .get_workflow_mut(&workflow_result.workflow_id)
                    .unwrap()
                    .get_run_mut(&workflow_result.workflow_run_id)
                    .unwrap();
                run.end_time = Some(NaiveDateTime::from_timestamp(
                    chrono::Utc::now().timestamp(),
                    0,
                ));

                println!("workflow_result = {:?}", workflow_result);
                match workflow_result.status.unwrap() {
                    workflow_result_v1::Status::Completed(x) => {
                        run.status = HistoryStatus::Completed(
                            serde_json::from_slice(&x.result.unwrap().data).unwrap(),
                        );
                    }
                    workflow_result_v1::Status::Failed(x) => {
                        run.status = HistoryStatus::Failed(format!("{:#?}", x));
                    }
                    workflow_result_v1::Status::Cancelled(x) => {
                        run.status = HistoryStatus::Failed(format!("{:#?}", x));
                    }
                }
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
                    let workers = self.workers.lock().await;
                    println!("workers = {:?}", workers);
                    let worker = match workers.iter().find(|(_, worker)| {
                        worker
                            .registered_activities
                            .contains(&activity_options.activity_type)
                    }) {
                        Some((_, worker)) => worker,
                        None => {
                            return Err(Status::not_found("Worker not found"));
                        }
                    };
                    worker
                        .tx
                        .send(Ok(ImmortalWorkerActionVersion {
                            version: Some(immortal_worker_action_version::Version::V1(
                                ImmortalWorkerActionV1 {
                                    action: Some(WorkerAction::StartActivity(
                                        immortal::StartActivityOptionsV1 {
                                            activity_id: activity_options.activity_id.clone(),
                                            activity_type: activity_options.activity_type.clone(),
                                            activity_input: activity_options.activity_input.clone(),
                                            workflow_id: activity_options.workflow_id.clone(),
                                            workflow_run_id: activity_options
                                                .workflow_run_id
                                                .clone(),
                                            activity_run_id: "0".to_string(),
                                        },
                                    )),
                                },
                            )),
                        }))
                        .await
                        .unwrap();
                }

                let (tx, rx) = oneshot::channel::<ActivityResultV1>();
                {
                    let mut running_activities = self.running_activities.lock().await;
                    running_activities.insert(activity_options.activity_id.clone(), tx);
                }

                let mut history = self.history.lock().await;
                let workflow = history
                    .get_workflow_mut(&activity_options.workflow_id)
                    .unwrap();
                let workflow_run = workflow
                    .get_run_mut(&activity_options.workflow_run_id)
                    .unwrap();
                let mut activity_history =
                    ActivityHistory::new(activity_options.activity_id.clone());
                activity_history
                    .runs
                    .push(ActivityRun::new("0".to_string()));
                workflow_run.activities.push(activity_history);
                match rx.await {
                    Ok(payload) => {
                        Ok(Response::new(ActivityResultVersion {
                            version: Some(activity_result_version::Version::V1(payload)),
                        }))
                    }
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
    Data(data): Data<Value>,
    pool: State<Pool<RedisConnectionManager>>,
) {
    info!("Socket.IO connected: {:?} {:?}", socket.ns(), socket.id);
    socket.emit("auth", data).ok();
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
    let s2 = socket.clone();
    let handle = tokio::spawn(async move {
        let mut last_id = "0-0".to_string();
        loop {
            let opts = StreamReadOptions::default().block(500);

            let srr: StreamReadReply = con
                .xread_options(&["immortal:logs"], &[last_id.as_str()], &opts)
                .await
                .expect("read");
            for StreamKey { key, ids } in srr.keys {
                println!("Stream {key}");
                for StreamId { id, map } in ids {
                    println!("\tID {id}");
                    last_id = id;
                    for (n, s) in map {
                        if let redis::Value::BulkString(bytes) = s {
                            println!("\t\t{}", n);

                            s2.emit("message-back", String::from_utf8(bytes).expect("utf8"))
                                .ok();
                        } else {
                            panic!("Weird data")
                        }
                    }
                }
            }
        }

        // info!("Stream ended");
    });
    let abort = handle.abort_handle();
    socket.on_disconnect(|socket: SocketRef, reason: DisconnectReason| async move {
        // sink.unsubscribe("immortal::logs").await.unwrap();
        abort.abort();
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
    tracing::subscriber::set_global_default(FmtSubscriber::default())?;
    let addr = "[::1]:10000".parse().unwrap();

    // let (tx, _rx) = broadcast::channel(100);

    // let log_streams = Arc::new(Mutex::new(HashMap::new()));
    let manager = RedisConnectionManager::new("redis://:pine5apple@172.24.10.200:30379").unwrap();
    let pool = bb8::Pool::builder().build(manager).await.unwrap();
    let immortal_service = ImmortalService {
        redis_pool: pool.clone(),
        history: Arc::new(Mutex::new(History::new())),
        // log_streams: (tx.clone(), Arc::clone(&log_streams)),
        workers: Arc::new(Mutex::new(HashMap::new())),
        running_activities: Arc::new(Mutex::new(HashMap::new())),
    };

    let svc = ImmortalServer::new(immortal_service.clone());
    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<ImmortalServer<ImmortalService>>()
        .await;

    tokio::spawn(service_status(health_reporter.clone()));

    let client = Client::open("redis://:pine5apple@172.24.10.200:30379?protocol=resp3").unwrap();
    {
        let cors = CorsLayer::new()
            // allow `GET` and `POST` when accessing the resource
            .allow_methods([Method::GET, Method::POST])
            // allow requests from any origin
            .allow_origin(Any);

        let (layer, io) = SocketIo::builder()
            // .with_state(tx.clone())
            // .with_state(log_streams)
            .with_state(pool)
            .with_state(client)
            .build_layer();
        io.ns("/", on_connect);
        let app = axum::Router::new()
            .nest("/api", api::router())
            .layer(cors)
            .layer(layer)
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
