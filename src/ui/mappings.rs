//! The mappings screen: each source and where it's mounted, Add and
//! Delete.

use std::path::PathBuf;

use mitsuami::prelude::*;

use super::{MappingRow, State, Status};
use crate::perms::DEFAULT_MODE;

/// Status and owner change while a row stays (the table keeps a row's
/// cells while its key does), so those cells read the row's latest from
/// `rows`.
fn columns(rows: Signal<Vec<MappingRow>>) -> Vec<TableColumn<MappingRow>> {
    let latest = move |id: u64| rows.get().into_iter().find(|r| r.mapping.id == id);
    vec![
        TableColumn::new("Source", |r: MappingRow| Text::new(r.mapping.source.display().to_string()).max_lines(1))
            .expand(),
        TableColumn::new("Mounted at", |r: MappingRow| {
            Text::new(r.mapping.dest_path().display().to_string()).max_lines(1)
        })
        .expand(),
        TableColumn::new("Access", |r: MappingRow| {
            Text::new(if r.mapping.read_only { "Read-only" } else { "Read & write" }).max_lines(1)
        })
        .width(110),
        // The mount folder's, which all its mappings share.
        TableColumn::new("Owner and mode", move |r: MappingRow| {
            let id = r.mapping.id;
            Text::new(move || {
                let settings = latest(id).map(|r| r.settings).unwrap_or_default();
                let owner = settings.owner.unwrap_or_else(|| "you".into());
                let mode = settings.mode.unwrap_or_else(|| format!("{DEFAULT_MODE:04o}"));
                format!("{owner}, {mode}")
            })
            .max_lines(1)
        })
        .width(150),
        TableColumn::new("Status", move |r: MappingRow| {
            let id = r.mapping.id;
            // The reason is below the table, for the selected one.
            Text::new(move || {
                match latest(id).map(|r| r.status) {
                    Some(Status::Mounted) => "Mounted",
                    Some(Status::Failed(_)) => "Not mounted",
                    _ => "…",
                }
                .to_string()
            })
            .max_lines(1)
        })
        .width(110),
    ]
}

pub fn screen(state: State) -> impl View {
    let rows = state.rows;
    let selected = signal(Vec::<u64>::new());
    let adding = signal(false);
    let delete = {
        let state = state.clone();
        move || {
            let ids = selected.get_untracked();
            selected.set(Vec::new());
            state.remove(&ids);
        }
    };
    // Restarts the server, which mounts everything again.
    let retry = {
        let state = state.clone();
        move || state.restart()
    };
    let can_retry = move || rows.get().iter().any(|r| r.status != Status::Mounted);
    // Why the selected mappings aren't mounted.
    let problems = move || {
        let selected = selected.get();
        rows.get()
            .into_iter()
            .filter(|r| selected.contains(&r.mapping.id))
            .filter_map(|r| match r.status {
                Status::Failed(e) => Some(format!("{}: {e}", r.mapping.dest_path().display())),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    view! {
        <Column gap=Spacing::Md grow=1.0>
            <Table
                each=move || rows.get()
                key=|r: &MappingRow| r.mapping.id
                columns=columns(rows)
                selection_mode=SelectionMode::Multiple
                selected=selected
                list_style=ListStyle::Framed
                grow=1.0
            />
            <Show when=move || !problems().is_empty()>
                <Text text_style=TextStyle::Caption>{problems}</Text>
            </Show>
            <Row gap=Spacing::Sm justify=Justify::End>
                <Button enabled=can_retry @click=retry>"Mount Again"</Button>
                <Button enabled=move || !selected.get().is_empty() @click=delete>"Delete"</Button>
                <Button @click=move || adding.set(true)>"Add…"</Button>
            </Row>
            {add_window(state, adding)}
        </Column>
    }
}

/// Picks a source, a mount folder and a dest inside it, typed or chosen.
fn add_window(state: State, open: Signal<bool>) -> impl View {
    let rows = state.rows;
    let source = signal(String::new());
    let mount = signal(String::new());
    let dest = signal(String::new());
    let owner = signal(String::new());
    let mode = signal(String::new());
    let read_only = signal(true);
    // The folder's owner and mode, if a mapping uses it already.
    let load_settings = move |folder: &str| {
        let known =
            rows.get_untracked().into_iter().find(|r| r.mapping.mount.as_path() == std::path::Path::new(folder.trim()));
        let settings = known.map(|r| r.settings).unwrap_or_default();
        owner.set(settings.owner.unwrap_or_default());
        mode.set(settings.mode.unwrap_or_default());
    };
    // A mode that doesn't parse is caught here, before the server would
    // refuse the folder.
    let mode_error = move || {
        let text = mode.get();
        (!text.trim().is_empty()).then(|| crate::perms::parse_mode(&text).err().map(|e| format!("{e:#}"))).flatten()
    };
    let choose_source = move || {
        spawn_local(async move {
            if let Some(path) =
                open_file(OpenFile::new().title("Choose the file to share")).await.and_then(|p| p.into_iter().next())
            {
                // The dest starts as the source's name.
                if dest.get_untracked().trim().is_empty()
                    && let Some(name) = path.file_name()
                {
                    dest.set(name.to_string_lossy().into_owned());
                }
                source.set(path.display().to_string());
            }
        });
    };
    let choose_mount = move || {
        spawn_local(async move {
            let request = OpenFile::new().title("Choose the folder to mount").directories();
            if let Some(path) = open_file(request).await.and_then(|p| p.into_iter().next()) {
                mount.set(path.display().to_string());
                load_settings(&path.display().to_string());
            }
        });
    };
    let filled = |s: Signal<String>| move || !s.get().trim().is_empty();
    let ready = move || filled(source)() && filled(mount)() && filled(dest)() && mode_error().is_none();
    let full_dest = move || {
        let path = PathBuf::from(mount.get().trim()).join(dest.get().trim());
        format!("Other programs open it at {}.", path.display())
    };
    let given = |s: Signal<String>| Some(s.get_untracked().trim().to_owned()).filter(|t| !t.is_empty());
    let add = move || {
        state.add(
            PathBuf::from(source.get_untracked().trim()),
            PathBuf::from(mount.get_untracked().trim()),
            PathBuf::from(dest.get_untracked().trim()),
            read_only.get_untracked(),
            given(owner),
            given(mode),
        );
        open.set(false);
    };
    view! {
        <Window title="Add Mapping" size=WindowSize::FollowHeight(560.0) modal=Modality::Window bind=open @open=move || {
            source.set(String::new());
            // The mount folder last used: mappings usually share one.
            let last = rows.get_untracked().last().map(|r| r.mapping.mount.display().to_string()).unwrap_or_default();
            load_settings(&last);
            mount.set(last);
            dest.set(String::new());
            read_only.set(true);
        }>
            <Column padding=Spacing::Xl gap=Spacing::Lg>
                <Text>"Source: the file to share. Only readgate reads it, as you."</Text>
                <Row gap=Spacing::Sm align=Align::Center>
                    <TextInput a11y_label="Source" placeholder="/home/you/secret.txt" bind=source grow=1.0/>
                    <Button @click=choose_source>"Choose…"</Button>
                </Row>
                <Text>"Mount folder: readgate's own folder, empty until mounted. Mappings can share one."</Text>
                <Row gap=Spacing::Sm align=Align::Center>
                    <TextInput a11y_label="Mount folder" placeholder="/home/shared/gate" bind=mount grow=1.0/>
                    <Button @click=choose_mount>"Choose…"</Button>
                </Row>
                <Text>"Owner and mode of the files in the mount folder, for all its mappings. Folders get execute where there's read."</Text>
                <Row gap=Spacing::Sm align=Align::Center>
                    <TextInput a11y_label="Owner" placeholder="you (or user:group)" bind=owner grow=1.0/>
                    <TextInput a11y_label="Mode" placeholder="0666" bind=mode width=120/>
                </Row>
                <Show when=move || mode_error().is_some()>
                    <Text text_style=TextStyle::Caption>{move || mode_error().unwrap_or_default()}</Text>
                </Show>
                <Text>"Path inside it: where the file shows, in subfolders if you like."</Text>
                <TextInput a11y_label="Path inside the mount folder" placeholder="keys/secret.txt" bind=dest/>
                <Show when=ready>
                    <Text text_style=TextStyle::Caption>{full_dest}</Text>
                </Show>
                <Checkbox bind=read_only>"Read-only"</Checkbox>
                <Text text_style=TextStyle::Caption>
                    {move || if read_only.get() {
                        "Writes to it fail."
                    } else {
                        "Writes go to the source. Opening it for writing asks you first, as reading does."
                    }
                    .to_string()}
                </Text>
                <Row gap=Spacing::Sm justify=Justify::End>
                    <Button role=ButtonRole::Cancel @click=move || open.set(false)>"Cancel"</Button>
                    <Button role=ButtonRole::Default enabled=ready @click=add.clone()>"Add"</Button>
                </Row>
            </Column>
        </Window>
    }
}
