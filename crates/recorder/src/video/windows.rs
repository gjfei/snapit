//! MP4 writing through Media Foundation's sink writer, which drives the system
//! H.264 encoder (hardware where the driver exposes one) and muxes the
//! container. Extended-range frames go to the HEVC encoder instead, as HDR10
//! (Main10, PQ, BT.2020): H.264 has no 10-bit profile a decoder accepts.
//!
//! The sink writer is created in a multithreaded apartment and used from the
//! capture thread. Objects created in the MTA are agile, so no marshalling is
//! needed; both threads just have to have joined the MTA, which `ensure_com`
//! does.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use snapit_capture::{HdrImage, RgbaImage};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFSinkWriter, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE,
    MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_VERSION,
    MFCreateAttributes, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFCreateSinkWriterFromURL, MFMediaType_Video, MFNominalRange_16_235, MFSTARTUP_LITE,
    MFShutdown, MFStartup, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_P010,
    MFVideoFormat_RGB32, MFVideoInterlace_Progressive, MFVideoPrimaries_BT2020,
    MFVideoTransFunc_2084, MFVideoTransferMatrix_BT2020_10, eAVEncH265VProfile_Main_420_10,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::HSTRING;

use super::{Nclx, hdr, mp4_colour, prepare};
use crate::{FrameData, FrameSink, RecordOptions};

/// Media Foundation counts time in 100 ns units.
const TICKS_PER_SECOND: i64 = 10_000_000;

/// Row pitch alignment for the 10-bit planes. Encoders read D3D surfaces,
/// whose 16-bit-per-sample rows are aligned, so the padded layout is the one
/// every HEVC MFT accepts.
const PITCH_ALIGNMENT: usize = 64;

/// The MP4 backend for these frames: HEVC Main10 for extended range, H.264 for
/// 8-bit. A machine without a usable 10-bit HEVC encoder — no extension
/// installed, or a driver that rejects the input type — gets the SDR recording
/// it would have got without the HDR option, because the file is reopened from
/// scratch before the fallback writer starts.
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
    t_scale: Duration,
    t_buffer: Duration,
    t_copy: Duration,
    t_write: Duration,
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
            t_scale: Duration::ZERO,
            t_buffer: Duration::ZERO,
            t_copy: Duration::ZERO,
            t_write: Duration::ZERO,
        })
    }

    fn append_scaled(
        &mut self,
        frame: &RgbaImage,
        at: Duration,
        duration_ticks: i64,
    ) -> Result<()> {
        let (width, height) = frame.dimensions();
        if width != self.width || height != self.height {
            bail!(
                "frame is {width}x{height}, encoder expects {}x{}",
                self.width,
                self.height
            );
        }

        let stride = (width * 4) as usize;
        let buffer_started = Instant::now();
        let buffer = unsafe { MFCreateMemoryBuffer((stride * height as usize) as u32) }?;
        self.t_buffer += buffer_started.elapsed();
        let copy_started = Instant::now();
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
        self.t_copy += copy_started.elapsed();

        let write_started = Instant::now();
        let sample = unsafe { MFCreateSample() }?;
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime((at.as_secs_f64() * TICKS_PER_SECOND as f64) as i64)?;
            sample.SetSampleDuration(duration_ticks.max(1))?;
            self.writer.WriteSample(self.stream, &sample)?;
        }
        self.t_write += write_started.elapsed();
        self.frames += 1;
        Ok(())
    }
}

impl FrameSink for Mp4Sink {
    fn push(&mut self, frame: FrameData, at: Duration) -> Result<()> {
        // The capture thread has to be in the MTA as well; the call is a no-op
        // when it already is.
        ensure_com()?;
        // `open` only routes SDR frames here; tonemapping keeps the sink
        // correct for a caller that hands it an extended-range frame.
        let frame = match frame {
            FrameData::Sdr(image) => image,
            FrameData::Hdr(hdr) => Arc::new(hdr.to_sdr()),
        };
        // Keep the scaled frame: finish() needs one more sample, and cloning
        // the full-resolution capture (or scaling it again) would mean another
        // pass over four times as many pixels.
        let scaled_started = Instant::now();
        let scaled = prepare(&frame, self.max_width).into_owned();
        self.t_scale += scaled_started.elapsed();
        self.append_scaled(&scaled, at, self.frame_ticks)?;
        self.last = Some((scaled, at));
        Ok(())
    }

    fn finish(mut self: Box<Self>, end: Duration) -> Result<()> {
        ensure_com()?;
        let closing_started = Instant::now();
        let mut closing = Duration::ZERO;
        // A container ends at its last sample, so the closing still stretch
        // needs a frame of its own or the video looks cut short.
        if let Some((frame, at)) = self.last.take()
            && let Some((hold, duration_ticks)) = closing_sample(at, end, self.frame_ticks)
        {
            self.append_scaled(&frame, hold, duration_ticks)?;
            closing = closing_started.elapsed();
        }
        let finalize_started = Instant::now();
        unsafe {
            self.writer.Finalize()?;
            let _ = MFShutdown();
        }
        log::info!("encoded {} frames to MP4", self.frames);
        log::info!(
            target: "timing",
            "mp4 sink: {} frames, scale {:.1} ms, buffer {:.1} ms, copy {:.1} ms, write {:.1} ms; closing sample {:.1} ms, finalize {:.1} ms",
            self.frames,
            crate::ms(self.t_scale),
            crate::ms(self.t_buffer),
            crate::ms(self.t_copy),
            crate::ms(self.t_write),
            crate::ms(closing),
            crate::ms(finalize_started.elapsed())
        );
        Ok(())
    }
}

/// HDR10: PQ BT.2020 in 10-bit 4:2:0, encoded by the system HEVC MFT.
pub(crate) struct HdrMp4Sink {
    /// The muxer writes no colour signalling, so `finish` reopens the file to
    /// put the HDR10 tagging into the container itself.
    path: PathBuf,
    writer: IMFSinkWriter,
    stream: u32,
    width: u32,
    height: u32,
    /// Padded row length both planes are written with.
    stride: usize,
    max_width: Option<u32>,
    frame_ticks: i64,
    /// The converter's output buffers. They hold the frame that was encoded
    /// last, which is what the closing still sample needs — no extra copy.
    y: Vec<u8>,
    uv: Vec<u8>,
    last_at: Option<Duration>,
    frames: usize,
    t_scale: Duration,
    t_convert: Duration,
    t_buffer: Duration,
    t_copy: Duration,
    t_write: Duration,
}

/// Same reasoning as the SDR sink: the writer lives on the capture thread.
unsafe impl Send for HdrMp4Sink {}

impl HdrMp4Sink {
    pub(crate) fn new(path: &Path, options: &RecordOptions, probe: &HdrImage) -> Result<Self> {
        ensure_com()?;
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }.context("Media Foundation unavailable")?;

        let first = hdr::prepare(probe, options.max_width);
        let (width, height) = (first.width(), first.height());
        let (y_len, uv_len) = hdr::plane_lens(width, height);
        let stride = (width as usize * 2).next_multiple_of(PITCH_ALIGNMENT);
        let fps = options.fps.max(1);
        let bitrate = bitrate(width, height, options);

        // Main10 lives in the hardware encoders: the HEVC extension that
        // Windows installs for CPU encoding is 8-bit only and rejects the
        // profile, so the writer has to be told to consider hardware MFTs. It
        // inserts the staging that lets this sink keep handing it ordinary
        // memory buffers.
        let mut attributes: Option<IMFAttributes> = None;
        unsafe { MFCreateAttributes(&mut attributes, 1) }.context("failed to create attributes")?;
        let attributes = attributes.context("failed to create attributes")?;
        unsafe { attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1) }
            .context("failed to enable hardware encoders")?;
        let writer = unsafe {
            MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, Some(&attributes))
        }
        .with_context(|| format!("failed to open {}", path.display()))?;

        // Output: HDR10 as the decoder must read it. The colour attributes also
        // seed the container tagging that `finish` adds by hand.
        let output = unsafe { MFCreateMediaType() }?;
        unsafe {
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            // Main tier, level auto-selected by the encoder; without the
            // profile the HEVC MFT refuses the stream.
            output.SetUINT32(
                &MF_MT_MPEG2_PROFILE,
                eAVEncH265VProfile_Main_420_10.0 as u32,
            )?;
            output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            output.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            // The colour signalling, on both the input and the output type:
            // the encoder copies it into the bitstream, the sink writer into
            // the container's `colr` box.
            output.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT2020.0 as u32)?;
            output.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_2084.0 as u32)?;
            output.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT2020_10.0 as u32)?;
            output.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        }
        let stream = unsafe { writer.AddStream(&output) }?;

        // Input: the converter's P010 planes, top-down.
        let input = unsafe { MFCreateMediaType() }?;
        unsafe {
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_P010)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, stride as u32)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            input.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT2020.0 as u32)?;
            input.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_2084.0 as u32)?;
            input.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT2020_10.0 as u32)?;
            input.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            writer.SetInputMediaType(stream, &input, None)?;
            writer.BeginWriting()?;
        }

        Ok(Self {
            path: path.to_path_buf(),
            writer,
            stream,
            width,
            height,
            stride,
            max_width: options.max_width,
            frame_ticks: TICKS_PER_SECOND / fps as i64,
            y: vec![0u8; y_len],
            uv: vec![0u8; uv_len],
            last_at: None,
            frames: 0,
            t_scale: Duration::ZERO,
            t_convert: Duration::ZERO,
            t_buffer: Duration::ZERO,
            t_copy: Duration::ZERO,
            t_write: Duration::ZERO,
        })
    }

    /// Hand the converter's tightly packed planes to the encoder, one padded
    /// row at a time.
    fn append_planes(&mut self, at: Duration, duration_ticks: i64) -> Result<()> {
        let row = self.width as usize * 2;
        let luma_len = self.stride * self.height as usize;
        let total = luma_len + luma_len / 2;
        let buffer_started = Instant::now();
        let buffer = unsafe { MFCreateMemoryBuffer(total as u32) }?;
        self.t_buffer += buffer_started.elapsed();
        let copy_started = Instant::now();
        {
            let mut raw: *mut u8 = std::ptr::null_mut();
            unsafe { buffer.Lock(&mut raw, None, None) }?;
            if raw.is_null() {
                bail!("Media Foundation returned an empty frame buffer");
            }
            let dst = unsafe { std::slice::from_raw_parts_mut(raw, total) };
            let (luma, chroma) = dst.split_at_mut(luma_len);
            for (y, line) in self.y.chunks_exact(row).enumerate() {
                let at = y * self.stride;
                luma[at..at + row].copy_from_slice(line);
            }
            for (y, line) in self.uv.chunks_exact(row).enumerate() {
                let at = y * self.stride;
                chroma[at..at + row].copy_from_slice(line);
            }
            unsafe { buffer.Unlock() }?;
        }
        unsafe { buffer.SetCurrentLength(total as u32) }?;
        self.t_copy += copy_started.elapsed();

        let write_started = Instant::now();
        let sample = unsafe { MFCreateSample() }?;
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime((at.as_secs_f64() * TICKS_PER_SECOND as f64) as i64)?;
            sample.SetSampleDuration(duration_ticks.max(1))?;
            self.writer.WriteSample(self.stream, &sample)?;
        }
        self.t_write += write_started.elapsed();
        self.frames += 1;
        self.last_at = Some(at);
        Ok(())
    }
}

impl FrameSink for HdrMp4Sink {
    fn push(&mut self, frame: FrameData, at: Duration) -> Result<()> {
        ensure_com()?;
        let FrameData::Hdr(hdr) = frame else {
            bail!("the HEVC Main10 sink only accepts extended-range frames");
        };
        let scaled_started = Instant::now();
        let scaled = hdr::prepare(&hdr, self.max_width);
        self.t_scale += scaled_started.elapsed();
        // The plane buffers are sized for the first frame; a mid-recording
        // resolution change must error like the SDR sink, not silently write a
        // partial frame into them.
        if scaled.width() != self.width || scaled.height() != self.height {
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
        self.append_planes(at, self.frame_ticks)
    }

    fn finish(mut self: Box<Self>, end: Duration) -> Result<()> {
        ensure_com()?;
        let closing_started = Instant::now();
        let mut closing = Duration::ZERO;
        // A container ends at its last sample, so the closing still stretch
        // needs a frame of its own or the video looks cut short.
        if let Some(at) = self.last_at
            && let Some((hold, duration_ticks)) = closing_sample(at, end, self.frame_ticks)
        {
            self.append_planes(hold, duration_ticks)?;
            closing = closing_started.elapsed();
        }
        let finalize_started = Instant::now();
        unsafe {
            self.writer.Finalize()?;
            let _ = MFShutdown();
        }
        // Release the sink writer before reopening the file: while it holds the
        // byte stream, the write-open below can fail with a sharing violation
        // and leave the recording untagged.
        drop(self.writer);
        // The muxer writes no colour box and the encoder no VUI description, so
        // a Main10 PQ file would play back as a dull SDR clip. A recording must
        // not be thrown away over metadata, hence the warning.
        if let Err(err) = mp4_colour::insert(&self.path, &Nclx::HDR10) {
            log::warn!("failed to tag {} as HDR10: {err:#}", self.path.display());
        }
        log::info!("encoded {} HDR frames to MP4", self.frames);
        log::info!(
            target: "timing",
            "mp4 hdr sink: {} frames, scale {:.1} ms, convert {:.1} ms, buffer {:.1} ms, copy {:.1} ms, write {:.1} ms; closing sample {:.1} ms, finalize {:.1} ms",
            self.frames,
            crate::ms(self.t_scale),
            crate::ms(self.t_convert),
            crate::ms(self.t_buffer),
            crate::ms(self.t_copy),
            crate::ms(self.t_write),
            crate::ms(closing),
            crate::ms(finalize_started.elapsed())
        );
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

fn closing_sample(at: Duration, end: Duration, frame_ticks: i64) -> Option<(Duration, i64)> {
    if end <= at {
        return None;
    }
    let hold = end
        .saturating_sub(Duration::from_millis(1))
        .max(at + Duration::from_micros(1));
    let duration =
        (end.saturating_sub(hold).as_secs_f64() * TICKS_PER_SECOND as f64).round() as i64;
    Some((hold, duration.clamp(1, frame_ticks.max(1))))
}

fn ensure_com() -> Result<()> {
    let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if result.is_err() {
        return Err(windows::core::Error::from(result).into());
    }
    Ok(())
}

#[cfg(test)]
mod hdr_sink_tests {
    use super::*;
    use crate::{RecordFormat, video::hdr::synth_solid};

    #[test]
    fn closing_samples_end_at_the_requested_timestamp() {
        let (hold, ticks) = closing_sample(Duration::ZERO, Duration::from_secs(1), 666_667)
            .expect("a closing sample");
        assert_eq!(hold, Duration::from_millis(999));
        assert_eq!(
            hold + Duration::from_nanos(ticks as u64 * 100),
            Duration::from_secs(1)
        );

        let (hold, ticks) = closing_sample(
            Duration::from_millis(100),
            Duration::from_micros(100_500),
            666_667,
        )
        .expect("a short closing sample");
        assert!(hold > Duration::from_millis(100));
        assert_eq!(
            hold + Duration::from_nanos(ticks as u64 * 100),
            Duration::from_micros(100_500)
        );
        assert!(closing_sample(Duration::from_secs(1), Duration::from_secs(1), 666_667).is_none());
    }

    /// Encode synthetic extended-range frames through the real sink writer and
    /// read the container back. The frames are built by hand, so this checks
    /// the HEVC path on a machine whose display can only capture SDR. Run it
    /// with `cargo test -p snapit-recorder hdr_sink -- --ignored`, then play
    /// the file it names.
    #[test]
    #[ignore = "encodes with the machine's HEVC encoder and writes a file"]
    fn main10_sink_writes_an_hdr10_container() {
        let path = std::env::temp_dir().join("snapit-main10-test.mp4");
        let _ = std::fs::remove_file(&path);
        let options = RecordOptions::default()
            .with_format(RecordFormat::Mp4)
            .with_fps(15)
            .with_hdr(true);
        // A hard cut between colours each frame, so the encoder has real
        // motion to encode rather than one infinite-length P frame.
        let colours = [[1.0, 1.0, 1.0], [4.0, 0.0, 0.0], [0.0, 4.0, 0.0]];
        let (width, height) = (320, 200);
        let first = FrameData::Hdr(Arc::new(synth_solid(width, height, colours[0], 203.0)));
        let mut sink = open(&path, &options, &first).expect("mp4 sink");

        for (index, colour) in colours.into_iter().cycle().take(10).enumerate() {
            let frame = FrameData::Hdr(Arc::new(synth_solid(width, height, colour, 203.0)));
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
        // configurationVersion, then profile_space (2 bits) | tier | profile_idc.
        assert_eq!(bytes[hvc + 5] & 0x7f, 2, "HEVC profile must be Main 10");

        // The container signalling, which is what makes a player switch the
        // panel to HDR mode instead of showing the PQ stream as dull SDR.
        let colr = find(&bytes, b"colrnclx").expect("no colr/nclx box in the file");
        let colour = &bytes[colr + 8..colr + 15];
        assert_eq!(
            colour,
            &[0x00, 0x09, 0x00, 0x10, 0x00, 0x09, 0x00],
            "expected BT.2020 primaries, PQ transfer, BT.2020 matrix, video range"
        );
        let moov = find(&bytes, b"moov").expect("no moov");
        assert!(
            (moov..moov + 64 * 1024).contains(&colr),
            "the box belongs in the moov"
        );
        assert!(
            colr < hvc,
            "a colr after the codec configuration is out of order"
        );
        // The sizes the patch grew must still walk the file to its exact end.
        let mut at = 0usize;
        while at + 8 <= bytes.len() {
            let size = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            assert!(size >= 8 && at + size <= bytes.len(), "bad box at {at}");
            println!(
                "{} {at}..{}",
                String::from_utf8_lossy(&bytes[at + 4..at + 8]),
                at + size
            );
            at += size;
        }
        assert_eq!(at, bytes.len(), "the top-level boxes do not tile the file");
    }

    fn find(bytes: &[u8], tag: &[u8]) -> Option<usize> {
        bytes.windows(tag.len()).position(|window| window == tag)
    }
}
