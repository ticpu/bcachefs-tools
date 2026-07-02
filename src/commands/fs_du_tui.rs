// bcachefs fs du --tui: interactive btdu-style live disk-usage browser.
//
// Reuses the batch sampler's per-sample classification (classify_sample,
// DirNode, ThreadAccum, ReflinkMap, resolve_inum_to_path, try_attribute —
// all `pub(super)` in fs_du.rs specifically for this module) but with two
// structural differences forced by having no fixed sample count:
//
//  - Batch mode pre-scales each sample's weight to total_sectors/n_samples
//    (fixed, known up front) so DirNode totals are already in sector
//    units. Continuous sampling has no final N, so this module instead
//    feeds classify_sample a constant weight of 1.0 (a raw visit count)
//    and defers the count -> sectors conversion to render time:
//    estimated_sectors = raw_count * (total_sectors / samples_so_far).
//    That scale factor shrinks as more samples arrive, so every displayed
//    number keeps refining live instead of carrying a stale weight from
//    whatever N happened to be when the sample landed.
//  - Path resolution can't block sampling (there's no "phase 2"): an
//    extents/reflink sample whose (inode, snapshot) hasn't been resolved
//    yet is held in App::pending/queued and re-attributed once the
//    resolver pool answers. It does NOT appear as a tree row — a moving
//    target would be a poor fit for a sorted list — instead the header
//    reports it as a "resolving: N inodes / X pending" status line. That
//    pending weight is already included in the header's used(est) total
//    (per-device accounting in classify_sample happens before attribution
//    is even attempted) but deliberately left out of every DirNode total
//    until resolved, so the visible tree's percentages stay internally
//    consistent instead of budgeting for an amount that keeps moving.
//    "Directory" is likewise inferred from the sampled tree (a node with
//    children) rather than a real stat call, since BCH_IOCTL_REV_MAP /
//    the ancestor-walk resolver never hand back one — that's the only
//    notion of "directory" available here.
//
// Threading: n sampler threads and n resolver threads, all spun up inside
// one std::thread::scope alongside the UI event loop, which runs on the
// scope's calling thread so it can interleave with the still-running
// workers. Quit signals sampler threads via an AtomicBool (checked once
// per ioctl) and resolver threads by dropping the work-queue Sender
// (their blocking recv() then errors out) - both are join()ed as scope()
// returns, so no thread outlives the TUI.
//
// Pause ('p'): parks both sampler and resolver threads. A shared
// PauseState (Mutex<bool> + Condvar) parks each thread between ioctls
// rather than having it busy-poll, so a paused TUI does no IO/CPU at
// all — the intended use case is a production server where "paused" must
// mean zero load, not "stop taking new samples but keep draining the
// resolve backlog for accuracy". Each loop checks pause at the top of its
// iteration, before touching the work queue: an in-flight resolution
// finishes its current key, then parks, so it never parks while holding
// the receiver lock.
//
// Terminal restore: the crate builds with `panic = "abort"` in release
// (fs/alloc catch_unwind can't safely swallow a libbcachefs-originated
// panic — see Cargo.toml), so a panic never unwinds there and a Drop
// guard alone would never run. TerminalGuard instead installs a panic
// hook that restores the terminal before delegating to the previous hook;
// that hook always runs before the abort. The Drop impl covers the
// unwinding (dev/test-build) case and the normal-return path.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::{cursor, execute, terminal};
use rand::distributions::WeightedIndex;
use rand::rngs::StdRng;
use rand::SeedableRng;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

use super::fs_du;
use super::fs_du::{AncestorCache, DeviceGeom, DirNode, PathCache, PerDeviceCounters, ReflinkMap, ThreadAccum};
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::sysfs::DevInfo;

const TICK: Duration = Duration::from_millis(100);
/// Samples a sampler thread accumulates locally before flushing a batch to
/// the UI thread. Bounds channel traffic without making the display feel
/// chunky (at any plausible per-thread ioctl rate this flushes many times
/// per TICK).
const SAMPLE_FLUSH_BATCH: u64 = 256;
const LIST_BAR_WIDTH: usize = 12;

// ──────────────────────────── Terminal setup ────────────────────────────

struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode().context("enabling raw mode")?;
        execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)
            .context("entering alternate screen")?;

        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            default_hook(info);
        }));

        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
        // The panic hook installed above is left in place rather than
        // restored to the previous one: it's a strict superset (restore,
        // then delegate), so a panic anywhere else in the process for the
        // rest of this run is still reported normally.
    }
}

fn restore_terminal() {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

// ──────────────────────────── Pause ────────────────────────────

/// Shared pause flag for sampler threads, toggled by 'p'. Sampler threads
/// park on the Condvar instead of busy-polling an AtomicBool.
struct PauseState {
    paused: Mutex<bool>,
    cv: Condvar,
}

impl PauseState {
    fn new() -> Self {
        PauseState { paused: Mutex::new(false), cv: Condvar::new() }
    }

    fn toggle(&self) {
        let mut paused = self.paused.lock().unwrap();
        *paused = !*paused;
        if !*paused {
            self.cv.notify_all();
        }
    }

    fn is_paused(&self) -> bool {
        *self.paused.lock().unwrap()
    }

    /// Block while paused. Returns once resumed or `stop` is set (the
    /// caller sets `stop` and notifies this Condvar together on shutdown,
    /// so a paused sampler thread wakes immediately rather than staying
    /// parked until the next resume).
    fn wait_while_paused(&self, stop: &AtomicBool) {
        let mut paused = self.paused.lock().unwrap();
        while *paused && !stop.load(Ordering::Relaxed) {
            paused = self.cv.wait(paused).unwrap();
        }
    }
}

// ──────────────────────────── Worker messages ────────────────────────────

enum SamplerMsg {
    Batch { accum: ThreadAccum, n_samples: u64 },
    Error(anyhow::Error),
}

enum ResolverMsg {
    Resolved { key: (u64, u32), result: Option<(String, u16)> },
    Error(anyhow::Error),
}

/// Sample continuously via BCH_IOCTL_REV_MAP until `stop` is set, flushing
/// locally-classified batches to `tx`. Each sample is classified with
/// weight 1.0 (see module notes) rather than batch mode's pre-scaled
/// sector weight. Parks on `pause` between ioctls while paused.
#[allow(clippy::too_many_arguments)]
fn sampler_loop(
    handle: &BcachefsHandle,
    seed: u64,
    devs: &[DevInfo],
    geoms: &[DeviceGeom],
    dev_dist: &WeightedIndex<u64>,
    stop: &AtomicBool,
    pause: &PauseState,
    tx: &Sender<SamplerMsg>,
) {
    let mut rng = StdRng::seed_from_u64(seed);
    let new_accum = || ThreadAccum::new(devs.len());
    let mut accum = new_accum();
    let mut batch_n: u64 = 0;

    while !stop.load(Ordering::Relaxed) {
        pause.wait_while_paused(stop);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match fs_du::sample_one(handle, &mut rng, dev_dist, geoms) {
            Ok((arg, dev_pos)) => {
                fs_du::classify_sample(&arg, 1.0, dev_pos, &mut accum);
                batch_n += 1;
                if batch_n >= SAMPLE_FLUSH_BATCH {
                    let flushed = std::mem::replace(&mut accum, new_accum());
                    if tx.send(SamplerMsg::Batch { accum: flushed, n_samples: batch_n }).is_err() {
                        return; // UI gone
                    }
                    batch_n = 0;
                }
            }
            Err(e) => {
                let _ = tx.send(SamplerMsg::Error(e));
                return;
            }
        }
    }
}

/// Resolve (inum, snapshot) keys from a shared work queue until it closes
/// (normal shutdown: the UI thread drops its Sender once it stops
/// enqueueing new keys). Parks on `pause` between keys while paused —
/// checked before locking `work_rx`, never while holding it — so a paused
/// TUI does no resolver IO either. A resolver parked at shutdown wakes via
/// `pause.wait_while_paused`'s `stop` check, then falls through to `recv()`,
/// which errors out once the UI thread has dropped the work-queue Sender
/// (it always does so before setting `stop`), so quit-while-paused exits
/// the same way as any other shutdown.
fn resolver_loop(
    handle: &BcachefsHandle,
    work_rx: &Mutex<Receiver<(u64, u32)>>,
    tx: &Sender<ResolverMsg>,
    stop: &AtomicBool,
    pause: &PauseState,
    ancestors: &AncestorCache,
) {
    loop {
        pause.wait_while_paused(stop);

        let key = {
            let guard = match work_rx.lock() {
                Ok(g) => g,
                Err(_) => {
                    let _ = tx.send(ResolverMsg::Error(anyhow::anyhow!(
                        "resolver work queue mutex poisoned (peer thread panicked)"
                    )));
                    return;
                }
            };
            guard.recv()
        };
        let key = match key {
            Ok(k) => k,
            Err(_) => return, // channel closed: normal shutdown
        };

        match fs_du::resolve_inum_to_path(handle, ancestors, key.0, key.1) {
            Ok(result) => {
                if tx.send(ResolverMsg::Resolved { key, result }).is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = tx.send(ResolverMsg::Error(e));
                return;
            }
        }
    }
}

// ──────────────────────────── UI state ────────────────────────────

/// One row in the currently displayed directory listing: a real `DirNode`
/// child, enterable if it has children of its own.
struct Row {
    name: String,
    count: f64,
    enterable: bool,
}

struct App<'a> {
    devs: &'a [DevInfo],
    total_sectors: u64,
    per_dev: Vec<PerDeviceCounters>,
    tree: DirNode,
    samples_taken: u64,
    /// Windowed sample rate: last measurement (time, samples_taken) and the
    /// rate over that window - a lifetime average would decay asymptotically
    /// instead of dropping to 0 when sampling is paused.
    rate_prev: (Instant, u64),
    rate: f64,
    path_cache: PathCache,
    /// Raw-count weight accumulated for (inode, snapshot) keys seen but
    /// not yet resolved, folded into the synthetic root-level
    /// `<resolving>` row until the resolver pool answers.
    pending: HashMap<(u64, u32), f64>,
    /// Keys already sent to the resolver work queue, so a key seen again
    /// before it resolves isn't re-enqueued.
    queued: HashSet<(u64, u32)>,
    reflink_map: ReflinkMap,
    cwd: Vec<String>,
    cursor: usize,
    list_state: ListState,
    error: Option<String>,
    error_count: u32,
    fs_path: String,
    seed: u64,
}

impl<'a> App<'a> {
    fn update_rate(&mut self) {
        let dt = self.rate_prev.0.elapsed().as_secs_f64();
        if dt >= 1.0 {
            self.rate = (self.samples_taken - self.rate_prev.1) as f64 / dt;
            self.rate_prev = (Instant::now(), self.samples_taken);
        }
    }

    fn new(
        devs: &'a [DevInfo],
        total_sectors: u64,
        reflink_map: ReflinkMap,
        fs_path: &str,
        seed: u64,
        cwd: Vec<String>,
    ) -> Self {
        App {
            per_dev: devs.iter().map(|_| PerDeviceCounters::default()).collect(),
            devs,
            total_sectors,
            tree: DirNode::default(),
            samples_taken: 0,
            rate_prev: (Instant::now(), 0),
            rate: 0.0,
            path_cache: HashMap::new(),
            pending: HashMap::new(),
            queued: HashSet::new(),
            reflink_map,
            cwd,
            cursor: 0,
            list_state: ListState::default(),
            error: None,
            error_count: 0,
            fs_path: fs_path.to_string(),
            seed,
        }
    }

    /// Sector estimate per raw sample count, given the samples taken so
    /// far. Recomputed every redraw, so every displayed total refines as
    /// `samples_taken` grows (see module notes).
    fn scale(&self) -> f64 {
        if self.samples_taken == 0 { 0.0 } else { self.total_sectors as f64 / self.samples_taken as f64 }
    }

    fn current_dir(&self) -> Option<&DirNode> {
        if self.cwd.is_empty() {
            Some(&self.tree)
        } else {
            self.tree.find(&self.cwd.join("/"))
        }
    }

    /// Denominator for "% of current dir": the current directory's sampled
    /// total. Pending (not-yet-resolved) weight is deliberately excluded —
    /// see module notes — and reported separately in the header instead.
    fn current_dir_total(&self) -> f64 {
        self.current_dir().map(|d| d.total).unwrap_or(0.0)
    }

    /// Current directory's children, sorted by count desc. Single source
    /// of truth for both rendering and cursor movement/descend, so they
    /// can't disagree about what row N is.
    fn visible_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        if let Some(dir) = self.current_dir() {
            for (name, child) in dir.sorted_children() {
                rows.push(Row { name: name.clone(), count: child.total, enterable: !child.children.is_empty() });
            }
        }
        rows.sort_by(|a, b| b.count.partial_cmp(&a.count).unwrap());
        rows
    }

    fn clamp_cursor(&mut self) {
        let n = self.visible_rows().len();
        self.cursor = if n == 0 { 0 } else { self.cursor.min(n - 1) };
        self.list_state.select(if n == 0 { None } else { Some(self.cursor) });
    }

    fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_down(&mut self) {
        let n = self.visible_rows().len();
        if n > 0 {
            self.cursor = (self.cursor + 1).min(n - 1);
        }
    }

    fn descend(&mut self) {
        if let Some(row) = self.visible_rows().into_iter().nth(self.cursor) {
            if row.enterable {
                self.cwd.push(row.name);
                self.cursor = 0;
            }
        }
    }

    fn ascend(&mut self) {
        if self.cwd.pop().is_some() {
            self.cursor = 0;
        }
    }

    fn record_error(&mut self, source: &str, e: anyhow::Error) {
        self.error_count += 1;
        if self.error.is_none() {
            self.error = Some(format!("{source}: {e:#}"));
        }
    }

    /// Attribute `weight` for `key` now if it's already resolved, otherwise
    /// accumulate it under `pending` and enqueue `key` for resolution
    /// (once per key, via `queued`).
    fn attribute_or_defer(
        &mut self,
        key: (u64, u32),
        weight: f64,
        not_found_label: &str,
        target_totals: &mut HashMap<String, f64>,
        work_tx: &Sender<(u64, u32)>,
    ) {
        if fs_du::try_attribute(key, weight, &self.path_cache, not_found_label, &mut self.tree, target_totals, self.devs) {
            return;
        }
        *self.pending.entry(key).or_insert(0.0) += weight;
        if self.queued.insert(key) {
            let _ = work_tx.send(key); // resolver pool gone (quitting): drop silently, UI is tearing down
        }
    }

    fn handle_sample_msg(&mut self, msg: SamplerMsg, work_tx: &Sender<(u64, u32)>) {
        match msg {
            SamplerMsg::Batch { accum, n_samples } => {
                self.samples_taken += n_samples;
                for (pd, thread_pd) in self.per_dev.iter_mut().zip(accum.per_dev) {
                    pd.merge(thread_pd);
                }
                self.tree.merge(accum.tree);

                // The TUI has no by-background_target panel; try_attribute
                // still wants somewhere to write target totals, so give it
                // a batch-scoped scratch map that's discarded once
                // attribution is done rather than an unread App field.
                let mut target_totals = HashMap::new();
                for (key, weight) in accum.extent_totals {
                    self.attribute_or_defer(key, weight, "<unresolved>", &mut target_totals, work_tx);
                }
                for sample in accum.reflink_samples {
                    let referencers = self.reflink_map.sample_referencers(&sample);
                    if referencers.is_empty() {
                        self.tree.insert_pseudo("<reflink unreferenced>", sample.weight);
                        continue;
                    }
                    let share = sample.weight / referencers.len() as f64;
                    for key in referencers {
                        self.attribute_or_defer(key, share, "<reflink unreferenced>", &mut target_totals, work_tx);
                    }
                }
            }
            SamplerMsg::Error(e) => self.record_error("sampler", e),
        }
    }

    fn handle_resolve_msg(&mut self, msg: ResolverMsg) {
        match msg {
            ResolverMsg::Resolved { key, result } => {
                self.path_cache.insert(key, result);
                self.queued.remove(&key);
                if let Some(weight) = self.pending.remove(&key) {
                    let mut target_totals = HashMap::new();
                    // A key can be deferred by both an extents sample and a
                    // reflink referencer before it resolves; once merged,
                    // "<unresolved>" is used uniformly here regardless of
                    // origin. Batch mode's finer-grained
                    // "<reflink unreferenced>" split only applies on the
                    // fast, immediate-resolution path in attribute_or_defer.
                    fs_du::try_attribute(key, weight, &self.path_cache, "<unresolved>", &mut self.tree, &mut target_totals, self.devs);
                }
            }
            ResolverMsg::Error(e) => self.record_error("resolver", e),
        }
    }
}

// ──────────────────────────── Rendering ────────────────────────────

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('\u{2026}');
        t
    }
}

/// Render a proportional bar using unicode eighth-block glyphs for a
/// smooth (non-blocky) fill.
fn bar(pct: f64, width: usize) -> String {
    const EIGHTHS: [char; 9] =
        [' ', '\u{258F}', '\u{258E}', '\u{258D}', '\u{258C}', '\u{258B}', '\u{258A}', '\u{2589}', '\u{2588}'];
    let eighths = (pct.clamp(0.0, 100.0) / 100.0 * width as f64 * 8.0).round() as usize;
    let full = (eighths / 8).min(width);
    let mut s: String = EIGHTHS[8].to_string().repeat(full);
    if full < width {
        s.push(EIGHTHS[eighths % 8]);
        s.push_str(&" ".repeat(width - full - 1));
    }
    s
}

fn render_header(f: &mut Frame, area: Rect, app: &App, pause: &PauseState) {
    let scale = app.scale();
    let free_est: f64 = app.per_dev.iter().map(|d| d.free).sum::<f64>() * scale;
    // used(est) already includes pending (not-yet-resolved) weight: per-device
    // accounting in classify_sample runs before path attribution is even
    // attempted, so it's independent of whether a sample's owning file has
    // been resolved yet.
    let used_est = app.total_sectors as f64 - free_est;
    let paused_tag = if pause.is_paused() { "  [PAUSED]" } else { "" };
    let pending_sectors: f64 = app.pending.values().sum::<f64>() * scale;

    let lines = vec![
        Line::from(format!("bcachefs fs du --tui  {}  (seed {}){}", app.fs_path, app.seed, paused_tag)),
        Line::from(format!(
            "samples: {}  {:.0}/s  total: {}  used(est): {}  free(est): {}",
            app.samples_taken,
            app.rate,
            fs_du::fmt_sectors(app.total_sectors as f64),
            fs_du::fmt_sectors(used_est),
            fs_du::fmt_sectors(free_est),
        )),
        Line::from(format!(
            "resolving: {} inodes / {} pending",
            app.pending.len(),
            fs_du::fmt_sectors(pending_sectors),
        )),
        Line::from(format!("/{}", app.cwd.join("/"))),
    ];

    f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::BOTTOM)), area);
}

fn render_list(f: &mut Frame, area: Rect, app: &mut App) {
    let rows = app.visible_rows();
    let dir_total = app.current_dir_total();
    let scale = app.scale();

    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let pct = if dir_total > 0.0 { row.count / dir_total * 100.0 } else { 0.0 };
            let sectors = row.count * scale;
            let line = format!(
                "{:<32} {:>10} {:>5.1}% {}",
                truncate(&row.name, 32),
                fs_du::fmt_sectors(sectors),
                pct,
                bar(pct, LIST_BAR_WIDTH),
            );
            ListItem::new(line)
        })
        .collect();

    let title = format!(" /{} ", app.cwd.join("/"));
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    f.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let error_line = match &app.error {
        Some(err) => {
            let extra = if app.error_count > 1 { format!("  (+{} more)", app.error_count - 1) } else { String::new() };
            Line::from(Span::styled(format!("error: {err}{extra}"), Style::default().add_modifier(Modifier::BOLD)))
        }
        None => Line::from(""),
    };
    let help =
        Line::from("q:quit  \u{2191}\u{2193}/j/k:move  enter/l:descend  backspace/h:ascend  p:pause/resume  r:refresh");

    f.render_widget(Paragraph::new(vec![error_line, help]).block(Block::default().borders(Borders::TOP)), area);
}

fn ui(f: &mut Frame, app: &mut App, pause: &PauseState) {
    let chunks = Layout::vertical([Constraint::Length(5), Constraint::Min(3), Constraint::Length(3)]).split(f.area());
    render_header(f, chunks[0], app, pause);
    render_list(f, chunks[1], app);
    render_footer(f, chunks[2], app);
}

// ──────────────────────────── Event loop ────────────────────────────

#[allow(clippy::too_many_arguments)]
fn run_ui(
    devs: &[DevInfo],
    total_sectors: u64,
    reflink_map: ReflinkMap,
    fs_path: &str,
    seed: u64,
    cwd: Vec<String>,
    sample_rx: Receiver<SamplerMsg>,
    work_tx: Sender<(u64, u32)>,
    resolve_rx: Receiver<ResolverMsg>,
    pause: &PauseState,
) -> Result<()> {
    let _guard = TerminalGuard::new()?;
    let mut terminal =
        Terminal::new(CrosstermBackend::new(io::stdout())).context("initializing ratatui terminal")?;
    terminal.clear().context("clearing terminal")?;

    let mut app = App::new(devs, total_sectors, reflink_map, fs_path, seed, cwd);

    loop {
        while let Ok(msg) = sample_rx.try_recv() {
            app.handle_sample_msg(msg, &work_tx);
        }
        while let Ok(msg) = resolve_rx.try_recv() {
            app.handle_resolve_msg(msg);
        }
        app.clamp_cursor();
        app.update_rate();

        terminal.draw(|f| ui(f, &mut app, pause)).context("drawing frame")?;

        if event::poll(TICK)? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Up | KeyCode::Char('k') => app.move_up(),
                    KeyCode::Down | KeyCode::Char('j') => app.move_down(),
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => app.descend(),
                    KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => app.ascend(),
                    KeyCode::Char('p') => pause.toggle(),
                    KeyCode::Char('r') => {} // redraw is already periodic; explicit no-op refresh key
                    _ => {}
                }
            }
            while event::poll(Duration::ZERO)? {
                let _ = event::read();
            }
        }
    }

    Ok(())
}

// ──────────────────────────── Entry point ────────────────────────────

pub(super) fn run(
    handle: &BcachefsHandle,
    fs_path: &str,
    seed: Option<u64>,
    path_prefix: Option<&str>,
    n_threads: usize,
) -> Result<()> {
    let fs_du::SamplingSetup { devs, geoms, total_sectors, dev_dist, seed } =
        fs_du::sampling_setup(handle, seed)?;

    println!("building reflink map...");
    let reflink_map = fs_du::build_reflink_map(handle)
        .context("building reflink attribution map (BCH_IOCTL_REFLINK_MAP)")?;

    let sample_handles = fs_du::open_handles(fs_path, n_threads, "sampling")?;
    let resolver_handles = fs_du::open_handles(fs_path, n_threads, "resolver")?;

    let stop = AtomicBool::new(false);
    let pause = PauseState::new();
    let ancestors = fs_du::new_ancestor_cache();
    let (sample_tx, sample_rx) = mpsc::channel::<SamplerMsg>();
    let (work_tx, work_rx) = mpsc::channel::<(u64, u32)>();
    let work_rx = Mutex::new(work_rx);
    let (resolve_tx, resolve_rx) = mpsc::channel::<ResolverMsg>();

    let cwd: Vec<String> = path_prefix
        .map(|p| p.split('/').filter(|c| !c.is_empty()).map(str::to_string).collect())
        .unwrap_or_default();

    std::thread::scope(|s| -> Result<()> {
        for (i, sh) in sample_handles.iter().enumerate() {
            let tx = sample_tx.clone();
            let thread_seed = seed.wrapping_add(i as u64);
            let devs = &devs;
            let geoms = &geoms;
            let dev_dist = &dev_dist;
            let stop = &stop;
            let pause = &pause;
            s.spawn(move || sampler_loop(sh, thread_seed, devs, geoms, dev_dist, stop, pause, &tx));
        }
        drop(sample_tx);

        for sh in &resolver_handles {
            let tx = resolve_tx.clone();
            let work_rx = &work_rx;
            let stop = &stop;
            let pause = &pause;
            let ancestors = &ancestors;
            s.spawn(move || resolver_loop(sh, work_rx, &tx, stop, pause, ancestors));
        }
        drop(resolve_tx);

        // work_tx is owned by run_ui and dropped when it returns, which is
        // what unblocks resolver_loop's blocking recv() below; stop then
        // unblocks sampler_loop. Both happen before this closure returns,
        // so thread::scope's implicit join sees every thread exit.
        let result = run_ui(
            &devs, total_sectors, reflink_map, fs_path, seed, cwd, sample_rx, work_tx, resolve_rx, &pause,
        );
        // work_tx is dropped by run_ui returning, above, before stop is set
        // here: a resolver parked on pause wakes via the stop check below,
        // then its recv() is guaranteed to see the Sender already gone.
        stop.store(true, Ordering::Relaxed);
        // Wake any parked sampler/resolver thread immediately on shutdown,
        // rather than leaving it blocked until a resume that will never
        // come.
        pause.cv.notify_all();
        result
    })
}
