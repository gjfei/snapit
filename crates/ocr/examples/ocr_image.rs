//! Run the default OCR engine on an image: `cargo run -p snapit-ocr --example ocr_image -- <image.png>`

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: ocr_image <image.png>"))?;
    let image = image::open(&path)?.to_rgba8();
    let engine = snapit_ocr::default_engine()?;
    let started = std::time::Instant::now();
    let blocks = engine.recognize(&image)?;
    println!(
        "{} found {} lines in {:?}",
        engine.name(),
        blocks.len(),
        started.elapsed()
    );
    for block in &blocks {
        let b = block.bounds;
        println!(
            "[{:.0},{:.0} {:.0}x{:.0}] {}",
            b.x, b.y, b.width, b.height, block.text
        );
    }
    Ok(())
}
