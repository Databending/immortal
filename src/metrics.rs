// SERVER
use serde::Serialize;
use std::{collections::VecDeque, sync::Arc, time::Duration};
use sysinfo::{get_current_pid, System};
use tokio::fs;
use tokio::sync::{broadcast, RwLock};
use tokio::time::{interval, MissedTickBehavior};
#[derive(Clone, Serialize, Debug)]
pub struct IdentifiableMetrics {
    // pub ts_ms: u128,
    pub worker_id: String,
    pub cpu_pct: f32,
    pub mem_used: u64,
    pub mem_total: u64,
}

async fn get_container_memory_limit() -> Option<u64> {
    // Try cgroups v2 first
    let v2_path = "/sys/fs/cgroup/memory.max";
    if let Ok(data) = fs::read_to_string(v2_path).await {
        let val = data.trim();
        if val != "max" {
            if let Ok(limit) = val.parse::<u64>() {
                return Some(limit);
            }
        }
    }

    // Try cgroups v1
    let v1_path = "/sys/fs/cgroup/memory/memory.limit_in_bytes";
    if let Ok(data) = fs::read_to_string(v1_path).await {
        if let Ok(limit) = data.trim().parse::<u64>() {
            return Some(limit);
        }
    }

    None
}

async fn get_total_mem(cgroup_exists: bool, sys: &System) -> u64 {
    if cgroup_exists {
        if let Some(limit) = get_container_memory_limit().await {
            return limit;
        }
    }
    sys.total_memory()
}

async fn get_sample(cgroup_exists: bool, sys: &System) -> IdentifiableMetrics {
    if let Ok(current_pid) = get_current_pid() {
        if let Some(process) = sys.process(current_pid) {
            let mem_total = get_total_mem(cgroup_exists, &sys).await;
            return IdentifiableMetrics {
                worker_id: "server".to_string(),
                // ts_ms: chrono::Utc::now().timestamp_millis() as u128,
                cpu_pct: process.cpu_usage(),
                mem_used: process.memory(),
                mem_total,
            };
        }
    }
    let cpu_pct = avg_cpu_pct(&sys);
    let mem_used = sys.used_memory();
    let mem_total = sys.total_memory();

    IdentifiableMetrics {
        worker_id: "server".to_string(),
        // ts_ms: chrono::Utc::now().timestamp_millis() as u128,
        cpu_pct,
        mem_used,
        mem_total,
    }
}

pub async fn server_sampler(
    // latest_tx: watch::Sender<IdentifiableMetrics>,
    stream_tx: broadcast::Sender<IdentifiableMetrics>,
    history: Arc<RwLock<VecDeque<IdentifiableMetrics>>>,
) {
    let mut sys = System::new_all();
    let mut tick = interval(Duration::from_millis(1000));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let cgroup_exists = get_container_memory_limit().await.is_some();

    loop {
        tick.tick().await;
        sys.refresh_memory();
        sys.refresh_cpu_usage(); // sysinfo computes delta since last refresh
        let sample = get_sample(cgroup_exists, &sys).await;

        let _ = stream_tx.send(sample.clone());

        {
            let mut buf = history.write().await;
            if buf.len() == buf.capacity() {
                buf.pop_front();
            }
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
    if n == 0 {
        0.0
    } else {
        sum / n as f32
    }
}
