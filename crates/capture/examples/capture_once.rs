//! Capture every monitor once and save PNGs: `cargo run -p snapit-capture --example capture_once [out_dir]`

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let out_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    for shot in snapit_capture::capture_all()? {
        let monitor = shot.monitor();
        let path = out_dir.join(format!("snapit-monitor-{}.png", monitor.id));
        snapit_capture::save_png(shot.image(), &path)?;
        let image = shot.image();
        let lit = image
            .pixels()
            .filter(|p| p.0[..3].iter().any(|&c| c > 16))
            .count();
        println!(
            "{} ({}): {}x{} units, {}x{} px, scale {}, non-black {:.1}% -> {}",
            monitor.name,
            monitor.id,
            monitor.width,
            monitor.height,
            image.width(),
            image.height(),
            monitor.scale_factor,
            100.0 * lit as f64 / (image.width() as f64 * image.height() as f64),
            path.display()
        );
    }
    Ok(())
}
