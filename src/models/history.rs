use anyhow::anyhow;
use bb8_redis::{
    bb8::{Pool, PooledConnection, RunError},
    RedisConnectionManager,
};
use const_format::formatcp;

use anyhow::Result;
use chrono::NaiveDateTime;
use itertools::Itertools;
use redis::{AsyncCommands, ErrorKind, FromRedisValue, RedisError, RedisWrite, ToRedisArgs};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct History(pub Vec<WorkflowHistory>, Pool<RedisConnectionManager>);
const BASE_REDIS_KEY: &str = "immortal::history";
const WORKFLOW_BASE_REDIS_KEY: &str = formatcp!("{}::workflow", BASE_REDIS_KEY);
impl History {
    pub fn new(pool: &Pool<RedisConnectionManager>) -> Self {
        Self(Vec::new(), pool.clone())
    }

    async fn get_con(
        &self,
    ) -> std::result::Result<PooledConnection<'_, RedisConnectionManager>, RunError<RedisError>>
    {
        self.1.get().await
    }

    pub async fn sync_workflow_index(&self) -> Result<()> {
        let mut con = self.get_con().await?;
        let currently_logged_workflows: Vec<String> = con
            .keys(format!("{WORKFLOW_BASE_REDIS_KEY}::*").as_str())
            .await?;
        let mut offset = 0;
        let limit = 100;

        loop {
            let workflows_in_index: Vec<String> = con
                .lrange(
                    format!("{WORKFLOW_BASE_REDIS_KEY}::workflow_index"),
                    (offset).try_into().unwrap(),
                    (offset + limit).try_into().unwrap(),
                )
                .await?;
            if workflows_in_index.len() == 0 {
                break;
            }
            for workflow_index in &workflows_in_index {
                if !currently_logged_workflows.contains(&format!("{WORKFLOW_BASE_REDIS_KEY}::{workflow_index}")) {
                    let _: () = con
                        .ltrim(
                            format!("{WORKFLOW_BASE_REDIS_KEY}::workflow_index"),
                            0,
                            offset,
                        )
                        .await?;
                }

                offset += 1;
            }
        }

        Ok(())
    }

    pub async fn add_workflow(&self, workflow: WorkflowHistory) -> Result<()> {
        let mut con = self.get_con().await?;
        let key = format!("{WORKFLOW_BASE_REDIS_KEY}::{}", workflow.workflow_id);
        if con.exists(&key).await? {
            return Err(anyhow!("Workflow already exists"));
        }
        let _: () = con
            .lpush(
                format!("{WORKFLOW_BASE_REDIS_KEY}::workflow_index"),
                workflow.workflow_id.clone(),
            )
            .await?;
        let _: () = con
            .set_ex(&key, WorkflowHistoryVersion::V1(workflow), 259200)
            .await?;
        Ok(())
    }
    pub async fn get_workflow(&self, workflow_id: &str) -> Result<Option<WorkflowHistory>> {
        let mut con = self.get_con().await?;
        let key = format!("{WORKFLOW_BASE_REDIS_KEY}::{}", workflow_id);
        let wv: Option<WorkflowHistoryVersion> = con.get(key).await?;
        Ok(wv.map(|wv| match wv {
            WorkflowHistoryVersion::V1(w) => w,
        }))
    }
    pub async fn get_workflows(
        &self,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<Vec<WorkflowHistoryVersion>> {
        let limit = limit.unwrap_or(10) as isize;
        let offset = offset.unwrap_or(0) as isize;
        let mut con = self.get_con().await?;
        //let _keys: Vec<String> = con
        //    .keys(format!("{WORKFLOW_BASE_REDIS_KEY}::*").as_str())
        //    .await?;
        // not sure why I moved to a workflow_index
        // second thought, probably to allow for pagination. I need to create a sync mechanism
        let keys2: Vec<String> = con
            .lrange(
                format!("{WORKFLOW_BASE_REDIS_KEY}::workflow_index"),
                (-1 - (offset + limit)).try_into().unwrap(),
                (-1 - offset).try_into().unwrap(),
            )
            .await?;
        let workflows: Vec<Option<String>> = con
            .mget(
                keys2
                    .iter()
                    .map(|f| format!("{WORKFLOW_BASE_REDIS_KEY}::{f}"))
                    .collect_vec(),
            )
            .await?;
        let workflows = workflows
            .into_iter()
            .filter(|f| f.is_some())
            .map(|x| serde_json::from_str(&x.unwrap()).unwrap())
            .collect();

        Ok(workflows)
    }

    pub async fn update_workflow(
        &self,
        workflow_id: &str,
        workflow: WorkflowHistory,
    ) -> Result<()> {
        let mut con = self.get_con().await?;
        let key = format!("{WORKFLOW_BASE_REDIS_KEY}::{}", workflow_id);
        let _: () = con
            .set_ex(key, WorkflowHistoryVersion::V1(workflow), 259200)
            .await?;
        Ok(())
    }

    // pub fn get_workflow_mut(&mut self, workflow_id: &str) -> Option<&mut WorkflowHistory> {
    //     self.0.iter_mut().find(|w| w.workflow_id == workflow_id)
    // }
    pub async fn get_activity(
        &self,
        workflow_id: &str,
        activity_id: &str,
    ) -> Result<Option<ActivityHistory>> {
        let workflow = self
            .get_workflow(workflow_id)
            .await?
            .ok_or(anyhow!("Workflow does not exist"))?;
        Ok(workflow
            .activities
            .iter()
            .find(|a| a.activity_id == activity_id)
            .cloned())
    }
    pub async fn add_activity(&self, workflow_id: &str, activity: ActivityHistory) -> Result<()> {
        let mut con = self.get_con().await?;
        let key = format!("{WORKFLOW_BASE_REDIS_KEY}::{}", workflow_id);
        let mut workflow = self
            .get_workflow(workflow_id)
            .await?
            .ok_or(anyhow!("Workflow does not exist"))?;
        workflow.activities.push(activity);
        let _: () = con
            .set_ex(key, WorkflowHistoryVersion::V1(workflow), 259200)
            .await?;
        Ok(())
    }
    pub async fn update_activity(
        &self,
        workflow_id: &str,
        activity: ActivityHistory,
    ) -> Result<()> {
        let mut con = self.get_con().await?;
        let key = format!("{WORKFLOW_BASE_REDIS_KEY}::{}", workflow_id);
        let mut workflow = self
            .get_workflow(workflow_id)
            .await?
            .ok_or(anyhow!("Workflow does not exist"))?;
        let previous_activity = workflow
            .activities
            .iter_mut()
            .find(|a| a.activity_id == activity.activity_id)
            .ok_or(anyhow!("Activity does not exist"))?;
        *previous_activity = activity;

        let _: () = con
            .set_ex(key, WorkflowHistoryVersion::V1(workflow), 259200)
            .await?;
        Ok(())
    }
    // pub fn get_activity_mut(&mut self, activity_id: &str) -> Option<&mut ActivityHistory> {
    //     self.0
    //         .iter_mut()
    //         .flat_map(|w| w.runs.iter_mut())
    //         .flat_map(|r| r.activities.iter_mut())
    //         .find(|a| a.activity_id == activity_id)
    // }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "version", content = "spec")]
pub enum WorkflowHistoryVersion {
    V1(WorkflowHistory),
}
impl ToRedisArgs for WorkflowHistoryVersion {
    fn to_redis_args(&self) -> Vec<Vec<u8>> {
        let json = serde_json::to_string(self).unwrap();
        vec![json.into_bytes()]
    }
    fn write_redis_args<W>(&self, out: &mut W)
    where
        W: ?Sized + RedisWrite,
    {
        // Serialize the struct to a JSON string
        let serialized = serde_json::to_string(self).expect("Failed to serialize to JSON");
        // Write the JSON string to the Redis argument
        out.write_arg(serialized.as_bytes());
    }
}

impl FromRedisValue for WorkflowHistoryVersion {
    fn from_redis_value(value: &redis::Value) -> Result<Self, RedisError> {
        match value {
            redis::Value::Nil => {
                return Err((ErrorKind::TypeError, "Value is nil").into());
            }
            redis::Value::BulkString(x) => {
                match serde_json::from_slice(x) {
                    Ok(data) => {
                        return Ok(data);
                    }
                    Err(_e) => {
                        return Err((ErrorKind::TypeError, "Couldn't convert JSON").into());
                    }
                };
            }
            _ => {
                return Err((ErrorKind::TypeError, "Value is not a string").into());
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowHistory {
    pub args: Vec<Value>,
    pub workflow_id: String,
    pub workflow_type: String,
    pub status: Status,
    pub activities: Vec<ActivityHistory>,
    pub start_time: NaiveDateTime,
    pub end_time: Option<NaiveDateTime>,
    // pub status: Status,
}

impl WorkflowHistory {
    pub fn new(workflow_type: String, workflow_id: String, args: Vec<Value>) -> Self {
        Self {
            args,
            workflow_type,
            workflow_id,
            status: Status::Running,
            activities: Vec::new(),
            start_time: chrono::Utc::now().naive_utc(),
            end_time: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "spec")]
pub enum Status {
    Running,
    Completed(Value),
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityHistory {
    pub activity_id: String,
    pub activity_type: String,
    // pub status: Status,
    // pub result: Option<Value>,
    pub runs: Vec<ActivityRun>,
}

impl ActivityHistory {
    pub fn new(activity_type: String, activity_id: String) -> Self {
        Self {
            activity_id,
            activity_type,
            // status: Status::Running,
            runs: Vec::new(),
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
    pub start_time: NaiveDateTime,
    pub end_time: Option<NaiveDateTime>,
}

impl ActivityRun {
    pub fn new(run_id: String) -> Self {
        Self {
            run_id,
            status: Status::Running,
            start_time: chrono::Utc::now().naive_utc(),
            end_time: None,
        }
    }
}
