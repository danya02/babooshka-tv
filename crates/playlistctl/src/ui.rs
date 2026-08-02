//! Rendering. Lists are drawn manually rather than with `List`/`ListState` so
//! the loudness view can align columns and the timeline can overlay a cursor.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{ANCHOR_RANK, App, Pane, Tab, base};
use crate::model::{is_playable, percentile_of};

/// Height of the timeline axis in dB, and its position relative to the target.
///
/// The axis is pinned to the target rather than fitted to each film. Fitting
/// spends most of the height on brief near-silence no one anchors in, and
/// cannot guarantee the target line is on screen at all — a film whose loudest
/// column falls below the target pushes the one reference that matters off the
/// top. Pinning also keeps dB-per-row identical everywhere, so bar heights stay
/// comparable between films.
///
/// Anything quieter than `target - SPAN_DB + HEADROOM_DB` is off the bottom.
/// That is deliberate: those are pauses and room tone, never anchor candidates.
const SPAN_DB: f64 = 20.0;
const HEADROOM_DB: f64 = 4.0;

const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3), Constraint::Length(2)])
        .split(f.area());

    draw_tabs(f, chunks[0], app);
    match app.tab {
        Tab::Playlist => draw_playlist_tab(f, chunks[1], app),
        Tab::Loudness => draw_loudness_tab(f, chunks[1], app),
    }
    draw_status(f, chunks[2], app);
}

fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    let mark = |on: bool| if on { Style::default().fg(Color::Black).bg(Color::Cyan) } else { Style::default().fg(Color::DarkGray) };
    let line = Line::from(vec![
        Span::styled(" 1 playlist ", mark(app.tab == Tab::Playlist)),
        Span::raw(" "),
        Span::styled(" 2 loudness ", mark(app.tab == Tab::Loudness)),
        Span::raw("   "),
        Span::styled(
            format!("target {:.1} LUFS", app.db.target_lufs),
            Style::default().fg(Color::Magenta),
        ),
        Span::raw("   "),
        Span::styled(
            if app.ignored.is_empty() {
                String::new()
            } else {
                format!("{} ignored   ", app.ignored.len())
            },
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("media {} · control {}", app.media.host, app.control.host),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

// ---- playlist tab -------------------------------------------------------

fn draw_playlist_tab(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(28), Constraint::Percentage(22)])
        .split(area);

    let current = app.current_index();
    let playlist_lines: Vec<Line> = app
        .playlist
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let anchored = app.db.files.get(p).and_then(|f| f.reference.as_ref()).is_some();
            let is_current = Some(i) == current;
            let mut spans = vec![Span::styled(
                format!("{:>3} ", i + 1),
                Style::default().fg(Color::DarkGray),
            )];
            spans.push(Span::styled(
                if is_current { "▶ " } else { "  " },
                Style::default().fg(Color::Yellow),
            ));
            spans.push(Span::styled(
                if anchored { "♪ " } else { "· " },
                Style::default().fg(if anchored { Color::Green } else { Color::DarkGray }),
            ));
            spans.push(Span::raw(app.rel(p).to_string()));
            Line::from(spans)
        })
        .collect();

    // Everything on disk is listed, playable or not, with the unplayable in red
    // so a stray DVD folder or artwork file cannot sit there unnoticed.
    let unlisted = app.unlisted();
    let unplayable = unlisted.iter().filter(|p| !is_playable(p)).count();
    let unlisted_lines: Vec<Line> = unlisted
        .iter()
        .map(|p| {
            let ok = is_playable(p);
            Line::from(Span::styled(
                app.rel(p).to_string(),
                Style::default().fg(if ok { Color::Green } else { Color::Red }),
            ))
        })
        .collect();

    let missing = app.missing();
    let missing_lines: Vec<Line> = missing
        .iter()
        .map(|p| Line::from(Span::styled(app.rel(p).to_string(), Style::default().fg(Color::Red))))
        .collect();

    render_list(
        f,
        cols[0],
        &format!("playlist ({}){}", app.playlist.len(), if app.dirty_playlist { " *" } else { "" }),
        &playlist_lines,
        app.sel[Pane::Playlist as usize],
        app.pane == Pane::Playlist,
    );
    render_list(
        f,
        cols[1],
        &format!(
            "on disk ({}){}",
            unlisted.len(),
            if unplayable > 0 { format!(" · {unplayable} unplayable") } else { String::new() }
        ),
        &unlisted_lines,
        app.sel[Pane::Unlisted as usize],
        app.pane == Pane::Unlisted,
    );
    render_list(
        f,
        cols[2],
        &format!("missing ({})", missing.len()),
        &missing_lines,
        app.sel[Pane::Missing as usize],
        app.pane == Pane::Missing,
    );
}

// ---- loudness tab -------------------------------------------------------

fn draw_loudness_tab(f: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(45), Constraint::Min(8)])
        .split(area);

    let lines: Vec<Line> = app
        .playlist
        .iter()
        .map(|p| {
            let fl = app.db.files.get(p);
            let (i_txt, anchor_txt, gain_txt, style) = match fl {
                Some(f) if f.measured() => {
                    let i = f.integrated.map(|v| format!("{v:>6.1}")).unwrap_or("     —".into());
                    match &f.reference {
                        Some(r) => (
                            i,
                            format!("{:>6.1}", r.lufs),
                            format!("{:>+6.1}", app.db.target_lufs - r.lufs),
                            Style::default().fg(Color::Green),
                        ),
                        None => (i, "     —".into(), "     —".into(), Style::default().fg(Color::Yellow)),
                    }
                }
                _ => (
                    "     —".into(),
                    "     —".into(),
                    "     —".into(),
                    Style::default().fg(Color::DarkGray),
                ),
            };
            Line::from(vec![
                Span::styled(format!("{i_txt} "), Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{anchor_txt} "), style),
                Span::styled(format!("{gain_txt}  "), style.add_modifier(Modifier::BOLD)),
                Span::raw(base(p).to_string()),
            ])
        })
        .collect();

    render_list(
        f,
        rows[0],
        &format!(
            "  integ. anchor   gain  file{}",
            if app.dirty_db { "   *" } else { "" }
        ),
        &lines,
        app.loud_sel,
        true,
    );

    draw_timeline(f, rows[1], app);
}

fn draw_timeline(f: &mut Frame, area: Rect, app: &App) {
    let title = match app.selected_loudness_path() {
        Some(p) => format!("loudness · {}", base(&p)),
        None => "loudness".to_string(),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 3 || inner.width < 4 {
        return;
    }

    let Some(fl) = app.selected_loudness() else {
        f.render_widget(
            Paragraph::new("no file selected").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    };
    if !fl.measured() {
        f.render_widget(
            Paragraph::new("not measured — press m to analyse this file, M for all")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    // Reserve the last two rows for the numeric readout.
    let graph_h = inner.height.saturating_sub(2).max(1);
    let graph = Rect { x: inner.x, y: inner.y, width: inner.width, height: graph_h };
    let info = Rect {
        x: inner.x,
        y: inner.y + graph_h,
        width: inner.width,
        height: inner.height - graph_h,
    };

    let w = graph.width as usize;
    let series = &fl.short_term;
    let secs_per_col = (series.len() as f64 / w as f64).max(1e-9);

    // Column value is the p85 of its span, the same statistic a segment's
    // loudness is measured with — so a column reads as "what anchoring here
    // would score". Taking the max instead saturates: at ~20s per column the
    // loudest second is near the film's peak almost everywhere, which flattens
    // the whole plot against its ceiling.
    let mut colv: Vec<f64> = Vec::with_capacity(w);
    for x in 0..w {
        let a = ((x as f64 * secs_per_col).floor() as usize).min(series.len().saturating_sub(1));
        let b = ((((x + 1) as f64 * secs_per_col).ceil() as usize).max(a + 1)).min(series.len());
        colv.push(percentile_of(&series[a..b], 85.0).unwrap_or(f64::NEG_INFINITY));
    }

    // The axis is pinned to the target, so it is the same in every film and the
    // reference line is always on screen. Extended upward only if a film
    // genuinely overshoots the headroom, which would otherwise clip flat.
    let peak = colv.iter().copied().filter(|v| v.is_finite()).fold(f64::NEG_INFINITY, f64::max);
    let ceil = (app.db.target_lufs + HEADROOM_DB).max(if peak.is_finite() { peak } else { f64::NEG_INFINITY });
    let floor = ceil - SPAN_DB;

    // A column reaching this line is already at the loudness every film is
    // being aligned to, so it is a viable anchor without any gain.
    let frac = (app.db.target_lufs - floor) / (ceil - floor);
    let target_row =
        graph_h.saturating_sub(1) - ((frac * (graph_h - 1) as f64).round() as u16).min(graph_h - 1);

    let cursor_col = ((app.cursor / secs_per_col) as usize).min(w.saturating_sub(1));
    let (seg_a, seg_b) = match app.seg {
        Some((s, Some(e))) => (
            ((s / secs_per_col) as usize).min(w),
            ((e / secs_per_col) as usize).min(w),
        ),
        Some((s, None)) => {
            let a = ((s / secs_per_col) as usize).min(w);
            (a, a + 1)
        }
        None => (usize::MAX, usize::MAX),
    };

    // Each cell resolves one eighth-block, so the effective resolution is
    // graph_h * 8 steps across the LUFS window.
    let steps = graph_h as f64 * 8.0;
    let mut lines: Vec<Line> = Vec::with_capacity(graph_h as usize);
    for row in 0..graph_h {
        let mut spans: Vec<Span> = Vec::with_capacity(w);
        // Row 0 is the top of the graph.
        let row_base = (graph_h - 1 - row) as f64 * 8.0;
        for (x, &v) in colv.iter().enumerate() {
            let frac = ((v - floor) / (ceil - floor)).clamp(0.0, 1.0);
            let filled = if v.is_finite() { frac * steps } else { 0.0 };
            let eighths = (filled - row_base).clamp(0.0, 8.0).round() as usize;
            let in_seg = x >= seg_a && x < seg_b;
            // Only the envelope carries information; the fill below it is the
            // same in every column, so it is dimmed to stop it dominating.
            let is_crest = eighths > 0 && filled <= row_base + 8.0;
            let style = if x == cursor_col {
                Style::default().fg(Color::White).bg(Color::Blue)
            } else if in_seg {
                Style::default().fg(Color::Black).bg(Color::Yellow)
            } else if is_crest {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::Blue)
            };
            // The target line shows through wherever the film is below it.
            let ch = if eighths == 0 && row == target_row {
                '─'
            } else {
                BLOCKS[eighths]
            };
            let style = if eighths == 0 && row == target_row {
                style.fg(Color::Magenta)
            } else {
                style
            };
            spans.push(Span::styled(ch.to_string(), style));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), graph);

    // Numeric readout: where the cursor is, and how the file compares.
    let cur_v = colv.get(cursor_col).copied().unwrap_or(f64::NAN);
    // A segment's rank within its own film, against the rank the calibrated
    // reference sits at. This is the check that catches an anchor set on a
    // quiet scene, which reads as a plausible gain but is not one.
    let rank_txt = match app.segment_rank() {
        Some(r) => format!(" · rank {r:.0}% of ref {ANCHOR_RANK:.0}%"),
        None => String::new(),
    };
    let seg_txt = match app.seg {
        Some((s, Some(e))) => format!("segment {}–{}{rank_txt}", hms(s), hms(e)),
        Some((s, None)) => format!("segment {}–… (] to close)", hms(s)),
        None => "no segment ([ to open)".to_string(),
    };
    let anchor_txt = match &fl.reference {
        Some(r) => format!(
            "anchor {}–{} = {:.1} LUFS → gain {:+.1} dB",
            hms(r.start),
            hms(r.end),
            r.lufs,
            app.db.target_lufs - r.lufs
        ),
        None => "no anchor set".to_string(),
    };
    let pct = |q: f64| fl.percentile(q).map(|v| format!("{v:.1}")).unwrap_or("—".into());
    let info_lines = vec![
        Line::from(vec![
            Span::styled(format!("t {} ", hms(app.cursor)), Style::default().fg(Color::Blue)),
            Span::raw(format!("({cur_v:.1} LUFS)  ")),
            Span::styled(seg_txt, Style::default().fg(Color::Yellow)),
            // The axis is fitted per film, so its extent has to be stated or a
            // tall bar in one film reads as louder than a short bar in another.
            Span::styled(
                format!("   scale {floor:.0}…{ceil:.0} LUFS"),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::styled(anchor_txt, Style::default().fg(Color::Green)),
            Span::styled(
                format!(
                    "   p50 {}  p85 {}  p95 {}  LRA {}",
                    pct(50.0),
                    pct(85.0),
                    pct(95.0),
                    fl.lra.map(|v| format!("{v:.1}")).unwrap_or("—".into())
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
    ];
    f.render_widget(Paragraph::new(info_lines), info);
}

// ---- shared -------------------------------------------------------------

/// Draw a bordered, scrollable list with the selected row highlighted.
fn render_list(f: &mut Frame, area: Rect, title: &str, lines: &[Line], sel: usize, focused: bool) {
    let border = if focused { Color::Cyan } else { Color::DarkGray };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(title.to_string());
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let h = inner.height as usize;
    // Keep the selection in view without tracking scroll state across frames.
    let offset = sel.saturating_sub(h.saturating_sub(1) / 2).min(lines.len().saturating_sub(h).max(0));
    let visible: Vec<Line> = lines
        .iter()
        .enumerate()
        .skip(offset)
        .take(h)
        .map(|(i, l)| {
            if i == sel && focused {
                let spans: Vec<Span> = l
                    .spans
                    .iter()
                    .map(|s| Span::styled(s.content.clone(), s.style.bg(Color::DarkGray).add_modifier(Modifier::BOLD)))
                    .collect();
                Line::from(spans)
            } else if i == sel {
                Line::from(
                    l.spans
                        .iter()
                        .map(|s| Span::styled(s.content.clone(), s.style.add_modifier(Modifier::BOLD)))
                        .collect::<Vec<_>>(),
                )
            } else {
                l.clone()
            }
        })
        .collect();
    f.render_widget(Paragraph::new(visible), inner);
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let keys = match app.tab {
        Tab::Playlist => "h/l pane  j/k move  J/K reorder  a add  i ignore  I ignore-unplayable  d del  x purge  r rescan  w write  q quit",
        Tab::Loudness => "j/k file  m/M measure  ←/→ ±5s  H/L ±60s  A propose  [ ] mark  p listen  s set anchor  t re-baseline target  g export gains  w write  q quit",
    };
    let pending = if app.pending > 0 {
        Span::styled(
            format!(" [{} measuring] ", app.pending),
            Style::default().fg(Color::Black).bg(Color::Yellow),
        )
    } else {
        Span::raw("")
    };
    let lines = vec![
        Line::from(vec![pending, Span::raw(" "), Span::raw(app.status.clone())]),
        Line::from(Span::styled(keys, Style::default().fg(Color::DarkGray))),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

/// Format seconds as `h:mm:ss`, the form mpv and play-state timestamps read in.
pub fn hms(t: f64) -> String {
    let t = t.max(0.0) as u64;
    format!("{}:{:02}:{:02}", t / 3600, (t % 3600) / 60, t % 60)
}
