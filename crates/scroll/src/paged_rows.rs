use std::{collections::VecDeque, sync::Arc};

use anyhow::{Context as _, Result, ensure};
use snapit_capture::DiskImage;

const SEGMENT_ROWS: u32 = 512;
const CACHE_BYTES: usize = 32 * 1024 * 1024;
const CACHE_ROWS: u32 = 256;

#[derive(Clone)]
struct Segment {
    id: u64,
    start: i64,
    rows: u32,
    image: DiskImage,
}

#[derive(Clone)]
struct CachedRows {
    segment: u64,
    start: u32,
    rows: u32,
    bytes: Arc<Vec<u8>>,
}

pub struct PagedRows {
    width: u32,
    segments: Vec<Segment>,
    top: i64,
    rows: u32,
    cache: VecDeque<CachedRows>,
    cache_bytes: usize,
    next_id: u64,
}

impl PagedRows {
    pub fn new(width: u32) -> Self {
        Self {
            width,
            segments: Vec::new(),
            top: 0,
            rows: 0,
            cache: VecDeque::new(),
            cache_bytes: 0,
            next_id: 1,
        }
    }

    pub fn len(&self) -> u32 {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    pub fn read_row(&mut self, y: i32) -> Result<Vec<u8>> {
        let offset = i64::from(y) - self.top;
        if offset < 0 || offset >= i64::from(self.rows) {
            anyhow::bail!("paged row {y} is outside the canvas");
        }
        let mut output = vec![0; self.width as usize * 4];
        self.copy_to(y, 1, &mut output)?;
        Ok(output)
    }

    pub fn read_rows(&mut self, start: i32, rows: u32) -> Result<image::RgbaImage> {
        ensure!(rows > 0, "paged row read is empty");
        let bytes = (self.width as usize)
            .checked_mul(4)
            .and_then(|stride| stride.checked_mul(rows as usize))
            .context("paged row read is too large")?;
        ensure!(bytes <= 64 * 1024 * 1024, "paged row read exceeds 64 MiB");
        let mut output = vec![0u8; bytes];
        self.copy_to(start, rows, &mut output)?;
        Ok(image::RgbaImage::from_raw(self.width, rows, output).unwrap())
    }

    pub fn write_row(&mut self, y: i32, pixels: &[u8]) -> Result<()> {
        self.write_rows(y, 1, pixels)
    }

    pub fn write_rows(&mut self, start: i32, rows: u32, pixels: &[u8]) -> Result<()> {
        let stride = self.width as usize * 4;
        ensure!(rows > 0, "paged row write is empty");
        let expected = stride
            .checked_mul(rows as usize)
            .context("paged row write is too large")?;
        ensure!(pixels.len() == expected, "paged row write size mismatch");
        let end = i64::from(start)
            .checked_add(i64::from(rows))
            .context("paged row write range overflows")?;
        ensure!(
            i64::from(start) >= self.top && end <= self.top + i64::from(self.rows),
            "paged row write is outside the canvas"
        );
        let mut index = self.segment_index(start)?;
        let mut done = 0u32;
        while done < rows {
            let (id, image, segment_start, segment_rows) = {
                let segment = &self.segments[index];
                (
                    segment.id,
                    segment.image.clone(),
                    segment.start,
                    segment.rows,
                )
            };
            let local = (i64::from(start) + i64::from(done) - segment_start) as u32;
            let take = (rows - done).min(segment_rows - local);
            let offset = done as usize * stride;
            let source = image::RgbaImage::from_raw(
                self.width,
                take,
                pixels[offset..offset + take as usize * stride].to_vec(),
            )
            .unwrap();
            image
                .write_rows_at(local, &source)
                .context("failed to overwrite stitched rows")?;
            self.invalidate_range(id, local, take);
            done += take;
            if local + take == segment_rows {
                index += 1;
            }
        }
        Ok(())
    }

    pub fn append_down(&mut self, start: i32, pixels: &[u8]) -> Result<()> {
        self.append(start, false, pixels)
    }

    pub fn prepend_up(&mut self, end: i32, pixels: &[u8]) -> Result<()> {
        self.append(end, true, pixels)
    }

    fn append(&mut self, boundary: i32, prepend: bool, pixels: &[u8]) -> Result<()> {
        let stride = self.width as usize * 4;
        if pixels.is_empty() || !pixels.len().is_multiple_of(stride) {
            anyhow::bail!("invalid paged row batch");
        }
        let count = (pixels.len() / stride) as u32;
        let expected_top = if prepend {
            i64::from(boundary) - i64::from(count)
        } else {
            i64::from(boundary)
        };
        if !self.is_empty() {
            if prepend {
                ensure!(
                    i64::from(boundary) == self.top,
                    "paged prepend is not adjacent: boundary={boundary}, top={}, rows={}",
                    self.top,
                    self.rows
                );
            } else {
                ensure!(
                    i64::from(boundary) == self.top + i64::from(self.rows),
                    "paged append is not adjacent: boundary={boundary}, top={}, rows={}",
                    self.top,
                    self.rows
                );
            }
        }
        let mut made = Vec::new();
        let segment_rows = (8 * 1024 * 1024 / stride).clamp(1, SEGMENT_ROWS as usize) as u32;
        for (chunk, bytes) in pixels.chunks(stride * segment_rows as usize).enumerate() {
            let rows = (bytes.len() / stride) as u32;
            let local_start = chunk as u32 * segment_rows;
            let start = expected_top + i64::from(local_start);
            let image = DiskImage::create(self.width, rows)?;
            let full = image::RgbaImage::from_raw(self.width, rows, bytes.to_vec()).unwrap();
            image
                .write_rows_at(0, &full)
                .context("failed to write stitched rows")?;
            made.push(Segment {
                id: self.next_id,
                start,
                rows,
                image,
            });
            self.next_id += 1;
        }
        if self.is_empty() || prepend {
            self.top = expected_top;
        }
        self.rows += count;
        if prepend {
            let mut segments = made;
            segments.append(&mut self.segments);
            self.segments = segments;
        } else {
            self.segments.extend(made);
        }
        Ok(())
    }

    pub fn trim_top(&mut self, count: u32) -> Result<()> {
        let count = count.min(self.rows);
        let mut left = count;
        let mut replacements = Vec::with_capacity(self.segments.len());
        let mut invalidated = Vec::new();
        for segment in &self.segments {
            if left == 0 {
                replacements.push(segment.clone());
                continue;
            }
            let take = left.min(segment.rows);
            if take == segment.rows {
                invalidated.push(segment.id);
            } else {
                let replacement = DiskImage::create(self.width, segment.rows - take)?;
                let rows = segment
                    .image
                    .read_rows(take, segment.rows - take)?
                    .into_raw();
                let image =
                    image::RgbaImage::from_raw(self.width, segment.rows - take, rows).unwrap();
                replacement.write_rows_at(0, &image)?;
                invalidated.push(segment.id);
                replacements.push(Segment {
                    id: segment.id,
                    start: segment.start + i64::from(take),
                    rows: segment.rows - take,
                    image: replacement,
                });
            }
            left -= take;
        }
        ensure!(left == 0, "paged trim exceeds the canvas");
        for id in invalidated {
            self.invalidate_segment(id);
        }
        self.segments = replacements;
        self.top += i64::from(count);
        self.rows -= count;
        Ok(())
    }

    pub fn trim_top_keep_origin(&mut self, count: u32) -> Result<()> {
        self.trim_top_with_origin_shift(count, 0)
    }

    pub fn trim_top_with_origin_shift(&mut self, count: u32, shift: u32) -> Result<()> {
        let count = count.min(self.rows);
        ensure!(
            shift <= count,
            "paged trim origin shift exceeds removed rows"
        );
        let origin = self.top;
        self.trim_top(count)?;
        let adjustment = i64::from(shift) - i64::from(count);
        for segment in &mut self.segments {
            segment.start += adjustment;
        }
        self.top = origin + i64::from(shift);
        Ok(())
    }

    pub fn trim_bottom(&mut self, count: u32) -> Result<()> {
        let count = count.min(self.rows);
        let mut left = count;
        let mut replacements = Vec::with_capacity(self.segments.len());
        let mut invalidated = Vec::new();
        for segment in self.segments.iter().rev() {
            if left == 0 {
                replacements.push(segment.clone());
                continue;
            }
            let take = left.min(segment.rows);
            if take == segment.rows {
                invalidated.push(segment.id);
            } else {
                let replacement = DiskImage::create(self.width, segment.rows - take)?;
                let rows = segment.image.read_rows(0, segment.rows - take)?.into_raw();
                let image =
                    image::RgbaImage::from_raw(self.width, segment.rows - take, rows).unwrap();
                replacement.write_rows_at(0, &image)?;
                invalidated.push(segment.id);
                replacements.push(Segment {
                    id: segment.id,
                    start: segment.start,
                    rows: segment.rows - take,
                    image: replacement,
                });
            }
            left -= take;
        }
        ensure!(left == 0, "paged trim exceeds the canvas");
        replacements.reverse();
        for id in invalidated {
            self.invalidate_segment(id);
        }
        self.segments = replacements;
        self.rows -= count;
        Ok(())
    }

    pub fn copy_to(&mut self, start: i32, rows: u32, output: &mut [u8]) -> Result<()> {
        let stride = self.width as usize * 4;
        if output.len() != rows as usize * stride {
            anyhow::bail!("paged output size mismatch");
        }
        let offset = i64::from(start) - self.top;
        if offset < 0 || offset + i64::from(rows) > i64::from(self.rows) {
            anyhow::bail!("paged range is outside the canvas");
        }
        let index = self.segment_index(start)?;
        let mut done = 0u32;
        let mut segment_index = index;
        while done < rows {
            let (id, image, segment_start, segment_rows) = {
                let segment = &self.segments[segment_index];
                (
                    segment.id,
                    segment.image.clone(),
                    segment.start,
                    segment.rows,
                )
            };
            let local = (i64::from(start) + i64::from(done) - segment_start).max(0) as u32;
            let cache_left = CACHE_ROWS - local % CACHE_ROWS;
            let take = (rows - done).min(segment_rows - local).min(cache_left);
            let cached = self.cached(id, &image, local)?;
            let src =
                &cached.bytes[(local - cached.start) as usize * stride..][..take as usize * stride];
            let dst = done as usize * stride..(done + take) as usize * stride;
            output[dst].copy_from_slice(src);
            done += take;
            if local + take == segment_rows {
                segment_index += 1;
            }
        }
        Ok(())
    }

    pub fn assemble(&mut self) -> Result<Option<snapit_capture::DiskImage>> {
        if self.is_empty() {
            return Ok(None);
        }
        let image = DiskImage::create(self.width, self.rows)?;
        let rows_per_batch = (8 * 1024 * 1024 / (self.width as usize * 4).max(1))
            .clamp(1, SEGMENT_ROWS as usize) as u32;
        let mut buffer = vec![0u8; self.width as usize * 4 * rows_per_batch as usize];
        let mut start = self.top;
        for offset in (0..self.rows).step_by(rows_per_batch as usize) {
            let rows = rows_per_batch.min(self.rows - offset);
            self.copy_to(
                start as i32,
                rows,
                &mut buffer[..rows as usize * self.width as usize * 4],
            )?;
            let source = image::RgbaImage::from_raw(
                self.width,
                rows,
                buffer[..rows as usize * self.width as usize * 4].to_vec(),
            )
            .unwrap();
            image.write_rows_at(offset, &source)?;
            start += i64::from(rows);
        }
        Ok(Some(image))
    }

    pub fn cached_bytes(&self) -> usize {
        self.cache_bytes
    }

    fn segment_index(&self, y: i32) -> Result<usize> {
        let y = i64::from(y);
        let index = self
            .segments
            .iter()
            .rposition(|segment| segment.start <= y)
            .context("missing canvas segment")?;
        let segment = self.segments.get(index).context("missing canvas segment")?;
        ensure!(
            y >= segment.start && y < segment.start + i64::from(segment.rows),
            "row {y} missing from segment {}",
            index
        );
        Ok(index)
    }

    fn cached(&mut self, segment_id: u64, image: &DiskImage, start: u32) -> Result<CachedRows> {
        let chunk = start / CACHE_ROWS;
        let chunk_start = chunk * CACHE_ROWS;
        let chunk_rows = CACHE_ROWS.min(image.height() - chunk_start);
        if let Some(index) = self
            .cache
            .iter()
            .position(|entry| entry.segment == segment_id && entry.start == chunk_start)
        {
            let entry = self.cache.remove(index).unwrap();
            self.cache.push_back(entry.clone());
            return Ok(entry);
        }
        let bytes = image
            .read_rows(chunk_start, chunk_rows)
            .context("failed to read stitched rows")?
            .into_raw();
        self.cache_bytes += bytes.len();
        let entry = CachedRows {
            segment: segment_id,
            start: chunk_start,
            rows: chunk_rows,
            bytes: Arc::new(bytes),
        };
        self.cache.push_back(entry.clone());
        while self.cache_bytes > CACHE_BYTES {
            let old = self.cache.pop_front().unwrap();
            self.cache_bytes -= old.bytes.len();
        }
        Ok(entry)
    }

    fn invalidate_range(&mut self, segment: u64, start: u32, rows: u32) {
        let end = start.saturating_add(rows);
        self.cache.retain(|entry| {
            let keep =
                entry.segment != segment || end <= entry.start || start >= entry.start + entry.rows;
            if !keep {
                self.cache_bytes -= entry.bytes.len();
            }
            keep
        });
    }

    fn invalidate_segment(&mut self, segment: u64) {
        self.cache.retain(|entry| {
            let keep = entry.segment != segment;
            if !keep {
                self.cache_bytes -= entry.bytes.len();
            }
            keep
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(width: u32, value: u8) -> Vec<u8> {
        vec![value; width as usize * 4]
    }

    #[test]
    fn appends_prepends_overwrites_and_trims_across_segments() {
        let mut rows = PagedRows::new(3);
        let mut down = Vec::new();
        for value in 0u16..600 {
            down.extend(row(3, value as u8));
        }
        rows.append_down(0, &down).unwrap();
        let mut up = Vec::new();
        for value in (0u16..600).rev() {
            up.extend(row(3, value as u8));
        }
        rows.prepend_up(0, &up).unwrap();
        assert_eq!(rows.len(), 1200);
        for y in -600..600 {
            let expected = if y < 0 { (-y - 1) as u8 } else { y as u8 };
            assert_eq!(rows.read_row(y).unwrap(), row(3, expected), "row {y}");
        }
        rows.write_row(-343, &row(3, 201)).unwrap();
        rows.write_row(341, &row(3, 202)).unwrap();
        rows.trim_top(257).unwrap();
        rows.trim_bottom(258).unwrap();
        assert_eq!(rows.len(), 685);
        assert_eq!(rows.read_row(-343).unwrap(), row(3, 201));
        assert_eq!(rows.read_row(341).unwrap(), row(3, 202));
        let disk = rows.assemble().unwrap().unwrap();
        let output = disk.read_rows(0, 685).unwrap().into_raw();
        assert_eq!(&output[..12], row(3, 201).as_slice());
        assert_eq!(&output[684 * 12..], row(3, 202).as_slice());
    }

    #[test]
    fn batch_overwrite_crosses_segments_and_invalidates_cached_rows() {
        let mut rows = PagedRows::new(3);
        let mut pixels = Vec::new();
        for value in 0u16..600 {
            pixels.extend(row(3, value as u8));
        }
        rows.append_down(0, &pixels).unwrap();
        rows.read_row(500).unwrap();
        assert!(rows.cached_bytes() > 0);
        let mut replacement = Vec::new();
        for value in 200u16..300 {
            replacement.extend(row(3, value as u8));
        }
        rows.write_rows(500, 100, &replacement).unwrap();
        for y in 500..600 {
            assert_eq!(rows.read_row(y).unwrap(), row(3, (y - 300) as u8));
        }
        assert_eq!(rows.read_row(499).unwrap(), row(3, 243));
        assert_eq!(rows.read_row(0).unwrap(), row(3, 0));
    }

    #[test]
    fn overwrites_outside_the_canvas_are_rejected_without_panicking() {
        let mut rows = PagedRows::new(3);
        rows.append_down(0, &row(3, 7)).unwrap();
        assert!(rows.write_rows(0, 2, &row(3, 7)).is_err());
        assert!(rows.write_rows(-1, 1, &row(3, 7)).is_err());
        assert_eq!(rows.read_row(0).unwrap(), row(3, 7));
    }

    #[test]
    fn trimming_past_the_end_clamps_the_origin_shift() {
        let mut rows = PagedRows::new(3);
        rows.append_down(-10, &row(3, 7)).unwrap();
        rows.trim_top_keep_origin(100).unwrap();
        assert!(rows.is_empty());
        assert_eq!(
            rows.read_row(-10).unwrap_err().to_string(),
            "paged row -10 is outside the canvas"
        );
    }

    #[test]
    fn cache_stays_bounded_after_repeated_random_reads() {
        let mut rows = PagedRows::new(256);
        let pixels = vec![77u8; 256 * 4 * 2048];
        rows.append_down(0, &pixels).unwrap();
        for y in (0..2048).rev() {
            rows.read_row(y).unwrap();
        }
        assert!(rows.cached_bytes() <= CACHE_BYTES);
    }
}
