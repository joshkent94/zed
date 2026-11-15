use super::{ListEntry, Sidebar, connect_remote, workspace_path_list};
use agent_client_protocol::v2::schema::ContentBlock;
use agent_ui::{
    Agent, AgentPanel, ConversationView, MessageEditor, ProjectError, remap_mention_paths,
};
use anyhow::{Context as _, Result, anyhow, ensure};
use editor::Editor;
use git_ui_core::worktree_service::remap_worktree_path;
use gpui::{
    App, AsyncWindowContext, Context, DismissEvent, Entity, EventEmitter, ExternalPaths,
    FocusHandle, Focusable, PathPromptOptions, SharedString, Subscription, Task, TaskExt as _,
    WeakEntity, Window, prelude::*,
};
use std::{path::Path, time::Duration};
use ui::{
    CommonAnimationExt as _, ContextMenu, DropdownMenu, DropdownStyle, PopoverMenu, TintColor,
    Tooltip, prelude::*,
};
use util::{ResultExt as _, path_list::PathList};
use workspace::{ModalView, MultiWorkspace, OpenMode, ProjectGroupKey, RemovalIntent, Workspace};

/// Long enough that the loading state does not flash by when a session starts quickly.
const MINIMUM_LOAD: Duration = Duration::from_secs(1);

struct ProjectChoice {
    key: ProjectGroupKey,
    label: SharedString,
    worktrees: Vec<(PathList, SharedString)>,
}

fn projects(sidebar: &Sidebar, owner: &MultiWorkspace, cx: &App) -> Vec<ProjectChoice> {
    let contents = sidebar
        .collect_contents(owner, String::new(), true, Default::default(), cx)
        .contents;
    let mut projects = vec![ProjectChoice {
        key: no_project_key(),
        label: "No Project".into(),
        worktrees: vec![(PathList::default(), "None".into())],
    }];
    for entry in contents.entries {
        match entry {
            ListEntry::ProjectHeader { key, label, .. } => {
                projects.push(ProjectChoice {
                    label: key
                        .host()
                        .map(|host| format!("{label} ({})", host.display_name()).into())
                        .unwrap_or(label),
                    key,
                    worktrees: Vec::new(),
                });
            }
            ListEntry::WorktreeHeader {
                roots,
                label,
                branch,
                extra_worktrees,
                ..
            } => {
                if let Some(project) = projects.last_mut() {
                    let paths = paths_from_header(&roots, &project.key);
                    if project.worktrees.iter().any(|(other, _)| other == &paths) {
                        continue;
                    }
                    let label = std::iter::once((label, branch))
                        .chain(extra_worktrees)
                        .map(|(label, branch)| match branch {
                            Some(branch) if branch != label => format!("{label} ({branch})"),
                            _ => label.to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(" / ");
                    project.worktrees.push((paths, label.into()));
                }
            }
            _ => {}
        }
    }
    for workspace in owner.workspaces() {
        let key = owner.project_group_key_for_workspace(workspace, cx);
        let paths = workspace_path_list(workspace, cx);
        if let Some(project) = projects.iter_mut().find(|project| project.key == key)
            && !paths.is_empty()
            && !project.worktrees.iter().any(|(other, _)| other == &paths)
        {
            let label = super::workspace_menu_worktree_labels(workspace, cx)
                .iter()
                .map(|label| label.text())
                .collect::<Vec<_>>()
                .join(" / ");
            project.worktrees.push((paths, label.into()));
        }
    }
    projects
}

/// Changes whenever the project choices may have.
fn projects_fingerprint(sidebar: &Sidebar, owner: &MultiWorkspace, cx: &App) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for entry in &sidebar.contents.entries {
        match entry {
            ListEntry::ProjectHeader { key, label, .. } => (key, label).hash(&mut hasher),
            ListEntry::WorktreeHeader {
                roots,
                label,
                branch,
                extra_worktrees,
                is_collapsed,
                ..
            } => (roots, label, branch, extra_worktrees, is_collapsed).hash(&mut hasher),
            _ => {}
        }
    }
    for workspace in owner.workspaces() {
        workspace.entity_id().hash(&mut hasher);
        workspace_path_list(workspace, cx).paths().hash(&mut hasher);
    }
    // A collapsed project's worktrees are not among the sidebar's entries, so the listings they come from are hashed too.
    let hash_one = |hash: &dyn Fn(&mut std::collections::hash_map::DefaultHasher)| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        hash(&mut hasher);
        hasher.finish()
    };
    let mut listings = Vec::new();
    for (key, worktrees) in &sidebar.closed_group_worktrees {
        listings.push(hash_one(&|hasher| (key, worktrees).hash(hasher)));
    }
    for (key, paths) in &sidebar.multi_root_worktree_groups {
        listings.push(hash_one(&|hasher| {
            (key, paths.folder_path_list().paths()).hash(hasher)
        }));
    }
    for workspace in sidebar.all_workspaces(cx) {
        let project = workspace.read(cx).project().read(cx);
        for repository in project.repositories(cx).values() {
            let snapshot = repository.read(cx).snapshot();
            listings.push(hash_one(&|hasher| {
                snapshot.work_directory_abs_path.hash(hasher);
                snapshot
                    .branch
                    .as_ref()
                    .map(|branch| branch.name())
                    .hash(hasher);
                snapshot.linked_worktrees().hash(hasher);
            }));
        }
    }
    // The maps iterate in no fixed order.
    listings.sort_unstable();
    listings.hash(&mut hasher);
    hasher.finish()
}

fn paths_from_header(roots: &[SharedString], key: &ProjectGroupKey) -> PathList {
    if roots.iter().all(|root| root.is_empty()) {
        key.path_list().clone()
    } else {
        PathList::new(
            &roots
                .iter()
                .map(|root| Path::new(root.as_ref()))
                .collect::<Vec<_>>(),
        )
    }
}

fn no_project_key() -> ProjectGroupKey {
    ProjectGroupKey::new(None, PathList::default())
}

/// The form field an error message is shown next to.
#[derive(Clone, Copy, PartialEq)]
enum Field {
    Project,
    Worktree,
}

impl Field {
    fn of(error: &anyhow::Error) -> Self {
        if error.is::<ProjectError>() {
            Self::Project
        } else {
            Self::Worktree
        }
    }
}

#[derive(Clone)]
struct Destination {
    project: ProjectGroupKey,
    /// The base worktree, when `new_worktree` is set.
    paths: PathList,
    /// Create a worktree for the thread when it is created, so a cancelled form leaves none behind.
    new_worktree: bool,
}

fn default_paths(key: &ProjectGroupKey, owner: &MultiWorkspace, cx: &App) -> PathList {
    owner
        .last_active_workspace_for_group(key, cx)
        .map(|workspace| workspace_path_list(&workspace, cx))
        .unwrap_or_else(|| key.path_list().clone())
}

fn initial_destination(
    sidebar: &Sidebar,
    owner: &MultiWorkspace,
    use_selection: bool,
    cx: &App,
) -> Destination {
    if use_selection
        && let Some(index) = sidebar.selection
        && let Some(project) = sidebar.selected_group_key()
        && let Some(entry) = sidebar.contents.entries.get(index)
    {
        let paths = match entry {
            ListEntry::Thread(thread) => thread.metadata.folder_paths().clone(),
            ListEntry::Terminal(terminal) => terminal.metadata.folder_paths().clone(),
            ListEntry::WorktreeHeader { roots, .. } => paths_from_header(roots, &project),
            ListEntry::ProjectHeader { .. } => default_paths(&project, owner, cx),
        };
        return Destination {
            project,
            paths,
            new_worktree: false,
        };
    }
    if use_selection && sidebar.projectless_thread_selected() {
        return Destination {
            project: no_project_key(),
            paths: PathList::default(),
            new_worktree: false,
        };
    }
    let workspace = owner.workspace();
    Destination {
        project: owner.project_group_key_for_workspace(workspace, cx),
        paths: workspace_path_list(workspace, cx),
        new_worktree: false,
    }
}

impl Sidebar {
    /// Shows the thread the form created in `workspace`, which may sit in a collapsed group or off screen.
    fn reveal_created_thread(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        let key = workspace.read(cx).project_group_key(cx);
        if key.path_list().is_empty() {
            self.set_projectless_collapsed(false, cx);
        } else {
            self.set_group_expanded(&key, true, cx);
        }
        self.expand_worktree_groups_for(workspace, cx);
        let workspace = workspace.clone();
        let this = cx.entity();
        // The thread is only listed once the effects of creating it have run.
        cx.defer(move |cx| {
            this.update(cx, |this, cx| {
                this.update_entries(cx);
                let created = workspace
                    .read(cx)
                    .panel::<AgentPanel>(cx)
                    .and_then(|panel| panel.read(cx).active_thread_id(cx));
                let index = this.contents.entries.iter().position(|entry| {
                    matches!(entry, ListEntry::Thread(thread) if Some(thread.metadata.thread_id) == created)
                });
                if let Some(index) = index {
                    this.scroll_to_reveal_entry(index);
                }
            });
        });
    }

    pub fn most_recent_workspace_window(cx: &App) -> Option<gpui::WindowHandle<MultiWorkspace>> {
        cx.windows()
            .into_iter()
            .filter_map(|window| window.downcast::<MultiWorkspace>())
            .max_by_key(|window| {
                let sidebar = window
                    .read(cx)
                    .ok()?
                    .sidebar()?
                    .to_any()
                    .downcast::<Sidebar>()
                    .ok()?;
                sidebar.read(cx).window_last_active
            })
    }
}

pub fn open_new_thread_modal(use_focused_row: bool, window: &mut Window, cx: &mut App) {
    let Some(owner) = window.root::<MultiWorkspace>().flatten() else {
        return;
    };
    let multi_workspace = owner.read(cx);
    let Some(sidebar) = multi_workspace
        .sidebar()
        .and_then(|sidebar| sidebar.to_any().downcast::<Sidebar>().ok())
    else {
        return;
    };
    if sidebar.read(cx).creating_prompted_thread {
        return;
    }
    if let Some(modal) = multi_workspace.active_modal::<NewThreadModal>(cx) {
        modal.update(cx, |modal, cx| modal.prompt_focus.focus(window, cx));
        return;
    }
    let sidebar_ref = sidebar.read(cx);
    let use_selection = use_focused_row
        && matches!(sidebar_ref.view, super::SidebarView::ThreadList)
        && sidebar_ref.focus_handle.contains_focused(window, cx);
    let destination = initial_destination(sidebar_ref, multi_workspace, use_selection, cx);
    let agent = multi_workspace
        .workspace()
        .read(cx)
        .panel::<AgentPanel>(cx)
        .map(|panel| panel.read(cx).selected_agent(cx))
        .unwrap_or_default();
    show_form(
        sidebar,
        owner,
        agent,
        Some(destination),
        String::new(),
        None,
        None,
        window,
        cx,
    );
}

fn show_form(
    sidebar: Entity<Sidebar>,
    owner: Entity<MultiWorkspace>,
    agent: Agent,
    destination: Option<Destination>,
    text: String,
    content: Option<Vec<ContentBlock>>,
    error: Option<(Field, String)>,
    window: &mut Window,
    cx: &mut App,
) {
    // Listing the projects reads the workspace, which is being updated while the form is built.
    let choices = projects(sidebar.read(cx), owner.read(cx), cx);
    let fingerprint = projects_fingerprint(sidebar.read(cx), owner.read(cx), cx);
    let workspace = owner.read(cx).workspace().clone();
    workspace.update(cx, |workspace, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            let prompt = cx.new(|cx| {
                let mut editor = Editor::auto_height(3, 9, window, cx);
                editor.set_placeholder_text("Something to get you going...", window, cx);
                editor.set_text(text.clone(), window, cx);
                editor
            });
            let project_focus = cx.focus_handle().tab_index(1).tab_stop(true);
            let prompt_focus = prompt.focus_handle(cx).tab_index(3).tab_stop(true);
            let observation = cx.observe(&sidebar, |this: &mut NewThreadModal, _, cx| {
                this.refresh_projects(cx);
                cx.notify();
            });
            let window_handle = window.window_handle();
            let release = cx.on_release(move |this: &mut NewThreadModal, cx| {
                let Some(workspace) = this.opened_workspace.take() else {
                    return;
                };
                let Some(owner) = this.sidebar.read(cx).multi_workspace.upgrade() else {
                    return;
                };
                cx.defer(move |cx| {
                    window_handle
                        .update(cx, |_, window, cx| {
                            close_background_workspace(&owner, workspace, window, cx)
                        })
                        .log_err();
                });
            });
            let mut modal = NewThreadModal {
                sidebar,
                projects: choices,
                projects_fingerprint: fingerprint,
                agent,
                destination,
                error,
                restore: content.map(|content| (text, content)),
                prompt,
                project_focus,
                prompt_focus,
                prepared: None,
                opened_workspace: None,
                preparing: false,
                session_settled: false,
                minimum_load_elapsed: false,
                minimum_load: Task::ready(()),
                prepare_task: Task::ready(()),
                _subscriptions: [observation, release],
            };
            modal.retarget(window, cx);
            modal
        })
    });
}

struct NewThreadModal {
    sidebar: Entity<Sidebar>,
    projects: Vec<ProjectChoice>,
    projects_fingerprint: u64,
    agent: Agent,
    destination: Option<Destination>,
    error: Option<(Field, String)>,
    /// The prompt's mentions and images, which the plain prompt cannot hold, and the text they belong to.
    restore: Option<(String, Vec<ContentBlock>)>,
    prompt: Entity<Editor>,
    project_focus: FocusHandle,
    prompt_focus: FocusHandle,
    prepared: Option<Prepared>,
    /// A workspace opened only to prepare a session, closed again unless the thread is created.
    opened_workspace: Option<Entity<Workspace>>,
    preparing: bool,
    session_settled: bool,
    minimum_load_elapsed: bool,
    minimum_load: Task<()>,
    prepare_task: Task<()>,
    _subscriptions: [Subscription; 2],
}

/// A session started before the thread is created, so the prompt can offer
/// the agent's commands and skills. Dropping it closes the session.
struct Prepared {
    view: Entity<ConversationView>,
    workspace: Entity<Workspace>,
    editor: Option<Entity<MessageEditor>>,
    _observation: Subscription,
}

pub(super) fn close_background_workspace(
    owner: &Entity<MultiWorkspace>,
    workspace: Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    if owner.read(cx).workspace() == &workspace {
        return;
    }
    owner
        .update(cx, |owner, cx| {
            owner.remove([workspace], RemovalIntent::KeepProject, window, cx)
        })
        .detach_and_log_err(cx);
}

/// Opening a project whose folders are all gone falls back to the project's own folders, activating a workspace that is already open.
async fn local_folders_exist(
    owner: &Entity<MultiWorkspace>,
    paths: &PathList,
    cx: &mut AsyncWindowContext,
) -> bool {
    let fs = owner.read_with(cx, |owner, cx| {
        owner.workspace().read(cx).app_state().fs.clone()
    });
    for path in paths.paths() {
        if !fs.is_dir(path).await {
            return false;
        }
    }
    true
}

async fn prepare(
    modal: &WeakEntity<NewThreadModal>,
    owner: &Entity<MultiWorkspace>,
    destination: &Destination,
    agent: Agent,
    cx: &mut AsyncWindowContext,
) -> Result<Option<(Entity<ConversationView>, Entity<Workspace>)>> {
    let key = &destination.project;
    let paths = &destination.paths;
    let existing = owner.read_with(cx, |owner, cx| {
        owner.workspace_for_paths(paths, key.host(), cx)
    });
    let workspace = match existing {
        Some(workspace) => workspace,
        // Connecting to a host shows its own modal, which would replace this one.
        None if key.host().is_some() => return Ok(None),
        None => {
            if !local_folders_exist(owner, paths, cx).await {
                return Ok(None);
            }
            let open_workspaces = owner.read_with(cx, |owner, _| {
                owner.workspaces().cloned().collect::<Vec<_>>()
            });
            let focused = cx.update(|window, cx| window.focused(cx))?;
            let workspace = owner
                .update_in(cx, |owner, window, cx| {
                    owner.find_or_create_local_workspace(
                        paths.clone(),
                        Some(key.clone()),
                        None,
                        OpenMode::Add,
                        None,
                        window,
                        cx,
                    )
                })?
                .await?;
            // Only a workspace this form opened is its to close.
            if !open_workspaces.contains(&workspace) {
                modal.update(cx, |modal, _| {
                    modal.opened_workspace = Some(workspace.clone())
                })?;
            }
            // A new workspace takes focus even when it is not displayed.
            if let Some(focused) = focused {
                cx.update(|window, cx| focused.focus(window, cx))?;
            }
            workspace
        }
    };
    if workspace.read_with(cx, |workspace, cx| {
        PathList::new(&workspace.root_paths(cx)) != *paths
    }) {
        return Ok(None);
    }
    if let Some(task) = workspace.update(cx, |workspace, _| workspace.take_panels_task()) {
        task.await?;
    }
    let panel = Sidebar::ensure_agent_panel(&workspace.downgrade(), cx).await?;
    let view = panel.update_in(cx, |panel, window, cx| {
        panel.prepare_thread(agent, paths.clone(), window, cx)
    })??;
    Ok(Some((view, workspace)))
}

impl EventEmitter<DismissEvent> for NewThreadModal {}
impl ModalView for NewThreadModal {}
impl Focusable for NewThreadModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        if self.worktree_label().is_some() {
            self.prompt_focus.clone()
        } else {
            self.project_focus.clone()
        }
    }
}

impl NewThreadModal {
    /// The sidebar notifies on every change to any thread, and listing the projects rebuilds all of its entries.
    fn refresh_projects(&mut self, cx: &App) {
        let sidebar = self.sidebar.read(cx);
        let Some(owner) = sidebar.multi_workspace.upgrade() else {
            return;
        };
        let owner = owner.read(cx);
        let fingerprint = projects_fingerprint(sidebar, owner, cx);
        if self.projects_fingerprint != fingerprint {
            self.projects_fingerprint = fingerprint;
            self.projects = projects(sidebar, owner, cx);
        }
    }

    fn project(&self) -> Option<&ProjectChoice> {
        let destination = self.destination.as_ref()?;
        self.projects
            .iter()
            .find(|project| project.key == destination.project)
    }

    fn worktree_label(&self) -> Option<SharedString> {
        let destination = self.destination.as_ref()?;
        let project = self.project()?;
        if destination.new_worktree {
            return Some("New Worktree".into());
        }
        project
            .worktrees
            .iter()
            .find(|(paths, _)| *paths == destination.paths)
            .map(|(_, label)| label.clone())
    }

    fn prompt_text(&self, cx: &App) -> String {
        match self
            .prepared
            .as_ref()
            .and_then(|prepared| prepared.editor.as_ref())
        {
            Some(editor) => editor.read(cx).text(cx),
            None => self.prompt.read(cx).text(cx),
        }
    }

    fn insert_dropped_paths(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(prepared) = self.prepared.as_ref() else {
            return;
        };
        let Some(editor) = prepared.editor.clone() else {
            return;
        };
        let project = prepared.workspace.read(cx).project().clone();
        let tasks = paths
            .paths()
            .iter()
            .map(|path| Workspace::project_path_for_path(project.clone(), path, false, cx))
            .collect::<Vec<_>>();
        cx.spawn_in(window, async move |_this, cx| {
            let mut paths = vec![];
            let mut added_worktrees = vec![];
            for task in tasks {
                if let Some((worktree, project_path)) = task.await.log_err() {
                    added_worktrees.push(worktree);
                    paths.push(project_path);
                }
            }
            editor
                .update_in(cx, |editor, window, cx| {
                    editor.insert_dragged_files(paths, added_worktrees, window, cx);
                    editor.focus_handle(cx).focus(window, cx);
                })
                .log_err();
        })
        .detach();
    }

    fn set_agent(&mut self, agent: Agent, window: &mut Window, cx: &mut Context<Self>) {
        self.agent = agent;
        self.retarget(window, cx);
    }

    fn set_destination(
        &mut self,
        destination: Destination,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.destination = Some(destination);
        self.error = None;
        self.retarget(window, cx);
    }

    /// The prompt and thread creation wait for the session, so that the
    /// prompt always offers the agent's commands and skills.
    fn set_preparing(&mut self, preparing: bool, cx: &mut Context<Self>) {
        self.preparing = preparing;
        self.prompt
            .update(cx, |prompt, _| prompt.set_read_only(preparing));
        cx.notify();
    }

    /// The session has started, failed, or cannot be started ahead of the thread.
    fn settle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.session_settled = true;
        if self.minimum_load_elapsed {
            self.finish_preparing(window, cx);
        }
    }

    fn finish_preparing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_preparing(false, cx);
        let Some(editor) = self
            .prepared
            .as_ref()
            .and_then(|prepared| prepared.editor.as_ref())
        else {
            return;
        };
        // The session's editor is only rendered from now on, so it could not hold focus earlier.
        let focus = editor.focus_handle(cx).tab_index(3).tab_stop(true);
        if self.prompt_focus.is_focused(window) {
            focus.focus(window, cx);
        }
        self.prompt_focus = focus;
    }

    fn needs_sign_in(&self, cx: &App) -> bool {
        self.prepared
            .as_ref()
            .is_some_and(|prepared| prepared.view.read(cx).needs_authentication())
    }

    /// Replaces the prepared session with one for the current agent and destination.
    fn retarget(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.prepared.take().and_then(|prepared| prepared.editor) {
            let text = editor.read(cx).text(cx);
            let focused = editor.focus_handle(cx).is_focused(window);
            self.restore = Some((
                text.clone(),
                editor.read(cx).draft_content_blocks_snapshot(cx),
            ));
            self.prompt
                .update(cx, |prompt, cx| prompt.set_text(text, window, cx));
            self.prompt_focus = self.prompt.focus_handle(cx).tab_index(3).tab_stop(true);
            if focused {
                self.prompt_focus.focus(window, cx);
            }
        }
        self.set_preparing(false, cx);
        self.minimum_load = Task::ready(());
        self.prepare_task = Task::ready(());
        let Some(owner) = self.sidebar.read(cx).multi_workspace.upgrade() else {
            return;
        };
        let destination = self
            .destination
            .clone()
            .filter(|_| self.worktree_label().is_some());
        if let Some(workspace) = self.opened_workspace.take_if(|workspace| {
            destination
                .as_ref()
                .is_none_or(|destination| workspace_path_list(workspace, cx) != destination.paths)
        }) {
            close_background_workspace(&owner, workspace, window, cx);
        }
        let Some(destination) = destination else {
            return;
        };
        let agent = self.agent.clone();
        self.set_preparing(true, cx);
        self.session_settled = false;
        self.minimum_load_elapsed = false;
        self.minimum_load = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(MINIMUM_LOAD).await;
            this.update_in(cx, |this, window, cx| {
                this.minimum_load_elapsed = true;
                if this.session_settled {
                    this.finish_preparing(window, cx);
                }
            })
            .log_err();
        });
        self.prepare_task = cx.spawn_in(window, async move |this, cx| {
            let prepared = prepare(&this, &owner, &destination, agent, cx).await;
            this.update_in(cx, |this, window, cx| match prepared {
                Ok(Some((view, workspace))) => this.set_prepared(view, workspace, window, cx),
                Ok(None) => this.settle(window, cx),
                Err(error) => {
                    this.error = Some((Field::of(&error), error.to_string()));
                    this.settle(window, cx)
                }
            })
            .log_err();
        });
    }

    fn set_prepared(
        &mut self,
        view: Entity<ConversationView>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let observation = cx.observe_in(&view, window, |this, _, window, cx| {
            this.prepared_changed(window, cx)
        });
        self.prepared = Some(Prepared {
            view,
            workspace,
            editor: None,
            _observation: observation,
        });
        self.prepared_changed(window, cx);
    }

    fn prepared_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.notify();
        let Some(prepared) = self.prepared.as_mut() else {
            return;
        };
        if prepared.editor.is_some() {
            return;
        }
        let view = prepared.view.read(cx);
        let editor = view.message_editor(cx);
        if editor.is_none() && !view.failed_to_start() {
            return;
        }
        prepared.editor = editor.clone();
        self.settle(window, cx);
        let Some(editor) = editor else {
            return;
        };
        let text = self.prompt.read(cx).text(cx);
        // Editing the plain prompt in the meantime leaves the saved content out of date.
        let content = self
            .restore
            .take()
            .filter(|(restored_text, _)| *restored_text == text);
        editor.update(cx, |editor, cx| match content {
            Some((_, content)) => editor.set_message(content, window, cx),
            None => editor.insert_text(&text, window, cx),
        });
    }

    fn add_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // shortcut: always the system folder picker, because the in-app one is a modal that would replace this form; upgrade if `use_system_path_prompts: false` must be honored here.
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Add Project".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let Some(paths) = selection.await?? else {
                    return anyhow::Ok(());
                };
                this.update_in(cx, |this, window, cx| {
                    this.add_project_paths(PathList::new(&paths), window, cx)
                })?
                .await
            }
            .await;
            if let Err(error) = result {
                this.update(cx, |this, cx| {
                    this.error = Some((Field::Project, error.to_string()));
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }

    fn add_project_paths(
        &mut self,
        paths: PathList,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let sidebar = self.sidebar.clone();
        let Some(owner) = sidebar.read(cx).multi_workspace.upgrade() else {
            return Task::ready(Ok(()));
        };
        let open = owner.update(cx, |owner, cx| {
            // An existing workspace is selected as is, because opening it again would activate it and dismiss this form.
            match owner.workspace_for_paths(&paths, None, cx) {
                Some(workspace) => Task::ready(Ok(workspace)),
                None => owner.find_or_create_local_workspace(
                    paths,
                    None,
                    None,
                    OpenMode::Add,
                    None,
                    window,
                    cx,
                ),
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let workspace = open.await?;
            this.update_in(cx, |this, window, cx| {
                this.refresh_projects(cx);
                let destination = Destination {
                    project: workspace.read(cx).project_group_key(cx),
                    paths: workspace_path_list(&workspace, cx),
                    new_worktree: false,
                };
                this.set_destination(destination, window, cx);
            })
        })
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.preparing {
            self.create(window, cx);
        }
    }

    /// Hands the waiting session to the agent panel, where the agent's sign-in
    /// can use the workspace's terminal.
    fn sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.needs_sign_in(cx) {
            self.create(window, cx);
        }
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut destination) = self.destination.clone() else {
            return;
        };
        if self.worktree_label().is_none() || self.sidebar.read(cx).creating_prompted_thread {
            return;
        }
        let Some(owner) = self.sidebar.read(cx).multi_workspace.upgrade() else {
            return;
        };
        self.sidebar
            .update(cx, |sidebar, _| sidebar.creating_prompted_thread = true);
        let sidebar = self.sidebar.clone();
        let agent = self.agent.clone();
        let text = self.prompt_text(cx);
        let waiting_for_sign_in = self.needs_sign_in(cx);
        let prepared = self.prepared.take();
        // A session for a new worktree ran in the base worktree only to offer the agent's commands, so only its prompt moves to the worktree.
        let (prepared, base_session) = if destination.new_worktree {
            (None, prepared)
        } else {
            (
                prepared.filter(|prepared| prepared.editor.is_some() || waiting_for_sign_in),
                None,
            )
        };
        let content = base_session
            .as_ref()
            .and_then(|session| session.editor.as_ref())
            .filter(|editor| !editor.read(cx).is_empty(cx))
            .map(|editor| editor.update(cx, |editor, cx| editor.draft_contents(cx)));
        // Without a session's editor, the mentions and images saved from the previous session still belong to the prompt.
        let saved = self
            .restore
            .take()
            .filter(|(saved_text, _)| *saved_text == text)
            .map(|(_, content)| content);
        let mut restore = prepared
            .as_ref()
            .and_then(|prepared| prepared.editor.as_ref())
            .map(|editor| editor.read(cx).draft_content_blocks_snapshot(cx))
            .or(saved);
        // The thread owns whatever was opened for it, unless it cannot be created.
        // A new worktree's base only lends its repository, so it is closed either way.
        let mut opened_workspace = self.opened_workspace.take();
        let closes_base = destination.new_worktree;
        cx.emit(DismissEvent);
        window
            .spawn(cx, async move |cx| {
                let result = match prepared {
                    Some(prepared) => {
                        match ensure_worktree_exists(&prepared.workspace, &destination.paths, cx)
                            .await
                        {
                            Ok(()) => cx.update(|window, cx| {
                                adopt_thread(
                                    &owner,
                                    &sidebar,
                                    &destination,
                                    prepared,
                                    &text,
                                    restore.clone(),
                                    window,
                                    cx,
                                )
                            })?,
                            Err(error) => Err(error),
                        }
                    }
                    None => {
                        // A prompt sent without the mentions and images it was written with would mislead the agent.
                        let content = match content {
                            Some(content) => content.await.map(Some),
                            None => Ok(restore.take()),
                        };
                        drop(base_session);
                        match content {
                            Ok(content) => {
                                restore = content;
                                create_thread(
                                    &owner,
                                    &sidebar,
                                    &mut destination,
                                    agent.clone(),
                                    &text,
                                    &mut restore,
                                    &mut opened_workspace,
                                    cx,
                                )
                                .await
                            }
                            Err(error) => Err(error),
                        }
                    }
                };
                sidebar.update(cx, |sidebar, _| sidebar.creating_prompted_thread = false);
                if (closes_base || result.is_err())
                    && let Some(workspace) = opened_workspace
                {
                    cx.update(|window, cx| {
                        close_background_workspace(&owner, workspace, window, cx)
                    })?;
                }
                if let Err(error) = result {
                    cx.update(|window, cx| {
                        show_form(
                            sidebar,
                            owner,
                            agent,
                            Some(destination),
                            text,
                            restore,
                            Some((Field::of(&error), error.to_string())),
                            window,
                            cx,
                        )
                    })?;
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
    }
}

fn adopt_thread(
    owner: &Entity<MultiWorkspace>,
    sidebar: &Entity<Sidebar>,
    destination: &Destination,
    prepared: Prepared,
    text: &str,
    content: Option<Vec<ContentBlock>>,
    window: &mut Window,
    cx: &mut App,
) -> Result<()> {
    let Prepared {
        view,
        workspace,
        editor,
        ..
    } = prepared;
    let panel = workspace.read(cx).panel::<AgentPanel>(cx).ok_or_else(|| {
        anyhow!(ProjectError(
            "The agent panel is unavailable in this project"
        ))
    })?;
    ensure!(
        PathList::new(&workspace.read(cx).root_paths(cx)) == destination.paths,
        "The selected worktree changed. Choose a worktree and try again."
    );
    // Before a sign-in the session has no editor, so the prompt is still the form's own.
    let pending_prompt =
        (editor.is_none() && !text.trim().is_empty()).then(|| (text.to_owned(), content));
    let send = editor.is_some_and(|editor| {
        // The panel's message editor is not part of a tab group.
        drop(editor.focus_handle(cx).tab_index(0).tab_stop(false));
        !editor.read(cx).is_empty(cx)
    });
    owner.update(cx, |owner, cx| {
        owner.activate(workspace.clone(), None, window, cx);
        owner.retain_active_workspace(cx);
    });
    panel.update(cx, |panel, cx| {
        panel.adopt_prepared_thread(view, send, pending_prompt, window, cx)
    });
    workspace.update(cx, |workspace, cx| {
        workspace.focus_panel::<AgentPanel>(window, cx);
    });
    sidebar.update(cx, |sidebar, cx| {
        sidebar.reveal_created_thread(&workspace, cx)
    });
    Ok(())
}

async fn ensure_worktree_exists(
    workspace: &Entity<Workspace>,
    paths: &PathList,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let checks = workspace.read_with(cx, |workspace, cx| {
        let project = workspace.project().read(cx);
        paths
            .paths()
            .iter()
            .map(|path| {
                project
                    .host_path_is_dir(path, cx)
                    .context("Cannot verify this worktree on its host")
            })
            .collect::<Result<Vec<_>>>()
    })?;
    for check in checks {
        ensure!(
            check.await? == Some(true),
            "The selected worktree no longer exists"
        );
    }
    Ok(())
}

async fn create_thread(
    owner: &Entity<MultiWorkspace>,
    sidebar: &Entity<Sidebar>,
    // Updated once a worktree is created, so a retry after a later failure uses it instead of creating another.
    destination: &mut Destination,
    agent: Agent,
    text: &str,
    content: &mut Option<Vec<ContentBlock>>,
    // Set when a new worktree's base is opened here, for the caller to close.
    opened_workspace: &mut Option<Entity<Workspace>>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let key = &destination.project.clone();
    let paths = &destination.paths.clone();
    ensure!(
        key.host().is_some() || local_folders_exist(owner, paths, cx).await,
        "The selected worktree no longer exists"
    );
    let original = owner.read_with(cx, |owner, _| owner.workspace().clone());
    let connection_host = original.clone();
    let open = owner.update_in(cx, |owner, window, cx| {
        if owner.project_group_key_for_workspace(owner.workspace(), cx) == *key {
            owner.retain_active_workspace(cx);
        }
        ensure!(
            paths.is_empty() || owner.project_group_keys(cx).contains(key),
            ProjectError("This project is no longer in the sidebar")
        );
        // A new worktree's base only lends its repository, so it stays out of sight.
        if destination.new_worktree && key.host().is_none() {
            return Ok(match owner.workspace_for_paths(paths, None, cx) {
                Some(workspace) => (Task::ready(Ok(workspace)), false),
                None => (
                    owner.find_or_create_local_workspace(
                        paths.clone(),
                        Some(key.clone()),
                        None,
                        OpenMode::Add,
                        None,
                        window,
                        cx,
                    ),
                    true,
                ),
            });
        }
        Ok((
            owner.find_or_create_workspace(
                paths.clone(),
                key.host().cloned(),
                Some(key.clone()),
                move |options, window, cx| connect_remote(connection_host, options, window, cx),
                None,
                OpenMode::Activate,
                None,
                window,
                cx,
            ),
            false,
        ))
    })??;
    let (open, opened_base) = open;
    let result = open.await;
    remote_connection::dismiss_connection_modal(&original, cx);
    let workspace = result?;
    if opened_base {
        *opened_workspace = Some(workspace.clone());
    }
    ensure_worktree_exists(&workspace, paths, cx).await?;
    let (workspace, paths) = if destination.new_worktree {
        let created = Sidebar::create_default_worktree(&workspace, cx).await?;
        let workspace = created.workspace;
        let (paths, path_style) = cx.update(|_, cx| {
            (
                workspace_path_list(&workspace, cx),
                workspace.read(cx).project().read(cx).path_style(cx),
            )
        })?;
        *content = content.take().map(|content| {
            remap_mention_paths(content, path_style, |path| {
                remap_worktree_path(path, &created.path_remapping)
            })
        });
        *destination = Destination {
            project: key.clone(),
            paths: paths.clone(),
            new_worktree: false,
        };
        (workspace, paths)
    } else {
        (workspace, paths.clone())
    };
    let content = content.clone();
    let paths = &paths;
    if let Some(task) = workspace.update(cx, |workspace, _| workspace.take_panels_task()) {
        task.await?;
    }
    let panel = Sidebar::ensure_agent_panel(&workspace.downgrade(), cx).await?;
    workspace.update_in(cx, |workspace, window, cx| {
        ensure!(
            workspace.project().read(cx).remote_connection_options(cx) == key.host()
                && PathList::new(&workspace.root_paths(cx)) == *paths,
            "The selected worktree changed. Choose a worktree and try again."
        );
        panel.update(cx, |panel, cx| {
            panel.create_thread_from_prompt(
                agent,
                paths.clone(),
                text.to_owned(),
                content,
                window,
                cx,
            )
        })?;
        workspace.focus_panel::<AgentPanel>(window, cx);
        anyhow::Ok(())
    })??;
    sidebar.update(cx, |sidebar, cx| {
        sidebar.reveal_created_thread(&workspace, cx)
    });
    Ok(())
}

impl Render for NewThreadModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let project_label = self
            .project()
            .map(|project| project.label.clone())
            .unwrap_or_else(|| "Select project".into());
        let worktree_label = self.worktree_label();
        let modal = cx.entity();
        let project_menu = ContextMenu::build(window, cx, |mut menu, _, _| {
            menu = menu
                .entry("Add Project…", None, {
                    let modal = modal.downgrade();
                    move |window, cx| {
                        modal
                            .update(cx, |modal, cx| modal.add_project(window, cx))
                            .log_err();
                    }
                })
                .separator();
            for (index, project) in self.projects.iter().enumerate() {
                if index == 1 {
                    menu = menu.separator();
                }
                let key = project.key.clone();
                let modal = modal.downgrade();
                menu = menu.entry(project.label.clone(), None, move |window, cx| {
                    modal
                        .update(cx, |modal, cx| {
                            let destination = Destination {
                                paths: modal
                                    .sidebar
                                    .read(cx)
                                    .multi_workspace
                                    .upgrade()
                                    .map(|owner| default_paths(&key, owner.read(cx), cx))
                                    .unwrap_or_else(|| key.path_list().clone()),
                                project: key.clone(),
                                new_worktree: false,
                            };
                            modal.set_destination(destination, window, cx);
                        })
                        .log_err();
                });
            }
            menu
        });
        let can_create_worktree = self
            .project()
            .is_some_and(|project| self.sidebar.read(cx).can_create_worktree(&project.key, cx));
        let worktree_menu = ContextMenu::build(window, cx, |mut menu, _, _| {
            if let Some(project) = self.project() {
                if can_create_worktree {
                    let destination = Destination {
                        project: project.key.clone(),
                        paths: project.key.path_list().clone(),
                        new_worktree: true,
                    };
                    let modal = modal.downgrade();
                    menu = menu
                        .entry("New Worktree", None, move |window, cx| {
                            modal
                                .update(cx, |modal, cx| {
                                    modal.set_destination(destination.clone(), window, cx)
                                })
                                .log_err();
                        })
                        .separator();
                }
                for (paths, label) in &project.worktrees {
                    let destination = Destination {
                        project: project.key.clone(),
                        paths: paths.clone(),
                        new_worktree: false,
                    };
                    let modal = modal.downgrade();
                    menu = menu.entry(label.clone(), None, move |window, cx| {
                        modal
                            .update(cx, |modal, cx| {
                                modal.set_destination(destination.clone(), window, cx)
                            })
                            .log_err();
                    });
                }
            }
            menu
        });
        let agent_store = self.sidebar.read(cx).active_workspace(cx).map(|workspace| {
            workspace
                .read(cx)
                .project()
                .read(cx)
                .agent_server_store()
                .clone()
        });
        let agent_label = |agent: &Agent| match (agent, &agent_store) {
            (Agent::Custom { id }, Some(store)) => store
                .read(cx)
                .agent_display_name(id)
                .unwrap_or_else(|| agent.label()),
            _ => agent.label(),
        };
        let selected_agent_label = agent_label(&self.agent);
        let prepared_editor = self
            .prepared
            .as_ref()
            .and_then(|prepared| prepared.editor.clone());
        // Fills the space between a field's label and its control.
        let field_message = |id: &'static str, message: Option<(SharedString, Color)>| {
            h_flex().id(id).flex_1().min_w_0().justify_end().when_some(
                message,
                |this, (message, color)| {
                    this.tooltip(Tooltip::text(message.clone())).child(
                        Label::new(message)
                            .size(LabelSize::Small)
                            .color(color)
                            .truncate(),
                    )
                },
            )
        };
        let field_error = |field: Field| {
            self.error
                .as_ref()
                .filter(|(error_field, _)| *error_field == field)
                .map(|(_, message)| (SharedString::from(message.clone()), Color::Error))
        };
        let needs_sign_in = self.needs_sign_in(cx);
        let preparing = self.preparing;
        let accepts_drops = !needs_sign_in
            && !preparing
            && self.prepared.as_ref().is_some_and(|prepared| {
                prepared.editor.is_some()
                    && prepared.workspace.read(cx).project().read(cx).is_local()
            });
        let loading_label = format!("Starting {selected_agent_label}…");
        let session_status = if needs_sign_in {
            Some(format!(
                "{selected_agent_label} needs you to sign in before a thread can start."
            ))
        } else if !preparing
            && self
                .prepared
                .as_ref()
                .is_some_and(|prepared| prepared.editor.is_none())
        {
            Some(format!(
                "{selected_agent_label} failed to start. Its commands and skills are unavailable."
            ))
        } else {
            None
        };
        let agents = std::iter::once(Agent::NativeAgent)
            .chain(agent_store.iter().flat_map(|store| {
                store
                    .read(cx)
                    .external_agents()
                    .cloned()
                    .map(Agent::from)
                    .collect::<Vec<_>>()
            }))
            .map(|agent| (agent_label(&agent), agent))
            .collect::<Vec<_>>();
        let agent_menu = ContextMenu::build(window, cx, |mut menu, _, _| {
            for (label, agent) in agents {
                let modal = modal.downgrade();
                menu = menu.entry(label, None, move |window, cx| {
                    modal
                        .update(cx, |modal, cx| modal.set_agent(agent.clone(), window, cx))
                        .log_err();
                });
            }
            menu
        });
        v_flex()
            .id("new-thread-modal")
            .key_context("NewThreadModal")
            .tab_group()
            .w(rems(40.))
            .p_4()
            .gap_3()
            .elevation_3(cx)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.submit(window, cx)))
            .on_action(|_: &menu::SelectNext, window, cx| window.focus_next(cx))
            .on_action(|_: &menu::SelectPrevious, window, cx| window.focus_prev(cx))
            .child(
                h_flex()
                    .gap_4()
                    .child(div().flex_none().child(Label::new("Agent:")))
                    .child(field_message(
                        "agent-message",
                        session_status.map(|status| (status.into(), Color::Muted)),
                    ))
                    .child(
                        div().flex_none().child(
                            DropdownMenu::new("thread-agent", selected_agent_label, agent_menu)
                                .aria_label("Agent")
                                .tab_index(0)
                                .trigger_size(ButtonSize::Medium)
                                .style(DropdownStyle::Outlined),
                        ),
                    ),
            )
            .child(
                h_flex()
                    .gap_4()
                    .child(div().flex_none().child(Label::new("Project:")))
                    .child(field_message(
                        "project-message",
                        field_error(Field::Project),
                    ))
                    .child(
                        PopoverMenu::new("thread-project")
                            .trigger(
                                Button::new("thread-project-trigger", project_label)
                                    .aria_label("Project")
                                    .style(ButtonStyle::Outlined)
                                    .size(ButtonSize::Medium)
                                    .end_icon(
                                        Icon::new(IconName::ChevronUpDown)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .track_focus(&self.project_focus),
                            )
                            .menu(move |_, _| Some(project_menu.clone())),
                    ),
            )
            .child(
                h_flex()
                    .gap_4()
                    .child(div().flex_none().child(Label::new("Worktree:")))
                    .child(field_message(
                        "worktree-message",
                        field_error(Field::Worktree),
                    ))
                    .child(
                        DropdownMenu::new(
                            "thread-worktree",
                            worktree_label
                                .clone()
                                .unwrap_or_else(|| "Select worktree".into()),
                            worktree_menu,
                        )
                        .aria_label("Worktree")
                        .tab_index(2)
                        .trigger_size(ButtonSize::Medium)
                        .style(DropdownStyle::Outlined)
                        .disabled(
                            self.project()
                                .is_none_or(|project| project.key.path_list().is_empty()),
                        ),
                    ),
            )
            .child(
                // A new thread's message editor fills its container, so the container needs a height.
                v_flex()
                    .h(rems(10.))
                    .p_2()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .overflow_hidden()
                    .relative()
                    .map(|prompt| {
                        // Neither editor is rendered while waiting, so the box itself holds focus.
                        if needs_sign_in {
                            prompt.track_focus(&self.prompt_focus)
                        } else if preparing {
                            prompt
                                .track_focus(&self.prompt_focus)
                                .items_center()
                                .justify_center()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Icon::new(IconName::ArrowCircle)
                                                .size(IconSize::Small)
                                                .color(Color::Muted)
                                                .with_rotate_animation(2),
                                        )
                                        .child(Label::new(loading_label).color(Color::Muted)),
                                )
                        } else if let Some(editor) = prepared_editor {
                            prompt.child(editor)
                        } else {
                            prompt.child(self.prompt.clone())
                        }
                    })
                    .when(accepts_drops, |prompt| {
                        // An overlay, because the editor paints over the container's background.
                        prompt.child(
                            div()
                                .invisible()
                                .absolute()
                                .top_0()
                                .right_0()
                                .bottom_0()
                                .left_0()
                                .bg(cx.theme().colors().drop_target_background)
                                .drag_over::<ExternalPaths>(|style, _, _, _| style.visible())
                                .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                                    this.insert_dropped_paths(paths, window, cx);
                                })),
                        )
                    }),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cancel", "Cancel")
                            .style(ButtonStyle::Outlined)
                            .size(ButtonSize::Medium)
                            .tab_index(4_isize)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(if needs_sign_in {
                        Button::new("sign-in", "Sign in")
                            .style(ButtonStyle::Tinted(TintColor::Accent))
                            .size(ButtonSize::Medium)
                            .tab_index(5_isize)
                            .on_click(cx.listener(|this, _, window, cx| this.sign_in(window, cx)))
                    } else {
                        Button::new("create", "Create thread")
                            .style(ButtonStyle::Tinted(TintColor::Accent))
                            .size(ButtonSize::Medium)
                            .tab_index(5_isize)
                            .disabled(worktree_label.is_none() || preparing)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar_tests::{
        add_agent_panel, init_delete_worktree_test, init_test_project_with_agent_panel,
        setup_sidebar, setup_sidebar_closed,
    };
    use agent_ui::thread_metadata_store::ThreadMetadataStore;
    use gpui::{TestAppContext, VisualTestContext};

    fn assert_created_thread_is_active(
        sidebar: &Entity<Sidebar>,
        owner: &Entity<MultiWorkspace>,
        cx: &mut VisualTestContext,
    ) {
        sidebar.read_with(cx, |sidebar, cx| {
            let workspace = owner.read(cx).workspace().clone();
            let panel = workspace.read(cx).panel::<AgentPanel>(cx).unwrap();
            let created = panel.read(cx).active_thread_id(cx).unwrap();
            assert!(
                matches!(
                    &sidebar.active_entry,
                    Some(crate::ActiveEntry::Thread { thread_id, workspace: active, .. })
                        if *thread_id == created && *active == workspace
                ),
                "the created thread should be the sidebar's active entry, got {:?}",
                sidebar.active_entry
            );
            let index = sidebar
                .contents
                .entries
                .iter()
                .position(|entry| matches!(entry, ListEntry::Thread(thread) if thread.metadata.thread_id == created));
            assert!(index.is_some(), "the created thread should be listed");
        });
    }

    fn settle(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.executor().advance_clock(MINIMUM_LOAD);
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn new_thread_modal_submits_once_and_preserves_drafts(cx: &mut TestAppContext) {
        let project = init_test_project_with_agent_panel("/project", cx).await;
        let (owner, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let sidebar = setup_sidebar_closed(&owner, cx);
        let workspace = owner.read_with(cx, |owner, _| owner.workspace().clone());
        let panel = add_agent_panel(&workspace, cx);
        assert!(!owner.read_with(cx, |owner, _| owner.active_workspace_is_retained()));
        let key = workspace.read_with(cx, |workspace, cx| workspace.project_group_key(cx));
        let mut created = Vec::new();
        for text in ["", " \t\n", "  first line\nsecond line  "] {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.set_group_expanded(&key, false, cx)
            });
            cx.update(|window, cx| open_new_thread_modal(false, window, cx));
            let modal = owner.read_with(cx, |owner, cx| {
                owner.active_modal::<NewThreadModal>(cx).unwrap()
            });
            settle(cx);
            let last_used = panel.read_with(cx, |panel, cx| panel.selected_agent(cx));
            let first = modal.update_in(cx, |modal, window, cx| {
                assert_eq!(modal.agent, last_used);
                assert!(modal.worktree_label().is_some());
                let first = modal.prepared.as_ref().unwrap().view.downgrade();
                modal.set_agent(Agent::Stub, window, cx);
                first
            });
            settle(cx);
            assert!(
                first.upgrade().is_none(),
                "changing the agent should discard the previous session"
            );
            let prepared = modal.update_in(cx, |modal, window, cx| {
                let prepared = modal.prepared.as_ref().unwrap();
                prepared
                    .editor
                    .as_ref()
                    .expect("stub session should be ready")
                    .update(cx, |editor, cx| editor.insert_text(text, window, cx));
                prepared.view.clone()
            });
            let prepared_id = prepared.read_with(cx, |view, cx| {
                assert!(
                    ThreadMetadataStore::global(cx)
                        .read(cx)
                        .entry(view.parent_id())
                        .is_none(),
                    "a prepared session should not be listed"
                );
                view.parent_id()
            });
            drop(prepared);
            panel.read_with(cx, |panel, cx| {
                assert_eq!(panel.selected_agent(cx), last_used);
                assert_ne!(panel.active_thread_id(cx), Some(prepared_id));
                assert!(!panel.is_retained_thread(&prepared_id));
            });
            cx.update(|window, cx| open_new_thread_modal(false, window, cx));
            assert_eq!(
                modal.entity_id(),
                owner.read_with(cx, |owner, cx| owner
                    .active_modal::<NewThreadModal>(cx)
                    .unwrap()
                    .entity_id())
            );
            modal.update_in(cx, |modal, window, cx| {
                assert_eq!(modal.prompt_text(cx), text);
                modal.submit(window, cx);
                modal.submit(window, cx);
            });
            settle(cx);
            panel.update_in(cx, |panel, window, cx| {
                let id = panel.active_thread_id(cx).expect("thread should be open");
                assert_eq!(id, prepared_id, "the prepared session should become the thread");
                assert!(
                    ThreadMetadataStore::global(cx)
                        .read(cx)
                        .entry(id)
                        .is_some()
                );
                assert!(!created.contains(&id));
                for previous in &created {
                    assert!(panel.is_retained_thread(previous));
                }
                assert_eq!(panel.selected_agent(cx), Agent::Stub);
                let thread = panel.active_agent_thread(cx).expect("stub session should be ready");
                let thread = thread.read(cx);
                assert_eq!(thread.work_dirs(), Some(&PathList::new(&["/project"])));
                let messages = thread.entries().iter()
                    .filter_map(|entry| entry.user_message())
                    .map(|message| message.content.source_blocks())
                    .collect::<Vec<_>>();
                if text.trim().is_empty() {
                    assert!(messages.is_empty());
                    let view = panel.active_thread_view(cx).unwrap();
                    assert!(view.read(cx).message_editor.focus_handle(cx).contains_focused(window, cx));
                } else {
                    assert_eq!(messages.len(), 1);
                    assert!(matches!(messages[0], [agent_client_protocol::v2::schema::ContentBlock::Text(block)] if block.text.trim() == text.trim()));
                }
                created.push(id);
            });
            assert_created_thread_is_active(&sidebar, &owner, cx);
        }
        cx.update(|window, cx| open_new_thread_modal(false, window, cx));
        settle(cx);
        let (cancelled, cancelled_id) = owner.read_with(cx, |owner, cx| {
            let modal = owner.active_modal::<NewThreadModal>(cx).unwrap();
            let view = &modal.read(cx).prepared.as_ref().unwrap().view;
            (view.downgrade(), view.read(cx).parent_id())
        });
        cx.dispatch_action(menu::Cancel);
        settle(cx);
        assert!(owner.read_with(cx, |owner, cx| {
            owner.active_modal::<NewThreadModal>(cx).is_none()
        }));
        assert!(
            cancelled.upgrade().is_none(),
            "closing the form should discard its session"
        );
        cx.update(|_, cx| {
            assert!(
                ThreadMetadataStore::global(cx)
                    .read(cx)
                    .entry(cancelled_id)
                    .is_none()
            );
        });
        assert_eq!(
            created.last().copied(),
            panel.read_with(cx, |panel, cx| panel.active_thread_id(cx))
        );
    }

    #[gpui::test]
    async fn new_thread_modal_uses_sidebar_destinations_and_restores_failed_prompt(
        cx: &mut TestAppContext,
    ) {
        use fs::Fs as _;
        let (fs, project) = init_delete_worktree_test(false, cx).await;
        let (owner, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let sidebar = setup_sidebar(&owner, cx);
        let key = owner.read_with(cx, |owner, cx| {
            owner.workspace().read(cx).project_group_key(cx)
        });
        owner.update(cx, |owner, cx| {
            owner.group_state_by_key_mut(&key, cx).unwrap().custom_name = Some("Personal".into())
        });
        sidebar.update(cx, |sidebar, cx| {
            sidebar.update_entries(cx);
            sidebar.selection = sidebar.contents.entries.iter().position(|entry| {
                matches!(
                    entry,
                    ListEntry::WorktreeHeader {
                        is_linked: true,
                        ..
                    }
                )
            });
        });
        let linked = sidebar.read_with(cx, |sidebar, cx| {
            initial_destination(sidebar, owner.read(cx), true, cx)
        });
        assert_ne!(linked.paths, *key.path_list());
        let active = sidebar.read_with(cx, |sidebar, cx| {
            initial_destination(sidebar, owner.read(cx), false, cx)
        });
        assert_eq!(active.paths, *key.path_list());
        sidebar.update(cx, |sidebar, cx| {
            sidebar.set_group_expanded(&key, false, cx)
        });
        cx.update(|window, cx| {
            show_form(
                sidebar.clone(),
                owner.clone(),
                Agent::Stub,
                Some(linked.clone()),
                "keep me".into(),
                None,
                None,
                window,
                cx,
            )
        });
        let modal = owner.read_with(cx, |owner, cx| {
            owner.active_modal::<NewThreadModal>(cx).unwrap()
        });
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.project().unwrap().label.as_ref(), "Personal");
            assert_eq!(modal.project().unwrap().worktrees.len(), 2);
        });
        modal.update_in(cx, |modal, window, cx| {
            assert!(modal.preparing);
            modal.submit(window, cx);
        });
        settle(cx);
        assert_eq!(
            modal.entity_id(),
            owner.read_with(cx, |owner, cx| owner
                .active_modal::<NewThreadModal>(cx)
                .unwrap()
                .entity_id()),
            "a thread cannot be created while its session is starting"
        );
        fs.remove_dir(
            &linked.paths.paths()[0],
            fs::RemoveOptions {
                recursive: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        modal.update_in(cx, |modal, window, cx| {
            let editor = modal.prepared.as_ref().unwrap().editor.clone().unwrap();
            editor.update(cx, |editor, cx| {
                editor.append_message(
                    vec![ContentBlock::ResourceLink(
                        agent_client_protocol::v2::schema::ResourceLink::new(
                            "src",
                            "file:///project/src/",
                        ),
                    )],
                    Some(" "),
                    window,
                    cx,
                )
            });
            modal.submit(window, cx)
        });
        settle(cx);
        let restored = owner.read_with(cx, |owner, cx| {
            owner.active_modal::<NewThreadModal>(cx).unwrap()
        });
        restored.read_with(cx, |modal, cx| {
            assert!(matches!(modal.error, Some((Field::Worktree, _))));
            assert!(modal.prompt.read(cx).text(cx).starts_with("keep me"));
            let content = modal
                .prepared
                .as_ref()
                .and_then(|prepared| prepared.editor.as_ref())
                .map(|editor| editor.read(cx).draft_content_blocks_snapshot(cx))
                .or_else(|| modal.restore.clone().map(|(_, content)| content))
                .expect("the failed prompt's content should be kept");
            assert!(
                content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ResourceLink(_))),
                "a failed prompt should keep its mentions: {content:?}"
            );
        });
    }
    #[gpui::test]
    async fn new_thread_modal_closes_workspace_opened_for_its_session(cx: &mut TestAppContext) {
        let (_fs, project) = init_delete_worktree_test(false, cx).await;
        let (owner, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let sidebar = setup_sidebar(&owner, cx);
        sidebar.update(cx, |sidebar, cx| {
            sidebar.update_entries(cx);
            sidebar.selection = sidebar.contents.entries.iter().position(|entry| {
                matches!(
                    entry,
                    ListEntry::WorktreeHeader {
                        is_linked: true,
                        ..
                    }
                )
            });
        });
        let linked = sidebar.read_with(cx, |sidebar, cx| {
            initial_destination(sidebar, owner.read(cx), true, cx)
        });
        let active_workspace = owner.read_with(cx, |owner, cx| {
            assert!(owner.workspace_for_paths(&linked.paths, None, cx).is_none());
            owner.workspace().clone()
        });
        cx.update(|window, cx| {
            show_form(
                sidebar.clone(),
                owner.clone(),
                Agent::Stub,
                Some(linked.clone()),
                String::new(),
                None,
                None,
                window,
                cx,
            )
        });
        settle(cx);
        owner.read_with(cx, |owner, cx| {
            assert_eq!(owner.workspace(), &active_workspace);
            let opened = owner
                .workspace_for_paths(&linked.paths, None, cx)
                .expect("the worktree should open in the background");
            let modal = owner
                .active_modal::<NewThreadModal>(cx)
                .expect("the form should stay open");
            assert_eq!(modal.read(cx).opened_workspace.as_ref(), Some(&opened));
        });
        cx.update(|window, cx| {
            let modal = owner.read(cx).active_modal::<NewThreadModal>(cx).unwrap();
            assert!(
                modal.focus_handle(cx).contains_focused(window, cx),
                "opening a workspace in the background should not take focus from the form"
            );
        });
        cx.dispatch_action(menu::Cancel);
        settle(cx);
        owner.read_with(cx, |owner, cx| {
            assert!(owner.active_modal::<NewThreadModal>(cx).is_none());
            assert_eq!(owner.workspace(), &active_workspace);
            assert!(owner.workspace_for_paths(&linked.paths, None, cx).is_none());
        });
    }

    #[gpui::test]
    async fn new_thread_modal_creates_worktree_with_thread(cx: &mut TestAppContext) {
        use fs::Fs as _;
        let (fs, project) = init_delete_worktree_test(false, cx).await;
        let (owner, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let sidebar = setup_sidebar(&owner, cx);
        let main_workspace = owner.read_with(cx, |owner, _| owner.workspace().clone());
        add_agent_panel(&main_workspace, cx);
        let key = main_workspace.read_with(cx, |workspace, cx| workspace.project_group_key(cx));
        let repository = fs.open_repo(Path::new("/project/.git"), None).unwrap();
        let worktree_count = repository.worktrees().await.unwrap().len();
        cx.update(|window, cx| {
            show_form(
                sidebar.clone(),
                owner.clone(),
                Agent::Stub,
                Some(Destination {
                    project: key.clone(),
                    paths: key.path_list().clone(),
                    new_worktree: true,
                }),
                String::new(),
                None,
                None,
                window,
                cx,
            )
        });
        let modal = owner.read_with(cx, |owner, cx| {
            owner.active_modal::<NewThreadModal>(cx).unwrap()
        });
        settle(cx);
        assert_eq!(repository.worktrees().await.unwrap().len(), worktree_count);
        modal.update_in(cx, |modal, window, cx| {
            assert_eq!(modal.worktree_label().unwrap().as_ref(), "New Worktree");
            let prepared = modal.prepared.as_ref().unwrap();
            assert_eq!(prepared.workspace, main_workspace);
            prepared
                .editor
                .as_ref()
                .expect("the base worktree's session should offer the agent's commands")
                .update(cx, |editor, cx| {
                    use agent_client_protocol::v2::schema::{
                        ContentBlock, ResourceLink, TextContent,
                    };
                    editor.set_message(
                        vec![
                            // The new worktree's session has not announced its commands when the thread is created.
                            ContentBlock::Text(TextContent::new("use /some-skill to see ")),
                            ContentBlock::ResourceLink(ResourceLink::new(
                                "src",
                                "file:///project/src/",
                            )),
                        ],
                        window,
                        cx,
                    )
                });
            modal.submit(window, cx)
        });
        for _ in 0..8 {
            settle(cx);
        }
        assert_eq!(
            repository.worktrees().await.unwrap().len(),
            worktree_count + 1
        );
        let thread = owner.read_with(cx, |owner, cx| {
            let panel = owner.workspace().read(cx).panel::<AgentPanel>(cx).unwrap();
            panel.read(cx).active_agent_thread(cx).unwrap()
        });
        assert!(
            thread.read_with(cx, |thread, _| thread.entries().is_empty()),
            "the prompt should wait for the session to announce its commands"
        );
        thread
            .update(cx, |thread, cx| {
                use agent_client_protocol::v1::schema as acp_v1;
                thread.handle_session_update(
                    acp_v1::SessionUpdate::AvailableCommandsUpdate(
                        acp_v1::AvailableCommandsUpdate::new(vec![acp_v1::AvailableCommand::new(
                            "some-skill",
                            "A skill",
                        )]),
                    ),
                    cx,
                )
            })
            .unwrap();
        cx.run_until_parked();
        owner.read_with(cx, |owner, cx| {
            assert!(owner.active_modal::<NewThreadModal>(cx).is_none());
            let workspace = owner.workspace();
            assert_ne!(workspace, &main_workspace);
            assert_eq!(workspace.read(cx).project_group_key(cx), key);
            let panel = workspace.read(cx).panel::<AgentPanel>(cx).unwrap();
            let thread = panel.read(cx).active_agent_thread(cx).unwrap().read(cx);
            assert_eq!(
                thread.work_dirs(),
                Some(&workspace_path_list(workspace, cx))
            );
            let messages = thread
                .entries()
                .iter()
                .filter_map(|entry| entry.user_message())
                .map(|message| message.content.source_blocks())
                .collect::<Vec<_>>();
            assert_eq!(messages.len(), 1);
            assert!(
                messages[0].iter().any(|block| !matches!(
                    block,
                    agent_client_protocol::v2::schema::ContentBlock::Text(_)
                )),
                "a mention typed into the base worktree's session should reach the new worktree: {:?}",
                messages[0]
            );
            let sent = format!("{:?}", messages[0]);
            let root = workspace.read(cx).root_paths(cx)[0].clone();
            assert!(
                sent.contains(&format!("file://{}/src", root.display()))
                    && !sent.contains("file:///project/src"),
                "the mention should point into the new worktree {root:?}: {sent}"
            );
            assert!(
                main_workspace
                    .read(cx)
                    .panel::<AgentPanel>(cx)
                    .unwrap()
                    .read(cx)
                    .active_thread_id(cx)
                    .is_none(),
                "the base worktree should not keep the session"
            );
        });
        assert_created_thread_is_active(&sidebar, &owner, cx);
    }

    #[gpui::test]
    async fn new_thread_modal_starts_session_for_non_git_project(cx: &mut TestAppContext) {
        let (fs, _) = init_delete_worktree_test(false, cx).await;
        fs.insert_tree("/other", serde_json::json!({"src": {}}))
            .await;
        let project = project::Project::test(fs, [Path::new("/other")], cx).await;
        let (owner, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let sidebar = setup_sidebar(&owner, cx);
        let workspace = owner.read_with(cx, |owner, _| owner.workspace().clone());
        add_agent_panel(&workspace, cx);
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, cx| {
            let key = workspace.read(cx).project_group_key(cx);
            assert!(
                !sidebar.can_create_worktree(&key, cx),
                "not a Git repository"
            );
        });
        cx.update(|window, cx| open_new_thread_modal(false, window, cx));
        let modal = owner.read_with(cx, |owner, cx| {
            owner.active_modal::<NewThreadModal>(cx).unwrap()
        });
        settle(cx);
        modal.read_with(cx, |modal, _| {
            assert!(modal.worktree_label().is_some());
            assert!(
                modal
                    .prepared
                    .as_ref()
                    .is_some_and(|prepared| prepared.editor.is_some()),
                "a non-Git project should get a session"
            );
        });
    }

    #[gpui::test]
    async fn new_thread_modal_inserts_dropped_files(cx: &mut TestAppContext) {
        let (fs, _) = init_delete_worktree_test(false, cx).await;
        fs.insert_tree("/other", serde_json::json!({"notes.txt": "hello"}))
            .await;
        let project = project::Project::test(fs, [Path::new("/other")], cx).await;
        let (owner, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        setup_sidebar(&owner, cx);
        let workspace = owner.read_with(cx, |owner, _| owner.workspace().clone());
        add_agent_panel(&workspace, cx);
        cx.run_until_parked();
        cx.update(|window, cx| open_new_thread_modal(false, window, cx));
        let modal = owner.read_with(cx, |owner, cx| {
            owner.active_modal::<NewThreadModal>(cx).unwrap()
        });
        settle(cx);
        modal.update_in(cx, |modal, window, cx| {
            let paths = ExternalPaths(vec!["/other/notes.txt".into()].into());
            modal.insert_dropped_paths(&paths, window, cx);
        });
        cx.run_until_parked();
        let text = modal.read_with(cx, |modal, cx| modal.prompt_text(cx));
        assert!(
            text.contains("notes.txt"),
            "the dropped file should be mentioned, got {text:?}"
        );
    }

    #[gpui::test]
    async fn new_thread_modal_starts_session_for_closed_non_git_project(cx: &mut TestAppContext) {
        let (fs, main_project) = init_delete_worktree_test(false, cx).await;
        fs.insert_tree("/other", serde_json::json!({"src": {}}))
            .await;
        let other_project = project::Project::test(fs, [Path::new("/other")], cx).await;
        let (owner, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project, window, cx));
        let sidebar = setup_sidebar(&owner, cx);
        let main_workspace = owner.read_with(cx, |owner, _| owner.workspace().clone());
        add_agent_panel(&main_workspace, cx);
        let other_workspace = owner.update_in(cx, |owner, window, cx| {
            owner.test_add_workspace(other_project, window, cx)
        });
        cx.run_until_parked();
        let key = other_workspace.read_with(cx, |workspace, cx| workspace.project_group_key(cx));
        owner
            .update_in(cx, |owner, window, cx| {
                owner.activate(main_workspace.clone(), None, window, cx);
                owner.remove([other_workspace], RemovalIntent::KeepProject, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        cx.update(|window, cx| {
            show_form(
                sidebar.clone(),
                owner.clone(),
                Agent::Stub,
                Some(Destination {
                    project: key.clone(),
                    paths: key.path_list().clone(),
                    new_worktree: false,
                }),
                String::new(),
                None,
                None,
                window,
                cx,
            )
        });
        let modal = owner.read_with(cx, |owner, cx| {
            owner.active_modal::<NewThreadModal>(cx).unwrap()
        });
        settle(cx);
        modal.read_with(cx, |modal, _| {
            assert!(
                modal.project().is_some(),
                "the closed project should be listed"
            );
            assert!(
                modal.worktree_label().is_some(),
                "a closed non-Git project should offer its folder"
            );
            assert!(
                modal
                    .prepared
                    .as_ref()
                    .is_some_and(|prepared| prepared.editor.is_some()),
                "a closed non-Git project should get a session"
            );
        });
    }

    #[gpui::test]
    async fn new_thread_modal_creates_projectless_thread(cx: &mut TestAppContext) {
        let project = init_test_project_with_agent_panel("/project", cx).await;
        let (owner, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let sidebar = setup_sidebar(&owner, cx);
        let project_workspace = owner.read_with(cx, |owner, _| owner.workspace().clone());
        add_agent_panel(&project_workspace, cx);
        cx.update(|window, cx| {
            show_form(
                sidebar.clone(),
                owner.clone(),
                Agent::Stub,
                Some(Destination {
                    project: no_project_key(),
                    paths: PathList::default(),
                    new_worktree: false,
                }),
                "hello".into(),
                None,
                None,
                window,
                cx,
            )
        });
        let modal = owner.read_with(cx, |owner, cx| {
            owner.active_modal::<NewThreadModal>(cx).unwrap()
        });
        settle(cx);
        modal.update_in(cx, |modal, window, cx| {
            assert_eq!(modal.project().unwrap().label.as_ref(), "No Project");
            let prepared = modal.prepared.as_ref().unwrap();
            assert!(
                prepared.editor.is_some(),
                "a session should start without a project"
            );
            assert_ne!(prepared.workspace, project_workspace);
            assert!(prepared.workspace.read(cx).root_paths(cx).is_empty());
            modal.submit(window, cx)
        });
        settle(cx);
        assert!(owner.read_with(cx, |owner, cx| {
            owner.active_modal::<NewThreadModal>(cx).is_none()
        }));
        let host = owner.read_with(cx, |owner, _| owner.workspace().clone());
        assert_ne!(host, project_workspace);
        let id = host.read_with(cx, |workspace, cx| {
            assert!(workspace.root_paths(cx).is_empty());
            let panel = workspace.panel::<AgentPanel>(cx).unwrap().read(cx);
            assert_eq!(
                panel.active_agent_thread(cx).unwrap().read(cx).work_dirs(),
                Some(&workspace.project().read(cx).default_path_list(cx)),
                "a thread without a project runs in the default directory, as in the agent panel"
            );
            panel.active_thread_id(cx).unwrap()
        });
        sidebar.read_with(cx, |sidebar, cx| {
            assert!(matches!(
                sidebar.contents.entries.first(),
                Some(ListEntry::Thread(thread))
                    if thread.metadata.thread_id == id && thread.metadata.is_projectless()
            ));
            assert!(
                initial_destination(sidebar, owner.read(cx), false, cx).project == no_project_key()
            );
        });
        assert_created_thread_is_active(&sidebar, &owner, cx);
    }

    #[gpui::test]
    async fn new_thread_modal_adds_project_without_leaving_form(cx: &mut TestAppContext) {
        let (fs, project) = init_delete_worktree_test(false, cx).await;
        fs.insert_tree("/other", serde_json::json!({"src": {}}))
            .await;
        let (owner, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let sidebar = setup_sidebar(&owner, cx);
        let original = owner.read_with(cx, |owner, _| owner.workspace().clone());
        cx.update(|window, cx| open_new_thread_modal(false, window, cx));
        let modal = owner.read_with(cx, |owner, cx| {
            owner.active_modal::<NewThreadModal>(cx).unwrap()
        });
        let other = PathList::new(&[Path::new("/other")]);
        modal
            .update_in(cx, |modal, window, cx| {
                modal.add_project_paths(other.clone(), window, cx)
            })
            .await
            .unwrap();
        settle(cx);
        assert_eq!(
            owner.read_with(cx, |owner, _| owner.workspace().clone()),
            original
        );
        assert_eq!(
            modal.entity_id(),
            owner.read_with(cx, |owner, cx| owner
                .active_modal::<NewThreadModal>(cx)
                .unwrap()
                .entity_id())
        );
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.destination.as_ref().unwrap().paths, other);
            assert!(modal.worktree_label().is_some());
        });
        sidebar.read_with(cx, |sidebar, cx| {
            assert!(
                projects(sidebar, owner.read(cx), cx)
                    .iter()
                    .any(|project| *project.key.path_list() == other)
            );
        });
    }

    #[gpui::test]
    async fn new_thread_modal_keeps_multi_root_paths_and_sidebar_order(cx: &mut TestAppContext) {
        let (fs, first) = init_delete_worktree_test(false, cx).await;
        fs.insert_tree("/other", serde_json::json!({"src": {}}))
            .await;
        let multi_root = project::Project::test(
            fs,
            [
                Path::new("/other"),
                Path::new("/worktrees/project/feature-a/project"),
            ],
            cx,
        )
        .await;
        let (owner, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(first, window, cx));
        let sidebar = setup_sidebar(&owner, cx);
        let active = owner.update_in(cx, |owner, window, cx| {
            owner.test_add_workspace(multi_root, window, cx)
        });
        settle(cx);
        let key = active.read_with(cx, |workspace, cx| workspace.project_group_key(cx));
        owner.update(cx, |owner, cx| {
            owner.move_project_group_up(&key, cx);
        });
        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.filter_editor.update(cx, |editor, cx| {
                editor.set_text("no matching project", window, cx)
            });
            sidebar.update_entries(cx);
            assert!(sidebar.contents.entries.is_empty());
            let choices = projects(sidebar, owner.read(cx), cx);
            assert_eq!(
                choices
                    .iter()
                    .skip(1)
                    .map(|choice| choice.key.clone())
                    .collect::<Vec<_>>(),
                owner.read(cx).project_group_keys(cx)
            );
            let selection = initial_destination(sidebar, owner.read(cx), false, cx);
            let project = choices
                .iter()
                .find(|choice| choice.key == selection.project)
                .unwrap();
            let (paths, _) = project
                .worktrees
                .iter()
                .find(|(paths, _)| *paths == selection.paths)
                .unwrap();
            assert_eq!(paths.paths().len(), 2);
            assert!(paths.ordered_paths().eq(selection.paths.ordered_paths()));
        });
    }

    fn mention_prompt() -> Vec<ContentBlock> {
        use agent_client_protocol::v2::schema::{ResourceLink, TextContent};
        vec![
            ContentBlock::Text(TextContent::new("see ")),
            ContentBlock::ResourceLink(ResourceLink::new("src", "file:///project/src/")),
        ]
    }

    fn linked_worktree_destination(
        sidebar: &Entity<Sidebar>,
        owner: &Entity<MultiWorkspace>,
        cx: &mut VisualTestContext,
    ) -> Destination {
        sidebar.update(cx, |sidebar, cx| {
            sidebar.update_entries(cx);
            sidebar.selection = sidebar.contents.entries.iter().position(|entry| {
                matches!(
                    entry,
                    ListEntry::WorktreeHeader {
                        is_linked: true,
                        ..
                    }
                )
            });
        });
        sidebar.read_with(cx, |sidebar, cx| {
            initial_destination(sidebar, owner.read(cx), true, cx)
        })
    }

    /// A thread in a new worktree whose prompt waits for commands the stub agent never announces.
    async fn worktree_thread_awaiting_commands(
        cx: &mut TestAppContext,
    ) -> (Entity<AgentPanel>, &mut VisualTestContext) {
        let (_fs, project) = init_delete_worktree_test(false, cx).await;
        let (owner, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let sidebar = setup_sidebar(&owner, cx);
        let main_workspace = owner.read_with(cx, |owner, _| owner.workspace().clone());
        add_agent_panel(&main_workspace, cx);
        let key = main_workspace.read_with(cx, |workspace, cx| workspace.project_group_key(cx));
        cx.update(|window, cx| {
            show_form(
                sidebar.clone(),
                owner.clone(),
                Agent::Stub,
                Some(Destination {
                    project: key.clone(),
                    paths: key.path_list().clone(),
                    new_worktree: true,
                }),
                String::new(),
                None,
                None,
                window,
                cx,
            )
        });
        let modal = owner.read_with(cx, |owner, cx| {
            owner.active_modal::<NewThreadModal>(cx).unwrap()
        });
        settle(cx);
        modal.update_in(cx, |modal, window, cx| {
            let editor = modal.prepared.as_ref().unwrap().editor.clone().unwrap();
            editor.update(cx, |editor, cx| {
                editor.set_message(mention_prompt(), window, cx)
            });
            modal.submit(window, cx)
        });
        for _ in 0..3 {
            settle(cx);
        }
        let panel = owner.read_with(cx, |owner, cx| {
            owner.workspace().read(cx).panel::<AgentPanel>(cx).unwrap()
        });
        assert_eq!(
            sent_messages(&panel, cx),
            0,
            "the stub agent announces no commands"
        );
        (panel, cx)
    }

    fn sent_messages(panel: &Entity<AgentPanel>, cx: &mut VisualTestContext) -> usize {
        panel.read_with(cx, |panel, cx| {
            let thread = panel.active_agent_thread(cx).unwrap();
            thread
                .read(cx)
                .entries()
                .iter()
                .filter_map(|entry| entry.user_message())
                .count()
        })
    }

    #[gpui::test]
    async fn new_thread_modal_sends_the_prompt_when_no_commands_are_announced(
        cx: &mut TestAppContext,
    ) {
        let (panel, cx) = worktree_thread_awaiting_commands(cx).await;
        cx.executor().advance_clock(Duration::from_secs(60));
        cx.run_until_parked();
        assert_eq!(sent_messages(&panel, cx), 1);
    }

    #[gpui::test]
    async fn new_thread_modal_leaves_a_prompt_the_user_already_sent(cx: &mut TestAppContext) {
        let (panel, cx) = worktree_thread_awaiting_commands(cx).await;
        let view = panel.read_with(cx, |panel, cx| panel.active_thread_view(cx).unwrap());
        let sent = |cx: &mut VisualTestContext| sent_messages(&panel, cx);
        view.update_in(cx, |view, window, cx| view.send(window, cx));
        cx.run_until_parked();
        assert_eq!(sent(cx), 1);
        view.update_in(cx, |view, window, cx| {
            view.message_editor
                .update(cx, |editor, cx| editor.set_text("follow-up", window, cx))
        });
        cx.executor().advance_clock(Duration::from_secs(60));
        cx.run_until_parked();
        assert_eq!(
            sent(cx),
            1,
            "giving up the wait should not send a follow-up being typed"
        );
        assert_eq!(
            view.read_with(cx, |view, cx| view.message_editor.read(cx).text(cx)),
            "follow-up"
        );
    }

    #[gpui::test]
    async fn new_thread_modal_leaves_open_workspaces_alone_for_a_missing_worktree(
        cx: &mut TestAppContext,
    ) {
        use fs::Fs as _;
        let (fs, main_project) = init_delete_worktree_test(false, cx).await;
        fs.insert_tree("/other", serde_json::json!({"src": {}}))
            .await;
        let other_project = project::Project::test(fs.clone(), [Path::new("/other")], cx).await;
        let (owner, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project, window, cx));
        let sidebar = setup_sidebar(&owner, cx);
        let main_workspace = owner.read_with(cx, |owner, _| owner.workspace().clone());
        add_agent_panel(&main_workspace, cx);
        let linked = linked_worktree_destination(&sidebar, &owner, cx);
        let other_workspace = owner.update_in(cx, |owner, window, cx| {
            owner.test_add_workspace(other_project, window, cx)
        });
        cx.run_until_parked();
        add_agent_panel(&other_workspace, cx);
        owner.update_in(cx, |owner, window, cx| {
            owner.activate(other_workspace.clone(), None, window, cx);
            owner.retain_active_workspace(cx);
        });
        cx.run_until_parked();
        fs.remove_dir(
            &linked.paths.paths()[0],
            fs::RemoveOptions {
                recursive: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        cx.update(|window, cx| {
            show_form(
                sidebar.clone(),
                owner.clone(),
                Agent::Stub,
                Some(linked.clone()),
                String::new(),
                None,
                None,
                window,
                cx,
            )
        });
        settle(cx);
        owner.read_with(cx, |owner, cx| {
            assert_eq!(owner.workspace(), &other_workspace);
            let modal = owner
                .active_modal::<NewThreadModal>(cx)
                .expect("the form should stay open");
            assert!(modal.read(cx).opened_workspace.is_none());
        });
        let modal = owner.read_with(cx, |owner, cx| {
            owner.active_modal::<NewThreadModal>(cx).unwrap()
        });
        modal.update_in(cx, |modal, window, cx| modal.submit(window, cx));
        settle(cx);
        owner.read_with(cx, |owner, cx| {
            assert_eq!(
                owner.workspace(),
                &other_workspace,
                "a thread that cannot be created should not switch workspaces"
            );
            let modal = owner.active_modal::<NewThreadModal>(cx).unwrap();
            assert!(modal.read(cx).error.is_some());
        });
        cx.dispatch_action(menu::Cancel);
        settle(cx);
        owner.read_with(cx, |owner, _| {
            assert!(
                owner
                    .workspaces()
                    .any(|workspace| workspace == &main_workspace),
                "a workspace the form did not open should stay open"
            );
        });
    }

    #[gpui::test]
    async fn new_thread_modal_keeps_mentions_without_a_session(cx: &mut TestAppContext) {
        use fs::Fs as _;
        let (fs, project) = init_delete_worktree_test(false, cx).await;
        let (owner, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let sidebar = setup_sidebar(&owner, cx);
        let main_workspace = owner.read_with(cx, |owner, _| owner.workspace().clone());
        add_agent_panel(&main_workspace, cx);
        let linked = linked_worktree_destination(&sidebar, &owner, cx);
        cx.update(|window, cx| open_new_thread_modal(false, window, cx));
        let modal = owner.read_with(cx, |owner, cx| {
            owner.active_modal::<NewThreadModal>(cx).unwrap()
        });
        settle(cx);
        let fingerprint = modal.read_with(cx, |modal, _| modal.projects_fingerprint);
        modal.update_in(cx, |modal, window, cx| {
            let editor = modal.prepared.as_ref().unwrap().editor.clone().unwrap();
            editor.update(cx, |editor, cx| {
                editor.set_message(mention_prompt(), window, cx)
            });
        });
        cx.run_until_parked();
        assert_eq!(
            modal.read_with(cx, |modal, _| modal.projects_fingerprint),
            fingerprint,
            "editing the prompt changes no project"
        );
        fs.remove_dir(
            &linked.paths.paths()[0],
            fs::RemoveOptions {
                recursive: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        modal.update_in(cx, |modal, window, cx| {
            modal.set_destination(linked.clone(), window, cx)
        });
        settle(cx);
        modal.update_in(cx, |modal, window, cx| {
            assert!(
                modal.prepared.is_none(),
                "a missing worktree gets no session"
            );
            modal.submit(window, cx)
        });
        settle(cx);
        let restored = owner.read_with(cx, |owner, cx| {
            owner.active_modal::<NewThreadModal>(cx).unwrap()
        });
        restored.read_with(cx, |modal, _| {
            assert!(modal.error.is_some());
            let (_, content) = modal
                .restore
                .as_ref()
                .expect("the failed prompt's content should be kept");
            assert!(
                content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ResourceLink(_))),
                "a prompt created without a session should keep its mentions: {content:?}"
            );
        });
    }

    #[gpui::test]
    async fn new_thread_modal_lists_worktrees_added_to_a_collapsed_project(
        cx: &mut TestAppContext,
    ) {
        let (fs, project) = init_delete_worktree_test(false, cx).await;
        let (owner, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let sidebar = setup_sidebar(&owner, cx);
        let workspace = owner.read_with(cx, |owner, _| owner.workspace().clone());
        add_agent_panel(&workspace, cx);
        let key = workspace.read_with(cx, |workspace, cx| workspace.project_group_key(cx));
        sidebar.update(cx, |sidebar, cx| {
            sidebar.set_group_expanded(&key, false, cx);
            sidebar.update_entries(cx);
        });
        cx.update(|window, cx| open_new_thread_modal(false, window, cx));
        let modal = owner.read_with(cx, |owner, cx| {
            owner.active_modal::<NewThreadModal>(cx).unwrap()
        });
        settle(cx);
        let worktree_count =
            modal.read_with(cx, |modal, _| modal.project().unwrap().worktrees.len());
        fs.add_linked_worktree_for_repo(
            Path::new("/project/.git"),
            true,
            git::repository::Worktree {
                path: "/worktrees/project/added-later/project".into(),
                ref_name: Some("refs/heads/added-later".into()),
                sha: "abc".into(),
                is_main: false,
                is_bare: false,
                is_prunable: false,
            },
        )
        .await;
        settle(cx);
        assert_eq!(
            modal.read_with(cx, |modal, _| modal.project().unwrap().worktrees.len()),
            worktree_count + 1
        );
    }

    #[gpui::test]
    async fn new_thread_modal_keeps_the_base_worktree_out_of_sight(cx: &mut TestAppContext) {
        let (fs, main_project) = init_delete_worktree_test(false, cx).await;
        fs.insert_tree("/other", serde_json::json!({"src": {}}))
            .await;
        let other_project = project::Project::test(fs.clone(), [Path::new("/other")], cx).await;
        let (owner, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(other_project, window, cx));
        let sidebar = setup_sidebar(&owner, cx);
        let other_workspace = owner.read_with(cx, |owner, _| owner.workspace().clone());
        let other_panel = add_agent_panel(&other_workspace, cx);
        let main_workspace = owner.update_in(cx, |owner, window, cx| {
            owner.test_add_workspace(main_project, window, cx)
        });
        cx.run_until_parked();
        let key = main_workspace.read_with(cx, |workspace, cx| workspace.project_group_key(cx));
        owner
            .update_in(cx, |owner, window, cx| {
                // Removing the active workspace would reopen its project.
                owner.activate(other_workspace.clone(), None, window, cx);
                owner.remove([main_workspace], RemovalIntent::KeepProject, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        other_panel
            .update_in(cx, |panel, window, cx| {
                let paths = PathList::new(&[Path::new("/other")]);
                panel.create_thread_from_prompt(Agent::Stub, paths, "hi".into(), None, window, cx)
            })
            .unwrap();
        settle(cx);
        let other_thread = other_panel.read_with(cx, |panel, cx| panel.active_thread_id(cx));
        let base_is_open = |cx: &mut VisualTestContext| {
            owner.read_with(cx, |owner, cx| {
                owner
                    .workspace_for_paths(key.path_list(), None, cx)
                    .is_some()
            })
        };

        cx.update(|window, cx| {
            show_form(
                sidebar.clone(),
                owner.clone(),
                Agent::Stub,
                Some(Destination {
                    project: key.clone(),
                    paths: key.path_list().clone(),
                    new_worktree: true,
                }),
                String::new(),
                None,
                None,
                window,
                cx,
            )
        });
        let modal = owner.read_with(cx, |owner, cx| {
            owner.active_modal::<NewThreadModal>(cx).unwrap()
        });
        settle(cx);
        assert_eq!(
            owner.read_with(cx, |owner, _| owner.workspace().clone()),
            other_workspace,
            "the base worktree should open in the background"
        );
        modal.update_in(cx, |modal, window, cx| modal.submit(window, cx));
        for _ in 0..8 {
            settle(cx);
        }
        let created = owner.read_with(cx, |owner, _| owner.workspace().clone());
        assert_ne!(created, other_workspace);
        assert!(
            !base_is_open(cx),
            "the base worktree should be closed once the new worktree exists"
        );

        let created_root =
            created.read_with(cx, |workspace, cx| workspace.root_paths(cx)[0].clone());
        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.update_entries(cx);
            let id = sidebar
                .contents
                .entries
                .iter()
                .find_map(|entry| match entry {
                    ListEntry::WorktreeHeader {
                        id,
                        path,
                        is_linked: true,
                        ..
                    } if Path::new(path.as_ref()) == created_root.as_ref() => Some(id.clone()),
                    _ => None,
                })
                .expect("the new worktree should have a header");
            sidebar.delete_worktree_group(&id, window, cx);
        });
        for _ in 0..8 {
            settle(cx);
        }
        assert_eq!(
            owner.read_with(cx, |owner, _| owner.workspace().clone()),
            other_workspace,
            "deleting the worktree should open the next thread"
        );
        assert!(
            !base_is_open(cx),
            "a workspace with nothing open should not be left behind"
        );
        sidebar.read_with(cx, |sidebar, _| {
            assert!(
                matches!(
                    &sidebar.active_entry,
                    Some(crate::ActiveEntry::Thread { thread_id, .. })
                        if Some(*thread_id) == other_thread
                ),
                "the next thread should be the sidebar's active entry, got {:?}",
                sidebar.active_entry
            );
        });
    }
}
