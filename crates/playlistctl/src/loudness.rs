//! Loudness measurement, driven by ffmpeg's `ebur128` filter on the media host.
//!
//! One pass yields everything: the short-term series (emitted every 100ms), the
//! gated integrated loudness, and the loudness range. `-vn` keeps ffmpeg from
//! decoding video it does not need, which is most of the cost.

use crate::model::{FileLoudness, RefSegment, percentile_of};
use crate::remote::{Remote, shq};

/// Percentile of short-term loudness used to characterise a dialogue segment.
///
/// Chosen empirically: across 6s, 15s and 60s windows around a scene calibrated
/// by ear, p85 held steady within 1 dB (-19.5 to -20.4 LUFS) while the median
/// swung by 8 dB depending on how much pause the window happened to catch.
/// Dialogue is bursty, so a high percentile tracks speech and a central one
/// tracks the gaps between words.
pub const SEGMENT_PERCENTILE: f64 = 85.0;

/// Parsed output of one `ebur128` pass.
pub struct Analysis {
    pub duration: f64,
    pub integrated: Option<f64>,
    pub lra: Option<f64>,
    /// Short-term loudness at the filter's native ~10 Hz rate.
    pub short_term: Vec<(f64, f32)>,
}

/// Extract the short-term series and summary from ffmpeg's stderr.
pub fn parse_ebur128(out: &str) -> Analysis {
    let mut short_term = Vec::new();
    let mut integrated = None;
    let mut lra = None;
    let mut duration = 0.0f64;

    let mut in_summary = false;
    for line in out.lines() {
        let line = line.trim();

        // The per-window progress lines look like:
        //   [Parsed_ebur128_0 @ ..] t: 12.3  TARGET:-23 LUFS  M: -20.1 S: -21.4 ...
        if let Some(t) = field_after(line, "t:")
            && let Some(s) = field_after(line, "S:")
        {
            duration = duration.max(t);
            // Early windows read `nan` until the 3s short-term buffer fills.
            if s.is_finite() {
                short_term.push((t, s as f32));
            }
            continue;
        }

        // The trailing summary block repeats `I:` and `LRA:` on their own lines.
        // ffmpeg prefixes the header with its filter tag, so match the tail.
        if line.ends_with("Summary:") {
            in_summary = true;
            continue;
        }
        if in_summary {
            if let Some(v) = field_after(line, "I:") {
                integrated = Some(v);
            } else if let Some(v) = field_after(line, "LRA:") {
                lra = Some(v);
            }
        }
    }

    Analysis { duration, integrated, lra, short_term }
}

/// Read the number following `key` on a line, if it parses.
fn field_after(line: &str, key: &str) -> Option<f64> {
    let idx = line.find(key)?;
    let rest = &line[idx + key.len()..];
    let tok = rest.split_whitespace().next()?;
    tok.parse::<f64>().ok()
}

/// Reduce the ~10 Hz short-term series to one sample per second.
///
/// Each output sample is the max of its second, so a brief loud line is not
/// averaged away — percentiles are meant to find speech peaks, not smooth them.
fn decimate_to_1hz(series: &[(f64, f32)]) -> Vec<f32> {
    let mut out: Vec<f32> = Vec::new();
    for &(t, s) in series {
        let bucket = t.floor().max(0.0) as usize;
        if bucket >= out.len() {
            out.resize(bucket + 1, f32::NEG_INFINITY);
        }
        if s > out[bucket] {
            out[bucket] = s;
        }
    }
    // Buckets with no window at all (possible across a seek) stay as -inf; mark
    // them as silence so downstream filters drop them rather than panicking.
    for v in &mut out {
        if !v.is_finite() {
            *v = -120.0;
        }
    }
    out
}

/// Measure a whole file. Runs on the media host, where the file is local.
pub fn measure_file(remote: &Remote, path: &str) -> Result<FileLoudness, String> {
    let cmd = format!(
        "ffmpeg -hide_banner -nostats -i {p} -vn -af ebur128 -f null - 2>&1",
        p = shq(path)
    );
    let out = remote.run_capture_stderr(&cmd)?;
    let a = parse_ebur128(&out);
    if a.short_term.is_empty() {
        return Err(format!("no loudness data returned for {path} (no audio stream?)"));
    }
    Ok(FileLoudness {
        duration: a.duration,
        integrated: a.integrated,
        lra: a.lra,
        short_term: decimate_to_1hz(&a.short_term),
        reference: None,
    })
}

/// Measure one segment and return it as a dialogue anchor.
///
/// `-ss` is placed before `-i` so ffmpeg seeks rather than decoding up to the
/// start point — the difference between a second and several minutes.
pub fn measure_segment(
    remote: &Remote,
    path: &str,
    start: f64,
    end: f64,
) -> Result<RefSegment, String> {
    let dur = (end - start).max(1.0);
    let cmd = format!(
        "ffmpeg -hide_banner -nostats -ss {start:.3} -i {p} -vn -t {dur:.3} -af ebur128 -f null - 2>&1",
        p = shq(path)
    );
    let out = remote.run_capture_stderr(&cmd)?;
    let a = parse_ebur128(&out);
    let series: Vec<f32> = a.short_term.iter().map(|&(_, s)| s).collect();
    let lufs = percentile_of(&series, SEGMENT_PERCENTILE)
        .ok_or_else(|| format!("segment {start:.0}-{end:.0}s of {path} is silent"))?;
    Ok(RefSegment { start, end, lufs })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real ffmpeg output, trimmed: progress lines plus the summary block.
    const SAMPLE: &str = "\
[Parsed_ebur128_0 @ 0x7f45a0003540] t: 0.099479  TARGET:-23 LUFS    M:-120.7 S:-120.7     I: -70.0 LUFS       LRA:   0.0 LU
[Parsed_ebur128_0 @ 0x7f45a0003540] t: 0.500312  TARGET:-23 LUFS    M:   nan S:   nan     I: -70.0 LUFS       LRA:   0.0 LU
[Parsed_ebur128_0 @ 0x7f45a0003540] t: 1.200312  TARGET:-23 LUFS    M: -23.0 S: -21.4     I: -21.3 LUFS       LRA:  15.7 LU
[Parsed_ebur128_0 @ 0x7f45a0003540] t: 1.700312  TARGET:-23 LUFS    M: -20.0 S: -19.2     I: -21.3 LUFS       LRA:  15.7 LU
[Parsed_ebur128_0 @ 0x7f45a0003540] t: 2.999146  TARGET:-23 LUFS    M: -21.3 S: -21.3     I: -21.3 LUFS       LRA:  15.6 LU
[Parsed_ebur128_0 @ 0x7f45a0003540] Summary:

  Integrated loudness:
    I:         -21.3 LUFS
    Threshold: -32.7 LUFS

  Loudness range:
    LRA:        15.6 LU
    Threshold: -42.8 LUFS
    LRA low:   -35.9 LUFS
    LRA high:  -20.2 LUFS
";

    #[test]
    fn parses_summary_without_confusing_it_for_progress() {
        let a = parse_ebur128(SAMPLE);
        assert_eq!(a.integrated, Some(-21.3));
        // `LRA low:` and `LRA high:` must not overwrite `LRA:`.
        assert_eq!(a.lra, Some(15.6));
        assert!((a.duration - 2.999146).abs() < 1e-6);
    }

    #[test]
    fn drops_nan_windows_before_the_short_term_buffer_fills() {
        let a = parse_ebur128(SAMPLE);
        assert_eq!(a.short_term.len(), 4);
        assert!(a.short_term.iter().all(|(_, s)| s.is_finite()));
    }

    #[test]
    fn decimation_keeps_the_peak_of_each_second() {
        let a = parse_ebur128(SAMPLE);
        let d = decimate_to_1hz(&a.short_term);
        assert_eq!(d.len(), 3);
        // Second 0 holds only the -120.7 window.
        assert!((d[0] - -120.7).abs() < 0.01);
        // Second 1 holds -21.4 and -19.2; the louder one survives.
        assert!((d[1] - -19.2).abs() < 0.01);
        assert!((d[2] - -21.3).abs() < 0.01);
    }

    #[test]
    fn percentile_ignores_near_silence() {
        // Without the -70 LUFS gate the leader would drag p85 far down.
        let series = vec![-120.0f32, -120.0, -30.0, -25.0, -20.0, -19.0];
        let p = crate::model::percentile_of(&series, 85.0).unwrap();
        assert!(p > -21.0, "expected a dialogue-level percentile, got {p}");
    }

    #[test]
    fn percentile_of_pure_silence_is_absent_rather_than_wrong() {
        assert!(crate::model::percentile_of(&[-120.0, -119.0], 85.0).is_none());
    }
}
