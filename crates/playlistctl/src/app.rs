//! Application state and the background measurement worker.

use std::collections::BTreeSet;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use crate::loudness::{SEGMENT_PERCENTILE, measure_file, measure_segment};
use crate::mpv::Mpv;
use crate::model::{
    FileLoudness, LoudnessDb, PlayState, Playlist, RefSegment, is_playable, percentile_of,
};
use crate::remote::Remote;

/// Length of a proposed anchor segment, in seconds.
///
/// Long enough to average over the rhythm of a conversation, short enough to
/// stay inside one scene — the anchors set by hand ran 45–85 s.
const ANCHOR_WINDOW_SECS: usize = 45;

/// Percentile, within a film's own loudness distribution, that a good dialogue
/// anchor sits at.
///
/// Measured, not chosen: the scene in "Иван Васильевич" that was calibrated by
/// ear ranks here. Anchors picked without this feedback landed at 50–80% and
/// were all too quiet, which is what the proposal is aiming to prevent.
pub const ANCHOR_RANK: f64 = 92.0;

/// Fraction of a film skipped at each end when proposing.
///
/// Opening titles and end credits are scored music: loud, sustained, and
/// exactly the shape this search rewards, but useless as a dialogue reference.
const EDGE_TRIM: f64 = 0.04;

/// Percentile rank of `level` within `series`, ignoring near-silence.
pub fn rank_of(series: &[f32], level: f64) -> Option<f64> {
    let v: Vec<f32> = series.iter().copied().filter(|s| s.is_finite() && *s > -70.0).collect();
    if v.is_empty() {
        return None;
    }
    let below = v.iter().filter(|s| (**s as f64) < level).count();
    Some(100.0 * below as f64 / v.len() as f64)
}

/// Find the window that best resembles the calibrated dialogue reference.
///
/// Two criteria, because level alone is not enough: the window's p85 should sit
/// at [`ANCHOR_RANK`] of the film, *and* the window should be internally steady.
/// Sustained mid-level plateaus are talking; a loud average reached by spikes
/// over silence is an action or music cue, and normalising to it makes the film
/// play quiet.
pub fn propose_segment(series: &[f32], window: usize) -> Option<(f64, f64)> {
    let target = percentile_of(series, ANCHOR_RANK)?;
    let trim = (series.len() as f64 * EDGE_TRIM) as usize;
    let (lo, hi) = (trim, series.len().saturating_sub(trim));
    if hi.saturating_sub(lo) < window {
        return None;
    }

    let mut best: Option<(f64, usize)> = None;
    // One-second steps: the series is already decimated to 1 Hz, and a finer
    // grid would not move the statistic.
    for start in lo..=(hi - window) {
        let w = &series[start..start + window];
        let Some(level) = percentile_of(w, SEGMENT_PERCENTILE) else { continue };
        let (Some(spread_hi), Some(spread_lo)) = (percentile_of(w, 95.0), percentile_of(w, 50.0))
        else {
            continue;
        };
        // Fraction of the window well below its own level — room tone, i.e. the
        // window hanging off the edge of the scene. A percentile cannot see
        // this: a few seconds of overhang sits below p10 and goes unnoticed.
        let dead = w.iter().filter(|s| (**s as f64) < level - 12.0).count() as f64 / w.len() as f64;
        // Both correction terms are weighted below the level term: being at the
        // right loudness matters most, and real dialogue is never flat.
        let score = (level - target).abs() + 0.3 * (spread_hi - spread_lo) + 12.0 * dead;
        if best.is_none_or(|(b, _)| score < b) {
            best = Some((score, start));
        }
    }
    best.map(|(_, s)| (s as f64, (s + window) as f64))
}


/// Remote paths the tool reads and writes.
#[derive(Debug, Clone)]
pub struct Paths {
    pub root: String,
    pub playlist: String,
    pub play_state: String,
    pub loudness_db: String,
    pub gain_out: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Playlist,
    Loudness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Playlist = 0,
    Unlisted = 1,
    Missing = 2,
}

enum Job {
    File(String),
    Segment { path: String, start: f64, end: f64 },
}

enum JobResult {
    File(String, Result<FileLoudness, String>),
    Segment(String, Result<RefSegment, String>),
}

pub struct App {
    /// Holds the video files; runs `find` and ffmpeg where the data is local.
    pub media: Remote,
    /// Reaches `/srv` through the NFS mount. The export squashes clients to the
    /// owning uid, so the Pi can create and replace files there while an SSH
    /// session on the media host cannot — all JSON writes go via this host.
    pub control: Remote,
    pub paths: Paths,

    pub playlist: Vec<String>,
    /// Every file under the media root, playable or not.
    pub on_disk: Vec<String>,
    /// Paths the operator has dismissed; never shown again.
    pub ignored: BTreeSet<String>,
    pub play_state: Option<PlayState>,
    pub db: LoudnessDb,

    pub tab: Tab,
    pub pane: Pane,
    /// Selection index per pane, indexed by `Pane as usize`.
    pub sel: [usize; 3],
    pub loud_sel: usize,
    /// Playhead within the selected file, in seconds.
    pub cursor: f64,
    /// A segment being marked out: start, and end once `]` is pressed.
    pub seg: Option<(f64, Option<f64>)>,

    /// The preview window, started on first use and kept alive after that.
    pub mpv: Option<Mpv>,
    pub mpv_socket: String,

    pub status: String,
    pub dirty_playlist: bool,
    pub dirty_db: bool,
    pub pending: usize,
    pub quit: bool,

    tx: Sender<Job>,
    rx: Receiver<JobResult>,
}

impl App {
    pub fn new(
        media: Remote,
        control: Remote,
        paths: Paths,
        target_lufs: Option<f64>,
    ) -> Result<Self, String> {
        let playlist: Playlist = match control.read_file(&paths.playlist)? {
            Some(s) => serde_json::from_str(&s)
                .map_err(|e| format!("{} is not valid playlist JSON: {e}", paths.playlist))?,
            None => Playlist::default(),
        };
        let play_state: Option<PlayState> = match control.read_file(&paths.play_state)? {
            Some(s) if !s.trim().is_empty() => serde_json::from_str(&s).ok(),
            _ => None,
        };
        let mut db: LoudnessDb = match control.read_file(&paths.loudness_db)? {
            Some(s) if !s.trim().is_empty() => serde_json::from_str(&s)
                .map_err(|e| format!("{} is not valid loudness JSON: {e}", paths.loudness_db))?,
            _ => LoudnessDb::default(),
        };
        if let Some(t) = target_lufs {
            db.target_lufs = t;
        }
        let on_disk = media.scan_files(&paths.root)?;

        let (tx, job_rx) = channel::<Job>();
        let (res_tx, rx) = channel::<JobResult>();
        {
            // One worker, so measurements queue rather than saturating the media
            // host — it is a 2-core VM also serving the running player.
            let remote = media.clone();
            thread::spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let out = match job {
                        Job::File(p) => {
                            let r = measure_file(&remote, &p);
                            JobResult::File(p, r)
                        }
                        Job::Segment { path, start, end } => {
                            let r = measure_segment(&remote, &path, start, end);
                            JobResult::Segment(path, r)
                        }
                    };
                    if res_tx.send(out).is_err() {
                        break;
                    }
                }
            });
        }

        Ok(Self {
            media,
            control,
            paths,
            playlist: playlist.items,
            on_disk,
            ignored: playlist.ignored.into_iter().collect(),
            play_state,
            db,
            tab: Tab::Playlist,
            pane: Pane::Playlist,
            sel: [0; 3],
            loud_sel: 0,
            cursor: 0.0,
            seg: None,
            mpv: None,
            mpv_socket: format!("/tmp/playlistctl-mpv-{}.sock", std::process::id()),
            status: "loaded".into(),
            dirty_playlist: false,
            dirty_db: false,
            pending: 0,
            quit: false,
            tx,
            rx,
        })
    }

    // ---- derived views -------------------------------------------------

    /// Everything on disk that is neither in the playlist nor dismissed.
    ///
    /// Includes files that cannot be played; the UI marks those and offers to
    /// dismiss them, which is what keeps stray media from going unnoticed.
    pub fn unlisted(&self) -> Vec<String> {
        let listed: BTreeSet<&String> = self.playlist.iter().collect();
        self.on_disk
            .iter()
            .filter(|p| !listed.contains(*p) && !self.ignored.contains(*p))
            .cloned()
            .collect()
    }

    /// Files in the playlist that no longer exist — deleted or renamed.
    pub fn missing(&self) -> Vec<String> {
        let disk: BTreeSet<&String> = self.on_disk.iter().collect();
        self.playlist.iter().filter(|p| !disk.contains(*p)).cloned().collect()
    }

    /// Path relative to the media root, so nesting is visible at a glance.
    pub fn rel<'a>(&self, p: &'a str) -> &'a str {
        p.strip_prefix(&self.paths.root).map(|r| r.trim_start_matches('/')).unwrap_or(p)
    }

    /// Dismiss the selected unlisted entry.
    pub fn ignore_selected(&mut self) {
        let items = self.unlisted();
        if items.is_empty() {
            return;
        }
        let i = self.sel[Pane::Unlisted as usize].min(items.len() - 1);
        let path = items[i].clone();
        self.ignored.insert(path.clone());
        self.dirty_playlist = true;
        self.sel[Pane::Unlisted as usize] =
            i.min(self.unlisted().len().saturating_sub(1));
        self.status = format!("ignoring {}", self.rel(&path));
    }

    /// Dismiss every unlisted entry that is not playable video in one go.
    pub fn ignore_all_unplayable(&mut self) {
        let victims: Vec<String> =
            self.unlisted().into_iter().filter(|p| !is_playable(p)).collect();
        if victims.is_empty() {
            self.status = "nothing unplayable left to ignore".into();
            return;
        }
        let n = victims.len();
        self.ignored.extend(victims);
        self.dirty_playlist = true;
        self.sel[Pane::Unlisted as usize] =
            self.sel[Pane::Unlisted as usize].min(self.unlisted().len().saturating_sub(1));
        self.status = format!("ignoring {n} unplayable file(s)");
    }

    /// Forget every dismissal, so the next scan shows them again.
    pub fn clear_ignored(&mut self) {
        if self.ignored.is_empty() {
            self.status = "nothing is ignored".into();
            return;
        }
        let n = self.ignored.len();
        self.ignored.clear();
        self.dirty_playlist = true;
        self.status = format!("un-ignored {n} file(s)");
    }

    pub fn pane_items(&self, pane: Pane) -> Vec<String> {
        match pane {
            Pane::Playlist => self.playlist.clone(),
            Pane::Unlisted => self.unlisted(),
            Pane::Missing => self.missing(),
        }
    }

    /// The playlist entry babooshka would currently resume into.
    ///
    /// Worth surfacing because `Playlist::next_file` silently falls back to
    /// `items[0]` when the saved path is no longer in the list, so removing or
    /// reordering this entry restarts the rotation.
    pub fn current_index(&self) -> Option<usize> {
        let cur = self.play_state.as_ref()?;
        self.playlist.iter().position(|p| *p == cur.path)
    }

    pub fn selected_loudness_path(&self) -> Option<String> {
        self.playlist.get(self.loud_sel).cloned()
    }

    pub fn selected_loudness(&self) -> Option<&FileLoudness> {
        self.db.files.get(&self.selected_loudness_path()?)
    }

    // ---- background jobs -----------------------------------------------

    pub fn queue_measure(&mut self, path: String) {
        if self.tx.send(Job::File(path)).is_ok() {
            self.pending += 1;
        }
    }

    pub fn queue_measure_all_unmeasured(&mut self) {
        let todo: Vec<String> = self
            .playlist
            .iter()
            .filter(|p| self.db.files.get(*p).is_none_or(|f| !f.measured()))
            .cloned()
            .collect();
        if todo.is_empty() {
            self.status = "every playlist file is already measured".into();
            return;
        }
        let n = todo.len();
        for p in todo {
            self.queue_measure(p);
        }
        self.status = format!("queued {n} file(s) for measurement");
    }

    pub fn queue_segment(&mut self, path: String, start: f64, end: f64) {
        if self.tx.send(Job::Segment { path, start, end }).is_ok() {
            self.pending += 1;
        }
    }

    /// Drain finished jobs. Returns true if anything changed.
    pub fn poll_jobs(&mut self) -> bool {
        let mut changed = false;
        while let Ok(res) = self.rx.try_recv() {
            self.pending = self.pending.saturating_sub(1);
            changed = true;
            match res {
                JobResult::File(path, Ok(mut f)) => {
                    // A re-measure must not discard the operator's chosen anchor.
                    if let Some(old) = self.db.files.get(&path) {
                        f.reference = old.reference.clone();
                    }
                    self.status = format!(
                        "measured {} — I {:.1} LUFS, LRA {:.1} LU",
                        base(&path),
                        f.integrated.unwrap_or(f64::NAN),
                        f.lra.unwrap_or(f64::NAN)
                    );
                    self.db.files.insert(path, f);
                    self.dirty_db = true;
                }
                JobResult::Segment(path, Ok(r)) => {
                    let gain = self.db.target_lufs - r.lufs;
                    self.status = format!(
                        "anchor {}: {:.1}-{:.1}s = {:.1} LUFS → gain {:+.1} dB",
                        base(&path),
                        r.start,
                        r.end,
                        r.lufs,
                        gain
                    );
                    self.db.files.entry(path).or_default().reference = Some(r);
                    self.dirty_db = true;
                }
                JobResult::File(path, Err(e)) | JobResult::Segment(path, Err(e)) => {
                    self.status = format!("{}: {e}", base(&path));
                }
            }
        }
        // Flush as soon as a result lands, so killing a long batch keeps every
        // measurement that already completed.
        if changed
            && self.dirty_db
            && let Err(e) = self.persist_db()
        {
            self.status = format!("{} (save failed: {e})", self.status);
        }
        changed
    }

    // ---- actions -------------------------------------------------------

    pub fn move_item(&mut self, delta: isize) {
        if self.pane != Pane::Playlist || self.playlist.is_empty() {
            return;
        }
        let i = self.sel[Pane::Playlist as usize].min(self.playlist.len() - 1);
        let j = i as isize + delta;
        if j < 0 || j as usize >= self.playlist.len() {
            return;
        }
        let j = j as usize;
        self.playlist.swap(i, j);
        self.sel[Pane::Playlist as usize] = j;
        self.dirty_playlist = true;
        self.warn_if_current(i.min(j), i.max(j));
    }

    /// Move the selected unlisted file into the playlist, after the cursor.
    pub fn promote(&mut self) {
        let items = self.unlisted();
        if items.is_empty() {
            return;
        }
        let i = self.sel[Pane::Unlisted as usize].min(items.len() - 1);
        let path = items[i].clone();
        if !is_playable(&path) {
            self.status =
                format!("{} is not a playable video — i to ignore it", self.rel(&path));
            return;
        }
        // The cursor follows the inserted item, so adding a run of episodes one
        // after another keeps them in the order they were picked.
        let at =
            insert_after(&mut self.playlist, self.sel[Pane::Playlist as usize], path.clone());
        self.sel[Pane::Playlist as usize] = at;
        self.dirty_playlist = true;
        // The promoted entry has left `unlisted`, so holding the index still
        // lands on the next candidate — repeated presses walk down the list.
        self.sel[Pane::Unlisted as usize] = i.min(self.unlisted().len().saturating_sub(1));
        self.status = format!("added {}", self.rel(&path));
    }

    pub fn remove_selected(&mut self) {
        if self.pane != Pane::Playlist || self.playlist.is_empty() {
            return;
        }
        let i = self.sel[Pane::Playlist as usize].min(self.playlist.len() - 1);
        if Some(i) == self.current_index() {
            self.status =
                "that entry is the saved resume point — press D to remove it anyway".into();
            return;
        }
        let p = self.playlist.remove(i);
        self.dirty_playlist = true;
        self.sel[Pane::Playlist as usize] = i.min(self.playlist.len().saturating_sub(1));
        self.status = format!("removed {}", base(&p));
    }

    /// Remove without the resume-point guard.
    pub fn force_remove_selected(&mut self) {
        if self.pane != Pane::Playlist || self.playlist.is_empty() {
            return;
        }
        let i = self.sel[Pane::Playlist as usize].min(self.playlist.len() - 1);
        let p = self.playlist.remove(i);
        self.dirty_playlist = true;
        self.sel[Pane::Playlist as usize] = i.min(self.playlist.len().saturating_sub(1));
        self.status = format!("removed {} (resume point cleared)", base(&p));
    }

    pub fn purge_missing(&mut self) {
        let disk: BTreeSet<String> = self.on_disk.iter().cloned().collect();
        let before = self.playlist.len();
        self.playlist.retain(|p| disk.contains(p));
        let n = before - self.playlist.len();
        if n > 0 {
            self.dirty_playlist = true;
            self.status = format!("dropped {n} missing entr{}", if n == 1 { "y" } else { "ies" });
        } else {
            self.status = "no missing entries".into();
        }
    }

    fn warn_if_current(&mut self, lo: usize, hi: usize) {
        if let Some(c) = self.current_index()
            && (lo..=hi).contains(&c)
        {
            self.status = format!(
                "note: moved the saved resume point ({})",
                base(&self.playlist[c])
            );
        }
    }

    pub fn rescan(&mut self) {
        match self.media.scan_files(&self.paths.root) {
            Ok(v) => {
                self.on_disk = v;
                self.status = format!("rescanned {} — {} file(s)", self.paths.root, self.on_disk.len());
            }
            Err(e) => self.status = e,
        }
    }

    // ---- persistence ---------------------------------------------------

    pub fn save_playlist(&mut self) {
        let pl = Playlist {
            items: self.playlist.clone(),
            ignored: self.ignored.iter().cloned().collect(),
        };
        match serde_json::to_string_pretty(&pl) {
            Ok(s) => match self.control.write_file_atomic(&self.paths.playlist, &format!("{s}\n")) {
                Ok(()) => {
                    self.dirty_playlist = false;
                    self.status = format!("wrote {}", self.paths.playlist);
                }
                Err(e) => self.status = e,
            },
            Err(e) => self.status = format!("failed to serialise playlist: {e}"),
        }
    }

    pub fn save_db(&mut self) {
        match self.persist_db() {
            Ok(()) => self.status = format!("wrote {}", self.paths.loudness_db),
            Err(e) => self.status = e,
        }
    }

    /// Write the loudness database without touching the status line.
    ///
    /// Analysis is minutes of remote CPU per batch, so every completed
    /// measurement is flushed immediately rather than held until the end — an
    /// interrupted run must never throw away work already paid for.
    pub fn persist_db(&mut self) -> Result<(), String> {
        let s = serde_json::to_string_pretty(&self.db)
            .map_err(|e| format!("failed to serialise loudness db: {e}"))?;
        self.control.write_file_atomic(&self.paths.loudness_db, &format!("{s}\n"))?;
        self.dirty_db = false;
        Ok(())
    }

    /// Export the flat `{path: gain}` map babooshka consumes.
    pub fn export_gain(&mut self) {
        let map = self.db.to_gain_map();
        if map.is_empty() {
            self.status = "no anchored files yet — nothing to export".into();
            return;
        }
        match serde_json::to_string_pretty(&map) {
            Ok(s) => match self.control.write_file_atomic(&self.paths.gain_out, &format!("{s}\n")) {
                Ok(()) => {
                    self.status =
                        format!("wrote {} entr{} to {}", map.len(), if map.len() == 1 { "y" } else { "ies" }, self.paths.gain_out)
                }
                Err(e) => self.status = e,
            },
            Err(e) => self.status = format!("failed to serialise gain map: {e}"),
        }
    }

    /// Start or focus the preview: one mpv window, kept alive across presses.
    ///
    /// Toggles pause when the right file is already loaded and positioned, so
    /// the same key both starts listening and stops it.
    pub fn preview(&mut self) {
        let Some(path) = self.selected_loudness_path() else { return };
        if self.mpv.is_none() {
            match Mpv::spawn(&self.mpv_socket) {
                Ok(m) => self.mpv = Some(m),
                Err(e) => {
                    self.status = e;
                    return;
                }
            }
        }
        let url = self.media.sftp_url(&path);
        let seg = self.seg;
        let cursor = self.cursor;
        let Some(mpv) = self.mpv.as_mut() else { return };

        if mpv.loaded.as_deref() != Some(path.as_str()) {
            if let Err(e) = mpv.load(&url, &path) {
                self.status = e;
                return;
            }
            // Opening over SFTP is the slow part; seek once the file is up.
            let start = match seg {
                Some((s, _)) => s,
                None => cursor,
            };
            let _ = mpv.seek(start);
            let _ = mpv.set_loop(match seg {
                Some((s, Some(e))) => Some((s, e)),
                _ => None,
            });
            let _ = mpv.set_paused(false);
            self.status = format!("opening {} at {}…", base(&path), crate::ui::hms(start));
            return;
        }

        let now_paused = !mpv.paused;
        let _ = mpv.set_paused(now_paused);
        self.status = if now_paused { "paused".into() } else { "playing".into() };
    }

    /// Push the TUI cursor to the preview window, if it has this file open.
    pub fn sync_mpv_position(&mut self) {
        let Some(path) = self.selected_loudness_path() else { return };
        let cursor = self.cursor;
        if let Some(mpv) = self.mpv.as_mut()
            && mpv.loaded.as_deref() == Some(path.as_str())
        {
            let _ = mpv.seek(cursor);
        }
    }

    /// Mirror the marked segment into mpv's A-B loop, so `p` repeats it.
    pub fn sync_mpv_loop(&mut self) {
        let seg = self.seg;
        if let Some(mpv) = self.mpv.as_mut() {
            let _ = mpv.set_loop(match seg {
                Some((s, Some(e))) => Some((s, e)),
                _ => None,
            });
        }
    }

    /// Pull mpv's playback position into the cursor. Returns true if it moved.
    ///
    /// This is the other half of the two-way binding: scrubbing in the mpv
    /// window walks the TUI cursor along the loudness timeline, so a scene can
    /// be found by watching rather than by reading the graph.
    pub fn poll_mpv(&mut self) -> bool {
        let Some(path) = self.selected_loudness_path() else { return false };
        let Some(mpv) = self.mpv.as_mut() else { return false };
        if !mpv.poll() || mpv.loaded.as_deref() != Some(path.as_str()) {
            return false;
        }
        let t = mpv.time_pos;
        if (t - self.cursor).abs() > 0.05 {
            self.cursor = t;
            return true;
        }
        false
    }

    /// Mark the best automatic guess at a dialogue anchor, for review by ear.
    ///
    /// Deliberately only *marks* a segment rather than committing it: the
    /// proposal comes from the 1 Hz timeline, which cannot tell speech from a
    /// sustained note. The operator still presses `p` then `s`.
    pub fn propose_anchor(&mut self) {
        let Some(fl) = self.selected_loudness() else { return };
        if !fl.measured() {
            self.status = "measure this file first (m)".into();
            return;
        }
        match propose_segment(&fl.short_term, ANCHOR_WINDOW_SECS) {
            Some((start, end)) => {
                self.seg = Some((start, Some(end)));
                self.cursor = start;
                self.status = format!(
                    "proposed {}–{} — p to listen, s to accept, or adjust and re-mark",
                    crate::ui::hms(start),
                    crate::ui::hms(end)
                );
            }
            None => self.status = "no stretch of steady dialogue found — pick one by hand".into(),
        }
    }

    /// Where a marked segment's level ranks within its own film, as a
    /// percentile. Shown next to [`ANCHOR_RANK`] so an anchor that is too quiet
    /// is visible before it is committed.
    pub fn segment_rank(&self) -> Option<f64> {
        let fl = self.selected_loudness()?;
        let (s, e) = match self.seg {
            Some((s, Some(e))) => (s, e),
            _ => return None,
        };
        let a = (s.max(0.0) as usize).min(fl.short_term.len());
        let b = (e.max(0.0) as usize).min(fl.short_term.len());
        if b <= a {
            return None;
        }
        let level = percentile_of(&fl.short_term[a..b], SEGMENT_PERCENTILE)?;
        rank_of(&fl.short_term, level)
    }

    /// Adopt the selected file's anchor as the global target.
    pub fn set_target_from_selection(&mut self) {
        let Some(f) = self.selected_loudness() else { return };
        let Some(r) = f.reference.clone() else {
            self.status = "that file has no anchor yet".into();
            return;
        };
        self.db.target_lufs = r.lufs;
        self.dirty_db = true;
        self.status = format!("target is now {:.1} LUFS — all gains recomputed", r.lufs);
    }
}

/// Last path component, for status lines that must fit on one row.
pub fn base(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Insert `path` just after `cursor`, returning the index it landed at.
///
/// Callers must move their cursor to the returned index. Leaving the cursor put
/// makes every insertion in a run land on the same spot, which silently reverses
/// the order — adding e01, e02, e03 would yield e03, e02, e01.
pub fn insert_after(list: &mut Vec<String>, cursor: usize, path: String) -> usize {
    let at = if list.is_empty() { 0 } else { (cursor + 1).min(list.len()) };
    list.insert(at, path);
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A film of quiet room tone with one sustained dialogue plateau and one
    /// louder-but-spiky action burst. The plateau is the anchor; the burst is
    /// the trap that a level-only search would fall into.
    fn synthetic_film() -> Vec<f32> {
        let mut s = vec![-34.0f32; 600];
        for (i, v) in s.iter_mut().enumerate().take(360).skip(300) {
            // Dialogue: steady, with the small variation speech always has.
            *v = -22.0 + (i % 3) as f32 * 0.5;
        }
        for (i, v) in s.iter_mut().enumerate().take(500).skip(440) {
            // Action: same average, reached by alternating bangs and silence.
            *v = if i % 2 == 0 { -12.0 } else { -40.0 };
        }
        s
    }

    #[test]
    fn proposes_the_steady_plateau_over_the_spiky_burst() {
        let (start, end) = propose_segment(&synthetic_film(), 45).expect("a candidate exists");
        assert!((300.0..=315.0).contains(&start), "expected the plateau, got {start}");
        assert_eq!(end - start, 45.0);
    }

    /// Opening titles and end credits are scored music: loud, sustained, and
    /// exactly the shape the search rewards.
    #[test]
    fn ignores_music_at_the_very_edges() {
        let mut s = synthetic_film();
        for v in s.iter_mut().take(20) {
            *v = -21.0;
        }
        let (start, _) = propose_segment(&s, 45).expect("a candidate exists");
        assert!(start > 20.0, "proposal landed in the opening titles at {start}");
    }

    #[test]
    fn a_film_shorter_than_the_window_yields_no_proposal() {
        assert!(propose_segment(&[-22.0; 30], 45).is_none());
    }

    #[test]
    fn rank_places_a_level_within_the_distribution() {
        let s: Vec<f32> = (0..100).map(|i| -40.0 + i as f32 * 0.2).collect();
        assert_eq!(rank_of(&s, -40.0), Some(0.0));
        assert_eq!(rank_of(&s, -30.0), Some(50.0));
        // Digital silence must not drag the rank of a real level upward.
        let mut with_gaps = s.clone();
        with_gaps.extend(std::iter::repeat_n(-90.0f32, 100));
        assert_eq!(rank_of(&with_gaps, -30.0), rank_of(&s, -30.0));
    }

    /// Adding a season one episode at a time must preserve the order they were
    /// picked in, which means advancing the cursor after each insertion.
    #[test]
    fn sequential_insertions_keep_their_order() {
        let mut list: Vec<String> = vec!["a.mkv".into(), "b.mkv".into()];
        let mut cursor = 0; // sitting on "a.mkv"
        for ep in ["e01", "e02", "e03"] {
            cursor = insert_after(&mut list, cursor, ep.into());
        }
        assert_eq!(list, ["a.mkv", "e01", "e02", "e03", "b.mkv"]);
    }

    #[test]
    fn a_stationary_cursor_is_what_reverses_them() {
        // Documents the old behaviour, so the guarantee above is not mistaken
        // for something `insert_after` provides on its own.
        let mut list: Vec<String> = vec!["a.mkv".into(), "b.mkv".into()];
        for ep in ["e01", "e02", "e03"] {
            insert_after(&mut list, 0, ep.into());
        }
        assert_eq!(list, ["a.mkv", "e03", "e02", "e01", "b.mkv"]);
    }

    #[test]
    fn insertion_into_an_empty_playlist_starts_at_the_front() {
        let mut list: Vec<String> = Vec::new();
        let at = insert_after(&mut list, 0, "first.mkv".into());
        assert_eq!(at, 0);
        let at = insert_after(&mut list, at, "second.mkv".into());
        assert_eq!(at, 1);
        assert_eq!(list, ["first.mkv", "second.mkv"]);
    }

    #[test]
    fn a_cursor_past_the_end_appends_rather_than_panicking() {
        let mut list: Vec<String> = vec!["a.mkv".into()];
        assert_eq!(insert_after(&mut list, 99, "b.mkv".into()), 1);
        assert_eq!(list, ["a.mkv", "b.mkv"]);
    }
}
