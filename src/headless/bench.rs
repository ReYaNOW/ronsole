//! `bench <frames> [csv=<path>] [action]`: forced frames with per-frame timings,
//! a CSV of those frames and a JSON summary of the run.

#[cfg(test)]
mod tests {
    use super::super::session::HeadlessSession;
    use super::super::tests::{dump, run_script, session_for_test, wait_until};
    use serde_json::Value;
    use std::fs;
    use std::path::PathBuf;

    /// Header of the per-frame CSV, in column order.
    const CSV_HEADER: &str = "update_ms,draw_cpu_ms,gpu_ms,total_ms,flush_calls,vertices,scroll_y";

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
        for row in &rows {
            assert_eq!(row.len(), 7, "seven columns per row: {row:?}");
            let flush_calls: u64 = row[4].parse().expect("flush_calls");
            let vertices: u64 = row[5].parse().expect("vertices");
            assert!(flush_calls >= 1, "every frame flushes: {row:?}");
            assert!(vertices > 0, "every frame has vertices: {row:?}");
        }
    }

    #[test]
    fn bench_type_action_runs_all_frames() {
        let mut session = session_for_test(800, 600);
        let summary = bench_summary(&mut session, "bench 3 type x");
        assert_eq!(summary["frames"], 3);
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
        let lines = run_script(&mut session, format!("bench 2 csv={} wheel 0 1\n", csv.display()).as_bytes());
        assert!(lines.first().is_some_and(|line| line.starts_with("ok ")), "bench answers ok: {lines:?}");
        let rows = csv_rows(&csv);
        assert_eq!(rows.len(), 2, "one csv row per frame");
        let first = &rows[0][6];
        let second = &rows[1][6];
        assert_ne!(first, second, "scroll_y changes between frames: {rows:?}");
    }
}
