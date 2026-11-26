use std::{
    fs::{File, OpenOptions},
    io::{BufReader, BufWriter, Read as _, Seek as _, SeekFrom, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, Result, ensure};
use image::RgbaImage;

const MAX_READ_BYTES: usize = 64 * 1024 * 1024;
const IO_CHUNK_BYTES: usize = 8 * 1024 * 1024;
static NEXT_DISK_IMAGE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct DiskImage {
    path: std::sync::Arc<PathBuf>,
    width: u32,
    height: u32,
}

impl DiskImage {
    pub fn create(width: u32, height: u32) -> Result<Self> {
        ensure!(width > 0 && height > 0, "disk image is empty");
        let row_bytes = width
            .checked_mul(4)
            .context("disk image row is too large")? as usize;
        let total_bytes = row_bytes
            .checked_mul(height as usize)
            .context("disk image is too large")?;
        let seq = NEXT_DISK_IMAGE.fetch_add(1, Ordering::Relaxed);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "snapit-long-{}-{nonce}-{seq}.rgba",
            std::process::id()
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        Self::from_created_file(path, width, height, total_bytes as u64, file)
    }

    fn from_created_file(
        path: PathBuf,
        width: u32,
        height: u32,
        total_bytes: u64,
        file: File,
    ) -> Result<Self> {
        let disk = Self {
            path: std::sync::Arc::new(path),
            width,
            height,
        };
        if let Err(err) = file.set_len(total_bytes) {
            // Close the handle before DiskImage's Drop removes the path on
            // Windows, where an open file can otherwise block deletion.
            drop(file);
            return Err(err).with_context(|| format!("failed to size {}", disk.path.display()));
        }
        Ok(disk)
    }

    pub fn from_image(image: &RgbaImage) -> Result<Self> {
        let disk = Self::create(image.width(), image.height())?;
        let mut file = OpenOptions::new()
            .write(true)
            .open(disk.path.as_path())
            .with_context(|| format!("failed to open {}", disk.path.display()))?;
        let rows_per_chunk = disk.rows_for_bytes(IO_CHUNK_BYTES);
        let row_bytes = image.width() as usize * 4;
        for y in (0..image.height()).step_by(rows_per_chunk as usize) {
            let rows = rows_per_chunk.min(image.height() - y);
            let start = y as usize * row_bytes;
            let end = start + rows as usize * row_bytes;
            file.write_all(&image.as_raw()[start..end])
                .context("failed to write disk image")?;
        }
        Ok(disk)
    }

    pub fn from_png(path: &Path) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
        decoder.set_transformations(png::Transformations::ALPHA | png::Transformations::STRIP_16);
        let mut reader = decoder
            .read_info()
            .with_context(|| format!("failed to read PNG header {}", path.display()))?;
        let info = reader.info().clone();
        let (width, height) = (info.width, info.height);
        ensure!(width > 0 && height > 0, "PNG image is empty");
        let (color_type, depth) = reader.output_color_type();
        ensure!(depth == png::BitDepth::Eight, "PNG did not decode to 8-bit");
        ensure!(
            matches!(
                color_type,
                png::ColorType::Rgba
                    | png::ColorType::Rgb
                    | png::ColorType::Grayscale
                    | png::ColorType::GrayscaleAlpha
            ),
            "unsupported PNG output format"
        );
        let disk = Self::create(info.width, info.height)?;
        let raw_output = OpenOptions::new()
            .write(true)
            .open(disk.path.as_path())
            .with_context(|| format!("failed to open {}", disk.path.display()))?;
        let mut output = BufWriter::with_capacity(IO_CHUNK_BYTES, raw_output);
        let mut rgba_row = vec![0u8; info.width as usize * 4];
        for _ in 0..info.height {
            let row = reader
                .next_row()
                .context("failed to decode PNG row")?
                .context("PNG ended before its declared height")?;
            match color_type {
                png::ColorType::Rgba => output
                    .write_all(row.data())
                    .context("failed to write decoded PNG row")?,
                png::ColorType::Rgb => {
                    for (pixel, src) in rgba_row
                        .as_chunks_mut::<4>()
                        .0
                        .iter_mut()
                        .zip(row.data().as_chunks::<3>().0)
                    {
                        pixel.copy_from_slice(&[src[0], src[1], src[2], 255]);
                    }
                    output
                        .write_all(&rgba_row)
                        .context("failed to write decoded PNG row")?;
                }
                png::ColorType::Grayscale => {
                    for (pixel, src) in rgba_row.as_chunks_mut::<4>().0.iter_mut().zip(row.data()) {
                        pixel.copy_from_slice(&[*src, *src, *src, 255]);
                    }
                    output
                        .write_all(&rgba_row)
                        .context("failed to write decoded PNG row")?;
                }
                png::ColorType::GrayscaleAlpha => {
                    for (pixel, src) in rgba_row
                        .as_chunks_mut::<4>()
                        .0
                        .iter_mut()
                        .zip(row.data().as_chunks::<2>().0)
                    {
                        pixel.copy_from_slice(&[src[0], src[0], src[0], src[1]]);
                    }
                    output
                        .write_all(&rgba_row)
                        .context("failed to write decoded PNG row")?;
                }
                _ => unreachable!(),
            }
        }
        output
            .flush()
            .context("failed to flush decoded PNG buffer")?;
        Ok(disk)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn byte_len(&self) -> u64 {
        self.width as u64 * self.height as u64 * 4
    }

    pub fn read_rows(&self, start: u32, rows: u32) -> Result<RgbaImage> {
        ensure!(rows > 0, "disk image read is empty");
        ensure!(
            start
                .checked_add(rows)
                .is_some_and(|end| end <= self.height),
            "disk image read is out of bounds"
        );
        let row_bytes = self.width as usize * 4;
        let len = row_bytes
            .checked_mul(rows as usize)
            .context("disk image read is too large")?;
        ensure!(
            len <= MAX_READ_BYTES,
            "disk image read is {:.2} MiB, exceeding the {:.0} MiB limit",
            len as f64 / (1024.0 * 1024.0),
            MAX_READ_BYTES as f64 / (1024.0 * 1024.0)
        );
        let mut bytes = vec![0u8; len];
        let mut file = File::open(self.path.as_path())
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        file.seek(SeekFrom::Start(start as u64 * row_bytes as u64))
            .context("failed to seek disk image")?;
        file.read_exact(&mut bytes)
            .context("failed to read disk image")?;
        RgbaImage::from_raw(self.width, rows, bytes).context("disk image buffer size mismatch")
    }

    pub fn write_rows_at(&self, start: u32, image: &RgbaImage) -> Result<()> {
        ensure!(image.width() == self.width, "disk image width mismatch");
        self.write_raw_rows_at(start, image.height(), image.as_raw())
    }

    pub fn write_raw_rows_at(&self, start: u32, rows: u32, raw: &[u8]) -> Result<()> {
        ensure!(rows > 0, "disk image write is empty");
        ensure!(
            start
                .checked_add(rows)
                .is_some_and(|end| end <= self.height),
            "disk image write is out of bounds"
        );
        let expected_len = (self.width as usize)
            .checked_mul(4)
            .and_then(|row_bytes| row_bytes.checked_mul(rows as usize))
            .context("disk image write is too large")?;
        ensure!(
            raw.len() == expected_len,
            "disk image write byte length mismatch"
        );
        let mut file = OpenOptions::new()
            .write(true)
            .open(self.path.as_path())
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        file.seek(SeekFrom::Start(start as u64 * self.width as u64 * 4))
            .context("failed to seek disk image")?;
        file.write_all(raw).context("failed to write disk image")?;
        Ok(())
    }

    pub fn crop_vertical(&self, top: u32, bottom: u32) -> Result<Self> {
        ensure!(
            top < bottom && bottom <= self.height,
            "disk image crop is invalid"
        );
        let cropped = Self::create(self.width, bottom - top)?;
        let rows_per_chunk = self.rows_for_bytes(IO_CHUNK_BYTES);
        let mut source_y = top;
        while source_y < bottom {
            let rows = rows_per_chunk.min(bottom - source_y);
            let image = self.read_rows(source_y, rows)?;
            cropped.write_rows_at(source_y - top, &image)?;
            source_y += rows;
        }
        Ok(cropped)
    }

    pub fn read_rows_scaled(&self, start: u32, rows: u32, target_width: u32) -> Result<RgbaImage> {
        ensure!(rows > 0 && target_width > 0, "disk image scale is empty");
        ensure!(
            start
                .checked_add(rows)
                .is_some_and(|end| end <= self.height),
            "disk image scale is out of bounds"
        );
        let target_height = ((rows as u64 * target_width as u64)
            .div_ceil(self.width.max(1) as u64)
            .clamp(1, u32::MAX as u64)) as u32;
        let len = target_width
            .checked_mul(target_height)
            .and_then(|pixels| pixels.checked_mul(4))
            .context("scaled disk image is too large")? as usize;
        ensure!(
            len <= MAX_READ_BYTES,
            "scaled disk image read exceeds the {:.0} MiB limit",
            MAX_READ_BYTES as f64 / (1024.0 * 1024.0)
        );
        let mut output = vec![0u8; len];
        let row_bytes = self.width as usize * 4;
        let x_offsets: Vec<usize> = (0..target_width)
            .map(|x| {
                ((x as u64 * self.width as u64 / target_width as u64).min((self.width - 1) as u64))
                    as usize
                    * 4
            })
            .collect();
        let mut cached_y = None;
        let mut cached_row = vec![0u8; row_bytes];
        let mut file = File::open(self.path.as_path())
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        for dy in 0..target_height {
            let source_y =
                start + ((dy as u64 * rows as u64 / target_height as u64) as u32).min(rows - 1);
            if cached_y != Some(source_y) {
                file.seek(SeekFrom::Start(source_y as u64 * row_bytes as u64))
                    .context("failed to seek disk image")?;
                file.read_exact(&mut cached_row)
                    .context("failed to read disk image")?;
                cached_y = Some(source_y);
            }
            let dst_start = dy as usize * target_width as usize * 4;
            for (x, &source_x) in x_offsets.iter().enumerate() {
                let dst = dst_start + x * 4;
                output[dst..dst + 4].copy_from_slice(&cached_row[source_x..source_x + 4]);
            }
        }
        RgbaImage::from_raw(target_width, target_height, output)
            .context("scaled disk image buffer size mismatch")
    }

    pub fn thumbnail(&self, max_width: u32, max_height: u32) -> Result<RgbaImage> {
        ensure!(
            max_width > 0 && max_height > 0,
            "thumbnail bounds are empty"
        );
        let scale = (max_width as f64 / self.width as f64)
            .min(max_height as f64 / self.height as f64)
            .min(1.0);
        let width = ((self.width as f64 * scale).round() as u32).max(1);
        let height = ((self.height as f64 * scale).round() as u32).max(1);
        let mut output = vec![0u8; width as usize * height as usize * 4];
        let mut sums = vec![[0u64; 4]; width as usize];
        let row_bytes = self.width as usize * 4;
        let mut row = vec![0u8; row_bytes];
        let x_ranges: Vec<(usize, usize)> = (0..width)
            .map(|x| {
                let x0 = (x as u64 * self.width as u64 / width as u64) as usize;
                let x1 = (((x as u64 + 1) * self.width as u64 / width as u64) as usize)
                    .max(x0 + 1)
                    .min(self.width as usize);
                (x0, x1)
            })
            .collect();
        let file = File::open(self.path.as_path())
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        let mut reader = BufReader::with_capacity(IO_CHUNK_BYTES, file);
        let mut next_row = 0u32;
        for dy in 0..height {
            sums.fill([0; 4]);
            let source_y0 = (dy as u64 * self.height as u64 / height as u64) as u32;
            let source_y1 = (((dy as u64 + 1) * self.height as u64 / height as u64) as u32)
                .max(source_y0 + 1)
                .min(self.height);
            if source_y0 != next_row {
                reader
                    .seek(SeekFrom::Start(source_y0 as u64 * row_bytes as u64))
                    .context("failed to seek disk image")?;
                next_row = source_y0;
            }
            for _source_y in source_y0..source_y1 {
                reader
                    .read_exact(&mut row)
                    .context("failed to read disk image")?;
                next_row += 1;
                for (sum, &(source_x0, source_x1)) in sums.iter_mut().zip(&x_ranges) {
                    for source_x in source_x0..source_x1 {
                        let src = source_x * 4;
                        for channel in 0..4 {
                            sum[channel] += row[src + channel] as u64;
                        }
                    }
                }
            }
            let row_count = (source_y1 - source_y0) as u64;
            for (x, (&sum, &(source_x0, source_x1))) in sums.iter().zip(&x_ranges).enumerate() {
                let count = (source_x1 - source_x0) as u64 * row_count;
                let dst = (dy as usize * width as usize + x) * 4;
                for channel in 0..4 {
                    output[dst + channel] = (sum[channel] / count) as u8;
                }
            }
        }
        RgbaImage::from_raw(width, height, output).context("thumbnail buffer size mismatch")
    }

    pub fn encode_png(&self, path: &Path) -> Result<()> {
        crate::atomic_write(path, |writer| {
            let mut encoder = png::Encoder::new(writer, self.width, self.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder
                .write_header()
                .context("failed to write PNG header")?;
            let mut stream = writer
                .stream_writer_with_size(IO_CHUNK_BYTES)
                .context("failed to create PNG stream")?;
            let mut file = File::open(self.path.as_path())
                .with_context(|| format!("failed to open {}", self.path.display()))?;
            let rows_per_chunk = self.rows_for_bytes(IO_CHUNK_BYTES);
            let row_bytes = self.width as usize * 4;
            let mut buffer = vec![0u8; rows_per_chunk as usize * row_bytes];
            let mut y = 0;
            while y < self.height {
                let rows = rows_per_chunk.min(self.height - y);
                let len = rows as usize * row_bytes;
                file.read_exact(&mut buffer[..len])
                    .context("failed to read disk image")?;
                stream
                    .write_all(&buffer[..len])
                    .context("failed to write PNG pixels")?;
                y += rows;
            }
            stream.finish().context("failed to finish PNG stream")?;
            writer.finish().context("failed to finish PNG")?;
            Ok(())
        })
        .with_context(|| format!("failed to save {}", path.display()))
    }

    fn rows_for_bytes(&self, budget: usize) -> u32 {
        ((budget / (self.width as usize * 4)).max(1) as u32).min(self.height)
    }
}

impl Drop for DiskImage {
    fn drop(&mut self) {
        if std::sync::Arc::strong_count(&self.path) != 1 {
            return;
        }
        if let Err(error) = std::fs::remove_file(self.path.as_path())
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "failed to remove disk image {}: {error}",
                self.path.display()
            );
        }
    }
}

pub fn cleanup_stale_disk_images() {
    cleanup_stale_in(&std::env::temp_dir(), Duration::from_secs(24 * 60 * 60));
}

fn cleanup_stale_in(dir: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("snapit-long-") && name.ends_with(".rgba"))
            || !entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            continue;
        }
        let expired = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > max_age);
        if expired
            && let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "failed to remove stale disk image {}: {error}",
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RgbaImage {
        RgbaImage::from_fn(4, 6, |x, y| {
            if (x + y) % 2 == 0 {
                image::Rgba([x as u8 * 10, y as u8 * 20, 30, 255])
            } else {
                image::Rgba([200, 100, 50, 128])
            }
        })
    }

    #[test]
    fn disk_image_round_trips_rows_and_crops() {
        let source = sample();
        let disk = DiskImage::from_image(&source).unwrap();
        assert_eq!(disk.dimensions(), (4, 6));
        assert_eq!(
            disk.read_rows(1, 3).unwrap(),
            image::imageops::crop_imm(&source, 0, 1, 4, 3).to_image()
        );
        let cropped = disk.crop_vertical(2, 5).unwrap();
        assert_eq!(cropped.dimensions(), (4, 3));
        assert_eq!(
            cropped.read_rows(0, 3).unwrap(),
            image::imageops::crop_imm(&source, 0, 2, 4, 3).to_image()
        );
    }

    #[test]
    fn disk_image_streams_png_without_full_image_bytes() {
        let source = sample();
        let disk = DiskImage::from_image(&source).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.png");
        std::fs::write(&path, b"old file").unwrap();
        disk.encode_png(&path).unwrap();
        let decoded = image::open(&path).unwrap().to_rgba8();
        let streamed = DiskImage::from_png(&path).unwrap();
        assert_eq!(streamed.read_rows(0, source.height()).unwrap(), source);
        assert_eq!(decoded, source);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn disk_image_streams_png_across_multiple_chunks() {
        let source = RgbaImage::from_fn(512, 4097, |x, y| {
            image::Rgba([x as u8, y as u8, (x ^ y) as u8, 255])
        });
        let disk = DiskImage::from_image(&source).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.png");
        disk.encode_png(&path).unwrap();
        let decoded = image::open(&path).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (512, 4097));
        assert_eq!(decoded, source);
    }

    #[test]
    fn disk_image_streams_png_beyond_the_former_scroll_height_limit() {
        let source = RgbaImage::from_fn(64, 30_047, |x, y| {
            image::Rgba([x as u8, y as u8, (x ^ y) as u8, 255])
        });
        let disk = DiskImage::from_image(&source).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unbounded-scroll.png");

        disk.encode_png(&path).unwrap();

        let streamed = DiskImage::from_png(&path).unwrap();
        assert_eq!(streamed.dimensions(), (64, 30_047));
        assert_eq!(streamed.read_rows(0, 30_047).unwrap(), source);
    }

    #[test]
    fn disk_image_rejects_oversized_reads_before_allocating() {
        let disk = DiskImage::create(4096, 5000).unwrap();
        let error = disk.read_rows(0, 5000).unwrap_err();
        assert!(error.to_string().contains("64 MiB"));
    }

    #[test]
    fn disk_image_scales_and_thumbnails_with_bounded_output() {
        let disk = DiskImage::from_image(&sample()).unwrap();
        assert_eq!(disk.read_rows_scaled(0, 6, 2).unwrap().dimensions(), (2, 3));
        let thumbnail = disk.thumbnail(2, 2).unwrap();
        assert_eq!(thumbnail.dimensions(), (1, 2));
        assert!(thumbnail.pixels().all(|pixel| pixel[3] > 0));
    }

    #[test]
    fn disk_image_thumbnail_preserves_opaque_pixel_values() {
        let source = RgbaImage::from_pixel(8, 12, image::Rgba([40, 42, 48, 255]));
        let disk = DiskImage::from_image(&source).unwrap();

        let thumbnail = disk.thumbnail(2, 2).unwrap();

        assert_eq!(thumbnail.dimensions(), (1, 2));
        assert!(
            thumbnail
                .pixels()
                .all(|pixel| *pixel == image::Rgba([40, 42, 48, 255]))
        );
    }

    #[test]
    fn disk_image_create_removes_the_file_when_sizing_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unsized.rgba");
        drop(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap(),
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        }
        #[cfg(windows)]
        {
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(&path, permissions).unwrap();
        }
        let file = OpenOptions::new().read(true).open(&path).unwrap();
        let error = DiskImage::from_created_file(path.clone(), 2, 2, 16, file)
            .err()
            .expect("read-only file cannot be sized");
        assert!(error.to_string().contains("failed to size"), "{error:#}");
        assert!(!path.exists());
    }

    #[test]
    fn disk_image_removes_its_temporary_file_on_drop() {
        let disk = DiskImage::create(2, 2).unwrap();
        let path = PathBuf::from(disk.path.as_path());
        assert!(path.exists());
        drop(disk);
        assert!(!path.exists());
    }

    #[test]
    fn disk_image_keeps_shared_file_until_the_last_owner_drops() {
        let disk = DiskImage::create(2, 2).unwrap();
        let path = PathBuf::from(disk.path.as_path());
        let clone = disk.clone();
        drop(disk);
        assert!(path.exists());
        assert_eq!(clone.read_rows(0, 2).unwrap().dimensions(), (2, 2));
        drop(clone);
        assert!(!path.exists());
    }

    #[test]
    fn stale_cleanup_removes_only_expired_managed_regular_files() {
        use std::fs::FileTimes;

        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("snapit-long-1-1.rgba");
        let recent = dir.path().join("snapit-long-2-2.rgba");
        let unmanaged = dir.path().join("other.rgba");
        std::fs::write(&stale, b"stale").unwrap();
        std::fs::write(&recent, b"recent").unwrap();
        std::fs::write(&unmanaged, b"keep").unwrap();
        let old = SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
        File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_times(FileTimes::new().set_modified(old))
            .unwrap();

        cleanup_stale_in(dir.path(), Duration::from_secs(24 * 60 * 60));

        assert!(!stale.exists());
        assert!(recent.exists());
        assert!(unmanaged.exists());
    }
}
