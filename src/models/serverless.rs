use std::sync::Arc;

use futures::lock::Mutex;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{transport::Server, Request, Response, Status};

use crate::immortal::{
    activity_result_version,
    immortal_serverless_action_v1::Action,
    immortal_serverless_action_version,
    immortal_serverless_server::{ImmortalServerless, ImmortalServerlessServer},
    start_activity_options_version, ActivityResultVersion, ImmortalServerlessActionV1,
    ImmortalServerlessActionVersion, StartActivityOptionsVersion,
};

use super::{activity::AppData, worker::Worker};

// #[derive(Debug, Clone)]
pub struct ImmortalServerlessService {
    worker: Arc<Mutex<Worker>>,
    safe_app_data: Arc<AppData>,
}

impl ImmortalServerlessService {}

#[tonic::async_trait]
impl ImmortalServerless for ImmortalServerlessService {
    type RunStream = ReceiverStream<Result<ImmortalServerlessActionVersion, Status>>;
    async fn run(
        &self,
        request: Request<StartActivityOptionsVersion>,
    ) -> Result<Response<Self::RunStream>, Status> {
        println!("Got a request: {:?}", request);
        let (tx, rx) = mpsc::channel(4);

        let worker = Arc::clone(&self.worker);
        match request.into_inner().version.unwrap() {
            start_activity_options_version::Version::V1(options) => {
                let safe_app_data = self.safe_app_data.clone();

                tokio::spawn(async move {
                    let mut worker = worker.lock().await;
                    let handle = worker
                        .run_serverless_activity(
                            &options.workflow_id,
                            &options.activity_type,
                            &options.activity_id,
                            &options.activity_run_id,
                            options.activity_input.unwrap(),
                            &safe_app_data,
                        )
                        .await
                        .unwrap();
                    tx.send(Ok(ImmortalServerlessActionVersion {
                        version: Some(immortal_serverless_action_version::Version::V1(
                            ImmortalServerlessActionV1 {
                                action: Some(Action::Result(ActivityResultVersion {
                                    version: Some(activity_result_version::Version::V1(handle)),
                                })),
                            },
                        )),
                    }))
                    .await
                    .unwrap();
                    // rx.close();
                });

                // let (tx, rx) = mpsc::channel(4);
                // tokio::spawn(async move {
                //     let res = worker.start_activity(options).await;
                //     tx.send(res).await.unwrap();
                // });
                // Ok(Response::new(ReceiverStream::new(rx)))
            }
        }

        // let act_handle = act.0.start_activity(
        //     payload,
        //     safe_app_data.clone(),
        //     activity_type,
        //     workflow_id.to_string(),
        //     activity_id.to_string(),
        //     activity_run_id.to_string(),
        // );
        // let handle = tokio::spawn(async move {
        //     let res = act_handle.await;
        //
        //     sender.send((wid.to_string(), aid, aid_run, res)).unwrap();
        // });
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

pub async fn main(
    worker: Worker,
    safe_app_data: Arc<AppData>,
) -> Result<(), Box<dyn std::error::Error>> {
    let port = std::env::var("PORT").unwrap_or("10001".to_string());
    let addr = format!("0.0.0.0:{port}").parse().unwrap();
    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<ImmortalServerlessServer<ImmortalServerlessService>>()
        .await;
    let svc = ImmortalServerlessServer::new(ImmortalServerlessService {
        worker: Arc::new(Mutex::new(worker)),
        safe_app_data,
    });
    Server::builder()
        .add_service(svc)
        .add_service(health_service)
        .serve(addr)
        .await?;
    Ok(())
}
