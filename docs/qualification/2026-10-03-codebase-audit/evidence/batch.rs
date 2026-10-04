extern crate reliaburger;
extern crate tokio;
extern crate axum;
extern crate tower;
extern crate http_body_util;
use tower::ServiceExt;
use http_body_util::BodyExt;
use reliaburger::bun::agent::{AgentCommand,ApplyEvent};
fn main(){tokio::runtime::Runtime::new().unwrap().block_on(async{
 let (tx,mut rx)=tokio::sync::mpsc::channel(64);
 tokio::spawn(async move {while let Some(cmd)=rx.recv().await {match cmd {
   AgentCommand::Deploy{config,events}=>{
      println!("AGENT GOT jobs {:?}",config.job.keys().collect::<Vec<_>>());
      tokio::spawn(async move {tokio::time::sleep(std::time::Duration::from_secs(2)).await; let _=events.send(ApplyEvent::Complete{created:config.job.len(),instances:vec![]}).await; println!("fake new Deploy completed");});
   },
   AgentCommand::Status{response}=>{let _=response.send(vec![reliaburger::bun::agent::InstanceStatus{id:"old-run".into(),app_name:"duplicate".into(),namespace:"default".into(),state:"stopped".into(),restart_count:0,host_port:None,exit_code:Some(0),pid:None,runtime_unknown:false,status_age_ms:None}]);},
   _=>{}
 }}});
 let store=reliaburger::sesame::auth::new_token_store();
 let created=reliaburger::sesame::token::create_token("audit",reliaburger::sesame::types::ApiRole::Deployer,reliaburger::sesame::types::TokenScope {apps:Some(vec!["allowed".into()]),namespaces:Some(vec!["allowedns".into()])},None).unwrap();
 store.write().await.push(created.token);
 let router=reliaburger::bun::api::router(tx,None,None,None,None,None,None,Some(store),None,None,None,None,0,None);
 let bodies=[
  r#"{"jobs":[{"name":"forbidden","namespace":"forbiddenns","spec":{"script":"echo hi"}}]}"#,
  r#"{"jobs":[{"name":"duplicate","spec":{"script":"echo first"}},{"name":"duplicate","spec":{"script":"echo second"}}]}"#,
  r#"{"jobs":[{"name":"duplicate","spec":{"script":"sleep 20; exit 1"}}]}"#,
 ];
 for body in bodies {
   let req=axum::http::Request::builder().method("POST").uri("/v1/batch").header("content-type","application/json").header("authorization",format!("Bearer {}",created.plaintext)).body(axum::body::Body::from(body)).unwrap();
   let resp=router.clone().oneshot(req).await.unwrap();let code=resp.status();let b=resp.into_body().collect().await.unwrap().to_bytes();
   println!("response {code}: {}",String::from_utf8_lossy(&b));
 }
 tokio::time::sleep(std::time::Duration::from_millis(100)).await;
 for id in [2,3]{
  let req=axum::http::Request::builder().uri(format!("/v1/batch/{id}")).header("authorization",format!("Bearer {}",created.plaintext)).body(axum::body::Body::empty()).unwrap();
  let resp=router.clone().oneshot(req).await.unwrap(); let b=resp.into_body().collect().await.unwrap().to_bytes();println!("batch{id} before new deploy completed: {}",String::from_utf8_lossy(&b));
 }
});}
