use anyhow::anyhow;
use bb8_redis::{
    bb8::{Pool, PooledConnection, RunError},
    RedisConnectionManager,
};
use const_format::formatcp;

use anyhow::Result;
use chrono::{DateTime, Utc};
use immortal_lib::common::Payload;
use redis::{aio::MultiplexedConnection, AsyncCommands, RedisError};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use simd_json::OwnedValue;
use uuid::Uuid;

// STRUCTS

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StatusFilter {
    Running,
    // #[serde(with = "serde_bytes")]
    Completed,
    // Completed(String),
    // Completed(Value),
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "spec")]
pub enum Status {
    Running,
    // #[serde(with = "serde_bytes")]
    Completed(OwnedValue),
    // Completed(String),
    // Completed(Value),
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "version", content = "spec")]
pub enum WorkflowHistoryVersion {
    V1(WorkflowHistory),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowHistory {
    pub args: Vec<OwnedValue>,
    pub output: Option<OwnedValue>,
    pub workflow_id: String,
    pub workflow_type: String,
    pub status: Status,
    pub activities: Vec<ActivityHistory>,
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
    pub task_queue: Option<String>,
    pub worker_id: Option<String>,
    // pub status: Status,
}

impl WorkflowHistory {
    pub fn new(
        workflow_type: String,
        workflow_id: String,
        args: Vec<OwnedValue>,
        task_queue: String,
        worker_id: String,
    ) -> Self {
        Self {
            args,
            output: None,
            workflow_type,
            workflow_id,
            status: Status::Running,
            activities: Vec::new(),
            start_time: chrono::Utc::now(),
            end_time: None,
            task_queue: Some(task_queue),
            worker_id: Some(worker_id),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityHistory {
    pub activity_id: String,
    pub activity_type: String,
    pub args: Option<OwnedValue>,
    pub output: Option<OwnedValue>,
    pub task_queue: Option<String>,
    pub input: Option<Payload>,
    // pub status: Status,
    // pub result: Option<Value>,
    pub runs: Vec<ActivityRun>,
    pub index: usize,
}

impl ActivityHistory {
    pub fn new(
        activity_type: String,
        activity_id: String,
        args: Option<OwnedValue>,
        task_queue: String,
        input: Option<Payload>,
        index: usize,
    ) -> Self {
        Self {
            activity_id,
            activity_type,
            args,
            input,
            task_queue: Some(task_queue),
            output: None,
            // status: Status::Running,
            runs: Vec::new(),
            index,
        }
    }

    pub fn add_run(&mut self, run: ActivityRun) {
        if self.runs.iter().any(|r| r.run_id == run.run_id) {
            return;
        }
        self.runs.push(run);
    }
    pub fn get_run(&self, run_id: &str) -> Option<&ActivityRun> {
        self.runs.iter().find(|r| r.run_id == run_id)
    }
    pub fn get_run_mut(&mut self, run_id: &str) -> Option<&mut ActivityRun> {
        self.runs.iter_mut().find(|r| r.run_id == run_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityRun {
    pub run_id: String,
    pub status: Status,
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
}

impl ActivityRun {
    pub fn new(run_id: String) -> Self {
        Self {
            run_id,
            status: Status::Running,
            start_time: chrono::Utc::now(),
            end_time: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct History(Pool<RedisConnectionManager>);
const BASE_REDIS_KEY: &str = "immortal:history";
const TTL: i64 = 259_200;
const WORKFLOW_BASE_REDIS_KEY: &str = formatcp!("{}:workflow", BASE_REDIS_KEY);
impl History {
    pub fn new(pool: &Pool<RedisConnectionManager>) -> Self {
        Self(pool.clone())
    }

    async fn get_con(
        &self,
    ) -> std::result::Result<PooledConnection<'_, RedisConnectionManager>, RunError<RedisError>>
    {
        self.0.get().await
    }

    // -------------------------------------------------------
    // delete entire workflow history
    // -------------------------------------------------------
    pub async fn delete_history(&self, workflow_id: &Uuid) -> Result<()> {
        let mut con = self.get_con().await?;
        let wf_id = workflow_id.to_string();
        let wf_meta = workflow_meta_key(&wf_id);

        // Remove from workflow index list
        let _: () = con
            .lrem(
                format!("{WORKFLOW_BASE_REDIS_KEY}:workflow_index"),
                0,
                &wf_id,
            )
            .await?;

        // Find all activity ids
        let act_ids: Vec<String> = con
            .lrange(workflow_activities_list_key(&wf_id), 0, -1)
            .await?;

        let mut keys_to_del = vec![
            wf_meta.clone(),
            workflow_args_key(&wf_id),
            workflow_output_key(&wf_id),
            workflow_status_blob_key(&wf_id),
            workflow_activities_list_key(&wf_id),
            format!("immortal:logs:{wf_id}"),
        ];

        for act_id in &act_ids {
            // delete runs for this activity
            let runs_list_key = activity_runs_list_key(&wf_id, act_id);
            let run_ids: Vec<String> = con.lrange(&runs_list_key, 0, -1).await?;

            for run_id in run_ids {
                keys_to_del.push(run_base_key(&wf_id, act_id, &run_id));
                keys_to_del.push(run_status_blob_key(&wf_id, act_id, &run_id));
            }

            keys_to_del.push(runs_list_key);
            keys_to_del.push(activity_base_key(&wf_id, act_id));
            keys_to_del.push(activity_args_key(&wf_id, act_id));
            keys_to_del.push(activity_input_key(&wf_id, act_id));
            keys_to_del.push(activity_output_key(&wf_id, act_id));
        }

        let _: () = con.del(keys_to_del).await?;
        Ok(())
    }

    // -------------------------------------------------------
    // sync workflow_index list with actual workflow keys
    // (keeps pagination list clean)
    // -------------------------------------------------------
    pub async fn sync_workflow_index(&self) -> Result<()> {
        let mut con = self.get_con().await?;

        // Collect existing workflow IDs from keys:
        // immortal:history:workflow:<id>
        let currently_logged_workflows: Vec<String> = con
            .keys::<&str, Vec<String>>(format!("{WORKFLOW_BASE_REDIS_KEY}:*").as_str())
            .await?
            .into_iter()
            .filter_map(|k: String| {
                k.strip_prefix(&format!("{WORKFLOW_BASE_REDIS_KEY}:"))
                    .map(|id| id.to_string())
            })
            .collect();

        let mut offset = 0;
        let limit = 100;

        loop {
            let workflows_in_index: Vec<String> = con
                .lrange(
                    format!("{WORKFLOW_BASE_REDIS_KEY}:workflow_index"),
                    offset,
                    offset + limit - 1,
                )
                .await?;

            if workflows_in_index.is_empty() {
                break;
            }

            for wf_id in workflows_in_index {
                if !currently_logged_workflows.contains(&wf_id) {
                    // remove all occurrences of this ID from index
                    let _: () = con
                        .lrem(
                            format!("{WORKFLOW_BASE_REDIS_KEY}:workflow_index"),
                            0,
                            &wf_id,
                        )
                        .await?;
                }
                offset += 1;
            }
        }

        Ok(())
    }

    // -------------------------------------------------------
    // add a new workflow
    // -------------------------------------------------------
    pub async fn add_workflow(&self, workflow: WorkflowHistory) -> Result<()> {
        let mut con = self.get_con().await?;
        let wf_id = workflow.workflow_id.clone();
        let wf_meta = workflow_meta_key(&wf_id);

        if con.exists(&wf_meta).await? {
            return Err(anyhow!("Workflow already exists"));
        }

        // Add to workflow index (for pagination)
        let _: () = con
            .lpush(format!("{WORKFLOW_BASE_REDIS_KEY}:workflow_index"), &wf_id)
            .await?;

        // Store workflow metadata hash (including status tag)
        let status_clone = workflow.status.clone();
        let _: () = bb8_redis::redis::pipe()
            .cmd("HSET")
            .arg(&wf_meta)
            .arg("version")
            .arg("V1")
            .arg("workflow_type")
            .arg(&workflow.workflow_type)
            .arg("status_tag")
            .arg(status_tag(&workflow.status))
            .arg("start_time")
            .arg(workflow.start_time.to_rfc3339())
            .arg("end_time")
            .arg(
                workflow
                    .end_time
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(String::new),
            )
            .arg("task_queue")
            .arg(workflow.task_queue.clone().unwrap_or_default())
            .arg("worker_id")
            .arg(workflow.worker_id.clone().unwrap_or_default())
            .ignore()
            .query_async(&mut *con)
            .await?;

        // Store full status + args + output as blobs
        set_blob(&mut con, &workflow_status_blob_key(&wf_id), &status_clone).await?;
        set_blob(&mut con, &workflow_args_key(&wf_id), &workflow.args).await?;
        set_blob(&mut con, &workflow_output_key(&wf_id), &workflow.output).await?;

        // Store activities (IDs list + each activity)
        let act_list_key = workflow_activities_list_key(&wf_id);

        for activity in &workflow.activities {
            let act_id = &activity.activity_id;

            // Append activity_id to ordered list
            let _: () = con.rpush(&act_list_key, act_id).await?;

            // Store activity itself
            self.store_activity(&mut con, &wf_id, act_id, activity)
                .await?;
        }

        // Optional TTL on top-level workflow metadata key
        let _: () = con.expire(&wf_meta, TTL).await?;

        println!("WRITING TO REDIS (add workflow)");
        Ok(())
    }

    // -------------------------------------------------------
    // update existing workflow (replace metadata + blobs + activities)
    // -------------------------------------------------------
    pub async fn update_workflow(
        &self,
        workflow_id: &str,
        workflow: WorkflowHistory,
    ) -> Result<()> {
        let mut con = self.get_con().await?;
        let wf_id = workflow_id.to_string();
        let wf_meta = workflow_meta_key(&wf_id);

        if !con.exists(&wf_meta).await? {
            return Err(anyhow!("Workflow does not exist"));
        }

        // Overwrite metadata hash
        let status_clone = workflow.status.clone();
        let _: () = redis::pipe()
            .cmd("HSET")
            .arg(&wf_meta)
            .arg("version")
            .arg("V1")
            .arg("workflow_type")
            .arg(&workflow.workflow_type)
            .arg("status_tag")
            .arg(status_tag(&workflow.status))
            .arg("start_time")
            .arg(workflow.start_time.to_rfc3339())
            .arg("end_time")
            .arg(
                workflow
                    .end_time
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(String::new),
            )
            .arg("task_queue")
            .arg(workflow.task_queue.clone().unwrap_or_default())
            .arg("worker_id")
            .arg(workflow.worker_id.clone().unwrap_or_default())
            .ignore()
            .query_async(&mut *con)
            .await?;

        // Overwrite status + args + output blobs
        set_blob(&mut con, &workflow_status_blob_key(&wf_id), &status_clone).await?;
        set_blob(&mut con, &workflow_args_key(&wf_id), &workflow.args).await?;
        set_blob(&mut con, &workflow_output_key(&wf_id), &workflow.output).await?;

        // Rebuild activities (list + per-activity keys)
        let act_list_key = workflow_activities_list_key(&wf_id);
        let old_act_ids: Vec<String> = con.lrange(&act_list_key, 0, -1).await?;

        // Delete old activities and their runs
        let mut keys_to_del: Vec<String> = Vec::new();
        for act_id in &old_act_ids {
            let runs_list_key = activity_runs_list_key(&wf_id, act_id);
            let run_ids: Vec<String> = con.lrange(&runs_list_key, 0, -1).await?;

            for run_id in run_ids {
                keys_to_del.push(run_base_key(&wf_id, act_id, &run_id));
                keys_to_del.push(run_status_blob_key(&wf_id, act_id, &run_id));
            }

            keys_to_del.push(runs_list_key);
            keys_to_del.push(activity_base_key(&wf_id, act_id));
            keys_to_del.push(activity_args_key(&wf_id, act_id));
            keys_to_del.push(activity_input_key(&wf_id, act_id));
            keys_to_del.push(activity_output_key(&wf_id, act_id));
        }
        keys_to_del.push(act_list_key.clone());
        if !keys_to_del.is_empty() {
            let _: () = con.del(keys_to_del).await?;
        }

        // Write new list + activities
        for activity in &workflow.activities {
            let act_id = &activity.activity_id;
            let _: () = con.rpush(&act_list_key, act_id).await?;
            self.store_activity(&mut con, &wf_id, act_id, activity)
                .await?;
        }

        let _: () = con.expire(&wf_meta, TTL).await?;

        println!("WRITING TO REDIS (update workflow)");
        Ok(())
    }

    // -------------------------------------------------------
    // fetch a single workflow (full)
    // -------------------------------------------------------
    pub async fn get_workflow(&self, workflow_id: &str) -> Result<Option<WorkflowHistory>> {
        let mut con = self.get_con().await?;
        let wf_meta = workflow_meta_key(workflow_id);

        if !con.exists(&wf_meta).await? {
            return Ok(None);
        }

        // metadata hash
        let meta_val: redis::Value = redis::cmd("HGETALL")
            .arg(&wf_meta)
            .query_async(&mut *con)
            .await?;
        let meta: std::collections::HashMap<String, String> = redis::from_redis_value(&meta_val)?;

        let workflow_type = meta.get("workflow_type").cloned().unwrap_or_default();
        let status_tag_str = meta
            .get("status_tag")
            .cloned()
            .unwrap_or_else(|| "Running".to_string());
        let start_time: DateTime<Utc> = meta
            .get("start_time")
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(Utc::now);
        let end_time: Option<DateTime<Utc>> =
            meta.get("end_time")
                .and_then(|s| if s.is_empty() { None } else { s.parse().ok() });
        let task_queue = meta.get("task_queue").cloned();
        let worker_id = meta.get("worker_id").cloned();

        // blobs
        let status_full: Status = get_blob(&mut con, &workflow_status_blob_key(workflow_id))
            .await?
            .unwrap_or(Status::Running);
        let status = status_from_tag_and_blob(&status_tag_str, status_full);

        let args: Vec<OwnedValue> = get_blob(&mut con, &workflow_args_key(workflow_id))
            .await?
            .unwrap_or_default();
        let output: Option<OwnedValue> = get_blob(&mut con, &workflow_output_key(workflow_id))
            .await?
            .unwrap_or(None);

        // activities
        let act_ids: Vec<String> = con
            .lrange(workflow_activities_list_key(workflow_id), 0, -1)
            .await?;
        let mut activities = Vec::with_capacity(act_ids.len());

        for (idx, act_id) in act_ids.into_iter().enumerate() {
            if let Some(mut act) = self.load_activity(&mut con, workflow_id, &act_id).await? {
                act.index = idx;
                activities.push(act);
            }
        }

        let wf = WorkflowHistory {
            args,
            output,
            workflow_id: workflow_id.to_string(),
            workflow_type,
            status,
            activities,
            start_time,
            end_time,
            task_queue,
            worker_id,
        };

        Ok(Some(wf))
    }

    // --
    // get workflow history summary
    // --
    pub async fn get_workflow_summary(
        &self,
        workflow_id: &str,
    ) -> Result<Option<WorkflowHistorySummary>> {
        let mut con = self.get_con().await?;
        let wf_meta = workflow_meta_key(workflow_id);

        if !con.exists(&wf_meta).await? {
            return Ok(None);
        }

        // metadata hash
        let meta_val: redis::Value = redis::cmd("HGETALL")
            .arg(&wf_meta)
            .query_async(&mut *con)
            .await?;
        let meta: std::collections::HashMap<String, String> = redis::from_redis_value(&meta_val)?;

        let workflow_type = meta.get("workflow_type").cloned().unwrap_or_default();
        let status_tag_str = meta
            .get("status_tag")
            .cloned()
            .unwrap_or_else(|| "Running".to_string());
        let start_time: DateTime<Utc> = meta
            .get("start_time")
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(Utc::now);
        let end_time: Option<DateTime<Utc>> =
            meta.get("end_time")
                .and_then(|s| if s.is_empty() { None } else { s.parse().ok() });
        let task_queue = meta.get("task_queue").cloned();
        let worker_id = meta.get("worker_id").cloned();

        // blobs
        let status_summary =
            get_blob_ref(&mut con, &workflow_status_blob_key(workflow_id), None).await?;
        let status = status_summary_from_tag_and_blob_ref(&status_tag_str, status_summary);

        let args = get_blob_ref(&mut con, &workflow_args_key(workflow_id), None).await?;
        let output = get_blob_ref(&mut con, &workflow_output_key(workflow_id), None).await?;

        // activities
        let act_ids: Vec<String> = con
            .lrange(workflow_activities_list_key(workflow_id), 0, -1)
            .await?;
        let mut activities = Vec::with_capacity(act_ids.len());

        for (idx, act_id) in act_ids.into_iter().enumerate() {
            if let Some(mut act) = self
                .load_activity_summary(&mut con, workflow_id, &act_id)
                .await?
            {
                act.index = idx;
                activities.push(act);
            }
        }

        let wf = WorkflowHistorySummary {
            args,
            output,
            workflow_id: workflow_id.to_string(),
            workflow_type,
            status,
            activities,
            start_time,
            end_time,
            task_queue,
            worker_id,
        };

        Ok(Some(wf))
    }

    // -------------------------------------------------------
    // number of activities for a workflow
    // -------------------------------------------------------
    pub async fn get_workflow_activity_len(&self, workflow_id: &str) -> Result<usize> {
        let mut con = self.get_con().await?;
        let len: i64 = con.llen(workflow_activities_list_key(workflow_id)).await?;
        Ok(len as usize)
    }

    // -------------------------------------------------------
    // list/paginate workflows (fully hydrated)
    // -------------------------------------------------------
    pub async fn get_workflows(
        &self,
        limit: Option<usize>,
        offset: Option<usize>,
        task_queues: Option<Vec<String>>,
        worker_ids: Option<Vec<String>>,
        status: Option<StatusFilter>,
    ) -> Result<Vec<WorkflowHistoryVersion>> {
        let limit = limit.unwrap_or(10) as isize;
        let offset = offset.unwrap_or(0) as isize;
        let start = offset as isize;
        let end = if limit == 0 {
            -1
        } else {
            (offset + limit - 1) as isize
        };

        let mut con = self.get_con().await?;

        let ids: Vec<String> = con
            .lrange(
                format!("{WORKFLOW_BASE_REDIS_KEY}:workflow_index"),
                start,
                end,
            )
            .await?;

        if ids.is_empty() {
            return Ok(vec![]);
        }

        let mut workflows: Vec<WorkflowHistoryVersion> = Vec::new();

        for wf_id in ids {
            if let Some(wf) = self.get_workflow(&wf_id).await? {
                workflows.push(WorkflowHistoryVersion::V1(wf));
            }
        }

        // Filtering in-memory (small n: paginated)
        if let Some(status_filter) = status {
            workflows = workflows
                .into_iter()
                .filter(|f| match f {
                    WorkflowHistoryVersion::V1(v1) => match status_filter {
                        StatusFilter::Failed => matches!(v1.status, Status::Failed(..)),
                        StatusFilter::Running => matches!(v1.status, Status::Running),
                        StatusFilter::Completed => matches!(v1.status, Status::Completed(..)),
                    },
                })
                .collect();
        }

        if let Some(task_queues) = task_queues {
            workflows = workflows
                .into_iter()
                .filter(|f| match f {
                    WorkflowHistoryVersion::V1(v1) => {
                        if let Some(history_task_queue) = &v1.task_queue {
                            task_queues.contains(history_task_queue)
                        } else {
                            false
                        }
                    }
                })
                .collect();
        }

        if let Some(worker_ids) = worker_ids {
            workflows = workflows
                .into_iter()
                .filter(|f| match f {
                    WorkflowHistoryVersion::V1(v1) => {
                        if let Some(history_worker_id) = &v1.worker_id {
                            worker_ids.contains(history_worker_id)
                        } else {
                            false
                        }
                    }
                })
                .collect();
        }

        Ok(workflows)
    }

    // -------------------------------------------------------
    // list/paginate workflows (partialy hydrated)
    // -------------------------------------------------------
    pub async fn get_workflows_summary(
        &self,
        limit: Option<usize>,
        offset: Option<usize>,
        task_queues: Option<Vec<String>>,
        worker_ids: Option<Vec<String>>,
        status: Option<StatusFilter>,
    ) -> Result<Vec<WorkflowHistoryVersionSummary>> {
        let limit = limit.unwrap_or(10) as isize;
        let offset = offset.unwrap_or(0) as isize;
        let start = offset as isize;
        let end = if limit == 0 {
            -1
        } else {
            (offset + limit - 1) as isize
        };

        let mut con = self.get_con().await?;

        let ids: Vec<String> = con
            .lrange(
                format!("{WORKFLOW_BASE_REDIS_KEY}:workflow_index"),
                start,
                end,
            )
            .await?;

        if ids.is_empty() {
            return Ok(vec![]);
        }

        let mut workflows: Vec<WorkflowHistoryVersionSummary> = Vec::new();

        for wf_id in ids {
            if let Some(wf) = self.get_workflow_summary(&wf_id).await? {
                workflows.push(WorkflowHistoryVersionSummary::V1(wf));
            }
        }

        // Filtering in-memory (small n: paginated)
        if let Some(status_filter) = status {
            workflows = workflows
                .into_iter()
                .filter(|f| match f {
                    WorkflowHistoryVersionSummary::V1(v1) => match status_filter {
                        StatusFilter::Failed => matches!(v1.status, StatusSummary::Failed(..)),
                        StatusFilter::Running => matches!(v1.status, StatusSummary::Running),
                        StatusFilter::Completed => {
                            matches!(v1.status, StatusSummary::Completed(..))
                        }
                    },
                })
                .collect();
        }

        if let Some(task_queues) = task_queues {
            workflows = workflows
                .into_iter()
                .filter(|f| match f {
                    WorkflowHistoryVersionSummary::V1(v1) => {
                        if let Some(history_task_queue) = &v1.task_queue {
                            task_queues.contains(history_task_queue)
                        } else {
                            false
                        }
                    }
                })
                .collect();
        }

        if let Some(worker_ids) = worker_ids {
            workflows = workflows
                .into_iter()
                .filter(|f| match f {
                    WorkflowHistoryVersionSummary::V1(v1) => {
                        if let Some(history_worker_id) = &v1.worker_id {
                            worker_ids.contains(history_worker_id)
                        } else {
                            false
                        }
                    }
                })
                .collect();
        }

        Ok(workflows)
    }

    // -------------------------------------------------------
    // add activity: append id to LIST + store data
    // -------------------------------------------------------
    pub async fn add_activity(&self, workflow_id: &str, activity: ActivityHistory) -> Result<()> {
        let mut con = self.get_con().await?;
        let wf_id = workflow_id.to_string();
        let act_id = activity.activity_id.clone();

        // Maintain order
        let _: () = con
            .rpush(workflow_activities_list_key(&wf_id), &act_id)
            .await?;

        let _: () = con
            .expire(&workflow_activities_list_key(&wf_id), TTL)
            .await?;
        self.store_activity(&mut con, &wf_id, &act_id, &activity)
            .await?;

        println!("WRITING TO REDIS (add activity)");
        Ok(())
    }

    // -------------------------------------------------------
    // update existing activity (by activity_id)
    // -------------------------------------------------------
    pub async fn update_activity(
        &self,
        workflow_id: &str,
        activity: ActivityHistory,
    ) -> Result<()> {
        let mut con = self.get_con().await?;
        let wf_id = workflow_id.to_string();
        let act_id = activity.activity_id.clone();

        // We don't change ordering here; we just overwrite the data
        self.store_activity(&mut con, &wf_id, &act_id, &activity)
            .await?;
        println!("WRITING TO REDIS (update activity)");
        Ok(())
    }

    // -------------------------------------------------------
    // get a single activity by id
    // -------------------------------------------------------
    pub async fn get_activity(
        &self,
        workflow_id: &str,
        activity_id: &str,
    ) -> Result<Option<ActivityHistory>> {
        let mut con = self.get_con().await?;

        // Find index for UI (optional)
        let act_ids: Vec<String> = con
            .lrange(workflow_activities_list_key(workflow_id), 0, -1)
            .await?;

        let index = act_ids.iter().position(|id| id == activity_id).unwrap_or(0);

        if let Some(mut act) = self
            .load_activity(&mut con, workflow_id, activity_id)
            .await?
        {
            act.index = index;
            Ok(Some(act))
        } else {
            Ok(None)
        }
    }

    // -------------------------------------------------------
    // internal: store one activity (meta + blobs + runs)
    // -------------------------------------------------------
    async fn store_activity(
        &self,
        con: &mut MultiplexedConnection,
        workflow_id: &str,
        activity_id: &str,
        activity: &ActivityHistory,
    ) -> Result<()> {
        let base = activity_base_key(workflow_id, activity_id);

        // activity metadata
        let _: () = redis::pipe()
            .cmd("HSET")
            .arg(&base)
            .arg("activity_id")
            .arg(&activity.activity_id)
            .arg("activity_type")
            .arg(&activity.activity_type)
            .arg("task_queue")
            .arg(activity.task_queue.clone().unwrap_or_default())
            .ignore()
            .query_async(con)
            .await?;

        // blobs for args/input/output
        set_blob(
            con,
            &activity_args_key(workflow_id, activity_id),
            &activity.args,
        )
        .await?;
        set_blob(
            con,
            &activity_input_key(workflow_id, activity_id),
            &activity.input,
        )
        .await?;
        set_blob(
            con,
            &activity_output_key(workflow_id, activity_id),
            &activity.output,
        )
        .await?;

        // runs: store list of run_ids + each run separately
        let runs_list_key = activity_runs_list_key(workflow_id, activity_id);

        // overwrite runs list completely for simplicity: delete + rebuild
        let _: () = con.del(&runs_list_key).await?;

        for run in &activity.runs {
            // append id to list
            let _: () = con.rpush(&runs_list_key, &run.run_id).await?;
            let _: () = con.expire(&runs_list_key, TTL).await?;
            // store run separately
            self.store_run(con, workflow_id, activity_id, run).await?;
        }

        let _: () = con.expire(&base, TTL).await?;

        Ok(())
    }

    // -------------------------------------------------------
    // internal: load one activity (meta + blobs + runs)
    // -------------------------------------------------------
    async fn load_activity(
        &self,
        con: &mut MultiplexedConnection,
        workflow_id: &str,
        activity_id: &str,
    ) -> Result<Option<ActivityHistory>> {
        let base = activity_base_key(workflow_id, activity_id);

        if !con.exists(&base).await? {
            return Ok(None);
        }

        let meta_val: redis::Value = redis::cmd("HGETALL").arg(&base).query_async(con).await?;
        let meta: std::collections::HashMap<String, String> = redis::from_redis_value(&meta_val)?;

        let activity_type = meta.get("activity_type").cloned().unwrap_or_default();
        let task_queue = meta.get("task_queue").cloned();

        let args: Option<OwnedValue> =
            get_blob(con, &activity_args_key(workflow_id, activity_id)).await?;
        let input: Option<Payload> =
            get_blob(con, &activity_input_key(workflow_id, activity_id)).await?;
        let output: Option<OwnedValue> =
            get_blob(con, &activity_output_key(workflow_id, activity_id)).await?;

        // load runs list
        let run_ids: Vec<String> = con
            .lrange(activity_runs_list_key(workflow_id, activity_id), 0, -1)
            .await?;
        let mut runs = Vec::with_capacity(run_ids.len());
        for run_id in run_ids {
            if let Some(run) = self
                .load_run(con, workflow_id, activity_id, &run_id)
                .await?
            {
                runs.push(run);
            }
        }

        Ok(Some(ActivityHistory {
            activity_id: activity_id.to_string(),
            activity_type,
            args,
            output,
            task_queue,
            input,
            runs,
            index: 0, // caller fills actual index based on activities list
        }))
    }

    // -------------------------------------------------------
    // internal: load one activity summary (meta + refs + run summaries)
    // -------------------------------------------------------
    async fn load_activity_summary(
        &self,
        con: &mut MultiplexedConnection,
        workflow_id: &str,
        activity_id: &str,
    ) -> Result<Option<ActivityHistorySummary>> {
        let base = activity_base_key(workflow_id, activity_id);

        if !con.exists(&base).await? {
            return Ok(None);
        }

        let meta_val: redis::Value = redis::cmd("HGETALL").arg(&base).query_async(con).await?;
        let meta: std::collections::HashMap<String, String> = redis::from_redis_value(&meta_val)?;

        let activity_type = meta.get("activity_type").cloned().unwrap_or_default();
        let task_queue = meta.get("task_queue").cloned();

        let args = get_blob_ref(con, &activity_args_key(workflow_id, activity_id), None).await?;
        let input = get_blob_ref(con, &activity_input_key(workflow_id, activity_id), None).await?;
        let output =
            get_blob_ref(con, &activity_output_key(workflow_id, activity_id), None).await?;

        // load runs list
        let run_ids: Vec<String> = con
            .lrange(activity_runs_list_key(workflow_id, activity_id), 0, -1)
            .await?;
        let mut runs = Vec::with_capacity(run_ids.len());
        for run_id in run_ids {
            if let Some(run) = self
                .load_run_summary(con, workflow_id, activity_id, &run_id)
                .await?
            {
                runs.push(run);
            }
        }

        Ok(Some(ActivityHistorySummary {
            activity_id: activity_id.to_string(),
            activity_type,
            args,
            output,
            task_queue,
            input,
            runs,
            index: 0, // caller fills actual index based on activities list
        }))
    }

    // -------------------------------------------------------
    // internal: store one run (meta + status blob)
    // -------------------------------------------------------
    async fn store_run(
        &self,
        con: &mut MultiplexedConnection,
        workflow_id: &str,
        activity_id: &str,
        run: &ActivityRun,
    ) -> Result<()> {
        let base = run_base_key(workflow_id, activity_id, &run.run_id);

        let _: () = redis::pipe()
            .cmd("HSET")
            .arg(&base)
            .arg("run_id")
            .arg(&run.run_id)
            .arg("status_tag")
            .arg(status_tag(&run.status))
            .arg("start_time")
            .arg(run.start_time.to_rfc3339())
            .arg("end_time")
            .arg(
                run.end_time
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(String::new),
            )
            .ignore()
            .query_async(con)
            .await?;

        // full status as blob
        set_blob(
            con,
            &run_status_blob_key(workflow_id, activity_id, &run.run_id),
            &run.status,
        )
        .await?;

        let _: () = con.expire(&base, TTL).await?;
        Ok(())
    }

    // -------------------------------------------------------
    // internal: load one run (meta + status blob)
    // -------------------------------------------------------
    async fn load_run(
        &self,
        con: &mut MultiplexedConnection,
        workflow_id: &str,
        activity_id: &str,
        run_id: &str,
    ) -> Result<Option<ActivityRun>> {
        let base = run_base_key(workflow_id, activity_id, run_id);

        if !con.exists(&base).await? {
            return Ok(None);
        }

        let meta_val: redis::Value = redis::cmd("HGETALL").arg(&base).query_async(con).await?;
        let meta: std::collections::HashMap<String, String> = redis::from_redis_value(&meta_val)?;

        let status_tag_str = meta
            .get("status_tag")
            .cloned()
            .unwrap_or_else(|| "Running".to_string());

        let start_time: DateTime<Utc> = meta
            .get("start_time")
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(Utc::now);
        let end_time: Option<DateTime<Utc>> =
            meta.get("end_time")
                .and_then(|s| if s.is_empty() { None } else { s.parse().ok() });

        let full_status: Status =
            get_blob(con, &run_status_blob_key(workflow_id, activity_id, run_id))
                .await?
                .unwrap_or(Status::Running);

        let status = status_from_tag_and_blob(&status_tag_str, full_status);

        Ok(Some(ActivityRun {
            run_id: run_id.to_string(),
            status,
            start_time,
            end_time,
        }))
    }

    // -------------------------------------------------------
    // internal: load one run (meta + status summary)
    // -------------------------------------------------------
    async fn load_run_summary(
        &self,
        con: &mut MultiplexedConnection,
        workflow_id: &str,
        activity_id: &str,
        run_id: &str,
    ) -> Result<Option<ActivityRunSummary>> {
        let base = run_base_key(workflow_id, activity_id, run_id);

        if !con.exists(&base).await? {
            return Ok(None);
        }

        let meta_val: redis::Value = redis::cmd("HGETALL").arg(&base).query_async(con).await?;
        let meta: std::collections::HashMap<String, String> = redis::from_redis_value(&meta_val)?;

        let status_tag_str = meta
            .get("status_tag")
            .cloned()
            .unwrap_or_else(|| "Running".to_string());

        let start_time: DateTime<Utc> = meta
            .get("start_time")
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(Utc::now);
        let end_time: Option<DateTime<Utc>> =
            meta.get("end_time")
                .and_then(|s| if s.is_empty() { None } else { s.parse().ok() });

        let full_status = get_blob_ref(
            con,
            &run_status_blob_key(workflow_id, activity_id, run_id),
            None,
        )
        .await?;

        let status = status_summary_from_tag_and_blob_ref(&status_tag_str, full_status);

        Ok(Some(ActivityRunSummary {
            run_id: run_id.to_string(),
            status,
            start_time,
            end_time,
        }))
    }
}

// binary blob helpers ------------------------------------

async fn set_blob<T: Serialize>(
    con: &mut MultiplexedConnection,
    key: &str,
    value: &T,
) -> Result<()> {
    let bytes = simd_json::to_string(value)?;
    let _: () = con.set_ex(key, bytes, TTL as u64).await?;
    Ok(())
}

pub async fn get_blob<T: DeserializeOwned>(
    con: &mut MultiplexedConnection,
    key: &str,
) -> Result<Option<T>> {
    let bytes: Option<Vec<u8>> = con.get(key).await?;
    Ok(match bytes {
        None => None,
        Some(mut b) => Some(simd_json::from_slice(&mut b)?),
    })
}

async fn get_blob_ref(
    con: &mut MultiplexedConnection,
    key: &str,
    max_size: Option<usize>,
) -> Result<BlobRef> {
    let max_size = max_size.unwrap_or(100_000);
    if con.exists(&key).await? {
        let size: usize = con.strlen(key).await?;
        if size > max_size {
            Ok(BlobRef {
                path: key.to_string(),
                size,
                present: true,
                data: None,
            })
        } else {
            let data: String = con.get(key).await?;
            Ok(BlobRef {
                path: key.to_string(),
                size,
                present: true,
                data: Some(data),
            })
        }
    } else {
        Ok(BlobRef {
            path: key.to_string(),
            size: 0,
            present: false,
            data: None,
        })
    }
}
//
// -------- key helpers + blob helpers + status helpers -------
//

fn workflow_meta_key(workflow_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}")
}

fn workflow_args_key(workflow_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:args")
}

fn workflow_output_key(workflow_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:output")
}

fn workflow_status_blob_key(workflow_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:status")
}

fn workflow_activities_list_key(workflow_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:activities")
}

fn activity_base_key(workflow_id: &str, activity_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:activity:{activity_id}")
}

fn activity_args_key(workflow_id: &str, activity_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:activity:{activity_id}:args")
}

fn activity_input_key(workflow_id: &str, activity_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:activity:{activity_id}:input")
}

fn activity_output_key(workflow_id: &str, activity_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:activity:{activity_id}:output")
}

fn activity_runs_list_key(workflow_id: &str, activity_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:activity:{activity_id}:runs_list")
}

fn run_base_key(workflow_id: &str, activity_id: &str, run_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:activity:{activity_id}:run:{run_id}")
}

fn run_status_blob_key(workflow_id: &str, activity_id: &str, run_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:activity:{activity_id}:run:{run_id}:status")
}

// status helpers -----------------------------------------

fn status_tag(status: &Status) -> &'static str {
    match status {
        Status::Running => "Running",
        Status::Completed(_) => "Completed",
        Status::Failed(_) => "Failed",
    }
}

fn status_from_tag_and_blob(tag: &str, full: Status) -> Status {
    // For now, we just trust the blob and use tag for classification/filtering.
    // If you ever want to "downgrade" Completed payloads, you can do it here.
    match tag {
        "Running" => Status::Running,
        "Completed" => full,
        "Failed" => full,
        _ => full,
    }
}

fn status_summary_from_tag_and_blob_ref(tag: &str, blob_ref: BlobRef) -> StatusSummary {
    // For now, we just trust the blob and use tag for classification/filtering.
    // If you ever want to "downgrade" Completed payloads, you can do it here.
    match tag {
        "Running" => StatusSummary::Running,
        "Completed" => StatusSummary::Completed(blob_ref),
        "Failed" => StatusSummary::Completed(blob_ref),
        _ => StatusSummary::Running,
    }
}

// TRUNCATED HISTORY

fn approx_size(v: &OwnedValue) -> usize {
    use simd_json::OwnedValue::*;

    match v {
        Static(_x) => 8,
        String(s) => s.len(),
        Array(arr) => arr.iter().map(approx_size).sum(),
        Object(obj) => obj.iter().map(|(k, v)| k.len() + approx_size(v)).sum(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "spec")]
pub enum TruncatedValue<T> {
    Full(T),
    Truncated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "spec")]
pub enum TruncatedStatus {
    Running,
    // #[serde(with = "serde_bytes")]
    Completed(TruncatedValue<OwnedValue>),
    // Completed(String),
    // Completed(Value),
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "version", content = "spec")]
pub enum TruncatedWorkflowHistoryVersion {
    V1(TruncatedWorkflowHistory),
}
impl Into<TruncatedWorkflowHistoryVersion> for WorkflowHistoryVersion {
    fn into(self) -> TruncatedWorkflowHistoryVersion {
        match self {
            Self::V1(x) => TruncatedWorkflowHistoryVersion::V1(x.into()),
        }
    }
}

impl Into<TruncatedWorkflowHistory> for WorkflowHistory {
    fn into(self) -> TruncatedWorkflowHistory {
        let mut args_approx_size = 0;
        for arg in &self.args {
            args_approx_size += approx_size(arg)
        }
        TruncatedWorkflowHistory {
            worker_id: self.worker_id,
            workflow_type: self.workflow_type,
            workflow_id: self.workflow_id,
            status: self.status.into(),
            start_time: self.start_time,
            end_time: self.end_time,
            args: match args_approx_size > 10_000 {
                true => TruncatedValue::Truncated,
                false => TruncatedValue::Full(self.args),
            },
            output: match self.output {
                Some(x) => {
                    if approx_size(&x) > 10_000 {
                        TruncatedValue::Truncated
                    } else {
                        TruncatedValue::Full(Some(x))
                    }
                }
                None => TruncatedValue::Full(None),
            },

            task_queue: self.task_queue,
            activities: self.activities.into_iter().map(|f| f.into()).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TruncatedWorkflowHistory {
    pub args: TruncatedValue<Vec<OwnedValue>>,
    pub output: TruncatedValue<Option<OwnedValue>>,
    pub workflow_id: String,
    pub workflow_type: String,
    // this needs to be TruncatedStatus
    pub status: TruncatedStatus,
    pub activities: Vec<TruncatedActivityHistory>,
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
    pub task_queue: Option<String>,
    pub worker_id: Option<String>,
    // pub status: Status,
}

impl Into<TruncatedStatus> for Status {
    fn into(self) -> TruncatedStatus {
        match self {
            Self::Running => TruncatedStatus::Running,
            Self::Failed(x) => TruncatedStatus::Failed(x),
            Self::Completed(x) => {
                if approx_size(&x) > 10_000 {
                    TruncatedStatus::Completed(TruncatedValue::Truncated)
                } else {
                    TruncatedStatus::Completed(TruncatedValue::Full(x))
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TruncatedActivityHistory {
    pub activity_id: String,
    pub activity_type: String,
    pub args: TruncatedValue<Option<OwnedValue>>,
    pub output: TruncatedValue<Option<OwnedValue>>,
    pub task_queue: Option<String>,
    pub input: TruncatedValue<Option<Payload>>,
    // pub status: Status,
    // pub result: Option<Value>,
    pub runs: Vec<TruncatedActivityRun>,
}

impl Into<TruncatedActivityHistory> for ActivityHistory {
    fn into(self) -> TruncatedActivityHistory {
        TruncatedActivityHistory {
            activity_id: self.activity_id,
            activity_type: self.activity_type.into(),
            args: match self.args {
                Some(x) => {
                    if approx_size(&x) > 10_000 {
                        TruncatedValue::Truncated
                    } else {
                        TruncatedValue::Full(Some(x))
                    }
                }
                None => TruncatedValue::Full(None),
            },
            input: match self.input {
                Some(x) => {
                    if x.data.len() > 10_000 {
                        TruncatedValue::Truncated
                    } else {
                        TruncatedValue::Full(Some(x))
                    }
                }
                None => TruncatedValue::Full(None),
            },
            output: match self.output {
                Some(x) => {
                    if approx_size(&x) > 10_000 {
                        TruncatedValue::Truncated
                    } else {
                        TruncatedValue::Full(Some(x))
                    }
                }
                None => TruncatedValue::Full(None),
            },

            task_queue: self.task_queue,
            runs: self.runs.into_iter().map(|f| f.into()).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TruncatedActivityRun {
    pub run_id: String,
    pub status: TruncatedStatus,
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
}

impl Into<TruncatedActivityRun> for ActivityRun {
    fn into(self) -> TruncatedActivityRun {
        TruncatedActivityRun {
            run_id: self.run_id,
            status: self.status.into(),
            start_time: self.start_time,
            end_time: self.end_time,
        }
    }
}
//
//
//

//
// #[derive(Debug, Clone, Serialize, Deserialize)]
// pub enum StatusSummary {
//     Running,
//     Completed,
//     Failed
// }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "spec")]
pub enum StatusSummary {
    Running,
    // #[serde(with = "serde_bytes")]
    Completed(BlobRef),
    // Completed(String),
    // Completed(Value),
    Failed(BlobRef),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "spec")]
pub enum TruncatedBlob {
    Truncated,
    // #[serde(with = "serde_bytes")]
    Full(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlobRef {
    // logical path, not Redis key
    pub path: String, // "workflow.output", "activity:ID.output"
    pub size: usize,
    pub present: bool,
    pub data: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "version", content = "spec")]
pub enum WorkflowHistoryVersionSummary {
    V1(WorkflowHistorySummary),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowHistorySummary {
    pub args: BlobRef,
    pub output: BlobRef,
    pub workflow_id: String,
    pub workflow_type: String,
    // this needs to be TruncatedStatus
    pub status: StatusSummary,
    pub activities: Vec<ActivityHistorySummary>,
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
    pub task_queue: Option<String>,
    pub worker_id: Option<String>,
    // pub status: Status,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityHistorySummary {
    pub activity_id: String,
    pub activity_type: String,
    pub args: BlobRef,
    pub output: BlobRef,
    pub task_queue: Option<String>,
    pub input: BlobRef,
    pub index: usize,
    // pub status: Status,
    // pub result: Option<Value>,
    pub runs: Vec<ActivityRunSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityRunSummary {
    pub run_id: String,
    pub status: StatusSummary,
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
}
