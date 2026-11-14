//! Record the top-left 400x300 (monitor units) of the primary monitor to MP4:
//! `cargo run -p snapit-recorder --example record_video -- out.mp4 3`

use std::time::Duration;

use snapit_capture::Region;
use snapit_recorder::{RecordFormat, RecordOptions, Recorder};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let output = args.next().unwrap_or_else(|| "snapit-test.mp4".into());
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);
    // Passing a width exercises the same downscale path the overlay asks for.
    let max_width: Option<u32> = args.next().and_then(|s| s.parse().ok());
    let monitor = snapit_capture::monitors()?
        .into_iter()
        .find(|m| m.is_primary)
        .ok_or_else(|| anyhow::anyhow!("no primary monitor"))?;

    let options = RecordOptions::default()
        .with_format(RecordFormat::Mp4)
        .with_max_duration(Duration::from_secs(30))
        .with_max_width(max_width);
    let recorder = Recorder::start(
        monitor.id,
        Region::new(0., 0., 400., 300.),
        options,
        &output,
    )?;
    std::thread::sleep(Duration::from_secs(seconds));
    let summary = recorder.stop()?;
    println!(
        "{} frames, {:.1}s, {:?} -> {}",
        summary.frames,
        summary.duration.as_secs_f32(),
        summary.format,
        summary.path.display()
    );
    Ok(())
}
