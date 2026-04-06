use std::collections::VecDeque;

const WINDOW_CAPACITY: usize = 10;
const REDIRECT_FAILURE_THRESHOLD: u8 = 2;
const META_PATTERNS: [&str; 5] = [
    "I apologize",
    "I cannot",
    "As an AI",
    "Let me explain why",
    "I understand your frustration",
];

pub struct MetaDetector {
    window: VecDeque<bool>,
    redirect_failures: u8,
}

impl MetaDetector {
    pub fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(WINDOW_CAPACITY),
            redirect_failures: 0,
        }
    }

    pub fn push_line(&mut self, line: &str) -> bool {
        let is_meta = META_PATTERNS.iter().any(|pattern| line.contains(pattern));

        if self.window.len() == WINDOW_CAPACITY {
            self.window.pop_front();
        }
        self.window.push_back(is_meta);

        if is_meta {
            self.redirect_failures = self.redirect_failures.saturating_add(1);
        } else {
            self.redirect_failures = 0;
        }

        self.window.iter().filter(|&&flag| flag).count() >= 5
    }

    pub fn should_restart(&self) -> bool {
        self.redirect_failures >= REDIRECT_FAILURE_THRESHOLD
    }

    pub fn reset(&mut self) {
        self.window.clear();
        self.redirect_failures = 0;
    }
}

impl Default for MetaDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::MetaDetector;

    #[test]
    fn detects_meta_patterns() {
        let mut detector = MetaDetector::new();

        assert!(!detector.push_line("I apologize, but I cannot help with that."));
        assert!(!detector.should_restart());
        detector.reset();
        assert!(!detector.push_line("As an AI, I have to refuse."));
        assert!(!detector.should_restart());
        detector.push_line("normal output");
        assert!(!detector.should_restart());
    }

    #[test]
    fn triggers_after_five_meta_lines_in_window() {
        let mut detector = MetaDetector::new();

        for _ in 0..4 {
            assert!(!detector.push_line("I apologize for the confusion."));
        }
        assert!(detector.push_line("Let me explain why this is blocked."));

        detector.reset();

        for _ in 0..5 {
            detector.push_line("normal output");
        }
        for _ in 0..4 {
            assert!(!detector.push_line("I understand your frustration."));
        }
        assert!(detector.push_line("As an AI, I need to stop here."));
    }

    #[test]
    fn restart_triggers_after_two_failed_redirects() {
        let mut detector = MetaDetector::new();

        assert!(!detector.should_restart());
        detector.push_line("I cannot continue with that request.");
        assert!(!detector.should_restart());
        detector.push_line("I apologize, but I cannot do that.");
        assert!(detector.should_restart());

        detector.reset();
        assert!(!detector.should_restart());
    }
}
