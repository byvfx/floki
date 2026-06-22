//! Image-sequence detection (#7).
//!
//! Pure logic — no GPU, no UI, no decode. Given a single EXR the user opened,
//! find its sibling frames in the same directory, group them, sort them
//! **numerically** (not lexically — `frame9` must precede `frame10`), and report
//! any missing frames. Opening one image with ≥2 matching siblings enables
//! sequence mode; a lone image yields `None`, preserving the single-image path.
//!
//! The frame field is the **last contiguous run of digits** in the file stem, so
//! all three common naming styles are handled: `name.0001.exr`, `name_0001.exr`,
//! `name0001.exr`. Frames are grouped by everything around that run
//! (`(prefix, suffix)`), tolerant of mixed zero-padding within a group
//! (`9 → 10 → 100`), the VFX norm. See `docs/playback/sequence-playback.md`.

use std::path::{Path, PathBuf};

/// A detected, numerically-ordered image sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sequence {
    /// Existing frame files, sorted by frame number ascending. Holes are absent
    /// (they are reported in [`Sequence::holes`], not represented by a path).
    pub frames: Vec<PathBuf>,
    /// Inclusive `(min, max)` frame numbers seen on disk.
    pub range: (u32, u32),
    /// Frame numbers missing from `[min..=max]`, ascending. Empty when contiguous.
    pub holes: Vec<u32>,
}

impl Sequence {
    /// Number of frames present on disk (excludes holes).
    #[must_use]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Always `false` — a `Sequence` is only constructed with ≥2 frames — but
    /// provided so clippy doesn't ask for it and callers can be explicit.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The frame number of each entry in [`Sequence::frames`], ascending.
    /// Reconstructed from `range` and `holes` — the present numbers are
    /// `[min..=max]` minus the holes, and `frames` is stored in that same order.
    #[must_use]
    pub fn numbers(&self) -> Vec<u32> {
        let holes: std::collections::HashSet<u32> = self.holes.iter().copied().collect();
        (self.range.0..=self.range.1)
            .filter(|n| !holes.contains(n))
            .collect()
    }

    /// Path of the frame with the given number, or `None` when it is out of range
    /// or a hole. The playhead may sit on a hole; the caller holds the previous
    /// frame in that case (it never stalls).
    #[must_use]
    pub fn path_for(&self, number: u32) -> Option<&Path> {
        if number < self.range.0 || number > self.range.1 {
            return None;
        }
        if self.holes.contains(&number) {
            return None;
        }
        // Index = how many present numbers precede `number` = (offset from min)
        // minus the holes below it.
        let holes_below = self.holes.iter().filter(|&&h| h < number).count();
        let index = (number - self.range.0) as usize - holes_below;
        self.frames.get(index).map(PathBuf::as_path)
    }

    /// The frame number of `path` within this sequence, or `None` if it is not a
    /// member. Used to place the playhead on the file the user actually opened.
    #[must_use]
    pub fn number_of(&self, path: &Path) -> Option<u32> {
        let index = self.frames.iter().position(|p| p == path)?;
        self.numbers().get(index).copied()
    }
}

/// The decomposition of a file stem around its frame field.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FrameName {
    /// Everything before the digit run (e.g. `render.` in `render.0007`).
    prefix: String,
    /// Everything after the digit run (usually empty; e.g. `_left` in
    /// `shot_0007_left`).
    suffix: String,
    /// The parsed frame number (leading zeros discarded; padding need not match
    /// across a group).
    number: u32,
}

/// Split a file stem into `(prefix, frame number, suffix)` on its **last
/// contiguous run of ASCII digits**. Returns `None` if the stem has no digits,
/// or the digit run does not fit a `u32` (treated as not-a-frame).
///
/// `file_stem` strips only the final extension, so `name.0001.exr` arrives here
/// as `name.0001` → `("name.", 1, "")`.
fn parse_frame(stem: &str) -> Option<FrameName> {
    // Byte index just past the last digit (digits are ASCII, so byte == char
    // boundary here).
    let end = stem.rfind(|c: char| c.is_ascii_digit())? + 1;
    let bytes = stem.as_bytes();
    let mut start = end - 1;
    while start > 0 && bytes[start - 1].is_ascii_digit() {
        start -= 1;
    }
    let number = stem[start..end].parse::<u32>().ok()?;
    Some(FrameName {
        prefix: stem[..start].to_string(),
        suffix: stem[end..].to_string(),
        number,
    })
}

/// Whether two paths share the same extension, compared case-insensitively
/// (`.EXR` matches `.exr`). Two extensionless paths also match.
fn same_extension(a: &Path, b: &Path) -> bool {
    match (a.extension(), b.extension()) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        (None, None) => true,
        _ => false,
    }
}

/// Detect the image sequence that the file at `path` belongs to.
///
/// Returns `Some(sequence)` when the file is a numbered frame and **at least one
/// other** sibling in the same directory shares its `(prefix, suffix)` and
/// extension — i.e. the group has ≥2 frames. Returns `None` for a lone image, a
/// file with no frame number, or a path with no readable parent directory, so
/// the caller keeps today's single-image behavior.
#[must_use]
pub fn detect_from_file(path: &Path) -> Option<Sequence> {
    let dir = path.parent()?;
    let anchor = parse_frame(path.file_stem()?.to_str()?)?;

    // Collect every sibling that parses into the same group: same extension,
    // same prefix and suffix around the frame field. Keyed by frame number so
    // duplicate numbers (e.g. `f1` and `f01`) collapse deterministically to the
    // path that sorts first, regardless of read_dir order.
    let mut by_number: std::collections::BTreeMap<u32, PathBuf> = std::collections::BTreeMap::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if !p.is_file() || !same_extension(&p, path) {
            continue;
        }
        let Some(parsed) = p.file_stem().and_then(|s| s.to_str()).and_then(parse_frame) else {
            continue;
        };
        if parsed.prefix != anchor.prefix || parsed.suffix != anchor.suffix {
            continue;
        }
        by_number
            .entry(parsed.number)
            .and_modify(|existing| {
                if p < *existing {
                    *existing = p.clone();
                }
            })
            .or_insert(p);
    }

    // A lone image (only the opened frame matched) is not a sequence.
    if by_number.len() < 2 {
        return None;
    }

    // BTreeMap iterates by key, so numbers/frames are already numerically sorted.
    let min = *by_number.keys().next()?;
    let max = *by_number.keys().next_back()?;
    let holes: Vec<u32> = (min..=max).filter(|n| !by_number.contains_key(n)).collect();
    let frames: Vec<PathBuf> = by_number.into_values().collect();

    Some(Sequence {
        frames,
        range: (min, max),
        holes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // --- parse_frame ---------------------------------------------------------

    #[test]
    fn parses_the_three_common_naming_styles() {
        // file_stem has already stripped the `.exr`.
        assert_eq!(
            parse_frame("name.0001"),
            Some(FrameName {
                prefix: "name.".into(),
                suffix: "".into(),
                number: 1
            })
        );
        assert_eq!(
            parse_frame("name_0001"),
            Some(FrameName {
                prefix: "name_".into(),
                suffix: "".into(),
                number: 1
            })
        );
        assert_eq!(
            parse_frame("name0001"),
            Some(FrameName {
                prefix: "name".into(),
                suffix: "".into(),
                number: 1
            })
        );
    }

    #[test]
    fn uses_the_last_digit_run_and_keeps_the_suffix() {
        // A trailing token after the frame field (e.g. a stereo view) becomes the
        // suffix; the frame is the *last* digit run, not the `010` mid-name.
        assert_eq!(
            parse_frame("shot_010_v0007_left"),
            Some(FrameName {
                prefix: "shot_010_v".into(),
                suffix: "_left".into(),
                number: 7
            })
        );
    }

    #[test]
    fn rejects_stems_without_digits() {
        assert_eq!(parse_frame("background"), None);
        assert_eq!(parse_frame(""), None);
    }

    #[test]
    fn discards_leading_zeros_when_parsing_number() {
        assert_eq!(parse_frame("f000123").unwrap().number, 123);
    }

    // --- detect_from_file ----------------------------------------------------

    /// Create empty files with the given names in `dir`.
    fn touch_all(dir: &Path, names: &[&str]) {
        for n in names {
            fs::write(dir.join(n), b"").unwrap();
        }
    }

    #[test]
    fn detects_padded_sequence_with_a_hole() {
        let dir = tempfile::tempdir().unwrap();
        touch_all(
            dir.path(),
            &[
                "frame.0001.exr",
                "frame.0002.exr",
                // 0003 missing -> a hole
                "frame.0004.exr",
                "frame.0005.exr",
            ],
        );
        let seq = detect_from_file(&dir.path().join("frame.0001.exr")).unwrap();
        assert_eq!(seq.len(), 4);
        assert_eq!(seq.range, (1, 5));
        assert_eq!(seq.holes, vec![3]);
        assert_eq!(
            seq.frames,
            vec![
                dir.path().join("frame.0001.exr"),
                dir.path().join("frame.0002.exr"),
                dir.path().join("frame.0004.exr"),
                dir.path().join("frame.0005.exr"),
            ]
        );
    }

    #[test]
    fn sorts_unpadded_frames_numerically_not_lexically() {
        let dir = tempfile::tempdir().unwrap();
        touch_all(dir.path(), &["f1.exr", "f2.exr", "f10.exr", "f100.exr"]);
        // Opening any member finds the whole group.
        let seq = detect_from_file(&dir.path().join("f10.exr")).unwrap();
        assert_eq!(seq.range, (1, 100));
        // Numeric order: 1, 2, 10, 100 — a lexical sort would give 1,10,100,2.
        assert_eq!(
            seq.frames,
            vec![
                dir.path().join("f1.exr"),
                dir.path().join("f2.exr"),
                dir.path().join("f10.exr"),
                dir.path().join("f100.exr"),
            ]
        );
        // Holes are every number in 1..=100 except the four present.
        assert!(seq.holes.contains(&3) && seq.holes.contains(&99));
        assert_eq!(seq.holes.len(), 100 - 4);
    }

    #[test]
    fn tolerates_mixed_zero_padding_in_one_group() {
        let dir = tempfile::tempdir().unwrap();
        // 1-digit and 2-digit padding for the same (prefix, suffix) group.
        touch_all(
            dir.path(),
            &["frame.8.exr", "frame.9.exr", "frame.10.exr", "frame.11.exr"],
        );
        let seq = detect_from_file(&dir.path().join("frame.8.exr")).unwrap();
        assert_eq!(seq.range, (8, 11));
        assert!(seq.holes.is_empty());
        assert_eq!(seq.len(), 4);
    }

    #[test]
    fn lone_image_is_not_a_sequence() {
        let dir = tempfile::tempdir().unwrap();
        touch_all(dir.path(), &["only.0001.exr"]);
        assert_eq!(detect_from_file(&dir.path().join("only.0001.exr")), None);
    }

    #[test]
    fn unnumbered_file_is_not_a_sequence() {
        let dir = tempfile::tempdir().unwrap();
        touch_all(dir.path(), &["beauty.exr", "beauty_extra.exr"]);
        assert_eq!(detect_from_file(&dir.path().join("beauty.exr")), None);
    }

    #[test]
    fn does_not_group_across_different_prefixes_suffixes_or_extensions() {
        let dir = tempfile::tempdir().unwrap();
        touch_all(
            dir.path(),
            &[
                "shot_0001.exr",
                "shot_0002.exr",
                "shot_0003.exr",
                "other_0001.exr",     // different prefix -> excluded
                "shot_0001_mask.exr", // different suffix -> excluded
                "shot_0004.png",      // different extension -> excluded
            ],
        );
        let seq = detect_from_file(&dir.path().join("shot_0001.exr")).unwrap();
        assert_eq!(seq.range, (1, 3));
        assert_eq!(seq.len(), 3);
        assert!(seq.holes.is_empty());
        for f in &seq.frames {
            let name = f.file_name().unwrap().to_str().unwrap();
            assert!(name.starts_with("shot_") && name.ends_with(".exr"));
            assert!(!name.contains("mask"));
        }
    }

    #[test]
    fn matches_extension_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        touch_all(dir.path(), &["frame.0001.EXR", "frame.0002.exr"]);
        let seq = detect_from_file(&dir.path().join("frame.0001.EXR")).unwrap();
        assert_eq!(seq.len(), 2);
        assert_eq!(seq.range, (1, 2));
    }

    // --- number <-> path lookup (Phase 2 helpers) ----------------------------

    #[test]
    fn numbers_lists_present_frames_skipping_holes() {
        let dir = tempfile::tempdir().unwrap();
        touch_all(
            dir.path(),
            &["s.0001.exr", "s.0002.exr", "s.0004.exr", "s.0005.exr"],
        );
        let seq = detect_from_file(&dir.path().join("s.0001.exr")).unwrap();
        assert_eq!(seq.numbers(), vec![1, 2, 4, 5]);
    }

    #[test]
    fn path_for_maps_number_to_file_and_holes_to_none() {
        let dir = tempfile::tempdir().unwrap();
        touch_all(
            dir.path(),
            &["s.0001.exr", "s.0002.exr", "s.0004.exr", "s.0005.exr"],
        );
        let seq = detect_from_file(&dir.path().join("s.0001.exr")).unwrap();
        assert_eq!(
            seq.path_for(1),
            Some(dir.path().join("s.0001.exr").as_path())
        );
        assert_eq!(
            seq.path_for(4),
            Some(dir.path().join("s.0004.exr").as_path())
        );
        assert_eq!(seq.path_for(3), None, "hole maps to None");
        assert_eq!(seq.path_for(99), None, "out of range maps to None");
    }

    #[test]
    fn number_of_recovers_the_opened_frames_number() {
        let dir = tempfile::tempdir().unwrap();
        touch_all(
            dir.path(),
            &["s.0001.exr", "s.0002.exr", "s.0004.exr", "s.0005.exr"],
        );
        let seq = detect_from_file(&dir.path().join("s.0004.exr")).unwrap();
        assert_eq!(seq.number_of(&dir.path().join("s.0004.exr")), Some(4));
        assert_eq!(seq.number_of(&dir.path().join("s.0001.exr")), Some(1));
        assert_eq!(seq.number_of(&dir.path().join("nope.exr")), None);
    }
}
