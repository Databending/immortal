// WORKER
use serde::Serialize;
use std::path::Path;
use std::time::Instant;
use std::{collections::VecDeque, sync::Arc, time::Duration};
use sysinfo::{System, get_current_pid};
use tokio::fs;
use tokio::sync::{broadcast, watch, RwLock};
use tokio::time::{MissedTickBehavior, interval};
#[derive(Clone, Serialize, Debug)]
pub struct Metrics {
    pub cpu_pct: f32,
    pub mem_used: u64,
    pub mem_total: u64,
}
fn count_cpus_from_list(s: &str) -> usize {
    // e.g. "0-3,5,7-8" => 4 + 1 + 2 = 7
    if s.trim().is_empty() { return 0; }
    s.trim().split(',')
        .map(|part| {
            let mut it = part.split('-');
            let a: usize = it.next().unwrap().parse().unwrap();
            match it.next() {
                Some(bstr) => {
                    let b: usize = bstr.parse().unwrap();
                    b - a + 1
                }
                None => 1
            }
        })
        .sum()
}

async fn read_to_string(path: &str) -> anyhow::Result<String> {
    Ok(fs::read_to_string(path).await?.trim().to_string())
}

async fn online_cpus() -> usize {
    // Prefer /sys/devices/system/cpu/online; fall back to /proc/cpuinfo
    if let Ok(s) = read_to_string("/sys/devices/system/cpu/online").await {
        let n = count_cpus_from_list(&s);
        if n > 0 { return n; }
    }
    match fs::read_to_string("/proc/cpuinfo").await {
        Ok(info) => info.lines().filter(|l| l.starts_with("processor")).count(),
        Err(_) => 1,
    }
}

fn is_cgroup_v2() -> bool {
    Path::new("/sys/fs/cgroup/cgroup.controllers").exists()
}

#[derive(Clone, Copy, Debug)]
struct CpuSample {
    _usage_secs: f64,     // cgroup cumulative CPU time in seconds
    _wall: Instant,       // sampling timestamp
    effective_cpus: f64, // quota/cpuset constrained CPUs
}

async fn read_cpu_mem_v2() -> anyhow::Result<(CpuSample, u64)> {
    // CPU usage (µs) from cpu.stat
    let cpu_stat = read_to_string("/sys/fs/cgroup/cpu.stat").await?;
    let mut usage_usec: u64 = 0;
    for line in cpu_stat.lines() {
        if let Some(v) = line.strip_prefix("usage_usec ") {
            usage_usec = v.parse().unwrap_or(0);
            break;
        }
    }

    // Effective CPUs: min(online, by_quota, by_cpuset)
    let online = online_cpus().await as f64;

    let mut cpus_by_quota = online;
    if let Ok(max) = read_to_string("/sys/fs/cgroup/cpu.max").await {
        let parts: Vec<&str> = max.split_whitespace().collect();
        if parts.len() == 2 && parts[0] != "max" {
            let quota: f64 = parts[0].parse().unwrap_or(0.0);
            let period: f64 = parts[1].parse().unwrap_or(100000.0);
            if quota > 0.0 && period > 0.0 {
                cpus_by_quota = (quota / period).max(0.01);
            }
        }
    }

    let cpuset_eff = read_to_string("/sys/fs/cgroup/cpuset.cpus.effective").await.ok()
        .map(|s| count_cpus_from_list(&s) as f64);

    let eff_cpus = match cpuset_eff {
        Some(n) if n > 0.0 => online.min(cpus_by_quota).min(n),
        _ => online.min(cpus_by_quota),
    };

    // Memory
    let mem_current: u64 = read_to_string("/sys/fs/cgroup/memory.current").await?
        .parse().unwrap_or(0);

    Ok((CpuSample {
        _usage_secs: usage_usec as f64 / 1_000_000.0,
        _wall: Instant::now(),
        effective_cpus: eff_cpus,
    }, mem_current))
}

async fn read_cpu_mem_v1() -> anyhow::Result<(CpuSample, u64)> {
    // CPU usage (ns) from cpuacct
    let usage_ns: u64 = read_to_string("/sys/fs/cgroup/cpuacct/cpuacct.usage").await?
        .parse().unwrap_or(0);

    let online = online_cpus().await as f64;

    // Quota/period
    let quota_us: i64 = read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us").await
        .unwrap_or_else(|_| "-1".into())
        .parse().unwrap_or(-1);
    let period_us: i64 = read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us").await
        .unwrap_or_else(|_| "100000".into())
        .parse().unwrap_or(100000);

    let mut cpus_by_quota = online;
    if quota_us > 0 && period_us > 0 {
        cpus_by_quota = (quota_us as f64 / period_us as f64).max(0.01);
    }

    let cpuset_eff = read_to_string("/sys/fs/cgroup/cpuset/cpuset.cpus").await.ok()
        .map(|s| count_cpus_from_list(&s) as f64);

    let eff_cpus = match cpuset_eff {
        Some(n) if n > 0.0 => online.min(cpus_by_quota).min(n),
        _ => online.min(cpus_by_quota),
    };

    // Memory
    let mem_current: u64 = read_to_string("/sys/fs/cgroup/memory/memory.usage_in_bytes").await?
        .parse().unwrap_or(0);


    Ok((CpuSample {
        _usage_secs: usage_ns as f64 / 1_000_000_000.0,
        _wall: Instant::now(),
        effective_cpus: eff_cpus,
    }, mem_current))
}

pub struct ContainerStats {
    pub cpu_percent: f64,           // over the sampling window
    pub mem_bytes: u64,
    pub mem_limit_bytes: Option<u64>,
    pub mem_percent: Option<f64>,
}

async fn sample_once() -> anyhow::Result<(CpuSample, u64)> {
    if is_cgroup_v2() { read_cpu_mem_v2().await } else { read_cpu_mem_v1().await }
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

async fn get_sample(cgroup_exists: bool, sys: &System) -> Metrics {
    if let Ok(current_pid) = get_current_pid() {
        if let Some(process) = sys.process(current_pid) {
            let mem_total = get_total_mem(cgroup_exists, &sys).await;
            let cpu_pct;
            let mem_used;
            if cgroup_exists {
                match sample_once().await {
                    Ok((cpu_sample, mem1)) => {
                        cpu_pct = cpu_sample.effective_cpus as f32;
                        mem_used = mem1;

                    },
                    Err(_) => {
                        cpu_pct = process.cpu_usage();
                        mem_used = process.memory();
                    }
                }
            } else {
                cpu_pct = process.cpu_usage();
                mem_used = process.memory();
            } 
            return Metrics {
                cpu_pct,
                mem_used,
                mem_total,
            };
        }
    }
    let cpu_pct = avg_cpu_pct(&sys);
    let mem_used = sys.used_memory();
    let mem_total = sys.total_memory();

    Metrics {
        cpu_pct,
        mem_used,
        mem_total,
    }
}

pub async fn sampler(
    latest_tx: watch::Sender<Metrics>,
    stream_tx: broadcast::Sender<Metrics>,
    history: Arc<RwLock<VecDeque<Metrics>>>,
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

        let _ = latest_tx.send(sample.clone());
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
    if n == 0 { 0.0 } else { sum / n as f32 }
}
