use crate::history::Status;
use crate::history::WORKFLOW_BASE_REDIS_KEY;
use chrono::{DateTime, Utc};
use redis::{aio::MultiplexedConnection, AsyncCommands};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "version")]
pub enum WorkflowTimelineEntryVersion {
    V1(WorkflowTimelineEntryV1),
}

impl WorkflowTimelineEntryVersion {
    pub async fn append(&self, con: &mut MultiplexedConnection) -> anyhow::Result<()> {
        let workflow_id = match self {
            WorkflowTimelineEntryVersion::V1(v1) => &v1.workflow_id,
        };
        let _: () = con
            .lpush(
                format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:timeline",),
                simd_json::to_vec(&self)?,
            )
            .await?;
        Ok(())
    }
}

/// A single ordered entry in the workflow timeline.
/// Store these append-only (e.g., RPUSH JSON into a Redis LIST).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowTimelineEntryV1 {
    /// Monotonic per-workflow sequence number (you already track this).
    // pub seq: u64,

    /// When this event was recorded (server time).
    pub ts: DateTime<Utc>,

    /// Workflow id for convenience/debugging (optional if implied by key).
    pub workflow_id: String,

    /// Epoch if you support "continue-as-new" or restarts that reset command space.
    pub epoch: u64,

    /// The actual event payload.
    pub event: WorkflowTimelineEventV1,
}

impl WorkflowTimelineEntryV1 {
    pub fn new(workflow_id: &String, epoch: u64, event: WorkflowTimelineEventV1) -> Self {
        let ts = Utc::now();
        Self {
            ts,
            workflow_id: workflow_id.clone(),
            epoch,
            event,
        }
    }
    pub async fn append(&self, con: &mut MultiplexedConnection) -> anyhow::Result<()> {
        let entry_version = WorkflowTimelineEntryVersion::V1(self.clone());
        entry_version.append(con).await
    }
}

/// Timeline event payloads.
/// Keep these small and reference larger blobs (inputs/outputs/errors) via your BlobRef system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WorkflowTimelineEventV1 {
    // -------------------------
    // Workflow lifecycle (MVP)
    // -------------------------
    WorkflowStarted {
        workflow_type: String,
        task_queue: String,
    },
    WorkflowCompleted {},
    WorkflowFailed {
        /// Keep this short; if you want full error details store a BlobRef elsewhere.
        message: String,
    },

    // -------------------------
    // Activities (MVP)
    // -------------------------
    ActivityScheduled {
        activity_id: String,
        activity_type: String,
        task_queue: Option<String>,
        /// Optional: you already track this hash, helpful for UI/debug.
        hash: Option<String>,
    },

    ActivityRunStarted {
        activity_id: String,
        run_id: String,
        /// Optional attempt number if you track retries.
        attempt: Option<u32>,
    },

    ActivityRunFinished {
        activity_id: String,
        run_id: String,
        status: Status, // typically Completed, but keeps it flexible
    },

    // ActivityRunFailed {
    //     activity_id: String,
    //     run_id: String,
    //     status: Status, // typically Failed
    //     message: String,
    // },

    // -------------------------
    // Timers / Sleep (MVP)
    // -------------------------
    TimerScheduled {
        timer_id: String,
        fire_at: DateTime<Utc>,
        /// Optional: helpful for UI (“sleep 30 days”, “backoff”, etc.)
        reason: Option<String>,
    },

    TimerFired {
        timer_id: String,
    },

    // -------------------------
    // Future-proofing (optional later)
    // -------------------------
    WorkflowCanceled {
        message: Option<String>,
    },
    WorkflowTerminated {
        message: Option<String>,
    },
    WorkflowClaimed {
        worker_id: String,
        worker_instance_id: String,
    },
    WorkflowReleased {},
    TimerCanceled {
        timer_id: String,
        message: Option<String>,
    },
    SignalReceived {
        name: String,
        /// If signals can be big, store payload as BlobRef and reference by path.
        payload_ref: Option<String>,
    },
}
