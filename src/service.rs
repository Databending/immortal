use crate::cron::CronManager;
use crate::history::{self, workflow_output_key, History};
use crate::history::{
    get_blob_raw, payload_to_blob_ref, run_output_blob_key, ActivityHistory, ActivityRun,
    Status as HistoryStatus, WorkflowHistory,
};
use crate::history_metadata::{ActivityHistoryMetadata, WorkflowHistoryMetadata};
use crate::metrics::IdentifiableMetrics;
use crate::{
    ActivityProperties, CallProperties, KillState, Notification, RegisteredWorker,
    RunningProperties,
};
use bb8_redis::{bb8, RedisConnectionManager};
use chrono::{DateTime, Duration, TimeDelta, Utc};
use immortal::immortal_worker_action_v1::Action as WorkerAction;
use immortal_lib::common::{Payload, Payloads};
use immortal_lib::failure;
use immortal_lib::immortal::RequestStartActivityOptionsV1;
use immortal_lib::immortal::{
    self, call_result_version, call_version, client_start_workflow_options_version,
    immortal_worker_action_version, ActivityResultV1, CallResultV1, CallResultVersion, CallVersion,
    ClientStartWorkflowOptionsV1, ClientStartWorkflowOptionsVersion, ImmortalWorkerActionV1,
    ImmortalWorkerActionVersion, StartWorkflowOptionsV1,
};
use immortal_lib::immortal::{workflow_result_v1, ActivityCache, WorkflowResultV1};
use rand::Rng;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot, watch, Mutex, Notify, RwLock};
use tonic::Status;
use tracing::{error, info};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CallOptions {
    pub call_type: String,
    pub input: Option<Payload>,
    pub task_queue: String,
}

#[derive(Debug, Clone)]
pub struct ImmortalService {
    pub redis_pool: bb8::Pool<RedisConnectionManager>,
    pub cron_manager: Arc<Mutex<CronManager>>,
    pub metrics_stream: broadcast::Sender<IdentifiableMetrics>,
    pub workers: Arc<RwLock<HashMap<Uuid, RegisteredWorker>>>,
    // log_streams: (
    //     broadcast::Sender<LogStreamUpdate>,
    //     Arc<Mutex<HashMap<String, LogStream>>>,
    // ),
    pub history: History,

    pub notification_tx: Arc<tokio::sync::broadcast::Sender<Notification>>,
    pub notification_rx: Arc<tokio::sync::broadcast::Receiver<Notification>>,

    pub call_notify: Arc<Notify>,
    pub running_calls: Arc<
        RwLock<
            HashMap<
                String,
                Box<(
                    Option<tokio::sync::mpsc::Sender<CallResultV1>>,
                    RunningProperties<CallProperties>,
                )>,
            >,
        >,
    >,
    // THIS SHOULD BE CHANGED. FOR NOW I AM JUST GOING TO CALL THEM ORPHANED AND ORPHANED2
    // ORPHANED2 IS PROBABLY THE TRUE DEFINITION OF AN ORPHANED WORKFLOW/ACTIVITY
    // THE ORIGINAL ORPHANED WAS JUST A WAY TO DEAL WITH WORKFLOWS AND ACTIVITIES THAT WERE STILL
    // RUNNING WHEN THE SERVER DISCONNECTED.
    // THIS WILL BE FIXED BY
    // 1) HAVING WORKERS STATE ON CONNECT WHAT WORKERS AND ACTIVITIES ARE RUNNING
    // 2) MOVING A LOT OF THIS TO REDIS SO THAT SERVERS CAN DISCONNECT WITHOUT AFFECTHING THE REST
    //    OF THE APPLICATION
    // WE STILL NEED TO FIGURE OUT WHAT TO DO WHEN A SERVER DISCONNECTS FOR LONG ENOUGH AND THE
    // WORKER BUILDS UP A QUEUE OF COMPLETED WORKFLOWS AND ACTIVITIES
    pub orphaned_activities: Arc<RwLock<HashMap<String, tokio::sync::oneshot::Sender<bool>>>>,
    pub orphaned_workflows: Arc<RwLock<HashMap<String, tokio::sync::oneshot::Sender<bool>>>>,
    pub activity_notify: Arc<Notify>,
    pub running_activities: Arc<
        RwLock<
            HashMap<
                String,
                Box<(
                    // worker instance id
                    Uuid,
                    tokio::sync::oneshot::Sender<ActivityResultV1>,
                    RunningProperties<ActivityProperties>,
                )>,
            >,
        >,
    >,
    pub running_workflows: Arc<
        RwLock<
            HashMap<
                String,
                Box<(
                    // worker instance id
                    Uuid,
                    RunningProperties<ClientStartWorkflowOptionsV1>,
                )>,
            >,
        >,
    >,
    pub call_queue: Arc<
        Mutex<
            HashMap<
                String,
                VecDeque<
                    Box<(
                        String,
                        CallOptions,
                        Option<tokio::sync::mpsc::Sender<CallResultV1>>,
                    )>,
                >,
            >,
        >,
    >,
    pub workflow_notify: Arc<Notify>,
    pub workflow_queue: Arc<
        Mutex<
            HashMap<
                String,
                VecDeque<
                    Box<(
                        String,
                        ClientStartWorkflowOptionsV1,
                        Option<watch::Sender<i32>>,
                        Option<Vec<ActivityCache>>,
                    )>,
                >,
            >,
        >,
    >,
    pub activity_queue: Arc<
        Mutex<
            HashMap<
                String,
                VecDeque<
                    Box<(
                        String,
                        RequestStartActivityOptionsV1,
                        tokio::sync::oneshot::Sender<ActivityResultV1>,
                        DateTime<Utc>,
                        // activity position index within workflow
                        usize,
                    )>,
                >,
            >,
        >,
    >,
}

enum AdjustCapacity {
    Workflow,
    Activity,
}

async fn fail_entire_workflow(
    history: &History,
    wf: &mut WorkflowHistoryMetadata,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let mut con = history.get_con().await?;
    for (_i, activity) in wf.activities.iter_mut().enumerate() {
        for run in &mut activity.runs {
            if matches!(run.status, HistoryStatus::Running) {
                run.status = HistoryStatus::Failed;
                history
                    .store_activity_run_output(
                        &wf.workflow_id,
                        &activity.activity_id,
                        &run.run_id,
                        Payload::new(&simd_json::json!({
                            "message": "Orphaned"
                        })),
                    )
                    .await?;
                // run.status = HistoryStatus::Failed("Orphaned".to_string());

                run.end_time = Some(now);
                run.store_run(&mut con, &wf.workflow_id, &activity.activity_id)
                    .await?;
            }
        }

        // if changed {
        //     let activity_clone = activity.clone();
        //     history
        //         .update_activity(&v1.workflow_id, activity_clone)
        //         .await?;
        // }
    }
    wf.status = HistoryStatus::Failed;
    history
        .store_workflow_output(
            &wf.workflow_id,
            Payload::new(&simd_json::json!({
                "message": "Orphaned"
            })),
        )
        .await?;
    wf.end_time = Some(now);
    wf.store(&mut con, false).await?;
    // history.update_workflow(&v1.workflow_id, v1.clone()).await?;
    Ok(())
}

async fn rehydrate_activity_if_absent(
    running_activities: &Arc<
        RwLock<
            HashMap<
                String,
                Box<(
                    Uuid,
                    tokio::sync::oneshot::Sender<ActivityResultV1>,
                    RunningProperties<ActivityProperties>,
                )>,
            >,
        >,
    >,
    workers: &Arc<RwLock<HashMap<Uuid, RegisteredWorker>>>,
    worker_id: &str,
    worker_instance_id: &Uuid,
    workflow_id: &str,
    activity: &ActivityHistoryMetadata,
    last_run_start: Option<DateTime<Utc>>,
) {
    let mut running = running_activities.write().await;
    if running.contains_key(&activity.activity_id) {
        return;
    }
    let (tx, _rx) = tokio::sync::oneshot::channel::<ActivityResultV1>();
    let now = Utc::now();
    running.insert(
        activity.activity_id.clone(),
        Box::new((
            worker_instance_id.clone(),
            tx,
            RunningProperties {
                kill_state: KillState::Healthy,
                worker_instance_id: worker_instance_id.clone(),
                start: last_run_start.unwrap_or(now),
                timeout: now + Duration::seconds(30),
                max_duration: Duration::seconds(30),
                worker_id: worker_id.to_string(),
                heartbeat_timeout: Duration::seconds(30),
                additional_properties: ActivityProperties {
                    // not sure if this is correct
                    latest_run_id: activity
                        .runs
                        .get(activity.runs.len() - 1)
                        .unwrap()
                        .run_id
                        .to_string(),
                    index: activity.index,
                    workflow_id: workflow_id.to_string(),
                    last_heartbeat: now,
                    scheduled: last_run_start.unwrap_or(now),
                    latest_run_start: last_run_start,
                },
            },
        )),
    );

    // decrement activity capacity (best-effort)
    ImmortalService::adjust_capacity(
        Arc::clone(workers),
        worker_instance_id,
        -1,
        AdjustCapacity::Activity,
    )
    .await;
}

// --- Activity retry policy (tweak as needed) ---
const ACTIVITY_MAX_ATTEMPTS: usize = 3;
const ACTIVITY_BACKOFF_BASE_MS: u64 = 1_000; // 1s
const ACTIVITY_BACKOFF_FACTOR: u64 = 2;
const ACTIVITY_BACKOFF_JITTER_MS: u64 = 250;

// Build RequestStartActivityOptionsV1 from history for a retry.
// We infer task_queue from the workflow (common case). If your activities
// use a different queue, swap this to your preferred source.
async fn build_retry_activity_options(
    history: &History,
    workflow_id: &str,
    activity: &ActivityHistoryMetadata,
) -> anyhow::Result<immortal::RequestStartActivityOptionsV1> {
    let mut con = history.get_con().await?;
    // use immortal_lib::common::Payload;
    if let Some(workflow_metadata) = WorkflowHistoryMetadata::get_opt(&mut con, &workflow_id, true)
        .await
        .map_err(|e| {
            println!("Failed to get workflow history: {:?}", e);
            Status::internal("Failed to get workflow history")
        })?
    {
        let mut activity_input = None;
        if let Some(blob_ref) = &activity.input {
            activity_input = Some(Payload {
                data: get_blob_raw(&mut con, &blob_ref.path).await?.unwrap(),
                metadata: blob_ref.metadata.clone().unwrap_or_default(),
            });
        }
        Ok(RequestStartActivityOptionsV1 {
            workflow_id: workflow_id.to_string(),
            activity_id: activity.activity_id.clone(),
            activity_type: activity.activity_type.clone(),
            activity_input,
            // Use the same queue as the workflow by default
            task_queue: workflow_metadata.task_queue,
            // Reasonable defaults; override if you persist these in history
            heartbeat_timeout: None,
            schedule_to_start_timeout: None,
            ..Default::default()
        })
    } else {
        Err(anyhow::anyhow!("Workflow not found"))
    }
    // Grab the workflow to infer its task_queue

    // let wf_metadata = history.get

    // WHY THE FUCK WAS I DOING THIS???????
    // Serialize the activity input (it’s stored as OwnedValue in history)
    // let activity_input = if let Some(v) = &activity.input {
    //     let mut tmp = simd_json::to_vec(v)?;
    //     Some(Payload {
    //         data: std::mem::take(&mut tmp),
    //         ..Default::default()
    //     })
    // } else {
    //     None
    // };
}

// Schedule a retry with exponential backoff (+ jitter)
impl ImmortalService {
    pub async fn completed_workflow_inner(
        &self,
        workflow_result: WorkflowResultV1,
    ) -> Result<(), Status> {
        let mut con = self.history.get_con().await.unwrap();

        let workflow_metadata =
            WorkflowHistoryMetadata::get_opt(&mut con, &workflow_result.workflow_id, true)
                .await
                .map_err(|e| {
                    println!("Failed to get workflow history: {:?}", e);
                    Status::internal("Failed to get workflow history")
                })?;

        // Fetch workflow history
        // let workflow_opt = self
        //     .history
        //     .get_workflow(&workflow_result.workflow_id)
        //     .await
        //     .map_err(|e| {
        //         println!("Failed to get workflow history: {:?}", e);
        //         Status::internal("Failed to get workflow history")
        //     })?;

        let mut workflow = match workflow_metadata {
            Some(wf) => wf,
            None => {
                error!(
                    "Workflow history not found: {}",
                    workflow_result.workflow_id
                );
                return Err(Status::not_found("Workflow history not found"));
            }
        };

        // Update end time
        workflow.end_time = Some(chrono::Utc::now());

        let _worker_id = workflow_result.worker_id.clone();
        let worker_instance_id = Uuid::parse_str(&workflow_result.worker_instance_id)
            .map_err(|_e| Status::invalid_argument("Worker Instance ID should be UUID v4."))?;
        let workflow_id = workflow_result.workflow_id.clone();

        let workflow_output;

        if workflow_result.epoch != workflow.epoch {
            // THIS IS NO LONGER THE LATEST WF RUN. IGNORE IT
            return Ok(());
        }
        if let Some(owner) = &workflow.owner {
            if owner.instance_id.to_string() != workflow_result.worker_instance_id {
                return Ok(());
            }
        }

        self.running_workflows
            .write()
            .await
            .remove(&workflow_result.workflow_id);

        // Set status and send specific notification
        match workflow_result.status {
            Some(workflow_result_v1::Status::Completed(x)) => {
                match x.result {
                    Some(result_data) => {
                        // let mut x = result_data.data.clone();
                        workflow.status = HistoryStatus::Completed;
                        workflow.output = Some(history::BlobRef {
                            path: workflow_output_key(&workflow_id),
                            size: result_data.data.len(),
                            present: true,
                            loaded: false,
                            data: None,
                            metadata: Some(result_data.metadata.clone()),
                        });
                        // workflow.output_metadata = Some(result_data.metadata.clone());
                        workflow_output = Some(result_data);
                        // match serde_json::from_slice(&result_data.data) {
                        //     Ok(deserialized) => {
                        //         workflow.status = HistoryStatus::Completed(deserialized);
                        //     }
                        //     Err(e) => {
                        //         println!("Failed to parse result: {:?}", e);
                        //         error!("Failed to deserialize workflow result: {:?}", e);
                        //         workflow.status =
                        //             HistoryStatus::Failed("Invalid result format".into());
                        //     }
                        // }
                    }
                    None => {
                        let error_message_json = simd_json::json!({
                            "message": "Missing result payload"
                        });
                        let data = simd_json::to_vec(&error_message_json).unwrap();
                        workflow.status = HistoryStatus::Failed;
                        workflow.output = Some(history::BlobRef {
                            path: workflow_output_key(&workflow_id),
                            size: data.len(),
                            present: true,
                            loaded: false,
                            data: None,
                            metadata: Some(HashMap::new()),
                        });
                        // workflow.output_metadata = Some(HashMap::new());
                        workflow_output = Some(Payload {
                            data,
                            ..Default::default()
                        });
                    }
                }

                if let Ok(uuid) = Uuid::parse_str(&workflow_result.workflow_id) {
                    if let Err(e) = self
                        .notification_tx
                        .send(Notification::WorkflowCompleted(uuid, workflow.clone()))
                    {
                        error!("Error sending WorkflowCompleted notification: {:?}", e);
                    }
                }
            }

            Some(workflow_result_v1::Status::Failed(x)) => {
                let data = simd_json::to_vec(&x).unwrap();
                workflow.status = HistoryStatus::Failed;
                workflow.output = Some(history::BlobRef {
                    path: workflow_output_key(&workflow_id),
                    size: data.len(),
                    present: true,
                    loaded: false,
                    data: None,
                    metadata: Some(HashMap::new()),
                });
                workflow_output = Some(Payload {
                    data: simd_json::to_vec(&x).unwrap(),
                    ..Default::default()
                });
                if let Ok(uuid) = Uuid::parse_str(&workflow_result.workflow_id) {
                    if let Err(e) = self
                        .notification_tx
                        .send(Notification::WorkflowFailed(uuid, workflow.clone()))
                    {
                        error!("Error sending WorkflowFailed notification: {:?}", e);
                    }
                }
            }

            Some(workflow_result_v1::Status::Cancelled(x)) => {
                let data = simd_json::to_vec(&x).unwrap();
                workflow.status = HistoryStatus::Failed;
                workflow.output = Some(history::BlobRef {
                    path: workflow_output_key(&workflow_id),
                    size: data.len(),
                    present: true,
                    loaded: false,
                    data: None,
                    metadata: Some(HashMap::new()),
                });
                workflow_output = Some(Payload {
                    data,
                    ..Default::default()
                });
            }

            None => {
                let error_message_json = simd_json::json!({
                    "message": "Missing status field"
                });
                let data = simd_json::to_vec(&error_message_json).unwrap();
                workflow.status = HistoryStatus::Failed;
                // workflow.output_metadata = Some(HashMap::new());
                workflow.output = Some(history::BlobRef {
                    path: workflow_output_key(&workflow_id),
                    size: data.len(),
                    present: true,
                    loaded: false,
                    data: None,
                    metadata: Some(HashMap::new()),
                });
                workflow_output = Some(Payload {
                    data,
                    ..Default::default()
                });
            }
        }

        // Increase worker capacity if found
        {
            let mut workers = self.workers.write().await;
            if let Some(worker) = workers.get_mut(&worker_instance_id) {
                if worker.workflow_capacity < worker.max_workflow_capacity {
                    worker.workflow_capacity += 1;
                }
            } else {
                error!(
                    "Worker {} not found when marking workflow complete",
                    workflow_result.worker_id
                );
            }
        }

        let _ = self.notification_tx.send(Notification::WorkflowCompleted(
            Uuid::parse_str(&workflow_id).unwrap(),
            workflow.clone(),
        ));
        workflow.store(&mut con, false).await.map_err(|e| {
            println!("Failed to store workflow metadata history: {:?}", e);
            Status::internal("Failed to get workflow history")
        })?;
        if let Some(output) = workflow_output {
            self.history
                .store_workflow_output(&workflow_id, output)
                .await
                .map_err(|e| {
                    println!("Failed to get workflow history: {:?}", e);
                    Status::internal("Failed to get workflow history")
                })?;
        }
        // Save updated workflow
        // self.history
        //     .update_workflow(&workflow_id, workflow)
        //     .await
        //     .map_err(|e| {
        //         error!("Failed to update workflow history: {:?}", e);
        //         Status::internal("Failed to update workflow history")
        //     })?;

        // Notify workflow result
        match Uuid::parse_str(&workflow_result.workflow_id) {
            Ok(uuid) => {
                if let Err(e) = self
                    .notification_tx
                    .send(Notification::WorkflowResult(uuid, workflow))
                {
                    error!("Error sending WorkflowResult notification: {:?}", e);
                }
            }
            Err(e) => {
                error!(
                    "Invalid UUID in workflow_id (result notification): {} ({:?})",
                    workflow_result.workflow_id, e
                );
            }
        }

        Ok(())
    }
    pub async fn completed_activity_inner(
        &self,
        activity_result: ActivityResultV1,
    ) -> Result<(), Status> {
        // 1) Make completion idempotent (required)
        //
        // Right now, watchdog can finalize, then later the worker reports completion and you return NotFound.
        //
        // For killing/timeout, you must treat “already gone” as success.
        // let (worker_id, tx, _) = match self
        //     .running_activities
        //     .write()
        //     .await
        //     .remove(&activity_result.activity_id)
        // {
        //     Some(entry) => *entry,
        //     None => return Err(Status::not_found("Activity not found")),
        // };
        let Some(entry) = self
            .running_activities
            .write()
            .await
            .remove(&activity_result.activity_id)
        else {
            // already finalized by watchdog / duplicate / late worker report
            return Ok(());
        };
        let (worker_id, tx, _props) = *entry;

        {
            let mut workers = self.workers.write().await;
            if let Some(worker) = workers.get_mut(&worker_id) {
                if worker.activity_capacity < worker.max_activity_capacity {
                    worker.activity_capacity += 1;
                }
            }
        }
        // println!("workflow_id {:?}", activity_result.workflow_id);
        // println!("activity_id {:?}", activity_result.activity_id);

        let mut con = self.history.get_con().await.map_err(|e| {
            error!("Error fetching redis connection: {:?}", e);
            Status::internal("Failed to fetch redis connection")
        })?;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // Fetch and update activity history
        // let activity_opt = self
        //     .history
        //     .get_activity(&activity_result.workflow_id, &activity_result.activity_id)
        //     .await
        //     .map_err(|e| {
        //         eprintln!("Error fetching activity: {:?}", e);
        //         Status::internal("Failed to fetch activity history")
        //     })?;

        let activity_opt = ActivityHistoryMetadata::get_opt(
            &mut con,
            &activity_result.workflow_id,
            &activity_result.activity_id,
        )
        .await
        .map_err(|e| {
            error!("Error fetching activity: {:?}", e);
            Status::internal("Failed to fetch activity history")
        })?;

        let mut activity = match activity_opt {
            Some(a) => a,
            None => {
                error!("Activity history not found for completed activity");
                return Err(Status::not_found("Activity history not found"));
            }
        };

        // {
        let run = match activity
            .runs
            .iter_mut()
            .find(|f| f.run_id == activity_result.activity_run_id)
        {
            Some(r) => r,
            None => {
                error!(
                    "Run ID {} not found in activity history",
                    activity_result.activity_run_id
                );
                return Err(Status::not_found(format!(
                    "Run ID {} not found in activity history",
                    activity_result.activity_run_id
                )));
            }
        };

        run.end_time = Some(chrono::Utc::now());

        // println!("{:?}", activity_result.status);

        let mut failed = false;
        let run_path = run_output_blob_key(
            &activity_result.workflow_id,
            &activity_result.activity_id,
            &run.run_id,
        );
        match &activity_result.status {
            Some(immortal::activity_result_v1::Status::Completed(x)) => {
                match &x.result {
                    Some(result_data) => {
                        run.status = HistoryStatus::Completed;
                        self.history
                            .store_activity_run_output(
                                &activity_result.workflow_id,
                                &activity_result.activity_id,
                                &run.run_id,
                                result_data.clone(),
                            )
                            .await
                            .map_err(|e| {
                                error!("Error storing activity run output: {:?}", e);
                                Status::internal("Error storing activity run output")
                            })?;
                        run.output = Some(payload_to_blob_ref(run_path, &result_data));
                    }
                    None => {
                        let error_message_json = simd_json::json!({
                            "message": "Missing result payload"
                        });
                        let data = simd_json::to_vec(&error_message_json).unwrap();
                        run.status = HistoryStatus::Failed;
                        let payload = Payload {
                            data,
                            ..Default::default()
                        };
                        run.output = Some(payload_to_blob_ref(run_path, &payload));
                        self.history
                            .store_activity_run_output(
                                &activity_result.workflow_id,
                                &activity_result.activity_id,
                                &run.run_id,
                                payload,
                            )
                            .await
                            .map_err(|e| {
                                error!("Error storing activity run output: {:?}", e);
                                Status::internal("Error storing activity run output")
                            })?;
                    }
                }
                // if let Ok(id) = Uuid::parse_str(&activity_result.workflow_id) {
                //     if let Err(e) = self
                //         .notification_tx
                //         .send(Notification::ActivityRunCompleted(id, activity.clone()))
                //     {
                //         error!("Error sending ActivityRunCompleted notification: {:?}", e);
                //     }
                // }
            }

            Some(immortal::activity_result_v1::Status::Failed(x))
            | Some(immortal::activity_result_v1::Status::Timeout(x)) => {
                failed = true;
                // Mark this run failed in history
                run.status = HistoryStatus::Failed;
                let payload = Payload::new(&x);
                run.output = Some(payload_to_blob_ref(run_path, &payload));
                self.history
                    .store_activity_run_output(
                        &activity_result.workflow_id,
                        &activity_result.activity_id,
                        &run.run_id,
                        payload,
                    )
                    .await
                    .map_err(|e| {
                        error!("Error storing activity run output: {:?}", e);
                        Status::internal("Error storing activity run output")
                    })?;
                // Count attempts so far (all runs for this activity)
            }

            Some(immortal::activity_result_v1::Status::Cancelled(x)) => {
                run.status = HistoryStatus::Failed;
                let payload = Payload::new(&x);
                run.output = Some(payload_to_blob_ref(run_path, &payload));
                self.history
                    .store_activity_run_output(
                        &activity_result.workflow_id,
                        &activity_result.activity_id,
                        &run.run_id,
                        payload,
                    )
                    .await
                    .map_err(|e| {
                        error!("Error storing activity run output: {:?}", e);
                        Status::internal("Error storing activity run output")
                    })?;
                // (No retry on cancelled by default; tweak if you want)
            }

            None => {
                let error_message_json = simd_json::json!({
                    "message": "Missing status field"
                });
                let data = simd_json::to_vec(&error_message_json).unwrap();
                run.status = HistoryStatus::Failed;
                let payload = Payload {
                    data,
                    ..Default::default()
                };
                run.output = Some(payload_to_blob_ref(run_path, &payload));
                self.history
                    .store_activity_run_output(
                        &activity_result.workflow_id,
                        &activity_result.activity_id,
                        &run.run_id,
                        payload,
                    )
                    .await
                    .map_err(|e| {
                        error!("Error storing activity run output: {:?}", e);
                        Status::internal("Error storing activity run output")
                    })?;
            }
        }
        // let _ = self.notification_tx.send(Notification::ActivityRunCompleted(
        //     Uuid::parse_str(&workflow_id).unwrap(),
        //     activity.clone(),
        // ));
        // I THINK IT'S THIS MOTHERFUCKER RIGHT HERE OVERWRITING MY ACTIVITY DATA
        run.store_run(
            &mut con,
            &activity_result.workflow_id,
            &activity_result.activity_id,
        )
        .await
        .map_err(|e| {
            error!("Error storing activity run metadata: {:?}", e);
            Status::internal("Error storing activity run metadata")
        })?;
        let attempts = activity.runs.len(); // includes this failed run

        // Send a completion/failed notification for this run (kept as-is)
        if let Ok(id) = Uuid::parse_str(&activity_result.workflow_id) {
            if let Err(e) = self
                .notification_tx
                .send(Notification::ActivityRunCompleted(id, activity.clone()))
            {
                error!("Error sending ActivityRunCompleted notification: {:?}", e);
            }
        }
        // if let Err(e) = self
        //     .history
        //     .update_activity(
        //         &activity_result.workflow_id,
        //         activity.clone(),
        //         // activity.index,
        //     )
        //     .await
        // {
        //     println!("error");
        //     eprintln!("Failed to update activity history: {:?}", e);
        //     return Err(Status::internal("Failed to update activity history"));
        // }

        // I TRIED MOVING THIS HERE SO THAT WE FIRST UPDATE THE RUN WITH A FAILED
        // STATUS AND THEN. IF WE CAN RUN AGAIN. WE WRITE A NEW ATTEMPT
        // I MIGHT HAVE TO CREATE A SYSTEM WHERE WE ALWAYS FETCH THE LATEST REDIS DATA
        // Decide retry
        if failed && attempts < ACTIVITY_MAX_ATTEMPTS {
            // inform_worker = false;
            // Persist the failed run before we schedule the retry

            // Build next options from history
            match build_retry_activity_options(
                &self.history,
                &activity_result.workflow_id,
                &activity,
            )
            .await
            {
                Ok(next_opts) => {
                    // Next run_id is attempts as string (previous runs are 0..attempts-1)
                    let next_run_id = attempts.to_string();

                    // Schedule retry with exponential backoff
                    self.schedule_activity_retry(
                        next_opts,
                        next_run_id,
                        attempts - 1, // attempt_index: 0-based for first retry
                        tx,
                        activity.index,
                    );

                    // (Optional) You can emit a small notification here if you want:
                    // let _ = self.notification_tx.send(Notification::ActivityRunStarted(...));
                }
                Err(err) => {
                    error!("Could not build retry options: {:?}", err);
                    let _ = tx.send(activity_result);
                    // fall through: no retry ⇒ final failure (we already marked run as failed)
                }
            }
        } else {
            // }

            // if inform_worker {
            println!("INFORMING WORKER");
            // Update the worker's activity capacity

            // THIS IS WHERE WE TELL THE WORKER THE RESULT OF THE ACTIVITY
            // IN THIS CASE. WHEN WE RETRY THE ACTIVITY. WE DON'T WANT TO INFORM THE WORKER
            // JUST YET. I WILL ADD A BOOL CALLED INFORM_WORKER
            if let Err(e) = tx.send(activity_result) {
                // IF THIS FAILS IT'S BECAUSE THE RECEIVER DROPPED
                error!("Failed to send activity result: {:?}", e);
            }
        }

        // println!("finished");

        // (Move essentially everything inside completed_activity's V1 branch here)
        // Replace `activity_result` references accordingly and return `Ok(())` at end.
        // IMPORTANT: if you currently depend on `Request<ActivityResultVersion>`,
        // you don’t need it here—just the decoded ActivityResultV1.
        //
        // The key win: watchdog can synthesize an ActivityResultV1 (Timeout/Failed/etc)
        // and still go through the same “store runs / store output / retries / capacity” path.
        Ok(())
    }

    fn schedule_activity_retry(
        &self,
        options: immortal::RequestStartActivityOptionsV1,
        next_run_id: String,
        attempt_index: usize, // 0-based (0 => first retry)
        tx: tokio::sync::oneshot::Sender<ActivityResultV1>,
        activity_index: usize,
    ) {
        let backoff_ms = ACTIVITY_BACKOFF_BASE_MS
            .saturating_mul(ACTIVITY_BACKOFF_FACTOR.saturating_pow(attempt_index as u32));
        let jitter = rand::rng().random_range(0..=ACTIVITY_BACKOFF_JITTER_MS);
        let delay = std::time::Duration::from_millis(backoff_ms + jitter);

        let activity_queue = Arc::clone(&self.activity_queue);
        let activity_notify = Arc::clone(&self.activity_notify);

        tokio::spawn(async move {
            tokio::time::sleep(delay).await;

            // Enqueue back onto the activity queue
            let now = chrono::Utc::now();
            {
                let mut queues = activity_queue.lock().await;
                match queues.get_mut(&options.task_queue) {
                    Some(q) => q.push_back(Box::new((
                        next_run_id.clone(),
                        options.clone(),
                        tx,
                        now,
                        activity_index,
                    ))),
                    None => {
                        let mut q = std::collections::VecDeque::new();
                        q.push_back(Box::new((
                            next_run_id.clone(),
                            options.clone(),
                            tx,
                            now,
                            activity_index,
                        )));
                        queues.insert(options.task_queue.clone(), q);
                    }
                }
            }
            activity_notify.notify_one();
        });
    }

    async fn adjust_capacity(
        workers: Arc<RwLock<HashMap<Uuid, RegisteredWorker>>>,
        worker_instance_id: &Uuid,
        amount: i32,
        capacity: AdjustCapacity,
    ) {
        let mut workers = workers.write().await;
        if let Some(worker) = workers
            .iter_mut()
            .find(|f| f.1.instance_id == *worker_instance_id)
        {
            match capacity {
                AdjustCapacity::Workflow => worker.1.workflow_capacity += amount,
                AdjustCapacity::Activity => worker.1.activity_capacity += amount,
            }
        }
    }

    // We need to first check
    // 1) Is the workflow still running on the workers
    // 1.1) If it is, we simply add it back to the running queue
    // 1.2) If it is not, we fail the entire workflow and all activities
    // 2) Once we have determined that the workflow is still running. We need to now check with
    //    activity it is stuck on. We can do an activity check as well.
    // 2.1) If the activity is running, we add it back to the queue
    // 2.2) If the activity is no longer running, depending on the retry policy, we either rerun it
    //   or fail everything

    async fn _continue_workflow(&self, workflow_id: &str) -> anyhow::Result<()> {
        if let Some(workflow) = self.history.get_workflow(workflow_id).await? {
            let activities_cache: Vec<_> = workflow
                .activities
                .iter()
                .map(|f| ActivityCache {
                    input: f.input.clone(),
                    // output: f.output.as_ref().map(|x| Payload::new(x)),
                    output: None,
                    activity_type: f.activity_type.clone(),
                    task_queue: f.task_queue.clone(),
                })
                .collect();
            self.start_workflow_internal(
                ClientStartWorkflowOptionsVersion {
                    version: Some(client_start_workflow_options_version::Version::V1(
                        ClientStartWorkflowOptionsV1 {
                            workflow_type: workflow.workflow_type.clone(),
                            workflow_version: "V1".to_string(),
                            input: Some(Payloads::new(workflow.args.iter().map(|f| f).collect())),
                            task_queue: workflow.task_queue.clone(),
                            workflow_id: Some(workflow_id.to_string()),
                        },
                    )),
                },
                None,
                Some(activities_cache),
            )
            .await?;
        }

        Ok(())
    }


    // this should be deleted soon
    pub async fn orphaned_workflows(&self) -> anyhow::Result<()> {
        use tokio::time::{timeout, Duration as TokioDuration};

        let running_activities = Arc::clone(&self.running_activities);
        let orphaned_activities = Arc::clone(&self.orphaned_activities);
        let orphaned_workflows = Arc::clone(&self.orphaned_workflows);
        let workers = Arc::clone(&self.workers);
        let history = self.history.clone();

        let mut con = history.get_con().await?;
        // scan recent set; tune page as desired

        let mut workflows = WorkflowHistoryMetadata::get_all(
            &mut con,
            Some(500),
            Some(0),
            None,
            None,
            None,
            None,
            true,
        )
        .await?;

        for wf in &mut workflows {
            if !matches!(wf.status, HistoryStatus::Running) {
                continue;
            }

            let now = Utc::now();
            let Some(worker_id) = wf.worker_id.clone() else {
                // nothing owns it: fail everything
                fail_entire_workflow(&history, wf, now).await?;
                continue;
            };
            let Some(owner) = wf.owner.clone() else {
                // nothing owns it: fail everything
                fail_entire_workflow(&history, wf, now).await?;
                continue;
            };

            // ensure worker exists & matches queue + can run the workflow type
            let (worker_tx, can_run_workflow) = {
                let guard = workers.read().await;
                if let Some(w) = guard.get(&owner.instance_id) {
                    let queue_ok = wf.task_queue == w.task_queue;
                    let wf_ok = w.registered_workflows.contains_key(&wf.workflow_type);
                    (Some(w.tx.clone()), queue_ok && wf_ok)
                } else {
                    (None, false)
                }
            };

            if worker_tx.is_none() || !can_run_workflow {
                fail_entire_workflow(&history, wf, now).await?;
                continue;
            }

            // Ask the worker if this workflow is actually still running.
            let (tx_wf, rx_wf) = oneshot::channel::<bool>();
            {
                let mut map = orphaned_workflows.write().await;
                map.insert(wf.workflow_id.clone(), tx_wf);
            }

            // fire-and-wait (bounded)
            if let Err(_e) = worker_tx
                .as_ref()
                .unwrap()
                .send(Ok(ImmortalWorkerActionVersion {
                    version: Some(immortal_worker_action_version::Version::V1(
                        ImmortalWorkerActionV1 {
                            action: Some(WorkerAction::CheckWorkflow(wf.workflow_id.clone())),
                        },
                    )),
                }))
                .await
            {
                // can’t ask => consider orphaned
                fail_entire_workflow(&history, wf, now).await?;
                continue;
            }

            let running = timeout(TokioDuration::from_secs(2), rx_wf)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(false);

            if !running {
                // worker said "no" (or timed out) => fail everything
                fail_entire_workflow(&history, wf, now).await?;
                continue;
            }

            // Workflow is running: check each Running activity run.
            for activity in &mut wf.activities {
                // Only check the *latest* running run (if any)
                let maybe_latest_running_idx = activity
                    .runs
                    .iter()
                    .rposition(|r| matches!(r.status, HistoryStatus::Running));

                let Some(idx) = maybe_latest_running_idx else {
                    continue;
                };
                // let run_id = activity.runs[idx].run_id.clone();

                // Ensure worker can run this activity on this queue
                let can_resume_activity = {
                    let guard = workers.read().await;
                    if let Some(w) = guard.get(&owner.instance_id) {
                        wf.task_queue == w.task_queue
                            && w.registered_activities
                                .contains_key(&activity.activity_type)
                    } else {
                        false
                    }
                };

                if !can_resume_activity {
                    // Mark that run failed
                    activity.runs[idx].status = HistoryStatus::Failed;
                    activity.runs[idx].end_time = Some(now);
                    activity.store(&mut con, &wf.workflow_id, false).await?;
                    // let cloned = activity.clone();

                    // history.update_activity(&wf.workflow_id, cloned).await?;
                    continue;
                }

                // Ask the worker about the activity
                let (tx_act, rx_act) = oneshot::channel::<bool>();
                {
                    let mut map = orphaned_activities.write().await;
                    map.insert(activity.activity_id.clone(), tx_act);
                }

                if let Err(_e) = worker_tx
                    .as_ref()
                    .unwrap()
                    .send(Ok(ImmortalWorkerActionVersion {
                        version: Some(immortal_worker_action_version::Version::V1(
                            ImmortalWorkerActionV1 {
                                action: Some(WorkerAction::CheckActivity(
                                    activity.activity_id.clone(),
                                )),
                            },
                        )),
                    }))
                    .await
                {
                    // treat as not running
                    activity.runs[idx].status = HistoryStatus::Failed;

                    activity.runs[idx].end_time = Some(now);
                    let run_id = activity.runs[idx].run_id.clone();
                    // let cloned = activity.clone();
                    history
                        .store_activity_run_output(
                            &wf.workflow_id,
                            &activity.activity_id,
                            &run_id,
                            Payload::new(&simd_json::json!({
                                "message": "Orphaned"
                            })),
                        )
                        .await?;
                    activity.runs[idx]
                        .store_run(&mut con, &wf.workflow_id, &activity.activity_id)
                        .await?;
                    // history.update_activity(&v1.workflow_id, cloned).await?;
                    continue;
                }

                let act_running = timeout(TokioDuration::from_secs(2), rx_act)
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or(false);

                if act_running {
                    // rehydrate into memory so watchdog & logs update heartbeats again
                    let last_run_start = activity.runs[idx].start_time;
                    rehydrate_activity_if_absent(
                        &running_activities,
                        &workers,
                        &worker_id,
                        &owner.instance_id,
                        &wf.workflow_id,
                        activity,
                        Some(last_run_start),
                    )
                    .await;
                } else {
                    // mark failed
                    activity.runs[idx].status = HistoryStatus::Failed;
                    activity.runs[idx].end_time = Some(now);

                    let run_id = activity.runs[idx].run_id.clone();
                    history
                        .store_activity_run_output(
                            &wf.workflow_id,
                            &activity.activity_id,
                            &run_id,
                            Payload::new(&simd_json::json!({
                                "message": "Orphaned"
                            })),
                        )
                        .await?;
                    activity.runs[idx]
                        .store_run(&mut con, &wf.workflow_id, &activity.activity_id)
                        .await?;
                }
            }

            // If *all* activities ended up not running anymore, it’s safer to fail the workflow too.
            let any_running_left = wf.activities.iter().any(|a| {
                a.runs
                    .iter()
                    .any(|r| matches!(r.status, HistoryStatus::Running))
            });

            if !any_running_left {
                wf.status = HistoryStatus::Failed;
                history
                    .store_workflow_output(
                        &wf.workflow_id,
                        Payload::new(&simd_json::json!({
                            "message": "Orphaned"
                        })),
                    )
                    .await?;
                wf.end_time = Some(now);
                wf.store(&mut con, false).await?;
                // history.update_workflow(&wf.workflow_id, v1.clone()).await?;
            }
        }

        Ok(())
    }
    pub async fn kill_workflow(&self, workflow_id: &str) -> anyhow::Result<()> {
        let mut con = self.history.get_con().await?;
        if let Some(workflow) =
            WorkflowHistoryMetadata::get_opt(&mut con, workflow_id, true).await?
        {
            if let Some(owner) = workflow.owner {
                {
                    let running_activities_stripped: Vec<_>;
                    {
                        let running_activities = self.running_activities.read().await;

                        running_activities_stripped = running_activities
                            .iter()
                            .map(|f| {
                                (
                                    f.0.clone(),
                                    f.1 .2.additional_properties.workflow_id.clone(),
                                )
                            })
                            .collect();
                    }
                    for (activity_id, w_id) in running_activities_stripped {
                        if w_id == workflow_id {
                            println!("killing activity");
                            self.kill_activity(&activity_id).await?;
                        }
                    }
                }
                {
                    let workers = self.workers.read().await;
                    if let Some(worker) = workers.get(&owner.instance_id) {
                        println!("killing workflow");
                        worker
                            .tx
                            .send(Ok(ImmortalWorkerActionVersion {
                                version: Some(immortal_worker_action_version::Version::V1(
                                    ImmortalWorkerActionV1 {
                                        action: Some(WorkerAction::KillWorkflow(
                                            workflow_id.to_string(),
                                        )),
                                    },
                                )),
                            }))
                            .await?;
                    }
                }
            }
            // let running_workflows = self.execute_workflow
        }
        Ok(())
    }
    async fn kill_activity(&self, activity_id: &str) -> anyhow::Result<()> {
        {
            let running_activities = self.running_activities.read().await;

            if let Some(running_activity) = running_activities.get(activity_id) {
                let workers = self.workers.read().await;
                if let Some(worker) = workers.get(&running_activity.2.worker_instance_id) {
                    worker
                        .tx
                        .send(Ok(ImmortalWorkerActionVersion {
                            version: Some(immortal_worker_action_version::Version::V1(
                                ImmortalWorkerActionV1 {
                                    action: Some(WorkerAction::KillActivity(
                                        activity_id.to_string(),
                                    )),
                                },
                            )),
                        }))
                        .await?;
                }
            }
        }
        Ok(())
    }

    pub fn watchdog(self: Arc<Self>) {
        // Tune these as you like
        const TICK_MS: u64 = 100;
        const MAX_KILL_ATTEMPTS: i32 = 3;
        // const WORKER_RPC_TIMEOUT_SECS: u64 = 2; // if you add ack later

        #[derive(Clone, Debug)]
        struct TimedOutActivity {
            activity_id: String,
            workflow_id: String,
            run_id: String,
            kill_state: KillState,
            // worker_id: String,
            worker_instance_id: Uuid,
        }

        #[derive(Clone, Debug)]
        struct TimedOutCall {
            call_id: String,
            // worker_id: String,
            worker_instance_id: Uuid,
        }

        #[derive(Clone, Debug)]
        struct OrphanedWorkflow {
            workflow_id: String,
            opts: ClientStartWorkflowOptionsV1,
        }

        tokio::spawn(async move {
            use chrono::Utc;
            use tokio::time::{sleep, Duration as TokioDuration};

            // Track how many times we’ve asked a worker to timeout/kill this activity
            // let mut activity_kill_attempts: HashMap<String, i32> = HashMap::new();

            loop {
                let orphaned_workflows: Vec<OrphanedWorkflow> = {
                    let guard = self.running_workflows.read().await;
                    let now = Utc::now();

                    guard
                        .iter()
                        .filter_map(|(workflow_id, boxed)| match boxed.1.kill_state {
                            KillState::Orphaned { first_seen } => {
                                let max_time = first_seen + TimeDelta::seconds(10);
                                if now > max_time {
                                    Some(OrphanedWorkflow {
                                        workflow_id: workflow_id.to_string(),
                                        opts: boxed.1.additional_properties.clone(),
                                    })
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        })
                        .collect()
                };

                // -------------------------
                // 1) Snapshot timed-out activities (no await inside loop holding locks)
                // -------------------------
                let timed_out_activities: Vec<TimedOutActivity> = {
                    let guard = self.running_activities.read().await;
                    let now = Utc::now();

                    guard
                        .iter()
                        .filter_map(|(activity_id, boxed)| {
                            let props = &boxed.2.additional_properties;
                            let max_time = props.last_heartbeat + boxed.2.heartbeat_timeout;

                            if now > max_time {
                                Some(TimedOutActivity {
                                    kill_state: boxed.2.kill_state.clone(),
                                    activity_id: activity_id.clone(),
                                    workflow_id: props.workflow_id.clone(),
                                    run_id: props.latest_run_id.clone(),
                                    // worker_id: boxed.2.worker_id.clone(),
                                    worker_instance_id: boxed.2.worker_instance_id.clone(),
                                })
                            } else {
                                None
                            }
                        })
                        .collect()
                };

                // -------------------------
                // 2) Snapshot timed-out calls
                // -------------------------
                let timed_out_calls: Vec<TimedOutCall> = {
                    let guard = self.running_calls.read().await;
                    let now = Utc::now();

                    guard
                        .iter()
                        .filter_map(|(call_id, boxed)| {
                            let props = &boxed.1;
                            if now > props.timeout {
                                Some(TimedOutCall {
                                    call_id: call_id.clone(),
                                    // worker_id: props.worker_id.clone(),
                                    worker_instance_id: props.worker_instance_id.clone(),
                                })
                            } else {
                                None
                            }
                        })
                        .collect()
                };

                // -------------------------
                // 3) Handle timed-out activities
                // -------------------------
                for t in timed_out_activities {
                    let kill_state = t.kill_state.clone();
                    let attempts = match kill_state {
                        KillState::Healthy => {
                            let mut guard = self.running_activities.write().await;
                            if let Some(activity) = guard.iter_mut().find(|f| *f.0 == t.activity_id)
                            {
                                activity.1 .2.kill_state = KillState::Suspected {
                                    first_seen: Utc::now(),
                                    attempts: 1,
                                };
                            };
                            1
                        }
                        // ORPHANS SHOULD NOT BE TIMED OUT AS WE WANT TO DO THE PROPER REBUMISSION
                        // FLOW
                        KillState::Orphaned { .. } => 0,
                        KillState::Suspected {
                            first_seen,
                            attempts,
                        } => {
                            let mut guard = self.running_activities.write().await;
                            if let Some(activity) = guard.iter_mut().find(|f| *f.0 == t.activity_id)
                            {
                                activity.1 .2.kill_state = KillState::Suspected {
                                    first_seen: first_seen.clone(),
                                    attempts: attempts + 1,
                                };
                            };
                            attempts + 1
                        }
                    };
                    // increment attempts
                    // let attempts = activity_kill_attempts
                    //     .entry(t.activity_id.clone())
                    //     .and_modify(|v| *v += 1)
                    //     .or_insert(1);

                    let force = attempts as i32 >= MAX_KILL_ATTEMPTS;

                    // Best-effort: ask worker to timeout the activity
                    let worker_tx = {
                        let workers = self.workers.read().await;
                        workers.get(&t.worker_instance_id).map(|w| w.tx.clone())
                    };

                    let mut sent_to_worker = false;
                    if let Some(tx) = worker_tx {
                        let msg = ImmortalWorkerActionVersion {
                            version: Some(immortal_worker_action_version::Version::V1(
                                ImmortalWorkerActionV1 {
                                    action: Some(WorkerAction::TimeoutActivity(
                                        t.activity_id.clone(),
                                    )),
                                },
                            )),
                        };

                        if tx.send(Ok(msg)).await.is_ok() {
                            sent_to_worker = true;
                        }
                    }

                    // If we *couldn’t* message the worker, or we hit the force threshold,
                    // finalize it server-side through the same completion path.
                    if !sent_to_worker || force {
                        // Build a forced timeout ActivityResultV1
                        let forced = ActivityResultV1 {
                            activity_id: t.activity_id.clone(),
                            workflow_id: t.workflow_id.clone(),
                            activity_run_id: t.run_id.clone(),
                            status: Some(immortal::activity_result_v1::Status::Timeout(
                                immortal::Failure {
                                    failure: Some(failure::Failure {
                                        message: if force {
                                            "timeout (forced by watchdog)".to_string()
                                        } else {
                                            "timeout (worker unreachable; watchdog)".to_string()
                                        },
                                        ..Default::default()
                                    }),
                                },
                            )),
                        };

                        // IMPORTANT:
                        let _ = self.completed_activity_inner(forced).await;

                        // Cleanup attempt tracking once it's finalized (best-effort)
                        // activity_kill_attempts.remove(&t.activity_id);
                    }
                }

                for wf in orphaned_workflows {
                    let mut wf_queue = self.workflow_queue.lock().await;
                    if !wf_queue.contains_key(&wf.workflow_id) {
                        if let Some(queue) = wf_queue.get_mut(&wf.opts.task_queue) {
                            queue.push_back(Box::new((wf.workflow_id, wf.opts, None, None)));
                        }
                    }
                }

                // -------------------------
                // 4) Handle timed-out calls
                // -------------------------
                for t in timed_out_calls {
                    // Best-effort: ask worker to kill call
                    let worker_tx = {
                        let workers = self.workers.read().await;
                        workers.get(&t.worker_instance_id).map(|w| w.tx.clone())
                    };

                    if let Some(tx) = worker_tx {
                        let msg = ImmortalWorkerActionVersion {
                            version: Some(immortal_worker_action_version::Version::V1(
                                ImmortalWorkerActionV1 {
                                    action: Some(WorkerAction::KillCall(t.call_id.clone())),
                                },
                            )),
                        };
                        let _ = tx.send(Ok(msg)).await;
                    }

                    // Always remove from running_calls so callers can proceed/fail appropriately.
                    // (If you want symmetric handling like activities, extract a `completed_call_inner` too.)
                    self.running_calls.write().await.remove(&t.call_id);
                }

                // Tick
                sleep(TokioDuration::from_millis(TICK_MS)).await;
            }
        });
    }
    pub fn call_queue_thread(&self) {
        let call_queue = Arc::clone(&self.call_queue);
        let running_calls = Arc::clone(&self.running_calls);
        let workers = Arc::clone(&self.workers);
        let notify = self.call_notify.clone();
        tokio::spawn(async move {
            loop {
                notify.notified().await;
                println!("running call queue");
                // Lock once and take a snapshot of queues
                let queues_snapshot: HashMap<String, _> = {
                    let call_queues = call_queue.lock().await;
                    call_queues
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                };

                // min: 1
                // max: 100
                // ratio: for every 10 tasks + 1 worker
                // interval: 10 mins
                println!("got snapshot");
                for (queue_name, queue) in queues_snapshot {
                    if queue.is_empty() {
                        println!("queue is empty so continuing");
                        continue;
                    }

                    // Try to assign one call from this queue
                    for (_index, queued_item) in queue.into_iter().enumerate() {
                        let (call_id, call_options, sender) = *queued_item;
                        //println!("executing {call_id}");
                        // Find eligible workers

                        if let Some(sender) = &sender {
                            if sender.is_closed() {
                                let mut call_queues = call_queue.lock().await;
                                if let Some(queue_vec) = call_queues.get_mut(&queue_name) {
                                    if let Some(pos) =
                                        queue_vec.iter().position(|x| (**x).0 == call_id)
                                    {
                                        queue_vec.remove(pos);
                                    }
                                    if queue_vec.is_empty() {
                                        call_queues.remove(&queue_name);
                                    }
                                }
                            }
                        }

                        let available_workers: Vec<_>;

                        {
                            let workers_guard = workers.read().await;

                            available_workers = workers_guard
                                .iter()
                                .filter(|(_, worker)| worker.task_queue == call_options.task_queue)
                                .map(|(_, worker)| {
                                    (
                                        worker.instance_id.clone(),
                                        worker.worker_id.clone(),
                                        worker.tx.clone(),
                                    )
                                })
                                .collect();
                        }

                        if available_workers.is_empty() {
                            break;
                        }

                        let chosen_worker_index =
                            rand::rng().random_range(0..available_workers.len());

                        if let Some(worker) = available_workers.get(chosen_worker_index) {
                            // Dispatch call to worker
                            if let Err(e) = worker
                                .2
                                .send(Ok(ImmortalWorkerActionVersion {
                                    version: Some(immortal_worker_action_version::Version::V1(
                                        ImmortalWorkerActionV1 {
                                            action: Some(WorkerAction::StartCall(
                                                immortal::StartCallOptionsV1 {
                                                    call_run_id: "0".to_string(),
                                                    call_id: call_id.clone(),
                                                    call_type: call_options.call_type.clone(),
                                                    call_input: call_options.input,
                                                },
                                            )),
                                        },
                                    )),
                                }))
                                .await
                            {
                                if available_workers.len() > 1 {
                                    notify.notify_one();
                                }
                                // match e.0 {
                                //    Ok(_) => {
                                //
                                //    },
                                //    Err(e) => match e {
                                //        Status
                                //
                                //    }
                                //
                                // }
                                error!("Failed to send call to worker {}: {:#?}", worker.0, e);
                                //TODO: need to go ahead, check the error, and then act accordingly
                                info!("{:#?}", available_workers);
                            } else {
                                // Remove the item from the actual call queue (not the snapshot)
                                {
                                    let mut call_queues = call_queue.lock().await;
                                    if let Some(queue_vec) = call_queues.get_mut(&queue_name) {
                                        if let Some(pos) =
                                            queue_vec.iter().position(|x| (**x).0 == call_id)
                                        {
                                            queue_vec.remove(pos);
                                        }
                                        if queue_vec.is_empty() {
                                            call_queues.remove(&queue_name);
                                        }
                                    }
                                }

                                // Register running call
                                {
                                    let mut running_calls = running_calls.write().await;
                                    let now = Utc::now();
                                    let timeout = now + Duration::seconds(30);
                                    running_calls.insert(
                                        call_id.clone(),
                                        Box::new((
                                            sender,
                                            RunningProperties {
                                                kill_state: KillState::Healthy,
                                                start: now.clone(),
                                                timeout,
                                                max_duration: Duration::seconds(30),
                                                worker_id: worker.1.clone(),
                                                worker_instance_id: worker.0.clone(),
                                                heartbeat_timeout: Duration::seconds(30),
                                                additional_properties: CallProperties {
                                                    last_heartbeat: now,
                                                },
                                            },
                                        )),
                                    );
                                }
                            }

                            //break; // Assign one call per loop per queue
                        } else {
                            println!("no available workers");
                        }
                    }
                }
                println!("finished running loop");
            }
        });
    }
    pub fn activity_queue_thread(&self) {
        let activity_queue = Arc::clone(&self.activity_queue);
        let running_activities = Arc::clone(&self.running_activities);
        let notification_tx = Arc::clone(&self.notification_tx);
        let workers = Arc::clone(&self.workers);
        let history = self.history.clone();

        let notify = self.activity_notify.clone();
        tokio::spawn(async move {
            loop {
                notify.notified().await;
                let mut activity_queues = activity_queue.lock().await;

                for (queue_name, queue) in activity_queues.iter_mut() {
                    if let Some(queued_item) = queue.pop_front() {
                        let (activity_run_id, activity_options, tx, scheduled, index) =
                            *queued_item;
                        if let Some(schedule_to_start_timeout) =
                            activity_options.schedule_to_start_timeout
                        {
                            let now = Utc::now();
                            let schedule_to_start_timeout: Duration =
                                schedule_to_start_timeout.into();
                            let max_time = scheduled + schedule_to_start_timeout;
                            if now > max_time {
                                if let Err(e) = tx.send(ActivityResultV1 {
                                    activity_id: activity_options.activity_id.clone(),

                                    workflow_id: activity_options.workflow_id.clone(),
                                    activity_run_id: activity_run_id.to_string(),

                                    status: Some(immortal::activity_result_v1::Status::Failed(
                                        immortal::Failure {
                                            failure: Some(failure::Failure {
                                                message: "timeout".to_string(),
                                                ..Default::default()
                                            }),
                                        },
                                    )),
                                }) {
                                    error!("{:#?}", e);
                                }
                                continue;
                            }
                        }
                        let available_workers: Vec<_>;
                        {
                            let workers_guard = workers.read().await;

                            available_workers = workers_guard
                                .iter()
                                .filter(|(_, worker)| {
                                    worker.task_queue == *queue_name
                                        && worker
                                            .registered_activities
                                            .contains_key(&activity_options.activity_type)
                                })
                                .map(|(_, worker)| {
                                    (
                                        worker.instance_id.clone(),
                                        worker.worker_id.clone(),
                                        worker.tx.clone(),
                                    )
                                })
                                .collect();
                        }

                        if available_workers.is_empty() {
                            queue.push_front(Box::new((
                                activity_run_id,
                                activity_options,
                                tx,
                                scheduled,
                                index,
                            )));
                            continue;
                        }

                        let random_index = rand::rng().random_range(0..available_workers.len());

                        if let Some(worker) = available_workers.get(random_index) {
                            let now = Utc::now();
                            let duration = Duration::seconds(30);
                            let timeout = now + duration;

                            running_activities.write().await.insert(
                                activity_options.activity_id.clone(),
                                Box::new((
                                    worker.0.clone(),
                                    tx,
                                    RunningProperties {
                                        start: now,
                                        timeout,
                                        max_duration: duration,
                                        worker_id: worker.1.clone(),
                                        worker_instance_id: worker.0.clone(),
                                        kill_state: KillState::Healthy,
                                        heartbeat_timeout: activity_options
                                            .heartbeat_timeout
                                            .map(|f| f.into())
                                            .unwrap_or(Duration::seconds(30)),
                                        additional_properties: ActivityProperties {
                                            // not sure this is right but we'll see
                                            latest_run_id: activity_run_id.clone(),
                                            workflow_id: activity_options.workflow_id.clone(),
                                            last_heartbeat: now,
                                            scheduled: scheduled.clone(),
                                            latest_run_start: None,
                                            index,
                                        },
                                    },
                                )),
                            );

                            let mut activity_history = ActivityHistory::new(
                                activity_options.workflow_id.clone(),
                                activity_options.activity_type.clone(),
                                activity_options.activity_id.clone(),
                                activity_options.task_queue.clone(),
                                activity_options.activity_input.clone(),
                                index,
                            );
                            activity_history.runs.push(ActivityRun::new(
                                activity_options.workflow_id.clone(),
                                activity_options.activity_id.clone(),
                                activity_run_id.to_string(),
                            ));

                            match history
                                .get_activity(
                                    &activity_options.workflow_id,
                                    &activity_options.activity_id,
                                )
                                .await
                            {
                                Ok(Some(mut existing)) => {
                                    existing.runs.push(ActivityRun::new(
                                        activity_options.workflow_id.clone(),
                                        activity_options.activity_id.clone(),
                                        activity_run_id.clone(),
                                    ));
                                    if let Err(e) = history
                                        .update_activity(
                                            &activity_options.workflow_id,
                                            existing.clone(),
                                        )
                                        .await
                                    {
                                        error!(
                                            "Failed to append run to existing activity: {:?}",
                                            e
                                        );
                                    }
                                    activity_history = existing;
                                }
                                _ => {
                                    if let Err(e) = history
                                        .add_activity(
                                            &activity_options.workflow_id,
                                            activity_history.clone(),
                                        )
                                        .await
                                    {
                                        error!("Failed to add activity to history: {:?}", e);
                                    }
                                }
                            }
                            if let Err(e) = worker
                                .2
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
                                                    activity_input: activity_options.activity_input,
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
                            {
                                running_activities
                                    .write()
                                    .await
                                    .remove(&activity_options.activity_id);

                                error!("Failed to send StartActivity to worker: {:#?}", e);
                                continue;
                            }

                            Self::adjust_capacity(
                                Arc::clone(&workers),
                                &worker.0.clone(),
                                -1,
                                AdjustCapacity::Activity,
                            )
                            .await;

                            if let Err(e) = notification_tx.send(Notification::ActivityRunStarted(
                                Uuid::parse_str(&activity_options.workflow_id).unwrap_or_default(),
                                activity_history,
                            )) {
                                error!("Failed to send notification: {:?}", e);
                            }
                        } else {
                            queue.push_front(Box::new((
                                activity_run_id,
                                activity_options,
                                tx,
                                scheduled,
                                index,
                            )));
                        }
                    }
                }

                activity_queues.retain(|_, q| !q.is_empty());
            }
        });
    }
    pub fn workflow_queue_thread(&self) {
        let workflow_queue = Arc::clone(&self.workflow_queue);
        let workers = Arc::clone(&self.workers);
        let notification_tx = Arc::clone(&self.notification_tx);
        let history = self.history.clone();

        let running_workflows = Arc::clone(&self.running_workflows);
        let pool = self.redis_pool.clone();
        let notify = self.workflow_notify.clone();
        tokio::spawn(async move {
            loop {
                notify.notified().await;

                // Snapshot and convert the queue structure
                let queues_snapshot: HashMap<
                    String,
                    Vec<(String, StartWorkflowOptionsV1, Option<watch::Sender<i32>>)>,
                > = {
                    let queue_guard = workflow_queue.lock().await;
                    let filtered_queues = queue_guard.iter().filter(|(_, v)| !v.is_empty());
                    let mut snapshot = HashMap::new();
                    for (queue_name, items) in filtered_queues {
                        let mut snapshot_queue = vec![];
                        for item in items {
                            let (id, client_opts, sender, cache) = *(item.clone());
                            let mut con = pool.get().await.unwrap();
                            let wf_history_metadata =
                                WorkflowHistoryMetadata::get_opt(&mut con, &id, false)
                                    .await
                                    .unwrap();
                            let mut epoch = 0;
                            if let Some(metadata) = wf_history_metadata {
                                epoch = metadata.epoch;
                            }
                            snapshot_queue.push((
                                id.clone(),
                                StartWorkflowOptionsV1 {
                                    epoch,
                                    cache: cache.unwrap_or(vec![]),
                                    // this might be incorrect
                                    workflow_id: id,
                                    workflow_type: client_opts.workflow_type,
                                    workflow_version: client_opts.workflow_version,
                                    task_queue: client_opts.task_queue,
                                    input: client_opts.input,
                                },
                                sender.clone(),
                            ))
                        }
                        snapshot.insert(queue_name.to_string(), snapshot_queue);
                    }
                    snapshot
                };

                for (queue_name, queue) in queues_snapshot {
                    if queue.is_empty() {
                        continue;
                    }

                    for (workflow_id, workflow_options, sender) in queue {
                        let available_workers: Vec<_>;
                        {
                            let workers_guard = workers.read().await;
                            available_workers = workers_guard
                                .iter()
                                .filter(|(_, worker)| {
                                    worker.task_queue == queue_name
                                        && worker
                                            .registered_workflows
                                            .contains_key(&workflow_options.workflow_type)
                                })
                                .map(|(_, worker)| {
                                    (
                                        worker.instance_id.clone(),
                                        worker.worker_id.clone(),
                                        worker.tx.clone(),
                                    )
                                })
                                .collect();
                        }

                        if available_workers.is_empty() {
                            println!("No available workers for workflow queue {}", queue_name);
                            break;
                        }

                        let chosen_index = rand::rng().random_range(0..available_workers.len());

                        if let Some(worker) = available_workers.get(chosen_index) {
                            // Remove the item from the actual queue
                            {
                                let mut queue_guard = workflow_queue.lock().await;
                                if let Some(vec) = queue_guard.get_mut(&queue_name) {
                                    if let Some(pos) =
                                        vec.iter().position(|x| (**x).0 == workflow_id)
                                    {
                                        vec.remove(pos);
                                    }
                                    if vec.is_empty() {
                                        queue_guard.remove(&queue_name);
                                    }
                                }
                            }

                            let now = Utc::now();
                            let duration = Duration::seconds(30);
                            let timeout = now + duration;
                            running_workflows.write().await.insert(
                                workflow_id.clone(),
                                Box::new((
                                    worker.0.clone(),
                                    RunningProperties {
                                        start: now,
                                        // this can be ignored for now
                                        timeout,
                                        // this can be ignored for now
                                        max_duration: duration,
                                        worker_id: worker.1.clone(),
                                        worker_instance_id: worker.0.clone(),
                                        kill_state: KillState::Healthy,
                                        heartbeat_timeout: Duration::seconds(0),
                                        additional_properties: ClientStartWorkflowOptionsV1 {
                                            workflow_type: workflow_options.workflow_type.clone(),
                                            workflow_version: workflow_options
                                                .workflow_version
                                                .clone(),
                                            input: workflow_options.input.clone(),
                                            task_queue: workflow_options.task_queue.clone(),
                                            workflow_id: Some(workflow_id.clone()),
                                        },
                                    },
                                )),
                            );
                            if let Some(tx) = sender {
                                if tx.receiver_count() == 0 {
                                    println!("DROPPING WORKFLOW AS RECEVIER NO LONG EXISTS");
                                    continue;
                                }
                            }

                            // Build and store history
                            let workflow_history = WorkflowHistory::new(
                                workflow_options.workflow_type.clone(),
                                workflow_id.clone(),
                                workflow_options
                                    .input
                                    .clone()
                                    .map(|i| i.payloads)
                                    .unwrap_or_default(),
                                queue_name.clone(),
                                worker.1.clone(),
                                worker.0.clone(),
                            );

                            if let Err(e) = history.add_workflow(workflow_history.clone()).await {
                                error!("Failed to add workflow to history: {:?}", e);
                            }

                            // Send to worker
                            if let Err(e) = worker
                                .2
                                .send(Ok(ImmortalWorkerActionVersion {
                                    version: Some(immortal_worker_action_version::Version::V1(
                                        ImmortalWorkerActionV1 {
                                            action: Some(WorkerAction::StartWorkflow(
                                                StartWorkflowOptionsV1 {
                                                    epoch: workflow_options.epoch,
                                                    cache: vec![],
                                                    workflow_id: workflow_id.clone(),
                                                    workflow_type: workflow_options
                                                        .workflow_type
                                                        .clone(),
                                                    workflow_version: workflow_options
                                                        .workflow_version
                                                        .clone(),
                                                    task_queue: workflow_options.task_queue.clone(),
                                                    input: workflow_options.input,
                                                },
                                            )),
                                        },
                                    )),
                                }))
                                .await
                            {
                                running_workflows.write().await.remove(&workflow_id);
                                error!("Failed to send workflow to worker: {:#?}", e);
                                continue;
                            }
                            Self::adjust_capacity(
                                Arc::clone(&workers),
                                &worker.0.clone(),
                                -1,
                                AdjustCapacity::Workflow,
                            )
                            .await;

                            if let Err(e) = notification_tx.send(Notification::WorkflowStarted(
                                Uuid::parse_str(&workflow_id).unwrap_or_default(),
                                workflow_history,
                            )) {
                                error!("Failed to send workflow notification: {:?}", e);
                            }

                            //break;
                        }
                    }
                }
                println!("DONE");
            }
        });
    }

    pub async fn start_activity_internal(
        &self,
        call_options: CallVersion,
    ) -> anyhow::Result<CallResultVersion> {
        match call_options.version {
            Some(call_version::Version::V1(call)) => {
                let (tx, mut rx) = mpsc::channel::<CallResultV1>(100);

                {
                    let mut queue = self.call_queue.lock().await;
                    match queue.get_mut(&call.call_type) {
                        Some(queue) => {
                            queue.push_back(Box::new((
                                Uuid::new_v4().to_string(),
                                CallOptions {
                                    call_type: call.call_type.clone(),
                                    input: call.input,
                                    task_queue: call.task_queue.clone(),
                                },
                                Some(tx),
                            )));
                        }
                        None => {
                            let mut queue2 = VecDeque::new();
                            queue2.push_back(Box::new((
                                Uuid::new_v4().to_string(),
                                CallOptions {
                                    call_type: call.call_type.clone(),
                                    input: call.input,
                                    task_queue: call.task_queue.clone(),
                                },
                                Some(tx),
                            )));
                            queue.insert(call.call_type.clone(), queue2);
                        }
                    }
                }

                self.call_notify.notify_one();
                match rx.recv().await {
                    Some(payload) => Ok(CallResultVersion {
                        version: Some(call_result_version::Version::V1(payload)),
                    }),
                    None => Err(anyhow::anyhow!("Call failed")),
                }
            }
            _ => Err(anyhow::anyhow!("unsupported version")),
        }
    }

    pub async fn start_workflow_internal(
        &self,
        workflow_options: ClientStartWorkflowOptionsVersion,
        sender: Option<watch::Sender<i32>>,
        activity_cache: Option<Vec<ActivityCache>>,
    ) -> Result<String, Status> {
        Ok(match workflow_options.version {
            Some(client_start_workflow_options_version::Version::V1(workflow_options)) => {
                let workflow_id = workflow_options
                    .workflow_id
                    .clone()
                    .unwrap_or(Uuid::new_v4().to_string());
                let mut wq = self.workflow_queue.lock().await;

                match wq.get_mut(&workflow_options.task_queue) {
                    Some(queue) => {
                        queue.push_back(Box::new((
                            workflow_id.clone(),
                            workflow_options.clone(),
                            sender,
                            activity_cache,
                        )));
                    }
                    None => {
                        let mut queue = VecDeque::new();
                        queue.push_back(Box::new((
                            workflow_id.clone(),
                            workflow_options.clone(),
                            sender,
                            activity_cache,
                        )));
                        wq.insert(workflow_options.task_queue.clone(), queue);
                    }
                }
                self.workflow_notify.notify_one();
                workflow_id
            }
            _ => {
                return Err(Status::internal("unsupported version"));
            }
        })
    }
}
