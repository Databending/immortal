use crate::common::Payload;
use crate::immortal;
use futures::future::BoxFuture;
use futures::future::FutureExt;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::any::Any;
use std::fmt::Debug;
use std::pin::Pin;
use std::time::SystemTime;
use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use tracing::info_span;
use tracing::Instrument;

use super::activity::AppData;

#[derive(Default, Debug)]
pub struct CallOptions {
    /// Identifier to use for tracking the activity in Workflow history.
    /// The `activityId` can be accessed by the activity function.
    /// Does not need to be unique.
    ///
    /// If `None` use the context's sequence number
    pub call_id: Option<String>,
    /// Type of activity to schedule
    pub call_type: String,
    /// Input to the activity
    pub input: Payload,
    /// Task queue to schedule the activity in
    ///
    /// If `None`, use the same task queue as the parent workflow.
    pub task_queue: Option<String>,
    /// Time that the Activity Task can stay in the Task Queue before it is picked up by a Worker.
    /// Do not specify this timeout unless using host specific Task Queues for Activity Tasks are
    /// being used for routing.
    /// `schedule_to_start_timeout` is always non-retryable.
    /// Retrying after this timeout doesn't make sense as it would just put the Activity Task back
    /// into the same Task Queue.
    pub schedule_to_start_timeout: Option<Duration>,
    /// Maximum time of a single Activity execution attempt.
    /// Note that the Temporal Server doesn't detect Worker process failures directly.
    /// It relies on this timeout to detect that an Activity that didn't complete on time.
    /// So this timeout should be as short as the longest possible execution of the Activity body.
    /// Potentially long running Activities must specify `heartbeat_timeout` and heartbeat from the
    /// activity periodically for timely failure detection.
    /// Either this option or `schedule_to_close_timeout` is required.
    pub start_to_close_timeout: Option<Duration>,
    /// Total time that a workflow is willing to wait for Activity to complete.
    /// `schedule_to_close_timeout` limits the total time of an Activity's execution including
    /// retries (use `start_to_close_timeout` to limit the time of a single attempt).
    /// Either this option or `start_to_close_timeout` is required.
    pub schedule_to_close_timeout: Option<Duration>,
    /// Heartbeat interval. Activity must heartbeat before this interval passes after a last
    /// heartbeat or activity start.
    // pub heartbeat_timeout: Option<Duration>,
    /// Determines what the SDK does when the Activity is cancelled.
    pub cancellation_type: immortal::ActivityCancellationType,
    /// Activity retry policy
    pub retry_policy: Option<immortal::RetryPolicy>,
}

// #[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
// pub enum ActivityCancellationType {
//     // Initiate a cancellation request and immediately report cancellation to the workflow.
//     #[default]
//     TryCancel,
//     // Wait for activity cancellation completion. Note that activity must heartbeat to receive a
//     // cancellation notification. This can block the cancellation for a long time if activity
//     // doesn't heartbeat or chooses to ignore the cancellation request.
//     WaitCancellationCompleted,
//     // Do not request cancellation of the activity and immediately report cancellation to the
//     // workflow
//     Abandon,
// }

#[derive(Debug, Deserialize, Serialize, Clone)]
/// Activity functions may return these values when exiting
pub enum CallExitValue<T> {
    /// Completion requires an asynchronous callback
    // WillCompleteAsync,
    /// Finish with a result
    Normal(T),
}

impl<T: Serialize> From<T> for CallExitValue<T> {
    fn from(t: T) -> Self {
        Self::Normal(t)
    }
}

type BoxCallFn = Arc<
    dyn Fn(CallContext, Payload) -> BoxFuture<'static, Result<CallExitValue<Value>, CallError>>
        + Send
        + Sync,
>;

/// Container for user-defined activity functions
#[derive(Clone)]
pub struct CallFunction {
    pub call_func: BoxCallFn,
}

impl CallFunction {
    /// Create a new ActivityFunction from a boxed function

    pub fn start_call(
        &mut self,
        payload: Payload,
        app_data: Arc<AppData>,
        call_type: String,
        call_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<CallExitValue<Value>, CallError>> + Send>> {
        let span = info_span!(
            "RunActivity",
            "otel.kind" = "server",
            "otel.name" = call_type,
            "otel.kind" = "server",
            "call_id" = call_id,
        );
        let handle = (self.call_func)(
            CallContext {
                app_data,
                cancellation_token: CancellationToken::new(),
                input: vec![],
                heartbeat_details: vec![],
                header_fields: HashMap::new(),
                info: CallInfo::default(),
            },
            payload,
        )
        .instrument(span);
        Box::pin(handle)
    }
}

/// Returned as errors from activity functions
#[derive(Debug)]
pub enum CallError {
    /// This error can be returned from activities to allow the explicit configuration of certain
    /// error properties. It's also the default error type that arbitrary errors will be converted
    /// into.
    Retryable {
        /// The underlying error
        source: anyhow::Error,
        /// If specified, the next retry (if there is one) will occur after this delay
        explicit_delay: Option<Duration>,
    },
    /// Return this error to indicate your activity is cancelling
    Cancelled {
        /// Some data to save as the cancellation reason
        details: Option<Value>,
    },
    /// Return this error to indicate that your activity non-retryable
    /// this is a transparent wrapper around anyhow Error so essentially any type of error
    /// could be used here.
    NonRetryable(anyhow::Error),
}

impl<E> From<E> for CallError
where
    E: Into<anyhow::Error>,
{
    fn from(source: E) -> Self {
        Self::Retryable {
            source: source.into(),
            explicit_delay: None,
        }
    }
}

impl CallError {
    /// Construct a cancelled error without details
    pub fn cancelled() -> Self {
        Self::Cancelled { details: None }
    }
}

/// Closures / functions which can be turned into activity functions implement this trait
pub trait IntoCallFunc<Args, Res, Out> {
    /// Consume the closure or fn pointer and turned it into a boxed activity function
    fn into_call_fn(self) -> BoxCallFn;
}

impl<A, Rf, R, O, F> IntoCallFunc<A, Rf, O> for F
where
    F: (Fn(CallContext, A) -> Rf) + Sync + Send + 'static,
    A: Serialize + DeserializeOwned,
    Rf: Future<Output = Result<R, CallError>> + Send + 'static,
    R: Into<CallExitValue<O>>,
    O: Serialize,
{
    fn into_call_fn(self) -> BoxCallFn {
        let wrapper = move |ctx: CallContext, input: Payload| {
            self(ctx, serde_json::from_slice(&input.data).unwrap())
                .map(|r| {
                    r.and_then(|r| {
                        let exit_val: CallExitValue<O> = r.into();
                        match exit_val {
                            // ActExitValue::WillCompleteAsync => Ok(ActExitValue::WillCompleteAsync),
                            CallExitValue::Normal(x) => match serde_json::to_value(x) {
                                Ok(v) => Ok(CallExitValue::Normal(v)),
                                Err(e) => Err(CallError::NonRetryable(e.into())),
                            },
                        }
                    })
                })
                .boxed()
            // Some minor gymnastics are required to avoid needing to clone the function
            // match A::from_json_payload(&input) {
            //     Ok(deser) => ,
            //     Err(e) => async move { Err(ActivityError::NonRetryable(e.into())) }.boxed(),
            // }
        };
        Arc::new(wrapper)
    }
}

fn downcast_owned<T: Send + Sync + 'static>(boxed: Box<dyn Any + Send + Sync>) -> Option<T> {
    boxed.downcast().ok().map(|boxed| *boxed)
}

#[derive(Clone)]
pub struct CallContext {
    // worker: Arc<dyn Worker>,
    app_data: Arc<AppData>,
    cancellation_token: CancellationToken,
    input: Vec<Value>,
    heartbeat_details: Vec<Value>,
    header_fields: HashMap<String, Value>,
    info: CallInfo,
}

struct CallHeartbeat {
    pub task_token: Vec<u8>,
    pub details: Vec<Value>,
}

#[derive(Clone, Default)]
pub struct CallInfo {
    pub task_token: Vec<u8>,
    pub namespace: String,
    pub notification_id: String,
    pub notification_type: String,
    pub task_queue: String,
    pub heartbeat_timeout: Option<Duration>,
    /// Time activity was scheduled by a workflow
    pub scheduled_time: Option<SystemTime>,
    /// Time of activity start
    pub started_time: Option<SystemTime>,
    /// Time of activity timeout
    pub deadline: Option<SystemTime>,
    /// Attempt starts from 1, and increase by 1 for every retry, if retry policy is specified.
    pub attempt: u32,
    /// Time this attempt at the activity was scheduled
    pub current_attempt_scheduled_time: Option<SystemTime>,
    pub retry_policy: immortal::RetryPolicy,
    pub is_local: bool,
}


#[derive(Clone)]
struct Start {
    // The namespace the workflow lives in
    pub namespace: String,
    // The activity's ID
    pub notification_id: String,
    // The activity's type name or function identifier
    pub notification_type: String,
    pub header_fields: HashMap<String, Value>,
    // Arguments to the activity
    pub input: Vec<Value>,
    // The last details that were recorded by a heartbeat when this task was generated
    pub heartbeat_details: Vec<Value>,
    // When the task was *first* scheduled
    pub scheduled_time: Option<SystemTime>,
    // When this current attempt at the task was scheduled
    pub current_attempt_scheduled_time: Option<SystemTime>,
    // When this attempt was started, which is to say when core received it by polling.
    pub started_time: Option<SystemTime>,
    pub attempt: u32,

    // Timeout from the first schedule time to completion
    pub schedule_to_close_timeout: Option<Duration>,
    // Timeout from starting an attempt to reporting its result
    pub start_to_close_timeout: Option<Duration>,
    // If set a heartbeat must be reported within this interval
    pub heartbeat_timeout: Option<Duration>,
    // This is an actual retry policy the service uses. It can be different from the one provided
    // (or not) during activity scheduling as the service can override the provided one in case its
    // values are not specified or exceed configured system limits.
    pub retry_policy: immortal::RetryPolicy,

    // Set to true if this is a local activity. Note that heartbeating does not apply to local
    // activities.
    pub is_local: bool,
}

impl CallContext {
    /// Construct new Activity Context, returning the context and the first argument to the activity
    /// (which may be a default [Payload]).
    pub(crate) fn new(
        // worker: Arc<dyn Worker>,
        app_data: Arc<AppData>,
        cancellation_token: CancellationToken,
        task_queue: String,
        task_token: Vec<u8>,
        task: Start,
    ) -> (Self, Value) {
        let Start {
            namespace,
            notification_id,
            notification_type,
            header_fields,
            mut input,
            heartbeat_details,
            scheduled_time,
            current_attempt_scheduled_time,
            started_time,
            attempt,
            schedule_to_close_timeout,
            start_to_close_timeout,
            heartbeat_timeout,
            retry_policy,
            is_local,
        } = task;
        let deadline = calculate_deadline(
            scheduled_time.as_ref(),
            started_time.as_ref(),
            start_to_close_timeout.as_ref(),
            schedule_to_close_timeout.as_ref(),
        );
        let first_arg = input.pop().unwrap_or_default();

        (
            CallContext {
                // worker,
                app_data,
                cancellation_token,
                input,
                heartbeat_details,
                header_fields,
                info: CallInfo {
                    task_token,
                    task_queue,
                    namespace,
                    notification_id,
                    notification_type,
                    heartbeat_timeout,
                    scheduled_time,
                    started_time,
                    deadline,
                    attempt,
                    current_attempt_scheduled_time,
                    retry_policy,
                    is_local,
                },
            },
            first_arg,
        )
    }

    /// Returns a future the completes if and when the activity this was called inside has been
    /// cancelled
    pub async fn cancelled(&self) {
        self.cancellation_token.clone().cancelled().await
    }

    /// Returns true if this activity has already been cancelled
    pub fn is_cancelled(&self) -> bool {
        self.cancellation_token.is_cancelled()
    }

    /// Retrieve extra parameters to the Activity. The first input is always popped and passed to
    /// the Activity function for the currently executing activity. However, if more parameters are
    /// passed, perhaps from another language's SDK, explicit access is available from extra_inputs
    pub fn extra_inputs(&mut self) -> &mut [Value] {
        &mut self.input
    }

    /// Extract heartbeat details from last failed attempt. This is used in combination with retry policy.
    pub fn get_heartbeat_details(&self) -> &[Value] {
        &self.heartbeat_details
    }

    /// RecordHeartbeat sends heartbeat for the currently executing activity
    pub fn record_heartbeat(&self, details: Vec<Value>) {
        // self.worker.record_activity_heartbeat(ActivityHeartbeat {
        //     task_token: self.info.task_token.clone(),
        //     details,
        // })
    }

    /// Get activity info of the executing activity
    pub fn get_info(&self) -> &CallInfo {
        &self.info
    }

    /// Get headers attached to this activity
    pub fn headers(&self) -> &HashMap<String, Value> {
        &self.header_fields
    }

    /// Get custom Application Data
    pub fn app_data<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.app_data.get::<T>()
    }
}

/// Deadline calculation.  This is a port of
/// https://github.com/temporalio/sdk-go/blob/8651550973088f27f678118f997839fb1bb9e62f/internal/activity.go#L225
fn calculate_deadline(
    scheduled_time: Option<&SystemTime>,
    started_time: Option<&SystemTime>,
    start_to_close_timeout: Option<&Duration>,
    schedule_to_close_timeout: Option<&Duration>,
) -> Option<SystemTime> {
    match (
        scheduled_time,
        started_time,
        start_to_close_timeout,
        schedule_to_close_timeout,
    ) {
        (
            Some(scheduled),
            Some(started),
            Some(start_to_close_timeout),
            Some(schedule_to_close_timeout),
        ) => {
            // let scheduled: SystemTime = maybe_convert_timestamp(scheduled)?;
            // let started: SystemTime = maybe_convert_timestamp(started)?;
            let start_to_close_timeout: Duration = (*start_to_close_timeout).try_into().ok()?;
            let schedule_to_close_timeout: Duration =
                (*schedule_to_close_timeout).try_into().ok()?;

            let start_to_close_deadline: SystemTime =
                started.checked_add(start_to_close_timeout)?;
            if schedule_to_close_timeout > Duration::ZERO {
                let schedule_to_close_deadline =
                    scheduled.checked_add(schedule_to_close_timeout)?;
                // Minimum of the two deadlines.
                if schedule_to_close_deadline < start_to_close_deadline {
                    Some(schedule_to_close_deadline)
                } else {
                    Some(start_to_close_deadline)
                }
            } else {
                Some(start_to_close_deadline)
            }
        }
        _ => None,
    }
}
//
// /// Helper function lifted from prost_types::Timestamp implementation to prevent double cloning in
// /// error construction
// fn maybe_convert_timestamp(timestamp: &SystemTime) -> Option<SystemTime> {
//     let mut timestamp = *timestamp;
//     timestamp.normalize();
//
//     let system_time = if timestamp.seconds >= 0 {
//         std::time::UNIX_EPOCH.checked_add(StdDuration::from_secs(timestamp.seconds as u64))
//     } else {
//         std::time::UNIX_EPOCH.checked_sub(StdDuration::from_secs((-timestamp.seconds) as u64))
//     };
//
//     system_time.and_then(|system_time| {
//         system_time.checked_add(StdDuration::from_nanos(timestamp.nanos as u64))
//     })
// }
//
