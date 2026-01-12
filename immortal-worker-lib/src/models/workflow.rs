use anyhow::{Error, anyhow};
use futures::future::{BoxFuture, FutureExt};
use immortal_lib::common::Payloads;
use immortal_lib::immortal::{
    RequestStartActivityOptionsV1, RequestStartActivityOptionsVersion, RetryPolicy,
};
use immortal_lib::immortal::{
    activity_result_v1::Status, activity_result_version, immortal_client::ImmortalClient,
    request_start_activity_options_version,
};
use serde::Deserialize;
use serde::{Serialize, de::DeserializeOwned};
use simd_json::OwnedValue;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::watch;
use tonic::transport::Channel;
use tracing::Instrument;
use tracing::info_span;
use tracing::instrument::Instrumented;

use crate::models::worker::Worker;

use super::activity::ActivityOptions;

pub struct Workflow {
    pub name: String,
    pub default_options: WorkflowOptions,
    pub fn_name: String,
    pub fn_args: Vec<OwnedValue>,
}
//
// #[derive(Clone)]
// pub struct ActivityCache {
//     pub input: Option<Payload>,
//     pub output: Option<Payload>,
// }
//
#[derive(Clone)]
pub struct WfContext {
    _namespace: String,
    task_queue: String,
    pub client: ImmortalClient<Channel>,
    pub args: Arc<Payloads>,
    pub id: String,
    pub epoch: u64,
    pub activity_seq: u32,
    pub connected_rx: watch::Receiver<bool>,
    // pub app_data: Option<AppData>,
    // chan: Sender<RustWfCmd>,
    // am_cancelled: watch::Receiver<bool>,
    // pub(crate) shared: Arc<RwLock<WfContextSharedData>>,

    // seq_nums: Arc<RwLock<WfCtxProtectedDat>>,
}

impl WfContext {
    async fn wait_until_connected(&mut self) {
        // Fast path
        if *self.connected_rx.borrow() {
            return;
        }

        // Wait for a change to true
        while self.connected_rx.changed().await.is_ok() {
            if *self.connected_rx.borrow() {
                return;
            }
        }
        // If sender is dropped, the worker is shutting down.
    }
    fn next_activity_seq(&mut self) -> u32 {
        let s = self.activity_seq;
        self.activity_seq = self.activity_seq.wrapping_add(1);
        s
    }
    pub async fn sleep(
        &mut self,
        options: ActivityOptions,
    ) -> anyhow::Result<()> {

        Ok(())
    }
    pub async fn activity<T: DeserializeOwned>(
        &mut self,
        options: ActivityOptions,
    ) -> anyhow::Result<T> {
        let seq = self.next_activity_seq();
        let idempotency_key = format!(
            "{}:{}:{}:{}",
            self.id,
            self.epoch,
            options.activity_type,
            seq,
            // for MVP you can hash the input bytes
            // blake3::hash(&options.input.data).to_hex()
        );
        let fingerprint = blake3::hash(&options.input.data).to_hex();
        // strip cache once it is done and remove some of the clones
        // if let Some(cache) = &self.cache {
        //     for item in cache {
        //         if item.input == Some(options.input.clone())
        //             && item.activity_type == options.activity_type
        //             && item.task_queue == options.task_queue
        //         {
        //             return Ok(item.output.clone().unwrap().to()?);
        //         }
        //     }
        // }

        let mut request = RequestStartActivityOptionsV1 {
            // activity_id: Uuid::new_v4().to_string(),
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
            idempotency_key,
            fingerprint: fingerprint.to_string(),
            ..Default::default()
        };
        request.set_cancellation_type(options.cancellation_type.into());

        let mut backoff = Duration::from_millis(200);
        let max_backoff = Duration::from_secs(5);

        loop {
            // Don’t even try if offline
            self.wait_until_connected().await;

            match self
                .client
                .start_activity(RequestStartActivityOptionsVersion {
                    version: Some(request_start_activity_options_version::Version::V1(
                        request.clone(),
                    )),
                })
                .await
            {
                Ok(activity_result) => match activity_result.into_inner().version {
                    Some(activity_result_version::Version::V1(x)) => match x.status {
                        Some(Status::Failed(x)) => {
                            return Err(anyhow!("{:#?}", x));
                        }
                        Some(Status::Timeout(x)) => {
                            return Err(anyhow!("{:#?}", x));
                        }
                        Some(Status::Cancelled(x)) => {
                            return Err(anyhow!("{:#?}", x));
                        }
                        Some(Status::Completed(y)) => {
                            return Ok(simd_json::from_slice(
                                &mut y.result.ok_or(anyhow!("No payload"))?.data,
                            )?);
                        }
                        None => return Err(anyhow!("Activity failed")),
                    },
                    None => return Err(anyhow!("Activity failed")),
                },
                Err(e) if Worker::is_retryable_rpc_error(&e) => {
                    // We treat this as "disconnected"
                    // let _ = self.connected_tx.send(false);

                    tokio::time::sleep(backoff).await;
                    backoff = std::cmp::min(backoff * 2, max_backoff);
                    continue;
                }
                Err(e) => return Err(anyhow!("Activity failed: {:?}", e)),
            }
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

type WfFunc = dyn Fn(WfContext) -> BoxFuture<'static, Result<WfExitValue<OwnedValue>, anyhow::Error>>
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
                                    WfExitValue::Normal(simd_json::serde::to_owned_value(o)?)
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
        connected_rx: tokio::sync::watch::Receiver<bool>,
        epoch: u64,
    ) -> Instrumented<Pin<Box<dyn Future<Output = Result<WfExitValue<OwnedValue>, Error>> + Send>>>
    {
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
            activity_seq: 0,
            connected_rx,
            epoch,
        })
        .instrument(span);
        handle
    }
}
pub type WorkflowResult<T> = Result<WfExitValue<T>, anyhow::Error>;
