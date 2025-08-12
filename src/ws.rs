use std::sync::Arc;

use bb8_redis::RedisConnectionManager;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use simd_json::OwnedValue;
use socketioxide::extract::{AckSender, State};
use socketioxide::extract::{Data, SocketRef};
use socketioxide::socket::DisconnectReason;

use bb8_redis::bb8::Pool;
use simd_json::prelude::ValueAsMutObject;
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;
use tracing::info;

use crate::metrics::IdentifiableMetrics;
use crate::Notification;
use redis::streams::{StreamId, StreamKey, StreamReadOptions, StreamReadReply};

#[derive(Deserialize)]
struct MetricsRequest {
    pub workers: Vec<String>,
}

#[derive(Deserialize)]
struct FetchLogs {
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

#[derive(Default)]
struct SocketState {
    subscribed: bool,
    forwarder: Option<JoinHandle<()>>,
}
pub async fn on_connect(
    socket: SocketRef,
    Data(_data): Data<OwnedValue>,
    pool: State<Pool<RedisConnectionManager>>,
    notification_tx: State<broadcast::Sender<Notification>>,
    stream_tx: State<broadcast::Sender<IdentifiableMetrics>>,
    // immortal_service: State<ImmortalService>,
) {
    println!("Socket.IO connected: {:?} {:?}", socket.ns(), socket.id);
    let per_socket = Arc::new(Mutex::new(SocketState::default()));

    // println!("Data = {:?}", data);
    // socket.emit("auth", data).ok();
    // let pool = pool.clone();
    let mut con = pool.get().await.unwrap().clone();
    // con.

    {
        let stream_tx = stream_tx.clone();

        let per_socket = per_socket.clone();

        socket.on(
            "metrics",
            |socket: SocketRef, Data::<OwnedValue>(data)| async move {
                println!("here");
                // {
                let per_socket = per_socket.clone();
                let mut guard = per_socket.lock().await;
                if guard.subscribed {
                    if let Some(forwarder) = &guard.forwarder {
                        forwarder.abort();
                    }
                    // already subscribed: noop (or emit ack)
                    // socket.emit("metrics:ack", "already-subscribed").ok();
                    // return;
                }
                guard.subscribed = true;
                // }

                let metrics_request: MetricsRequest =
                    simd_json::serde::from_owned_value(data.clone()).unwrap();
                {
                    let socket = socket.clone();
                    guard.forwarder = Some(tokio::spawn(async move {
                        let mut sub = stream_tx.subscribe();
                        while let Ok(sample) = sub.recv().await {
                            if metrics_request.workers.contains(&"all".to_string())
                                || metrics_request.workers.contains(&sample.worker_id)
                            {
                                match socket.emit(
                                    "metrics-back",
                                    &MetricsResponse {
                                        worker_id: sample.worker_id,
                                        cpu_pct: sample.cpu_pct,
                                        mem_used: sample.mem_used,
                                        mem_total: sample.mem_total,
                                    },
                                ) {
                                    Ok(_) => {}
                                    Err(e) => {
                                        println!("{:#?}", e);
                                        break;
                                    }
                                }
                            }
                        }
                    }));
                }
            },
        );
    }

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

    {
        let tx = notification_tx.clone();

        socket.on(
            "history-notifications",
            |socket: SocketRef, Data::<OwnedValue>(data), ack: AckSender| async move {
                info!("Received event: {:?} ", data);
                println!("Received event: {:?} ", data);
                ack.send(&data).ok();
                let s2 = socket.clone();
                let mut rx = tx.clone().subscribe();
                let handle = tokio::spawn(async move {
                    while let Ok(z) = rx.recv().await {
                        s2.emit(
                            "history-update",
                            &simd_json::serde::to_owned_value(z).unwrap_or(simd_json::json!({})),
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
        |socket: SocketRef, Data::<OwnedValue>(data), ack: AckSender| {
            info!("Received event: {:?}", data);
            let fetch_logs: FetchLogs = simd_json::serde::from_owned_value(data.clone()).unwrap();
            ack.send(&data).ok();
            let s2 = socket.clone();
            let handle = tokio::spawn(async move {
                let mut last_id = "0-0".to_string();
                loop {
                    let opts = StreamReadOptions::default().block(500);

                    let srr: StreamReadReply = con
                        .xread_options(
                            &[format!("immortal:logs:{}", fetch_logs.workflow_id)],
                            &[last_id.as_str()],
                            &opts,
                        )
                        .await
                        .expect("read");
                    for StreamKey { key: _, ids } in srr.keys {
                        for StreamId { id, map } in ids {
                            last_id = id.clone();
                            let mut parsed_map = simd_json::json!({
                                "id": id.clone()
                            });
                            if let Some(activity_id) = &fetch_logs.activity_id {
                                if let Some(log_activity_id) = map.get("activity_id") {
                                    if let redis::Value::BulkString(bytes) = log_activity_id.clone()
                                    {
                                        let log_activity_id: String =
                                            String::from_utf8(bytes).unwrap();
                                        if *activity_id != log_activity_id {
                                            continue;
                                        }
                                    }
                                }
                            }
                            if let Some(activity_run_id) = &fetch_logs.run_id {
                                if let Some(log_activity_id) = map.get("activity_run_id") {
                                    if let redis::Value::BulkString(bytes) = log_activity_id.clone()
                                    {
                                        let log_activity_id: String =
                                            String::from_utf8(bytes).unwrap();
                                        if *activity_run_id != log_activity_id {
                                            continue;
                                        }
                                    }
                                }
                            }

                            for (n, s) in map {
                                if let redis::Value::BulkString(mut bytes) = s {
                                    if n == "metadata" {
                                        parsed_map
                                            .as_object_mut() // Get a mutable reference to the underlying Map
                                            .unwrap() // Panics if not an object, which we've initialized it to be
                                            .insert(
                                                n.to_owned(), // Convert String n to OwnedValue::String for the key, or use n.as_str()
                                                simd_json::from_slice(&mut bytes)
                                                    .unwrap_or(OwnedValue::default()),
                                            );
                                    } else {
                                        parsed_map.as_object_mut().unwrap().insert(
                                            n.to_owned(), // Convert String n to OwnedValue::String for the key
                                            OwnedValue::String(String::from_utf8(bytes).unwrap()),
                                        );
                                    }
                                } else {
                                    panic!("Weird data")
                                }
                            }
                            s2.emit("log-back", &parsed_map).ok();
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
