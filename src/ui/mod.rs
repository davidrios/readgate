//! The app, which does everything in one process: it mounts the mappings,
//! asks about each request, and keeps running in the tray after its
//! window closes. Its window has the mappings and the log, in tabs. It's
//! one instance per session: launching it again opens the running one's
//! window, and `readgate stop` quits it.

mod log;
mod mappings;
mod prompt;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Instant;

use mitsuami::prelude::*;
use time::{OffsetDateTime, UtcOffset};

use crate::access_log::Decision;
use crate::config::{Config, Mapping, MountSettings};
use crate::gate::{self, AccessRequest, Gate, Requests};
use crate::mount::Mounts;
use crate::service::{self, Grants, PromptRequest};
use prompt::Prompt;

/// A mapping as the mappings screen lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct MappingRow {
    pub mapping: Mapping,
    pub settings: MountSettings,
    pub status: Status,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Mounted,
    /// Why it isn't mounted.
    Failed(String),
}

/// What every screen shares. Clones share it too.
#[derive(Clone)]
pub struct State {
    config_path: Rc<PathBuf>,
    config: Rc<RefCell<Config>>,
    pub rows: Signal<Vec<MappingRow>>,
    /// The answers given since the app started. Earlier ones are in the
    /// log file.
    pub log: log::LogView,
    /// The last thing that went wrong, shown under the tabs until the next
    /// success.
    pub problem: Signal<Option<String>>,
    gate: Gate,
    /// Dropped (unmounting everything) to mount again, and on quitting.
    mounts: Rc<RefCell<Option<Mounts>>>,
    grants: Rc<RefCell<Grants>>,
    prompt: Prompt,
}

impl State {
    /// Mounts the mappings and starts answering their requests.
    fn start(config_path: PathBuf, config: Config, prompt: Prompt) -> State {
        let (gate, requests) = gate::gate(config.prompt_timeout());
        let state = State {
            config_path: Rc::new(config_path),
            config: Rc::new(RefCell::new(config)),
            rows: signal(Vec::new()),
            log: log::LogView::new(),
            problem: signal(None),
            gate,
            mounts: Rc::default(),
            grants: Rc::default(),
            prompt,
        };
        state.remount();
        state.serve(requests);
        state
    }

    /// Unmounts everything and mounts the config's mappings again: after a
    /// change, or to mount what failed once what stopped it is fixed.
    /// Grants and the request being asked about stay.
    pub fn remount(&self) {
        // First: a folder still in the config mounts again at the same
        // path. Each is detached, so it goes even with files open in it.
        drop(self.mounts.borrow_mut().take());
        let config = self.config.borrow();
        let mut mounts = Mounts::new(self.gate.clone());
        let mut errors = HashMap::new();
        for m in &config.mappings {
            if let Err(e) = mounts.mount(m, &config.mount_settings(&m.mount)) {
                eprintln!("readgate: {} isn't mounted: {e}", m.dest_path().display());
                errors.insert(m.id, e);
            }
        }
        *self.mounts.borrow_mut() = Some(mounts);
        let rows = config
            .mappings
            .iter()
            .map(|m| MappingRow {
                mapping: m.clone(),
                settings: config.mount_settings(&m.mount),
                status: errors.remove(&m.id).map_or(Status::Mounted, Status::Failed),
            })
            .collect();
        self.rows.set(rows);
    }

    /// One request at a time: the others wait in line, their readers still
    /// blocked, their time still running. A task of the app's scope, so
    /// it outlives the windows.
    fn serve(&self, requests: Requests) {
        let state = self.clone();
        spawn_local(async move {
            while let Ok(request) = requests.recv().await {
                let decision = state.decide(&request).await;
                let entry = service::record(&state.config.borrow(), &request, decision);
                state.log.push(&entry);
                request.answer(decision);
            }
        });
    }

    async fn decide(&self, request: &AccessRequest) -> Decision {
        if self.grants.borrow_mut().cover(request) {
            return Decision::Granted;
        }
        // Ran out while waiting in line: never shown.
        if request.deadline <= Instant::now() {
            return Decision::TimedOut;
        }
        let answer = self.prompt.ask(PromptRequest::of(request)).await;
        service::decide(&mut self.grants.borrow_mut(), request, answer)
    }

    /// Saves the config, and mounts its mappings.
    fn save(&self) {
        let result = self.config.borrow().save(&self.config_path);
        self.remount();
        match result {
            Ok(()) => self.problem.set(None),
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

    /// Unmounts everything and quits. A reader waiting on a prompt gets an
    /// error as the mount goes.
    fn shutdown(&self) {
        drop(self.mounts.borrow_mut().take());
        quit();
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

/// How the app was started.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Start {
    /// With its window open.
    Open,
    /// In the tray only, as at login.
    Hidden,
    /// To quit the running one: `readgate stop`. Started with none
    /// running, it quits at once.
    Stop,
}

pub fn run(config_path: PathBuf, config: Config, start: Start) {
    // The windows are made one after the other, in the app's scope: the
    // first starts everything, and the prompt's takes the state from it.
    let setup = RefCell::new(Some((config_path, config)));
    let shared = Rc::new(RefCell::new(None::<State>));
    let for_prompt = shared.clone();
    App::new()
        .id("br.com.roboport.readgate")
        .name("readgate")
        .single_instance()
        .runs_without_windows()
        .open(move || {
            let (config_path, config) = setup.borrow_mut().take().expect("readgate starts once");
            if start == Start::Stop {
                quit();
                return Window::new("readgate");
            }
            let state = State::start(config_path, config, Prompt::new());
            *shared.borrow_mut() = Some(state.clone());
            main_window(state, start == Start::Open)
        })
        .open(move || match for_prompt.borrow().as_ref() {
            Some(state) => state.prompt.window(),
            None => Window::new("readgate"),
        })
        .run();
}

/// The mappings and the log. Closing it leaves the app in the tray.
fn main_window(state: State, open_now: bool) -> Window {
    let open = signal(open_now);
    let window = node_ref();
    let show = move || {
        open.set(true);
        window.present();
    };
    {
        let state = state.clone();
        on_reopen(move |reopen| match reopen.args().first().map(String::as_str) {
            Some("stop") => state.shutdown(),
            // Another login's start: it's running already.
            Some("--hidden") => {}
            _ => show(),
        });
    }
    set_tray(tray(state.clone(), show));
    Window::new("readgate")
        .size(Size::new(900.0, 520.0))
        .bind(open)
        .node_ref(window)
        .content(move || root(state.clone()))
}

/// The icon says whether a read is waiting, and its menu opens the
/// window, or the prompt while one is.
fn tray(state: State, show: impl Fn() + 'static) -> TrayIcon {
    let prompt = state.prompt;
    let question = move || prompt.question();
    let remount = state.clone();
    TrayIcon::new("readgate")
        .icon(move || {
            AppIcon::named(if question().is_some() { "dialog-question-symbolic" } else { "changes-prevent-symbolic" })
        })
        .tooltip(move || question().unwrap_or_else(|| "readgate: no reads waiting".into()))
        .menu(
            Menu::new("").children((
                MenuItem::new(move || question().unwrap_or_default())
                    .visible(move || question().is_some())
                    .on_select(move || prompt.show()),
                MenuItem::new("Open readgate").on_select(show),
                MenuItem::new("Mount Again").on_select(move || remount.remount()),
                MenuSeparator,
                MenuItem::new("Quit").role(MenuRole::Quit).on_select(move || state.shutdown()),
            )),
        )
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
