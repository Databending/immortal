use std::collections::HashMap;

use anyhow::anyhow;
use anyhow::Result;
use bb8_redis::{
    bb8::{Pool, PooledConnection, RunError},
    RedisConnectionManager,
};
use blake3::Hasher;
use chrono::Duration;
use chrono::TimeDelta;
use chrono::{DateTime, Utc};
use const_format::formatcp;
use immortal_lib::common::Payload;
use redis::FromRedisValue;
use redis::{aio::MultiplexedConnection, AsyncCommands, RedisError};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use strum::{AsRefStr, EnumString};
use uuid::Uuid;

use crate::history_metadata::WorkerOwner;
use crate::history_metadata::{
    ActivityHistoryMetadata, ActivityRunHistoryMetadata, WorkflowHistoryMetadata,
};
use crate::immortal_ttl;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, AsRefStr, EnumString)]
pub enum Status {
    Running,
    Completed,
    Failed,
    Orphaned,
    Sleeping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "version", content = "spec")]
pub enum WorkflowHistoryVersion {
    V1(WorkflowHistory),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowHistory {
    pub args: Vec<Payload>,
    pub output: Option<Payload>,
    pub workflow_id: String,
    pub workflow_type: String,
    pub status: Status,
    pub epoch: u64,
    pub activities: Vec<ActivityHistory>,
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
    pub task_queue: String,
    pub worker_id: Option<String>,
    pub worker_instance_id: Option<Uuid>,
    // pub status: Status,
}

impl WorkflowHistory {
    pub fn new(
        workflow_type: String,
        workflow_id: String,
        args: Vec<Payload>,
        task_queue: String,
        worker_id: String,
        worker_instance_id: Uuid,
        epoch: u64,
    ) -> Self {
        Self {
            args,
            output: None,
            workflow_type,
            workflow_id,
            epoch,
            status: Status::Running,
            worker_instance_id: Some(worker_instance_id),
            activities: Vec::new(),
            start_time: chrono::Utc::now(),
            end_time: None,
            task_queue: task_queue,
            worker_id: Some(worker_id),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityHistory {
    pub activity_id: String,
    pub activity_type: String,
    pub hash: String,
    pub start_time: DateTime<Utc>,
    // pub args: Option<Payload>,
    // pub output: Option<Payload>,
    pub task_queue: Option<String>,
    pub input: Option<Payload>,
    // pub status: Status,
    // pub result: Option<Value>,
    pub runs: Vec<ActivityRun>,
    pub index: usize,
    // NEED THIS FOR BLOB REF
    pub workflow_id: String,
    pub schedule_to_start_timeout: Option<Duration>,
    pub start_to_close_timeout: Option<Duration>,
    pub schedule_to_close_timeout: Option<Duration>,
    pub heartbeat_timeout: Option<Duration>,
}

impl ActivityHistory {
    pub fn hash(activity_type: &str, input: &Option<Payload>) -> String {
        let mut h = Hasher::new();
        h.update(activity_type.as_bytes());

        if let Some(p) = input {
            h.update(&p.data);

            // metadata must be deterministic
            let mut kv: Vec<_> = p.metadata.iter().collect();
            kv.sort_by(|a, b| a.0.cmp(b.0));
            for (k, v) in kv {
                h.update(k.as_bytes());
                h.update(v);
            }
        }

        h.finalize().to_hex().to_string()
    }
    pub fn new(
        workflow_id: String,
        activity_type: String,
        activity_id: String,
        task_queue: String,
        input: Option<Payload>,
        index: usize,
        idempotency_key: String,
        schedule_to_start_timeout: Option<Duration>,
        schedule_to_close_timeout: Option<Duration>,
        start_to_close_timeout: Option<Duration>,
        heartbeat_timeout: Option<Duration>,
    ) -> Self {
        Self {
            hash: if idempotency_key == "" {
                Self::hash(&activity_type, &input)
            } else {
                idempotency_key
            },
            activity_id,
            activity_type,
            workflow_id,
            start_time: Utc::now(),
            // args,
            input,
            task_queue: Some(task_queue),
            // output: None,
            // status: Status::Running,
            runs: Vec::new(),
            index,
            schedule_to_close_timeout,
            schedule_to_start_timeout,
            start_to_close_timeout,
            heartbeat_timeout,
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
    pub workflow_id: String,
    pub activity_id: String,
    pub run_id: String,
    pub owner: Option<WorkerOwner>,
    pub status: Status,
    pub output: Option<Payload>,
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
    pub workflow_epoch: u64,
}

impl ActivityRun {
    pub fn new(
        workflow_id: String,
        activity_id: String,
        run_id: String,
        owner: Option<WorkerOwner>,
        workflow_epoch: u64,
    ) -> Self {
        Self {
            owner,
            workflow_id,
            activity_id,
            run_id,
            status: Status::Running,
            start_time: chrono::Utc::now(),
            end_time: None,
            output: None,
            workflow_epoch,
        }
    }
}

#[derive(Debug, Clone)]
pub struct History(Pool<RedisConnectionManager>);
const BASE_REDIS_KEY: &str = "immortal:history";

pub const WORKFLOW_BASE_REDIS_KEY: &str = formatcp!("{}:workflow", BASE_REDIS_KEY);
impl History {
    pub fn new(pool: &Pool<RedisConnectionManager>) -> Self {
        Self(pool.clone())
    }

    pub async fn get_con(
        &self,
    ) -> std::result::Result<PooledConnection<'_, RedisConnectionManager>, RunError<RedisError>>
    {
        self.0.get().await
    }

    pub async fn store_activity_run_output(
        &self,
        workflow_id: &str,
        activity_id: &str,
        run_id: &str,
        output: Payload,
    ) -> Result<()> {
        let mut con = self.get_con().await?;

        set_blob_raw(
            &mut con,
            &run_output_blob_key(&workflow_id, activity_id, run_id),
            &output.data,
        )
        .await?;
        Ok(())
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

        let wf_metadata = WorkflowHistoryMetadata::get_opt(&mut con, &wf_id, true).await?;

        // Find all activity ids
        let act_ids: Vec<String> = con
            .lrange(workflow_activities_list_key(&wf_id), 0, -1)
            .await?;

        let mut keys_to_del = vec![
            wf_meta.clone(),
            workflow_output_key(&wf_id),
            workflow_status_blob_key(&wf_id),
            workflow_activities_list_key(&wf_id),
            format!("immortal:logs:{wf_id}"),
        ];

        if let Some(wf_metadata) = wf_metadata {
            for (i, _) in wf_metadata.args.iter().enumerate() {
                keys_to_del.push(workflow_args_key(&wf_id, i));
            }
        }

        for act_id in &act_ids {
            // delete runs for this activity
            let runs_list_key = activity_runs_list_key(&wf_id, act_id);
            let run_ids: Vec<String> = con.lrange(&runs_list_key, 0, -1).await?;

            for run_id in run_ids {
                keys_to_del.push(run_base_key(&wf_id, act_id, &run_id));
                keys_to_del.push(run_output_blob_key(&wf_id, act_id, &run_id));
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
            // check and see if it's the next epoch  (safe to unwrap as it exists)
            let existing_wf = WorkflowHistoryMetadata::get_opt(&mut con, &wf_id, false)
                .await?
                .unwrap();
            if workflow.epoch <= existing_wf.epoch
                && workflow.task_queue == existing_wf.task_queue
                && workflow.workflow_type == existing_wf.workflow_type
            {
                return Err(anyhow!("Workflow already exists"));
            }
        }

        // Add to workflow index (for pagination)
        let _: () = con
            .lpush(format!("{WORKFLOW_BASE_REDIS_KEY}:workflow_index"), &wf_id)
            .await?;

        let wf_metadata: WorkflowHistoryMetadata = (&workflow).into();

        // we don't need to store children because we add_workflow itself is already recursive
        wf_metadata.store(&mut con, false).await?;

        for (i, arg) in workflow.args.iter().enumerate() {
            set_blob_raw(&mut con, &workflow_args_key(&wf_id, i), &arg.data).await?;
        }
        if let Some(output) = workflow.output {
            set_blob_raw(&mut con, &workflow_output_key(&wf_id), &output.data).await?;
        }

        // for (i, arg) in workflow.

        // Store activities (IDs list + each activity)
        let act_list_key = workflow_activities_list_key(&wf_id);

        for activity in &workflow.activities {
            let act_id = &activity.activity_id;

            // Append activity_id to ordered list
            let _: () = con.rpush(&act_list_key, act_id).await?;

            // Store activity itself
            self.store_activity(&mut con, &wf_id, act_id, activity, true)
                .await?;
        }

        // Metadata and the blobs it points at have to expire together -- see `refresh_ttl`.
        refresh_ttl(
            &mut con,
            std::iter::once(wf_meta.clone())
                .chain(std::iter::once(workflow_activities_list_key(&wf_id)))
                .chain((0..workflow.args.len()).map(|i| workflow_args_key(&wf_id, i)))
                .chain(std::iter::once(workflow_output_key(&wf_id))),
        )
        .await?;

        // println!("WRITING TO REDIS (add workflow)");
        Ok(())
    }

    pub async fn store_workflow_output(&self, workflow_id: &str, output: Payload) -> Result<()> {
        let mut con = self.get_con().await?;

        set_blob_raw(&mut con, &workflow_output_key(&workflow_id), &output.data).await?;
        Ok(())
    }
    pub async fn get_workflow_output(&self, workflow_id: &str) -> Result<Option<Vec<u8>>> {
        let mut con = self.get_con().await?;

        get_blob_raw::<Vec<u8>>(&mut con, &workflow_output_key(&workflow_id)).await
    }
    // -------------------------------------------------------
    // update existing workflow (replace metadata + blobs + activities)
    // -------------------------------------------------------
    pub async fn update_workflow(
        &self,
        workflow_id: &str,
        workflow: WorkflowHistory,
        store_children: bool,
    ) -> Result<()> {
        let mut con = self.get_con().await?;
        let wf_id = workflow_id.to_string();
        let wf_meta = workflow_meta_key(&wf_id);

        if !con.exists(&wf_meta).await? {
            return Err(anyhow!("Workflow does not exist"));
        }

        let wf_metadata: WorkflowHistoryMetadata = (&workflow).into();

        // we don't need to store children because we add_workflow itself is already recursive
        wf_metadata.store(&mut con, false).await?;

        for (i, arg) in workflow.args.iter().enumerate() {
            set_blob_raw(&mut con, &workflow_args_key(&wf_id, i), &arg.data).await?;
        }
        if let Some(output) = workflow.output {
            set_blob_raw(&mut con, &workflow_output_key(&wf_id), &output.data).await?;
        }

        if store_children {
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
            // Write new list + activities
            for activity in &workflow.activities {
                let act_id = &activity.activity_id;
                let _: () = con.rpush(&act_list_key, act_id).await?;
                self.store_activity(&mut con, &wf_id, act_id, activity, store_children)
                    .await?;
            }
            if !keys_to_del.is_empty() {
                let _: () = con.del(keys_to_del).await?;
            }
        }

        let _: () = con.expire(&wf_meta, immortal_ttl()).await?;

        // println!("WRITING TO REDIS (update workflow)");
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

        let wf_metadata = WorkflowHistoryMetadata::get_opt(&mut con, workflow_id, true)
            .await?
            .unwrap();

        let mut args = vec![];

        for (i, blob_ref) in wf_metadata.args.into_iter().enumerate() {
            let data: Vec<u8> = get_blob(&mut con, &workflow_args_key(workflow_id, i))
                .await?
                .unwrap_or_default();
            args.push(Payload {
                metadata: blob_ref.metadata.unwrap(),
                data,
            });
        }

        let mut output = None;

        if let Some(outputx) = wf_metadata.output {
            let data: Vec<u8> = get_blob(&mut con, &workflow_output_key(workflow_id))
                .await?
                .unwrap_or_default();
            output = Some(Payload {
                metadata: outputx.metadata.unwrap(),
                data,
            })
        }

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
            epoch: wf_metadata.epoch,
            worker_instance_id: wf_metadata.owner.map(|f| f.instance_id),
            args,
            output,
            workflow_id: workflow_id.to_string(),
            workflow_type: wf_metadata.workflow_type,
            status: wf_metadata.status,
            activities,
            start_time: wf_metadata.start_time,
            end_time: wf_metadata.end_time,
            task_queue: wf_metadata.task_queue,
            worker_id: wf_metadata.worker_id,
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
                        StatusFilter::Failed => matches!(v1.status, Status::Failed),
                        StatusFilter::Running => matches!(v1.status, Status::Running),
                        StatusFilter::Completed => matches!(v1.status, Status::Completed),
                    },
                })
                .collect();
        }

        if let Some(task_queues) = task_queues {
            workflows = workflows
                .into_iter()
                .filter(|f| match f {
                    WorkflowHistoryVersion::V1(v1) => task_queues.contains(&v1.task_queue),
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
            .expire(&workflow_activities_list_key(&wf_id), immortal_ttl())
            .await?;
        self.store_activity(&mut con, &wf_id, &act_id, &activity, true)
            .await?;

        // println!("WRITING TO REDIS (add activity)");
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
        // this might not always be true
        self.store_activity(&mut con, &wf_id, &act_id, &activity, true)
            .await?;
        // println!("WRITING TO REDIS (update activity)");
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

        match self.load_activity(&mut con, workflow_id, activity_id).await {
            Ok(act) => {
                if let Some(mut act) = act {
                    act.index = index;
                    Ok(Some(act))
                } else {
                    Ok(None)
                }
            }
            Err(error) => {
                println!("{} {}", error.backtrace(), error);
                Ok(None)
            }
        }

        // if let Some(mut act) = self
        //     .load_activity(&mut con, workflow_id, activity_id)
        //     .await?
        // {
        //     act.index = index;
        //     Ok(Some(act))
        // } else {
        //     Ok(None)
        // }
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
        store_children: bool,
    ) -> Result<()> {
        let base = activity_base_key(workflow_id, activity_id);

        // activity metadata

        let activity_metadata: ActivityHistoryMetadata = activity.into();

        // we don't need to store children because we already do it here.
        activity_metadata.store(con, workflow_id, false).await?;

        if let Some(input) = &activity.input {
            set_blob_raw(
                con,
                &activity_input_key(workflow_id, activity_id),
                &input.data,
            )
            .await?;
        }

        if store_children {
            // runs: store list of run_ids + each run separately
            let runs_list_key = activity_runs_list_key(workflow_id, activity_id);

            // overwrite runs list completely for simplicity: delete + rebuild
            let _: () = con.del(&runs_list_key).await?;

            for run in &activity.runs {
                // append id to list
                let _: () = con.rpush(&runs_list_key, &run.run_id).await?;
                let _: () = con.expire(&runs_list_key, immortal_ttl()).await?;
                // store run separately
                self.store_run(con, run).await?;
            }
        }

        // Metadata and the blobs it points at have to expire together -- see `refresh_ttl`.
        refresh_ttl(
            con,
            [
                base.clone(),
                activity_runs_list_key(workflow_id, activity_id),
                activity_input_key(workflow_id, activity_id),
            ],
        )
        .await?;

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

        let activity_metadata = ActivityHistoryMetadata::get_opt(con, workflow_id, activity_id)
            .await?
            .unwrap();

        // WHAT I DID HEAR IS PRETTY HACKY AND MIGHT BIGHT MY ASS IN THE FUTURE
        // THE ISSUE IS THAT I DON'T STORE AN INPUT IF IT DOESN'T EXIST (history2.rs 718)
        // TECHNICALLY THIS IS CORRECT THOUGH
        let input: Option<Payload> = get_blob(con, &activity_input_key(workflow_id, activity_id))
            .await
            .unwrap_or(None);
        // let output: Option<OwnedValue> =
        //     get_blob(con, &activity_output_key(workflow_id, activity_id)).await?;

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
            start_time: activity_metadata.start_time,
            workflow_id: workflow_id.to_string(),
            activity_id: activity_id.to_string(),
            hash: activity_metadata.hash,
            activity_type: activity_metadata.activity_type,
            task_queue: activity_metadata.task_queue,
            schedule_to_close_timeout: activity_metadata.schedule_to_close_timeout,
            schedule_to_start_timeout: activity_metadata.schedule_to_start_timeout,
            start_to_close_timeout: activity_metadata.start_to_close_timeout,
            heartbeat_timeout: activity_metadata.heartbeat_timeout,
            input,
            runs,
            index: 0, // caller fills actual index based on activities list
        }))
    }

    // -------------------------------------------------------
    // internal: store one run (meta + status blob)
    // -------------------------------------------------------
    async fn store_run(&self, con: &mut MultiplexedConnection, run: &ActivityRun) -> Result<()> {
        let base = run_base_key(&run.workflow_id, &run.activity_id, &run.run_id);

        let run_metadata: ActivityRunHistoryMetadata = run.into();
        run_metadata
            .store_run(con, &run.workflow_id, &run.activity_id)
            .await?;

        // // full status as blob
        // set_blob(
        //     con,
        //     &run_status_blob_key(&run.workflow_id, &run.activity_id, &run.run_id),
        //     &run.status,
        // )
        // .await?;

        refresh_ttl(
            con,
            std::iter::once(base.clone()).chain(
                run.output
                    .iter()
                    .map(|_| run_output_blob_key(&run.workflow_id, &run.activity_id, &run.run_id)),
            ),
        )
        .await?;
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

        let activity_run_metadata =
            ActivityRunHistoryMetadata::get_opt(con, workflow_id, activity_id, run_id)
                .await?
                .unwrap();

        let mut output = None;

        if let Some(blob_ref) = activity_run_metadata.output {
            let data: Vec<u8> =
                get_blob_raw(con, &run_output_blob_key(workflow_id, activity_id, run_id))
                    .await?
                    .unwrap_or_default();
            output = Some(Payload {
                metadata: blob_ref.metadata.clone().unwrap_or_default(),
                data,
            })
        }

        Ok(Some(ActivityRun {
            owner: activity_run_metadata.owner,
            workflow_id: workflow_id.to_string(),
            activity_id: activity_id.to_string(),
            run_id: run_id.to_string(),
            status: activity_run_metadata.status,
            start_time: activity_run_metadata.start_time,
            end_time: activity_run_metadata.end_time,
            output,
            workflow_epoch: activity_run_metadata.workflow_epoch,
        }))
    }
}

// binary blob helpers ------------------------------------
//
// async fn set_blob<T: Serialize>(
//     con: &mut MultiplexedConnection,
//     key: &str,
//     value: &T,
// ) -> Result<()> {
//     let bytes = simd_json::to_string(value)?;
//     let _: () = con.set_ex(key, bytes, TTL as u64).await?;
//     Ok(())
// }

async fn set_blob_raw(con: &mut MultiplexedConnection, key: &str, data: &Vec<u8>) -> Result<()> {
    // let bytes = simd_json::to_string(value)?;
    let _: () = con.set_ex(key, data, immortal_ttl() as u64).await?;
    Ok(())
}

/// Push a set of keys out to the standard TTL window in one round trip.
///
/// A blob is written once with its own `SET ... EX` and never touched again, while the metadata
/// pointing at it gets a fresh `EXPIRE` on every store. Refreshing only the metadata lets the
/// blob expire out from under it: `to_payload` then quietly yields an empty payload for a run
/// history still marked `Completed`, and a retry cannot find its input at all. So every site that
/// extends a metadata key's life has to extend its blobs' too.
///
/// `EXPIRE` on a missing key is a no-op, so callers may pass paths that were never written.
pub async fn refresh_ttl<I>(con: &mut MultiplexedConnection, keys: I) -> Result<()>
where
    I: IntoIterator<Item = String>,
{
    let mut pipe = redis::pipe();
    let mut any = false;
    for key in keys {
        pipe.cmd("EXPIRE").arg(key).arg(immortal_ttl()).ignore();
        any = true;
    }
    if any {
        let _: () = pipe.query_async(con).await?;
    }
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

pub async fn get_blob_raw<T: FromRedisValue>(
    con: &mut MultiplexedConnection,
    key: &str,
) -> Result<Option<T>> {
    let bytes: Option<T> = con.get(key).await?;
    Ok(match bytes {
        None => None,
        Some(b) => Some(b),
    })
}

pub fn payload_to_blob_ref(path: String, payload: &Payload) -> BlobRef {
    BlobRef {
        path,
        data: None,
        size: payload.data.len(),
        present: true,
        loaded: false,
        metadata: Some(payload.metadata.clone()),
    }
}

pub async fn get_blob_ref(
    con: &mut MultiplexedConnection,
    key: &str,
    max_size: Option<usize>,
    metadata: Option<HashMap<String, Vec<u8>>>,
) -> Result<BlobRef> {
    let max_size = max_size.unwrap_or(100_000);
    if con.exists(&key).await? {
        let size: usize = con.strlen(key).await?;
        if size > max_size {
            Ok(BlobRef {
                path: key.to_string(),
                size,
                present: true,
                loaded: false,
                data: None,
                metadata,
            })
        } else {
            let data: Vec<u8> = con.get(key).await?;
            Ok(BlobRef {
                path: key.to_string(),
                size,
                present: true,
                loaded: true,
                data: Some(data),
                metadata,
            })
        }
    } else {
        Ok(BlobRef {
            path: key.to_string(),
            size: 0,
            present: false,
            loaded: false,
            data: None,
            metadata,
        })
    }
}
//
// -------- key helpers + blob helpers + status helpers -------
//

pub fn workflow_meta_key(workflow_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}")
}

pub fn workflow_args_key(workflow_id: &str, arg_idx: usize) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:args:{arg_idx}")
}

pub fn workflow_output_key(workflow_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:output")
}

fn workflow_status_blob_key(workflow_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:status")
}

pub fn workflow_activities_list_key(workflow_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:activities")
}

pub fn activity_base_key(workflow_id: &str, activity_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:activity:{activity_id}")
}

fn activity_args_key(workflow_id: &str, activity_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:activity:{activity_id}:args")
}

pub fn activity_input_key(workflow_id: &str, activity_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:activity:{activity_id}:input")
}

fn activity_output_key(workflow_id: &str, activity_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:activity:{activity_id}:output")
}

pub fn activity_runs_list_key(workflow_id: &str, activity_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:activity:{activity_id}:runs_list")
}

pub fn run_base_key(workflow_id: &str, activity_id: &str, run_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:activity:{activity_id}:run:{run_id}")
}

fn run_status_blob_key(workflow_id: &str, activity_id: &str, run_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:activity:{activity_id}:run:{run_id}:status")
}

pub fn run_output_blob_key(workflow_id: &str, activity_id: &str, run_id: &str) -> String {
    format!("{WORKFLOW_BASE_REDIS_KEY}:{workflow_id}:activity:{activity_id}:run:{run_id}:output")
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
    pub loaded: bool,
    pub data: Option<Vec<u8>>,
    pub metadata: Option<HashMap<String, Vec<u8>>>,
}

impl BlobRef {
    pub async fn to_payload(&self, con: &mut MultiplexedConnection) -> Result<Payload> {
        let metadata = self.metadata.clone().unwrap_or_default();
        if self.loaded {
            Ok(Payload {
                metadata,
                data: self.data.clone().unwrap_or_default(),
            })
        } else {
            let data = get_blob_raw::<Vec<u8>>(con, &self.path).await?;
            Ok(Payload {
                metadata,
                data: data.unwrap_or_default(),
            })
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use immortal_lib::common::Payload;

    #[test]
    fn test_workflow_history_creation() {
        let args = vec![Payload::new(&"arg1")];
        let wf = WorkflowHistory::new(
            "test_workflow".to_string(),
            "wf_id_1".to_string(),
            args.clone(),
            "default".to_string(),
            "worker_1".to_string(),
            Uuid::new_v4(),
            0,
        );

        assert_eq!(wf.workflow_type, "test_workflow");
        assert_eq!(wf.workflow_id, "wf_id_1");
        assert_eq!(wf.task_queue, "default".to_string());
        assert_eq!(wf.worker_id, Some("worker_1".to_string()));
        assert_eq!(wf.status, Status::Running);
        assert_eq!(wf.activities.len(), 0);
        assert!(wf.end_time.is_none());
    }

    #[test]
    fn test_activity_history_creation_and_run() {
        let mut activity = ActivityHistory::new(
            "wf_id_1".to_string(),
            "test_activity".to_string(),
            "act_id_1".to_string(),
            "default".to_string(),
            Some(Payload::new(&"input")),
            0,
            "".to_string(),
            None,
            None,
            None,
            None,
        );

        assert_eq!(activity.activity_id, "act_id_1");
        assert_eq!(activity.activity_type, "test_activity");
        assert_eq!(activity.runs.len(), 0);

        let run = ActivityRun::new(
            "wf_id_1".to_string(),
            "act_id_1".to_string(),
            "run_id_1".to_string(),
            None,
            0,
        );

        activity.add_run(run.clone());
        assert_eq!(activity.runs.len(), 1);

        // Test duplicate run addition
        activity.add_run(run);
        assert_eq!(activity.runs.len(), 1);

        let retrieved_run = activity.get_run("run_id_1");
        assert!(retrieved_run.is_some());
        assert_eq!(retrieved_run.unwrap().status, Status::Running);
    }

    #[test]
    fn test_activity_run_creation() {
        let run = ActivityRun::new(
            "wf_id_1".to_string(),
            "act_id_1".to_string(),
            "run_id_1".to_string(),
            None,
            0,
        );

        assert_eq!(run.workflow_id, "wf_id_1");
        assert_eq!(run.activity_id, "act_id_1");
        assert_eq!(run.run_id, "run_id_1");
        assert_eq!(run.status, Status::Running);
        assert!(run.end_time.is_none());
        assert!(run.output.is_none());
    }
}
