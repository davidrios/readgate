//! The app: the mappings and the log, in tabs. It edits the config and
//! shows what the server does; the server mounts and asks. Opening the app
//! starts the server if it isn't running, and each change to the mappings
//! restarts it, so it reads the new config. Closing the app leaves it
//! running.

mod log;
mod mappings;
pub mod prompt;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::OnceLock;

use mitsuami::prelude::*;
use time::{OffsetDateTime, UtcOffset};

use crate::config::{Config, Mapping, MountSettings};
use crate::ipc::{self, Event, MappingStatus};

/// A mapping as the mappings screen lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct MappingRow {
    pub mapping: Mapping,
    pub settings: MountSettings,
    pub status: Status,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Not heard from the server yet (it's starting or restarting).
    Unknown,
    Mounted,
    /// Why it isn't mounted.
    Failed(String),
}

/// What the link to the server tells the app.
enum Link {
    Event(Event),
    /// The server went away.
    Lost,
    /// It couldn't be started.
    Failed(String),
}

/// What every screen shares. Clones share it too.
#[derive(Clone)]
pub struct State {
    config_path: Rc<PathBuf>,
    config: Rc<RefCell<Config>>,
    pub rows: Signal<Vec<MappingRow>>,
    /// The answers given while the app is open. Earlier ones are in the
    /// log file.
    pub log: log::LogView,
    /// The last thing that went wrong, shown under the tabs until the next
    /// success.
    pub problem: Signal<Option<String>>,
    /// A restart is under way: losing the server is expected.
    restarting: Signal<bool>,
    links: async_channel::Sender<Link>,
}

impl State {
    fn new(config_path: PathBuf, config: Config, links: async_channel::Sender<Link>) -> State {
        let state = State {
            config_path: Rc::new(config_path),
            config: Rc::new(RefCell::new(config)),
            rows: signal(Vec::new()),
            log: log::LogView::new(),
            problem: signal(None),
            restarting: signal(false),
            links,
        };
        state.refresh_rows();
        state
    }

    /// Lists the config's mappings, keeping the status each had.
    fn refresh_rows(&self) {
        let config = self.config.borrow();
        let old = self.rows.get_untracked();
        let rows = config
            .mappings
            .iter()
            .map(|m| MappingRow {
                mapping: m.clone(),
                settings: config.mount_settings(&m.mount),
                status: old.iter().find(|r| r.mapping.id == m.id).map_or(Status::Unknown, |r| r.status.clone()),
            })
            .collect();
        self.rows.set(rows);
    }

    fn set_statuses(&self, statuses: &[MappingStatus]) {
        self.rows.update(|rows| {
            for row in rows {
                row.status = match statuses.iter().find(|s| s.id == row.mapping.id) {
                    Some(MappingStatus { error: None, .. }) => Status::Mounted,
                    Some(MappingStatus { error: Some(e), .. }) => Status::Failed(e.clone()),
                    None => Status::Unknown,
                };
            }
        });
    }

    /// Saves the config, and restarts the server to use it.
    fn save(&self) {
        let result = self.config.borrow().save(&self.config_path);
        self.refresh_rows();
        match result {
            Ok(()) => self.restart(),
            Err(e) => self.problem.set(Some(format!("Couldn't save the mappings: {e:#}"))),
        }
    }

    /// `owner` and `mode` change the mount folder's settings, for all its
    /// mappings; `None` keeps what it has.
    pub fn add(
        &self,
        source: PathBuf,
        mount: PathBuf,
        dest: PathBuf,
        read_only: bool,
        owner: Option<String>,
        mode: Option<String>,
    ) {
        {
            let mut config = self.config.borrow_mut();
            let mut settings = config.mount_settings(&mount);
            settings.owner = owner.or(settings.owner);
            settings.mode = mode.or(settings.mode);
            config.set_mount_settings(settings);
            let id = config.next_id();
            config.mappings.push(Mapping { id, source, mount, dest, read_only });
        }
        self.save();
    }

    pub fn remove(&self, ids: &[u64]) {
        {
            let mut config = self.config.borrow_mut();
            config.mappings.retain(|m| !ids.contains(&m.id));
            config.prune_mounts();
        }
        self.save();
    }

    /// Stops the server and starts it again: after a change, or to mount
    /// what failed once what stopped it is fixed.
    pub fn restart(&self) {
        self.restarting.set(true);
        self.set_statuses(&[]);
        self.follow(true);
    }

    /// Starts the server if it isn't running, and follows it until it goes.
    fn connect(&self) {
        self.follow(false);
    }

    /// On a thread of its own, not a task: a task belongs to the window it
    /// started in, and the Add window closes (cancelling its tasks) as it
    /// adds.
    fn follow(&self, restart: bool) {
        let links = self.links.clone();
        let path = (*self.config_path).clone();
        std::thread::spawn(move || {
            if restart && let Err(e) = ipc::shutdown(&path) {
                let _ = links.send_blocking(Link::Failed(format!("couldn't stop it: {e:#}")));
                return;
            }
            if let Err(e) = ipc::ensure_running(&path) {
                let _ = links.send_blocking(Link::Failed(format!("{e:#}")));
                return;
            }
            let _ = ipc::subscribe(&path, |event| {
                let _ = links.send_blocking(Link::Event(event));
            });
            let _ = links.send_blocking(Link::Lost);
        });
    }

    fn on_link(&self, link: Link) {
        match link {
            Link::Event(Event::Status { mappings }) => {
                self.restarting.set(false);
                self.problem.set(None);
                self.set_statuses(&mappings);
            }
            Link::Event(Event::Answer { entry }) => self.log.push(&entry),
            // A restart's: the new server's status comes next.
            Link::Lost if self.restarting.get_untracked() => {}
            Link::Lost => {
                self.set_statuses(&[]);
                self.problem.set(Some("The server stopped. Mount Again starts it.".into()));
            }
            Link::Failed(e) => {
                self.restarting.set(false);
                self.problem.set(Some(format!("Couldn't start the server: {e}")));
            }
        }
    }
}

static LOCAL_OFFSET: OnceLock<UtcOffset> = OnceLock::new();

/// Reads the local offset while the process has one thread: `time` won't
/// read it later on Linux, where reading it races with `setenv`.
pub fn init_local_offset() {
    let _ = LOCAL_OFFSET.set(UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC));
}

pub fn local_time(time: OffsetDateTime) -> String {
    let format = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    time.to_offset(*LOCAL_OFFSET.get().unwrap_or(&UtcOffset::UTC)).format(&format).unwrap_or_default()
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Pane {
    Mappings,
    Log,
}

pub fn run(config_path: PathBuf, config: Config) {
    // Taken once, by the window: `App::window` wants a factory.
    let setup = RefCell::new(Some((config_path, config)));
    App::new()
        .id("br.com.roboport.readgate")
        .name("readgate")
        .window("readgate", Size::new(900.0, 520.0), move || {
            let (config_path, config) = setup.borrow_mut().take().expect("readgate has one window");
            let (links, linked) = async_channel::unbounded();
            let state = State::new(config_path, config, links);
            let follower = state.clone();
            spawn_local(async move {
                while let Ok(link) = linked.recv().await {
                    follower.on_link(link);
                }
            });
            state.connect();
            root(state)
        })
        .run();
}

fn root(state: State) -> impl View {
    let pane = signal(Pane::Mappings);
    let problem = state.problem;
    view! {
        <Column padding=Spacing::Lg gap=Spacing::Md grow=1.0>
            <Tabs selection=pane grow=1.0>
                <Tab title="Mappings" value=Pane::Mappings padding=Spacing::Md gap=Spacing::Md>
                    {mappings::screen(state.clone())}
                </Tab>
                <Tab title="Log" value=Pane::Log padding=Spacing::Md gap=Spacing::Md>
                    {log::screen(state.log)}
                </Tab>
            </Tabs>
            <Show when=move || problem.get().is_some()>
                <Text>{move || problem.get().unwrap_or_default()}</Text>
            </Show>
        </Column>
    }
}
