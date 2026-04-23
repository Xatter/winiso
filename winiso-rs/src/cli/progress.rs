use std::fmt::Write;

use indicatif::{ProgressBar, ProgressState, ProgressStyle};

fn mib_style(template: &str) -> ProgressStyle {
    ProgressStyle::default_bar()
        .with_key("mib_pos", |state: &ProgressState, w: &mut dyn Write| {
            let _ = write!(w, "{:.1} MiB", state.pos() as f64 / (1024.0 * 1024.0));
        })
        .with_key("mib_len", |state: &ProgressState, w: &mut dyn Write| {
            let _ = write!(
                w,
                "{:.1} MiB",
                state.len().unwrap_or(0) as f64 / (1024.0 * 1024.0)
            );
        })
        .with_key("mib_per_sec", |state: &ProgressState, w: &mut dyn Write| {
            let _ = write!(w, "{:.1} MiB/s", state.per_sec() / (1024.0 * 1024.0));
        })
        .template(template)
        .unwrap()
        .progress_chars("=> ")
}

pub fn transfer_bar(total: u64, msg: &str) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(mib_style("{msg} [{bar:40}] {mib_pos}/{mib_len} {mib_per_sec}"));
    pb.set_message(msg.to_string());
    pb
}

pub fn download_bar(total: u64) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(mib_style(
        "{msg} [{bar:40}] {mib_pos}/{mib_len} {mib_per_sec} ETA {eta}",
    ));
    pb
}

