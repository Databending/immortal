use crate::history::{
    activity_base_key, activity_input_key, activity_runs_list_key, get_blob_ref, run_base_key,
    run_output_blob_key, workflow_activities_list_key, workflow_meta_key, workflow_output_key,
    ActivityHistory, ActivityRun, Status, WorkflowHistory,
};
use crate::history::{workflow_args_key, BlobRef};
use anyhow::Result;
use chrono::{DateTime, Utc};
use const_format::formatcp;
use redis::aio::MultiplexedConnection;
use redis::AsyncCommands;
use redis::Script;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, str::FromStr};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "version", content = "spec")]
pub enum WorkflowHistoryMetadataVersion {
    V1(WorkflowHistoryMetadata),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowHistoryMetadata {
    pub workflow_id: String,
    // pub args_metadata: Vec<HashMap<String, Vec<u8>>>,
    pub args: Vec<BlobRef>,
    // pub output_metadata: Option<HashMap<String, Vec<u8>>>,
    pub output: Option<BlobRef>,
    pub workflow_type: String,
    pub status: Status,
    pub activities: Vec<ActivityHistoryMetadata>,
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
    pub task_queue: Option<String>,
    pub worker_id: Option<String>,
}

const TTL: i64 = 259_200;
const BASE_REDIS_KEY: &str = "immortal:history";
const WORKFLOW_BASE_REDIS_KEY: &str = formatcp!("{}:workflow", BASE_REDIS_KEY);

impl WorkflowHistoryMetadata {
    pub async fn get_opt(
        con: &mut MultiplexedConnection,
        workflow_id: &str,
    ) -> Result<Option<Self>> {
        let wf_meta = workflow_meta_key(workflow_id);
        let meta_val: redis::Value = redis::cmd("HGETALL")
            .arg(&wf_meta)
            .query_async(&mut *con)
            .await?;
        let meta: std::collections::HashMap<String, String> = redis::from_redis_value(&meta_val)?;

        let workflow_type = meta.get("workflow_type").cloned().unwrap_or_default();
        let status = Status::from_str(
            &meta
                .get("status")
                .cloned()
                .unwrap_or_else(|| "Running".to_string()),
        )?;
        let start_time: DateTime<Utc> = meta
            .get("start_time")
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(Utc::now);
        let end_time: Option<DateTime<Utc>> =
            meta.get("end_time")
                .and_then(|s| if s.is_empty() { None } else { s.parse().ok() });
        let task_queue = meta.get("task_queue").cloned();
        let worker_id = meta.get("worker_id").cloned();
        // let mut args_metadata = vec![];
        let mut args = vec![];

        if let Some(mut meta_args_metadata) = meta.get("args_metadata").cloned() {
            let args_metadata: Vec<HashMap<String, Vec<u8>>> =
                unsafe { simd_json::from_str(meta_args_metadata.as_mut_str())? };
            for (i, arg_metadata) in args_metadata.iter().enumerate() {
                args.push(
                    get_blob_ref(
                        con,
                        &workflow_args_key(workflow_id, i),
                        None,
                        Some(arg_metadata.clone()),
                    )
                    .await?,
                );
            }
        }

        let mut output = None;
        if let Some(mut meta_output_metadata) = meta.get("output_metadata").cloned() {
            let output_metadata =
                unsafe { simd_json::from_str(meta_output_metadata.as_mut_str())? };
            output = Some(
                get_blob_ref(
                    con,
                    &workflow_output_key(workflow_id),
                    None,
                    Some(output_metadata),
                )
                .await?,
            );
        }
        // activities
        let act_ids: Vec<String> = con
            .lrange(workflow_activities_list_key(workflow_id), 0, -1)
            .await?;
        let mut activities = Vec::with_capacity(act_ids.len());

        for (idx, act_id) in act_ids.into_iter().enumerate() {
            if let Some(mut act) =
                ActivityHistoryMetadata::get_opt(con, workflow_id, &act_id).await?
            {
                act.index = idx;
                activities.push(act);
            }
        }

        Ok(Some(Self {
            args,
            output,
            workflow_id: workflow_id.to_string(),
            workflow_type,
            status,
            activities,
            start_time,
            // args_metadata,
            // output_metadata,
            end_time,
            task_queue,
            worker_id,
        }))
    }

    pub async fn store(&self, con: &mut MultiplexedConnection, store_children: bool) -> Result<()> {
        let wf_id = self.workflow_id.clone();
        let wf_meta = workflow_meta_key(&wf_id);
        //
        // if !con.exists(&wf_meta).await? {
        //     let _: () = con
        //         .lpush(format!("{WORKFLOW_BASE_REDIS_KEY}:workflow_index"), &wf_id)
        //         .await?;
        // }

        // Add to workflow index (for pagination)

        // Store workflow metadata hash (including status tag)
        // let args_metadata = self
        //     .args
        //     .iter()
        //     .map(|f| f.metadata.clone())
        //     .collect::<Vec<_>>();

        let mut query = bb8_redis::redis::pipe();
        query
            .cmd("HSET")
            .arg(&wf_meta)
            .arg("version")
            .arg("V1")
            .arg("workflow_type")
            .arg(&self.workflow_type)
            .arg("status")
            .arg(self.status.as_ref())
            .arg("start_time")
            .arg(self.start_time.to_rfc3339())
            .arg("end_time")
            .arg(
                self.end_time
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(String::new),
            )
            .arg("task_queue")
            .arg(self.task_queue.clone().unwrap_or_default())
            .arg("worker_id")
            .arg(self.worker_id.clone().unwrap_or_default())
            .arg("args_metadata")
            .arg(simd_json::to_string(
                &self
                    .args
                    .iter()
                    .map(|f| f.metadata.clone().unwrap())
                    .collect::<Vec<_>>(),
            )?);

        if let Some(output) = &self.output {
            query
                .arg("output_metadata")
                .arg(simd_json::to_string(&output.metadata)?);
        }

        let _: () = query.ignore().query_async(&mut *con).await?;

        if store_children {
            // Store activities (IDs list + each activity)
            let act_list_key = workflow_activities_list_key(&wf_id);

            for activity in &self.activities {
                let act_id = &activity.activity_id;

                // Append activity_id to ordered list
                let _: () = con.rpush(&act_list_key, act_id).await?;

                // Store activity itself
                activity.store(con, &wf_id, store_children).await?;
            }
        }

        // Optional TTL on top-level workflow metadata key
        let _: () = con.expire(&wf_meta, TTL).await?;

        println!("WRITING TO REDIS (store workflow metadata)");
        Ok(())
    }

    async fn get_all_workflow_ids(
        con: &mut redis::aio::MultiplexedConnection,
        limit: Option<usize>,
        offset: Option<usize>,
        task_queues: Option<Vec<String>>,
        worker_ids: Option<Vec<String>>,
        status: Option<Status>,
    ) -> redis::RedisResult<Vec<String>> {
        let limit = limit.unwrap_or(10) as isize;
        let offset = offset.unwrap_or(0) as isize;

        let index_key = format!("{WORKFLOW_BASE_REDIS_KEY}:workflow_index");
        let prefix = format!("{WORKFLOW_BASE_REDIS_KEY}:");

        let status_str = match status {
            Some(Status::Failed) => "Failed",
            Some(Status::Running) => "Running",
            Some(Status::Completed) => "Completed",
            None => "",
        };

        let task_queues = task_queues.unwrap_or_default();
        let worker_ids = worker_ids.unwrap_or_default();

        let lua = Script::new(include_str!("lua/get_all_workflows.lua"));
        let mut lua_key = lua.key(index_key);

        let mut script = lua_key
            .key(prefix)
            .arg(offset)
            .arg(limit)
            .arg(status_str)
            .arg(task_queues.len() as i64);

        for tq in &task_queues {
            script = script.arg(tq);
        }

        script = script.arg(worker_ids.len() as i64);
        for wid in &worker_ids {
            script = script.arg(wid);
        }

        script.invoke_async(con).await
    }

    pub async fn get_all(
        con: &mut MultiplexedConnection,
        limit: Option<usize>,
        offset: Option<usize>,
        task_queues: Option<Vec<String>>,
        worker_ids: Option<Vec<String>>,
        status: Option<Status>,
    ) -> Result<Vec<Self>> {
        let ids: Vec<String> =
            Self::get_all_workflow_ids(con, limit, offset, task_queues, worker_ids, status).await?;
        //
        // ids.sort();
        // ids.dedup();

        if ids.is_empty() {
            return Ok(vec![]);
        }

        let mut workflows = vec![];
        for wf_id in ids {
            if let Some(wf) = Self::get_opt(con, &wf_id).await? {
                workflows.push(wf);
            }
        }

        Ok(workflows)
    }
}

impl From<&WorkflowHistory> for WorkflowHistoryMetadata {
    fn from(wf: &WorkflowHistory) -> Self {
        let mut args_metadata = vec![];
        let mut args = vec![];
        {
            let mut index = 0;
            for arg in &wf.args {
                args.push(BlobRef {
                    path: workflow_args_key(&wf.workflow_id, index),
                    size: arg.data.len(),
                    present: true,
                    loaded: false,
                    data: None,
                    metadata: Some(arg.metadata.clone()),
                });
                args_metadata.push(arg.metadata.clone());
                index += 1;
            }
        }
        let mut output = None;

        if let Some(wf_output) = &wf.output {
            output = Some(BlobRef {
                path: workflow_output_key(&wf.workflow_id),
                size: wf_output.data.len(),
                present: true,
                loaded: false,
                data: None,
                metadata: Some(wf_output.metadata.clone()),
            });
        }

        Self {
            args,
            output,
            worker_id: wf.worker_id.clone(),
            workflow_type: wf.workflow_type.clone(),
            // args_metadata,
            // output_metadata,
            status: wf.status.clone(),
            workflow_id: wf.workflow_id.clone(),
            start_time: wf.start_time,
            end_time: wf.end_time,
            task_queue: wf.task_queue.clone(),
            activities: wf.activities.iter().map(|f| f.into()).collect(),
        }
    }
}

impl Into<WorkflowHistoryMetadata> for WorkflowHistory {
    fn into(self) -> WorkflowHistoryMetadata {
        let mut args = vec![];
        {
            let mut index = 0;
            for arg in &self.args {
                args.push(BlobRef {
                    path: workflow_args_key(&self.workflow_id, index),
                    size: arg.data.len(),
                    present: false,
                    loaded: false,
                    data: None,
                    metadata: Some(arg.metadata.clone()),
                });
                index += 1;
            }
        }
        let mut output = None;

        if let Some(wf_output) = &self.output {
            output = Some(BlobRef {
                path: workflow_output_key(&self.workflow_id),
                size: wf_output.data.len(),
                present: true,
                loaded: false,
                data: None,
                metadata: Some(wf_output.metadata.clone()),
            });
        }
        WorkflowHistoryMetadata {
            args,
            output,
            worker_id: self.worker_id,
            workflow_type: self.workflow_type,
            // args_metadata: self.args.into_iter().map(|f| f.metadata).collect(),
            // output_metadata: self.output.map(|f| f.metadata),
            status: self.status,
            workflow_id: self.workflow_id,
            start_time: self.start_time,
            end_time: self.end_time,
            task_queue: self.task_queue,
            activities: self.activities.into_iter().map(|f| f.into()).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityHistoryMetadata {
    pub activity_id: String,
    pub activity_type: String,
    pub task_queue: Option<String>,
    pub input: Option<BlobRef>,
    pub runs: Vec<ActivityRunHistoryMetadata>,
    pub index: usize,

    // NEED THIS FOR INTO
    pub workflow_id: String,
}

impl ActivityHistoryMetadata {
    pub async fn get_opt(
        con: &mut MultiplexedConnection,
        workflow_id: &str,
        activity_id: &str,
    ) -> Result<Option<Self>> {
        let base = activity_base_key(workflow_id, activity_id);

        if !con.exists(&base).await? {
            return Ok(None);
        }

        let meta_val: redis::Value = redis::cmd("HGETALL").arg(&base).query_async(con).await?;
        let meta: std::collections::HashMap<String, String> = redis::from_redis_value(&meta_val)?;

        let activity_type = meta.get("activity_type").cloned().unwrap_or_default();
        let task_queue = meta.get("task_queue").cloned();

        // load runs list
        let run_ids: Vec<String> = con
            .lrange(activity_runs_list_key(workflow_id, activity_id), 0, -1)
            .await?;
        let mut runs = Vec::with_capacity(run_ids.len());
        for run_id in run_ids {
            if let Some(run) =
                ActivityRunHistoryMetadata::get_opt(con, workflow_id, activity_id, &run_id).await?
            {
                runs.push(run);
            }
        }

        let mut input = None;
        if let Some(mut meta_output_metadata) = meta.get("input_metadata").cloned() {
            let input_metadata = unsafe { simd_json::from_str(meta_output_metadata.as_mut_str())? };
            input = Some(
                get_blob_ref(
                    con,
                    &activity_input_key(workflow_id, activity_id),
                    None,
                    Some(input_metadata),
                )
                .await?,
            );
        }

        Ok(Some(Self {
            workflow_id: workflow_id.to_string(),
            activity_id: activity_id.to_string(),
            activity_type,
            task_queue,
            runs,
            input,
            index: 0, // caller fills actual index based on activities list
        }))
    }
    pub async fn store(
        &self,
        con: &mut MultiplexedConnection,
        workflow_id: &str,
        store_children: bool,
    ) -> Result<()> {
        let base = activity_base_key(workflow_id, &self.activity_id);

        let mut query = bb8_redis::redis::pipe();
        // activity metadata
        query
            .cmd("HSET")
            .arg(&base)
            .arg("activity_id")
            .arg(&self.activity_id)
            .arg("activity_type")
            .arg(&self.activity_type)
            .arg("task_queue")
            .arg(self.task_queue.clone().unwrap_or_default());
        if let Some(input) = &self.input {
            query
                .arg("input_metadata")
                .arg(simd_json::to_string(&input.metadata)?);
        }

        let _: () = query.ignore().query_async(&mut *con).await?;

        if store_children {
            // runs: store list of run_ids + each run separately
            let runs_list_key = activity_runs_list_key(workflow_id, &self.activity_id);

            // overwrite runs list completely for simplicity: delete + rebuild
            let _: () = con.del(&runs_list_key).await?;

            for run in &self.runs {
                // append id to list
                let _: () = con.rpush(&runs_list_key, &run.run_id).await?;
                let _: () = con.expire(&runs_list_key, TTL).await?;
                // store run separately
                run.store_run(con, workflow_id, &self.activity_id).await?;
            }
        }

        let _: () = con.expire(&base, TTL).await?;

        Ok(())
    }
}

impl Into<ActivityHistoryMetadata> for ActivityHistory {
    fn into(self) -> ActivityHistoryMetadata {
        ActivityHistoryMetadata {
            input: self.input.map(|f| BlobRef {
                path: activity_input_key(&self.workflow_id, &self.activity_id),
                size: f.data.len(),
                present: true,
                loaded: false,
                data: None,
                metadata: Some(f.metadata.clone()),
            }),
            activity_id: self.activity_id,
            workflow_id: self.workflow_id,

            activity_type: self.activity_type,
            task_queue: self.task_queue,
            index: self.index,
            runs: self.runs.into_iter().map(|f| f.into()).collect(),
        }
    }
}

impl From<&ActivityHistory> for ActivityHistoryMetadata {
    fn from(activity_history: &ActivityHistory) -> Self {
        let runs = activity_history.runs.iter().map(|f| f.into()).collect();

        let mut input = None;

        if let Some(f) = &activity_history.input {
            input = Some(BlobRef {
                path: activity_input_key(
                    &activity_history.workflow_id,
                    &activity_history.activity_id,
                ),
                size: f.data.len(),
                present: true,
                loaded: false,
                data: None,
                metadata: Some(f.metadata.clone()),
            })
        }

        Self {
            input,
            activity_type: activity_history.activity_type.clone(),
            activity_id: activity_history.activity_id.clone(),
            workflow_id: activity_history.workflow_id.clone(),
            task_queue: activity_history.task_queue.clone(),
            index: activity_history.index,
            runs,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityRunHistoryMetadata {
    pub run_id: String,
    pub status: Status,
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
    pub output: Option<BlobRef>,
}

impl ActivityRunHistoryMetadata {
    pub async fn get_opt(
        con: &mut MultiplexedConnection,
        workflow_id: &str,
        activity_id: &str,
        run_id: &str,
    ) -> Result<Option<Self>> {
        let base = run_base_key(workflow_id, activity_id, run_id);

        if !con.exists(&base).await? {
            return Ok(None);
        }

        let meta_val: redis::Value = redis::cmd("HGETALL").arg(&base).query_async(con).await?;
        let meta: std::collections::HashMap<String, String> = redis::from_redis_value(&meta_val)?;

        let status = Status::from_str(
            &meta
                .get("status")
                .cloned()
                .unwrap_or_else(|| "Running".to_string()),
        )?;

        let start_time: DateTime<Utc> = meta
            .get("start_time")
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(Utc::now);
        let end_time: Option<DateTime<Utc>> =
            meta.get("end_time")
                .and_then(|s| if s.is_empty() { None } else { s.parse().ok() });

        let mut output = None;
        if let Some(mut meta_output_metadata) = meta.get("output_metadata").cloned() {
            let output_metadata =
                unsafe { simd_json::from_str(meta_output_metadata.as_mut_str())? };
            output = Some(
                get_blob_ref(
                    con,
                    &run_output_blob_key(workflow_id, activity_id, run_id),
                    None,
                    Some(output_metadata),
                )
                .await?,
            );
        }
        Ok(Some(Self {
            run_id: run_id.to_string(),
            status,
            start_time,
            end_time,
            output,
        }))
    }
    pub async fn store_run(
        &self,
        con: &mut MultiplexedConnection,
        workflow_id: &str,
        activity_id: &str,
    ) -> Result<()> {
        let base = run_base_key(workflow_id, activity_id, &self.run_id);

        let mut query = bb8_redis::redis::pipe();
        // activity metadata
        query
            .cmd("HSET")
            .arg(&base)
            .arg("run_id")
            .arg(&self.run_id)
            .arg("status")
            .arg(self.status.as_ref())
            .arg("start_time")
            .arg(self.start_time.to_rfc3339())
            .arg("end_time")
            .arg(
                self.end_time
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(String::new),
            );
        if let Some(output) = &self.output {
            query
                .arg("output_metadata")
                .arg(simd_json::to_string(&output.metadata)?);
        }

        let _: () = query.ignore().query_async(&mut *con).await?;

        let _: () = con.expire(&base, TTL).await?;
        Ok(())
    }
}

impl Into<ActivityRunHistoryMetadata> for ActivityRun {
    fn into(self) -> ActivityRunHistoryMetadata {
        let mut output = None;

        if let Some(wf_output) = &self.output {
            output = Some(BlobRef {
                path: run_output_blob_key(&self.workflow_id, &self.activity_id, &self.run_id),
                size: wf_output.data.len(),
                present: true,
                loaded: false,
                data: None,
                metadata: Some(wf_output.metadata.clone()),
            });
        }
        ActivityRunHistoryMetadata {
            run_id: self.run_id.clone(),
            status: self.status.clone(),
            start_time: self.start_time,
            end_time: self.end_time,
            output,
        }
    }
}

impl From<&ActivityRun> for ActivityRunHistoryMetadata {
    fn from(activity_run: &ActivityRun) -> Self {
        let mut output = None;

        if let Some(f) = &activity_run.output {
            output = Some(BlobRef {
                path: activity_input_key(&activity_run.workflow_id, &activity_run.activity_id),
                size: f.data.len(),
                present: true,
                loaded: false,
                data: None,
                metadata: Some(f.metadata.clone()),
            })
        }
        Self {
            run_id: activity_run.run_id.clone(),
            status: activity_run.status.clone(),
            start_time: activity_run.start_time,
            end_time: activity_run.end_time,
            output,
        }
    }
}
