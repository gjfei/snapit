use std::{
    borrow::Cow,
    fs::{File, OpenOptions},
    io::{BufWriter, Seek, SeekFrom, Write},
    sync::Arc,
};

use anyhow::{Context as _, Result, anyhow};
use png::{BitDepth, ColorType, Compression, DisposeOp, Encoder};
use snapit_capture::RgbaImage;

use super::{DirtyRect, PendingFrame, dirty_rect, video};

pub(crate) fn encode(
    receiver: std::sync::mpsc::Receiver<PendingFrame>,
    file: File,
    max_width: Option<u32>,
) -> Result<()> {
    let started = std::time::Instant::now();
    let mut raw_writer = ApngWriter::new(BufWriter::new(file));
    let mut pending = receiver.into_iter();
    let first = loop {
        match pending.next() {
            Some(frame) => {
                if let Some(scaled) = prepared(&frame.image, max_width) {
                    break (frame, scaled);
                }
            }
            None => {
                raw_writer.flush().context("failed to flush APNG file")?;
                return Ok(());
            }
        }
    };
    let (first, first_image) = first;
    let (width, height) = first_image.dimensions();
    let mut encoder = Encoder::new(&mut raw_writer, width, height);
    encoder.set_depth(BitDepth::Eight);
    encoder.set_color(ColorType::Rgba);
    encoder.set_compression(Compression::Fast);
    // The total is unknown while streaming; patch acTL after the final frame.
    encoder
        .set_animated(u32::MAX, 0)
        .context("failed to enable APNG animation")?;
    encoder.validate_sequence(false);
    encoder
        .set_sep_def_img(false)
        .context("failed to configure the APNG default image")?;
    let mut writer = encoder
        .write_header()
        .context("failed to write the APNG header")?;
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    write_frame(&mut writer, &first_image, None, first.duration, &mut pixels)?;
    let mut previous = first_image;
    let mut encoded = 1u32;
    let mut diff_time = std::time::Duration::ZERO;
    let mut write_time = std::time::Duration::ZERO;

    for frame in pending {
        let Some(image) = prepared(&frame.image, max_width) else {
            continue;
        };
        if image.dimensions() != (width, height) {
            continue;
        }
        let tick = std::time::Instant::now();
        let changed = dirty_rect(&previous, &image).unwrap_or(DirtyRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        });
        diff_time += tick.elapsed();
        let tick = std::time::Instant::now();
        write_frame(
            &mut writer,
            &image,
            Some(changed),
            frame.duration,
            &mut pixels,
        )?;
        write_time += tick.elapsed();
        encoded += 1;
        previous = image;
    }

    writer
        .finish()
        .context("failed to finish the APNG container")?;
    raw_writer.patch_frame_count(encoded)?;
    log::info!(
        target: "timing",
        "apng writer: {encoded} frames, diff {:.1} ms, encode {:.1} ms, total {:.1} ms",
        super::ms(diff_time),
        super::ms(write_time),
        super::ms(started.elapsed())
    );
    Ok(())
}

fn prepared(image: &Arc<RgbaImage>, max_width: Option<u32>) -> Option<Arc<RgbaImage>> {
    match video::prepare(image, max_width) {
        Cow::Borrowed(_) => Some(Arc::clone(image)),
        Cow::Owned(image) => Some(Arc::new(image)),
    }
}

fn write_frame<W: Write>(
    writer: &mut png::Writer<W>,
    image: &RgbaImage,
    region: Option<DirtyRect>,
    duration: std::time::Duration,
    pixels: &mut Vec<u8>,
) -> Result<()> {
    let (x, y, width, height) = region.map_or((0, 0, image.width(), image.height()), |rect| {
        (rect.x, rect.y, rect.width, rect.height)
    });
    let (numerator, denominator) = delay(duration);
    // Clear retained offsets before png::Writer validates a new frame size.
    writer
        .reset_frame_position()
        .context("failed to reset APNG frame position")?;
    writer
        .set_frame_dimension(width, height)
        .context("failed to set APNG frame dimensions")?;
    writer
        .set_frame_position(x, y)
        .context("failed to set APNG frame position")?;
    writer
        .set_dispose_op(DisposeOp::None)
        .context("failed to set APNG disposal")?;
    writer
        .set_blend_op(png::BlendOp::Source)
        .context("failed to set APNG blending")?;
    writer
        .set_frame_delay(numerator, denominator)
        .context("failed to set APNG frame delay")?;
    let row_bytes = width as usize * 4;
    pixels.clear();
    pixels.reserve(width as usize * height as usize * 4);
    for row in y..y + height {
        let start = row as usize * image.width() as usize * 4 + x as usize * 4;
        pixels.extend_from_slice(&image.as_raw()[start..start + row_bytes]);
    }
    writer
        .write_image_data(&pixels[..])
        .context("failed to write APNG pixels")?;
    Ok(())
}

fn delay(duration: std::time::Duration) -> (u16, u16) {
    let milliseconds = duration.as_millis().max(1);
    if milliseconds <= u16::MAX as u128 {
        (milliseconds as u16, 1_000)
    } else if milliseconds <= u16::MAX as u128 * 10 {
        ((milliseconds / 10) as u16, 10)
    } else {
        ((milliseconds / 1_000).clamp(1, u16::MAX as u128) as u16, 1)
    }
}

struct ApngWriter<W: Write> {
    inner: W,
    written: u64,
    prefix: Vec<u8>,
    animation_control: Option<AnimationControlPatch>,
}

struct AnimationControlPatch {
    frames: u64,
    crc: u64,
}

impl<W: Write> ApngWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            written: 0,
            prefix: Vec::new(),
            animation_control: None,
        }
    }

    fn observe(&mut self, bytes: &[u8]) {
        if self.animation_control.is_some() {
            return;
        }
        self.prefix.extend_from_slice(bytes);
        if let Some(at) = self.prefix.windows(4).position(|chunk| chunk == b"acTL") {
            let start = self.written - self.prefix.len() as u64;
            let frames = start + at as u64 + 4;
            self.animation_control = Some(AnimationControlPatch {
                frames,
                crc: frames + 8,
            });
            self.prefix.clear();
        }
    }
}

impl<W: Write> Write for ApngWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let count = self.inner.write(bytes)?;
        self.written += count as u64;
        self.observe(&bytes[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl ApngWriter<BufWriter<File>> {
    fn patch_frame_count(&mut self, frames: u32) -> Result<()> {
        let offset = self
            .animation_control
            .as_ref()
            .ok_or_else(|| anyhow!("APNG animation control chunk was not written"))?;
        self.inner
            .seek(SeekFrom::Start(offset.frames))
            .context("failed to seek to the APNG frame count")?;
        self.inner
            .write_all(&frames.to_be_bytes())
            .context("failed to patch the APNG frame count")?;
        self.inner
            .seek(SeekFrom::Start(offset.crc))
            .context("failed to seek to the APNG frame count CRC")?;
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(b"acTL");
        hasher.update(&frames.to_be_bytes());
        hasher.update(&0u32.to_be_bytes());
        self.inner
            .write_all(&hasher.finalize().to_be_bytes())
            .context("failed to patch the APNG frame count CRC")?;
        self.inner
            .seek(SeekFrom::End(0))
            .context("failed to restore the APNG write position")?;
        self.inner
            .flush()
            .context("failed to flush the APNG frame count")?;
        Ok(())
    }
}

pub(crate) fn create(output: &std::path::Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .with_context(|| format!("failed to create {}", output.display()))
}

#[cfg(test)]
mod tests {
    use super::super::{FrameData, FrameSink, RecordFormat};
    use super::*;
    use png::{Decoder, Transformations};
    use std::io::{BufReader, Cursor};

    #[test]
    fn frame_count_is_patched_from_the_chunk_type() {
        let mut writer = ApngWriter::new(Cursor::new(Vec::new()));
        writer.write_all(b"\x89PNG\r\n\x1a\nacTLxxxx").unwrap();
        assert_eq!(
            writer.animation_control.as_ref().map(|patch| patch.frames),
            Some(12)
        );
    }

    #[test]
    fn delays_are_nonzero_and_millisecond_precise() {
        assert_eq!(delay(std::time::Duration::ZERO), (1, 1_000));
        assert_eq!(delay(std::time::Duration::from_millis(83)), (83, 1_000));
    }

    #[test]
    fn moving_dirty_rects_do_not_reuse_previous_offsets() {
        let path = std::env::temp_dir().join(format!(
            "snapit-apng-moving-rect-{}.png",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut sink = super::super::AnimationSink::new(
            &path,
            &super::super::RecordOptions::default(),
            RecordFormat::Apng,
            4 * 3 * 4,
            None,
            (0, 0),
        )
        .unwrap();
        let black = image::Rgba([0, 0, 0, 255]);
        let white = image::Rgba([255, 255, 255, 255]);
        for (index, pixels) in [
            [black, black, black, black],
            [black, black, black, white],
            [white, black, black, black],
        ]
        .into_iter()
        .enumerate()
        {
            let image = RgbaImage::from_fn(4, 1, |x, _| pixels[x as usize]);
            sink.push(
                FrameData::Sdr(Arc::new(image)),
                std::time::Duration::from_millis(index as u64 * 100),
            )
            .unwrap();
        }
        Box::new(sink)
            .finish(std::time::Duration::from_millis(300))
            .unwrap();

        let file = File::open(&path).unwrap();
        let mut decoder = Decoder::new(BufReader::new(file));
        decoder.set_transformations(Transformations::IDENTITY);
        let mut reader = decoder.read_info().unwrap();
        assert_eq!(reader.info().animation_control.unwrap().num_frames, 3);
        let first = *reader.info().frame_control.as_ref().unwrap();
        assert_eq!(first.width, 4);
        for _ in 0..first.height {
            reader.next_row().unwrap().unwrap();
        }
        for expected in [(3, 1), (0, 4)] {
            let control = *reader.next_frame_info().unwrap();
            assert_eq!((control.x_offset, control.width), expected);
            for _ in 0..control.height {
                reader.next_row().unwrap().unwrap();
            }
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn writes_lossless_partial_animation_frames() {
        let path = std::env::temp_dir().join(format!(
            "snapit-apng-test-{}.png",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut sink = super::super::AnimationSink::new(
            &path,
            &super::super::RecordOptions::default(),
            RecordFormat::Apng,
            4 * 3 * 4,
            None,
            (0, 0),
        )
        .unwrap();
        for (index, color) in [
            image::Rgba([10, 20, 30, 255]),
            image::Rgba([10, 20, 30, 255]),
            image::Rgba([200, 100, 50, 255]),
        ]
        .into_iter()
        .enumerate()
        {
            sink.push(
                FrameData::Sdr(Arc::new(RgbaImage::from_pixel(4, 3, color))),
                std::time::Duration::from_millis(index as u64 * 100),
            )
            .unwrap();
        }
        Box::new(sink)
            .finish(std::time::Duration::from_millis(300))
            .unwrap();

        let file = File::open(&path).unwrap();
        let mut decoder = Decoder::new(BufReader::new(file));
        decoder.set_transformations(Transformations::IDENTITY);
        let mut reader = decoder.read_info().unwrap();
        assert_eq!(reader.info().animation_control.unwrap().num_frames, 3);
        assert_eq!(reader.info().animation_control.unwrap().num_plays, 0);
        for (expected, (width, height, color)) in [
            (4, 3, image::Rgba([10, 20, 30, 255])),
            (1, 1, image::Rgba([10, 20, 30, 255])),
            (4, 3, image::Rgba([200, 100, 50, 255])),
        ]
        .into_iter()
        .enumerate()
        {
            let control = if expected == 0 {
                *reader.info().frame_control.as_ref().unwrap()
            } else {
                *reader.next_frame_info().unwrap()
            };
            assert_eq!((control.width, control.height), (width, height));
            assert_eq!(control.delay_num, 100);
            assert_eq!(control.delay_den, 1_000);
            let mut row = vec![0; reader.output_line_size(control.width).unwrap()];
            for pixel in row.as_chunks_mut::<4>().0 {
                pixel.copy_from_slice(&color.0);
            }
            for _ in 0..control.height {
                let pixels = reader.next_row().unwrap().unwrap();
                assert_eq!(pixels.data(), row.as_slice());
            }
        }
        std::fs::remove_file(path).unwrap();
    }
}
