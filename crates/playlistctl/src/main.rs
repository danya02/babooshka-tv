//! playlistctl — a terminal editor for babooshka's playlist and loudness data.
//!
//! Runs on a workstation and reaches the media host over SSH. See `remote.rs`
//! for why SSH rather than an NFS mount, and `loudness.rs` for how gains are
//! derived from operator-chosen dialogue segments.

mod app;
mod loudness;
mod model;
mod mpv;
mod remote;
mod ui;

use std::io;
use std::time::Duration;

use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use crossterm::execute;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use app::{App, Pane, Paths, Tab};
use remote::Remote;

/// Loudness that a film's dialogue anchor is aligned to, in LUFS.
///
/// Derived by measurement, not convention: a scene in "Иван Васильевич меняет
/// профессию" around t=2652s was calibrated by ear to a comfortable level, and
/// its short-term p85 came out at -20.4 LUFS across every window size tried.
/// Everything else is matched to that.
pub const DEFAULT_TARGET_LUFS: f64 = -20.4;

#[derive(Parser, Debug)]
#[command(about = "Edit babooshka's playlist and per-file loudness gains", long_about = None)]
struct Args {
    /// SSH destination of the media host holding the video files.
    ///
    /// Scanning and ffmpeg run here, where the files are on local disk.
    #[arg(long, default_value = "danya@10.22.0.60")]
    host: String,

    /// SSH destination used for reading and writing the JSON state files.
    ///
    /// Must be a host that mounts the share over NFS: the export squashes
    /// clients to the owning uid, so the Pi may replace files under /srv while
    /// a local session on the media host may not.
    #[arg(long, default_value = "danya@10.22.0.51")]
    control_host: String,

    /// Directory on the media host to scan for videos.
    #[arg(long, default_value = "/srv/babooshka-tv")]
    root: String,

    /// Playlist JSON on the media host.
    #[arg(long, default_value = "/srv/playlist.json")]
    playlist: String,

    /// babooshka's saved resume point, read to warn before editing it.
    #[arg(long, default_value = "/srv/play-state.json")]
    play_state: String,

    /// Loudness database written by this tool.
    #[arg(long, default_value = "/srv/loudness.json")]
    loudness_db: String,

    /// Flat gain map consumed by babooshka.
    #[arg(long, default_value = "/srv/gain_db.json")]
    gain_out: String,

    /// Override the target loudness in LUFS.
    #[arg(long)]
    target: Option<f64>,

    /// Regenerate gain_db.json from the loudness database and exit.
    #[arg(long)]
    export_only: bool,

    /// Measure every playlist file lacking data, save the database, and exit.
    ///
    /// The whole-file pass only produces the timeline; dialogue anchors still
    /// have to be chosen by ear in the TUI before any gain is derived.
    #[arg(long)]
    measure_all: bool,
}

fn main() {
    let args = Args::parse();
    let media = Remote::new(args.host);
    let control = Remote::new(args.control_host);
    let paths = Paths {
        root: args.root,
        playlist: args.playlist,
        play_state: args.play_state,
        loudness_db: args.loudness_db,
        gain_out: args.gain_out,
    };

    let mut app = match App::new(media, control, paths, args.target) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("playlistctl: {e}");
            std::process::exit(1);
        }
    };

    if args.measure_all {
        measure_all(&mut app);
        return;
    }

    if args.export_only {
        app.export_gain();
        println!("{}", app.status);
        return;
    }

    if let Err(e) = run_tui(&mut app) {
        eprintln!("playlistctl: {e}");
        std::process::exit(1);
    }
}

/// Headless equivalent of pressing `M` in the TUI, for scripted runs.
fn measure_all(app: &mut App) {
    app.queue_measure_all_unmeasured();
    println!("{}", app.status);
    let total = app.pending;
    while app.pending > 0 {
        std::thread::sleep(Duration::from_millis(250));
        if app.poll_jobs() {
            println!("[{}/{total}] {}", total - app.pending, app.status);
        }
    }
    // poll_jobs already flushed each result; this only catches a trailing
    // failure and reports where the data landed.
    if app.dirty_db {
        app.save_db();
    }
    println!("database: {}", app.paths.loudness_db);
}

fn run_tui(app: &mut App) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;

    let res = event_loop(&mut term, app);

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    res
}

fn event_loop(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    loop {
        term.draw(|f| ui::draw(f, app))?;

        // Poll rather than block so finished measurements appear without a
        // keypress; the worker has no way to interrupt the event loop.
        if event::poll(Duration::from_millis(200))?
            && let Event::Key(k) = event::read()?
            && k.kind == KeyEventKind::Press
        {
            handle_key(app, k.code, k.modifiers);
        }
        app.poll_jobs();
        // Scrubbing in the mpv window walks the cursor along the timeline, so
        // a scene can be found by watching rather than by reading the graph.
        app.poll_mpv();

        if app.quit {
            return Ok(());
        }
    }
}

fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    // Global keys first.
    match code {
        KeyCode::Char('q') => {
            if app.dirty_playlist || app.dirty_db {
                app.status = "unsaved changes — w to write, Q to discard and quit".into();
            } else {
                app.quit = true;
            }
            return;
        }
        KeyCode::Char('Q') => {
            app.quit = true;
            return;
        }
        KeyCode::Char('1') => {
            app.tab = Tab::Playlist;
            return;
        }
        KeyCode::Char('2') => {
            app.tab = Tab::Loudness;
            return;
        }
        KeyCode::Tab => {
            app.tab = if app.tab == Tab::Playlist { Tab::Loudness } else { Tab::Playlist };
            return;
        }
        _ => {}
    }

    match app.tab {
        Tab::Playlist => handle_playlist_key(app, code, mods),
        Tab::Loudness => handle_loudness_key(app, code, mods),
    }
}

fn handle_playlist_key(app: &mut App, code: KeyCode, _mods: KeyModifiers) {
    let pane = app.pane;
    let len = app.pane_items(pane).len();
    let sel = &mut app.sel[pane as usize];
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            if len > 0 {
                *sel = (*sel + 1).min(len - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            *sel = sel.saturating_sub(1);
        }
        KeyCode::Char('g') => *sel = 0,
        KeyCode::Char('G') => *sel = len.saturating_sub(1),
        KeyCode::Char('h') | KeyCode::Left => {
            app.pane = match pane {
                Pane::Playlist => Pane::Missing,
                Pane::Unlisted => Pane::Playlist,
                Pane::Missing => Pane::Unlisted,
            }
        }
        KeyCode::Char('l') | KeyCode::Right => {
            app.pane = match pane {
                Pane::Playlist => Pane::Unlisted,
                Pane::Unlisted => Pane::Missing,
                Pane::Missing => Pane::Playlist,
            }
        }
        KeyCode::Char('J') => app.move_item(1),
        KeyCode::Char('K') => app.move_item(-1),
        KeyCode::Char('a') | KeyCode::Enter => {
            if pane == Pane::Unlisted {
                app.promote();
            }
        }
        KeyCode::Char('i') => {
            if pane == Pane::Unlisted {
                app.ignore_selected();
            }
        }
        KeyCode::Char('I') => app.ignore_all_unplayable(),
        KeyCode::Char('U') => app.clear_ignored(),
        KeyCode::Char('d') => app.remove_selected(),
        KeyCode::Char('D') => app.force_remove_selected(),
        KeyCode::Char('x') => app.purge_missing(),
        KeyCode::Char('r') => app.rescan(),
        KeyCode::Char('w') => app.save_playlist(),
        _ => {}
    }
}

fn handle_loudness_key(app: &mut App, code: KeyCode, _mods: KeyModifiers) {
    let len = app.playlist.len();
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            if len > 0 {
                app.loud_sel = (app.loud_sel + 1).min(len - 1);
                app.cursor = 0.0;
                app.seg = None;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.loud_sel = app.loud_sel.saturating_sub(1);
            app.cursor = 0.0;
            app.seg = None;
        }
        KeyCode::Left => seek(app, -5.0),
        KeyCode::Right => seek(app, 5.0),
        KeyCode::Char('H') => seek(app, -60.0),
        KeyCode::Char('L') => seek(app, 60.0),
        KeyCode::Char('h') => seek(app, -5.0),
        KeyCode::Char('l') => seek(app, 5.0),
        KeyCode::Char('m') => {
            if let Some(p) = app.selected_loudness_path() {
                app.status = format!("measuring {}…", app::base(&p));
                app.queue_measure(p);
            }
        }
        KeyCode::Char('M') => app.queue_measure_all_unmeasured(),
        KeyCode::Char('[') => {
            app.seg = Some((app.cursor, None));
            app.sync_mpv_loop();
            app.status = format!("segment opens at {}", ui::hms(app.cursor));
        }
        KeyCode::Char(']') => match app.seg {
            Some((s, _)) if app.cursor > s => {
                app.seg = Some((s, Some(app.cursor)));
                // Mirrored into mpv's A-B loop, so p repeats just this stretch.
                app.sync_mpv_loop();
                app.status = format!("segment {} – {}", ui::hms(s), ui::hms(app.cursor));
            }
            Some(_) => app.status = "segment end must come after its start".into(),
            None => app.status = "press [ first to open a segment".into(),
        },
        KeyCode::Char('A') => {
            app.propose_anchor();
            app.sync_mpv_loop();
            app.sync_mpv_position();
        }
        KeyCode::Char('p') => app.preview(),
        KeyCode::Char('s') => match (app.selected_loudness_path(), app.seg) {
            (Some(p), Some((s, Some(e)))) => {
                app.status = format!("measuring anchor {} – {}…", ui::hms(s), ui::hms(e));
                app.queue_segment(p, s, e);
            }
            (Some(_), _) => app.status = "mark a segment with [ and ] first".into(),
            _ => {}
        },
        KeyCode::Char('t') => app.set_target_from_selection(),
        KeyCode::Char('g') => app.export_gain(),
        KeyCode::Char('w') => app.save_db(),
        _ => {}
    }
}

fn seek(app: &mut App, delta: f64) {
    let max = app.selected_loudness().map(|f| f.duration).unwrap_or(0.0);
    app.cursor = (app.cursor + delta).clamp(0.0, max.max(0.0));
    app.sync_mpv_position();
}
