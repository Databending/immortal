use anyhow::Context;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use bb8_redis::bb8::Pool;
use bb8_redis::RedisConnectionManager;
use dashmap::{DashMap, Entry};
use redis::aio::MultiplexedConnection;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use simd_json::prelude::ValueAsMutObject;
use simd_json::OwnedValue;
use socketioxide::extract::{AckSender, State};
use socketioxide::extract::{Data, SocketRef};
use socketioxide::socket::DisconnectReason;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::history::WorkflowHistoryVersion;
use crate::metrics::IdentifiableMetrics;
use crate::{ImmortalService, Notification};
use redis::streams::{StreamId, StreamKey, StreamReadOptions, StreamReadReply};

#[derive(Deserialize, Clone)]
struct MetricsRequest {
    pub workers: Vec<String>,
}

impl MetricsRequest {
    fn to_room_id_hashed(&self) -> String {
        let mut workers = self.workers.clone();
        workers.sort();
        let json = simd_json::to_string(&workers).unwrap();
        let mut hasher = DefaultHasher::new();
        json.hash(&mut hasher);
        format!("metrics:{}", hasher.finish())
    }
}

#[derive(Deserialize, Serialize, Clone)]
#[serde(tag = "type", content = "spec")]
pub enum FetchLogs {
    Workflow(FetchWorkflowLogs),
    Worker(Vec<String>),
    TaskQueue(Vec<String>),
}

impl FetchLogs {
    fn to_room_id_hashed(&self) -> String {
        let normalized_struct = match self {
            FetchLogs::Worker(worker) => {
                let mut workers = worker.clone();
                workers.sort();
                &FetchLogs::Worker(workers)
            }
            FetchLogs::TaskQueue(task_queues) => {
                let mut task_queues = task_queues.clone();
                task_queues.sort();
                &FetchLogs::Worker(task_queues)
            }
            FetchLogs::Workflow(workflow) => &FetchLogs::Workflow(workflow.clone()),
        };
        let json = simd_json::to_string(normalized_struct).unwrap();
        let mut hasher = DefaultHasher::new();
        json.hash(&mut hasher);
        format!("logs:{}", hasher.finish())
    }
}

#[derive(Deserialize, Serialize, Clone)]
pub struct FetchWorkflowLogs {
    pub workflow_id: String,
    pub activity_id: Option<String>,
    pub run_id: Option<String>,
}

#[derive(Serialize)]
struct MetricsResponse {
    pub worker_id: String,
    pub cpu_pct: f32,
    pub mem_used: u64,
    pub mem_total: u64,
}

async fn read_and_send_log(
    room_name: &Option<String>,
    parsed_map: &mut simd_json::OwnedValue,
    map: &HashMap<String, redis::Value>,
    io: &SocketRef,
) -> anyhow::Result<()> {
    for (n, s) in map.clone() {
        if let redis::Value::BulkString(mut bytes) = s {
            if n == "metadata" {
                parsed_map
                    .as_object_mut() // Get a mutable reference to the underlying Map
                    .unwrap() // Panics if not an object, which we've initialized it to be
                    .insert(
                        n.to_owned(), // Convert String n to OwnedValue::String for the key, or use n.as_str()
                        simd_json::from_slice(&mut bytes).unwrap_or(OwnedValue::default()),
                    );
            } else {
                parsed_map.as_object_mut().unwrap().insert(
                    n.to_owned(), // Convert String n to OwnedValue::String for the key
                    OwnedValue::String(String::from_utf8(bytes)?),
                );
            }
        } else {
            panic!("Weird data")
        }
    }

    match room_name {
        Some(room_name) => {
            // println!("sending to {room_name}");
            // println!("listening rooms: {:?}", io.rooms());

            let _x = io
                .within(room_name.clone())
                .emit("log-back", &parsed_map)
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string())) // convert to anyhow::Error
                .context("emitting log-back")?;
            // println!("{:#?}", x);
        }
        None => {
            io.emit("log-back", &parsed_map)?
            // println!("{:#?}", x);
        }
    }

    Ok(())
}

pub fn merge_last_ids(last_ids: &mut HashMap<String, String>, workflow_ids: &Vec<String>) {
    for workflow_id in workflow_ids {
        if !last_ids.contains_key(workflow_id) {
            last_ids.insert(workflow_id.clone(), "0-0".to_string());
        }
    }
}

pub fn sort_last_ids(
    last_ids: &HashMap<String, String>,
    workflow_ids: &Vec<String>,
) -> Vec<String> {
    workflow_ids
        .iter()
        .filter_map(|k| last_ids.get(k).cloned())
        .collect()
}
// --------- Types ----------
#[derive(Clone, Default)]
pub struct WsState {
    // jobId -> shared producer
    producers: Arc<DashMap<String, Producer>>,
    // jobId -> subscriber count
    subs: Arc<DashMap<String, usize>>,
    // socket.id -> set of jobIds this socket subscribed to
    by_socket: Arc<DashMap<String, HashSet<String>>>,
}

#[derive(Debug)]
struct Producer {
    cancel: CancellationToken,
    _handle: JoinHandle<()>,
}

// Helper to compute room name
fn room(job_id: &str, r#type: &str) -> String {
    format!("room:{type}:{job_id}")
}

async fn start_metrics_producer(
    state: &WsState,
    io: SocketRef,
    metrics_request: MetricsRequest,
    stream_tx: broadcast::Sender<IdentifiableMetrics>,
) {
    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();

    let room_name = room(&metrics_request.to_room_id_hashed(), "metrics");
    let producer_name = metrics_request.to_room_id_hashed();
    let handle;
    {
        let room_name = room_name.clone();

        handle = tokio::spawn(async move {
            // let room_name = room_name.clone();
            // Example: simple polling loop; replace with your stream/reader
            let mut sub = stream_tx.subscribe();

            loop {
                let room_name = room_name.clone();
                tokio::select! {
                    // Cancellation signal
                    _ = cancel_child.cancelled() => break,
                    // New metric sample
                    res = sub.recv() => {
                        match res {
                            Ok(sample) => {
                                if metrics_request.workers.contains(&"all".to_string())
                                    || metrics_request.workers.contains(&sample.worker_id)
                                {
                                    if let Err(e) = io.within(room_name).emit(
                                        "metrics-back",
                                        &MetricsResponse {
                                            worker_id: sample.worker_id,
                                            cpu_pct: sample.cpu_pct,
                                            mem_used: sample.mem_used,
                                            mem_total: sample.mem_total,
                                        },
                                    ).await {
                                        println!("emit error: {e:#?}");
                                        break;
                                    }
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                println!("lagged {n} messages in metrics stream");
                                continue; // skip and keep going
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                println!("metrics channel closed");
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    state.producers.insert(
        producer_name,
        Producer {
            cancel,
            _handle: handle,
        },
    );
    // println!("inserted {room_name}");
}

async fn start_log_producer(
    state: &WsState,
    io: SocketRef,
    fetch_logs: FetchLogs,
    pool: Pool<RedisConnectionManager>,
    immortal_service: &Arc<ImmortalService>,
    last_ids: HashMap<String, String>,
    last_id: String,
) {
    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();

    let room_name = room(&fetch_logs.to_room_id_hashed(), "logs");
    let producer_name = fetch_logs.to_room_id_hashed();
    let handle;
    let con = pool.get().await.unwrap().clone();
    {
        let room_name = room_name.clone();
        let mut con = con.clone();
        let immortal_service = immortal_service.clone();
        handle = tokio::spawn(async move {
            let room_name = room_name.clone();
            // Example: simple polling loop; replace with your stream/reader
            let mut last_id = last_id.clone();
            let mut last_ids = last_ids.clone();
            loop {
                tokio::select! {
                                    _ = cancel_child.cancelled() => break,
                                    _ = tokio::time::sleep(Duration::from_millis(400)) => {
                                        // Fetch new lines and broadcast to room
                fetch_logs_from_redis(
                    &mut last_ids,
                    &mut last_id,
                    &fetch_logs,
                    &mut con,
                    &Some(room_name.clone()),
                    &io,
                    &immortal_service)
                    .await
                                        }
                                                }
            }
        });
    }

    state.producers.insert(
        producer_name,
        Producer {
            cancel,
            _handle: handle,
        },
    );
    // println!("inserted {room_name}");
}

fn dec_and_maybe_stop(state: &WsState, job_id: &str) {
    // decrement refcount safely
    let should_stop = {
        let mut stop = false;
        if let Some(mut e) = state.subs.get_mut(job_id) {
            if *e > 1 {
                *e -= 1;
            } else {
                // last subscriber leaving
                *e = 0;
                stop = true;
            }
        }
        stop
    };

    if should_stop {
        // println!("stopping {job_id}");
        // remove count
        state.subs.remove(job_id);
        // println!("producers {:#?}", state.producers.get(job_id));
        // stop and remove producer
        if let Some((_, prod)) = state.producers.remove(job_id) {
            // println!("cancelling");
            prod.cancel.cancel();
            // detach: the task will end on next tick; no need to await here
        }
    }
}

async fn fetch_logs_from_redis(
    last_ids: &mut HashMap<String, String>,
    last_id: &mut String,
    fetch_logs: &FetchLogs,
    con: &mut MultiplexedConnection,
    room_name: &Option<String>,
    io: &SocketRef,
    immortal_service: &Arc<ImmortalService>,
) {
    // println!("LAST ID: {last_id}");
    let opts = StreamReadOptions::default().block(500);
    match fetch_logs {
        FetchLogs::Workflow(ref workflow) => {
            let srr: StreamReadReply = con
                .xread_options(
                    &[format!("immortal:logs:{}", workflow.workflow_id)],
                    &[last_id.as_str()],
                    &opts,
                )
                .await
                .expect("read");
            for StreamKey { key: _, ids } in srr.keys {
                for StreamId { id, map } in ids {
                    *last_id = id.clone();
                    let mut parsed_map = simd_json::json!({
                        "id": id.clone()
                    });

                    if let Some(activity_id) = &workflow.activity_id {
                        if let Some(log_activity_id) = map.get("activity_id") {
                            if let redis::Value::BulkString(bytes) = log_activity_id.clone() {
                                let log_activity_id: String = String::from_utf8(bytes).unwrap();
                                if *activity_id != log_activity_id {
                                    continue;
                                }
                            }
                        } else {
                            continue;
                        }
                    }
                    if let Some(activity_run_id) = &workflow.run_id {
                        if let Some(log_activity_id) = map.get("activity_run_id") {
                            if let redis::Value::BulkString(bytes) = log_activity_id.clone() {
                                let log_activity_id: String = String::from_utf8(bytes).unwrap();
                                if *activity_run_id != log_activity_id {
                                    continue;
                                }
                            }
                        } else {
                            continue;
                        }
                    }
                    let mut count = 0;
                    while count < 3 {
                        let value = read_and_send_log(&room_name, &mut parsed_map, &map, &io);
                        match value.await {
                            Ok(_) => break,
                            Err(_) => {
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                count += 1
                            }
                        }
                    }
                }
            }
        }
        FetchLogs::Worker(ref worker_ids) => {
            let workflows = immortal_service
                .history
                .get_workflows(Some(1000), Some(0), None, Some(worker_ids.to_vec()), None)
                .await
                .unwrap();
            let workflow_ids = workflows
                .iter()
                .map(|f| match f {
                    WorkflowHistoryVersion::V1(v1) => v1.workflow_id.clone(),
                })
                .collect::<Vec<_>>();
            merge_last_ids(last_ids, &workflow_ids);
            let srr: StreamReadReply = con
                .xread_options(
                    &workflow_ids
                        .iter()
                        .map(|f| format!("immortal:logs:{}", f))
                        .collect::<Vec<_>>(),
                    &sort_last_ids(&last_ids, &workflow_ids),
                    &opts,
                )
                .await
                .expect("read");
            for StreamKey { key, ids } in srr.keys {
                for StreamId { id, map } in ids {
                    let key_id = key.split("immortal:logs:").collect::<Vec<_>>();
                    let key_id = key_id.get(1).unwrap();
                    if let Some(last_id) = last_ids.get_mut(&key_id.to_string()) {
                        *last_id = id.clone();
                    }
                    // last_ids.get_mut("key") = id.clone();
                    let mut parsed_map = simd_json::json!({
                        "id": id.clone()
                    });
                    read_and_send_log(&room_name, &mut parsed_map, &map, &io)
                        .await
                        .unwrap();
                }
            }
        }
        FetchLogs::TaskQueue(ref _task_queues) => {}
    }
}

pub async fn on_connect(
    socket: SocketRef,
    Data(_data): Data<OwnedValue>,

    state: State<WsState>,
    _pool: State<Pool<RedisConnectionManager>>,
    notification_tx: State<broadcast::Sender<Notification>>,
    _stream_tx: State<broadcast::Sender<IdentifiableMetrics>>,
    _immortal_service: State<Arc<ImmortalService>>,
) {
    println!("Socket.IO connected: {:?} {:?}", socket.ns(), socket.id);

    // println!("Data = {:?}", data);
    // socket.emit("auth", data).ok();
    // let pool = pool.clone();
    // con.

    // let a: () = con
    //     .xgroup_create_mkstream("immortal::logs", "immortal::logs::group", "$")
    //     .await
    //     .unwrap();

    socket.on("message", |socket: SocketRef, Data::<OwnedValue>(data)| {
        info!("Received event: {:?}", data);
        socket.emit("message-back", &data).ok();
    });

    socket.on(
        "message-with-ack",
        |Data::<OwnedValue>(data), ack: AckSender| {
            info!("Received event: {:?}", data);
            ack.send(&data).ok();
        },
    );

    socket.join("history-update");
    {
        let tx = notification_tx.clone();

        let s2 = socket.clone();

        let mut rx = tx.clone().subscribe();
        let count_now = {
            match state.subs.entry("history-update".to_string()) {
                Entry::Occupied(mut e) => {
                    *e.get_mut() += 1;
                    *e.get()
                }
                Entry::Vacant(v) => {
                    v.insert(1);
                    1
                }
            }
        };

        if count_now == 1 {
            println!("spawning");
            tokio::spawn(async move {
                let cancel = CancellationToken::new();
                let cancel_child = cancel.clone();
                loop {
                    tokio::select! {
                                            _ = cancel_child.cancelled() => break,

                                            _ = tokio::time::sleep(Duration::from_millis(400)) => {
                    if let Ok(z) = rx.recv().await {
                        // println!("RECEIVED EVENT");
                                        s2.within("history-update")
                                            .emit(
                                                "history-update",
                                                &simd_json::serde::to_owned_value(z).unwrap_or(simd_json::json!({})),
                                            )
                                            .await
                                            .ok();
                                    }
                                            }
                                        }
                }
            });
        }
        socket.on(
            "history-notifications",
            |_socket: SocketRef, Data::<OwnedValue>(data), ack: AckSender| async move {
                // info!("Received event: {:?} ", data);
                // println!("Received event: {:?} ", data);
                ack.send(&data).ok();

                // socket.join("history-notifications");
                // s2.join("history-update");
                // let mut rx = tx.clone().subscribe();
                // let handle = tokio::spawn(async move {
                //     while let Ok(z) = rx.recv().await {
                //         s2.emit(
                //             "history-update",
                //             &simd_json::serde::to_owned_value(z).unwrap_or(simd_json::json!({})),
                //         )
                //         .ok();
                //     }
                //
                //     // info!("Stream ended");
                // });
                //
                // info!("Stream ended");
                // let abort = handle.abort_handle();
                // socket.on_disconnect(|_socket: SocketRef, _reason: DisconnectReason| async move {
                //     // sink.unsubscribe("immortal::logs").await.unwrap();
                //     abort.abort();
                //     println!("aborting stream");
                //     // handle.abort();
                // })
            },
        );
    }

    // LOGS
    socket.on(
        "logs:subscribe",
        async move |socket: SocketRef,
                    Data::<FetchLogs>(req),
                    state: State<WsState>,
                    pool: State<Pool<RedisConnectionManager>>,
                    immortal_service: State<Arc<ImmortalService>>| {
            let job_id = req.to_room_id_hashed();
            let room_name = room(&job_id, "logs");

            // println!("joining room {room_name}");
            // join room
            socket.join(room_name.clone());

            // println!("room joined");
            // track per-socket subscriptions
            state
                .by_socket
                .entry(socket.id.to_string())
                .or_default()
                .insert(job_id.clone());

            // println!("state updated");
            // bump refcount
            let count_now = {
                match state.subs.entry(job_id.clone()) {
                    Entry::Occupied(mut e) => {
                        *e.get_mut() += 1;
                        *e.get()
                    }
                    Entry::Vacant(v) => {
                        v.insert(1);
                        1
                    }
                }
            };
            let last_id = "$".to_string();
            let last_ids = HashMap::new();
            // println!("fetching from pool");
            // let mut con = pool.get().await.unwrap();
            // println!("fetching logs");
            // fetch_logs_from_redis(
            //     &mut last_ids,
            //     &mut last_id,
            //     &req,
            //     &mut con,
            //     &None,
            //     &socket,
            //     &immortal_service,
            // )
            // .await;

            // println!("fetched {last_id}");
            // start producer if first subscriber
            if count_now == 1 {
                start_log_producer(
                    &state,
                    socket,
                    req.clone(),
                    pool.clone(),
                    &immortal_service,
                    last_ids,
                    last_id,
                )
                .await;
            }
        },
    );
    socket.on(
        "logs:unsubscribe",
        move |socket: SocketRef, Data::<FetchLogs>(req), state: State<WsState>| {
            let job_id = req.to_room_id_hashed();
            let room_name = room(&job_id, "logs");

            // println!("unsubscribing {room_name}");
            // leave room
            socket.leave(room_name);

            // update per-socket map
            if let Some(mut set) = state.by_socket.get_mut(&socket.id.to_string()) {
                set.remove(&job_id);
            }

            dec_and_maybe_stop(&state, &job_id);
        },
    );

    // METRICS

    socket.on(
        "metrics:subscribe",
        async move |socket: SocketRef,
                    Data::<MetricsRequest>(req),
                    state: State<WsState>,
                    stream_tx: State<broadcast::Sender<IdentifiableMetrics>>| {
            let job_id = req.to_room_id_hashed();
            let room_name = room(&job_id, "metrics");

            // println!("joining room {room_name}");
            // join room
            socket.join(room_name.clone());

            // track per-socket subscriptions
            state
                .by_socket
                .entry(socket.id.to_string())
                .or_default()
                .insert(job_id.clone());

            // bump refcount
            let count = state
                .subs
                .entry(job_id.clone())
                .and_modify(|c| *c += 1)
                .or_insert(1usize);
            let count_now = *count;

            // start producer if first subscriber
            if count_now == 1 {
                start_metrics_producer(&state, socket, req.clone(), stream_tx.clone()).await;
            }
        },
    );
    socket.on(
        "metrics:unsubscribe",
        move |socket: SocketRef, Data::<MetricsRequest>(req), state: State<WsState>| {
            let job_id = req.clone().to_room_id_hashed();

            let room_name = room(&job_id, "metrics");

            println!("unsubscribing {room_name}");
            // leave room
            socket.leave(room_name);

            // update per-socket map
            if let Some(mut set) = state.by_socket.get_mut(&socket.id.to_string()) {
                set.remove(&job_id);
            }

            dec_and_maybe_stop(&state, &job_id);
        },
    );

    //
    // // Clean up on disconnect (remove socket from all its job rooms)
    // socket.on_disconnect(move |socket, state: State<AppState>, _reason| {
    //     if let Some(set) = state.by_socket.remove(&sid).map(|(_, s)| s) {
    //         for job_id in set {
    //             // Leave the room if still in it (safe to call)
    //             socket.leave(room(&job_id));
    //             dec_and_maybe_stop(&state, &job_id, socket.io());
    //         }
    //     }
    // });
    // let s2 = socket.clone();

    // let abort = handle.abort_handle();
    socket.on_disconnect(
        |socket: SocketRef, reason: DisconnectReason, state: State<WsState>| async move {
            // sink.unsubscribe("immortal::logs").await.unwrap();
            // abort.abort();
            println!(
                "Socket {} on ns {} disconnected, reason: {:?}",
                socket.id,
                socket.ns(),
                reason
            );
            if let Some(set) = state
                .by_socket
                .remove(&socket.id.to_string())
                .map(|(_, s)| s)
            {
                for job_id in set {
                    // Leave the room if still in it (safe to call)
                    {
                        socket.leave(room(&job_id, "logs"));
                        dec_and_maybe_stop(&state, &job_id);
                    }
                    {
                        socket.leave(room(&job_id, "metrics"));
                        dec_and_maybe_stop(&state, &job_id);
                    }
                    {
                        socket.leave("history-update");
                        dec_and_maybe_stop(&state, "history-update");
                    }
                }
            }
            // handle.abort();
        },
    )
}
