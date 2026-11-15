//! What the app does with a request, apart from asking: the grants of
//! "Allow for 1 Minute", what the prompt shows, and the access log.

use std::time::{Duration, Instant};

use time::OffsetDateTime;

use crate::access_log::{self, Access, Decision, LogEntry};
use crate::config::Config;
use crate::gate::AccessRequest;
use crate::reader::Ancestor;

/// How long "Allow for 1 Minute" lasts.
const GRANT_LENGTH: Duration = Duration::from_secs(60);

/// An "Allow for 1 Minute": requests for its mapping, for its access or
/// less, from its process or anything it started, are allowed without
/// asking until it runs out.
struct Grant {
    mapping_id: u64,
    access: Access,
    /// The process it was given to: a pid with its start time, so a later
    /// process that gets the pid again isn't it.
    pid: u32,
    start: Option<u64>,
    until: Instant,
}

impl Grant {
    fn covers(&self, request: &AccessRequest, now: Instant) -> bool {
        let reader = &request.reader;
        let is_it = |pid: u32, start: Option<u64>| pid == self.pid && start == self.start;
        now < self.until
            && request.mapping.id == self.mapping_id
            && self.access.covers(request.access)
            && (is_it(reader.pid, reader.start) || reader.ancestors.iter().any(|a| is_it(a.pid, a.start)))
    }
}

/// The grants given and not run out. They last until the app quits:
/// changing the mappings keeps them.
#[derive(Default)]
pub struct Grants(Vec<Grant>);

impl Grants {
    /// Whether one covers `request`. Checked as each request's turn comes,
    /// so those that waited in line behind the prompt that gave it are
    /// covered too.
    pub fn cover(&mut self, request: &AccessRequest) -> bool {
        let now = Instant::now();
        self.0.retain(|g| now < g.until);
        self.0.iter().any(|g| g.covers(request, now))
    }

    /// For a minute, allows what `to` (one of the reader's ancestors) and
    /// everything it starts asks of the same file.
    pub fn give(&mut self, request: &AccessRequest, to: &Ancestor) {
        self.0.push(Grant {
            mapping_id: request.mapping.id,
            access: request.access,
            pid: to.pid,
            start: to.start,
            until: Instant::now() + GRANT_LENGTH,
        });
    }
}

/// What the prompt shows and offers for a request.
#[derive(Clone, Debug)]
pub struct PromptRequest {
    /// The question: "Allow cat to read notes.txt?".
    pub title: String,
    /// Who's asking, in a line, for the notification.
    pub summary: String,
    /// Everything about the reader, for the window.
    pub details: String,
    /// When its time is up.
    pub deadline: Instant,
    /// The reader's parent, named, for "Allow for 1 Minute": unset when
    /// it has none known, and then there's no such button.
    pub grant: Option<String>,
    /// The programs further up the chain the button's menu offers
    /// instead, closest first: empty for a plain button.
    pub grant_choices: Vec<GrantChoice>,
}

/// One of "Allow for 1 Minute"'s other choices.
#[derive(Clone, Debug, PartialEq)]
pub struct GrantChoice {
    /// Its place in the reader's ancestors.
    pub at: usize,
    /// "claude (pid 51680)".
    pub label: String,
}

/// What the user answered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Answer {
    Allow,
    /// For a minute, to the reader's parent.
    AllowMinute,
    /// For a minute, to the ancestor at this place, one the menu offered.
    AllowMinuteTo(usize),
    Deny,
    TimedOut,
}

impl PromptRequest {
    pub fn of(request: &AccessRequest) -> PromptRequest {
        let reader = &request.reader;
        let dest = request.mapping.dest_path();
        let name = dest.file_name().map_or_else(|| dest.display().to_string(), |n| n.to_string_lossy().into_owned());
        PromptRequest {
            title: format!("Allow {} to {} {name}?", reader.program(), request.access.verb()),
            summary: format!("{} (pid {}), user {}", reader.program(), reader.pid, reader.user_label()),
            details: format!(
                "{}\n\nFile: {}\nMounted from: {}",
                reader.describe(),
                dest.display(),
                request.mapping.source.display()
            ),
            deadline: request.deadline,
            grant: reader.ancestors.first().map(|parent| parent.label()),
            grant_choices: reader
                .grant_choices()
                .into_iter()
                .map(|(at, a)| GrantChoice { at, label: a.label() })
                .collect(),
        }
    }
}

/// The decision for `answer`, giving the grant it asks for.
pub fn decide(grants: &mut Grants, request: &AccessRequest, answer: Answer) -> Decision {
    match answer {
        Answer::Allow => Decision::Allowed,
        Answer::AllowMinute => {
            if let Some(parent) = request.reader.ancestors.first() {
                grants.give(request, parent);
            }
            Decision::Allowed
        }
        // Only one the menu could have offered.
        Answer::AllowMinuteTo(at) => match request.reader.grant_choices().into_iter().find(|(i, _)| *i == at) {
            Some((_, to)) => {
                grants.give(request, to);
                Decision::Allowed
            }
            None => Decision::Denied,
        },
        Answer::Deny => Decision::Denied,
        Answer::TimedOut => Decision::TimedOut,
    }
}

/// Appends the answer to the access log, and returns its entry for the
/// log screen.
pub fn record(config: &Config, request: &AccessRequest, decision: Decision) -> LogEntry {
    let entry = LogEntry {
        time: OffsetDateTime::now_utc(),
        mapping_id: request.mapping.id,
        source: request.mapping.source.clone(),
        dest: request.mapping.dest_path(),
        reader: request.reader.clone(),
        access: request.access,
        decision,
    };
    if let Err(e) = access_log::append(&config.log_file(), &entry) {
        eprintln!("readgate: couldn't write the access log: {e:#}");
    }
    entry
}
