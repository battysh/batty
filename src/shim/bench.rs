//! Shim performance benchmarks: measures state detection latency.
//!
//! Gated behind `shim-benchmark` feature flag.
//! Run with: cargo test --features shim-benchmark bench_ -- --nocapture
//!
//! These tests spawn a real bash process via the shim runtime and
//! measure latency for key state transitions, comparing against
//! the legacy 5-second tmux poll cycle.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use crate::shim::protocol::{self, Channel, Command, Event, ShimState};
    use crate::shim::runtime::ShimArgs;

    /// Legacy tmux polling cycle (what the shim replaces).
    const LEGACY_POLL_CYCLE_MS: u64 = 5000;

    /// Maximum acceptable latency for any shim metric (sub-second target).
    const SUB_SECOND_THRESHOLD_MS: u64 = 1000;

    /// Number of iterations for transition/injection benchmarks.
    const BENCH_ITERATIONS: usize = 5;

    /// Shell command for the benchmark agent.
    /// Uses `exec` to replace the wrapper bash -c process with an
    /// interactive shell. The `--norc --noprofile` flags skip startup
    /// files for fast, predictable startup. The env var silences the
    /// macOS "default shell is now zsh" deprecation warning.
    const BENCH_REPL_CMD: &str =
        "export BASH_SILENCE_DEPRECATION_WARNING=1; exec bash --norc --noprofile -i";

    /// Helper: spawn a shim with a mini-REPL in a background thread,
    /// return the parent channel with a read timeout set so recv doesn't
    /// block forever.
    fn spawn_bench_shim() -> Channel {
        let (parent_sock, child_sock) = protocol::socketpair().unwrap();

        // Set a read timeout so Channel::recv() returns Err periodically,
        // allowing wait-loop deadline checks to execute.
        parent_sock
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();

        let channel = Channel::new(child_sock);

        let args = ShimArgs {
            id: "bench-agent".into(),
            agent_type: crate::shim::classifier::AgentType::Generic,
            cmd: BENCH_REPL_CMD.into(),
            cwd: PathBuf::from("/tmp"),
            rows: 24,
            cols: 80,
            pty_log_path: None,
        };

        std::thread::spawn(move || {
            crate::shim::runtime::run(args, channel).ok();
        });

        Channel::new(parent_sock)
    }

    /// Wait for a Ready event (with timeout). Returns elapsed time.
    /// Handles read-timeout errors by retrying until deadline.
    fn wait_for_ready(ch: &mut Channel, timeout: Duration) -> Option<Duration> {
        let start = Instant::now();
        let deadline = start + timeout;
        loop {
            if Instant::now() > deadline {
                return None;
            }
            match ch.recv::<Event>() {
                Ok(Some(Event::Ready)) => return Some(start.elapsed()),
                Ok(Some(_)) => continue,
                Ok(None) => return None, // clean EOF — peer closed
                Err(_) => continue,      // read timeout — retry
            }
        }
    }

    // -----------------------------------------------------------------------
    // Benchmark: Ready detection latency
    // -----------------------------------------------------------------------

    #[test]
    #[cfg_attr(not(feature = "shim-benchmark"), ignore)]
    fn bench_ready_detection_latency() {
        let mut samples = Vec::new();

        for i in 0..BENCH_ITERATIONS {
            let mut ch = spawn_bench_shim();
            let elapsed = wait_for_ready(&mut ch, Duration::from_secs(30));
            assert!(
                elapsed.is_some(),
                "iteration {i}: shim did not become ready"
            );
            let ms = elapsed.unwrap().as_millis() as u64;
            samples.push(ms);
            ch.send(&Command::Shutdown { timeout_secs: 2 }).unwrap();
            // Small delay between iterations for cleanup
            std::thread::sleep(Duration::from_millis(200));
        }

        let (min, max, avg) = stats(&samples);

        eprintln!();
        eprintln!("## Ready Detection Latency");
        eprintln!();
        eprintln!("Time from shim spawn to Ready event (agent prompt detected).");
        eprintln!();
        eprintln!("| Metric | Value |");
        eprintln!("|--------|-------|");
        eprintln!("| Samples | {} |", samples.len());
        eprintln!("| Min | {}ms |", min);
        eprintln!("| Max | {}ms |", max);
        eprintln!("| Avg | {}ms |", avg);
        eprintln!("| Legacy poll | {}ms |", LEGACY_POLL_CYCLE_MS);
        eprintln!(
            "| Speedup | {:.1}x |",
            LEGACY_POLL_CYCLE_MS as f64 / avg.max(1) as f64
        );
        eprintln!();

        assert!(
            avg < SUB_SECOND_THRESHOLD_MS,
            "avg ready latency {}ms exceeds sub-second threshold ({}ms)",
            avg,
            SUB_SECOND_THRESHOLD_MS
        );
    }

    // -----------------------------------------------------------------------
    // Benchmark: Working→Idle transition latency
    // -----------------------------------------------------------------------

    #[test]
    #[cfg_attr(not(feature = "shim-benchmark"), ignore)]
    fn bench_working_to_idle_latency() {
        let mut ch = spawn_bench_shim();
        let ready = wait_for_ready(&mut ch, Duration::from_secs(30));
        assert!(ready.is_some(), "shim did not become ready");

        let mut samples = Vec::new();

        for i in 0..BENCH_ITERATIONS {
            ch.send(&Command::SendMessage {
                from: "bench".into(),
                body: format!("echo bench-marker-{i}"),
                message_id: Some(format!("bench-{i}")),
            })
            .unwrap();

            // Drain all events for this command in a single pass, extracting
            // the timestamps we need. This avoids selective-consumption issues
            // where one wait function accidentally swallows another's event.
            let send_time = Instant::now();
            let mut got_working = false;
            let mut idle_latency: Option<Duration> = None;
            let mut got_completion = false;
            let deadline = Instant::now() + Duration::from_secs(10);

            while Instant::now() < deadline {
                if got_completion {
                    break;
                }
                match ch.recv::<Event>() {
                    Ok(Some(Event::StateChanged {
                        to: ShimState::Working,
                        ..
                    })) => {
                        got_working = true;
                    }
                    Ok(Some(Event::StateChanged {
                        to: ShimState::Idle,
                        ..
                    })) => {
                        if got_working && idle_latency.is_none() {
                            idle_latency = Some(send_time.elapsed());
                        }
                    }
                    Ok(Some(Event::Completion { .. })) => {
                        if got_working && idle_latency.is_none() {
                            idle_latency = Some(send_time.elapsed());
                        }
                        got_completion = true;
                    }
                    Ok(Some(Event::Died { .. })) | Ok(None) => break,
                    Ok(Some(_)) => continue,
                    Err(_) => continue, // read timeout
                }
            }

            assert!(got_working, "iteration {i}: did not transition to Working");
            assert!(
                idle_latency.is_some(),
                "iteration {i}: did not return to Idle (got_completion={got_completion})"
            );
            samples.push(idle_latency.unwrap().as_millis() as u64);

            // Brief pause to let the shim's PTY reader settle
            std::thread::sleep(Duration::from_millis(150));
        }

        ch.send(&Command::Shutdown { timeout_secs: 2 }).unwrap();

        let (min, max, avg) = stats(&samples);

        eprintln!();
        eprintln!("## Working→Idle Transition Latency");
        eprintln!();
        eprintln!("Time from command completion (last PTY output) to StateChanged event.");
        eprintln!();
        eprintln!("| Metric | Value |");
        eprintln!("|--------|-------|");
        eprintln!("| Samples | {} |", samples.len());
        eprintln!("| Min | {}ms |", min);
        eprintln!("| Max | {}ms |", max);
        eprintln!("| Avg | {}ms |", avg);
        eprintln!("| Legacy poll | {}ms |", LEGACY_POLL_CYCLE_MS);
        eprintln!(
            "| Speedup | {:.1}x |",
            LEGACY_POLL_CYCLE_MS as f64 / avg.max(1) as f64
        );
        eprintln!();

        assert!(
            avg < SUB_SECOND_THRESHOLD_MS,
            "avg transition latency {}ms exceeds sub-second threshold ({}ms)",
            avg,
            SUB_SECOND_THRESHOLD_MS
        );
    }

    // -----------------------------------------------------------------------
    // Benchmark: Message injection latency
    // -----------------------------------------------------------------------

    #[test]
    #[cfg_attr(not(feature = "shim-benchmark"), ignore)]
    fn bench_message_injection_latency() {
        let mut ch = spawn_bench_shim();
        let ready = wait_for_ready(&mut ch, Duration::from_secs(30));
        assert!(ready.is_some(), "shim did not become ready");

        let mut samples = Vec::new();

        for i in 0..BENCH_ITERATIONS {
            let send_time = Instant::now();

            ch.send(&Command::SendMessage {
                from: "bench".into(),
                body: format!("echo inject-{i}"),
                message_id: Some(format!("inject-{i}")),
            })
            .unwrap();

            // Drain all events, extracting injection latency (time to Working).
            let mut injection_latency: Option<Duration> = None;
            let mut got_completion = false;
            let deadline = Instant::now() + Duration::from_secs(10);

            while Instant::now() < deadline {
                if got_completion {
                    break;
                }
                match ch.recv::<Event>() {
                    Ok(Some(Event::StateChanged {
                        to: ShimState::Working,
                        ..
                    })) => {
                        if injection_latency.is_none() {
                            injection_latency = Some(send_time.elapsed());
                        }
                    }
                    Ok(Some(Event::Completion { .. })) => {
                        got_completion = true;
                    }
                    Ok(Some(Event::Died { .. })) | Ok(None) => break,
                    Ok(Some(_)) => continue,
                    Err(_) => continue,
                }
            }

            assert!(
                injection_latency.is_some(),
                "iteration {i}: did not receive Working transition"
            );
            samples.push(injection_latency.unwrap().as_millis() as u64);

            // Brief pause to let the shim's PTY reader settle
            std::thread::sleep(Duration::from_millis(150));
        }

        ch.send(&Command::Shutdown { timeout_secs: 2 }).unwrap();

        let (min, max, avg) = stats(&samples);

        eprintln!();
        eprintln!("## Message Injection Latency");
        eprintln!();
        eprintln!(
            "Time from SendMessage command to Working state transition (PTY write + detection)."
        );
        eprintln!();
        eprintln!("| Metric | Value |");
        eprintln!("|--------|-------|");
        eprintln!("| Samples | {} |", samples.len());
        eprintln!("| Min | {}ms |", min);
        eprintln!("| Max | {}ms |", max);
        eprintln!("| Avg | {}ms |", avg);
        eprintln!("| Legacy poll | {}ms |", LEGACY_POLL_CYCLE_MS);
        eprintln!(
            "| Speedup | {:.1}x |",
            LEGACY_POLL_CYCLE_MS as f64 / avg.max(1) as f64
        );
        eprintln!();

        assert!(
            avg < SUB_SECOND_THRESHOLD_MS,
            "avg injection latency {}ms exceeds sub-second threshold ({}ms)",
            avg,
            SUB_SECOND_THRESHOLD_MS
        );
    }

    // -----------------------------------------------------------------------
    // Combined report
    // -----------------------------------------------------------------------

    #[test]
    #[cfg_attr(not(feature = "shim-benchmark"), ignore)]
    fn bench_full_report() {
        eprintln!();
        eprintln!("# Shim Performance Benchmark Report");
        eprintln!();
        eprintln!("Comparing shim-based state detection vs legacy 5-second tmux poll cycle.");
        eprintln!();

        // --- Ready detection ---
        let mut ready_samples = Vec::new();
        for _ in 0..BENCH_ITERATIONS {
            let mut ch = spawn_bench_shim();
            if let Some(elapsed) = wait_for_ready(&mut ch, Duration::from_secs(30)) {
                ready_samples.push(elapsed.as_millis() as u64);
            }
            ch.send(&Command::Shutdown { timeout_secs: 2 }).unwrap();
            std::thread::sleep(Duration::from_millis(200));
        }

        // --- Transition + injection (reuse one shim) ---
        let mut ch = spawn_bench_shim();
        assert!(
            wait_for_ready(&mut ch, Duration::from_secs(30)).is_some(),
            "shim not ready for transition/injection benchmarks"
        );

        let mut transition_samples = Vec::new();
        let mut injection_samples = Vec::new();

        for i in 0..BENCH_ITERATIONS {
            let send_time = Instant::now();
            ch.send(&Command::SendMessage {
                from: "bench".into(),
                body: format!("echo report-{i}"),
                message_id: Some(format!("report-{i}")),
            })
            .unwrap();

            // Drain all events, extracting both injection and transition latency
            let mut injection_time: Option<Duration> = None;
            let mut transition_time: Option<Duration> = None;
            let mut got_completion = false;
            let deadline = Instant::now() + Duration::from_secs(10);

            while Instant::now() < deadline {
                if got_completion {
                    break;
                }
                match ch.recv::<Event>() {
                    Ok(Some(Event::StateChanged {
                        to: ShimState::Working,
                        ..
                    })) => {
                        if injection_time.is_none() {
                            injection_time = Some(send_time.elapsed());
                        }
                    }
                    Ok(Some(Event::StateChanged {
                        to: ShimState::Idle,
                        ..
                    })) => {
                        if transition_time.is_none() {
                            transition_time = Some(send_time.elapsed());
                        }
                    }
                    Ok(Some(Event::Completion { .. })) => {
                        if transition_time.is_none() {
                            transition_time = Some(send_time.elapsed());
                        }
                        got_completion = true;
                    }
                    Ok(Some(Event::Died { .. })) | Ok(None) => break,
                    Ok(Some(_)) => continue,
                    Err(_) => continue,
                }
            }

            if let Some(t) = injection_time {
                injection_samples.push(t.as_millis() as u64);
            }
            if let Some(t) = transition_time {
                transition_samples.push(t.as_millis() as u64);
            }

            std::thread::sleep(Duration::from_millis(150));
        }

        ch.send(&Command::Shutdown { timeout_secs: 2 }).unwrap();

        // --- Generate report ---
        let (r_min, r_max, r_avg) = stats(&ready_samples);
        let (t_min, t_max, t_avg) = stats(&transition_samples);
        let (i_min, i_max, i_avg) = stats(&injection_samples);

        eprintln!("### Results Summary");
        eprintln!();
        eprintln!("| Metric | Min | Max | Avg | Legacy | Speedup |");
        eprintln!("|--------|-----|-----|-----|--------|---------|");
        eprintln!(
            "| Ready detection | {}ms | {}ms | {}ms | {}ms | {:.1}x |",
            r_min,
            r_max,
            r_avg,
            LEGACY_POLL_CYCLE_MS,
            LEGACY_POLL_CYCLE_MS as f64 / r_avg.max(1) as f64
        );
        eprintln!(
            "| Working→Idle | {}ms | {}ms | {}ms | {}ms | {:.1}x |",
            t_min,
            t_max,
            t_avg,
            LEGACY_POLL_CYCLE_MS,
            LEGACY_POLL_CYCLE_MS as f64 / t_avg.max(1) as f64
        );
        eprintln!(
            "| Injection | {}ms | {}ms | {}ms | {}ms | {:.1}x |",
            i_min,
            i_max,
            i_avg,
            LEGACY_POLL_CYCLE_MS,
            LEGACY_POLL_CYCLE_MS as f64 / i_avg.max(1) as f64
        );
        eprintln!();
        eprintln!(
            "All metrics target sub-second latency (<{}ms).",
            SUB_SECOND_THRESHOLD_MS
        );
        eprintln!();

        // Assert all under threshold
        assert!(
            r_avg < SUB_SECOND_THRESHOLD_MS,
            "ready latency {}ms too high",
            r_avg
        );
        assert!(
            t_avg < SUB_SECOND_THRESHOLD_MS,
            "transition latency {}ms too high",
            t_avg
        );
        assert!(
            i_avg < SUB_SECOND_THRESHOLD_MS,
            "injection latency {}ms too high",
            i_avg
        );
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Compute min, max, avg from samples. Returns (0, 0, 0) if empty.
    fn stats(samples: &[u64]) -> (u64, u64, u64) {
        if samples.is_empty() {
            return (0, 0, 0);
        }
        let min = *samples.iter().min().unwrap();
        let max = *samples.iter().max().unwrap();
        let avg = samples.iter().sum::<u64>() / samples.len() as u64;
        (min, max, avg)
    }
}
