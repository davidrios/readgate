//! The question loop, apart from any window: it takes each request from
//! the gate, applies the grants, asks every front-end that's listening,
//! and answers with the first answer any of them gives.
//!
//! The front-ends are the app's prompt window and the terminals attached
//! to its socket (`readgate attach`). Each one subscribes and gets every
//! [`Event`]. `readgate serve` runs this loop with no window at all.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use crate::access_log::{Decision, LogEntry};
use crate::gate::Requests;
use crate::service::{self, Answer, Grants, PromptRequest};

/// A request being asked about, as the front-ends see it.
#[derive(Clone, Debug)]
pub struct Question {
    /// Answers name it, so a late answer to an earlier question doesn't
    /// count for the next.
    pub id: u64,
    pub request: PromptRequest,
}

#[derive(Clone, Debug)]
pub enum Event {
    /// A question to show. A front-end that subscribes while one is up
    /// gets it first thing.
    Ask(Question),
    /// The question with this id was answered, here or elsewhere, or ran
    /// out of time: stop showing it.
    Done(u64),
    /// An answer, given or by a grant, as it went to the log file.
    Logged(LogEntry),
}

struct Inner {
    subscribers: Vec<async_channel::Sender<Event>>,
    current: Option<Question>,
}

/// Where front-ends listen and answer. Shared with every one of them.
pub struct Hub {
    inner: Mutex<Inner>,
    answers: mpsc::Sender<(u64, Answer)>,
}

impl Hub {
    /// Every event from now on, starting with the question being asked,
    /// if there is one.
    pub fn subscribe(&self) -> async_channel::Receiver<Event> {
        let (sender, receiver) = async_channel::unbounded();
        let mut inner = self.inner.lock().unwrap();
        if let Some(question) = &inner.current {
            let _ = sender.try_send(Event::Ask(question.clone()));
        }
        inner.subscribers.push(sender);
        receiver
    }

    /// Answers the question with `id`. The first answer to it wins; an
    /// answer to a question that's gone is dropped.
    pub fn answer(&self, id: u64, answer: Answer) {
        let _ = self.answers.send((id, answer));
    }

    fn broadcast(&self, inner: &mut Inner, event: Event) {
        // A front-end that went away has dropped its end.
        inner.subscribers.retain(|s| s.try_send(event.clone()).is_ok());
    }

    fn ask(&self, question: Question) {
        let mut inner = self.inner.lock().unwrap();
        inner.current = Some(question.clone());
        self.broadcast(&mut inner, Event::Ask(question));
    }

    fn done(&self, id: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.current = None;
        self.broadcast(&mut inner, Event::Done(id));
    }

    fn logged(&self, entry: LogEntry) {
        let mut inner = self.inner.lock().unwrap();
        self.broadcast(&mut inner, Event::Logged(entry));
    }
}

/// Starts answering `requests` on a thread of its own, writing each
/// answer to `log_file`. One request at a time: the others wait in line,
/// their readers still blocked, their time still running. Grants last as
/// long as the loop, so mounting again keeps them.
pub fn start(requests: Requests, log_file: PathBuf) -> Arc<Hub> {
    let (answers, answered) = mpsc::channel();
    let hub = Arc::new(Hub { inner: Mutex::new(Inner { subscribers: Vec::new(), current: None }), answers });
    let server = hub.clone();
    std::thread::Builder::new()
        .name("readgate-questions".into())
        .spawn(move || {
            let mut grants = Grants::default();
            let mut next_id = 0;
            while let Ok(request) = requests.recv_blocking() {
                let decision = if grants.cover(&request) {
                    Decision::Granted
                } else if request.deadline <= Instant::now() {
                    // Ran out while waiting in line: never shown.
                    Decision::TimedOut
                } else {
                    next_id += 1;
                    let id = next_id;
                    server.ask(Question { id, request: PromptRequest::of(&request) });
                    let answer = wait(&answered, id, request.deadline);
                    server.done(id);
                    service::decide(&mut grants, &request, answer)
                };
                let entry = service::record(&log_file, &request, decision);
                server.logged(entry);
                request.answer(decision);
            }
        })
        .expect("readgate couldn't start its question thread");
    hub
}

/// The first answer to question `id`, or a time out at `deadline`.
fn wait(answered: &mpsc::Receiver<(u64, Answer)>, id: u64, deadline: Instant) -> Answer {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match answered.recv_timeout(left) {
            Ok((answer_id, answer)) if answer_id == id => return answer,
            // To an earlier question, given as it ended.
            Ok(_) => continue,
            Err(mpsc::RecvTimeoutError::Timeout) => return Answer::TimedOut,
            // The hub keeps a sender for as long as this thread runs.
            Err(mpsc::RecvTimeoutError::Disconnected) => return Answer::Deny,
        }
    }
}
