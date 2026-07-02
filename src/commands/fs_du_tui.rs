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
//
// View toggle ('t' or 'v'): the main list cycles paths -> targets ->
// devices -> paths. Targets pivots on background_target (`target_trees`,
// one DirNode per resolved target label — group labels resolved from the
// `disk_groups` sysfs attribute, see `fs_du::read_group_labels`); devices
// pivots on the physical device a sample landed on (`device_trees`, one
// DirNode per device's label/name). Both are chosen over tagging every
// path-tree node with per-target/per-device weights because they reuse
// `try_attribute`'s existing resolution and `DirNode::insert_path`
// unchanged — each resolved key is just inserted again, into its
// target's/device's own tree, at attribution time. This is why extents
// samples are aggregated by (inode, snapshot, device) rather than just
// (inode, snapshot): a replicated file's weight has to stay split by the
// device each sample physically landed on, or the device view couldn't
// attribute it back to more than one device. Reflink samples carry their
// device the same way. Costs one extra DirNode insertion per resolved
// sample per extra view; acceptable next to the ioctl cost it's riding
// on. Each view keeps its own navigation stack (`path_cwd`/`target_cwd`/
// `device_cwd`) so switching views doesn't disturb the others' position.

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
/// Caps on how many messages each channel drain (see run_ui) processes
/// per event-loop iteration. Uncapped `while let Ok(msg) = rx.try_recv()`
/// only terminates once the channel is momentarily empty; with several
/// sampler/resolver threads producing faster than this single thread can
/// merge DirNode/target_trees updates, that condition can stop happening
/// at all — the drain never returns, so input handling and drawing never
/// run again (observed: 400% CPU, frozen UI). Capping each drain bounds
/// one iteration's work no matter the production rate; the mpsc channel
/// is the buffer, so anything past the cap is simply left queued for the
/// next iteration. A sampler batch (up to SAMPLE_FLUSH_BATCH samples) does
/// more work per message than a single resolved key, hence the lower cap.
const MAX_SAMPLE_BATCHES_PER_ITER: usize = 16;
const MAX_RESOLVE_MSGS_PER_ITER: usize = 512;
/// Cap on queued terminal input events drained per iteration (see run_ui)
/// — defensive bound against a pathological burst (e.g. bracketed paste)
/// spinning the drain the same way an unbounded channel drain would.
const MAX_INPUT_EVENTS_PER_ITER: usize = 256;
/// How long a channel drain must keep hitting its cap, back-to-back and
/// without a gap, before the header surfaces it as backlog. Wall-clock
/// rather than an iteration count: once backlog stops blocking the idle
/// wait (see `backlog` in run_ui), iterations while catching up run back
/// to back with no fixed period, so a duration is what actually
/// distinguishes a routine burst from sustained overload.
const BACKLOG_WARN_DURATION: Duration = Duration::from_secs(1);

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

/// Which pivot the main list is currently showing. See module notes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Paths,
    Targets,
    Devices,
}

struct App<'a> {
    devs: &'a [DevInfo],
    total_sectors: u64,
    per_dev: Vec<PerDeviceCounters>,
    tree: DirNode,
    /// Per-background_target pivot of the same attributed samples, keyed
    /// on the resolved target label (see module notes). Populated
    /// alongside `tree` by `try_attribute`.
    target_trees: HashMap<String, DirNode>,
    /// Per-device pivot of the same attributed samples, keyed on the
    /// device's row label (`fs_du::device_row_label`). Populated
    /// alongside `tree` by `try_attribute`.
    device_trees: HashMap<String, DirNode>,
    /// group id -> disk-group label, resolved once at startup (see
    /// `fs_du::read_group_labels`) and passed to every `try_attribute`
    /// call for target-label resolution.
    group_labels: HashMap<u32, String>,
    samples_taken: u64,
    /// Windowed sample rate: last measurement (time, samples_taken) and the
    /// rate over that window - a lifetime average would decay asymptotically
    /// instead of dropping to 0 when sampling is paused.
    rate_prev: (Instant, u64),
    rate: f64,
    path_cache: PathCache,
    /// Raw-count weight accumulated for (inode, snapshot) keys seen but
    /// not yet resolved, folded into the synthetic root-level
    /// `<resolving>` row until the resolver pool answers. Weight is kept
    /// broken down per originating device (inner map) so that once
    /// resolved, each device's share can still be attributed to its own
    /// `device_trees` entry rather than only the summed total.
    pending: HashMap<(u64, u32), HashMap<usize, f64>>,
    /// Keys already sent to the resolver work queue, so a key seen again
    /// before it resolves isn't re-enqueued.
    queued: HashSet<(u64, u32)>,
    reflink_map: ReflinkMap,
    view: ViewMode,
    path_cwd: Vec<String>,
    target_cwd: Vec<String>,
    device_cwd: Vec<String>,
    cursor: usize,
    list_state: ListState,
    error: Option<String>,
    error_count: u32,
    /// When the current unbroken run of cap-hitting channel drains began
    /// (see MAX_SAMPLE_BATCHES_PER_ITER / MAX_RESOLVE_MSGS_PER_ITER),
    /// `None` if the last drain came back under cap. Sustained backlog
    /// means production is outpacing this thread's consumption —
    /// surfaced in the header once it's lasted BACKLOG_WARN_DURATION
    /// rather than silently falling further behind.
    backlog_since: Option<Instant>,
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

    #[allow(clippy::too_many_arguments)]
    fn new(
        devs: &'a [DevInfo],
        total_sectors: u64,
        reflink_map: ReflinkMap,
        group_labels: HashMap<u32, String>,
        fs_path: &str,
        seed: u64,
        path_cwd: Vec<String>,
    ) -> Self {
        App {
            per_dev: devs.iter().map(|_| PerDeviceCounters::default()).collect(),
            devs,
            total_sectors,
            tree: DirNode::default(),
            target_trees: HashMap::new(),
            device_trees: HashMap::new(),
            group_labels,
            samples_taken: 0,
            rate_prev: (Instant::now(), 0),
            rate: 0.0,
            path_cache: HashMap::new(),
            pending: HashMap::new(),
            queued: HashSet::new(),
            reflink_map,
            view: ViewMode::Paths,
            path_cwd,
            target_cwd: Vec::new(),
            device_cwd: Vec::new(),
            cursor: 0,
            list_state: ListState::default(),
            error: None,
            error_count: 0,
            backlog_since: None,
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

    /// Navigation stack for the active view. Each view keeps its own (see
    /// module notes), so toggling views doesn't disturb the other's
    /// position.
    fn cwd(&self) -> &[String] {
        match self.view {
            ViewMode::Paths => &self.path_cwd,
            ViewMode::Targets => &self.target_cwd,
            ViewMode::Devices => &self.device_cwd,
        }
    }

    fn cwd_mut(&mut self) -> &mut Vec<String> {
        match self.view {
            ViewMode::Paths => &mut self.path_cwd,
            ViewMode::Targets => &mut self.target_cwd,
            ViewMode::Devices => &mut self.device_cwd,
        }
    }

    fn toggle_view(&mut self) {
        self.view = match self.view {
            ViewMode::Paths => ViewMode::Targets,
            ViewMode::Targets => ViewMode::Devices,
            ViewMode::Devices => ViewMode::Paths,
        };
        self.cursor = 0;
    }

    /// Current directory's `DirNode`, or `None` at the synthetic root of
    /// the targets/devices views (a virtual directory of target/device
    /// labels, not a real `DirNode` — see `visible_rows`).
    fn current_dir(&self) -> Option<&DirNode> {
        match self.view {
            ViewMode::Paths => {
                if self.path_cwd.is_empty() { Some(&self.tree) } else { self.tree.find(&self.path_cwd.join("/")) }
            }
            ViewMode::Targets => {
                let root = self.target_trees.get(self.target_cwd.first()?)?;
                if self.target_cwd.len() == 1 { Some(root) } else { root.find(&self.target_cwd[1..].join("/")) }
            }
            ViewMode::Devices => {
                let root = self.device_trees.get(self.device_cwd.first()?)?;
                if self.device_cwd.len() == 1 { Some(root) } else { root.find(&self.device_cwd[1..].join("/")) }
            }
        }
    }

    /// Denominator for "% of current dir": the current directory's sampled
    /// total. Pending (not-yet-resolved) weight is deliberately excluded —
    /// see module notes — and reported separately in the header instead.
    fn current_dir_total(&self) -> f64 {
        match self.view {
            ViewMode::Targets if self.target_cwd.is_empty() => {
                self.target_trees.values().map(|d| d.total).sum()
            }
            ViewMode::Devices if self.device_cwd.is_empty() => {
                self.device_trees.values().map(|d| d.total).sum()
            }
            _ => self.current_dir().map(|d| d.total).unwrap_or(0.0),
        }
    }

    /// Current directory's children, sorted by count desc. Single source
    /// of truth for both rendering and cursor movement/descend, so they
    /// can't disagree about what row N is. At the targets/devices views'
    /// root, rows are the target/device labels themselves rather than a
    /// `DirNode`'s children.
    fn visible_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        if self.view == ViewMode::Targets && self.target_cwd.is_empty() {
            for (label, node) in &self.target_trees {
                rows.push(Row { name: label.clone(), count: node.total, enterable: !node.children.is_empty() });
            }
        } else if self.view == ViewMode::Devices && self.device_cwd.is_empty() {
            for (label, node) in &self.device_trees {
                rows.push(Row { name: label.clone(), count: node.total, enterable: !node.children.is_empty() });
            }
        } else if let Some(dir) = self.current_dir() {
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
                self.cwd_mut().push(row.name);
                self.cursor = 0;
            }
        }
    }

    fn ascend(&mut self) {
        if self.cwd_mut().pop().is_some() {
            self.cursor = 0;
        }
    }

    fn record_error(&mut self, source: &str, e: anyhow::Error) {
        self.error_count += 1;
        if self.error.is_none() {
            self.error = Some(format!("{source}: {e:#}"));
        }
    }

    /// Attribute `weight` for `key` (sampled on device `dev_pos`) now if
    /// it's already resolved, otherwise accumulate it under `pending`
    /// (keyed by device, so a later resolve can still split it back out
    /// per device) and enqueue `key` for resolution (once per key, via
    /// `queued`).
    fn attribute_or_defer(
        &mut self,
        key: (u64, u32),
        weight: f64,
        dev_pos: usize,
        not_found_label: &str,
        target_totals: &mut HashMap<String, f64>,
        work_tx: &Sender<(u64, u32)>,
    ) {
        let dev_label = fs_du::device_row_label(self.devs, dev_pos);
        if fs_du::try_attribute(
            key,
            weight,
            &self.path_cache,
            not_found_label,
            &mut self.tree,
            target_totals,
            self.devs,
            &self.group_labels,
            Some(&mut self.target_trees),
            Some((&dev_label, &mut self.device_trees)),
        ) {
            return;
        }
        *self.pending.entry(key).or_default().entry(dev_pos).or_insert(0.0) += weight;
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

                // try_attribute's flat target_totals sum isn't read here —
                // target_trees (populated as a side effect below) already
                // carries the same per-target totals plus the full subtree
                // the target view descends into — but it still wants
                // somewhere to write, so give it a batch-scoped scratch map.
                let mut target_totals = HashMap::new();
                for (&(inum, snapshot, dev_pos), &weight) in &accum.extent_totals {
                    self.attribute_or_defer((inum, snapshot), weight, dev_pos, "<unresolved>", &mut target_totals, work_tx);
                }
                for sample in accum.reflink_samples {
                    let referencers = self.reflink_map.sample_referencers(&sample);
                    if referencers.is_empty() {
                        self.tree.insert_pseudo("<reflink unreferenced>", sample.weight);
                        let dev_label = fs_du::device_row_label(self.devs, sample.dev_pos);
                        self.device_trees.entry(dev_label).or_default().insert_pseudo("<reflink unreferenced>", sample.weight);
                        continue;
                    }
                    let share = sample.weight / referencers.len() as f64;
                    for key in referencers {
                        self.attribute_or_defer(key, share, sample.dev_pos, "<reflink unreferenced>", &mut target_totals, work_tx);
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
                if let Some(dev_weights) = self.pending.remove(&key) {
                    let mut target_totals = HashMap::new();
                    // A key can be deferred by both an extents sample and a
                    // reflink referencer before it resolves; once merged,
                    // "<unresolved>" is used uniformly here regardless of
                    // origin. Batch mode's finer-grained
                    // "<reflink unreferenced>" split only applies on the
                    // fast, immediate-resolution path in attribute_or_defer.
                    for (dev_pos, weight) in dev_weights {
                        let dev_label = fs_du::device_row_label(self.devs, dev_pos);
                        fs_du::try_attribute(
                            key,
                            weight,
                            &self.path_cache,
                            "<unresolved>",
                            &mut self.tree,
                            &mut target_totals,
                            self.devs,
                            &self.group_labels,
                            Some(&mut self.target_trees),
                            Some((&dev_label, &mut self.device_trees)),
                        );
                    }
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

/// Current path as shown in the header/list title: `/cwd` in the paths
/// view, and in the targets/devices views either `/cwd` under the
/// selected target/device or the literal string `targets`/`devices` at
/// its synthetic root (no `DirNode` there to derive a path from — see
/// `App::current_dir`).
fn display_path(app: &App) -> String {
    match app.view {
        ViewMode::Targets if app.target_cwd.is_empty() => "targets".to_string(),
        ViewMode::Devices if app.device_cwd.is_empty() => "devices".to_string(),
        _ => format!("/{}", app.cwd().join("/")),
    }
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
    // Sustained (not bursty — see BACKLOG_WARN_DURATION) channel backlog:
    // this thread can't keep up with the sampler/resolver threads, so
    // display is lagging behind the true sample rate. Surfaced rather
    // than hidden behind an ever-growing drain — see
    // MAX_SAMPLE_BATCHES_PER_ITER.
    let backlog_tag = match app.backlog_since {
        Some(since) if since.elapsed() >= BACKLOG_WARN_DURATION => "  [UI FALLING BEHIND SAMPLING]",
        _ => "",
    };
    let pending_sectors: f64 = app.pending.values().flat_map(|m| m.values()).sum::<f64>() * scale;
    let view_tag = match app.view {
        ViewMode::Paths => "paths",
        ViewMode::Targets => "targets",
        ViewMode::Devices => "devices",
    };

    let lines = vec![
        Line::from(format!(
            "bcachefs fs du --tui  {}  (seed {})  view: {}{}{}",
            app.fs_path, app.seed, view_tag, paused_tag, backlog_tag
        )),
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
        Line::from(display_path(app)),
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

    let title = format!(" {} ", display_path(app));
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
    let help = Line::from(
        "q:quit  \u{2191}\u{2193}/j/k:move  enter/l:descend  backspace/h:ascend  t/v:toggle view  p:pause/resume",
    );

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
    group_labels: HashMap<u32, String>,
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

    let mut app = App::new(devs, total_sectors, reflink_map, group_labels, fs_path, seed, cwd);
    // Set once a channel drain hits its cap (more still queued) and
    // cleared once a drain comes back under cap. Gates the idle wait
    // below: blocking a full TICK while backlog is waiting would throttle
    // consumption to `cap * (1000ms/TICK)` regardless of how fast the
    // sampler/resolver threads actually produce, manufacturing exactly
    // the "falling behind" condition the cap was meant to only report.
    let mut backlog = false;

    'main: loop {
        // 1. Input first, drained to empty (capped — see
        // MAX_INPUT_EVENTS_PER_ITER): a held/repeated key must never be
        // discarded (that was the bug — see module notes) and must never
        // wait behind a channel drain + DirNode merges + draw.
        let mut had_event = false;
        for _ in 0..MAX_INPUT_EVENTS_PER_ITER {
            if !event::poll(Duration::ZERO)? {
                break;
            }
            had_event = true;
            if let Event::Key(key) = event::read()? {
                if dispatch_key(&mut app, pause, key) {
                    break 'main;
                }
            }
        }
        if !had_event {
            // Nothing queued: if the previous iteration left channel
            // backlog, loop straight back into the drain below instead of
            // blocking; otherwise block up to one tick, both to pace the
            // ~100ms redraw and to react to a key the instant it arrives
            // rather than only at the next iteration.
            let wait = if backlog { Duration::ZERO } else { TICK };
            if event::poll(wait)? {
                if let Event::Key(key) = event::read()? {
                    if dispatch_key(&mut app, pause, key) {
                        break 'main;
                    }
                }
            }
        }

        // 2. Sampler/resolver channels, each capped per iteration (see
        // module notes) so this drain can never starve step 1/3 no matter
        // how fast the worker threads produce.
        let mut sample_drained = 0;
        for _ in 0..MAX_SAMPLE_BATCHES_PER_ITER {
            match sample_rx.try_recv() {
                Ok(msg) => {
                    app.handle_sample_msg(msg, &work_tx);
                    sample_drained += 1;
                }
                Err(_) => break,
            }
        }
        let mut resolve_drained = 0;
        for _ in 0..MAX_RESOLVE_MSGS_PER_ITER {
            match resolve_rx.try_recv() {
                Ok(msg) => {
                    app.handle_resolve_msg(msg);
                    resolve_drained += 1;
                }
                Err(_) => break,
            }
        }
        backlog = sample_drained == MAX_SAMPLE_BATCHES_PER_ITER || resolve_drained == MAX_RESOLVE_MSGS_PER_ITER;
        app.backlog_since = if backlog { Some(app.backlog_since.unwrap_or_else(Instant::now)) } else { None };
        app.clamp_cursor();
        app.update_rate();

        // 3. Draw once.
        terminal.draw(|f| ui(f, &mut app, pause)).context("drawing frame")?;
    }

    Ok(())
}

/// Dispatch one key event. Returns `true` if the UI should quit.
fn dispatch_key(app: &mut App, pause: &PauseState, key: crossterm::event::KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') => return true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Up | KeyCode::Char('k') => app.move_up(),
        KeyCode::Down | KeyCode::Char('j') => app.move_down(),
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => app.descend(),
        KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => app.ascend(),
        KeyCode::Char('t') | KeyCode::Char('v') => app.toggle_view(),
        KeyCode::Char('p') => pause.toggle(),
        _ => {}
    }
    false
}

// ──────────────────────────── Entry point ────────────────────────────

pub(super) fn run(
    handle: &BcachefsHandle,
    fs_path: &str,
    seed: Option<u64>,
    path_prefix: Option<&str>,
    n_threads: usize,
) -> Result<()> {
    let fs_du::SamplingSetup { sysfs_path, devs, geoms, total_sectors, dev_dist, seed } =
        fs_du::sampling_setup(handle, seed)?;

    println!("building reflink map...");
    let reflink_map = fs_du::build_reflink_map(handle)
        .context("building reflink attribution map (BCH_IOCTL_REFLINK_MAP)")?;
    let group_labels = fs_du::read_group_labels(&sysfs_path);

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
            &devs, total_sectors, reflink_map, group_labels, fs_path, seed, cwd, sample_rx, work_tx, resolve_rx,
            &pause,
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
