//! The container half of the HDR signalling.
//!
//! Media Foundation's MPEG-4 muxer may omit `colr`, and the HEVC encoder may
//! leave the colour description out of the bitstream's VUI. The result can be a
//! Main10 PQ stream nothing can identify: a player defaults to BT.709 gamma,
//! shows it as a dark, washed-out SDR clip and never asks the display to switch
//! modes. The finished file is inspected and tagged by hand when necessary.
//!
//! Only the tail of the file is rewritten. The writer puts `mdat` before
//! `moov`, so a box inserted inside `moov` moves no sample data and the
//! absolute chunk offsets in `stco` stay correct — which is why [`insert`]
//! refuses a file laid out any other way.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context as _, Result, anyhow, bail};

/// The colour signalling of a `colr` box of type `nclx` (ISO/IEC 23001-8):
/// three code tables plus the sample range.
pub(crate) struct Nclx {
    pub(crate) primaries: u16,
    pub(crate) transfer: u16,
    pub(crate) matrix: u16,
    /// `false` for the video range the 10-bit encoder is told to write.
    pub(crate) full_range: bool,
}

impl Nclx {
    /// HDR10 as this pipeline encodes it: BT.2020 primaries, ST 2084 PQ, the
    /// BT.2020 non-constant-luminance matrix, samples in the 64..940 range.
    pub(crate) const HDR10: Nclx = Nclx {
        primaries: 9,
        transfer: 16,
        matrix: 9,
        full_range: false,
    };

    fn box_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(19);
        bytes.extend_from_slice(&[0, 0, 0, 19]);
        bytes.extend_from_slice(b"colrnclx");
        bytes.extend_from_slice(&self.primaries.to_be_bytes());
        bytes.extend_from_slice(&self.transfer.to_be_bytes());
        bytes.extend_from_slice(&self.matrix.to_be_bytes());
        bytes.push(u8::from(self.full_range) << 7);
        bytes
    }
}

/// A box in a buffer: header start, contents start, end, and header length
/// (16 when a 32-bit `size` of 1 is followed by a 64-bit `largesize`).
struct Found {
    start: usize,
    payload: usize,
    end: usize,
    header: usize,
}

impl Found {
    fn fourcc(&self, bytes: &[u8]) -> Option<[u8; 4]> {
        Some(
            bytes
                .get(self.start + 4..self.start + 8)?
                .try_into()
                .unwrap(),
        )
    }
}

/// The video sample entry plus every box enclosing it, in the order a size
/// rewrite needs them.
struct Entry {
    payload: usize,
    end: usize,
    ancestors: Vec<usize>,
}

/// The 78 bytes a `VisualSampleEntry` puts between its header and its children.
const VISUAL_ENTRY_BASE: usize = 78;

/// Add the `colr` box to the video sample entry of a finished file. The frames
/// are already encoded by the time this runs, so the caller logs a failure and
/// keeps the recording rather than discarding it over metadata.
pub(crate) fn insert(path: &Path, colour: &Nclx) -> Result<()> {
    let mut file =
        File::open(path).with_context(|| format!("failed to reopen {}", path.display()))?;
    let length = usize::try_from(file.metadata()?.len()).context("file size")?;

    // The top-level walk reads one header at a time; `mdat` can be gigabytes.
    let mut at = 0usize;
    let (mut mdat, mut moov) = (None, None);
    while at + 8 <= length {
        file.seek(SeekFrom::Start(at as u64))?;
        let mut header = [0u8; 16];
        if file.read(&mut header)? < 8 {
            break;
        }
        let size = resolve(&header, 0, length - at).ok_or_else(|| anyhow!("box at {at}"))?;
        if size.end == 0 {
            bail!("a zero-length box at {at}");
        }
        match header[4..8] {
            [b'm', b'd', b'a', b't'] => mdat = Some(at),
            [b'm', b'o', b'o', b'v'] => moov = Some((at, size.header, size.end)),
            _ => {}
        }
        // `end` is relative to the header just read, since the walk resolves
        // each box in a buffer of its own.
        at += size.end;
    }
    let (moov_at, moov_header, moov_end) = moov.ok_or_else(|| anyhow!("no moov box"))?;
    // Chunk offsets are absolute, so inserting ahead of the sample data would
    // silently move every frame.
    if !matches!(mdat, Some(mdat) if mdat < moov_at) {
        bail!("the sample data does not precede the metadata; leaving the file alone");
    }

    // Everything from `moov` on is metadata, small enough to patch in memory,
    // and with the moov at offset 0 of it the walk needs no file offsets. The
    // trailing boxes a writer may add after it shift right with the patch.
    let mut tail = Vec::with_capacity(length - moov_at);
    file.seek(SeekFrom::Start(moov_at as u64))?;
    file.read_to_end(&mut tail)?;

    let entry = video_sample_entry(&tail, moov_header, moov_end)?;
    let base = entry.payload;
    let existing = child(&tail, base, entry.end, b"colr");
    let (insert_at, replaced) = match &existing {
        Some(found) => (found.start, found.end - found.start),
        None => (base, 0),
    };
    let added = colour.box_bytes();
    if replaced == added.len() && tags(&tail, insert_at, colour) {
        return Ok(());
    }

    let before = tail.len();
    tail.splice(insert_at..insert_at + replaced, added);
    let delta = tail.len() as i64 - before as i64;
    for start in entry.ancestors {
        patch_ancestor_size(&mut tail, start, delta)?;
    }

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let (temporary_path, mut temporary) = loop {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let candidate = parent.unwrap_or(Path::new(".")).join(format!(
            ".{name}.{}.{}.colr.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err).with_context(|| "failed to create the colour-tag patch"),
        }
    };
    let result = (|| {
        file.seek(SeekFrom::Start(0))?;
        let mut prefix = (&mut file).take(moov_at as u64);
        std::io::copy(&mut prefix, &mut temporary)?;
        temporary.write_all(&tail)?;
        temporary.sync_all()?;
        drop(temporary);
        drop(file);
        replace_file(path, &temporary_path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    result
}

fn patch_ancestor_size(bytes: &mut [u8], start: usize, delta: i64) -> Result<()> {
    let raw = u32::from_be_bytes(
        bytes
            .get(start..start + 4)
            .ok_or_else(|| anyhow!("no box header at {start}"))?
            .try_into()
            .map_err(|_| anyhow!("no box header at {start}"))?,
    );
    if raw == 0 {
        // A zero-sized box extends to its parent's end; growing the parent
        // grows this box without changing its header.
        return Ok(());
    }
    if raw == 1 {
        let at = start + 8;
        let large = u64::from_be_bytes(
            bytes
                .get(at..at + 8)
                .ok_or_else(|| anyhow!("no large box size at {start}"))?
                .try_into()
                .map_err(|_| anyhow!("no large box size at {start}"))?,
        );
        let patched = large
            .checked_add_signed(delta)
            .ok_or_else(|| anyhow!("the metadata no longer fits a 64-bit box size"))?;
        bytes[at..at + 8].copy_from_slice(&patched.to_be_bytes());
    } else {
        let patched = i64::from(raw)
            .checked_add(delta)
            .and_then(|size| u32::try_from(size).ok())
            .ok_or_else(|| anyhow!("the metadata no longer fits a 32-bit box size"))?;
        bytes[start..start + 4].copy_from_slice(&patched.to_be_bytes());
    }
    Ok(())
}

fn replace_file(path: &Path, temporary: &Path) -> Result<()> {
    use windows::{
        Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW},
        core::HSTRING,
    };
    unsafe {
        ReplaceFileW(
            &HSTRING::from(path.as_os_str()),
            &HSTRING::from(temporary.as_os_str()),
            None,
            REPLACEFILE_WRITE_THROUGH,
            None,
            None,
        )?;
    }
    Ok(())
}

/// The `colr` box's own values, read back so a second pass changes nothing.
fn tags(bytes: &[u8], at: usize, colour: &Nclx) -> bool {
    let Some(payload) = bytes.get(at + 8..at + 19) else {
        return false;
    };
    payload[..4] == *b"nclx"
        && u16::from_be_bytes(payload[4..6].try_into().unwrap()) == colour.primaries
        && u16::from_be_bytes(payload[6..8].try_into().unwrap()) == colour.transfer
        && u16::from_be_bytes(payload[8..10].try_into().unwrap()) == colour.matrix
        // The range is the top bit; the seven below it are reserved as zero.
        && payload[10] == (u8::from(colour.full_range) << 7)
}

/// Walk `moov` down to the codec's sample entry the way a demuxer finds it.
fn video_sample_entry(bytes: &[u8], moov_header: usize, moov_end: usize) -> Result<Entry> {
    let mut ancestors = vec![0];
    let mut parent = Found {
        start: 0,
        payload: moov_header,
        end: moov_end,
        header: moov_header,
    };
    for fourcc in [b"trak", b"mdia", b"minf", b"stbl"] {
        let name = String::from_utf8_lossy(fourcc).into_owned();
        parent = child(bytes, parent.payload, parent.end, fourcc)
            .ok_or_else(|| anyhow!("no {name} box"))?;
        ancestors.push(parent.start);
    }
    let mut stsd =
        child(bytes, parent.payload, parent.end, b"stsd").ok_or_else(|| anyhow!("no stsd box"))?;
    ancestors.push(stsd.start);
    // A `SampleTableBox` is a full box: version and flags, then an entry count.
    stsd.payload += 8;
    for fourcc in [b"hvc1", b"hev1", b"avc1", b"avc3"] {
        if let Some(found) = child(bytes, stsd.payload, stsd.end, fourcc) {
            ancestors.push(found.start);
            return Ok(Entry {
                payload: found.start + found.header + VISUAL_ENTRY_BASE,
                end: found.end,
                ancestors,
            });
        }
    }
    bail!("no video sample entry to tag")
}

/// The first box of this type in `[start, end)`.
fn child(bytes: &[u8], start: usize, end: usize, fourcc: &[u8; 4]) -> Option<Found> {
    let mut at = start;
    loop {
        let found = resolve(bytes, at, end)?;
        if found.fourcc(bytes) == Some(*fourcc) {
            return Some(found);
        }
        if found.end <= at {
            return None;
        }
        at = found.end;
    }
}

/// A box header at `at`, clamped so a child cannot claim more than its parent.
fn resolve(bytes: &[u8], at: usize, end: usize) -> Option<Found> {
    let raw = u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().unwrap()) as usize;
    let (size, header) = if raw == 1 {
        let large = bytes.get(at + 8..at + 16)?.try_into().unwrap();
        (usize::try_from(u64::from_be_bytes(large)).ok()?, 16)
    } else {
        (raw, 8)
    };
    let end = if raw == 0 { end } else { (at + size).min(end) };
    Some(Found {
        start: at,
        payload: at + header,
        end,
        header,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A file laid out the way the muxer writes one, its sample entry holding
    /// the codec configuration and optionally a `colr` already.
    fn sample_file(colr: Option<&[u8]>) -> Vec<u8> {
        let mut entry = vec![0u8; VISUAL_ENTRY_BASE];
        entry[24..26].copy_from_slice(&640u16.to_be_bytes());
        entry[26..28].copy_from_slice(&360u16.to_be_bytes());
        if let Some(colr) = colr {
            entry.extend_from_slice(colr);
        }
        entry.extend_from_slice(&box_of(b"hvcC", &[1, 2, 3]));
        let entry = box_of(b"hvc1", &entry);
        let mut body = vec![0, 0, 0, 0, 0, 0, 0, 1];
        body.extend_from_slice(&entry);
        let stsd = box_of(b"stsd", &body);
        let stbl = box_of(b"stbl", &stsd);
        let minf = box_of(b"minf", &stbl);
        let mdia = box_of(b"mdia", &minf);
        let trak = box_of(b"trak", &mdia);
        let mut moov = box_of(b"moov", &trak);
        moov.extend_from_slice(&box_of(b"free", b""));
        let mut file = box_of(b"ftyp", b"isom");
        file.extend_from_slice(&mdat(200));
        file.extend_from_slice(&moov);
        file
    }

    fn box_of(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + payload.len());
        bytes.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
        bytes.extend_from_slice(fourcc);
        bytes.extend_from_slice(payload);
        bytes
    }

    fn mdat(len: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; len + 8];
        bytes[..4].copy_from_slice(&((len + 8) as u32).to_be_bytes());
        bytes[4..8].copy_from_slice(b"mdat");
        bytes[8..].fill(0x5a);
        bytes
    }

    /// Patch a synthetic file and read the result back.
    fn roundtrip(colr: Option<Vec<u8>>, colour: &Nclx) -> (Vec<u8>, Vec<u8>) {
        // Tests run in parallel and share a process, so every call needs its
        // own file or one test reads another's half-written bytes.
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "snapit-colr-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("case.mp4");
        let before = sample_file(colr.as_deref());
        std::fs::write(&path, &before).unwrap();
        insert(&path, colour).unwrap();
        let after = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        (before, after)
    }

    fn nclx_in(bytes: &[u8]) -> Option<(u16, u16, u16, u8)> {
        // The search lands on the `colr` fourcc, so the box starts four bytes
        // earlier and its `nclx` payload at `at + 8`.
        let at = bytes.windows(4).position(|w| w == b"colr")? - 4;
        let payload = bytes.get(at + 8..at + 19)?;
        Some((
            u16::from_be_bytes(payload[4..6].try_into().unwrap()),
            u16::from_be_bytes(payload[6..8].try_into().unwrap()),
            u16::from_be_bytes(payload[8..10].try_into().unwrap()),
            payload[10] >> 7,
        ))
    }

    fn position(bytes: &[u8], fourcc: &[u8; 4]) -> usize {
        bytes.windows(4).position(|w| w == fourcc).unwrap()
    }

    #[test]
    fn inserts_the_box_before_the_codec_configuration() {
        let (before, after) = roundtrip(None, &Nclx::HDR10);
        assert_eq!(nclx_in(&after), Some((9, 16, 9, 0)));
        assert!(
            position(&after, b"colr") < position(&after, b"hvcC"),
            "a colr after hvcC is out of spec order"
        );
        assert_eq!(after.len(), before.len() + 19);
        // The mdat is untouched, and so is everything ahead of the moov.
        let moov = position(&before, b"moov") - 4;
        assert_eq!(&after[..moov], &before[..moov]);
    }

    #[test]
    fn every_enclosing_box_grows() {
        let (before, after) = roundtrip(None, &Nclx::HDR10);
        for fourcc in [
            b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd", b"hvc1",
        ] {
            let was_at = position(&before, fourcc) - 4;
            let was = u32::from_be_bytes(before[was_at..was_at + 4].try_into().unwrap());
            let now_at = position(&after, fourcc) - 4;
            let now = u32::from_be_bytes(after[now_at..now_at + 4].try_into().unwrap());
            assert_eq!(
                now,
                was + 19,
                "{} did not grow to hold the box",
                String::from_utf8_lossy(fourcc)
            );
        }
        assert_fills(&after, 0, after.len());
    }

    /// Walk a box chain the way a demuxer does: every declared size must stay
    /// inside its parent, and every container must be filled exactly by its
    /// children — a size the patch forgot to grow shows up here.
    fn assert_fills(bytes: &[u8], start: usize, end: usize) {
        let mut at = start;
        while at < end {
            assert!(at + 8 <= end, "truncated box header at {at}");
            let mut size = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            if size == 1 {
                size = usize::try_from(u64::from_be_bytes(
                    bytes[at + 8..at + 16].try_into().unwrap(),
                ))
                .unwrap();
            }
            assert!(size >= 8, "undersized box at {at}");
            assert!(at + size <= end, "the box at {at} overruns its parent");
            let fourcc: [u8; 4] = bytes[at + 4..at + 8].try_into().unwrap();
            match &fourcc {
                b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" => {
                    assert_fills(bytes, at + 8, at + size);
                }
                // The sample description is a full box ahead of its entries.
                b"stsd" => assert_fills(bytes, at + 16, at + size),
                // A VisualSampleEntry's children follow its 78-byte base.
                b"hvc1" | b"hev1" | b"avc1" | b"avc3" => {
                    assert_fills(bytes, at + 8 + VISUAL_ENTRY_BASE, at + size);
                }
                _ => {}
            }
            at += size;
        }
        assert_eq!(at, end, "the boxes stop at {at}, not {end}");
    }

    #[test]
    fn a_second_pass_changes_nothing() {
        let (before, once) = roundtrip(None, &Nclx::HDR10);
        let at = position(&once, b"colr") - 4;
        let existing = once[at..at + 19].to_vec();
        let (twice_source, twice) = roundtrip(Some(existing), &Nclx::HDR10);
        assert_eq!(twice_source.len(), twice.len());
        assert_eq!(
            nclx_in(&twice),
            Some((9, 16, 9, 0)),
            "the tagged file must stay tagged"
        );
        assert_eq!(twice.len(), once.len(), "no second box was added");
        assert!(!before.is_empty());
    }

    #[test]
    fn replaces_a_box_the_muxer_did_write() {
        let wrong = Nclx {
            primaries: 1,
            transfer: 1,
            matrix: 1,
            full_range: true,
        }
        .box_bytes();
        let (before, after) = roundtrip(Some(wrong), &Nclx::HDR10);
        assert_eq!(nclx_in(&after), Some((9, 16, 9, 0)));
        assert_eq!(after.len(), before.len(), "an equal swap moves nothing");
        assert_eq!(position(&after, b"colr"), position(&before, b"colr"));
    }

    #[test]
    fn ancestor_sizes_support_both_header_widths() {
        let mut short = vec![0; 8];
        short[..4].copy_from_slice(&100u32.to_be_bytes());
        patch_ancestor_size(&mut short, 0, 19).unwrap();
        assert_eq!(u32::from_be_bytes(short[..4].try_into().unwrap()), 119);

        let mut large = vec![0; 16];
        large[..4].copy_from_slice(&1u32.to_be_bytes());
        large[8..16].copy_from_slice(&1000u64.to_be_bytes());
        patch_ancestor_size(&mut large, 0, 19).unwrap();
        assert_eq!(u64::from_be_bytes(large[8..16].try_into().unwrap()), 1019);

        let mut to_parent_end = vec![0; 8];
        patch_ancestor_size(&mut to_parent_end, 0, 19).unwrap();
        assert_eq!(to_parent_end[..4], [0; 4]);
    }

    #[test]
    fn refuses_a_file_the_sample_data_follows() {
        // The muxer's usual file, but with the metadata moved ahead of the
        // sample data. Inserting there would shift every absolute chunk offset.
        let before = sample_file(None);
        let mdat = position(&before, b"mdat") - 4;
        let moov = position(&before, b"moov") - 4;
        let mut file = before[..mdat].to_vec();
        file.extend_from_slice(&before[moov..]);
        file.extend_from_slice(&before[mdat..moov]);

        let dir = std::env::temp_dir().join(format!("snapit-colr-bad-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("case.mp4");
        std::fs::write(&path, &file).unwrap();
        let err = insert(&path, &Nclx::HDR10).unwrap_err();
        assert!(
            err.to_string().contains("sample data"),
            "unexpected error: {err:#}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), file, "left untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
