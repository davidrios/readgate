//! `readgate prompt`: one request, started by the server. It reads the
//! request as a JSON line on stdin and prints its answer on stdout: `allow`,
//! `deny`, or `timeout` when its time runs out. Ending without an answer is
//! taken as a no.
//!
//! It posts a notification and waits, with no window, until it's clicked:
//! then the window shows everything about the reader, and asks. A click
//! only reaches the process that posted the notification, so this one runs
//! until it's answered or its time is up. Where nothing shows notifications,
//! the window opens straight away.

use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use mitsuami::prelude::*;
use serde::{Deserialize, Serialize};

/// The prompt's one notification.
const NOTIFICATION: &str = "request";

#[derive(Debug, Serialize, Deserialize)]
pub struct PromptRequest {
    /// The question: "Allow cat to read notes.txt?".
    pub title: String,
    /// Who's asking, in a line, for the notification.
    #[serde(default)]
    pub summary: String,
    /// Everything about the reader, for the window.
    pub details: String,
    /// How long it has to answer.
    pub seconds: f64,
    /// The reader's parent, named, for "Allow for 1 Minute": unset when
    /// it has none known, and then there's no such button.
    #[serde(default)]
    pub grant: Option<String>,
    /// The first program up the chain that isn't a shell, named, for the
    /// button's other choice: unset when that's the parent, or there's
    /// none.
    #[serde(default)]
    pub grant_program: Option<String>,
}

pub fn run() -> anyhow::Result<()> {
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let request: PromptRequest = serde_json::from_str(&line)?;
    let deadline = Instant::now() + Duration::from_secs_f64(request.seconds.max(0.0));
    let request = std::cell::RefCell::new(Some(request));
    App::new()
        .id("br.com.roboport.readgate")
        .name("readgate")
        .runs_without_windows()
        .open(move || prompt(request.borrow_mut().take().expect("the prompt opens once"), deadline))
        .run();
    Ok(())
}

/// Prints the answer for the server, takes the notification away, and
/// ends.
fn answer(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
    withdraw_notification(NOTIFICATION);
    quit();
}

fn prompt(request: PromptRequest, deadline: Instant) -> Window {
    let showing = signal(false);
    // Whole seconds left, rounded up.
    let remaining = signal(0u64);
    spawn_local(async move {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                answer("timeout");
                return;
            }
            remaining.set(left.as_secs() + u64::from(left.subsec_nanos() > 0));
            // Wake when the shown number changes.
            let tick = match left - Duration::from_secs(left.as_secs()) {
                Duration::ZERO => Duration::from_secs(1),
                fraction => fraction,
            };
            sleep(tick).await;
        }
    });

    on_notification_activated(move |id| {
        if id == NOTIFICATION {
            showing.set(true);
        }
    });
    let body = format!("{}\nClick to see who's asking, and answer.", request.summary);
    let notification = Notification::new(request.title.clone()).body(body).id(NOTIFICATION);
    spawn_local(async move {
        if let Err(e) = notify(notification).await {
            // Nothing to show it (or they're turned off): ask in the window.
            eprintln!("readgate: no notification ({e}), showing the window");
            showing.set(true);
        }
    });

    let PromptRequest { title, details, grant, grant_program, .. } = request;
    Window::new("readgate")
        // As tall as what it shows: the details scroll past a cap.
        .size(WindowSize::FitHeight(560.0))
        .open(showing)
        // Closing it is a no.
        .on_close_request(|| answer("deny"))
        .content(move || {
            let (title, details, grant) = (title.clone(), details.clone(), grant.clone());
            let program = grant_program.clone();
            let has_program = program.is_some();

            let scope = grant.clone().map(|parent| {
                let mut scope = format!(
                    "Allow for 1 Minute also allows this file to {parent} and whatever it starts, \
                     without asking, for the next minute."
                );
                if let Some(program) = &program {
                    scope.push_str(&format!(" Its menu allows it to {program} instead, past the shells."));
                }
                scope
            });
            let can_grant = grant.is_some();
            view! {
                <Column padding=Spacing::Xl gap=Spacing::Lg grow=1.0>
                    <Text text_style=TextStyle::Headline selectable=true>{title}</Text>
                    // Up to a cap, then scrolled: the buttons stay in view
                    // however long the details are. Selectable, to copy a
                    // path or a pid.
                    <ScrollView max_height=400>
                        <Text selectable=true>{details}</Text>
                    </ScrollView>
                    <Row gap=Spacing::Sm align=Align::Center>
                        <Text grow=1.0 text_style=TextStyle::Caption>
                            {move || format!("Denies in {}", countdown(remaining.get()))}
                        </Text>
                        // No default button: Return shouldn't allow by
                        // accident.
                        <Button role=ButtonRole::Cancel @click=|| answer("deny")>"Deny"</Button>
                        // The parent's grant, and, when there's one past the
                        // shells, that program's in its menu.
                        <Show when=move || can_grant && !has_program>
                            <Button @click=|| answer("allow-minute")>"Allow for 1 Minute"</Button>
                        </Show>
                        <Show when=move || can_grant && has_program>
                            <SplitButton
                                on_click=|| answer("allow-minute")
                                menu=MenuItem::new(format!(
                                    "Allow {} for 1 Minute",
                                    program.clone().unwrap_or_default()
                                ))
                                .on_select(|| answer("allow-minute-program"))
                            >"Allow for 1 Minute"</SplitButton>
                        </Show>
                        <Button @click=|| answer("allow")>"Allow"</Button>
                    </Row>
                    <Show when=move || can_grant>
                        <Text text_style=TextStyle::Caption>{scope.clone().unwrap_or_default()}</Text>
                    </Show>
                </Column>
            }
        })
}

/// `1:45`, or `42 s` under a minute.
fn countdown(seconds: u64) -> String {
    if seconds >= 60 { format!("{}:{:02}", seconds / 60, seconds % 60) } else { format!("{seconds} s") }
}
