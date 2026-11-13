//! Where a filesystem thread waits for the user.
//!
//! A filesystem thread sends an [`AccessRequest`] to the UI and blocks on
//! its answer; the UI asks the user and answers. The thread holds the
//! reader's read the whole time, so to the reader the read only takes
//! longer. Past the timeout, the answer is no.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::access_log::{Access, Decision};
use crate::config::Mapping;
use crate::reader::ReaderInfo;

/// How long the filesystem thread waits past the deadline for the UI's own
/// timeout before it gives up by itself, in case the UI is stuck.
const BACKSTOP: Duration = Duration::from_secs(5);

pub struct AccessRequest {
    pub mapping: Mapping,
    pub reader: ReaderInfo,
    pub access: Access,
    /// When the answer is due: the read came in, plus the timeout. The
    /// timeout counts while a request waits in line behind another prompt.
    pub deadline: Instant,
    answer: mpsc::SyncSender<Decision>,
}

impl AccessRequest {
    pub fn answer(self, decision: Decision) {
        // The filesystem thread may be gone (unmounted, or gave up at the
        // backstop); nothing to tell.
        let _ = self.answer.send(decision);
    }
}

/// The filesystem's end. Cheap to clone, one per mount.
#[derive(Clone)]
pub struct Gate {
    requests: async_channel::Sender<AccessRequest>,
    timeout: Duration,
}

/// The UI's end.
pub type Requests = async_channel::Receiver<AccessRequest>;

pub fn gate(timeout: Duration) -> (Gate, Requests) {
    // Unbounded: a request waits in line while another prompt is up, and
    // each one blocks its own filesystem thread, not the sender.
    let (requests, receiver) = async_channel::unbounded();
    (Gate { requests, timeout }, receiver)
}

impl Gate {
    /// Asks the user, blocking until they answer or the timeout passes.
    /// Denies when the UI is gone, so nothing is ever read without a yes.
    pub fn ask(&self, mapping: Mapping, reader: ReaderInfo, access: Access) -> Decision {
        let deadline = Instant::now() + self.timeout;
        let (answer, decision) = mpsc::sync_channel(1);
        if self.requests.send_blocking(AccessRequest { mapping, reader, access, deadline, answer }).is_err() {
            return Decision::Denied;
        }
        match decision.recv_timeout(self.timeout + BACKSTOP) {
            Ok(decision) => decision,
            Err(mpsc::RecvTimeoutError::Timeout) => Decision::TimedOut,
            Err(mpsc::RecvTimeoutError::Disconnected) => Decision::Denied,
        }
    }
}
