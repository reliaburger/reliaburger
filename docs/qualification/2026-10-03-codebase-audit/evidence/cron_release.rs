#[path="../../../../src/meat/cron.rs"]
mod cron;
fn main(){
 let s=cron::CronSchedule::parse("59/255 * * * *").unwrap();
 let at=time::OffsetDateTime::from_unix_timestamp(0).unwrap();
 let fired:Vec<_>=(0..60).filter(|m|s.matches(at+time::Duration::minutes(*m))).collect();
 println!("release minutes matched by 59/255: {fired:?}");
}
