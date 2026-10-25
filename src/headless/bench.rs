//! `bench <frames> [csv=<path>] [action]`: forced frames with per-frame timings,
//! a CSV of those frames and a JSON summary of the run.

use super::protocol::{BenchAction, Response};
use super::session::{FrameBudget, HeadlessSession, HzSource};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Header of the per-frame CSV, in column order: RRiter's columns without `root_*`/`chrome_*`.
/// `gpu_ms` is empty when the context has no GPU timer queries.
const CSV_HEADER: &str = "frame,update_ms,draw_cpu_ms,gpu_ms,total_ms,flush_calls,vertices,scroll_y";

/// Number of slowest frames listed in the summary.
const WORST_FRAMES: usize = 5;

/// Timings and render counters of one forced frame.
struct FrameRow {
    frame: u32,
    update_ms: f64,
    draw_cpu_ms: f64,
    /// `None` when the context has no GPU timer queries.
    gpu_ms: Option<f64>,
    total_ms: f64,
    flush_calls: u32,
    vertices: u32,
    scroll_y: f64,
}

/// `bench <frames> [csv=<path>] [action]` → `ok <json summary>`.
pub(crate) fn bench(session: &mut HeadlessSession, frames: u32, csv: Option<PathBuf>, action: BenchAction) -> Response {
    match run(session, frames, csv, &action) {
        Ok(summary) => Response::Ok(Some(summary)),
        Err(reason) => Response::Err(reason),
    }
}

fn run(session: &mut HeadlessSession, frames: u32, csv: Option<PathBuf>, action: &BenchAction) -> Result<String, String> {
    let budget = session.budget();
    let path = match csv {
        Some(path) => path,
        None => default_csv_path(),
    };
    let path = std::path::absolute(&path).map_err(|error| format!("csv: {error}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("csv: {error}"))?;
    }
    let file = fs::File::create(&path).map_err(|error| format!("csv: {error}"))?;
    let mut csv_out = BufWriter::new(file);
    writeln!(csv_out, "{CSV_HEADER}").map_err(|error| format!("csv: {error}"))?;

    let system_before = system_snapshot();
    let mut rows = Vec::with_capacity(frames as usize);
    for frame in 0..frames {
        let start = Instant::now();
        let gpu_query = session
            .app_mut()
            .runtime()
            .and_then(|runtime| runtime.renderer().begin_gpu_timer());
        let stepped = session.bench_frame(action);
        // Ends the query even when the step failed, so no timer query is left open.
        let gpu_ms = gpu_query.and_then(|query| {
            session
                .app_mut()
                .runtime()
                .map(|runtime| runtime.renderer().end_gpu_timer(query))
        });
        let (update, draw) = stepped?;
        let total_ms = ms(start.elapsed());
        let app = session.app_mut();
        let (flush_calls, vertices) = app
            .runtime()
            .map_or((0, 0), |runtime| runtime.renderer().frame_stats());
        // The animation's own offset, not the frame's pixel-rounded `layout.scroll_offset`.
        let scroll_y = app
            .active_terminal()
            .map_or(0.0, |terminal| f64::from(terminal.scroll_y.current));
        let row = FrameRow {
            frame,
            update_ms: ms(update),
            draw_cpu_ms: ms(draw),
            gpu_ms,
            total_ms,
            flush_calls,
            vertices,
            scroll_y,
        };
        writeln!(
            csv_out,
            "{},{:.3},{:.3},{},{:.3},{},{},{}",
            row.frame,
            row.update_ms,
            row.draw_cpu_ms,
            row.gpu_ms.map_or_else(String::new, |gpu_ms| format!("{gpu_ms:.3}")),
            row.total_ms,
            row.flush_calls,
            row.vertices,
            row.scroll_y
        )
        .map_err(|error| format!("csv: {error}"))?;
        rows.push(row);
    }
    csv_out.flush().map_err(|error| format!("csv: {error}"))?;
    let system_after = system_snapshot();
    let process_cpu = system_after.process_cpu_ms - system_before.process_cpu_ms;

    let summary = summary_json(&rows, budget, &system_before, &system_after, process_cpu, &path);
    Ok(summary.to_string())
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// `${XDG_RUNTIME_DIR:-/tmp}/ronsole-headless/bench-<unix_ms>.csv`.
fn default_csv_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    base.join("ronsole-headless").join(format!("bench-{unix_ms}.csv"))
}

/// Process and machine state read before or after the run.
struct SystemSnapshot {
    loadavg_1m: Option<f64>,
    gpu_util: Option<f64>,
    process_cpu_ms: f64,
}

fn system_snapshot() -> SystemSnapshot {
    SystemSnapshot {
        loadavg_1m: loadavg_1m(),
        gpu_util: gpu_util(),
        process_cpu_ms: process_cpu_ms(),
    }
}

/// The one-minute load average from `/proc/loadavg`.
fn loadavg_1m() -> Option<f64> {
    let text = fs::read_to_string("/proc/loadavg").ok()?;
    text.split_whitespace().next()?.parse().ok()
}

/// GPU utilization in percent from `nvidia-smi`; `None` when the tool is missing or fails.
fn gpu_util() -> Option<f64> {
    let output = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=utilization.gpu", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    text.lines().next()?.trim().parse().ok()
}

/// User plus system CPU time of this process. `/proc/self/stat` counts in clock ticks, assumed
/// to be 100 per second (`USER_HZ` on Linux), so one tick is 10 ms.
fn process_cpu_ms() -> f64 {
    let Ok(text) = fs::read_to_string("/proc/self/stat") else {
        return 0.0;
    };
    // Fields after the parenthesized command name start at field 3 (`state`); utime and stime are fields 14 and 15.
    let Some(after_comm) = text.rsplit_once(')').map(|(_, rest)| rest) else {
        return 0.0;
    };
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let ticks = |index: usize| -> f64 {
        fields.get(index).and_then(|field| field.parse::<f64>().ok()).unwrap_or(0.0)
    };
    (ticks(11) + ticks(12)) * 10.0
}

/// Nearest-rank percentile of `sorted`, which must be non-empty and ascending.
fn percentile(sorted: &[f64], percent: usize) -> f64 {
    let rank = (sorted.len() * percent).div_ceil(100).clamp(1, sorted.len());
    sorted[rank - 1]
}

/// `p50`/`p95`/`p99`/`max` of `values`, or `null` when there are no values.
fn percentiles(mut values: Vec<f64>) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    values.sort_by(f64::total_cmp);
    json!({
        "p50": percentile(&values, 50),
        "p95": percentile(&values, 95),
        "p99": percentile(&values, 99),
        "max": values[values.len() - 1],
    })
}

fn summary_json(
    rows: &[FrameRow],
    budget: FrameBudget,
    before: &SystemSnapshot,
    after: &SystemSnapshot,
    process_cpu_ms: f64,
    csv: &std::path::Path,
) -> Value {
    let mut worst: Vec<&FrameRow> = rows.iter().collect();
    worst.sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms));
    let worst: Vec<Value> = worst
        .into_iter()
        .take(WORST_FRAMES)
        .map(|row| {
            json!({
                "frame": row.frame,
                "total_ms": row.total_ms,
                "update_ms": row.update_ms,
                "draw_cpu_ms": row.draw_cpu_ms,
            })
        })
        .collect();
    let over_budget = rows.iter().filter(|row| row.total_ms > budget.budget_ms).count();
    json!({
        "frames": rows.len(),
        "budget_ms": budget.budget_ms,
        "hz": budget.hz,
        "hz_source": match budget.source {
            HzSource::Arg => "arg",
            HzSource::Default => "default",
        },
        "total_ms": percentiles(rows.iter().map(|row| row.total_ms).collect()),
        "gpu_ms": percentiles(rows.iter().filter_map(|row| row.gpu_ms).collect()),
        "draw_cpu_ms": percentiles(rows.iter().map(|row| row.draw_cpu_ms).collect()),
        "over_budget": over_budget,
        "worst": worst,
        "system": {
            "loadavg_before": before.loadavg_1m,
            "loadavg_after": after.loadavg_1m,
            "cpus": std::thread::available_parallelism().map_or(Value::Null, |count| json!(count.get())),
            "gpu_util_before": before.gpu_util,
            "gpu_util_after": after.gpu_util,
            "process_cpu_ms": process_cpu_ms,
        },
        "csv": csv.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::CSV_HEADER;
    use super::super::session::HeadlessSession;
    use super::super::tests::{dump, run_script, session_for_test, wait_until};
    use serde_json::Value;
    use std::fs;
    use std::path::PathBuf;

    /// Column of `scroll_y` in a CSV row.
    const SCROLL_Y: usize = 7;

    /// Fresh scratch directory whose name is unique to this test process and tag.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ronsole-t5-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    /// Runs one `bench` line and returns its parsed summary; panics unless the answer is `ok <json>`.
    fn bench_summary(session: &mut HeadlessSession, line: &str) -> Value {
        let lines = run_script(session, format!("{line}\n").as_bytes());
        let json = lines
            .first()
            .and_then(|line| line.strip_prefix("ok "))
            .unwrap_or_else(|| panic!("bench answers `ok <json>`, got {lines:?}"));
        serde_json::from_str(json).expect("bench payload is JSON")
    }

    /// Splits the CSV into its rows of comma-separated fields; panics unless the header matches.
    fn csv_rows(path: &std::path::Path) -> Vec<Vec<String>> {
        let text = fs::read_to_string(path).expect("csv written");
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(CSV_HEADER), "csv header");
        lines
            .map(|line| line.split(',').map(str::to_string).collect())
            .collect()
    }

    #[test]
    fn bench_writes_csv_and_summary() {
        let mut session = session_for_test(800, 600);
        let dir = scratch_dir("csv");
        let csv = dir.join("b.csv");
        let summary = bench_summary(&mut session, &format!("bench 5 csv={}", csv.display()));
        assert_eq!(summary["frames"], 5);
        assert_eq!(summary["hz_source"], "default");
        assert!(summary["total_ms"]["p50"].as_f64().expect("total_ms p50") > 0.0);
        let rows = csv_rows(&csv);
        assert_eq!(rows.len(), 5, "one csv row per frame");
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row.len(), 8, "eight columns per row: {row:?}");
            assert_eq!(row[0], index.to_string(), "frame column: {row:?}");
            let flush_calls: u64 = row[5].parse().expect("flush_calls");
            let vertices: u64 = row[6].parse().expect("vertices");
            assert!(flush_calls >= 1, "every frame flushes: {row:?}");
            assert!(vertices > 0, "every frame has vertices: {row:?}");
        }
    }

    /// Runs `bench <frames> csv=<scratch> <action>` and asserts that it draws exactly `frames`
    /// frames and writes one CSV row per frame.
    fn assert_exact_frames(tag: &str, frames: u64, action: &str) {
        let mut session = session_for_test(800, 600);
        let csv = scratch_dir(tag).join("b.csv");
        let before = dump(&mut session)["event_loop"]["frames"].as_u64().expect("frames");
        let summary = bench_summary(&mut session, &format!("bench {frames} csv={} {action}", csv.display()));
        let after = dump(&mut session)["event_loop"]["frames"].as_u64().expect("frames");
        assert_eq!(summary["frames"], frames, "summary of {action}");
        assert_eq!(after - before, frames, "frames drawn by bench {frames} {action}");
        assert_eq!(csv_rows(&csv).len() as u64, frames, "csv rows of {action}");
    }

    #[test]
    fn bench_type_action_runs_all_frames() {
        assert_exact_frames("type", 3, "type x");
    }

    #[test]
    fn bench_key_action_draws_one_frame_per_bench_frame() {
        assert_exact_frames("key", 4, "key a");
        assert_exact_frames("key-mods", 2, "key ctrl+shift+a");
    }

    #[test]
    fn bench_wheel_action_draws_one_frame_per_bench_frame() {
        assert_exact_frames("wheel", 3, "wheel 0 1");
    }

    #[test]
    fn bench_rejects_csv_path_under_a_file() {
        let mut session = session_for_test(800, 600);
        let file = scratch_dir("csv-file").join("plain");
        fs::write(&file, "x").expect("plain file");
        let lines = run_script(&mut session, format!("bench 2 csv={}/b.csv\n", file.display()).as_bytes());
        assert!(
            lines.first().is_some_and(|line| line.starts_with("err csv: ")),
            "bench with a csv under a file answers err: {lines:?}"
        );
    }

    #[test]
    fn bench_wheel_action_moves_scroll_y_between_rows() {
        let mut session = session_for_test(800, 600);
        let rows = dump(&mut session)["terminal"]["rows"].as_u64().expect("rows") as usize;
        let last = 3 * rows;
        run_script(&mut session, format!("type seq 1 {last}\n").as_bytes());
        run_script(&mut session, b"key enter\n");
        wait_until(&mut session, 10_000, "seq output in scrollback", |dump| {
            dump["terminal"]["viewport"]["scrollback"].as_u64().unwrap_or(0) > 0
        });
        run_script(&mut session, b"mouse_cell 0 0\n");
        let dir = scratch_dir("wheel");
        let csv = dir.join("wheel.csv");
        let lines = run_script(&mut session, format!("bench 60 csv={} wheel 0 1\n", csv.display()).as_bytes());
        assert!(lines.first().is_some_and(|line| line.starts_with("ok ")), "bench answers ok: {lines:?}");
        let rows = csv_rows(&csv);
        assert_eq!(rows.len(), 60, "one csv row per frame");
        // The animation runs on the real frame `dt`, so the frames move it by small amounts;
        // over the run it still moves.
        let first = &rows[0][SCROLL_Y];
        let last = &rows[rows.len() - 1][SCROLL_Y];
        assert_ne!(first, last, "scroll_y changes over the run: {rows:?}");
    }
}
