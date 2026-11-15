//! MP4 writing through Media Foundation's sink writer, which drives the
//! system H.264 encoder (hardware where the driver exposes one) and muxes the
//! container.
//!
//! The sink writer is created in a multithreaded apartment and used from the
//! capture thread. Objects created in the MTA are agile, so no marshalling is
//! needed; both threads just have to have joined the MTA, which `ensure_com`
//! does.

use std::{path::Path, time::Duration};

use anyhow::{Context as _, Result, bail};
use snapit_capture::RgbaImage;
use windows::Win32::Media::MediaFoundation::{
    IMFSinkWriter, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_VERSION,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL,
    MFMediaType_Video, MFSTARTUP_LITE, MFShutdown, MFStartup, MFVideoFormat_H264,
    MFVideoFormat_RGB32, MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::HSTRING;

use super::prepare;
use crate::{FrameSink, RecordOptions};

/// Media Foundation counts time in 100 ns units.
const TICKS_PER_SECOND: i64 = 10_000_000;

pub(crate) struct Mp4Sink {
    writer: IMFSinkWriter,
    stream: u32,
    width: u32,
    height: u32,
    max_width: Option<u32>,
    /// Nominal frame duration, used when a still stretch has to guess.
    frame_ticks: i64,
    last: Option<(RgbaImage, Duration)>,
    frames: usize,
}

/// The writer lives on the capture thread after construction and Media
/// Foundation requires nothing else of it, so moving it there is safe.
unsafe impl Send for Mp4Sink {}

impl Mp4Sink {
    pub(crate) fn new(path: &Path, options: &RecordOptions, probe: &RgbaImage) -> Result<Self> {
        ensure_com()?;
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }.context("Media Foundation unavailable")?;

        let first = prepare(probe, options.max_width);
        let (width, height) = first.dimensions();
        let fps = options.fps.max(1);
        let bitrate = bitrate(width, height, options);

        let writer =
            unsafe { MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, None) }
                .with_context(|| format!("failed to open {}", path.display()))?;

        // Output: what the container stores.
        let output = unsafe { MFCreateMediaType() }?;
        unsafe {
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            // The encoder rejects an output type without this (0xC00D36B4),
            // even though interlace is only meaningful for the input.
            output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            output.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        }
        let stream = unsafe { writer.AddStream(&output) }?;

        // Input: the frames handed to the encoder, packed BGRA.
        let input = unsafe { MFCreateMediaType() }?;
        unsafe {
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            // Negative stride marks a bottom-up DIB, which is what RGB32 means.
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, (width * 4).wrapping_neg())?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            writer.SetInputMediaType(stream, &input, None)?;
            writer.BeginWriting()?;
        }

        Ok(Self {
            writer,
            stream,
            width,
            height,
            max_width: options.max_width,
            frame_ticks: TICKS_PER_SECOND / fps as i64,
            last: None,
            frames: 0,
        })
    }

    fn append_scaled(&mut self, frame: &RgbaImage, at: Duration) -> Result<()> {
        let (width, height) = frame.dimensions();
        if width != self.width || height != self.height {
            bail!(
                "frame is {width}x{height}, encoder expects {}x{}",
                self.width,
                self.height
            );
        }

        let stride = (width * 4) as usize;
        let buffer = unsafe { MFCreateMemoryBuffer((stride * height as usize) as u32) }?;
        {
            let mut raw: *mut u8 = std::ptr::null_mut();
            unsafe { buffer.Lock(&mut raw, None, None) }?;
            if raw.is_null() {
                bail!("Media Foundation returned an empty frame buffer");
            }
            // RGBA in, BGRA out, rows bottom-up: RGB32 is a DIB.
            unsafe {
                for y in 0..height as usize {
                    let row = raw.add((height as usize - 1 - y) * stride);
                    let (src, _) = frame.as_raw()[y * stride..(y + 1) * stride].as_chunks::<4>();
                    let dst = std::slice::from_raw_parts_mut(row as *mut [u8; 4], width as usize);
                    for (into, from) in dst.iter_mut().zip(src) {
                        *into = [from[2], from[1], from[0], from[3]];
                    }
                }
            }
            unsafe { buffer.Unlock() }?;
        }
        unsafe { buffer.SetCurrentLength((stride * height as usize) as u32) }?;

        let sample = unsafe { MFCreateSample() }?;
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime((at.as_secs_f64() * TICKS_PER_SECOND as f64) as i64)?;
            sample.SetSampleDuration(self.frame_ticks.max(1))?;
            self.writer.WriteSample(self.stream, &sample)?;
        }
        self.frames += 1;
        Ok(())
    }
}

impl FrameSink for Mp4Sink {
    fn push(&mut self, frame: &RgbaImage, at: Duration) -> Result<()> {
        // The capture thread has to be in the MTA as well; the call is a no-op
        // when it already is.
        ensure_com()?;
        // Keep the scaled frame: finish() needs one more sample, and cloning
        // the full-resolution capture (or scaling it again) would mean another
        // pass over four times as many pixels.
        let scaled = prepare(frame, self.max_width).into_owned();
        self.append_scaled(&scaled, at)?;
        self.last = Some((scaled, at));
        Ok(())
    }

    fn finish(mut self: Box<Self>, end: Duration) -> Result<()> {
        // A container ends at its last sample, so the closing still stretch
        // needs a frame of its own or the video looks cut short.
        if let Some((frame, at)) = self.last.take()
            && end > at
        {
            // Sample times must strictly increase, or the sink can reject the
            // closing sample on a very short recording.
            let hold = end
                .saturating_sub(Duration::from_millis(1))
                .max(at + Duration::from_millis(2));
            self.append_scaled(&frame, hold)?;
        }
        unsafe {
            self.writer.Finalize()?;
            let _ = MFShutdown();
        }
        log::info!("encoded {} frames to MP4", self.frames);
        Ok(())
    }
}

/// Bits per second for a screen recording: screen content compresses far
/// better than camera footage, so this sits below the usual 0.1 bpp ladder.
fn bitrate(width: u32, height: u32, options: &RecordOptions) -> u32 {
    let pixels = width as f64 * height as f64;
    let quality = (options.quality.max(1) as f64) / 90.0;
    let bits = pixels * options.fps.max(1) as f64 * 0.06 * quality;
    bits.clamp(500_000.0, 25_000_000.0) as u32
}

/// Media Foundation packs two 32-bit attributes (size, ratio) into one
/// `UINT64`: high half first.
fn pack(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

fn ensure_com() -> Result<()> {
    // S_FALSE means this thread was already initialised, which is fine.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    Ok(())
}
