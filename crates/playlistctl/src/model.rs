//! On-disk data formats shared with babooshka.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Extensions treated as playable video.
///
/// Note the absence of `vob`: DVD rips split one feature across several `VTS_*`
/// fragments, so listing them individually would put chunks of a film into the
/// rotation rather than the film.
pub const VIDEO_EXTS: &[&str] =
    &["mkv", "avi", "mp4", "m4v", "mov", "webm", "mpg", "mpeg", "wmv", "ts", "m2ts", "ogv", "flv"];

/// Whether babooshka could plausibly play this path as a playlist item.
pub fn is_playable(path: &str) -> bool {
    let Some(ext) = path.rsplit('.').next() else { return false };
    let ext = ext.to_ascii_lowercase();
    VIDEO_EXTS.contains(&ext.as_str())
}

/// `playlist.json` — the ordered list babooshka plays through.
///
/// Deliberately *not* reusing `player::Playlist`: that type panics on an empty
/// item list, which is a reasonable guard for the player but makes an editor
/// unable to open a fresh or emptied playlist.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Playlist {
    pub items: Vec<String>,

    /// Paths deliberately dismissed: artwork, DVD fragments, stray downloads.
    ///
    /// Kept in this file rather than a sidecar so a dismissal survives
    /// alongside the list it refers to. babooshka's own `Playlist` does not set
    /// `deny_unknown_fields`, so this extra key is ignored by the player.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored: Vec<String>,
}

/// `play-state.json` — babooshka's resume point, read-only here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayState {
    pub path: String,
    pub time: f64,
}

/// A human-chosen stretch of dialogue used to anchor a file's gain.
///
/// The whole normalisation scheme rests on this: rather than trusting a
/// whole-file statistic (which latches onto whatever is loudest-and-common —
/// dialogue in one film, an action reel or score in another), an operator picks
/// a representative talking scene and every film is aligned on that.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefSegment {
    pub start: f64,
    pub end: f64,
    /// p85 of short-term loudness within the segment, in LUFS.
    pub lufs: f64,
}

/// Everything measured about one file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileLoudness {
    #[serde(default)]
    pub duration: f64,
    /// EBU R128 gated integrated loudness over the whole file (LUFS).
    #[serde(default)]
    pub integrated: Option<f64>,
    /// Loudness range over the whole file (LU).
    #[serde(default)]
    pub lra: Option<f64>,
    /// Short-term (3s window) loudness decimated to one sample per second.
    ///
    /// Stored in full so any percentile can be recomputed later without
    /// re-measuring, and so a time-varying gain remains possible without a new
    /// analysis pass.
    #[serde(default)]
    pub short_term: Vec<f32>,
    /// The operator-chosen dialogue anchor, once set.
    #[serde(default)]
    pub reference: Option<RefSegment>,
}

impl FileLoudness {
    pub fn measured(&self) -> bool {
        !self.short_term.is_empty()
    }

    /// Percentile over the short-term series, ignoring near-silence.
    ///
    /// Windows below -70 LUFS are digital silence (leader, gaps between reels)
    /// and would drag every statistic down without saying anything about how
    /// loud the film actually plays.
    pub fn percentile(&self, q: f64) -> Option<f64> {
        percentile_of(&self.short_term, q)
    }

    /// Gain in dB that brings this file's anchor to `target_lufs`.
    ///
    /// Returns `None` until a reference segment has been chosen — an unanchored
    /// file gets no entry in `gain_db.json` rather than a guessed one.
    pub fn gain_db(&self, target_lufs: f64) -> Option<f64> {
        self.reference.as_ref().map(|r| target_lufs - r.lufs)
    }
}

/// Percentile over a short-term loudness series, ignoring near-silence.
pub fn percentile_of(series: &[f32], q: f64) -> Option<f64> {
    let mut v: Vec<f32> = series.iter().copied().filter(|s| s.is_finite() && *s > -70.0).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).expect("NaNs were filtered out"));
    let idx = ((q / 100.0) * (v.len() - 1) as f64).round() as usize;
    Some(v[idx] as f64)
}

/// `loudness.json` — playlistctl's own database. babooshka never reads this;
/// it consumes the flat `gain_db.json` derived from it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoudnessDb {
    /// The loudness every film's dialogue anchor is aligned to, in LUFS.
    pub target_lufs: f64,
    #[serde(default)]
    pub files: BTreeMap<String, FileLoudness>,
}

impl Default for LoudnessDb {
    fn default() -> Self {
        Self { target_lufs: crate::DEFAULT_TARGET_LUFS, files: BTreeMap::new() }
    }
}

impl LoudnessDb {
    /// Flatten to the `{path: gain_db}` map babooshka already knows how to read.
    pub fn to_gain_map(&self) -> BTreeMap<String, f64> {
        self.files
            .iter()
            .filter_map(|(p, f)| {
                f.gain_db(self.target_lufs).map(|g| (p.clone(), (g * 100.0).round() / 100.0))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_video_files_case_insensitively() {
        assert!(is_playable("/srv/babooshka-tv/Операция Ы.mkv"));
        assert!(is_playable("/srv/babooshka-tv/x.AVI"));
        assert!(is_playable("/srv/babooshka-tv/12 стульев/a-1.mkv"));
    }

    #[test]
    fn dvd_fragments_and_artwork_are_not_playable() {
        // A DVD rip splits one feature across VTS_* fragments, so listing them
        // individually would put chunks of a film into the rotation.
        assert!(!is_playable("/srv/babooshka-tv/VOLSHEBNAYA_SILA/VIDEO_TS/VTS_02_1.VOB"));
        assert!(!is_playable("/srv/babooshka-tv/VOLSHEBNAYA_SILA/VIDEO_TS/VTS_01_0.IFO"));
        assert!(!is_playable("/srv/babooshka-tv/VOLSHEBNAYA_SILA/Обложка.jpg"));
        assert!(!is_playable("/srv/babooshka-tv/no-extension"));
    }

    #[test]
    fn ignored_round_trips_and_is_omitted_when_empty() {
        let pl = Playlist { items: vec!["/a.mkv".into()], ignored: vec![] };
        let s = serde_json::to_string(&pl).unwrap();
        assert!(!s.contains("ignored"), "empty ignore list should not clutter the file: {s}");

        let pl = Playlist { items: vec!["/a.mkv".into()], ignored: vec!["/b.jpg".into()] };
        let s = serde_json::to_string(&pl).unwrap();
        let back: Playlist = serde_json::from_str(&s).unwrap();
        assert_eq!(back.ignored, vec!["/b.jpg".to_string()]);
    }

    #[test]
    fn a_playlist_without_ignored_still_loads() {
        let back: Playlist = serde_json::from_str(r#"{"items":["/a.mkv"]}"#).unwrap();
        assert!(back.ignored.is_empty());
    }
}
