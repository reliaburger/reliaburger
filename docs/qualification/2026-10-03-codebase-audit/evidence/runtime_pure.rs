extern crate reliaburger;
use reliaburger::meat::cron::CronSchedule;
use reliaburger::config::Config;
fn main() {
    println!("cron_overflow_panics={}",std::panic::catch_unwind(||CronSchedule::parse("59/255 * * * *")).is_err());
    for target in ["0%","-20%","NaN","inf"] {
        let text=format!("[app.web]\nimage = \"busybox\"\n[app.web.autoscale]\nmetric = \"cpu\"\ntarget = \"{target}\"\nmin = 1\nmax = 5\n");
        let config=Config::parse(&text).unwrap();
        println!("target {target} validates: {}", config.validate().is_ok());
    }
    let config=Config::parse("[app.db]\nimage=\"busybox\"\n[[app.db.volumes]]\npath=\"/data.a\"\nsize=\"128Mi\"\n[[app.db.volumes]]\npath=\"/data.b\"\nsize=\"128Mi\"\n").unwrap();
    println!("colliding_volumes_validate={}",config.validate().is_ok());
    for volume in &config.app["db"].volumes { println!("{} -> {}",volume.path.display(),volume.path.with_extension("img").display()); }
}
