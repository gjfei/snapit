//! MP4 writing through `AVAssetWriter`, which drives the system H.264 encoder
//! (VideoToolbox) and muxes the container. Frames are handed over as BGRA
//! `CVPixelBuffer`s, the format the encoder converts most cheaply.

use std::{path::Path, ptr::NonNull, time::Duration};

use anyhow::{Result, anyhow, bail};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread as _, Message};
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterInputPixelBufferAdaptor, AVFileTypeMPEG4,
    AVMediaTypeVideo, AVVideoAverageBitRateKey, AVVideoCodecKey, AVVideoCodecTypeH264,
    AVVideoCompressionPropertiesKey, AVVideoExpectedSourceFrameRateKey, AVVideoHeightKey,
    AVVideoWidthKey,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::CMTime;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSMutableDictionary, NSNumber, NSString, NSURL};
use snapit_capture::RgbaImage;

use super::prepare;
use crate::{FrameSink, RecordOptions};

/// The timescale presentation timestamps are expressed in; 600 is what
/// AVFoundation uses for the movie itself.
const TIMESCALE: i32 = 600;

pub(crate) struct Mp4Sink {
    writer: Retained<AVAssetWriter>,
    input: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    /// Output dimensions, after the logical-width cap.
    width: usize,
    height: usize,
    max_width: Option<u32>,
    /// Last encoded frame and its timestamp: a still tail is only visible in
    /// the file if something is written after it.
    last: Option<(RgbaImage, Duration)>,
    frames: usize,
}

impl Mp4Sink {
    pub(crate) fn new(path: &Path, options: &RecordOptions, probe: &RgbaImage) -> Result<Self> {
        let first = prepare(probe, options.max_width);
        let (width, height) = first.dimensions();
        let (width_usize, height_usize) = (width as usize, height as usize);

        let url = NSString::from_str(&path.to_string_lossy());
        let url = NSURL::fileURLWithPath(&url);
        let writer = unsafe {
            AVAssetWriter::initWithURL_fileType_error(
                AVAssetWriter::alloc(),
                &url,
                AVFileTypeMPEG4.ok_or_else(|| anyhow!("MPEG-4 file type unavailable"))?,
            )
        }
        .map_err(|err| anyhow!("failed to open {}: {err}", path.display()))?;

        let settings = NSMutableDictionary::<NSString, AnyObject>::new();
        let compression = NSMutableDictionary::<NSString, AnyObject>::new();
        unsafe {
            compression.insert(
                AVVideoAverageBitRateKey.ok_or_else(|| anyhow!("missing bit rate key"))?,
                any(&*NSNumber::new_u32(bitrate(width, height, options))),
            );
            compression.insert(
                AVVideoExpectedSourceFrameRateKey
                    .ok_or_else(|| anyhow!("missing frame rate key"))?,
                any(&*NSNumber::new_u32(options.fps.max(1))),
            );
            settings.insert(
                AVVideoCodecKey.ok_or_else(|| anyhow!("missing codec key"))?,
                any(AVVideoCodecTypeH264.ok_or_else(|| anyhow!("H.264 unavailable"))?),
            );
            settings.insert(
                AVVideoWidthKey.ok_or_else(|| anyhow!("missing width key"))?,
                any(&*NSNumber::new_u32(width)),
            );
            settings.insert(
                AVVideoHeightKey.ok_or_else(|| anyhow!("missing height key"))?,
                any(&*NSNumber::new_u32(height)),
            );
            settings.insert(
                AVVideoCompressionPropertiesKey
                    .ok_or_else(|| anyhow!("missing compression key"))?,
                any(&*compression),
            );
        }

        let input = unsafe {
            AVAssetWriterInput::initWithMediaType_outputSettings_sourceFormatHint(
                AVAssetWriterInput::alloc(),
                AVMediaTypeVideo.ok_or_else(|| anyhow!("video media type unavailable"))?,
                Some(&settings),
                None,
            )
        };
        // Screen captures are produced as fast as they are captured; telling the
        // encoder so keeps it from buffering frames it will never be asked for.
        unsafe { input.setExpectsMediaDataInRealTime(true) };

        if !unsafe { writer.canAddInput(&input) } {
            bail!("the H.264 encoder rejected the {width}x{height} input");
        }
        unsafe { writer.addInput(&input) };

        let adaptor = unsafe {
            AVAssetWriterInputPixelBufferAdaptor::initWithAssetWriterInput_sourcePixelBufferAttributes(
                AVAssetWriterInputPixelBufferAdaptor::alloc(),
                &input,
                None,
            )
        };

        if !unsafe { writer.startWriting() } {
            bail!("failed to start writing: {}", status(&writer));
        }
        unsafe { writer.startSessionAtSourceTime(CMTime::with_seconds(0.0, TIMESCALE)) };

        Ok(Self {
            writer,
            input,
            adaptor,
            width: width_usize,
            height: height_usize,
            max_width: options.max_width,
            last: None,
            frames: 0,
        })
    }

    fn append_scaled(&mut self, frame: &RgbaImage, at: Duration) -> Result<()> {
        let (width, height) = frame.dimensions();
        if width as usize != self.width || height as usize != self.height {
            bail!(
                "frame is {width}x{height}, encoder expects {}x{}",
                self.width,
                self.height
            );
        }

        let buffer = pixel_buffer(frame)?;
        if unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) } != 0 {
            bail!("failed to lock the pixel buffer");
        }
        let base = CVPixelBufferGetBaseAddress(&buffer) as *mut u8;
        let stride = CVPixelBufferGetBytesPerRow(&buffer);
        if !base.is_null() {
            // RGBA in, BGRA out: the encoder's native layout. Writing whole
            // pixels at once lets the compiler emit one 32-bit store instead of
            // four byte stores with a bounds check each.
            unsafe {
                for y in 0..height as usize {
                    let (src, _) = frame.as_raw()
                        [y * width as usize * 4..(y + 1) * width as usize * 4]
                        .as_chunks::<4>();
                    let dst = std::slice::from_raw_parts_mut(
                        base.add(y * stride) as *mut [u8; 4],
                        width as usize,
                    );
                    for (into, from) in dst.iter_mut().zip(src) {
                        *into = [from[2], from[1], from[0], from[3]];
                    }
                }
            }
        }
        unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) };

        // The hardware encoder drains on the capture thread, so waiting for
        // room costs at most a frame's encode time.
        let mut spins = 0;
        while !unsafe { self.input.isReadyForMoreMediaData() } {
            if spins > 2_000 {
                bail!("the encoder stopped accepting frames");
            }
            std::thread::sleep(Duration::from_millis(1));
            spins += 1;
        }

        let time = unsafe { CMTime::with_seconds(at.as_secs_f64(), TIMESCALE) };
        if !unsafe {
            self.adaptor
                .appendPixelBuffer_withPresentationTime(&buffer, time)
        } {
            bail!("failed to append a frame: {}", status(&self.writer));
        }
        self.frames += 1;
        Ok(())
    }
}

/// The writer lives entirely on the capture thread once it is moved there, so
/// handing it over is safe even though AVFoundation's classes are not `Send`.
unsafe impl Send for Mp4Sink {}

impl FrameSink for Mp4Sink {
    fn push(&mut self, frame: &RgbaImage, at: Duration) -> Result<()> {
        // Keep the scaled frame, not the full-resolution capture: finish()
        // needs one more sample, and cloning the capture (or scaling it again)
        // would mean another pass over four times as many pixels.
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
            // CMTime quantizes seconds to 1/600 s, so a one-millisecond nudge
            // can land on the same tick as the last sample. A duplicate
            // presentation time makes AVAssetWriter fail at finishWriting on
            // very short recordings.
            let hold = end
                .saturating_sub(Duration::from_millis(1))
                .max(at + Duration::from_millis(2));
            self.append_scaled(&frame, hold)?;
        }
        unsafe { self.input.markAsFinished() };
        // The completion-handler variant needs a block and a semaphore for the
        // same result; this thread has nothing else to do anyway.
        #[allow(deprecated)]
        let finished = unsafe { self.writer.finishWriting() };
        if !finished {
            bail!("failed to finish the video: {}", status(&self.writer));
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

fn pixel_buffer(frame: &RgbaImage) -> Result<CFRetained<CVPixelBuffer>> {
    let (width, height) = frame.dimensions();
    let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            width as usize,
            height as usize,
            kCVPixelFormatType_32BGRA,
            None,
            NonNull::from(&mut raw),
        )
    };
    if status != 0 {
        bail!("CVPixelBufferCreate failed with {status}");
    }
    let raw = NonNull::new(raw).ok_or_else(|| anyhow!("CVPixelBufferCreate returned null"))?;
    Ok(unsafe { CFRetained::from_raw(raw) })
}

fn status(writer: &AVAssetWriter) -> String {
    unsafe {
        match writer.status().0 {
            0 => "cancelled".to_string(),
            2 => "completed".to_string(),
            3 => match writer.error() {
                Some(err) => format!("failed: {}", err.localizedDescription()),
                None => "failed".to_string(),
            },
            _ => format!("status {}", writer.status().0),
        }
    }
}

/// Borrow any object as `AnyObject` for the dictionary APIs. Every Objective-C
/// object shares the layout of an opaque pointer, so the cast is a no-op.
fn any<T: Message>(object: &T) -> &AnyObject {
    unsafe { &*(object as *const T as *const AnyObject) }
}
