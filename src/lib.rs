#[macro_export]
macro_rules! register_workflow {
    ($worker:expr, $expression:expr, $func_name:path) => {
        paste::paste! {
            // Create the schema function name as a local variable

            $worker
                .register_wf(
                    $expression,
                    $func_name::wf,
                    $func_name::wf_schema()
                )
                .await;
        }
    };
}

use anyhow::anyhow;
use common::{Payload, Payloads};
use immortal::{
    call_result_v1, immortal_client::ImmortalClient, CallV1, CallVersion,
    ClientStartWorkflowOptionsV1, ClientStartWorkflowOptionsVersion, ClientStartWorkflowResponse,
    NotifyV1, NotifyVersion, WorkflowResultV1,
};
use serde::de::DeserializeOwned;
use tonic::transport::Channel;

#[derive(Clone)]
pub struct Client {
    inner: ImmortalClient<Channel>,
}

impl Client {
    pub async fn connect(addr: String) -> Result<Self, tonic::transport::Error> {
        let inner = ImmortalClient::connect(addr).await?;
        Ok(Self { inner })
    }

    pub async fn execute_workflow_v1(
        &mut self,
        input: Option<Payloads>,
        workflow_type: &str,
        task_queue: &str,
    ) -> Result<WorkflowResultV1, tonic::Status> {
        let result = self
            .inner
            .execute_workflow(ClientStartWorkflowOptionsVersion {
                version: Some(
                    immortal::client_start_workflow_options_version::Version::V1(
                        ClientStartWorkflowOptionsV1 {
                            workflow_type: workflow_type.to_string(),
                            input,
                            task_queue: task_queue.to_string(),
                            ..Default::default()
                        },
                    ),
                ),
            })
            .await?;
        Ok(match result.into_inner().version.unwrap() {
            immortal::workflow_result_version::Version::V1(v1) => v1,
        })
    }

    //pub async fn start_activity_v1(
    //    &mut self,
    //    input: Option<Payloads>,
    //    activity_type: &str,
    //    task_queue: &str,
    //) -> Result<ClientStartWorkflowResponse, tonic::Status> {
    //    let result = self
    //        .inner
    //        .start_activity(RequestStartActivityOptionsVersion {
    //            version: Some(
    //                immortal::request_start_activity_options_version::Version::V1(
    //                    immortal::RequestStartActivityOptionsV1 {
    //                        activity_type: activity_type.to_string(),
    //                        input,
    //                        task_queue: task_queue.to_string(),
    //                        ..Default::default()
    //                    },
    //                ),
    //            ),
    //        })
    //        .await?;
    //    Ok(result.into_inner())
    //}

    pub async fn start_workflow_v1(
        &mut self,
        input: Option<Payloads>,
        workflow_type: &str,
        task_queue: &str,
    ) -> Result<ClientStartWorkflowResponse, tonic::Status> {
        let result = self
            .inner
            .start_workflow(ClientStartWorkflowOptionsVersion {
                version: Some(
                    immortal::client_start_workflow_options_version::Version::V1(
                        ClientStartWorkflowOptionsV1 {
                            workflow_type: workflow_type.to_string(),
                            input,
                            task_queue: task_queue.to_string(),
                            ..Default::default()
                        },
                    ),
                ),
            })
            .await?;
        Ok(result.into_inner())
    }

    pub async fn call_async_v1(
        &mut self,
        input: Option<Payload>,
        call_type: &str,
        task_queue: &str,
    ) -> anyhow::Result<()> {
        self
            .inner
            .call_async(CallVersion {
                version: Some(immortal::call_version::Version::V1(CallV1 {
                    input,
                    call_type: call_type.to_string(),
                    call_version: "V1".to_string(),
                    task_queue: task_queue.to_string(),
                    ..Default::default()
                })),
            })
            .await?;
        Ok(())
    }

    pub async fn call_v1<O: DeserializeOwned>(
        &mut self,
        input: Option<Payload>,
        call_type: &str,
        task_queue: &str,
    ) -> anyhow::Result<Option<O>> {
        let result = self
            .inner
            .call(CallVersion {
                version: Some(immortal::call_version::Version::V1(CallV1 {
                    input,
                    call_type: call_type.to_string(),
                    call_version: "V1".to_string(),
                    task_queue: task_queue.to_string(),
                    ..Default::default()
                })),
            })
            .await?;
        match result.into_inner().version.unwrap() {
            immortal::call_result_version::Version::V1(v1) => match v1.status {
                Some(x) => match x {
                    call_result_v1::Status::Completed(x) => {
                        if let Some(result) = x.result {
                            Ok(Some(result.to()?))
                        } else {
                            Ok(None)
                        }
                    }
                    call_result_v1::Status::Failed(x) => Err(anyhow!("{:#?}", x)),
                    call_result_v1::Status::Cancelled(x) => Err(anyhow!("{:#?}", x)),
                },
                None => Err(anyhow!("Call returned empty status")),
            },
        }
    }

    pub async fn notify_v1(
        &mut self,
        input: Option<Payload>,
        call_type: &str,
        task_queues: Vec<&str>,
    ) -> Result<(), tonic::Status> {
        let _result = self
            .inner
            .notify(NotifyVersion {
                version: Some(immortal::notify_version::Version::V1(NotifyV1 {
                    input,
                    notify_type: call_type.to_string(),
                    notify_version: "V1".to_string(),
                    task_queues: task_queues.iter().map(|f| f.to_string()).collect(),
                    ..Default::default()
                })),
            })
            .await?;
        Ok(())
    }
}

pub mod immortal {
    tonic::include_proto!("immortal");
    use serde::{de::DeserializeOwned, Serialize};

    use super::failure::{failure, CanceledFailureInfo, Failure as APIFailure};

    use crate::{
        common::{Payload, Payloads},
        immortal::activity_result_v1::Status,
    };

    impl Payloads {
        pub fn new<O>(data: Vec<&O>) -> Self
        where
            O: Serialize,
        {
            Self {
                payloads: data.iter().map(|d| Payload::new(d)).collect(),
            }
        }
        pub fn to<O>(&self) -> anyhow::Result<Vec<O>>
        where
            O: DeserializeOwned + Clone + Serialize,
        {
            let mut data = vec![];
            for payload in &self.payloads {
                let serialized: O = payload.to()?;
                data.push(serialized);
            }
            Ok(data)
        }
    }

    impl Payload {
        pub fn new<O>(data: &O) -> Self
        where
            O: Serialize,
        {
            Self {
                data: serde_json::to_vec(data).unwrap(),
                metadata: Default::default(),
            }
        }
        pub fn to<O>(&self) -> anyhow::Result<O>
        where
            O: DeserializeOwned,
        {
            Ok(serde_json::from_slice(&self.data)?)
        }
    }

    impl CallResultV1 {
        pub const fn ok(call_id: String, call_run_id: String, result: Option<Payload>) -> Self {
            Self {
                call_id,
                call_run_id,
                status: Some(call_result_v1::Status::Completed(Success { result })),
            }
        }

        pub fn fail(call_id: String, call_run_id: String, fail: super::failure::Failure) -> Self {
            Self {
                call_id,
                call_run_id,
                status: Some(call_result_v1::Status::Failed(Failure {
                    failure: Some(fail),
                })),
            }
        }

        pub fn cancel_from_details(
            call_id: String,
            call_run_id: String,

            payload: Option<Vec<Vec<u8>>>,
        ) -> Self {
            Self {
                call_id,
                call_run_id,

                status: Some(call_result_v1::Status::Cancelled(
                    Cancellation::from_details(payload),
                )),
            }
        }

        // pub const fn will_complete_async(activity_id: String, activity_run_id: String) -> Self {
        //     Self {
        //         activity_id,
        //         activity_run_id,
        //         status: Some(Status::WillCompleteAsync(WillCompleteAsync {})),
        //     }
        // }

        pub fn is_cancelled(&self) -> bool {
            matches!(self.status, Some(call_result_v1::Status::Cancelled(_)))
        }
    }

    impl ActivityResultV1 {
        pub const fn ok(
            workflow_id: String,
            activity_id: String,
            activity_run_id: String,
            result: Option<Payload>,
        ) -> Self {
            Self {
                workflow_id,
                activity_id,
                activity_run_id,
                status: Some(Status::Completed(Success { result })),
            }
        }

        pub fn fail(
            workflow_id: String,
            activity_id: String,
            activity_run_id: String,
            fail: super::failure::Failure,
        ) -> Self {
            Self {
                workflow_id,
                activity_id,
                activity_run_id,
                status: Some(Status::Failed(Failure {
                    failure: Some(fail),
                })),
            }
        }

        pub fn cancel_from_details(
            workflow_id: String,
            activity_id: String,
            activity_run_id: String,
            payload: Option<Vec<Vec<u8>>>,
        ) -> Self {
            Self {
                workflow_id,
                activity_id,
                activity_run_id,
                status: Some(Status::Cancelled(Cancellation::from_details(payload))),
            }
        }

        // pub const fn will_complete_async(activity_id: String, activity_run_id: String) -> Self {
        //     Self {
        //         activity_id,
        //         activity_run_id,
        //         status: Some(Status::WillCompleteAsync(WillCompleteAsync {})),
        //     }
        // }

        pub fn is_cancelled(&self) -> bool {
            matches!(self.status, Some(Status::Cancelled(_)))
        }
    }

    impl Cancellation {
        /// Create a cancellation result from some payload. This is to be used when telling Core
        /// that an activity completed as cancelled.
        pub fn from_details(details: Option<Vec<Vec<u8>>>) -> Self {
            Cancellation {
                failure: Some(APIFailure {
                    message: "Activity cancelled".to_string(),
                    failure_info: Some(failure::FailureInfo::CanceledFailureInfo(
                        CanceledFailureInfo {
                            details: details.unwrap_or_default(),
                        },
                    )),
                    ..Default::default()
                }),
            }
        }
    }
}
pub mod common {
    tonic::include_proto!("common");
}
pub mod failure {
    tonic::include_proto!("failure");

    use failure::FailureInfo;

    use super::enums::TimeoutType;

    impl Failure {
        pub fn is_timeout(&self) -> Option<TimeoutType> {
            match &self.failure_info {
                Some(FailureInfo::TimeoutFailureInfo(ti)) => Some(ti.timeout_type()),
                _ => None,
            }
        }

        pub fn application_failure(message: String, non_retryable: bool) -> Self {
            Self {
                message,
                failure_info: Some(FailureInfo::ApplicationFailureInfo(
                    ApplicationFailureInfo {
                        non_retryable,
                        ..Default::default()
                    },
                )),
                ..Default::default()
            }
        }

        pub fn application_failure_from_error(ae: anyhow::Error, non_retryable: bool) -> Self {
            Self {
                failure_info: Some(FailureInfo::ApplicationFailureInfo(
                    ApplicationFailureInfo {
                        non_retryable,
                        ..Default::default()
                    },
                )),
                ..ae.chain()
                    .rfold(None, |cause, e| {
                        Some(Self {
                            message: e.to_string(),
                            cause: cause.map(Box::new),
                            ..Default::default()
                        })
                    })
                    .unwrap_or_default()
            }
        }

        /// Extracts an ApplicationFailureInfo from a Failure instance if it exists
        pub fn maybe_application_failure(&self) -> Option<&ApplicationFailureInfo> {
            if let Failure {
                failure_info: Some(FailureInfo::ApplicationFailureInfo(f)),
                ..
            } = self
            {
                Some(f)
            } else {
                None
            }
        }
    }
}
pub mod enums {
    tonic::include_proto!("enums");
}
pub mod models;
