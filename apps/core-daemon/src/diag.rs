use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

const EARLY_LOG_PATH_ENV: &str = "FLOWTILE_EARLY_LOG_PATH";
const TOUCHPAD_DUMP_PATH_ENV: &str = "FLOWTILE_TOUCHPAD_DUMP_PATH";

pub(crate) fn write_runtime_log(message: impl AsRef<str>) {
    let Some(path) = env::var_os(EARLY_LOG_PATH_ENV).map(PathBuf::from) else {
        return;
    };

    append_line(path, message.as_ref());
}

pub(crate) fn write_touchpad_dump(message: impl AsRef<str>) {
    let Some(path) = env::var_os(TOUCHPAD_DUMP_PATH_ENV).map(PathBuf::from) else {
        return;
    };

    append_line(path, message.as_ref());
}

fn append_line(path: PathBuf, message: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };

    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let _ = writeln!(file, "[{timestamp_ms}] {message}");
}
