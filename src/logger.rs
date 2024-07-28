use env_logger::{Builder, Env};
use std::io::Write;

pub fn init_logger() {
    Builder::from_env(Env::default().default_filter_or("debug"))
        .format(|buf, record| {
            writeln!(buf, "{}: {}", record.level(), record.args())
        })
        .init();
}
