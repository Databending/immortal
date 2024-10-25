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
        pub fn to<O>(&self) -> Vec<O>
        where
            O: DeserializeOwned,
        {
            self.payloads.iter().map(|p| p.to()).collect()
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
        pub fn to<O>(&self) -> O
        where
            O: DeserializeOwned,
        {
            serde_json::from_slice(&self.data).unwrap()
        }
    }

    impl ActivityResultV1 {
        pub const fn ok(
            activity_id: String,
            activity_run_id: String,
            result: Option<Payload>,
        ) -> Self {
            Self {
                activity_id,
                activity_run_id,
                status: Some(Status::Completed(Success { result })),
            }
        }

        pub fn fail(
            activity_id: String,
            activity_run_id: String,
            fail: super::failure::Failure,
        ) -> Self {
            Self {
                activity_id,
                activity_run_id,
                status: Some(Status::Failed(Failure {
                    failure: Some(fail),
                })),
            }
        }

        pub fn cancel_from_details(
            activity_id: String,
            activity_run_id: String,
            payload: Option<Vec<Vec<u8>>>,
        ) -> Self {
            Self {
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
