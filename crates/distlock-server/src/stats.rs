//! Process and host figures for the `GetStats` health check, all read through `sysinfo`.
use distlock_proto::v1 as pb;
use sysinfo::{MINIMUM_CPU_UPDATE_INTERVAL, Pid, ProcessRefreshKind, ProcessesToUpdate, System};

pub(crate) struct Sampler {
    /// `None` if the platform refuses to start a sampler; figures are then zero or absent.
    sys: Option<System>,
    pid: Option<Pid>,
}

impl Sampler {
    pub(crate) fn new() -> Self {
        Self {
            sys: System::new().ok(),
            pid: sysinfo::get_current_pid().ok(),
        }
    }

    /// CPU usage is a rate, so it needs two readings. This takes one, waits
    /// `MINIMUM_CPU_UPDATE_INTERVAL` (200 ms), takes another, and reports the rate between them.
    /// Run it on a connection task, never on the lock engine.
    pub(crate) async fn sample(&mut self) -> (pb::SystemStats, pb::ProcessStats) {
        let pid = self.pid;
        let kind = ProcessRefreshKind::nothing().with_memory().with_cpu();
        let refresh = |sys: &mut System| {
            sys.refresh_memory();
            sys.refresh_cpu_usage();
            if let Some(pid) = pid {
                sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, kind);
            }
        };
        if let Some(sys) = self.sys.as_mut() {
            refresh(sys);
        }
        tokio::time::sleep(MINIMUM_CPU_UPDATE_INTERVAL).await;
        if let Some(sys) = self.sys.as_mut() {
            refresh(sys);
        }

        let load = System::load_average().ok();
        // Logical cores as sysinfo counts them: the same figure its per-process percentage is
        // relative to, so dividing by it gives the average across cores.
        let cores = self.sys.as_ref().map_or(0, |s| s.cpus().len()).max(1);
        let system = pb::SystemStats {
            total_memory_bytes: self.sys.as_ref().map_or(0, System::total_memory),
            used_memory_bytes: self.sys.as_ref().map_or(0, System::used_memory),
            cpu_count: u32::try_from(cores).unwrap_or(u32::MAX),
            cpu_percent: self
                .sys
                .as_ref()
                .map_or(0.0, |s| f64::from(s.global_cpu_usage())),
            load_avg_1m: load.as_ref().map(|l| l.one),
            load_avg_5m: load.as_ref().map(|l| l.five),
            load_avg_15m: load.as_ref().map(|l| l.fifteen),
        };
        let mut process = pb::ProcessStats {
            pid: pid.map_or(0, |p| p.as_u32()),
            ..Default::default()
        };
        if let (Some(sys), Some(pid)) = (self.sys.as_ref(), pid)
            && let Some(p) = sys.process(pid)
        {
            process.cpu_percent = (f64::from(p.cpu_usage()) / cores as f64).clamp(0.0, 100.0);
            process.memory_bytes = Some(p.memory());
            process.virtual_memory_bytes = Some(p.virtual_memory());
            process.cpu_time_ms = Some(p.accumulated_cpu_time());
            process.open_files = p.open_files().map(|n| n as u64);
            process.open_files_limit = p.open_files_limit().map(|n| n as u64);
        }
        (system, process)
    }
}
