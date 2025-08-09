use std::collections::HashMap;

use bb8_redis::RedisConnectionManager;
use redis::AsyncCommands;
use serde::Deserialize;
use simd_json::OwnedValue;
use socketioxide::extract::{AckSender, State};
use socketioxide::extract::{Data, SocketRef};
use socketioxide::socket::DisconnectReason;

use bb8_redis::bb8::Pool;
use simd_json::prelude::ValueAsMutObject;
use tokio::sync::broadcast::Sender;
use tokio::sync::{broadcast, watch};
use tracing::info;

use crate::{ImmortalService, Notification};
use immortal_worker_lib::metrics::Metrics;
use redis::streams::{StreamId, StreamKey, StreamReadOptions, StreamReadReply};

#[derive(Deserialize)]
struct MetricsRequest {
    pub workers: Vec<String>,
}

pub async fn on_connect(
    socket: SocketRef,
    Data(_data): Data<OwnedValue>,
    pool: State<Pool<RedisConnectionManager>>,
    notification_tx: State<broadcast::Sender<Notification>>,
    _latest_rx: State<watch::Receiver<Metrics>>,
    stream_tx: State<broadcast::Sender<Metrics>>,
    immortal_service: State<ImmortalService>,
) {
    info!("Socket.IO connected: {:?} {:?}", socket.ns(), socket.id);
    // println!("Data = {:?}", data);
    // socket.emit("auth", data).ok();
    // let pool = pool.clone();
    let mut con = pool.get().await.unwrap().clone();
    // con.

    {
        let stream_tx = stream_tx.clone();
        let workers;
        {
            let workers_temp = immortal_service.workers.read().await;
            workers = (*workers_temp)
                .iter()
                .map(|f| (f.0.clone(), f.1.metrics_stream.clone()))
                .collect::<HashMap<String, Sender<Metrics>>>();
        }

        socket.on(
            "metrics",
            |socket: SocketRef, Data::<OwnedValue>(data)| async move {
                println!("here");
                let metrics_request: MetricsRequest =
                    simd_json::serde::from_owned_value(data.clone()).unwrap();
                if metrics_request.workers.contains(&"server".to_string()) {
                    tokio::spawn(async move {
                        let mut sub = stream_tx.subscribe();
                        while let Ok(sample) = sub.recv().await {
                            match socket
                                .emit("metrics-back", &serde_json::to_value(sample).unwrap())
                            {
                                Ok(_) => {}
                                Err(e) => {
                                    println!("{:#?}", e);
                                    break;
                                }
                            }
                        }
                    });
                } else {
                    for worker in metrics_request.workers {
                        if let Some(sender) = workers.get(&worker) {
                            let mut sub = sender.subscribe();
                            let socket = socket.clone();
                            tokio::spawn(async move {
                                while let Ok(sample) = sub.recv().await {
                                    match socket.emit(
                                        "metrics-back",
                                        &serde_json::to_value(sample).unwrap(),
                                    ) {
                                        Ok(_) => {}
                                        Err(e) => {
                                            println!("{:#?}", e);
                                            break;
                                        }
                                    }
                                }
                            });
                        }
                    }
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
            let log_id: String = simd_json::serde::from_owned_value(data.clone()).unwrap();
            ack.send(&data).ok();
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
                            let mut parsed_map = simd_json::json!({
                                "id": id.clone()
                            });
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
                            s2.emit("message-back", &parsed_map).ok();
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
