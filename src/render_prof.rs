//! Lightweight opt-in render profiling for local performance investigations.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const ENV_VAR: &str = "HERDR_RENDER_PROF";
const SCROLL_TRACE_ENV_VAR: &str = "HERDR_SCROLL_TRACE";
const OUTPUT_FILE_ENV_VAR: &str = "HERDR_SCROLL_TRACE_FILE";
const OUTPUT_FILE_ALIAS_ENV_VAR: &str = "HERDR_SCROLL_TRACE_OUT";
const WINDOW_MS_ENV_VAR: &str = "HERDR_SCROLL_TRACE_WINDOW_MS";
const DEFAULT_WINDOW: Duration = Duration::from_secs(1);

static ENABLED: OnceLock<bool> = OnceLock::new();
static CONFIG: OnceLock<RenderProfilerConfig> = OnceLock::new();
static PROFILER: OnceLock<Mutex<RenderProfiler>> = OnceLock::new();
static TEST_ENABLED: AtomicBool = AtomicBool::new(false);
static TRACE_SEQ: AtomicU64 = AtomicU64::new(1);
#[cfg(test)]
static TEST_GUARD: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Default)]
struct DurationStats {
    count: u64,
    total_ns: u128,
    max_ns: u128,
    samples_ns: Vec<u128>,
}

#[derive(Clone)]
struct RenderProfilerConfig {
    output_file: Option<PathBuf>,
    window: Duration,
}

struct RenderProfiler {
    window_started: Instant,
    counters: BTreeMap<&'static str, u64>,
    durations: BTreeMap<&'static str, DurationStats>,
}

impl RenderProfiler {
    fn new() -> Self {
        Self {
            window_started: Instant::now(),
            counters: BTreeMap::new(),
            durations: BTreeMap::new(),
        }
    }

    fn increment(&mut self, name: &'static str, value: u64) {
        *self.counters.entry(name).or_default() += value;
    }

    fn duration(&mut self, name: &'static str, duration: Duration) {
        let stats = self.durations.entry(name).or_default();
        let ns = duration.as_nanos();
        stats.count += 1;
        stats.total_ns += ns;
        stats.max_ns = stats.max_ns.max(ns);
        stats.samples_ns.push(ns);
    }

    fn summary(&self) -> String {
        let mut durations = self.durations.iter().collect::<Vec<_>>();
        durations.sort_by_key(|(_, stats)| std::cmp::Reverse(stats.total_ns));

        let mut lines = Vec::new();
        lines.push(format!(
            "scroll/render trace window_ms={}",
            self.window_started.elapsed().as_millis()
        ));
        if !self.counters.is_empty() {
            lines.push("counters:".to_owned());
            for (name, value) in &self.counters {
                lines.push(format!("  {name}: {value}"));
            }
        }
        if !durations.is_empty() {
            lines.push("durations ranked by total cost:".to_owned());
            for (rank, (name, stats)) in durations.into_iter().enumerate() {
                let avg_us = if stats.count == 0 {
                    0
                } else {
                    stats.total_ns / u128::from(stats.count) / 1_000
                };
                let p50_us = percentile_us(&stats.samples_ns, 50);
                let p95_us = percentile_us(&stats.samples_ns, 95);
                let p99_us = percentile_us(&stats.samples_ns, 99);
                let max_us = stats.max_ns / 1_000;
                lines.push(format!(
                    "  {}. {name}: count={} avg_us={avg_us} p50_us={p50_us} p95_us={p95_us} p99_us={p99_us} max_us={max_us}",
                    rank + 1,
                    stats.count
                ));
            }
        }
        lines.join("\n")
    }

    fn flush_if_due(&mut self) {
        if self.window_started.elapsed() < config().window {
            return;
        }
        self.flush_now();
    }

    fn flush_now(&mut self) {
        if self.counters.is_empty() && self.durations.is_empty() {
            self.window_started = Instant::now();
            return;
        }

        let summary = self.summary();
        if let Err(err) = write_summary(&summary) {
            tracing::warn!(err = %err, "failed to write scroll/render profiler summary");
        }

        self.window_started = Instant::now();
        self.counters.clear();
        self.durations.clear();
    }
}

fn percentile_us(samples_ns: &[u128], percentile: usize) -> u128 {
    if samples_ns.is_empty() {
        return 0;
    }
    let mut samples = samples_ns.to_vec();
    samples.sort_unstable();
    let rank = ((samples.len().saturating_sub(1)) * percentile).div_ceil(100);
    samples[rank] / 1_000
}

pub(crate) fn enabled() -> bool {
    if TEST_ENABLED.load(Ordering::Relaxed) {
        return true;
    }
    *ENABLED.get_or_init(|| env_truthy(ENV_VAR) || env_truthy(SCROLL_TRACE_ENV_VAR))
}

fn config() -> &'static RenderProfilerConfig {
    CONFIG.get_or_init(|| RenderProfilerConfig {
        output_file: std::env::var_os(OUTPUT_FILE_ENV_VAR)
            .or_else(|| std::env::var_os(OUTPUT_FILE_ALIAS_ENV_VAR))
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
        window: std::env::var(WINDOW_MS_ENV_VAR)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_WINDOW),
    })
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn write_summary(summary: &str) -> std::io::Result<()> {
    if let Some(path) = &config().output_file {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{summary}\n")?;
        return Ok(());
    }

    eprintln!("{summary}\n");
    Ok(())
}

fn json_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => escaped.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => escaped.push(ch),
        }
    }
    escaped.push('"');
    escaped
}

fn write_trace_line(line: &str) -> std::io::Result<()> {
    if let Some(path) = &config().output_file {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{line}")?;
        return Ok(());
    }

    eprintln!("{line}");
    Ok(())
}

pub(crate) fn next_trace_seq() -> u64 {
    TRACE_SEQ.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn trace_event(name: &'static str, fields: &[(&'static str, String)]) {
    if !enabled() {
        return;
    }
    let mut line = format!(
        "{{\"event\":{},\"trace_seq\":{}",
        json_string(name),
        next_trace_seq()
    );
    for (key, value) in fields {
        line.push(',');
        line.push_str(&json_string(key));
        line.push(':');
        line.push_str(value);
    }
    line.push('}');
    if let Err(err) = write_trace_line(&line) {
        tracing::warn!(err = %err, event = name, "failed to write scroll trace event");
    }
}

pub(crate) fn trace_string(value: impl std::fmt::Display) -> String {
    json_string(&value.to_string())
}

pub(crate) fn trace_opt<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_owned())
}

fn with_profiler(update: impl FnOnce(&mut RenderProfiler)) {
    if !enabled() {
        return;
    }
    let profiler = PROFILER.get_or_init(|| Mutex::new(RenderProfiler::new()));
    if let Ok(mut profiler) = profiler.lock() {
        update(&mut profiler);
    }
}

pub(crate) fn counter(name: &'static str, value: u64) {
    if value == 0 {
        return;
    }
    with_profiler(|profiler| profiler.increment(name, value));
}

pub(crate) fn event(name: &'static str) {
    counter(name, 1);
}

pub(crate) fn duration(name: &'static str, duration: Duration) {
    with_profiler(|profiler| profiler.duration(name, duration));
}

pub(crate) fn timer() -> Option<Instant> {
    enabled().then(Instant::now)
}

pub(crate) fn duration_since(name: &'static str, started: Option<Instant>) {
    if let Some(started) = started {
        duration(name, started.elapsed());
    }
}

pub(crate) fn flush_if_due() {
    with_profiler(RenderProfiler::flush_if_due);
}

pub(crate) fn flush_now() {
    with_profiler(RenderProfiler::flush_now);
}

#[cfg(test)]
pub(crate) struct TestRenderProfilerGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
    previous_enabled: bool,
}

#[cfg(test)]
impl Drop for TestRenderProfilerGuard {
    fn drop(&mut self) {
        TEST_ENABLED.store(self.previous_enabled, Ordering::Relaxed);
        reset_profiler_for_tests();
    }
}

#[cfg(test)]
pub(crate) fn enable_for_test() -> TestRenderProfilerGuard {
    let guard = TEST_GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("render profiler test guard poisoned");
    let previous_enabled = TEST_ENABLED.swap(true, Ordering::Relaxed);
    reset_profiler_for_tests();
    TestRenderProfilerGuard {
        _guard: guard,
        previous_enabled,
    }
}

#[cfg(test)]
pub(crate) fn reset_profiler_for_tests() {
    let profiler = PROFILER.get_or_init(|| Mutex::new(RenderProfiler::new()));
    if let Ok(mut profiler) = profiler.lock() {
        *profiler = RenderProfiler::new();
    }
}

#[cfg(test)]
fn is_actionable_leaf_duration(name: &str) -> bool {
    !matches!(
        name,
        "client.attach_input.total"
            | "server.attach_scroll.apply_total"
            | "server.attach_scroll.handle_total"
            | "server.attach_scroll.coalesce_drain"
            | "full_render.total"
            | "full_render.render_virtual"
            | "full_render.render_terminal_virtual"
            | "full_render.ratatui_draw_total"
            | "full_render.ui.panes"
            | "full_render.panes.terminal_render"
            | "retained.total"
            | "ansi_encode.total"
            | "dirty_collect.total"
    )
}

#[cfg(test)]
pub(crate) fn top_duration_name_for_tests() -> Option<&'static str> {
    let profiler = PROFILER.get_or_init(|| Mutex::new(RenderProfiler::new()));
    profiler.lock().ok().and_then(|profiler| {
        profiler
            .durations
            .iter()
            .max_by_key(|(_, stats)| stats.total_ns)
            .map(|(name, _)| *name)
    })
}

#[cfg(test)]
pub(crate) fn top_actionable_leaf_duration_name_for_tests() -> Option<&'static str> {
    let profiler = PROFILER.get_or_init(|| Mutex::new(RenderProfiler::new()));
    profiler.lock().ok().and_then(|profiler| {
        profiler
            .durations
            .iter()
            .filter(|(name, _)| is_actionable_leaf_duration(name))
            .max_by_key(|(_, stats)| stats.total_ns)
            .map(|(name, _)| *name)
    })
}

#[cfg(test)]
pub(crate) fn summary_for_tests() -> String {
    let profiler = PROFILER.get_or_init(|| Mutex::new(RenderProfiler::new()));
    profiler
        .lock()
        .map(|profiler| profiler.summary())
        .unwrap_or_else(|_| "scroll/render trace unavailable: profiler lock poisoned".to_owned())
}
