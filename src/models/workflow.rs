// pub mod immortal {
//     tonic::include_proto!("immortal");
// }

use crate::common::Payloads;
use crate::immortal::{
    activity_result_v1::Status, activity_result_version, immortal_client::ImmortalClient,
    request_start_activity_options_version,
};
use crate::immortal::{
    RequestStartActivityOptionsV1, RequestStartActivityOptionsVersion, RetryPolicy,
};
use anyhow::{anyhow, Error};
use futures::future::{BoxFuture, FutureExt};
use serde::Deserialize;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tonic::transport::Channel;
use tracing::info_span;
use tracing::instrument::Instrumented;
use tracing::Instrument;
use uuid::Uuid;

use super::activity::ActivityOptions;

pub struct Workflow {
    pub name: String,
    pub default_options: WorkflowOptions,
    pub fn_name: String,
    pub fn_args: Vec<Value>,
}

#[derive(Clone)]
pub struct WfContext {
    _namespace: String,
    task_queue: String,
    pub client: ImmortalClient<Channel>,
    pub args: Arc<Payloads>,
    pub id: String,
    // pub app_data: Option<AppData>,
    // chan: Sender<RustWfCmd>,
    // am_cancelled: watch::Receiver<bool>,
    // pub(crate) shared: Arc<RwLock<WfContextSharedData>>,

    // seq_nums: Arc<RwLock<WfCtxProtectedDat>>,
}

impl WfContext {
    pub async fn activity<T: DeserializeOwned >(
        &mut self,
        options: ActivityOptions,
    ) -> anyhow::Result<T> {
        let mut request = RequestStartActivityOptionsV1 {
            activity_id: Uuid::new_v4().to_string(),
            activity_type: options.activity_type.to_string(),
            activity_input: Some(options.input),
            workflow_id: self.id.clone(),
            schedule_to_close_timeout: options
                .schedule_to_close_timeout
                .map(|x| x.try_into().unwrap()),
            schedule_to_start_timeout: options
                .schedule_to_start_timeout
                .map(|x| x.try_into().unwrap()),
            start_to_close_timeout: options
                .start_to_close_timeout
                .map(|x| x.try_into().unwrap()),
            heartbeat_timeout: options.heartbeat_timeout.map(|x| x.try_into().unwrap()),
            retry_policy: options.retry_policy.map(|x| x.try_into().unwrap()),
            task_queue: options.task_queue.unwrap_or(self.task_queue.clone()),
            ..Default::default()
        };
        request.set_cancellation_type(options.cancellation_type.into());
        match self
            .client
            .start_activity(RequestStartActivityOptionsVersion {
                version: Some(request_start_activity_options_version::Version::V1(request)),
            })
            .await
        {
            Ok(activity_result) => match activity_result.into_inner().version {
                Some(activity_result_version::Version::V1(x)) => match x.status {
                    Some(Status::Failed(x)) => Err(anyhow!("{:#?}", x)),
                    Some(Status::Cancelled(x)) => Err(anyhow!("{:#?}", x)),
                    Some(Status::Completed(y)) => {
                        match y.result {
                            Some(x) => Ok(x.to()?),
                            None => Err(anyhow!("Paylaod empty"))
                        }
                        // let result: ActExitValue<T> = serde_json::from_slice(&y.result.unwrap().data)?;
                        // match result {
                        // ActExitValue::Normal(x) => Ok(x),
                        // ActExitValue::WillCompleteAsync => {
                        //     Err(anyhow!("Activity was cancelled"))
                        // }
                        // }
                    }
                    None => Err(anyhow!("Activity failed")),
                },
                // Some(x) => Ok(serde_json::from_str(&x)?),
                None => Err(anyhow!("Activity failed")),
            },
            Err(e) => Err(anyhow!("Activity failed: {:?}", e)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum WorkflowIdReusePolicy {
    #[default]
    Unspecified = 0,
    /// Allow starting a workflow execution using the same workflow id.
    AllowDuplicate = 1,
    /// Allow starting a workflow execution using the same workflow id, only when the last
    /// execution's final state is one of \[terminated, cancelled, timed out, failed\].
    AllowDuplicateFailedOnly = 2,
    /// Do not permit re-use of the workflow id for this workflow. Future start workflow requests
    /// could potentially change the policy, allowing re-use of the workflow id.
    RejectDuplicate = 3,
    /// This option belongs in WorkflowIdConflictPolicy but is here for backwards compatibility.
    /// If specified, it acts like ALLOW_DUPLICATE, but also the WorkflowId*Conflict*Policy on
    /// the request is treated as WORKFLOW_ID_CONFLICT_POLICY_TERMINATE_EXISTING.
    /// If no running workflow, then the behavior is the same as ALLOW_DUPLICATE.
    TerminateIfRunning = 4,
}

#[derive(Debug, Clone, Default)]
pub struct WorkflowOptions {
    /// Set the policy for reusing the workflow id
    pub id_reuse_policy: WorkflowIdReusePolicy,

    /// Optionally set the execution timeout for the workflow
    /// <https://docs.temporal.io/workflows/#workflow-execution-timeout>
    pub execution_timeout: Option<Duration>,

    /// Optionally indicates the default run timeout for a workflow run
    pub run_timeout: Option<Duration>,

    /// Optionally indicates the default task timeout for a workflow run
    pub task_timeout: Option<Duration>,

    /// Optionally set a cron schedule for the workflow
    pub cron_schedule: Option<String>,

    /// Optionally associate extra search attributes with a workflow
    // pub search_attributes: Option<HashMap<String, Payload>>,

    /// Optionally set a retry policy for the workflow
    pub retry_policy: Option<RetryPolicy>,
}

type WfFunc = dyn Fn(WfContext) -> BoxFuture<'static, Result<WfExitValue<Value>, anyhow::Error>>
    + Send
    + Sync
    + 'static;

// #[derive(Clone)]
/// The user's async function / workflow code
pub struct WorkflowFunction {
    wf_func: Box<WfFunc>,
}

impl<F, Fut, O> From<F> for WorkflowFunction
where
    F: Fn(WfContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<WfExitValue<O>, anyhow::Error>> + Send + 'static,
    O: Serialize + DeserializeOwned,
    // A: Serialize + DeserializeOwned,
{
    fn from(wf_func: F) -> Self {
        Self::new(wf_func)
    }
}

/// Workflow functions may return these values when exiting
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WfExitValue<T: Serialize> {
    /// Continue the workflow as a new execution
    // #[from(ignore)]
    // ContinueAsNew(Box<ContinueAsNewWorkflowExecution>),
    /// Confirm the workflow was cancelled (can be automatic in a more advanced iteration)
    // #[from(ignore)]
    Cancelled,
    /// The run was evicted
    // #[from(ignore)]
    Evicted,
    /// Finish with a result
    Normal(T),
}

impl WorkflowFunction {
    /// Build a workflow function from a closure or function pointer which accepts a [WfContext]
    pub fn new<F, Fut, O>(f: F) -> Self
    where
        F: Fn(WfContext) -> Fut + Send + Sync + 'static,

        Fut: Future<Output = Result<WfExitValue<O>, anyhow::Error>> + Send + 'static,
        O: Serialize,
    {
        Self {
            wf_func: Box::new(move |ctx: WfContext| {
                (f)(ctx)
                    .map(|r| {
                        r.and_then(|r| {
                            Ok(match r {
                                // WfExitValue::ContinueAsNew(b) => WfExitValue::ContinueAsNew(b),
                                WfExitValue::Cancelled => WfExitValue::Cancelled,
                                WfExitValue::Evicted => WfExitValue::Evicted,
                                WfExitValue::Normal(o) => {
                                    WfExitValue::Normal(serde_json::to_value(o)?)
                                }
                            })
                        })
                    })
                    .boxed()
            }),
        }
    }

    pub fn start_workflow(
        &mut self,
        client: ImmortalClient<Channel>,
        args: Payloads,
        workflow_type: String,
        workflow_id: String,
        namespace: String,
        task_queue: String,
    ) -> Instrumented<Pin<Box<dyn Future<Output = Result<WfExitValue<Value>, Error>> + Send>>> {
        let span = info_span!(
            "RunWorkflow",
            "otel.name" = workflow_type,
            "otel.kind" = "server",
            "workflow_id" = workflow_id.clone(),
        );
        let handle = (self.wf_func)(WfContext {
            _namespace: namespace,
            task_queue,
            client,
            args: Arc::new(args),
            id: workflow_id,
        })
        .instrument(span);
        handle
    }
}
pub type WorkflowResult<T> = Result<WfExitValue<T>, anyhow::Error>;
