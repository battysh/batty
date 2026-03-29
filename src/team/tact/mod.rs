pub mod parser;
pub mod prompt;

/// A parsed task specification from the architect's planning response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    pub title: String,
    pub body: String,
    pub priority: Option<String>,
    pub depends_on: Vec<u32>,
    pub tags: Vec<String>,
}

pub use parser::{create_board_tasks, parse_planning_response};
pub use prompt::{PLANNING_RESPONSE_FORMAT, compose_planning_prompt};
