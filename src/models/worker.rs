// use futures::{FutureExt};
use anyhow::{anyhow, Error};
// pub mod immortal {
//     tonic::include_proto!("immortal");
// }

use async_stream::stream;
use serde::Serialize;
use serde_json::Value;
use std::fmt::{Debug, Write};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::broadcast::Receiver;
use tokio::sync::{
    broadcast,
    mpsc::{self, UnboundedReceiver, UnboundedSender},
    Mutex,
};
use tonic::transport::Channel;
use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::span::{Attributes, Id};
use tracing::{Event, Subscriber};
use tracing_subscriber::Registry;
use tracing_subscriber::{layer::Context, layer::SubscriberExt, Layer};
use uuid::Uuid;

use tokio::task::JoinHandle;

use crate::common::{Payload, Payloads};
use crate::failure::failure::FailureInfo;
use crate::failure::Failure;
use crate::immortal::{
    activity_result_v1, activity_result_version, immortal_client::ImmortalClient,
    immortal_server_action_v1, immortal_server_action_version, immortal_worker_action_v1::Action,
    immortal_worker_action_version, request_start_activity_options_version, ActivityResultV1,
    ActivityResultVersion, ImmortalServerActionV1, ImmortalServerActionVersion, Log,
    RegisterImmortalWorkerV1, StartWorkflowOptionsV1,
};
use crate::immortal::{
    workflow_result_v1, workflow_result_version, Failure as ImmortalFailure,
    RequestStartActivityOptionsV1, RequestStartActivityOptionsVersion, Success as ImmortalSuccess,
    WorkflowResultV1, WorkflowResultVersion,
};

use super::{
    activity::{
        ActExitValue, ActivityError, ActivityFunction, ActivityOptions, AppData, IntoActivityFunc,
    },
    workflow::{WfExitValue, WorkflowFunction},
};

pub struct RunningWorkflow {
    pub workflow_id: String,
    pub run_id: String,
    pub join_handle: JoinHandle<()>,
}

//
// pub struct PreviousWorkflow {
//     pub workflow_id: String,
//     pub run_id: String,
//     pub start: NaiveDateTime,
//     pub end: NaiveDateTime,
// }

pub struct RunningActivity {
    pub activity_id: String,
    pub run_id: String,
    pub join_handle: JoinHandle<()>,
}

pub struct Worker {
    pub task_queue: String,
    pub client: ImmortalClient<Channel>,
    pub config: WorkerConfig,
    pub server_channel: tokio::sync::broadcast::Sender<ImmortalServerActionV1>,
    pub workflow_sender: UnboundedSender<(String, String, Result<WfExitValue<Value>, Error>)>,
    pub activity_sender:
        UnboundedSender<(String, String, Result<ActExitValue<Value>, ActivityError>)>,
    // pub client: Arc<dyn WorkerClient>,
    pub workery_key: String,
    pub registered_workflows: Arc<Mutex<HashMap<String, WorkflowFunction>>>,
    pub running_workflows: Arc<Mutex<HashMap<String, RunningWorkflow>>>,
    pub running_activities: Arc<Mutex<HashMap<String, RunningActivity>>>,
    // active_workflows: WorkflowHalf,
    pub registered_activities: Arc<Mutex<HashMap<String, ActivityFunction>>>,
    pub app_data: Option<AppData>,
}

struct ChannelLayer {
    sender: broadcast::Sender<ImmortalServerActionV1>, // Define the type of data to be sent through the channel
}

// impl Into<immortal::Event> for Event<'_> {
//     fn into(self) -> immortal::Event {
//         let metadata = self.metadata().
//         immortal::Event {
//             metadata: Some(immortal::Metadata { name: self.metadata().name(), target: self.metadata().target(), level: self.metadata().level(), module_path: self.metadata().module_path(), location: self.metadata().line(), fields: self.metadata().fields(), callsite: self.metadata().callsite(), kind: self.metadata().  })
//             level: self.metadata.level.into(),
//             target: self.metadata.target.into(),
//             fields: self.fields.into_iter().map(|x| x.into()).collect(),
//         }
//     }
// }

// impl Into<prost_wkt_types::Duration> for Duration {
//     fn into(self) -> immortal::Duration {
//         immortal::Duration {
//             seconds: self.as_secs() as i64,
//             nanos: self.subsec_nanos() as i32,
//         }
//     }
// }
//
// impl From<immortal::Duration> for Duration {
//     fn from(d: immortal::Duration) -> Self {
//         Duration::new(d.seconds as u64, d.nanos as u32)
//     }
// }

#[derive(Debug)]
struct SpanData {
    workflow_id: String,
    workflow_run_id: String,
    activity_id: Option<String>,
    activity_run_id: Option<String>,
}
#[derive(Default)]
struct StrVisitor {
    activity_id: Option<String>,
    activity_run_id: Option<String>,
    workflow_id: Option<String>,
    workflow_run_id: Option<String>,
}

impl Visit for StrVisitor {
    fn record_debug(&mut self, _field: &Field, _value: &dyn Debug) {}
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "workflow_id" {
            self.workflow_id = Some(value.to_string());
        } else if field.name() == "workflow_run_id" {
            self.workflow_run_id = Some(value.to_string());
        } else if field.name() == "activity_id" {
            self.activity_id = Some(value.to_string());
        } else if field.name() == "activity_run_id" {
            self.activity_run_id = Some(value.to_string());
        }
    }
}

impl<S> Layer<S> for ChannelLayer
where
    S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let fields: Vec<_> = attrs.fields().iter().map(|f| f.name()).collect();
        if fields.contains(&"workflow_id") {
            let values = attrs.values();
            let mut visitor = StrVisitor::default();
            values.record(&mut visitor);
            ctx.span(id).unwrap().extensions_mut().insert(SpanData {
                workflow_id: visitor.workflow_id.unwrap(),
                workflow_run_id: visitor.workflow_run_id.unwrap(),
                activity_id: visitor.activity_id,
                activity_run_id: visitor.activity_run_id,
            });
        }

        // match visitor.value {
        //     StrVisitorFound::Found(x) => {
        //         println!("Found: {:?}", x);
        //     }
        //     StrVisitorFound::NotFound => {
        //         println!("Not Found");
        //     }
        // }

        // let span = ctx.span(id).unwrap();
        // if value_in_valueset(attrs.values(), "myfield", "myvalue") {
        // ctx.span(id).unwrap().extensions_mut().insert(CustomLayerEnabled);
        // }
    }
    fn on_event(&self, event: &Event, ctx: Context<S>) {
        // event.
        // Format the event as a string
        let mut event_str = String::new();

        let _ = writeln!(&mut event_str, "{:?}", event);

        println!("Event: {:?}", event_str);
        if let Some(scope) = ctx.event_scope(&event) {
            for span in scope.from_root() {
                if let Some(data) = span.extensions().get::<SpanData>() {
                    let _ = self.sender.send(ImmortalServerActionV1 {
                        action: Some(immortal_server_action_v1::Action::LogEvent(Log {
                            workflow_run_id: data.workflow_run_id.clone(),
                            activity_run_id: data.activity_run_id.clone(),
                            message: event_str.clone(),
                            when: chrono::Utc::now().to_utc().timestamp(),
                            workflow_id: data.workflow_id.clone(),
                            activity_id: data.activity_id.clone(),
                        })),
                    });
                    return;
                }
            }
        }

        // Send the formatted event to the channel
    }
}

impl Worker {
    pub async fn new() -> anyhow::Result<(Self, Receiver<ImmortalServerActionV1>)> {
        let config = WorkerConfigBuilder::default()
            .namespace("default".to_string())
            .task_queue("test-worker".to_string())
            .worker_build_id("test-worker".to_string())
            .build()?;

        let (tx, rx) = mpsc::unbounded_channel();
        let (atx, arx) = mpsc::unbounded_channel();

        let (stx, srx) = broadcast::channel(100);
        let stx2 = stx.clone();
        // let make_writer = move || ChannelWriter(stx2.clone())
        let channel_layer = ChannelLayer {
            sender: stx2.clone(),
        };
        let subscriber = Registry::default()
            .with(channel_layer)
            .with(LevelFilter::INFO);
        tracing::subscriber::set_global_default(subscriber)
            .expect("Failed to set global subscriber");
        let client = ImmortalClient::connect("http://[::1]:10000").await?;
        let mut worker = Worker {
            server_channel: stx,
            client,
            config,
            workflow_sender: tx,
            activity_sender: atx,
            task_queue: "test-worker".to_string(),
            workery_key: "test-worker".to_string(),
            running_workflows: Arc::new(Mutex::new(HashMap::new())),
            running_activities: Arc::new(Mutex::new(HashMap::new())),
            registered_workflows: Arc::new(Mutex::new(HashMap::new())),
            registered_activities: Arc::new(Mutex::new(HashMap::new())),
            // previous_workflows: Arc::new(Mutex::new(HashMap::new())),
            app_data: Some(AppData::default()),
        };
        worker.workflow_thread(rx);
        worker.activity_thread(arx);
        Ok((worker, srx))
    }

    pub async fn register_wf(
        &mut self,
        workflow_type: impl Into<String>,
        wf_function: impl Into<WorkflowFunction>,
    ) {
        let mut registered_workflows = self.registered_workflows.lock().await;
        registered_workflows.insert(workflow_type.into(), wf_function.into());
        // .workflow_fns
        // .get_mut()
        // .insert(workflow_type.into(), wf_function.into());
    }
    pub async fn register_activity<A, R, O>(
        &mut self,
        activity_type: impl Into<String>,
        act_function: impl IntoActivityFunc<A, R, O>,
    ) {
        let mut registered_activities = self.registered_activities.lock().await;
        registered_activities.insert(
            activity_type.into(),
            ActivityFunction {
                act_func: act_function.into_activity_fn(),
            },
        );
    }

    /// Insert Custom App Context for Workflows and Activities
    pub fn insert_app_data<T: Send + Sync + 'static>(&mut self, data: T) {
        self.app_data.as_mut().map(|a| a.insert(data));
    }

    async fn main_thread2(
        &mut self,
        mut rx: Receiver<ImmortalServerActionV1>,
        register_immortal_worker: RegisterImmortalWorkerV1,
        safe_app_data: &Arc<AppData>,
    ) -> anyhow::Result<()> {
        let mut stream = self
            .client
            .register_worker(stream! {
                yield ImmortalServerActionVersion {
                    version: Some(immortal_server_action_version::Version::V1(ImmortalServerActionV1 {
                        action: Some(immortal_server_action_v1::Action::RegisterWorker(register_immortal_worker.clone()))
                    }
                    ))
                };
                while let y = rx.recv().await {
                    match y {
                        Ok(y) => {
                            yield ImmortalServerActionVersion {
                                version: Some(immortal_server_action_version::Version::V1(y))
                            }
                        },
                        Err(e) => println!("Error: {:?}", e)

                    }

                }
                println!("worker Stream ended");
            })
            .await?
            .into_inner();

        // info!("Worker registered");
        // on failure this needs to reconnect
        while let Some(feature) = stream.message().await? {
            // println!("NOTE = {:?}", feature);
            match feature.version {
                Some(immortal_worker_action_version::Version::V1(x)) => match x.action {
                    Some(Action::StartWorkflow(workflow)) => {
                        println!("Starting workflow: {:?}", workflow);
                        self.start_workflow_v1(&workflow).await;
                    }
                    Some(Action::StartActivity(activity)) => {
                        self.start_activity(
                            &activity.workflow_id,
                            &activity.workflow_run_id,
                            &activity.activity_type,
                            &activity.activity_id,
                            &activity.activity_run_id,
                            activity.activity_input.unwrap(),
                            safe_app_data,
                        )
                        .await;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        Ok(())
    }

    pub async fn main_thread(
        &mut self,
        rx: &broadcast::Receiver<ImmortalServerActionV1>,
    ) -> anyhow::Result<()> {
        // let (tx, mut rx) = broadcast::channel(100);
        let rx = rx.resubscribe();

        let register_immortal_worker = RegisterImmortalWorkerV1 {
            worker_id: self.workery_key.clone(),
            worker_type: self.task_queue.clone(),
            registered_activities: self
                .registered_activities
                .lock()
                .await
                .keys()
                .map(|x| x.clone())
                .collect(),
            registered_workflows: self
                .registered_workflows
                .lock()
                .await
                .keys()
                .map(|x| x.clone())
                .collect(),
        };

        let safe_app_data = Arc::new(
            self.app_data
                .take()
                .ok_or_else(|| anyhow!("app_data should exist on run"))
                .unwrap(),
        );
        match self
            .main_thread2(rx, register_immortal_worker, &safe_app_data)
            .await
        {
            Ok(_) => {}
            Err(_) => {}
        }
        // let temp_stream = BroadcastStream::new(rx)
        //     .filter_map(|item| async move {
        //         // ignore receive errors
        //         item.ok()
        //     })
        //     .map(Ok);

        self.app_data = Some(
            Arc::try_unwrap(safe_app_data)
                .map_err(|_| anyhow!("some references of AppData exist on worker shutdown"))?,
        );
        Ok(())
    }

    pub async fn start_workflow_v1(&mut self, workflow_options: &StartWorkflowOptionsV1) {
        let registered_workflows = Arc::clone(&self.registered_workflows);
        let workflow_type = workflow_options.workflow_type.to_string();
        let sender = self.workflow_sender.clone();
        let client = self.client.clone();
        // let args = serde_json::from_slice(&workflow_options.input).unwrap();
        println!("waiting for lock");
        let workflow_id = workflow_options.workflow_id.clone();
        let workflow_run_id = workflow_options.workflow_run_id.clone();
        let wf_handle;
        {
            let mut wf2 = registered_workflows.lock().await;
            let wf = wf2.get_mut(&workflow_type).unwrap();

            wf_handle = wf.start_workflow(
                client,
                workflow_options
                    .input
                    .clone()
                    .unwrap_or(Payloads::default()),
                workflow_type.clone(),
                workflow_id.clone(),
                workflow_run_id.clone(),
            );
        }
        let handle;
        {
            let workflow_id = workflow_id.clone();
            let workflow_run_id = workflow_run_id.clone();
            handle = tokio::spawn(async move {
                let res = wf_handle.await;
                sender
                    .send((workflow_id.clone(), workflow_run_id.clone(), res))
                    .unwrap();
            });
        }

        let running_workflow = RunningWorkflow {
            workflow_id,
            run_id: workflow_run_id,
            join_handle: handle,
        };
        self.running_workflows
            .lock()
            .await
            .insert("test".to_string(), running_workflow);
    }

    pub fn workflow_thread(
        &mut self,
        mut rx: UnboundedReceiver<(String, String, Result<WfExitValue<Value>, Error>)>,
    ) {
        let mut client = self.client.clone();
        let running_workflows_arc = Arc::clone(&self.running_workflows);
        tokio::spawn(async move {
            while let Some(result) = rx.recv().await {
                match result.2 {
                    Ok(res) => {
                        client
                            .completed_workflow(WorkflowResultVersion {
                                version: Some(workflow_result_version::Version::V1(
                                    WorkflowResultV1 {
                                        workflow_id: result.0,
                                        workflow_run_id: result.1,
                                        status: Some(workflow_result_v1::Status::Completed(
                                            ImmortalSuccess {
                                                result: Some(Payload::new(&res)),
                                            },
                                        )),
                                    },
                                )),
                            })
                            .await
                            .unwrap();
                    }
                    Err(e) => {
                        client
                            .completed_workflow(WorkflowResultVersion {
                                version: Some(workflow_result_version::Version::V1(
                                    WorkflowResultV1 {
                                        workflow_id: result.0,
                                        workflow_run_id: result.1,
                                        status: Some(workflow_result_v1::Status::Failed(
                                            ImmortalFailure {
                                                failure: Some(Failure {
                                                    message: e.to_string(),
                                                    source: "worker".to_string(),
                                                    stack_trace: e.backtrace().to_string(),
                                                    encoded_attributes: None,
                                                    cause: None,
                                                    failure_info: None,
                                                }),
                                            },
                                        )),
                                    },
                                )),
                            })
                            .await
                            .unwrap();
                    }
                }
                let mut running_workflows = running_workflows_arc.lock().await;
                // let running_workflow = running_workflows.get("test").unwrap();
                // running_workflow.join_handle.abort();
                running_workflows.remove("test");
            }
        });
    }

    pub fn activity_thread(
        &mut self,
        mut rx: UnboundedReceiver<(String, String, Result<ActExitValue<Value>, ActivityError>)>,
    ) {
        let running_activities_arc = Arc::clone(&self.running_activities);

        let mut client = self.client.clone();
        tokio::spawn(async move {
            while let Some(result) = rx.recv().await {
                let res = match result.2 {
                    // Err(e) => ActivityExecutionResult::fail(Failure::application_failure(
                    //     format!("Activity function panicked: {}", panic_formatter(e)),
                    //     true,
                    // )),
                    Ok(ActExitValue::Normal(p)) => ActivityResultV1::ok(
                        result.0.clone(),
                        result.1.clone(),
                        Some(Payload::new(&p)),
                    ),

                    Err(err) => match err {
                        ActivityError::Retryable {
                            source,
                            explicit_delay,
                        } => ActivityResultV1::fail(result.0.clone(), result.1.clone(), {
                            println!("source: {:?}", source);
                            let mut f = Failure::application_failure_from_error(source, false);
                            if let Some(d) = explicit_delay {
                                if let Some(FailureInfo::ApplicationFailureInfo(fi)) =
                                    f.failure_info.as_mut()
                                {
                                    fi.next_retry_delay = d.try_into().ok();
                                }
                            }
                            f
                        }),
                        ActivityError::Cancelled { details } => {
                            ActivityResultV1::cancel_from_details(
                                result.0.clone(),
                                result.1.clone(),
                                match details {
                                    Some(d) => Some(vec![serde_json::to_vec(&d).unwrap()]),
                                    None => None,
                                },
                            )
                        }
                        ActivityError::NonRetryable(nre) => ActivityResultV1::fail(
                            result.0.clone(),
                            result.1.clone(),
                            Failure::application_failure_from_error(nre, true),
                        ),
                    },
                };
                client
                    .completed_activity(ActivityResultVersion {
                        version: Some(activity_result_version::Version::V1(res)),
                    })
                    .await
                    .unwrap();

                let mut running_activities = running_activities_arc.lock().await;
                // let running_activity = running_activities.get("test").unwrap();
                // running_activity.join_handle.abort();
                running_activities.remove(&result.0);
            }
        });
    }

    pub async fn activity(
        &mut self,
        workflow_id: String,
        workflow_run_id: String,
        activity_options: ActivityOptions,
    ) -> anyhow::Result<Value> {
        let mut temp = RequestStartActivityOptionsV1 {
            activity_id: activity_options
                .activity_id
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            activity_type: activity_options.activity_type,
            activity_input: Some(activity_options.input),
            schedule_to_close_timeout: activity_options
                .schedule_to_close_timeout
                .map(|x| x.try_into().unwrap()),
            schedule_to_start_timeout: activity_options
                .schedule_to_start_timeout
                .map(|x| x.try_into().unwrap()),
            start_to_close_timeout: activity_options
                .start_to_close_timeout
                .map(|x| x.try_into().unwrap()),
            heartbeat_timeout: activity_options
                .heartbeat_timeout
                .map(|x| x.try_into().unwrap()),
            retry_policy: activity_options.retry_policy.map(|x| x.try_into().unwrap()),
            workflow_id,
            workflow_run_id,
            ..Default::default()
        };
        temp.set_cancellation_type(activity_options.cancellation_type);
        match self
            .client
            .start_activity(RequestStartActivityOptionsVersion {
                version: Some(request_start_activity_options_version::Version::V1(temp)),
            })
            .await
        {
            Ok(activity_result) => {
                println!("received Activity completed: {:?}", activity_result);
                match activity_result.into_inner().version {
                    Some(activity_result_version::Version::V1(x)) => match x.status {
                        Some(activity_result_v1::Status::Failed(x)) => Err(anyhow!("{:#?}", x)),
                        Some(activity_result_v1::Status::Cancelled(x)) => Err(anyhow!("{:#?}", x)),
                        Some(activity_result_v1::Status::Completed(y)) => {
                            println!("Activity result: {:?}", y.result);
                            Ok(serde_json::from_slice(&y.result.unwrap().data)?)
                        }

                        None => Err(anyhow!("Activity failed")),
                    },
                    // Some(x) => Ok(serde_json::from_str(&x)?),
                    None => Err(anyhow!("Activity failed")),
                }
            }
            Err(e) => Err(anyhow!("Activity failed: {:?}", e)),
        }
    }

    pub async fn start_activity(
        &mut self,
        workflow_id: &str,
        workflow_run_id: &str,
        activity_type: &str,
        activity_id: &str,
        activity_run_id: &str,
        payload: Payload,
        safe_app_data: &Arc<AppData>,
    ) {
        let registered_activities = Arc::clone(&self.registered_activities);
        let activity_type = activity_type.to_string();

        let sender = self.activity_sender.clone();
        let mut act2 = registered_activities.lock().await;
        let act = act2.get_mut(&activity_type).unwrap();

        let aid = activity_id.to_string();
        let aid_run = activity_run_id.to_string();

        let act_handle = act.start_activity(
            payload,
            safe_app_data.clone(),
            activity_type,
            workflow_id.to_string(),
            workflow_run_id.to_string(),
            activity_id.to_string(),
            activity_run_id.to_string(),
        );
        let handle = tokio::spawn(async move {
            let res = act_handle.await;

            sender.send((aid, aid_run, res)).unwrap();
        });
        let running_workflow = RunningActivity {
            activity_id: activity_id.to_string(),
            run_id: activity_run_id.to_string(),
            join_handle: handle,
        };
        self.running_activities
            .lock()
            .await
            .insert(activity_id.to_string(), running_workflow);
        // self.app_data = Some(
        //     Arc::try_unwrap(safe_app_data)
        //         .map_err(|_| anyhow!("some references of AppData exist on worker shutdown"))
        //         .unwrap(),
        // );
    }
}

/// Defines per-worker configuration options
#[derive(Clone, derive_builder::Builder)]
#[builder(setter(into))]
#[non_exhaustive]
pub struct WorkerConfig {
    /// The Temporal service namespace this worker is bound to
    pub namespace: String,
    /// What task queue will this worker poll from? This task queue name will be used for both
    /// workflow and activity polling.
    pub task_queue: String,
    /// A string that should be unique to the set of code this worker uses. IE: All the workflow,
    /// activity, interceptor, and data converter code.
    pub worker_build_id: String,
    /// A human-readable string that can identify this worker. Using something like sdk version
    /// and host name is a good default. If set, overrides the identity set (if any) on the client
    /// used by this worker.
    #[builder(default)]
    pub client_identity_override: Option<String>,
    /// If set nonzero, workflows will be cached and sticky task queues will be used, meaning that
    /// history updates are applied incrementally to suspended instances of workflow execution.
    /// Workflows are evicted according to a least-recently-used policy one the cache maximum is
    /// reached. Workflows may also be explicitly evicted at any time, or as a result of errors
    /// or failures.
    #[builder(default = "0")]
    pub max_cached_workflows: usize,
    /// Set a [WorkerTuner] for this worker. Either this or at least one of the `max_outstanding_*`
    /// fields must be set.
    // #[builder(setter(into = false, strip_option), default)]
    // pub tuner: Option<Arc<dyn WorkerTuner + Send + Sync>>,
    /// Maximum number of concurrent poll workflow task requests we will perform at a time on this
    /// worker's task queue. See also [WorkerConfig::nonsticky_to_sticky_poll_ratio]. Must be at
    /// least 1.
    // #[builder(default = "MAX_CONCURRENT_WFT_POLLS_DEFAULT")]
    // pub max_concurrent_wft_polls: usize,
    /// [WorkerConfig::max_concurrent_wft_polls] * this number = the number of max pollers that will
    /// be allowed for the nonsticky queue when sticky tasks are enabled. If both defaults are used,
    /// the sticky queue will allow 4 max pollers while the nonsticky queue will allow one. The
    /// minimum for either poller is 1, so if `max_concurrent_wft_polls` is 1 and sticky queues are
    /// enabled, there will be 2 concurrent polls.
    #[builder(default = "0.2")]
    pub nonsticky_to_sticky_poll_ratio: f32,
    /// Maximum number of concurrent poll activity task requests we will perform at a time on this
    /// worker's task queue
    #[builder(default = "5")]
    pub max_concurrent_at_polls: usize,
    /// If set to true this worker will only handle workflow tasks and local activities, it will not
    /// poll for activity tasks.
    #[builder(default = "false")]
    pub no_remote_activities: bool,
    /// How long a workflow task is allowed to sit on the sticky queue before it is timed out
    /// and moved to the non-sticky queue where it may be picked up by any worker.
    #[builder(default = "Duration::from_secs(10)")]
    pub sticky_queue_schedule_to_start_timeout: Duration,

    /// Longest interval for throttling activity heartbeats
    #[builder(default = "Duration::from_secs(60)")]
    pub max_heartbeat_throttle_interval: Duration,

    /// Default interval for throttling activity heartbeats in case
    /// `ActivityOptions.heartbeat_timeout` is unset.
    /// When the timeout *is* set in the `ActivityOptions`, throttling is set to
    /// `heartbeat_timeout * 0.8`.
    #[builder(default = "Duration::from_secs(30)")]
    pub default_heartbeat_throttle_interval: Duration,

    /// Sets the maximum number of activities per second the task queue will dispatch, controlled
    /// server-side. Note that this only takes effect upon an activity poll request. If multiple
    /// workers on the same queue have different values set, they will thrash with the last poller
    /// winning.
    #[builder(default)]
    pub max_task_queue_activities_per_second: Option<f64>,

    /// Limits the number of activities per second that this worker will process. The worker will
    /// not poll for new activities if by doing so it might receive and execute an activity which
    /// would cause it to exceed this limit. Negative, zero, or NaN values will cause building
    /// the options to fail.
    #[builder(default)]
    pub max_worker_activities_per_second: Option<f64>,

    /// # UNDER DEVELOPMENT
    /// If set to true this worker will opt-in to the whole-worker versioning feature.
    /// `worker_build_id` will be used as the version.
    /// todo: link to feature docs
    #[builder(default = "false")]
    pub use_worker_versioning: bool,

    /// If set false (default), shutdown will not finish until all pending evictions have been
    /// issued and replied to. If set true shutdown will be considered complete when the only
    /// remaining work is pending evictions.
    ///
    /// This flag is useful during tests to avoid needing to deal with lots of uninteresting
    /// evictions during shutdown. Alternatively, if a lang implementation finds it easy to clean
    /// up during shutdown, setting this true saves some back-and-forth.
    #[builder(default = "false")]
    pub ignore_evicts_on_shutdown: bool,

    /// Maximum number of next page (or initial) history event listing requests we'll make
    /// concurrently. I don't this it's worth exposing this to users until we encounter a reason.
    #[builder(default = "5")]
    pub fetching_concurrency: usize,

    /// If set, core will issue cancels for all outstanding activities after shutdown has been
    /// initiated and this amount of time has elapsed.
    #[builder(default)]
    pub graceful_shutdown_period: Option<Duration>,

    /// The amount of time core will wait before timing out activities using its own local timers
    /// after one of them elapses. This is to avoid racing with server's own tracking of the
    /// timeout.
    #[builder(default = "Duration::from_secs(5)")]
    pub local_timeout_buffer_for_activities: Duration,

    /// Any error types listed here will cause any workflow being processed by this worker to fail,
    /// rather than simply failing the workflow task.
    // #[builder(default)]
    // pub workflow_failure_errors: HashSet<WorkflowErrorType>,

    /// Like [WorkerConfig::workflow_failure_errors], but specific to certain workflow types (the
    /// map key).
    // #[builder(default)]
    // pub workflow_types_to_failure_errors: HashMap<String, HashSet<WorkflowErrorType>>,

    /// The maximum allowed number of workflow tasks that will ever be given to this worker at one
    /// time. Note that one workflow task may require multiple activations - so the WFT counts as
    /// "outstanding" until all activations it requires have been completed.
    ///
    /// Mutually exclusive with `tuner`
    #[builder(setter(into, strip_option), default)]
    pub max_outstanding_workflow_tasks: Option<usize>,
    /// The maximum number of activity tasks that will ever be given to this worker concurrently
    ///
    /// Mutually exclusive with `tuner`
    #[builder(setter(into, strip_option), default)]
    pub max_outstanding_activities: Option<usize>,
    /// The maximum number of local activity tasks that will ever be given to this worker
    /// concurrently
    ///
    /// Mutually exclusive with `tuner`
    #[builder(setter(into, strip_option), default)]
    pub max_outstanding_local_activities: Option<usize>,
}

pub(crate) trait WorkerClient: Sync + Send {
    // async fn poll_workflow_task(
    //     &self,
    //     task_queue: TaskQueue,
    // ) -> Result<PollWorkflowTaskQueueResponse>;
    // async fn poll_activity_task(
    //     &self,
    //     task_queue: String,
    //     max_tasks_per_sec: Option<f64>,
    // ) -> Result<PollActivityTaskQueueResponse>;
    // async fn complete_workflow_task(
    //     &self,
    //     request: WorkflowTaskCompletion,
    // ) -> Result<RespondWorkflowTaskCompletedResponse>;
    // async fn complete_activity_task(
    //     &self,
    //     task_token: TaskToken,
    //     result: Option<Payloads>,
    // ) -> Result<RespondActivityTaskCompletedResponse>;
    // async fn record_activity_heartbeat(
    //     &self,
    //     task_token: TaskToken,
    //     details: Option<Payloads>,
    // ) -> Result<RecordActivityTaskHeartbeatResponse>;
    // async fn cancel_activity_task(
    //     &self,
    //     task_token: TaskToken,
    //     details: Option<Payloads>,
    // ) -> Result<RespondActivityTaskCanceledResponse>;
    // async fn fail_activity_task(
    //     &self,
    //     task_token: TaskToken,
    //     failure: Option<Failure>,
    // ) -> Result<RespondActivityTaskFailedResponse>;
    // async fn fail_workflow_task(
    //     &self,
    //     task_token: TaskToken,
    //     cause: WorkflowTaskFailedCause,
    //     failure: Option<Failure>,
    // ) -> Result<RespondWorkflowTaskFailedResponse>;
    // async fn get_workflow_execution_history(
    //     &self,
    //     workflow_id: String,
    //     run_id: Option<String>,
    //     page_token: Vec<u8>,
    // ) -> Result<GetWorkflowExecutionHistoryResponse>;
    // async fn respond_legacy_query(
    //     &self,
    //     task_token: TaskToken,
    //     query_result: QueryResult,
    // ) -> Result<RespondQueryTaskCompletedResponse>;
    // async fn describe_namespace(&self) -> Result<DescribeNamespaceResponse>;
    //
    // fn replace_client(&self, new_client: RetryClient<Client>);
    // fn capabilities(&self) -> Option<Capabilities>;
    // fn workers(&self) -> Arc<SlotManager>;
    // fn is_mock(&self) -> bool;
}
