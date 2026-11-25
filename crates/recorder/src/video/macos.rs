//! MP4 writing through `AVAssetWriter`, which drives the system H.264 encoder
//! (VideoToolbox) and muxes the container. Frames are handed over as BGRA
//! `CVPixelBuffer`s, the format the encoder converts most cheaply.
//! Extended-range frames go to HEVC instead, as HDR10: 10-bit PQ BT.2020 in
//! `kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange` buffers, with the colour
//! signalling the container needs to tell a player to switch the display.

use std::{
    path::Path,
    ptr::NonNull,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread as _, Message};
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterInputPixelBufferAdaptor, AVFileTypeMPEG4,
    AVMediaTypeVideo, AVVideoAverageBitRateKey, AVVideoCodecKey, AVVideoCodecTypeH264,
    AVVideoCodecTypeHEVC, AVVideoColorPrimaries_ITU_R_709_2, AVVideoColorPrimaries_ITU_R_2020,
    AVVideoColorPrimariesKey, AVVideoColorPropertiesKey, AVVideoCompressionPropertiesKey,
    AVVideoExpectedSourceFrameRateKey, AVVideoHeightKey, AVVideoProfileLevelKey,
    AVVideoTransferFunction_ITU_R_709_2, AVVideoTransferFunction_SMPTE_ST_2084_PQ,
    AVVideoTransferFunctionKey, AVVideoWidthKey, AVVideoYCbCrMatrix_ITU_R_709_2,
    AVVideoYCbCrMatrix_ITU_R_2020, AVVideoYCbCrMatrixKey,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::CMTime;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetDataSize, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetPlaneCount, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
};
use objc2_foundation::{NSError, NSMutableDictionary, NSNumber, NSString, NSURL};
use snapit_capture::{HdrImage, RgbaImage};

use super::{bitrate, hdr, prepare_video};
use crate::{FrameData, FrameSink, RecordOptions};

/// The timescale presentation timestamps are expressed in; 600 is what
/// AVFoundation uses for the movie itself.
const TIMESCALE: i32 = 600;

#[derive(Default)]
struct PresentationTimes {
    last_tick: Option<i64>,
}

impl PresentationTimes {
    fn next(&mut self, at: Duration) -> i64 {
        let tick = (at.as_secs_f64() * f64::from(TIMESCALE)).round() as i64;
        let tick = tick.max(self.last_tick.map_or(0, |last| last + 1));
        self.last_tick = Some(tick);
        tick
    }
}

pub(crate) struct Mp4Sink {
    writer: Retained<AVAssetWriter>,
    input: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    /// Output dimensions after the optional width limit and even-size crop.
    width: usize,
    height: usize,
    max_width: Option<u32>,
    /// Last encoded frame and its timestamp: a still tail is only visible in
    /// the file if something is written after it.
    last: Option<(Arc<RgbaImage>, Duration)>,
    frames: usize,
    t_scale: Duration,
    t_buffer: Duration,
    t_copy: Duration,
    t_wait: Duration,
    t_append: Duration,
    presentation: PresentationTimes,
}

impl Mp4Sink {
    pub(crate) fn new(path: &Path, options: &RecordOptions, probe: &RgbaImage) -> Result<Self> {
        let first = prepare_video(probe, options.max_width);
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
        let color = NSMutableDictionary::<NSString, AnyObject>::new();
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
            color.insert(
                AVVideoColorPrimariesKey.ok_or_else(|| anyhow!("missing primaries key"))?,
                any(AVVideoColorPrimaries_ITU_R_709_2
                    .ok_or_else(|| anyhow!("BT.709 unavailable"))?),
            );
            color.insert(
                AVVideoTransferFunctionKey.ok_or_else(|| anyhow!("missing transfer key"))?,
                any(AVVideoTransferFunction_ITU_R_709_2
                    .ok_or_else(|| anyhow!("BT.709 transfer unavailable"))?),
            );
            color.insert(
                AVVideoYCbCrMatrixKey.ok_or_else(|| anyhow!("missing matrix key"))?,
                any(AVVideoYCbCrMatrix_ITU_R_709_2
                    .ok_or_else(|| anyhow!("BT.709 matrix unavailable"))?),
            );
            settings.insert(
                AVVideoColorPropertiesKey.ok_or_else(|| anyhow!("missing color key"))?,
                any(&*color),
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
            t_scale: Duration::ZERO,
            t_buffer: Duration::ZERO,
            t_copy: Duration::ZERO,
            t_wait: Duration::ZERO,
            t_append: Duration::ZERO,
            presentation: PresentationTimes::default(),
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

        let buffer_started = Instant::now();
        let buffer = pixel_buffer(frame)?;
        self.t_buffer += buffer_started.elapsed();
        let copy_started = Instant::now();
        {
            let _lock = lock_pixel_buffer(&buffer)?;
            let base = CVPixelBufferGetBaseAddress(&buffer) as *mut u8;
            if base.is_null() {
                bail!("the BGRA pixel buffer has a null base address");
            }
            let stride = CVPixelBufferGetBytesPerRow(&buffer);
            let data_size = CVPixelBufferGetDataSize(&buffer);
            validate_bgra_rows(width as usize, height as usize, stride, data_size)?;
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
        self.t_copy += copy_started.elapsed();

        // The hardware encoder drains on the capture thread, so waiting for
        // room costs at most a frame's encode time.
        let wait_started = Instant::now();
        let mut spins = 0;
        while !unsafe { self.input.isReadyForMoreMediaData() } {
            if spins > 2_000 {
                bail!("the encoder stopped accepting frames");
            }
            std::thread::sleep(Duration::from_millis(1));
            spins += 1;
        }
        self.t_wait += wait_started.elapsed();

        let append_started = Instant::now();
        let tick = self.presentation.next(at);
        let time = unsafe { CMTime::new(tick, TIMESCALE) };
        let appended = unsafe {
            self.adaptor
                .appendPixelBuffer_withPresentationTime(&buffer, time)
        };
        self.t_append += append_started.elapsed();
        if !appended {
            bail!(
                "failed to append frame {} at {:.3} ms (tick {tick}/{TIMESCALE}): {}",
                self.frames + 1,
                at.as_secs_f64() * 1000.0,
                status(&self.writer)
            );
        }
        self.frames += 1;
        Ok(())
    }
}

/// AVFoundation access is serialized: capture-thread pushes stop before
/// `finish` runs on the scoped finisher thread. No callback retains Rust state.
unsafe impl Send for Mp4Sink {}

impl FrameSink for Mp4Sink {
    fn push(&mut self, frame: FrameData, at: Duration) -> Result<bool> {
        // `open` only routes SDR frames here; tonemapping keeps the sink
        // correct for a caller that hands it an extended-range frame.
        let frame = match frame {
            FrameData::Sdr(image) => image,
            FrameData::Hdr(hdr) => Arc::new(hdr.to_sdr()),
        };
        // Keep the scaled frame, not the full-resolution capture: finish()
        // needs one more sample, and cloning the capture (or scaling it again)
        // would mean another pass over four times as many pixels.
        let scaled_started = Instant::now();
        let scaled = prepare_video(&frame, self.max_width);
        self.t_scale += scaled_started.elapsed();
        self.append_scaled(&scaled, at)?;
        self.last = Some((
            match scaled {
                std::borrow::Cow::Borrowed(_) => Arc::clone(&frame),
                std::borrow::Cow::Owned(image) => Arc::new(image),
            },
            at,
        ));
        Ok(true)
    }

    fn finish(mut self: Box<Self>, end: Duration) -> Result<()> {
        let closing_started = Instant::now();
        let mut closing = Duration::ZERO;
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
            closing = closing_started.elapsed();
        }
        unsafe { self.input.markAsFinished() };
        // The completion-handler variant needs a block and a semaphore for the
        // same result; this thread has nothing else to do anyway.
        let finish_started = Instant::now();
        #[allow(deprecated)]
        let finished = unsafe { self.writer.finishWriting() };
        let finished_in = finish_started.elapsed();
        if !finished {
            bail!("failed to finish the video: {}", status(&self.writer));
        }
        log::info!("encoded {} frames to MP4", self.frames);
        log::info!(
            target: "timing",
            "mp4 sink: {} frames, scale {:.1} ms, buffer {:.1} ms, copy {:.1} ms, encoder wait {:.1} ms, append {:.1} ms; closing sample {:.1} ms, finishWriting {:.1} ms",
            self.frames,
            crate::ms(self.t_scale),
            crate::ms(self.t_buffer),
            crate::ms(self.t_copy),
            crate::ms(self.t_wait),
            crate::ms(self.t_append),
            crate::ms(closing),
            crate::ms(finished_in)
        );
        Ok(())
    }
}

/// The MP4 backend for these frames: HEVC Main10 for extended range, H.264 for
/// 8-bit. A Mac without a working 10-bit HEVC encoder gets the SDR recording it
/// would have got without the HDR option, from a file recreated from scratch.
pub(crate) fn open(
    output: &Path,
    options: &RecordOptions,
    first: &FrameData,
) -> Result<Box<dyn FrameSink>> {
    match first {
        FrameData::Sdr(image) => Ok(Box::new(Mp4Sink::new(output, options, image)?)),
        FrameData::Hdr(hdr) => match HdrMp4Sink::new(output, options, hdr) {
            Ok(sink) => Ok(Box::new(sink)),
            Err(err) => {
                log::warn!("HEVC Main10 encoder unavailable, recording SDR: {err:#}");
                let _ = std::fs::remove_file(output);
                Ok(Box::new(Mp4Sink::new(output, options, &hdr.to_sdr())?))
            }
        },
    }
}

/// HDR10: PQ BT.2020 in 10-bit 4:2:0, encoded by the system HEVC encoder.
pub(crate) struct HdrMp4Sink {
    writer: Retained<AVAssetWriter>,
    input: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    width: usize,
    height: usize,
    max_width: Option<u32>,
    reference_white_nits: f32,
    sdr_white_nits: f32,
    mixed_sdr_logged: bool,
    /// The converter's output buffers. They still hold the frame that was
    /// encoded last, which is what the closing still sample needs — so, unlike
    /// the SDR sink, nothing has to be kept per frame.
    y: Vec<u8>,
    uv: Vec<u8>,
    last_at: Option<Duration>,
    frames: usize,
    t_scale: Duration,
    t_convert: Duration,
    t_buffer: Duration,
    t_copy: Duration,
    t_wait: Duration,
    t_append: Duration,
    presentation: PresentationTimes,
}

/// Same serialized AVFoundation ownership as `Mp4Sink`.
unsafe impl Send for HdrMp4Sink {}

impl HdrMp4Sink {
    fn new(path: &Path, options: &RecordOptions, probe: &HdrImage) -> Result<Self> {
        let first = hdr::prepare(probe, options.max_width);
        let (width, height) = (first.width(), first.height());
        let (y_len, uv_len) = hdr::plane_lens(width, height);
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
        let color = NSMutableDictionary::<NSString, AnyObject>::new();
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
            // Without this the system encoder accepted the 10-bit buffers and
            // still wrote an 8-bit Main stream (profile_idc 1, both SPS bit
            // depths 0), which no player treats as HDR. The value is
            // `kVTProfileLevel_HEVC_Main10_AutoLevel` from
            // VideoToolbox/VTCompressionProperties.h, which AVFoundation
            // documents as usable for this key.
            compression.insert(
                AVVideoProfileLevelKey.ok_or_else(|| anyhow!("missing profile level key"))?,
                any(&*NSString::from_str("HEVC_Main10_AutoLevel")),
            );
            settings.insert(
                AVVideoCodecKey.ok_or_else(|| anyhow!("missing codec key"))?,
                any(AVVideoCodecTypeHEVC.ok_or_else(|| anyhow!("HEVC unavailable"))?),
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
            // Top-level colour signalling makes the muxer write the `colr` box
            // a player needs to switch the display to HDR mode.
            color.insert(
                AVVideoColorPrimariesKey.ok_or_else(|| anyhow!("missing primaries key"))?,
                any(AVVideoColorPrimaries_ITU_R_2020
                    .ok_or_else(|| anyhow!("BT.2020 unavailable"))?),
            );
            color.insert(
                AVVideoTransferFunctionKey.ok_or_else(|| anyhow!("missing transfer key"))?,
                any(AVVideoTransferFunction_SMPTE_ST_2084_PQ
                    .ok_or_else(|| anyhow!("PQ unavailable"))?),
            );
            color.insert(
                AVVideoYCbCrMatrixKey.ok_or_else(|| anyhow!("missing matrix key"))?,
                any(AVVideoYCbCrMatrix_ITU_R_2020
                    .ok_or_else(|| anyhow!("2020 matrix unavailable"))?),
            );
            settings.insert(
                AVVideoColorPropertiesKey.ok_or_else(|| anyhow!("missing color key"))?,
                any(&*color),
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
        unsafe { input.setExpectsMediaDataInRealTime(true) };

        if !unsafe { writer.canAddInput(&input) } {
            bail!("the HEVC encoder rejected the {width}x{height} input");
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
            reference_white_nits: probe.reference_white_nits(),
            sdr_white_nits: probe.sdr_white_nits(),
            mixed_sdr_logged: false,
            y: vec![0u8; y_len],
            uv: vec![0u8; uv_len],
            last_at: None,
            frames: 0,
            t_scale: Duration::ZERO,
            t_convert: Duration::ZERO,
            t_buffer: Duration::ZERO,
            t_copy: Duration::ZERO,
            t_wait: Duration::ZERO,
            t_append: Duration::ZERO,
            presentation: PresentationTimes::default(),
        })
    }

    /// Copy the planes into a fresh 10-bit buffer and hand it to the encoder.
    fn append_planes(&mut self, at: Duration) -> Result<()> {
        let buffer_started = Instant::now();
        let buffer = planar_pixel_buffer(self.width, self.height)?;
        self.t_buffer += buffer_started.elapsed();
        let copy_started = Instant::now();
        {
            let _lock = lock_pixel_buffer(&buffer)?;
            let row = self
                .width
                .checked_mul(2)
                .ok_or_else(|| anyhow!("10-bit row size overflow"))?;
            let data_size = CVPixelBufferGetDataSize(&buffer);
            unsafe {
                if CVPixelBufferGetPlaneCount(&buffer) != 2 {
                    bail!("the 10-bit pixel buffer does not have two planes");
                }
                let first_base = CVPixelBufferGetBaseAddressOfPlane(&buffer, 0) as usize;
                if first_base == 0 {
                    bail!("the 10-bit pixel buffer has no luma plane");
                }
                for (plane, source) in [(0usize, &self.y[..]), (1, &self.uv[..])] {
                    let base = CVPixelBufferGetBaseAddressOfPlane(&buffer, plane) as *mut u8;
                    if base.is_null() {
                        bail!("the 10-bit pixel buffer has no plane {plane}");
                    }
                    let plane_height = CVPixelBufferGetHeightOfPlane(&buffer, plane);
                    let stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, plane);
                    let offset = (base as usize)
                        .checked_sub(first_base)
                        .ok_or_else(|| anyhow!("10-bit plane {plane} precedes the allocation"))?;
                    validate_plane_rows(offset, row, plane_height, stride, data_size)?;
                    let expected = row
                        .checked_mul(plane_height)
                        .ok_or_else(|| anyhow!("10-bit plane {plane} size overflow"))?;
                    if source.len() != expected {
                        bail!(
                            "10-bit plane {plane} has {} bytes, expected {expected}",
                            source.len()
                        );
                    }
                    for (y, line) in source.chunks_exact(row).enumerate() {
                        let dst = std::slice::from_raw_parts_mut(base.add(y * stride), stride);
                        dst.fill(0);
                        dst[..row].copy_from_slice(line);
                    }
                }
            }
        }
        self.t_copy += copy_started.elapsed();

        let wait_started = Instant::now();
        let mut spins = 0;
        while !unsafe { self.input.isReadyForMoreMediaData() } {
            if spins > 2_000 {
                bail!("the encoder stopped accepting frames");
            }
            std::thread::sleep(Duration::from_millis(1));
            spins += 1;
        }
        self.t_wait += wait_started.elapsed();

        let append_started = Instant::now();
        let tick = self.presentation.next(at);
        let time = unsafe { CMTime::new(tick, TIMESCALE) };
        let appended = unsafe {
            self.adaptor
                .appendPixelBuffer_withPresentationTime(&buffer, time)
        };
        self.t_append += append_started.elapsed();
        if !appended {
            bail!(
                "failed to append frame {} at {:.3} ms (tick {tick}/{TIMESCALE}): {}",
                self.frames + 1,
                at.as_secs_f64() * 1000.0,
                status(&self.writer)
            );
        }
        self.frames += 1;
        self.last_at = Some(at);
        Ok(())
    }
}

impl HdrMp4Sink {
    fn append_hdr(&mut self, hdr: &HdrImage, at: Duration) -> Result<()> {
        let scaled_started = Instant::now();
        let scaled = hdr::prepare(hdr, self.max_width);
        self.t_scale += scaled_started.elapsed();
        // The plane buffers are sized for the first frame; a mid-recording
        // resolution change must error like the SDR sink, not silently write a
        // partial frame into them.
        if scaled.width() as usize != self.width || scaled.height() as usize != self.height {
            bail!(
                "frame is {}x{}, encoder expects {}x{}",
                scaled.width(),
                scaled.height(),
                self.width,
                self.height
            );
        }
        let convert_started = Instant::now();
        hdr::convert_into(&scaled, &mut self.y, &mut self.uv);
        self.t_convert += convert_started.elapsed();
        self.append_planes(at)
    }
}

impl FrameSink for HdrMp4Sink {
    fn push(&mut self, frame: FrameData, at: Duration) -> Result<bool> {
        match frame {
            FrameData::Hdr(hdr) => self.append_hdr(&hdr, at)?,
            FrameData::Sdr(image) => {
                if !self.mixed_sdr_logged {
                    log::info!(
                        "HDR MP4 session received an SDR frame; lifting sRGB at {} nits (reference {} nits)",
                        self.sdr_white_nits,
                        self.reference_white_nits
                    );
                    self.mixed_sdr_logged = true;
                }
                // SCK can change its delivered pixel format when HDR mode is
                // toggled. Keep the Main10 session and preserve SDR white
                // instead of failing the whole recording mid-stream.
                let lifted =
                    hdr::promote_sdr(&image, self.reference_white_nits, self.sdr_white_nits)?;
                self.append_hdr(&lifted, at)?;
            }
        }
        Ok(true)
    }

    fn finish(mut self: Box<Self>, end: Duration) -> Result<()> {
        let closing_started = Instant::now();
        let mut closing = Duration::ZERO;
        if let Some(at) = self.last_at
            && end > at
        {
            // CMTime quantizes seconds to 1/600 s, so the closing sample has to
            // clear the last presentation time by more than a millisecond.
            let hold = end
                .saturating_sub(Duration::from_millis(1))
                .max(at + Duration::from_millis(2));
            self.append_planes(hold)?;
            closing = closing_started.elapsed();
        }
        unsafe { self.input.markAsFinished() };
        let finish_started = Instant::now();
        #[allow(deprecated)]
        let finished = unsafe { self.writer.finishWriting() };
        let finished_in = finish_started.elapsed();
        if !finished {
            bail!("failed to finish the video: {}", status(&self.writer));
        }
        log::info!("encoded {} HDR frames to MP4", self.frames);
        log::info!(
            target: "timing",
            "mp4 hdr sink: {} frames, scale {:.1} ms, convert {:.1} ms, buffer {:.1} ms, copy {:.1} ms, encoder wait {:.1} ms, append {:.1} ms; closing sample {:.1} ms, finishWriting {:.1} ms",
            self.frames,
            crate::ms(self.t_scale),
            crate::ms(self.t_convert),
            crate::ms(self.t_buffer),
            crate::ms(self.t_copy),
            crate::ms(self.t_wait),
            crate::ms(self.t_append),
            crate::ms(closing),
            crate::ms(finished_in)
        );
        Ok(())
    }
}

/// A 10-bit 4:2:0 video-range buffer, the layout `hdr::convert` produces.
fn planar_pixel_buffer(width: usize, height: usize) -> Result<CFRetained<CVPixelBuffer>> {
    let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            width,
            height,
            kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
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

struct LockedPixelBuffer<'a> {
    buffer: &'a CVPixelBuffer,
}

impl Drop for LockedPixelBuffer<'_> {
    fn drop(&mut self) {
        unsafe { CVPixelBufferUnlockBaseAddress(self.buffer, CVPixelBufferLockFlags(0)) };
    }
}

fn lock_pixel_buffer(buffer: &CVPixelBuffer) -> Result<LockedPixelBuffer<'_>> {
    if unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags(0)) } != 0 {
        bail!("failed to lock the pixel buffer");
    }
    Ok(LockedPixelBuffer { buffer })
}

fn validate_bgra_rows(width: usize, height: usize, stride: usize, data_size: usize) -> Result<()> {
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| anyhow!("BGRA row size overflow"))?;
    if width == 0 || height == 0 || stride < row_bytes {
        bail!("invalid BGRA layout: {width}x{height}, stride={stride}, data_size={data_size}");
    }
    let last_end = (height - 1)
        .checked_mul(stride)
        .and_then(|offset| offset.checked_add(row_bytes))
        .ok_or_else(|| anyhow!("BGRA row range overflow"))?;
    if last_end > data_size {
        bail!(
            "BGRA rows exceed the pixel buffer: last_end={last_end}, data_size={data_size}, \
             width={width}, height={height}, stride={stride}"
        );
    }
    Ok(())
}

fn validate_plane_rows(
    plane_offset: usize,
    row_bytes: usize,
    height: usize,
    stride: usize,
    data_size: usize,
) -> Result<()> {
    if row_bytes == 0 || height == 0 || stride < row_bytes {
        bail!(
            "invalid 10-bit plane layout: offset={plane_offset}, height={height}, \
             stride={stride}, row_bytes={row_bytes}, data_size={data_size}"
        );
    }
    let last_end = height
        .checked_mul(stride)
        .and_then(|end| plane_offset.checked_add(end))
        .ok_or_else(|| anyhow!("10-bit plane row range overflow"))?;
    if last_end > data_size {
        bail!(
            "10-bit plane rows exceed the pixel buffer: last_end={last_end}, data_size={data_size}"
        );
    }
    Ok(())
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
                Some(err) => format!("failed: {}", error_description(&err)),
                None => "failed".to_string(),
            },
            _ => format!("status {}", writer.status().0),
        }
    }
}

fn error_description(error: &NSError) -> String {
    let domain = error.domain().to_string();
    let description = error.localizedDescription().to_string();
    let underlying = error.underlyingErrors();
    if underlying.count() == 0 {
        format!("{description} ({domain}, code {})", error.code())
    } else {
        let nested = underlying.objectAtIndex(0);
        format!(
            "{} ({}, code {}); underlying {} ({}, code {})",
            description,
            domain,
            error.code(),
            nested.localizedDescription(),
            nested.domain(),
            nested.code()
        )
    }
}

/// Borrow any object as `AnyObject` for the dictionary APIs. Every Objective-C
/// object shares the layout of an opaque pointer, so the cast is a no-op.
fn any<T: Message>(object: &T) -> &AnyObject {
    unsafe { &*(object as *const T as *const AnyObject) }
}

#[cfg(test)]
mod presentation_tests {
    use super::*;

    #[test]
    fn bgra_pixel_buffer_rows_require_a_valid_stride_and_data_size() {
        assert!(validate_bgra_rows(2, 2, 8, 16).is_ok());
        assert!(validate_bgra_rows(2, 2, 7, 16).is_err());
        assert!(validate_bgra_rows(2, 2, 8, 11).is_err());
        assert!(validate_bgra_rows(0, 2, 8, 16).is_err());
        assert!(validate_bgra_rows(usize::MAX, 2, usize::MAX, usize::MAX).is_err());
    }

    #[test]
    fn planar_pixel_buffer_rows_require_valid_offsets_strides_and_lengths() {
        assert!(validate_plane_rows(0, 8, 2, 8, 16).is_ok());
        assert!(validate_plane_rows(16, 8, 1, 8, 24).is_ok());
        assert!(validate_plane_rows(0, 8, 2, 7, 16).is_err());
        assert!(validate_plane_rows(0, 8, 2, 8, 15).is_err());
        assert!(validate_plane_rows(16, 8, 1, 8, 23).is_err());
        assert!(validate_plane_rows(0, 8, 2, 16, 24).is_err());
        assert!(validate_plane_rows(0, 8, 0, 8, 16).is_err());
        assert!(validate_plane_rows(usize::MAX, 8, 2, usize::MAX, usize::MAX).is_err());
    }

    #[test]
    fn created_planar_buffer_satisfies_the_checked_layout() {
        let buffer = planar_pixel_buffer(320, 200).expect("10-bit pixel buffer");
        let _lock = lock_pixel_buffer(&buffer).expect("lock pixel buffer");
        let data_size = CVPixelBufferGetDataSize(&buffer);
        let first = CVPixelBufferGetBaseAddressOfPlane(&buffer, 0) as usize;
        assert_ne!(first, 0);
        for plane in 0..2 {
            let base = CVPixelBufferGetBaseAddressOfPlane(&buffer, plane) as usize;
            let offset = base.checked_sub(first).expect("plane follows luma");
            validate_plane_rows(
                offset,
                320 * 2,
                CVPixelBufferGetHeightOfPlane(&buffer, plane),
                CVPixelBufferGetBytesPerRowOfPlane(&buffer, plane),
                data_size,
            )
            .expect("CoreVideo's planar layout fits its data size");
        }
    }

    #[test]
    fn sub_tick_timestamps_still_advance_by_one_media_tick() {
        let mut times = PresentationTimes::default();
        assert_eq!(times.next(Duration::ZERO), 0);
        assert_eq!(times.next(Duration::from_micros(500)), 1);
        assert_eq!(times.next(Duration::from_micros(1_000)), 2);
        assert_eq!(times.next(Duration::from_micros(1_100)), 3);
        assert_eq!(times.next(Duration::from_millis(2)), 4);
    }

    #[test]
    fn backwards_timestamps_never_repeat_or_move_back() {
        let mut times = PresentationTimes::default();
        assert_eq!(times.next(Duration::from_millis(100)), 60);
        assert_eq!(times.next(Duration::from_millis(90)), 61);
        assert_eq!(times.next(Duration::from_millis(90)), 62);
    }
}

#[cfg(test)]
mod hdr_sink_tests {
    use std::sync::Arc;

    use super::*;
    use crate::RecordFormat;
    use crate::video::hdr::synth_solid;
    use snapit_capture::{PixelRect, RasterLayer, RasterTile};

    fn watermark() -> Arc<RasterLayer> {
        Arc::new(RasterLayer {
            tiles: vec![RasterTile {
                bounds: PixelRect {
                    x: 280,
                    y: 160,
                    width: 32,
                    height: 32,
                },
                pixels: vec![[0, 96, 255, 255]; 32 * 32],
            }],
        })
    }

    /// Encode synthetic extended-range frames through the real AVAssetWriter
    /// pipeline and read the container back. The frames are built by hand, so
    /// this checks the HEVC Main10 path on a machine whose display need not be
    /// in HDR mode. Run it with
    /// `cargo test -p snapit-recorder hdr_sink -- --ignored`, then play the
    /// file it names.
    #[test]
    #[ignore = "encodes with the system H.264 encoder and writes a file"]
    fn sdr_sink_keeps_native_dimensions_and_tags_bt709() {
        let path = std::env::temp_dir().join("snapit-sdr-macos-test.mp4");
        let _ = std::fs::remove_file(&path);
        let options = RecordOptions::default()
            .with_format(RecordFormat::Mp4)
            .with_fps(15);
        let colours = [[240, 240, 240], [220, 40, 30], [30, 200, 60]];
        let watermark = watermark();
        let first = FrameData::Sdr(Arc::new(RgbaImage::from_pixel(
            323,
            203,
            image::Rgba([colours[0][0], colours[0][1], colours[0][2], 255]),
        )))
        .apply_watermark(Some(&watermark));
        let mut sink = open(&path, &options, &first).expect("mp4 sink");

        for index in 0..320 {
            let colour = colours[index % colours.len()];
            let frame = FrameData::Sdr(Arc::new(RgbaImage::from_pixel(
                323,
                203,
                image::Rgba([colour[0], colour[1], colour[2], 255]),
            )))
            .apply_watermark(Some(&watermark));
            let at = match index {
                0 => Duration::ZERO,
                1 => Duration::from_micros(500),
                2 => Duration::from_millis(1),
                3 => Duration::from_micros(1_100),
                _ => Duration::from_millis((index as u64 - 3) * 33),
            };
            sink.push(frame, at).expect("push");
        }
        sink.finish(Duration::from_secs(11)).expect("finalize");

        let bytes = std::fs::read(&path).expect("the encoder wrote nothing");
        let entry = find(&bytes, b"avc1").expect("no avc1 sample entry") - 4;
        let width = u16::from_be_bytes(bytes[entry + 32..entry + 34].try_into().unwrap());
        let height = u16::from_be_bytes(bytes[entry + 34..entry + 36].try_into().unwrap());
        assert_eq!((width, height), (322, 202));
        let colr = find(&bytes, b"colrnclx").expect("no colr/nclx box in the file");
        assert_eq!(
            &bytes[colr + 8..colr + 15],
            &[0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00],
            "expected BT.709 primaries, transfer and matrix with video range"
        );
    }

    #[test]
    #[ignore = "encodes with the system HEVC encoder and writes a file"]
    fn main10_sink_writes_an_hdr10_container() {
        let path = std::env::temp_dir().join("snapit-main10-macos-test.mp4");
        let _ = std::fs::remove_file(&path);
        let options = RecordOptions::default()
            .with_format(RecordFormat::Mp4)
            .with_fps(15)
            .with_hdr(true);
        // A hard cut between colours each frame, so the encoder has real
        // motion to encode rather than one infinite-length P frame.
        let colours = [[1.0, 1.0, 1.0], [4.0, 0.0, 0.0], [0.0, 4.0, 0.0]];
        let (width, height) = (320, 200);
        let watermark = watermark();
        let first = FrameData::Hdr(Arc::new(synth_solid(width, height, colours[0], 203.0)))
            .apply_watermark(Some(&watermark));
        let mut sink = open(&path, &options, &first).expect("mp4 sink");

        for (index, colour) in colours.into_iter().cycle().take(10).enumerate() {
            let frame = if index == 5 {
                // SCK may deliver SDR after an HDR first frame. Exercise the
                // real sink's promotion branch, not only the converter test.
                FrameData::Sdr(Arc::new(RgbaImage::from_pixel(
                    width,
                    height,
                    image::Rgba([255, 255, 255, 255]),
                )))
            } else {
                FrameData::Hdr(Arc::new(synth_solid(width, height, colour, 203.0)))
            }
            .apply_watermark(Some(&watermark));
            sink.push(frame, Duration::from_millis(index as u64 * 66))
                .expect("push");
        }
        sink.finish(Duration::from_millis(700)).expect("finalize");

        let bytes = std::fs::read(&path).expect("the encoder wrote nothing");
        println!("{}: {} bytes", path.display(), bytes.len());
        assert!(
            bytes.len() > 1024,
            "a 10-frame clip cannot be {} bytes",
            bytes.len()
        );
        let hvc = find(&bytes, b"hvcC").expect("no hvcC: the file is not HEVC");
        // `find` lands on the box type; the payload follows it. Byte 1 holds
        // profile_space (2 bits) | tier (1) | profile_idc (5), and the SPS bit
        // depths sit 16 and 17 bytes into the payload, biased by 8.
        assert_eq!(bytes[hvc + 5] & 0x7f, 2, "HEVC profile must be Main 10");
        assert_eq!(
            (bytes[hvc + 21] & 0x07, bytes[hvc + 22] & 0x07),
            (2, 2),
            "both planes must be 10-bit"
        );

        // AVFoundation writes this box from AVVideoColorPropertiesKey; without
        // it a player shows the PQ stream as dull SDR and never switches the
        // display into HDR mode. `find` lands on the box type, so `nclx`
        // follows it and the three code points start four bytes later.
        let colr = find(&bytes, b"colrnclx").expect("no colr/nclx box in the file");
        let nclx = &bytes[colr + 8..colr + 15];
        println!("colr nclx payload: {nclx:02x?}");
        assert_eq!(&nclx[0..2], &[0x00, 0x09], "expected BT.2020 primaries");
        assert_eq!(&nclx[2..4], &[0x00, 0x10], "expected the PQ transfer");
        assert_eq!(&nclx[4..6], &[0x00, 0x09], "expected the BT.2020 matrix");
        // The encoder is told video range, so the full-range flag must be 0.
        assert_eq!(nclx[6] & 0x80, 0, "expected video range");
    }

    fn find(bytes: &[u8], tag: &[u8]) -> Option<usize> {
        bytes.windows(tag.len()).position(|window| window == tag)
    }
}
