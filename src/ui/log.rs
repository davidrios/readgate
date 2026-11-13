//! The log screen: this run's answers, oldest first, in a text area that
//! follows its end as answers come in. Earlier runs' are in the log file.

use std::collections::VecDeque;

use mitsuami::prelude::*;

use super::local_time;
use crate::access_log::{Decision, LogEntry};

/// How many entries the screen keeps; the file keeps them all.
const KEEP: usize = 1000;

/// What the screen shows: one block of text per entry.
#[derive(Clone, Copy)]
pub struct LogView {
    text: Signal<String>,
    /// Each block's length in `text`, oldest first, to drop the oldest.
    lengths: Signal<VecDeque<usize>>,
}

impl LogView {
    pub fn new() -> LogView {
        LogView { text: signal(String::new()), lengths: signal(VecDeque::new()) }
    }

    pub fn push(self, entry: &LogEntry) {
        let block = format_entry(entry);
        let mut lengths = self.lengths.get_untracked();
        lengths.push_back(block.len());
        let dropped = if lengths.len() > KEEP { lengths.pop_front().unwrap_or(0) } else { 0 };
        self.lengths.set(lengths);
        self.text.update(|text| {
            text.drain(..dropped);
            text.push_str(&block);
        });
    }
}

/// A summary line, then everything about the reader, indented.
fn format_entry(e: &LogEntry) -> String {
    let answer = match e.decision {
        Decision::Allowed => "ALLOWED",
        Decision::Denied => "DENIED",
        Decision::TimedOut => "TIMED OUT",
        Decision::Granted => "GRANTED",
    };
    let mut block = format!(
        "{}  {answer:<9}  {} (pid {}, user {}): {} {}\n",
        local_time(e.time),
        e.reader.program(),
        e.reader.pid,
        e.reader.user_label(),
        e.access.verb(),
        e.dest.display()
    );
    let details = format!("{}\nMounted from: {}", e.reader.describe(), e.source.display());
    for line in details.lines() {
        block.push_str("    ");
        block.push_str(line);
        block.push('\n');
    }
    block.push('\n');
    block
}

pub fn screen(log: LogView) -> impl View {
    let text = log.text;
    view! {
        <Column gap=Spacing::Md grow=1.0>
            <TextArea
                a11y_label="Access log"
                value=text
                read_only=true
                line_wrap=false
                text_style=TextStyle::Monospace
                follows_end=true
                grow=1.0
            />
        </Column>
    }
}
