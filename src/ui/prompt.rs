//! The prompt: one request at a time. It posts a notification, and its
//! click opens the window with everything about the reader, which asks.
//! The tray's menu opens it too. Where nothing shows notifications, the
//! window opens straight away. When its time runs out, it's a no.

use std::time::{Duration, Instant};

use mitsuami::prelude::*;

use crate::service::{Answer, PromptRequest};

/// The prompt's one notification.
const NOTIFICATION: &str = "request";

/// The request being asked, and where its answer goes.
#[derive(Clone)]
struct Asking {
    request: PromptRequest,
    answers: async_channel::Sender<Answer>,
}

#[derive(Clone, Copy)]
pub struct Prompt {
    asking: Signal<Option<Asking>>,
    /// The window's open: after the notification's click.
    showing: Signal<bool>,
    /// Whole seconds left, rounded up.
    remaining: Signal<u64>,
    window: NodeRef,
}

impl Prompt {
    /// Made in the app's scope, which its notification's clicks need to
    /// outlive every window.
    pub fn new() -> Prompt {
        let prompt = Prompt { asking: signal(None), showing: signal(false), remaining: signal(0), window: node_ref() };
        on_notification_activated(move |id| {
            if id == NOTIFICATION {
                prompt.show();
            }
        });
        prompt
    }

    /// The question being asked, if one is.
    pub fn question(self) -> Option<String> {
        self.asking.get().map(|a| a.request.title)
    }

    /// Opens the window, or brings it forward, while a question is asked.
    pub fn show(self) {
        if self.asking.get_untracked().is_some() {
            self.showing.set(true);
            self.window.present();
        }
    }

    /// Asks the user about `request`, and waits for the answer, or for its
    /// time to run out.
    pub async fn ask(self, request: PromptRequest) -> Answer {
        // Room for one: the first answer wins, and the rest are dropped.
        let (answers, answered) = async_channel::bounded(1);
        let deadline = request.deadline;
        let notification = Notification::new(request.title.clone())
            .body(format!("{}\nClick to see who's asking, and answer.", request.summary))
            .id(NOTIFICATION);
        self.asking.set(Some(Asking { request, answers: answers.clone() }));
        let remaining = self.remaining;
        let countdown = spawn_local(async move {
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    let _ = answers.try_send(Answer::TimedOut);
                    return;
                }
                remaining.set(left.as_secs() + u64::from(left.subsec_nanos() > 0));
                // Wake when the shown number changes.
                sleep(match left - Duration::from_secs(left.as_secs()) {
                    Duration::ZERO => Duration::from_secs(1),
                    fraction => fraction,
                })
                .await;
            }
        });
        if let Err(e) = notify(notification).await {
            // Nothing to show it (or they're turned off): ask in the window.
            eprintln!("readgate: no notification ({e}), showing the window");
            self.show();
        }
        let answer = answered.recv().await.unwrap_or(Answer::Deny);
        countdown.cancel();
        withdraw_notification(NOTIFICATION);
        self.showing.set(false);
        self.asking.set(None);
        answer
    }

    fn answer(self, answer: Answer) {
        if let Some(asking) = self.asking.get_untracked() {
            let _ = asking.answers.try_send(answer);
        }
    }

    /// The window, which follows the question being asked.
    pub fn window(self) -> Window {
        Window::new("readgate")
            // As tall as what it shows: the details scroll past a cap.
            .size(WindowSize::FitHeight(560.0))
            .open(self.showing)
            .node_ref(self.window)
            // Closing it is a no.
            .on_close_request(move || self.answer(Answer::Deny))
            .content(move || self.content())
    }

    fn content(self) -> impl View {
        let request = move || self.asking.get().map(|a| a.request);
        let title = move || request().map(|r| r.title).unwrap_or_default();
        let details = move || request().map(|r| r.details).unwrap_or_default();
        let can_grant = move || request().is_some_and(|r| r.grant.is_some());
        let has_choices = move || request().is_some_and(|r| !r.grant_choices.is_empty());
        let scope = move || {
            let Some(parent) = request().and_then(|r| r.grant) else { return String::new() };
            let mut scope = format!(
                "Allow for 1 Minute also allows this file to {parent} and whatever it starts, without asking, for \
                 the next minute."
            );
            if has_choices() {
                scope.push_str(" Its menu allows it to a program further up instead, past the shells.");
            }
            scope
        };
        let choices = move || {
            request()
                .map(|r| r.grant_choices)
                .unwrap_or_default()
                .into_iter()
                .map(|c| {
                    MenuItem::new(format!("Allow {} for 1 Minute", c.label))
                        .on_select(move || self.answer(Answer::AllowMinuteTo(c.at)))
                })
                .collect::<Vec<_>>()
        };
        let remaining = self.remaining;
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
                    <Button role=ButtonRole::Cancel @click=move || self.answer(Answer::Deny)>"Deny"</Button>
                    // The parent's grant, and, when there are programs
                    // further up, theirs in its menu.
                    <Show when=move || can_grant() && !has_choices()>
                        <Button @click=move || self.answer(Answer::AllowMinute)>"Allow for 1 Minute"</Button>
                    </Show>
                    <Show when=move || can_grant() && has_choices()>
                        <SplitButton on_click=move || self.answer(Answer::AllowMinute) menu_with=choices>
                            "Allow for 1 Minute"
                        </SplitButton>
                    </Show>
                    <Button @click=move || self.answer(Answer::Allow)>"Allow"</Button>
                </Row>
                <Show when=can_grant>
                    <Text text_style=TextStyle::Caption>{scope}</Text>
                </Show>
            </Column>
        }
    }
}

/// `1:45`, or `42 s` under a minute.
fn countdown(seconds: u64) -> String {
    if seconds >= 60 { format!("{}:{:02}", seconds / 60, seconds % 60) } else { format!("{seconds} s") }
}
