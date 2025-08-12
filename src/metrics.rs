use std::{collections::VecDeque, sync::Arc, time::Duration};
use serde::Serialize;
use sysinfo::{System};
use tokio::time::{interval, MissedTickBehavior};

use tokio::sync::{broadcast, RwLock};
#[derive(Clone, Serialize, Debug)]
pub struct IdentifiableMetrics {
    // pub ts_ms: u128,

    pub worker_id: String,
    pub cpu_pct: f32,
    pub mem_used: u64,
    pub mem_total: u64,
}

pub async fn server_sampler(
    // latest_tx: watch::Sender<IdentifiableMetrics>,
    stream_tx: broadcast::Sender<IdentifiableMetrics>,
    history: Arc<RwLock<VecDeque<IdentifiableMetrics>>>,
) {
    let mut sys = System::new_all();
    let mut tick = interval(Duration::from_millis(1000));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tick.tick().await;
        sys.refresh_memory();
        sys.refresh_cpu_usage(); // sysinfo computes delta since last refresh

        let cpu_pct = avg_cpu_pct(&sys);
        let mem_used = sys.used_memory();
        let mem_total = sys.total_memory();

        let sample = IdentifiableMetrics {
            worker_id: "server".to_string(),
            // ts_ms: chrono::Utc::now().timestamp_millis() as u128,
            cpu_pct,
            mem_used,
            mem_total,
        };

        // update latest + stream
        // let _ = latest_tx.send(sample.clone());
        let _ = stream_tx.send(sample.clone());
        // println!("sending {:#?}", sample);

        // keep short history (bounded)
        {
            let mut buf = history.write().await;
            if buf.len() == buf.capacity() { buf.pop_front(); }
            buf.push_back(sample);
        }
    }
}

fn avg_cpu_pct(sys: &System) -> f32 {
    // Average across CPUs; sysinfo gives 0..100 per CPU
    let mut sum = 0.0;
    let mut n = 0;
    for cpu in sys.cpus() {
        sum += cpu.cpu_usage() as f32;
        n += 1;
    }
    if n == 0 { 0.0 } else { sum / n as f32 }
}
