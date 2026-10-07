use std::{collections::BTreeMap,path::PathBuf};
use reliaburger::mayo::{store::MayoStore,types::{MetricKey,Sample}};
#[tokio::main]
async fn main(){
let root=PathBuf::from(format!("/tmp/rb-mayo-collision-{}",std::process::id()));let remote=root.join("remote");std::fs::create_dir_all(&remote).unwrap();let url=format!("file://{}",remote.display());
let mut a=MayoStore::open(root.join("node-a"),Some(&url)).await.unwrap();let mut b=MayoStore::open(root.join("node-b"),Some(&url)).await.unwrap();
a.insert(&MetricKey::with_labels("cpu",BTreeMap::from([("node".into(),"a".into())])),Sample::at(100,11.0));b.insert(&MetricKey::with_labels("cpu",BTreeMap::from([("node".into(),"b".into())])),Sample::at(100,22.0));a.flush().await.unwrap();println!("after node a flush {:?}",a.query_sql("SELECT timestamp, metric_name, labels, value FROM metrics").await.unwrap());b.flush().await.unwrap();println!("after node b flush {:?}",a.query_sql("SELECT timestamp, metric_name, labels, value FROM metrics").await.unwrap());println!("remote files={:?}",std::fs::read_dir(&remote).unwrap().map(|r|r.unwrap().file_name()).collect::<Vec<_>>());std::fs::remove_dir_all(root).unwrap();
}
