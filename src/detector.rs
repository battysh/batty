//! Prompt detection state machine.
//!
//! Combines silence detection (no new output for N seconds) with regex-based
//! prompt pattern matching to detect when the executor is asking a question.
//!
//! ## State machine
//!
//! ```text
//! WORKING   → new output arriving       → extract events, do nothing else
//! PAUSED    → output stopped (N secs)   → check if last line is a prompt
//! QUESTION  → prompt detected           → trigger response (Tier 1 or Tier 2)
//! ANSWERING → response injected         → wait for executor to resume
//! → back to WORKING
//! ```

use std::time::{Duration, Instant};

use crate::prompt::{DetectedPrompt, PromptPatterns, strip_ansi};

/// Supervisor state machine states.
#[derive(Debug, Clone, PartialEq)]
pub enum SupervisorState {
    /// Executor is producing output — everything normal.
    Working,
    /// Output stopped — waiting to see if it's a prompt.
    Paused {
        /// When the silence started.
        since: Instant,
        /// The last non-empty line seen (for prompt matching).
        last_line: String,
        /// Whether an unknown-request fallback event was already emitted for
        /// this paused period.
        unknown_emitted: bool,
    },
    /// A prompt was detected — waiting for a response decision.
    Question {
        /// The detected prompt.
        prompt: DetectedPrompt,
        /// When the question was detected.
        detected_at: Instant,
    },
    /// A response was injected — waiting for executor to resume.
    Answering {
        /// When the response was injected.
        injected_at: Instant,
    },
}

/// Configuration for the prompt detector.
#[derive(Debug, Clone)]
pub struct DetectorConfig {
    /// How long to wait with no output before checking for a prompt (seconds).
    pub silence_timeout: Duration,
    /// How long after injecting an answer to wait before returning to Working.
    pub answer_cooldown: Duration,
    /// If true, emit an `UnknownRequest` event when output is silent and no
    /// known prompt pattern matches. This lets Tier 2 decide what to do.
    pub unknown_request_fallback: bool,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            silence_timeout: Duration::from_secs(3),
            answer_cooldown: Duration::from_secs(1),
            unknown_request_fallback: true,
        }
    }
}

/// Prompt detection state machine.
///
/// Call `on_output()` when new output arrives and `tick()` periodically.
/// The detector transitions through states and signals when a prompt is
/// detected or when the executor resumes.
pub struct PromptDetector {
    state: SupervisorState,
    config: DetectorConfig,
    patterns: PromptPatterns,
    last_output_time: Option<Instant>,
    last_line: String,
}

/// Events emitted by the detector for the supervisor to act on.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum DetectorEvent {
    /// Executor is working normally — no action needed.
    Working,
    /// Output has paused — may be a prompt.
    Silence {
        duration: Duration,
        last_line: String,
    },
    /// Output is silent and no known prompt pattern matched. Caller should
    /// ask Tier 2 for an intelligent action or escalate.
    UnknownRequest {
        duration: Duration,
        last_line: String,
    },
    /// A prompt was detected (via pattern match or silence + pattern).
    PromptDetected(DetectedPrompt),
    /// Waiting for executor to resume after answer injection.
    WaitingForResume,
    /// Executor resumed after answer injection.
    Resumed,
}

impl PromptDetector {
    pub fn new(patterns: PromptPatterns, config: DetectorConfig) -> Self {
        Self {
            state: SupervisorState::Working,
            config,
            patterns,
            last_output_time: None,
            last_line: String::new(),
        }
    }

    /// Get the current state.
    pub fn state(&self) -> &SupervisorState {
        &self.state
    }

    /// Called when new output arrives from the executor.
    ///
    /// Returns a DetectorEvent if a prompt is immediately detected in the output.
    pub fn on_output(&mut self, line: &str) -> Option<DetectorEvent> {
        let stripped = strip_ansi(line);
        let trimmed = stripped.trim();

        if trimmed.is_empty() {
            return None;
        }

        self.last_output_time = Some(Instant::now());
        self.last_line = trimmed.to_string();

        match &self.state {
            SupervisorState::Working | SupervisorState::Paused { .. } => {
                // Check for immediate prompt detection (inline, not silence-based)
                if let Some(detected) = self.patterns.detect(trimmed) {
                    self.state = SupervisorState::Question {
                        prompt: detected.clone(),
                        detected_at: Instant::now(),
                    };
                    return Some(DetectorEvent::PromptDetected(detected));
                }

                // Back to working
                self.state = SupervisorState::Working;
                None
            }
            SupervisorState::Answering { .. } => {
                // Output arrived after we injected an answer — executor resumed
                self.state = SupervisorState::Working;
                Some(DetectorEvent::Resumed)
            }
            SupervisorState::Question { .. } => {
                // New output while in question state — executor may have been
                // answered by human or resumed on its own
                self.state = SupervisorState::Working;
                Some(DetectorEvent::Resumed)
            }
        }
    }

    /// Called periodically (e.g., every 100ms) to check for silence-based prompts.
    ///
    /// Returns a DetectorEvent describing the current situation.
    pub fn tick(&mut self) -> DetectorEvent {
        let now = Instant::now();

        match self.state.clone() {
            SupervisorState::Working => {
                // Check if output has gone silent
                if let Some(last) = self.last_output_time {
                    let silence = now.duration_since(last);
                    if silence >= self.config.silence_timeout && !self.last_line.is_empty() {
                        // Transition to Paused
                        self.state = SupervisorState::Paused {
                            since: last,
                            last_line: self.last_line.clone(),
                            unknown_emitted: false,
                        };

                        // Check if last line matches a prompt pattern
                        if let Some(detected) = self.patterns.detect(&self.last_line) {
                            self.state = SupervisorState::Question {
                                prompt: detected.clone(),
                                detected_at: now,
                            };
                            return DetectorEvent::PromptDetected(detected);
                        }

                        if self.config.unknown_request_fallback {
                            self.state = SupervisorState::Paused {
                                since: last,
                                last_line: self.last_line.clone(),
                                unknown_emitted: true,
                            };
                            return DetectorEvent::UnknownRequest {
                                duration: silence,
                                last_line: self.last_line.clone(),
                            };
                        }

                        return DetectorEvent::Silence {
                            duration: silence,
                            last_line: self.last_line.clone(),
                        };
                    }
                }
                DetectorEvent::Working
            }
            SupervisorState::Paused {
                since,
                last_line,
                unknown_emitted,
            } => {
                let silence = now.duration_since(since);

                if !unknown_emitted && self.config.unknown_request_fallback {
                    self.state = SupervisorState::Paused {
                        since,
                        last_line: last_line.clone(),
                        unknown_emitted: true,
                    };
                    return DetectorEvent::UnknownRequest {
                        duration: silence,
                        last_line,
                    };
                }

                DetectorEvent::Silence {
                    duration: silence,
                    last_line,
                }
            }
            SupervisorState::Question { prompt, .. } => DetectorEvent::PromptDetected(prompt),
            SupervisorState::Answering { injected_at } => {
                let elapsed = now.duration_since(injected_at);
                if elapsed >= self.config.answer_cooldown {
                    // Cooldown expired, back to working
                    self.state = SupervisorState::Working;
                    DetectorEvent::Resumed
                } else {
                    DetectorEvent::WaitingForResume
                }
            }
        }
    }

    /// Signal that an answer was injected into the executor.
    ///
    /// Transitions to Answering state.
    pub fn answer_injected(&mut self) {
        self.state = SupervisorState::Answering {
            injected_at: Instant::now(),
        };
    }

    /// Signal that the human took over (typed directly).
    ///
    /// Cancels any pending question and returns to Working.
    pub fn human_override(&mut self) {
        self.state = SupervisorState::Working;
        self.last_output_time = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::PromptKind;
    use std::thread;

    fn make_detector() -> PromptDetector {
        PromptDetector::new(
            PromptPatterns::claude_code(),
            DetectorConfig {
                silence_timeout: Duration::from_millis(100),
                answer_cooldown: Duration::from_millis(50),
                unknown_request_fallback: true,
            },
        )
    }

    #[test]
    fn starts_in_working_state() {
        let d = make_detector();
        assert!(matches!(d.state(), SupervisorState::Working));
    }

    #[test]
    fn output_keeps_working_state() {
        let mut d = make_detector();
        let event = d.on_output("Writing function to parse YAML...");
        assert!(event.is_none());
        assert!(matches!(d.state(), SupervisorState::Working));
    }

    #[test]
    fn empty_output_ignored() {
        let mut d = make_detector();
        let event = d.on_output("   ");
        assert!(event.is_none());
    }

    #[test]
    fn inline_prompt_detected_immediately() {
        let mut d = make_detector();
        let event = d.on_output("Allow tool Read on /home/user/file.rs?");
        assert!(matches!(event, Some(DetectorEvent::PromptDetected(_))));
        assert!(matches!(d.state(), SupervisorState::Question { .. }));
    }

    #[test]
    fn silence_triggers_paused_state() {
        let mut d = make_detector();
        d.on_output("some output");

        // Wait for silence timeout
        thread::sleep(Duration::from_millis(150));

        let event = d.tick();
        match event {
            DetectorEvent::Silence { last_line, .. } => {
                assert_eq!(last_line, "some output");
            }
            DetectorEvent::UnknownRequest { last_line, .. } => {
                assert_eq!(last_line, "some output");
            }
            DetectorEvent::PromptDetected(_) => {} // also acceptable if pattern matched
            other => panic!("expected Silence/UnknownRequest/PromptDetected, got: {other:?}"),
        }
    }

    #[test]
    fn silence_with_prompt_pattern_detects_question() {
        let mut d = make_detector();
        d.on_output("Continue? [y/n]");

        // The inline detection should fire immediately
        // But let's also test the silence path — reset state and simulate
        let mut d2 = PromptDetector::new(
            PromptPatterns::claude_code(),
            DetectorConfig {
                silence_timeout: Duration::from_millis(100),
                answer_cooldown: Duration::from_millis(50),
                unknown_request_fallback: true,
            },
        );
        // Manually set last line and time to simulate a prompt that wasn't caught inline
        d2.last_output_time = Some(Instant::now() - Duration::from_millis(200));
        d2.last_line = "Continue? [y/n]".to_string();

        let event = d2.tick();
        match event {
            DetectorEvent::PromptDetected(ref p) => {
                assert!(matches!(p.kind, PromptKind::Confirmation { .. }));
            }
            other => panic!("expected PromptDetected, got: {other:?}"),
        }
    }

    #[test]
    fn answer_injection_transitions_to_answering() {
        let mut d = make_detector();
        d.on_output("Allow tool Read?");
        d.answer_injected();
        assert!(matches!(d.state(), SupervisorState::Answering { .. }));
    }

    #[test]
    fn output_after_answer_resumes_working() {
        let mut d = make_detector();
        d.on_output("Allow tool Read?");
        d.answer_injected();

        let event = d.on_output("Reading file...");
        assert!(matches!(event, Some(DetectorEvent::Resumed)));
        assert!(matches!(d.state(), SupervisorState::Working));
    }

    #[test]
    fn answer_cooldown_returns_to_working() {
        let mut d = make_detector();
        d.on_output("Allow tool Read?");
        d.answer_injected();

        // Before cooldown
        let event = d.tick();
        assert!(matches!(event, DetectorEvent::WaitingForResume));

        // After cooldown
        thread::sleep(Duration::from_millis(60));
        let event = d.tick();
        assert!(matches!(event, DetectorEvent::Resumed));
        assert!(matches!(d.state(), SupervisorState::Working));
    }

    #[test]
    fn human_override_cancels_question() {
        let mut d = make_detector();
        d.on_output("Allow tool Read?");
        assert!(matches!(d.state(), SupervisorState::Question { .. }));

        d.human_override();
        assert!(matches!(d.state(), SupervisorState::Working));
    }

    #[test]
    fn new_output_during_question_resumes() {
        let mut d = make_detector();
        d.on_output("Allow tool Read?");
        assert!(matches!(d.state(), SupervisorState::Question { .. }));

        // Human typed something or executor continued
        let event = d.on_output("Reading file...");
        assert!(matches!(event, Some(DetectorEvent::Resumed)));
        assert!(matches!(d.state(), SupervisorState::Working));
    }

    #[test]
    fn tick_before_any_output_is_working() {
        let mut d = make_detector();
        let event = d.tick();
        assert!(matches!(event, DetectorEvent::Working));
    }

    #[test]
    fn ansi_stripped_before_matching() {
        let mut d = make_detector();
        let event = d.on_output("\x1b[33mAllow tool Read?\x1b[0m");
        assert!(matches!(event, Some(DetectorEvent::PromptDetected(_))));
    }

    #[test]
    fn default_config_values() {
        let config = DetectorConfig::default();
        assert_eq!(config.silence_timeout, Duration::from_secs(3));
        assert_eq!(config.answer_cooldown, Duration::from_secs(1));
        assert!(config.unknown_request_fallback);
    }

    #[test]
    fn paused_state_persists_on_tick() {
        let mut d = make_detector();
        d.on_output("some non-prompt output");

        thread::sleep(Duration::from_millis(150));

        // First tick should emit unknown request fallback for this pause.
        let first = d.tick();
        assert!(matches!(first, DetectorEvent::UnknownRequest { .. }));
        // Second tick should report Silence (fallback emitted once).
        let event = d.tick();
        assert!(matches!(event, DetectorEvent::Silence { .. }));
    }

    #[test]
    fn output_resets_silence() {
        let mut d = make_detector();
        d.on_output("line 1");
        thread::sleep(Duration::from_millis(50));
        d.on_output("line 2"); // reset the silence timer

        // Should still be working (not enough silence since line 2)
        let event = d.tick();
        assert!(matches!(event, DetectorEvent::Working));
    }

    #[test]
    fn unknown_request_disabled_reports_silence_only() {
        let mut d = PromptDetector::new(
            PromptPatterns::claude_code(),
            DetectorConfig {
                silence_timeout: Duration::from_millis(100),
                answer_cooldown: Duration::from_millis(50),
                unknown_request_fallback: false,
            },
        );
        d.on_output("some non-prompt output");
        thread::sleep(Duration::from_millis(150));

        let first = d.tick();
        assert!(matches!(first, DetectorEvent::Silence { .. }));
        let second = d.tick();
        assert!(matches!(second, DetectorEvent::Silence { .. }));
    }
}
