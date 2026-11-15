mod thread_switcher;

use acp_thread::ThreadStatus;
use action_log::DiffStats;
use agent::{ThreadStore, ZED_AGENT_ID};
use agent_client_protocol::v2::schema as acp;
use agent_settings::{AgentSettings, THREADS_LIST_MAX_WIDTH, THREADS_LIST_MIN_WIDTH};
use agent_ui::terminal_thread_metadata_store::{
    TerminalThreadMetadata, TerminalThreadMetadataStore, terminal_title_prefix,
};
use agent_ui::thread_metadata_store::{
    ThreadMetadata, ThreadMetadataStore, WorktreePaths, worktree_info_from_thread_paths,
};
use agent_ui::threads_archive_view::{
    ThreadsArchiveView, ThreadsArchiveViewEvent, format_history_entry_timestamp,
    fuzzy_match_positions,
};
use agent_ui::{
    AcpThreadImportOnboarding, Agent, AgentPanel, AgentPanelEvent, AgentThreadSource,
    ArchiveSelectedThread, CrossChannelImportOnboarding, DEFAULT_THREAD_TITLE, NewTerminalThread,
    RenameSelectedThread, TerminalId, ThreadId, ThreadImportModal, ThreadTitleRegenerationResult,
    channels_with_threads, import_threads_from_other_channels,
};
use agent_ui::{MessageEditorEvent, StateChange, thread_worktree_archive};
use chrono::{DateTime, Utc};
use editor::Editor;
use feature_flags::{
    AgentThreadWorktreeLabel, AgentThreadWorktreeLabelFlag, FeatureFlag, FeatureFlagAppExt as _,
};
use gpui::{
    Action as _, AnimationExt as _, AnyElement, App, ClickEvent, Context, Decorations,
    DismissEvent, DragMoveEvent, Entity, EntityId, FocusHandle, Focusable, KeyContext, ListState,
    Modifiers, Pixels, Point, Render, SharedString, Task, TaskExt, WeakEntity, Window,
    WindowBackgroundAppearance, WindowHandle, linear_color_stop, linear_gradient, list, prelude::*,
    px,
};
use itertools::Itertools;
use language_model::LanguageModelRegistry;
use menu::{
    Cancel, Confirm, SelectChild, SelectFirst, SelectLast, SelectNext, SelectParent, SelectPrevious,
};
use notifications::status_toast::StatusToast;
use platform_title_bar::{SidebarWindowControls, TitleBarSettings, apply_title_bar_insets};
use project::{AgentId, AgentRegistryStore, Event as ProjectEvent, WorktreeId};
use recent_projects::sidebar_recent_projects::SidebarRecentProjects;
use remote::RemoteConnectionOptions;
use ui::utils::platform_title_bar_height;

use serde::{Deserialize, Serialize};
use settings::{Settings as _, SettingsStore};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::mem;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use theme::{ActiveTheme, CLIENT_SIDE_DECORATION_ROUNDING};
use ui::{
    AgentThreadStatus, CommonAnimationExt, ContextMenu, ContextMenuEntry, Divider, GradientFade,
    HighlightedLabel, Indicator, KeyBinding, PopoverMenu, PopoverMenuHandle, ProjectEmptyState,
    SIDEBAR_ACTION_RIGHT_INSET, ScrollAxes, Scrollbars, Tab, ThreadItem, ThreadItemWorktreeInfo,
    TintColor, Tooltip, WithScrollbar, prelude::*, render_modifiers, right_click_menu,
};
use unicode_segmentation::UnicodeSegmentation as _;
use util::ResultExt as _;
use util::path_list::PathList;
use workspace::{
    CloseWindow, FocusWorkspaceSidebar, MoveProjectDown, MoveProjectUp, MultiWorkspace,
    MultiWorkspaceEvent, NextProject, NextThread, Open, OpenMode, PreviousProject, PreviousThread,
    ProjectGroupKey, RemovalIntent, SaveIntent, Sidebar as WorkspaceSidebar, SidebarSide, Toast,
    ToggleWorkspaceSidebar, Workspace, notifications::NotificationId, sidebar_side_context_menu,
};

use git_ui_core::notifications::show_error_toast;
use git_ui_core::worktree_service::{RemoteBranchName, worktree_create_targets};
use util::paths::PathStyle;
use zed_actions::editor::{MoveDown, MoveUp};
use zed_actions::{CreateWorktree, NewWorktreeBranchTarget, OpenRecent};

use zed_actions::agents_sidebar::{FocusSidebarFilter, ToggleThreadSwitcher};

use crate::thread_switcher::{
    ThreadSwitcher, ThreadSwitcherEntry, ThreadSwitcherEvent, ThreadSwitcherSelection,
    ThreadSwitcherTerminalEntry, ThreadSwitcherThreadEntry,
};

#[cfg(test)]
mod sidebar_tests;

gpui::actions!(
    agents_sidebar,
    [
        /// Creates a new thread in the currently selected or active project group.
        NewThreadInGroup,
        /// Toggles between the thread list and the thread history.
        ToggleThreadHistory,
    ]
);

gpui::actions!(
    dev,
    [
        /// Dumps multi-workspace state (projects, worktrees, active threads) into a new buffer.
        DumpWorkspaceInfo,
    ]
);

#[derive(Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum SerializedSidebarView {
    #[default]
    ThreadList,
    #[serde(alias = "Archive")]
    History,
}

#[derive(Clone, Copy)]
enum NewEntryTarget {
    LastCreatedKind,
    Terminal,
}

#[derive(Default, Serialize, Deserialize)]
struct SerializedSidebar {
    #[serde(default)]
    width: Option<f32>,
    /// Whether `width` came from the user dragging the divider.
    ///
    /// Legacy state recorded every width without this flag. A width other than
    /// the old default of 300 pixels still identifies a manual resize. Only
    /// that ambiguous default falls back to `agent.threads_sidebar.default_width`.
    #[serde(default)]
    width_set_by_user: bool,
    #[serde(default)]
    active_view: SerializedSidebarView,
    #[serde(default)]
    worktree_group_order: Vec<String>,
    #[serde(default)]
    collapsed_worktree_groups: Vec<String>,
    /// Root combinations of local multi-root projects. A worktree group of such a
    /// project has no other record once its threads and workspace are gone.
    #[serde(default)]
    multi_root_worktree_groups: Vec<SerializedWorktreeGroup>,
}

/// The main worktree and folder of each root, in the same order.
#[derive(Serialize, Deserialize)]
struct SerializedWorktreeGroup {
    main_paths: Vec<PathBuf>,
    folder_paths: Vec<PathBuf>,
}

#[derive(Debug, Default)]
enum SidebarView {
    #[default]
    ThreadList,
    Archive(Entity<ThreadsArchiveView>),
}

enum ArchiveWorktreeOutcome {
    Success,
    Cancelled,
}

#[derive(Clone, Debug)]
enum ActiveEntry {
    Thread {
        thread_id: agent_ui::ThreadId,
        /// Stable remote identifier, used for matching when thread_id
        /// differs (e.g. after cross-window activation creates a new
        /// local ThreadId).
        session_id: Option<acp::SessionId>,
        workspace: Entity<Workspace>,
    },
    Terminal {
        terminal_id: TerminalId,
        workspace: Entity<Workspace>,
    },
}

impl ActiveEntry {
    fn workspace(&self) -> &Entity<Workspace> {
        match self {
            ActiveEntry::Thread { workspace, .. } | ActiveEntry::Terminal { workspace, .. } => {
                workspace
            }
        }
    }

    fn is_active_thread(&self, thread_id: &agent_ui::ThreadId) -> bool {
        matches!(self, ActiveEntry::Thread { thread_id: active_thread_id, .. } if active_thread_id == thread_id)
    }

    fn is_active_terminal(&self, terminal_id: TerminalId) -> bool {
        matches!(self, ActiveEntry::Terminal { terminal_id: active_terminal_id, .. } if *active_terminal_id == terminal_id)
    }

    fn matches_entry(&self, entry: &ListEntry) -> bool {
        match (self, entry) {
            (
                ActiveEntry::Thread {
                    thread_id,
                    session_id,
                    ..
                },
                ListEntry::Thread(thread),
            ) => {
                *thread_id == thread.metadata.thread_id
                    || session_id
                        .as_ref()
                        .zip(thread.metadata.session_id.as_ref())
                        .is_some_and(|(a, b)| a == b)
            }
            (ActiveEntry::Terminal { terminal_id, .. }, ListEntry::Terminal(terminal)) => {
                *terminal_id == terminal.metadata.terminal_id
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug)]
struct ActiveThreadInfo {
    session_id: acp::SessionId,
    title: SharedString,
    status: AgentThreadStatus,
    icon: IconName,
    icon_from_external_svg: Option<SharedString>,
    is_background: bool,
    is_title_generating: bool,
    diff_stats: DiffStats,
}

#[derive(Clone)]
enum ThreadEntryWorkspace {
    Open(Entity<Workspace>),
    Closed {
        /// The paths this entry uses (may point to linked worktrees).
        folder_paths: PathList,
        /// The project group this entry belongs to.
        project_group_key: ProjectGroupKey,
    },
}

impl ThreadEntryWorkspace {
    fn is_remote(&self, cx: &App) -> bool {
        match self {
            ThreadEntryWorkspace::Open(workspace) => {
                !workspace.read(cx).project().read(cx).is_local()
            }
            ThreadEntryWorkspace::Closed {
                project_group_key, ..
            } => project_group_key.host().is_some(),
        }
    }
}

/// If the title begins with a decorative prefix (such as a leading emoji,
/// spinner glyph, or symbol the agent prefixed the title with), splits that
/// prefix off so a single representative glyph can be displayed in place of the
/// entry's icon.
fn split_leading_icon_char(
    title: &SharedString,
    highlight_positions: &[usize],
) -> Option<(SharedString, SharedString, Vec<usize>)> {
    let prefix = terminal_title_prefix(title)?;
    let icon_char = pick_icon_glyph(prefix)?;

    let stripped_len = prefix.len();
    let trimmed_title = &title[stripped_len..];
    if trimmed_title.is_empty() {
        return None;
    }

    let adjusted_positions = highlight_positions
        .iter()
        .filter(|&&position| position >= stripped_len)
        .map(|&position| position - stripped_len)
        .collect();

    Some((
        icon_char,
        trimmed_title.to_string().into(),
        adjusted_positions,
    ))
}

/// Picks a single glyph to render as the icon from a detected title prefix.
///
/// We only ever show one glyph, so this makes a best effort to choose a
/// meaningful one by glancing at the leading characters of the prefix:
/// runs of `.` are condensed into a single ellipsis, surrounding ASCII brackets
/// are stripped (so `[!]` yields `!`), and a leading run of the same character
/// is collapsed (so `>>>` yields `>`). The result is the first grapheme cluster
/// of whatever remains, keeping multi-codepoint emoji intact.
fn pick_icon_glyph(prefix: &str) -> Option<SharedString> {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return None;
    }

    // Strip a single pair of surrounding ASCII brackets, e.g. `[!]` -> `!`.
    let unwrapped = match prefix.chars().next() {
        Some('[') => prefix.strip_prefix('[').and_then(|s| s.strip_suffix(']')),
        Some('(') => prefix.strip_prefix('(').and_then(|s| s.strip_suffix(')')),
        Some('{') => prefix.strip_prefix('{').and_then(|s| s.strip_suffix('}')),
        Some('<') => prefix.strip_prefix('<').and_then(|s| s.strip_suffix('>')),
        _ => None,
    };
    let prefix = unwrapped
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(prefix);

    // Condense a leading run of dots (`...`) into a single ellipsis.
    if prefix.starts_with("..") {
        return Some("\u{2026}".into());
    }

    // Take the first grapheme cluster so multi-codepoint emoji stay intact.
    let first_grapheme = prefix.graphemes(true).next()?;
    if first_grapheme.trim().is_empty() {
        return None;
    }

    Some(first_grapheme.to_string().into())
}

fn draft_display_label_for_thread_metadata(
    metadata: &ThreadMetadata,
    workspace: &ThreadEntryWorkspace,
    fallback_workspace: Option<&Entity<Workspace>>,
    cx: &App,
) -> Option<(SharedString, DraftKind)> {
    let workspace = match workspace {
        ThreadEntryWorkspace::Open(workspace) => Some(workspace),
        ThreadEntryWorkspace::Closed { .. } => None,
    };

    if let Some(label) =
        agent_ui::draft_prompt_store::display_label_for_draft(workspace, metadata.thread_id, cx)
    {
        return Some((label, DraftKind::WithContent));
    }

    // A closed workspace has no agent registry, so its placeholder would show the raw
    // agent id until the workspace opens.
    let placeholder = agent_ui::draft_prompt_store::empty_draft_placeholder_label(
        workspace.or(fallback_workspace),
        &metadata.agent_id,
        cx,
    );
    Some((placeholder, DraftKind::Empty))
}

fn thread_metadata_would_render_sidebar_row(
    metadata: &ThreadMetadata,
    workspace: &ThreadEntryWorkspace,
    cx: &App,
) -> bool {
    if !metadata.is_draft() {
        return true;
    }

    draft_display_label_for_thread_metadata(metadata, workspace, None, cx).is_some()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DraftKind {
    WithContent,
    Empty,
}

#[derive(Clone)]
struct ThreadEntry {
    metadata: ThreadMetadata,
    icon: IconName,
    icon_from_external_svg: Option<SharedString>,
    status: AgentThreadStatus,
    workspace: ThreadEntryWorkspace,
    is_live: bool,
    is_background: bool,
    is_title_generating: bool,
    draft: Option<DraftKind>,
    highlight_positions: Vec<usize>,
    worktrees: Vec<ThreadItemWorktreeInfo>,
    diff_stats: DiffStats,
}

#[derive(Clone)]
struct TerminalEntry {
    metadata: TerminalThreadMetadata,
    workspace: ThreadEntryWorkspace,
    worktrees: Vec<ThreadItemWorktreeInfo>,
    has_notification: bool,
    highlight_positions: Vec<usize>,
}

impl ThreadEntry {
    /// Updates this thread entry with active thread information.
    ///
    /// The existing [`ThreadEntry`] was likely deserialized from the database
    /// but if we have a correspond thread already loaded we want to apply the
    /// live information.
    fn apply_active_info(&mut self, info: &ActiveThreadInfo) {
        self.metadata.title = Some(info.title.clone());
        self.status = info.status;
        self.icon = info.icon;
        self.icon_from_external_svg = info.icon_from_external_svg.clone();
        self.is_live = true;
        self.is_background = info.is_background;
        self.is_title_generating = info.is_title_generating;
        self.diff_stats = info.diff_stats;
    }
}

#[derive(Clone)]
enum ListEntry {
    ProjectHeader {
        key: ProjectGroupKey,
        label: SharedString,
        highlight_positions: Vec<usize>,
        has_running_threads: bool,
        waiting_thread_count: usize,
        has_notifications: bool,
        is_active: bool,
        has_threads: bool,
    },
    WorktreeHeader {
        /// Unique across projects, unlike `path`. See [`worktree_group_id`].
        id: SharedString,
        path: SharedString,
        /// Every root of the group, in order. `path` is the first.
        roots: Vec<SharedString>,
        label: SharedString,
        branch: Option<SharedString>,
        extra_worktrees: Vec<(SharedString, Option<SharedString>)>,
        /// A linked git worktree that still exists, so it can be deleted.
        is_linked: bool,
        has_rows: bool,
        is_collapsed: bool,
        /// The rows hidden by collapsing, which the thread switcher still lists.
        collapsed_rows: Vec<ListEntry>,
        previous: Option<SharedString>,
        next: Option<SharedString>,
        has_running_threads: bool,
        waiting_thread_count: usize,
        has_active_agent: bool,
        has_notifications: bool,
        pull_request_targets: Vec<(PathBuf, SharedString)>,
        pull_request: Option<HeaderPullRequest>,
    },
    Thread(Arc<ThreadEntry>),
    Terminal(TerminalEntry),
}

#[derive(Clone)]
struct DraggedWorktreeHeader {
    id: SharedString,
    label: SharedString,
}

impl Render for DraggedWorktreeHeader {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        dragged_header_preview(self.label.clone(), cx)
    }
}

const PULL_REQUEST_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
const PULL_REQUEST_REFRESH_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(2);

/// Ordered by how much attention a state needs: a header with several pull requests shows the first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PullRequestState {
    Open,
    Draft,
    Closed,
    Merged,
}

#[derive(Clone, Debug, PartialEq)]
struct WorktreePullRequest {
    branch: SharedString,
    state_and_url: Option<(PullRequestState, SharedString)>,
}

#[derive(Clone, Debug, PartialEq)]
struct HeaderPullRequest {
    urls: Vec<SharedString>,
    state: PullRequestState,
}

fn header_pull_request(
    targets: &[(PathBuf, SharedString)],
    worktree_pull_requests: &HashMap<PathBuf, WorktreePullRequest>,
) -> Option<HeaderPullRequest> {
    let (states, urls): (Vec<_>, Vec<_>) = targets
        .iter()
        .filter_map(|(root, branch)| {
            let cached = worktree_pull_requests.get(root)?;
            if cached.branch != *branch {
                return None;
            }
            cached.state_and_url.clone()
        })
        .unzip();
    Some(HeaderPullRequest {
        state: states.into_iter().min()?,
        urls,
    })
}

fn parse_gh_pull_request(stdout: &[u8]) -> Option<(PullRequestState, SharedString)> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Output {
        state: String,
        is_draft: bool,
        url: String,
    }
    let output: Output = serde_json::from_slice(stdout).ok()?;
    let state = match output.state.as_str() {
        "OPEN" if output.is_draft => PullRequestState::Draft,
        "OPEN" => PullRequestState::Open,
        "MERGED" => PullRequestState::Merged,
        "CLOSED" => PullRequestState::Closed,
        _ => return None,
    };
    Some((state, output.url.into()))
}

/// `gh auth status` would also fail for a broken account on a host these lookups never use.
async fn gh_is_authenticated(environment: &HashMap<String, String>) -> bool {
    util::command::new_command("gh")
        .args(["auth", "token"])
        .envs(environment)
        .output()
        .await
        .is_ok_and(|output| output.status.success())
}

/// No branch is passed: given a bare branch name, `gh` misses pull requests opened from a fork.
async fn look_up_pull_request(
    worktree_path: &Path,
    environment: &HashMap<String, String>,
) -> anyhow::Result<Option<(PullRequestState, SharedString)>> {
    let output = util::command::new_command("gh")
        .args(["pr", "view", "--json", "state,isDraft,url"])
        .envs(environment)
        .current_dir(worktree_path)
        .kill_on_drop(true)
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no pull requests found") {
            return Ok(None);
        }
        anyhow::bail!("gh pr view failed: {}", stderr.trim());
    }
    match parse_gh_pull_request(&output.stdout) {
        Some(pull_request) => Ok(Some(pull_request)),
        None => anyhow::bail!(
            "unexpected gh pr view output: {}",
            String::from_utf8_lossy(&output.stdout).trim()
        ),
    }
}

fn worktree_group_id(key: &ProjectGroupKey, path: &str) -> SharedString {
    format!("{key:?}\n{path}").into()
}

/// Roots are sorted so the id doesn't depend on the order a thread stored them in.
fn worktree_group_id_for_roots<'a>(
    key: &ProjectGroupKey,
    paths: impl IntoIterator<Item = &'a str>,
) -> SharedString {
    let mut paths: Vec<&str> = paths.into_iter().collect();
    paths.sort_unstable();
    worktree_group_id(key, &paths.join("\0"))
}

fn worktree_group_project(id: &str) -> &str {
    id.rsplit_once('\n').map_or(id, |(project, _)| project)
}

fn worktree_group_ids(entries: &[ListEntry]) -> impl DoubleEndedIterator<Item = &SharedString> {
    entries.iter().filter_map(|entry| match entry {
        ListEntry::WorktreeHeader { id, .. } => Some(id),
        _ => None,
    })
}

fn collapsed_group_status(
    waiting_indicator_id: impl Into<gpui::ElementId>,
    has_running_threads: bool,
    waiting_thread_count: usize,
    has_notifications: bool,
) -> Vec<AnyElement> {
    let mut indicators = Vec::new();
    if has_running_threads {
        indicators.push(
            Icon::new(IconName::LoadCircle)
                .size(IconSize::XSmall)
                .color(Color::Muted)
                .with_rotate_animation(2)
                .into_any_element(),
        );
    }
    if waiting_thread_count > 0 {
        let tooltip_text = if waiting_thread_count == 1 {
            "1 thread is waiting for confirmation".to_string()
        } else {
            format!("{waiting_thread_count} threads are waiting for confirmation")
        };
        indicators.push(
            div()
                .id(waiting_indicator_id)
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::XSmall)
                        .color(Color::Warning),
                )
                .tooltip(Tooltip::text(tooltip_text))
                .into_any_element(),
        );
    }
    if has_notifications && !has_running_threads && waiting_thread_count == 0 {
        indicators.push(
            Icon::new(IconName::Circle)
                .size(IconSize::Small)
                .color(Color::Accent)
                .into_any_element(),
        );
    }
    indicators
}

fn dragged_header_preview(label: SharedString, cx: &App) -> impl IntoElement {
    let color = cx.theme().colors();
    h_flex()
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(color.border)
        .bg(color.elevated_surface_background)
        .shadow_md()
        .child(Label::new(label))
}

#[derive(Clone)]
struct DraggedProjectHeader {
    key: ProjectGroupKey,
    label: SharedString,
}

impl Render for DraggedProjectHeader {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        dragged_header_preview(self.label.clone(), cx)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RenameTarget {
    Thread(ThreadId),
    Terminal(TerminalId),
    Project(ProjectGroupKey),
}

impl RenameTarget {
    fn from_entry(entry: &ListEntry) -> Option<(Self, SharedString)> {
        match entry {
            ListEntry::Thread(thread) => Some((
                Self::Thread(thread.metadata.thread_id),
                thread.metadata.display_title(),
            )),
            ListEntry::Terminal(terminal) => Some((
                Self::Terminal(terminal.metadata.terminal_id),
                terminal.metadata.editable_title(),
            )),
            ListEntry::ProjectHeader { key, label, .. } => {
                Some((Self::Project(key.clone()), label.clone()))
            }
            ListEntry::WorktreeHeader { .. } => None,
        }
    }
}

#[derive(Clone)]
enum ActivatableEntry {
    Thread {
        metadata: ThreadMetadata,
    },
    Terminal {
        metadata: TerminalThreadMetadata,
        workspace: ThreadEntryWorkspace,
    },
}

impl ActivatableEntry {
    fn from_list_entry(entry: &ListEntry) -> Option<Self> {
        match entry {
            ListEntry::Thread(thread) => Some(Self::Thread {
                metadata: thread.metadata.clone(),
            }),
            ListEntry::Terminal(terminal) => Some(Self::Terminal {
                metadata: terminal.metadata.clone(),
                workspace: terminal.workspace.clone(),
            }),
            ListEntry::ProjectHeader { .. } | ListEntry::WorktreeHeader { .. } => None,
        }
    }
}

#[cfg(test)]
impl ListEntry {
    fn session_id(&self) -> Option<&acp::SessionId> {
        match self {
            ListEntry::Thread(thread_entry) => thread_entry.metadata.session_id.as_ref(),
            ListEntry::Terminal(_)
            | ListEntry::ProjectHeader { .. }
            | ListEntry::WorktreeHeader { .. } => None,
        }
    }

    fn reachable_workspaces<'a>(
        &'a self,
        multi_workspace: &'a workspace::MultiWorkspace,
        cx: &'a App,
    ) -> Vec<Entity<Workspace>> {
        match self {
            ListEntry::Thread(thread) => match &thread.workspace {
                ThreadEntryWorkspace::Open(ws) => vec![ws.clone()],
                ThreadEntryWorkspace::Closed { .. } => Vec::new(),
            },
            ListEntry::Terminal(terminal) => match &terminal.workspace {
                ThreadEntryWorkspace::Open(workspace) => vec![workspace.clone()],
                ThreadEntryWorkspace::Closed { .. } => Vec::new(),
            },
            ListEntry::ProjectHeader { key, .. } => multi_workspace
                .workspaces_for_project_group(key, cx)
                .cloned()
                .collect(),
            ListEntry::WorktreeHeader { .. } => Vec::new(),
        }
    }
}

impl From<ThreadEntry> for ListEntry {
    fn from(thread: ThreadEntry) -> Self {
        ListEntry::Thread(Arc::new(thread))
    }
}

impl From<TerminalEntry> for ListEntry {
    fn from(terminal: TerminalEntry) -> Self {
        ListEntry::Terminal(terminal)
    }
}

#[derive(Default)]
struct SidebarContents {
    entries: Vec<ListEntry>,
    notified_threads: HashSet<agent_ui::ThreadId>,
    notified_terminals: HashSet<TerminalId>,
    project_header_indices: Vec<usize>,
    has_open_projects: bool,
}

/// Identity-and-layout key for a [`ListEntry`] used to preserve measured list items
/// across rebuilds. Equal shapes must render to the same height; add any new
/// height-affecting state here.
#[derive(Debug, PartialEq, Eq)]
enum EntryShape {
    ProjectHeader {
        key: ProjectGroupKey,
        has_threads: bool,
        is_collapsed: bool,
    },
    WorktreeHeader {
        id: SharedString,
        // The "No threads yet" row is rendered when `!is_collapsed && !has_rows`.
        has_rows: bool,
        is_collapsed: bool,
    },
    Thread(ThreadId),
    Terminal(TerminalId),
}

impl SidebarContents {
    fn is_thread_notified(&self, thread_id: &agent_ui::ThreadId) -> bool {
        self.notified_threads.contains(thread_id)
    }

    fn is_terminal_notified(&self, terminal_id: TerminalId) -> bool {
        self.notified_terminals.contains(&terminal_id)
    }
}

// TODO: The mapping from workspace root paths to git repositories needs a
// unified approach across the codebase: this function, `AgentPanel::classify_worktrees`,
// thread persistence (which PathList is saved to the database), and thread
// querying (which PathList is used to read threads back). All of these need
// to agree on how repos are resolved for a given workspace, especially in
// multi-root and nested-repo configurations.
fn root_repository_snapshots(
    workspace: &Entity<Workspace>,
    cx: &App,
) -> impl Iterator<Item = project::git_store::RepositorySnapshot> {
    let path_list = workspace_path_list(workspace, cx);
    let project = workspace.read(cx).project().read(cx);
    project.repositories(cx).values().filter_map(move |repo| {
        let snapshot = repo.read(cx).snapshot();
        let is_root = path_list
            .paths()
            .iter()
            .any(|p| p.as_path() == snapshot.work_directory_abs_path.as_ref());
        is_root.then_some(snapshot)
    })
}

/// Whether git has reported on any of the workspaces' root repositories yet.
/// Until then, a closed project's worktree listing stays in use.
fn has_live_git_state(workspaces: &[Entity<Workspace>], cx: &App) -> bool {
    workspaces.iter().any(|workspace| {
        root_repository_snapshots(workspace, cx)
            .any(|snapshot| snapshot.branch.is_some() || snapshot.head_commit.is_some())
    })
}

fn workspace_path_list(workspace: &Entity<Workspace>, cx: &App) -> PathList {
    PathList::new(&workspace.read(cx).root_paths(cx))
}

fn find_workspace_for_thread(
    multi_workspace: &MultiWorkspace,
    thread: &ThreadMetadata,
    cx: &App,
) -> Option<Entity<Workspace>> {
    multi_workspace
        .workspaces()
        .find(|workspace| {
            if workspace_path_list(workspace, cx) != *thread.folder_paths() {
                return false;
            }

            thread.matches_remote_connection(
                workspace
                    .read(cx)
                    .project()
                    .read(cx)
                    .remote_connection_options(cx),
            )
        })
        .cloned()
}

const WORKTREE_GROUP_INDENT: f32 = 8.;

fn linked_worktree_path_lists_for_workspaces(
    workspaces: &[Entity<Workspace>],
    main_paths: &[PathBuf],
    cx: &App,
) -> Vec<PathList> {
    let mut linked_worktree_paths: Vec<PathBuf> = Vec::new();
    for workspace in workspaces {
        if workspace.read(cx).visible_worktrees(cx).count() != 1 {
            continue;
        }
        for snapshot in root_repository_snapshots(workspace, cx) {
            // A repository's listing leaves out its own worktree, so a project with
            // only a linked worktree open would otherwise not list that worktree.
            linked_worktree_paths.push(snapshot.work_directory_abs_path.to_path_buf());
            linked_worktree_paths.extend(
                snapshot
                    .linked_worktrees()
                    .iter()
                    .filter(|worktree| !worktree.is_main && !worktree.is_bare)
                    .map(|worktree| worktree.path.clone()),
            );
        }
    }

    linked_worktree_paths.retain(|path| !main_paths.contains(path));
    linked_worktree_paths.sort();
    linked_worktree_paths.dedup();
    linked_worktree_paths
        .iter()
        .map(|path| PathList::new(std::slice::from_ref(path)))
        .collect()
}

#[derive(Clone)]
struct WorkspaceMenuWorktreeLabel {
    icon: Option<IconName>,
    primary_name: SharedString,
    secondary_name: Option<SharedString>,
}

impl WorkspaceMenuWorktreeLabel {
    fn render(&self) -> impl IntoElement {
        h_flex()
            .min_w_0()
            .gap_0p5()
            .when_some(self.icon, |this, icon| {
                this.child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
            })
            .child(Label::new(self.primary_name.clone()).truncate())
            .when_some(self.secondary_name.clone(), |this, secondary_name| {
                this.child(Label::new("/").alpha(0.5))
                    .child(Label::new(secondary_name).truncate())
            })
    }
}

#[derive(Clone)]
struct WorktreeTarget {
    main_path: PathBuf,
    path: PathBuf,
}

/// A worktree group being deleted. `path` is the group's first root, which hides it.
#[derive(Clone)]
struct WorktreeDeletion {
    path: PathBuf,
    roots: Vec<PathBuf>,
    targets: Vec<WorktreeTarget>,
}

/// Marks a worktree as being deleted until dropped, however the deletion ends.
struct DeletingWorktree {
    paths: Rc<RefCell<HashSet<PathBuf>>>,
    deleting: Vec<PathBuf>,
}

impl Drop for DeletingWorktree {
    fn drop(&mut self) {
        let mut paths = self.paths.borrow_mut();
        for path in &self.deleting {
            paths.remove(path);
        }
    }
}

/// A worktree that gets a header even when it has no rows.
struct KnownWorktree {
    path: SharedString,
    roots: Vec<SharedString>,
    label: SharedString,
    is_linked: bool,
    extra_worktrees: Vec<(SharedString, Option<SharedString>)>,
}

fn workspace_menu_worktree_labels(
    workspace: &Entity<Workspace>,
    cx: &App,
) -> Vec<WorkspaceMenuWorktreeLabel> {
    let root_paths = workspace.read(cx).root_paths(cx);
    let show_folder_name = root_paths.len() > 1;
    let project = workspace.read(cx).project().clone();
    let (path_style, repository_snapshots) = {
        let project = project.read(cx);
        let path_style = project.path_style(cx);
        let repository_snapshots = project
            .repositories(cx)
            .values()
            .map(|repo| repo.read(cx).snapshot())
            .collect::<Vec<_>>();
        (path_style, repository_snapshots)
    };

    root_paths
        .into_iter()
        .map(|root_path| {
            let root_path = root_path.as_ref();
            let folder_name = path_style
                .file_name(root_path)
                .map(|name| SharedString::from(name.to_string_lossy().into_owned()))
                .unwrap_or_default();
            let repository_snapshot = repository_snapshots
                .iter()
                .find(|snapshot| snapshot.work_directory_abs_path.as_ref() == root_path);

            if let Some(snapshot) = repository_snapshot {
                let worktree_name = if snapshot.is_linked_worktree() {
                    project::linked_worktree_short_name(
                        snapshot.linked_worktree_anchor_path(),
                        root_path,
                        snapshot.path_style,
                    )
                    .unwrap_or_else(|| folder_name.clone())
                } else {
                    "main".into()
                };

                if show_folder_name {
                    WorkspaceMenuWorktreeLabel {
                        icon: Some(IconName::GitWorktree),
                        primary_name: folder_name,
                        secondary_name: Some(worktree_name),
                    }
                } else {
                    WorkspaceMenuWorktreeLabel {
                        icon: Some(IconName::GitWorktree),
                        primary_name: worktree_name,
                        secondary_name: None,
                    }
                }
            } else {
                WorkspaceMenuWorktreeLabel {
                    icon: None,
                    primary_name: folder_name,
                    secondary_name: None,
                }
            }
        })
        .collect()
}

fn apply_worktree_label_mode(
    mut worktrees: Vec<ThreadItemWorktreeInfo>,
    mode: AgentThreadWorktreeLabel,
) -> Vec<ThreadItemWorktreeInfo> {
    match mode {
        AgentThreadWorktreeLabel::Both => {}
        AgentThreadWorktreeLabel::Worktree => {
            for wt in &mut worktrees {
                wt.branch_name = None;
            }
        }
        AgentThreadWorktreeLabel::Branch => {
            for wt in &mut worktrees {
                // Fall back to showing the worktree name when no branch is
                // known; an empty chip would be worse.
                if wt.branch_name.is_some() {
                    wt.worktree_name = None;
                }
            }
        }
    }
    worktrees
}

/// Shows a [`RemoteConnectionModal`] on the given workspace and establishes
/// an SSH connection. Suitable for passing to
/// [`MultiWorkspace::find_or_create_workspace`] as the `connect_remote`
/// argument.
fn connect_remote(
    modal_workspace: Entity<Workspace>,
    connection_options: RemoteConnectionOptions,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) -> gpui::Task<anyhow::Result<Option<Entity<remote::RemoteClient>>>> {
    remote_connection::connect_with_modal(&modal_workspace, connection_options, window, cx)
}

// Mirrors the behavior of the worktree picker's "Create new worktree" entries.
fn create_worktree_in_workspace(
    workspace: &Entity<Workspace>,
    branch_target: NewWorktreeBranchTarget,
    window: &mut Window,
    cx: &mut App,
) -> gpui::Task<anyhow::Result<Entity<Workspace>>> {
    let creation = workspace.update(cx, |workspace, cx| {
        let focused_dock = workspace.focused_dock_position(window, cx);
        git_ui_core::worktree_service::create_worktree_workspace_and_activate(
            workspace,
            &CreateWorktree {
                worktree_name: None,
                branch_target,
            },
            window,
            focused_dock,
            cx,
        )
    });
    cx.spawn(async move |_| Ok(creation.await?.workspace))
}

/// The sidebar re-derives its entire entry list from scratch on every
/// change via `update_entries` → `rebuild_contents`. Avoid adding
/// incremental or inter-event coordination state — if something can
/// be computed from the current world state, compute it in the rebuild.
pub struct Sidebar {
    multi_workspace: WeakEntity<MultiWorkspace>,
    width: Pixels,
    /// Whether `width` came from the user rather than from
    /// `agent.threads_sidebar.default_width`. Only a user-chosen width is persisted, so
    /// that changing the setting is not overridden by a width the user never
    /// picked. Serialization runs on many triggers besides resizing.
    width_set_by_user: bool,
    focus_handle: FocusHandle,
    filter_editor: Entity<Editor>,
    rename_editor: Entity<Editor>,
    list_state: ListState,
    contents: SidebarContents,
    /// The index of the list item that currently has the keyboard focus
    ///
    /// Note: This is NOT the same as the active item.
    selection: Option<usize>,
    /// Tracks which sidebar entry is currently active (highlighted).
    active_entry: Option<ActiveEntry>,
    hovered_thread_index: Option<usize>,
    rename_target: Option<RenameTarget>,
    /// The group a dragged project header would land on, and whether it
    /// lands below that group rather than above it.
    project_drop_target: Option<(ProjectGroupKey, bool)>,
    worktree_drop_target: Option<(SharedString, bool)>,
    /// Git's worktree listing for projects without an open workspace, which have
    /// no live git state.
    closed_group_worktrees: HashMap<ProjectGroupKey, Vec<git::repository::Worktree>>,
    groups_creating_worktree: HashSet<ProjectGroupKey>,
    closed_group_worktree_tasks: HashMap<ProjectGroupKey, Task<()>>,
    worktree_pull_requests: HashMap<PathBuf, WorktreePullRequest>,
    worktree_pull_request_tasks: HashMap<PathBuf, (SharedString, Task<()>)>,
    gh_available: Option<bool>,
    gh_environment: Arc<HashMap<String, String>>,
    gh_availability_check: Option<Task<()>>,
    pull_request_refresh: Option<Task<()>>,
    _pull_request_poll: Task<()>,
    deleting_worktree_paths: Rc<RefCell<HashSet<PathBuf>>>,
    /// Display order of worktree group ids. One list serves every project.
    worktree_group_order: Vec<SharedString>,
    collapsed_worktree_groups: HashSet<SharedString>,
    /// Root combinations seen in multi-root projects, so that their worktree groups
    /// outlive their rows and workspaces.
    multi_root_worktree_groups: Vec<(ProjectGroupKey, WorktreePaths)>,
    /// Restored groups may have been deleted from disk while Zed was closed.
    multi_root_worktree_groups_unchecked: bool,
    initial_scan_shown: bool,
    multi_root_worktree_groups_check: Option<Task<()>>,
    unlisted_worktrees_check: Option<Task<()>>,
    /// Removed worktrees the user still wants: their workspace was not allowed to close,
    /// or one of their threads was restored. They are left alone until Zed restarts.
    unlisted_worktrees_kept_open: HashSet<PathBuf>,
    /// Threads in the database-backed regeneration path need their own loading
    /// state because they do not have a live `agent::Thread` to report it.
    regenerating_titles: HashSet<ThreadId>,
    /// Starting a rename must seed the current title into the title editor,
    /// so this prevents that BufferEdited event from being interpreted as user input.
    suppress_next_rename_edit: bool,

    /// Updated only in response to explicit user actions (clicking a
    /// thread, confirming in the thread switcher, etc.) — never from
    /// background data changes. Used to sort the thread switcher popup.
    thread_last_accessed: HashMap<ThreadId, DateTime<Utc>>,
    terminal_last_accessed: HashMap<TerminalId, DateTime<Utc>>,
    thread_switcher: Option<Entity<ThreadSwitcher>>,
    _thread_switcher_subscriptions: Vec<gpui::Subscription>,
    pending_thread_activation: Option<agent_ui::ThreadId>,
    /// Persists live thread statuses across rebuilds so that Running→Completed
    /// transitions can be detected even when the group is collapsed (and
    /// thread entries are not present in the list).
    live_thread_statuses: HashMap<acp::SessionId, (AgentThreadStatus, ThreadId)>,
    /// Remembers whether each draft last rendered as empty or with content so
    /// that when a draft that was empty gains content again, we refresh
    /// its interaction time.
    draft_kinds: HashMap<ThreadId, DraftKind>,
    view: SidebarView,
    restoring_tasks: HashMap<agent_ui::ThreadId, Task<()>>,
    recent_projects_popover_handle: PopoverMenuHandle<SidebarRecentProjects>,
    project_header_menu_handles: HashMap<usize, PopoverMenuHandle<ContextMenu>>,
    worktree_header_menu_handles: HashMap<usize, PopoverMenuHandle<ContextMenu>>,
    project_header_menu_ix: Option<usize>,
    _subscriptions: Vec<gpui::Subscription>,
    _draft_editor_observations: Vec<gpui::Subscription>,
    update_task: Option<Task<()>>,
    /// For the thread import banners, if there is just one we show "Import
    /// Threads" but if we are showing both the external agents and other
    /// channels import banners then we change the text to disambiguate the
    /// buttons. This field tracks whether we were using verbose labels so they
    /// can stay stable after dismissing one of the banners.
    import_banners_use_verbose_labels: Option<bool>,
    /// Display names of other release channels that have threads available to
    /// import.
    cross_channel_import_channels: Vec<SharedString>,
}

impl Sidebar {
    pub fn new(
        multi_workspace: Entity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        cx.on_focus_in(&focus_handle, window, Self::focus_in)
            .detach();
        // Worktrees of closed projects may have changed outside Zed in the meantime.
        cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                if this.gh_available == Some(false) {
                    this.gh_available = None;
                }
                this.refresh_worktree_pull_requests(true, cx);
                this.fetch_closed_group_worktrees(true, cx);
                this.clean_up_unlisted_worktrees(window, cx);
            }
        })
        .detach();
        // Worktrees may also have been removed while Zed was closed.
        let threads_loaded = ThreadMetadataStore::global(cx).read(cx).reload_task();
        cx.spawn_in(window, async move |this, cx| {
            threads_loaded.await;
            this.update_in(cx, |this, window, cx| {
                this.clean_up_unlisted_worktrees(window, cx);
            })
            .ok();
        })
        .detach();

        AgentThreadWorktreeLabelFlag::watch(cx);

        let mut previous_default_width =
            AgentSettings::get_global(cx).threads_sidebar.default_width;
        cx.observe_global::<SettingsStore>(move |this, cx| {
            let width = AgentSettings::get_global(cx).threads_sidebar.default_width;
            if previous_default_width != width {
                previous_default_width = width;
                this.set_width(None, cx);
                this.serialize(cx);
            }
        })
        .detach();

        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search threads…", window, cx);
            editor
        });
        let rename_editor = cx.new(|cx| Editor::single_line(window, cx));

        cx.subscribe_in(
            &multi_workspace,
            window,
            |this, _multi_workspace, event: &MultiWorkspaceEvent, window, cx| match event {
                MultiWorkspaceEvent::ActiveWorkspaceChanged { .. } => {
                    this.sync_active_entry_from_active_workspace(cx);
                    this.replace_archived_panel_thread(window, cx);
                    this.schedule_update_entries(false, cx);
                }
                MultiWorkspaceEvent::WorkspaceAdded(workspace) => {
                    this.subscribe_to_workspace(workspace, window, cx);
                    this.schedule_update_entries(false, cx);
                }
                MultiWorkspaceEvent::WorkspaceRemoved(_)
                | MultiWorkspaceEvent::ProjectGroupsChanged => {
                    this.schedule_update_entries(false, cx);
                }
            },
        )
        .detach();

        cx.subscribe(&filter_editor, |this: &mut Self, _, event, cx| {
            if let editor::EditorEvent::BufferEdited = event {
                let query = this.filter_editor.read(cx).text(cx);
                if !query.is_empty() {
                    this.selection.take();
                }
                this.schedule_update_entries(!query.is_empty(), cx);
            }
        })
        .detach();

        cx.subscribe_in(
            &rename_editor,
            window,
            |this, title_editor, event, window, cx| {
                this.handle_rename_editor_event(title_editor, event, window, cx);
            },
        )
        .detach();

        cx.observe(&ThreadMetadataStore::global(cx), |this, _store, cx| {
            this.schedule_update_entries(false, cx);
        })
        .detach();

        cx.observe(
            &TerminalThreadMetadataStore::global(cx),
            |this, _store, cx| {
                this.schedule_update_entries(false, cx);
            },
        )
        .detach();

        let channels_with_threads = channels_with_threads(cx);
        cx.spawn(async move |this, cx| {
            let channels = channels_with_threads.await;
            this.update(cx, |this, cx| {
                this.cross_channel_import_channels = channels;
                cx.notify();
            })
            .ok();
        })
        .detach();

        let deferred_multi_workspace = multi_workspace.downgrade();
        cx.defer_in(window, move |this, window, cx| {
            if let Some(multi_workspace) = deferred_multi_workspace.upgrade() {
                let workspaces: Vec<_> = multi_workspace.read(cx).workspaces().cloned().collect();
                for workspace in &workspaces {
                    this.subscribe_to_workspace(workspace, window, cx);
                }
            }
            this.schedule_update_entries(false, cx);
        });

        Self {
            multi_workspace: multi_workspace.downgrade(),
            width: AgentSettings::get_global(cx).threads_sidebar.default_width,
            width_set_by_user: false,
            focus_handle,
            filter_editor,
            rename_editor,
            list_state: ListState::new(0, gpui::ListAlignment::Top, px(1000.)),
            contents: SidebarContents::default(),
            selection: None,
            active_entry: None,
            hovered_thread_index: None,
            rename_target: None,
            project_drop_target: None,
            worktree_drop_target: None,
            closed_group_worktrees: HashMap::new(),
            groups_creating_worktree: HashSet::new(),
            closed_group_worktree_tasks: HashMap::new(),
            worktree_pull_requests: HashMap::new(),
            worktree_pull_request_tasks: HashMap::new(),
            gh_available: None,
            gh_environment: Arc::default(),
            gh_availability_check: None,
            pull_request_refresh: None,
            _pull_request_poll: cx.spawn_in(window, async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(PULL_REQUEST_POLL_INTERVAL)
                        .await;
                    let refreshed = this.update_in(cx, |this, window, cx| {
                        if window.is_window_active() {
                            this.refresh_worktree_pull_requests(true, cx);
                        }
                    });
                    if refreshed.is_err() {
                        break;
                    }
                }
            }),
            deleting_worktree_paths: Rc::default(),
            worktree_group_order: Vec::new(),
            collapsed_worktree_groups: HashSet::new(),
            multi_root_worktree_groups: Vec::new(),
            multi_root_worktree_groups_unchecked: false,
            initial_scan_shown: false,
            multi_root_worktree_groups_check: None,
            unlisted_worktrees_check: None,
            unlisted_worktrees_kept_open: HashSet::new(),
            regenerating_titles: HashSet::new(),
            suppress_next_rename_edit: false,

            thread_last_accessed: HashMap::new(),
            terminal_last_accessed: HashMap::new(),
            thread_switcher: None,
            _thread_switcher_subscriptions: Vec::new(),
            pending_thread_activation: None,
            live_thread_statuses: HashMap::new(),
            draft_kinds: HashMap::new(),
            view: SidebarView::default(),
            restoring_tasks: HashMap::new(),
            recent_projects_popover_handle: PopoverMenuHandle::default(),
            project_header_menu_handles: HashMap::new(),
            worktree_header_menu_handles: HashMap::new(),
            project_header_menu_ix: None,
            _subscriptions: Vec::new(),
            _draft_editor_observations: Vec::new(),
            update_task: None,
            import_banners_use_verbose_labels: None,
            cross_channel_import_channels: Vec::new(),
        }
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        cx.emit(workspace::SidebarEvent::SerializeNeeded);
    }

    fn is_group_collapsed(&self, key: &ProjectGroupKey, cx: &App) -> bool {
        self.multi_workspace
            .upgrade()
            .and_then(|mw| {
                mw.read(cx)
                    .group_state_by_key(key)
                    .map(|state| !state.expanded)
            })
            .unwrap_or(false)
    }

    fn set_group_expanded(&self, key: &ProjectGroupKey, expanded: bool, cx: &mut Context<Self>) {
        if let Some(mw) = self.multi_workspace.upgrade() {
            mw.update(cx, |mw, cx| {
                if let Some(state) = mw.group_state_by_key_mut(key) {
                    state.expanded = expanded;
                }
                mw.serialize(cx);
            });
        }
    }

    fn is_active_workspace(&self, workspace: &Entity<Workspace>, cx: &App) -> bool {
        self.multi_workspace
            .upgrade()
            .map_or(false, |mw| mw.read(cx).workspace() == workspace)
    }

    fn subscribe_to_workspace(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project = workspace.read(cx).project().clone();
        if project.read(cx).is_via_collab() {
            return;
        }

        cx.subscribe_in(
            &project,
            window,
            |this, project, event, _window, cx| match event {
                ProjectEvent::WorktreeAdded(_)
                | ProjectEvent::WorktreeRemoved(_)
                | ProjectEvent::WorktreeOrderChanged => {
                    this.schedule_update_entries(false, cx);
                }
                ProjectEvent::WorktreePathsChanged { old_worktree_paths } => {
                    this.move_entry_paths(project, old_worktree_paths, cx);
                    this.schedule_update_entries(false, cx);
                }
                _ => {}
            },
        )
        .detach();

        let scan_wait = project
            .read(cx)
            .worktree_store()
            .read(cx)
            .wait_for_initial_scan();
        cx.spawn(async move |this, cx| {
            scan_wait.await;
            this.update(cx, |this, cx| this.update_entries(cx)).ok();
        })
        .detach();

        let git_store = workspace.read(cx).project().read(cx).git_store().clone();
        cx.subscribe_in(
            &git_store,
            window,
            |this, _, event: &project::git_store::GitStoreEvent, window, cx| {
                let project::git_store::GitStoreEvent::RepositoryUpdated(_, event, _) = event
                else {
                    return;
                };
                match event {
                    project::git_store::RepositoryEvent::GitWorktreeListChanged => {
                        this.clean_up_unlisted_worktrees(window, cx);
                        this.schedule_update_entries(false, cx);
                    }
                    project::git_store::RepositoryEvent::HeadChanged => {
                        this.schedule_update_entries(false, cx);
                        this.schedule_pull_request_refresh(cx);
                    }
                    project::git_store::RepositoryEvent::BranchListChanged => {
                        this.schedule_pull_request_refresh(cx);
                    }
                    _ => {}
                }
            },
        )
        .detach();

        cx.subscribe_in(
            workspace,
            window,
            move |this, workspace, event: &workspace::Event, window, cx| match event {
                workspace::Event::PanelAdded(view) => {
                    if let Ok(agent_panel) = view.clone().downcast::<AgentPanel>() {
                        this.subscribe_to_agent_panel(workspace, &agent_panel, window, cx);
                        this.schedule_update_entries(false, cx);
                    }
                }
                workspace::Event::WorktreeCreationChanged => cx.notify(),
                _ => {}
            },
        )
        .detach();

        self.observe_docks(workspace, cx);

        if let Some(agent_panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
            self.subscribe_to_agent_panel(workspace, &agent_panel, window, cx);
        }
    }

    fn move_entry_paths(
        &mut self,
        project: &Entity<project::Project>,
        old_paths: &WorktreePaths,
        cx: &mut Context<Self>,
    ) {
        if project.read(cx).is_via_collab() {
            return;
        }

        let new_paths = project.read(cx).worktree_paths(cx);
        let old_folder_paths = old_paths.folder_path_list().clone();

        let added_pairs: Vec<_> = new_paths
            .ordered_pairs()
            .filter(|(main, folder)| {
                !old_paths
                    .ordered_pairs()
                    .any(|(old_main, old_folder)| old_main == *main && old_folder == *folder)
            })
            .map(|(m, f)| (m.clone(), f.clone()))
            .collect();

        let new_folder_paths = new_paths.folder_path_list();
        let removed_folder_paths: Vec<PathBuf> = old_folder_paths
            .paths()
            .iter()
            .filter(|p| !new_folder_paths.paths().contains(p))
            .cloned()
            .collect();

        if added_pairs.is_empty() && removed_folder_paths.is_empty() {
            return;
        }

        let remote_connection = project.read(cx).remote_connection_options(cx).cloned();
        // The rows of the old roots move to the new ones, so the old group would
        // linger empty. It is saved again if another workspace still has those roots.
        let saved_group_count = self.multi_root_worktree_groups.len();
        self.multi_root_worktree_groups.retain(|(key, paths)| {
            key.host().map(|host| host.identity())
                != remote_connection.as_ref().map(|host| host.identity())
                || paths.folder_path_list().paths() != old_folder_paths.paths()
        });
        if self.multi_root_worktree_groups.len() != saved_group_count {
            self.serialize(cx);
        }
        let apply_path_changes = |paths: &mut WorktreePaths| {
            for (main_path, folder_path) in &added_pairs {
                paths.add_path(main_path, folder_path);
            }
            for path in &removed_folder_paths {
                paths.remove_folder_path(path);
            }
        };
        ThreadMetadataStore::global(cx).update(cx, |store, store_cx| {
            store.change_worktree_paths(
                &old_folder_paths,
                remote_connection.as_ref(),
                &apply_path_changes,
                store_cx,
            );
        });
        TerminalThreadMetadataStore::global(cx).update(cx, |store, store_cx| {
            store.change_worktree_paths(
                &old_folder_paths,
                remote_connection.as_ref(),
                &apply_path_changes,
                store_cx,
            );
        });
    }

    fn subscribe_to_agent_panel(
        &mut self,
        workspace: &Entity<Workspace>,
        agent_panel: &Entity<AgentPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = workspace.downgrade();
        cx.subscribe_in(
            agent_panel,
            window,
            move |this, agent_panel, event: &AgentPanelEvent, window, cx| match event {
                AgentPanelEvent::ActiveViewChanged
                | AgentPanelEvent::ActiveViewFocused
                | AgentPanelEvent::EntryChanged => {
                    this.sync_active_entry_from_panel(agent_panel, cx);
                    this.schedule_update_entries(false, cx);
                }
                AgentPanelEvent::TerminalCloseRequested { metadata } => {
                    if let Some(workspace) = workspace.upgrade() {
                        let workspace = ThreadEntryWorkspace::Open(workspace);
                        this.close_terminal(metadata, &workspace, window, cx);
                    }
                }
                AgentPanelEvent::ThreadInteracted { thread_id } => {
                    this.record_thread_interacted(thread_id, cx);
                    this.schedule_update_entries(false, cx);
                }
            },
        )
        .detach();
    }

    fn sync_active_entry_from_active_workspace(&mut self, cx: &App) {
        let panel = self
            .active_workspace(cx)
            .and_then(|ws| ws.read(cx).panel::<AgentPanel>(cx));
        if let Some(panel) = panel {
            self.sync_active_entry_from_panel(&panel, cx);
        }
    }

    /// When switching workspaces, the active panel may still be showing
    /// a thread that was archived from a different workspace. In that
    /// case, create a fresh draft so the panel has valid content and
    /// `active_entry` can point at it.
    fn replace_archived_panel_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
            return;
        };
        let Some(thread_id) = panel.read(cx).active_thread_id(cx) else {
            return;
        };
        let is_archived = ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .is_some_and(|m| m.archived);
        if is_archived {
            self.create_new_thread(&workspace, window, cx);
        }
    }

    /// Syncs `active_entry` from the agent panel's current state.
    /// Called from `ActiveViewChanged` — the panel has settled into its
    /// new view, so we can safely read it without race conditions.
    ///
    /// Also resolves `pending_thread_activation` when the panel's
    /// active thread matches the pending activation.
    fn sync_active_entry_from_panel(&mut self, agent_panel: &Entity<AgentPanel>, cx: &App) -> bool {
        let Some(active_workspace) = self.active_workspace(cx) else {
            return false;
        };

        // Only sync when the event comes from the active workspace's panel.
        let is_active_panel = active_workspace
            .read(cx)
            .panel::<AgentPanel>(cx)
            .is_some_and(|p| p == *agent_panel);
        if !is_active_panel {
            return false;
        }

        let panel = agent_panel.read(cx);

        if let Some(pending_thread_id) = self.pending_thread_activation {
            let panel_thread_id = panel
                .active_conversation_view()
                .map(|cv| cv.read(cx).parent_id());

            if panel_thread_id == Some(pending_thread_id) {
                let session_id = panel
                    .active_agent_thread(cx)
                    .map(|thread| thread.read(cx).session_id().clone());
                self.active_entry = Some(ActiveEntry::Thread {
                    thread_id: pending_thread_id,
                    session_id,
                    workspace: active_workspace,
                });
                self.pending_thread_activation = None;
                return true;
            }
            // Pending activation not yet resolved — keep current active_entry.
            return false;
        }

        if let Some(terminal_id) = panel.active_terminal_id() {
            self.active_entry = Some(ActiveEntry::Terminal {
                terminal_id,
                workspace: active_workspace,
            });
        } else if let Some(thread_id) = panel.active_thread_id(cx) {
            let is_archived = ThreadMetadataStore::global(cx)
                .read(cx)
                .entry(thread_id)
                .is_some_and(|m| m.archived);
            if !is_archived {
                let session_id = panel
                    .active_agent_thread(cx)
                    .map(|thread| thread.read(cx).session_id().clone());
                self.active_entry = Some(ActiveEntry::Thread {
                    thread_id,
                    session_id,
                    workspace: active_workspace,
                });
            }
        }

        false
    }

    fn observe_docks(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        let docks: Vec<_> = workspace
            .read(cx)
            .all_docks()
            .into_iter()
            .cloned()
            .collect();
        let workspace = workspace.downgrade();
        for dock in docks {
            let workspace = workspace.clone();
            cx.observe(&dock, move |this, _dock, cx| {
                let Some(workspace) = workspace.upgrade() else {
                    return;
                };
                if !this.is_active_workspace(&workspace, cx) {
                    return;
                }

                cx.notify();
            })
            .detach();
        }
    }

    /// Opens a new workspace for a group that has no open workspaces.
    fn open_workspace_for_group(
        &mut self,
        project_group_key: &ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let path_list = project_group_key.path_list().clone();
        let host = project_group_key.host().cloned();
        let provisional_key = Some(project_group_key.clone());
        let active_workspace = multi_workspace.read(cx).workspace().clone();
        let modal_workspace = active_workspace.clone();

        let task = multi_workspace.update(cx, |this, cx| {
            this.find_or_create_workspace(
                path_list,
                host,
                provisional_key,
                |options, window, cx| connect_remote(active_workspace, options, window, cx),
                None,
                OpenMode::Activate,
                None,
                window,
                cx,
            )
        });

        cx.spawn_in(window, async move |_this, cx| {
            let result = task.await;
            remote_connection::dismiss_connection_modal(&modal_workspace, cx);
            result?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn open_workspace_and_create_entry(
        &mut self,
        project_group_key: &ProjectGroupKey,
        path_list: PathList,
        target: NewEntryTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let host = project_group_key.host().cloned();
        let provisional_key = Some(project_group_key.clone());
        let active_workspace = multi_workspace.read(cx).workspace().clone();

        let task = multi_workspace.update(cx, |this, cx| {
            this.find_or_create_workspace(
                path_list,
                host,
                provisional_key,
                |options, window, cx| connect_remote(active_workspace, options, window, cx),
                None,
                OpenMode::Activate,
                None,
                window,
                cx,
            )
        });

        cx.spawn_in(window, async move |this, cx| {
            let workspace = task.await?;
            this.update_in(cx, |this, window, cx| match target {
                NewEntryTarget::LastCreatedKind => this.create_new_entry(&workspace, window, cx),
                NewEntryTarget::Terminal => this.create_new_terminal(&workspace, window, cx),
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Rebuilds the sidebar contents from current workspace and thread state.
    ///
    /// Iterates [`MultiWorkspace::project_group_keys`] to determine project
    /// groups, then populates thread entries from the metadata store and
    /// merges live thread info from active agent panels.
    ///
    /// Aim for a single forward pass over workspaces and threads plus an
    /// O(T log T) sort. Avoid adding extra scans over the data.
    ///
    /// Properties:
    ///
    /// - Should always show every workspace in the multiworkspace
    ///     - If you have no threads, and two workspaces for the worktree and the main workspace, make sure at least one is shown
    /// - Should always show every thread, associated with each workspace in the multiworkspace
    /// - After every build_contents, our "active" state should exactly match the current workspace's, current agent panel's current thread.
    fn rebuild_contents(&mut self, cx: &App) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let mw = multi_workspace.read(cx);
        let workspaces: Vec<_> = mw.workspaces().cloned().collect();
        let active_workspace = Some(mw.workspace().clone());

        let agent_server_store = workspaces
            .first()
            .map(|ws| ws.read(cx).project().read(cx).agent_server_store().clone());

        let query = self.filter_editor.read(cx).text(cx);

        let previous = mem::take(&mut self.contents);

        let old_statuses = &self.live_thread_statuses;

        let mut entries = Vec::new();
        let mut notified_threads = previous.notified_threads;
        let mut notified_terminals: HashSet<TerminalId> = HashSet::new();
        let mut new_live_statuses: HashMap<acp::SessionId, (AgentThreadStatus, ThreadId)> =
            HashMap::new();
        let mut current_session_ids: HashSet<acp::SessionId> = HashSet::new();
        let mut current_thread_ids: HashSet<agent_ui::ThreadId> = HashSet::new();
        let mut current_terminal_ids: HashSet<TerminalId> = HashSet::new();
        let mut project_header_indices: Vec<usize> = Vec::new();
        let active_agent_paths = self.worktree_paths_with_active_agents(cx);
        let mut seen_thread_ids: HashSet<agent_ui::ThreadId> = HashSet::new();
        let mut seen_terminal_ids: HashSet<TerminalId> = HashSet::new();

        let has_open_projects = workspaces
            .iter()
            .any(|ws| !workspace_path_list(ws, cx).paths().is_empty());

        let resolve_agent_icon = |agent_id: &AgentId| -> (IconName, Option<SharedString>) {
            let agent = Agent::from(agent_id.clone());
            let icon = match agent {
                Agent::NativeAgent => IconName::ZedAgent,
                Agent::Custom { .. } => IconName::Terminal,

                _ => IconName::ZedAgent,
            };
            let icon_from_external_svg = agent_server_store
                .as_ref()
                .and_then(|store| store.read(cx).agent_icon(&agent_id));
            (icon, icon_from_external_svg)
        };

        let groups = mw.project_groups(cx);
        let mut live_notified_terminal_ids: HashSet<TerminalId> = HashSet::new();
        for workspace in &workspaces {
            if let Some(agent_panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
                live_notified_terminal_ids.extend(
                    agent_panel
                        .read(cx)
                        .terminals(cx)
                        .into_iter()
                        .filter_map(|terminal| terminal.has_notification.then_some(terminal.id)),
                );
            }
        }

        let path_detail_map =
            ProjectGroupKey::path_detail_map(groups.iter().map(|group| &group.key));

        let mut branch_by_path: HashMap<PathBuf, SharedString> = HashMap::new();
        for ws in &workspaces {
            let project = ws.read(cx).project().read(cx);
            for repo in project.repositories(cx).values() {
                let snapshot = repo.read(cx).snapshot();
                if let Some(branch) = &snapshot.branch {
                    branch_by_path.insert(
                        snapshot.work_directory_abs_path.to_path_buf(),
                        SharedString::from(Arc::<str>::from(branch.name())),
                    );
                }
                for linked_wt in snapshot.linked_worktrees() {
                    if let Some(branch) = linked_wt.branch_name() {
                        branch_by_path.insert(
                            linked_wt.path.clone(),
                            SharedString::from(Arc::<str>::from(branch)),
                        );
                    }
                }
            }
        }

        for worktree in self.closed_group_worktrees.values().flatten() {
            if let Some(branch) = worktree.branch_name() {
                branch_by_path
                    .entry(worktree.path.clone())
                    .or_insert_with(|| SharedString::from(branch.to_string()));
            }
        }

        let thread_store = ThreadMetadataStore::global(cx);
        let multi_root_groups = || {
            groups
                .iter()
                .filter(|group| group.key.path_list().paths().len() > 1)
        };
        let multi_root_worktree_paths: HashSet<PathBuf> = multi_root_groups()
            .flat_map(|group| group.workspaces.iter())
            .flat_map(|workspace| workspace.read(cx).root_paths(cx))
            .map(|path| path.to_path_buf())
            .chain(multi_root_groups().flat_map(|group| {
                thread_store
                    .read(cx)
                    .entries_for_main_worktree_path(group.key.path_list(), group.key.host())
                    .flat_map(|thread| thread.folder_paths().paths().to_vec())
                    .collect::<Vec<_>>()
            }))
            .chain(
                self.multi_root_worktree_groups
                    .iter()
                    .filter(|(key, _)| groups.iter().any(|group| group.key == *key))
                    .flat_map(|(_, paths)| paths.folder_path_list().paths().iter().cloned()),
            )
            .collect();

        for group in &groups {
            let group_key = &group.key;
            let group_workspaces = &group.workspaces;

            let workspace_by_path_list: HashMap<PathList, &Entity<Workspace>> = group_workspaces
                .iter()
                .map(|ws| (workspace_path_list(ws, cx), ws))
                .collect();
            let resolve_workspace = |folder_paths: &PathList| -> ThreadEntryWorkspace {
                workspace_by_path_list
                    .get(folder_paths)
                    .map(|ws| ThreadEntryWorkspace::Open((*ws).clone()))
                    .unwrap_or_else(|| ThreadEntryWorkspace::Closed {
                        folder_paths: folder_paths.clone(),
                        project_group_key: group_key.clone(),
                    })
            };
            let mut linked_worktree_path_lists = if !has_live_git_state(group_workspaces, cx) {
                let mut path_lists: Vec<PathList> = self
                    .closed_group_worktrees
                    .get(group_key)
                    .into_iter()
                    .flatten()
                    .filter(|worktree| !worktree.is_main && !worktree.is_bare)
                    .map(|worktree| PathList::new(std::slice::from_ref(&worktree.path)))
                    .collect();
                path_lists.sort_by(|a, b| a.paths()[0].cmp(&b.paths()[0]));
                path_lists
            } else {
                linked_worktree_path_lists_for_workspaces(
                    group_workspaces,
                    group_key.path_list().paths(),
                    cx,
                )
            };
            if group_key.path_list().paths().len() <= 1 {
                linked_worktree_path_lists.retain(|path_list| {
                    !path_list
                        .paths()
                        .iter()
                        .any(|path| multi_root_worktree_paths.contains(path))
                });
            }
            let make_terminal_entry =
                |metadata: TerminalThreadMetadata, workspace: ThreadEntryWorkspace| {
                    let worktrees =
                        worktree_info_from_thread_paths(&metadata.worktree_paths, &branch_by_path);
                    let has_notification =
                        live_notified_terminal_ids.contains(&metadata.terminal_id);
                    TerminalEntry {
                        metadata,
                        workspace,
                        worktrees,
                        has_notification,
                        highlight_positions: Vec::new(),
                    }
                };

            let mut terminals = Vec::new();
            let terminal_store = TerminalThreadMetadataStore::global(cx);
            let group_host = group_key.host();
            let mut push_terminal_metadata =
                |metadata: TerminalThreadMetadata, workspace: ThreadEntryWorkspace| {
                    if !seen_terminal_ids.insert(metadata.terminal_id) {
                        return;
                    }
                    terminals.push(make_terminal_entry(metadata, workspace));
                };
            for row in terminal_store
                .read(cx)
                .entries_for_main_worktree_path(group_key.path_list(), group_host)
                .cloned()
            {
                let workspace = resolve_workspace(row.folder_paths());
                push_terminal_metadata(row, workspace);
            }
            for row in terminal_store
                .read(cx)
                .entries_for_path(group_key.path_list(), group_host)
                .cloned()
            {
                let workspace = resolve_workspace(row.folder_paths());
                push_terminal_metadata(row, workspace);
            }
            for ws in group_workspaces {
                let ws_paths = workspace_path_list(ws, cx);
                if ws_paths.paths().is_empty() {
                    continue;
                }
                for row in terminal_store
                    .read(cx)
                    .entries_for_path(&ws_paths, group_host)
                    .cloned()
                {
                    push_terminal_metadata(row, ThreadEntryWorkspace::Open(ws.clone()));
                }
            }
            for worktree_path_list in &linked_worktree_path_lists {
                for row in terminal_store
                    .read(cx)
                    .entries_for_path(worktree_path_list, group_host)
                    .cloned()
                {
                    push_terminal_metadata(
                        row,
                        ThreadEntryWorkspace::Closed {
                            folder_paths: worktree_path_list.clone(),
                            project_group_key: group_key.clone(),
                        },
                    );
                }
            }
            current_terminal_ids.extend(
                terminals
                    .iter()
                    .map(|terminal| terminal.metadata.terminal_id),
            );
            notified_terminals.extend(terminals.iter().filter_map(|terminal| {
                terminal
                    .has_notification
                    .then_some(terminal.metadata.terminal_id)
            }));
            if group_key.path_list().paths().is_empty() {
                continue;
            }

            let label = mw
                .group_state_by_key(group_key)
                .and_then(|state| state.custom_name.clone())
                .unwrap_or_else(|| group_key.display_name(&path_detail_map));

            let fallback_label = label.clone();

            let is_collapsed = self.is_group_collapsed(group_key, cx);
            let should_load_threads = !is_collapsed || !query.is_empty();

            let is_active = active_workspace
                .as_ref()
                .is_some_and(|active| group_workspaces.contains(active));

            // Collect live thread infos from all workspaces in this group.
            let live_infos = group_workspaces
                .iter()
                .flat_map(|ws| all_thread_infos_for_workspace(ws, cx));

            let mut threads: Vec<Arc<ThreadEntry>> = Vec::new();
            let mut has_running_threads = false;
            let mut waiting_thread_count: usize = 0;
            let group_host = group_key.host();

            if should_load_threads {
                let thread_store = ThreadMetadataStore::global(cx);

                let make_thread_entry =
                    |row: ThreadMetadata, workspace: ThreadEntryWorkspace| -> Arc<ThreadEntry> {
                        let (icon, icon_from_external_svg) = resolve_agent_icon(&row.agent_id);
                        let worktrees =
                            worktree_info_from_thread_paths(&row.worktree_paths, &branch_by_path);
                        // Start drafts as `WithContent`; the post-processing
                        // pass below downgrades them to `Empty` if no draft
                        // label can be derived.
                        let draft = row.is_draft().then_some(DraftKind::WithContent);
                        Arc::new(ThreadEntry {
                            metadata: row,
                            icon,
                            icon_from_external_svg,
                            status: AgentThreadStatus::default(),
                            workspace,
                            is_live: false,
                            is_background: false,
                            is_title_generating: false,
                            draft,
                            highlight_positions: Vec::new(),
                            worktrees,
                            diff_stats: DiffStats::default(),
                        })
                    };

                // Main code path: one query per group via main_worktree_paths.
                // The main_worktree_paths column is set on all new threads and
                // points to the group's canonical paths regardless of which
                // linked worktree the thread was opened in.
                for row in thread_store
                    .read(cx)
                    .entries_for_main_worktree_path(group_key.path_list(), group_host)
                    .cloned()
                {
                    if !seen_thread_ids.insert(row.thread_id) {
                        continue;
                    }
                    let workspace = resolve_workspace(row.folder_paths());
                    threads.push(make_thread_entry(row, workspace));
                }

                // Legacy threads did not have `main_worktree_paths` populated, so they
                // must be queried by their `folder_paths`.

                // Load any legacy threads for the main worktrees of this project group.
                for row in thread_store
                    .read(cx)
                    .entries_for_path(group_key.path_list(), group_host)
                    .cloned()
                {
                    if !seen_thread_ids.insert(row.thread_id) {
                        continue;
                    }
                    let workspace = resolve_workspace(row.folder_paths());
                    threads.push(make_thread_entry(row, workspace));
                }

                // Also surface any thread whose `folder_paths` equals
                // one of this group's open workspaces' root paths.
                // The three lookups above can all miss when the
                // thread's stored `main_worktree_paths` disagree with
                // the group key (for example, a stale row whose main
                // paths equal its folder paths for a linked-worktree
                // workspace). The thread will be rewritten into the
                // correct shape the next time `handle_conversation_event`
                // fires, but until then the sidebar should still show
                // it under the group whose workspace it actually
                // belongs to.
                for ws in group_workspaces {
                    let ws_paths = workspace_path_list(ws, cx);
                    if ws_paths.paths().is_empty() {
                        continue;
                    }
                    for row in thread_store
                        .read(cx)
                        .entries_for_path(&ws_paths, group_host)
                        .cloned()
                    {
                        if !seen_thread_ids.insert(row.thread_id) {
                            continue;
                        }
                        threads.push(make_thread_entry(
                            row,
                            ThreadEntryWorkspace::Open(ws.clone()),
                        ));
                    }
                }

                // Load any legacy threads for any single linked worktree of this project group.
                for worktree_path_list in &linked_worktree_path_lists {
                    for row in thread_store
                        .read(cx)
                        .entries_for_path(worktree_path_list, group_host)
                        .cloned()
                    {
                        if !seen_thread_ids.insert(row.thread_id) {
                            continue;
                        }
                        threads.push(make_thread_entry(
                            row,
                            ThreadEntryWorkspace::Closed {
                                folder_paths: worktree_path_list.clone(),
                                project_group_key: group_key.clone(),
                            },
                        ));
                    }
                }

                // A project on another host may know the same agent by another name.
                let fallback_workspace = group_workspaces.first().or_else(|| {
                    workspaces.iter().find(|workspace| {
                        workspace
                            .read(cx)
                            .project()
                            .read(cx)
                            .remote_connection_options(cx)
                            .map(|options| options.identity())
                            == group_host.map(|options| options.identity())
                    })
                });
                for thread in &mut threads {
                    if thread.draft.is_none() {
                        continue;
                    }
                    if let Some((label, kind)) = draft_display_label_for_thread_metadata(
                        &thread.metadata,
                        &thread.workspace,
                        fallback_workspace,
                        cx,
                    ) {
                        let thread = Arc::make_mut(thread);
                        thread.metadata.title = Some(label);
                        thread.draft = Some(kind);
                    }
                }
                threads.retain(|thread| thread.draft.is_none() || thread.metadata.title.is_some());

                // Build a lookup from live_infos and compute running/waiting
                // counts in a single pass.
                let mut live_info_by_session: HashMap<acp::SessionId, ActiveThreadInfo> =
                    HashMap::new();
                for info in live_infos {
                    if info.status == AgentThreadStatus::Running {
                        has_running_threads = true;
                    }
                    if info.status == AgentThreadStatus::WaitingForConfirmation {
                        waiting_thread_count += 1;
                    }
                    live_info_by_session.insert(info.session_id.clone(), info);
                }

                // Merge live info into threads and update notification state
                // in a single pass.
                for thread in &mut threads {
                    if let Some(session_id) = thread.metadata.session_id.clone() {
                        if let Some(info) = live_info_by_session.get(&session_id) {
                            let status = info.status;
                            let thread_id = thread.metadata.thread_id;
                            Arc::make_mut(thread).apply_active_info(info);
                            new_live_statuses.insert(session_id, (status, thread_id));
                        }
                    }

                    let session_id = &thread.metadata.session_id;
                    let is_active_thread = self.active_entry.as_ref().is_some_and(|entry| {
                        entry.is_active_thread(&thread.metadata.thread_id)
                            && active_workspace
                                .as_ref()
                                .is_some_and(|active| active == entry.workspace())
                    });

                    if thread.status == AgentThreadStatus::Completed
                        && !is_active_thread
                        && session_id
                            .as_ref()
                            .and_then(|sid| old_statuses.get(sid))
                            .is_some_and(|(s, _)| *s == AgentThreadStatus::Running)
                    {
                        notified_threads.insert(thread.metadata.thread_id);
                    }

                    if is_active_thread && !thread.is_background {
                        notified_threads.remove(&thread.metadata.thread_id);
                    }
                }

                threads.sort_by(|a, b| {
                    let a_time = Self::thread_display_time(&a.metadata);
                    let b_time = Self::thread_display_time(&b.metadata);
                    b_time.cmp(&a_time)
                });
            } else {
                for info in live_infos {
                    if info.status == AgentThreadStatus::Running {
                        has_running_threads = true;
                    }
                    if info.status == AgentThreadStatus::WaitingForConfirmation {
                        waiting_thread_count += 1;
                    }
                    // Resolve the thread_id for this session so we can
                    // track its status and detect transitions even while
                    // the group is collapsed.
                    let thread_id = old_statuses
                        .get(&info.session_id)
                        .map(|(_, tid)| *tid)
                        .or_else(|| {
                            ThreadMetadataStore::global(cx)
                                .read(cx)
                                .entry_by_session(&info.session_id)
                                .map(|m| m.thread_id)
                        });

                    if let Some(thread_id) = thread_id {
                        let old_status = old_statuses.get(&info.session_id).map(|(s, _)| *s);
                        new_live_statuses.insert(info.session_id.clone(), (info.status, thread_id));
                        if info.status == AgentThreadStatus::Completed
                            && old_status == Some(AgentThreadStatus::Running)
                        {
                            notified_threads.insert(thread_id);
                        }
                    }
                }

                if is_active
                    && let Some(ActiveEntry::Thread { thread_id, .. }) = self.active_entry.as_ref()
                {
                    notified_threads.remove(thread_id);
                }
            }

            let has_visible_rows = !threads.is_empty() || !terminals.is_empty();
            let has_stored_thread_rows = !should_load_threads && !has_visible_rows && {
                let store = ThreadMetadataStore::global(cx).read(cx);
                store
                    .entries_for_main_worktree_path(group_key.path_list(), group_host)
                    .any(|metadata| {
                        let workspace = resolve_workspace(metadata.folder_paths());
                        thread_metadata_would_render_sidebar_row(metadata, &workspace, cx)
                    })
                    || store
                        .entries_for_path(group_key.path_list(), group_host)
                        .any(|metadata| {
                            let workspace = resolve_workspace(metadata.folder_paths());
                            thread_metadata_would_render_sidebar_row(metadata, &workspace, cx)
                        })
            };
            let has_threads = has_visible_rows || has_stored_thread_rows;

            if !query.is_empty() {
                let workspace_highlight_positions =
                    fuzzy_match_positions(&query, &label).unwrap_or_default();
                let workspace_matched = !workspace_highlight_positions.is_empty();

                let mut matched_threads: Vec<Arc<ThreadEntry>> = Vec::new();
                for mut thread in threads {
                    let mut worktree_matched = false;
                    {
                        let thread = Arc::make_mut(&mut thread);
                        let title = thread.metadata.display_title();
                        if let Some(positions) = fuzzy_match_positions(&query, title.as_ref()) {
                            thread.highlight_positions = positions;
                        }
                        for worktree in &mut thread.worktrees {
                            let Some(name) = worktree.worktree_name.as_ref() else {
                                continue;
                            };
                            if let Some(positions) = fuzzy_match_positions(&query, name) {
                                worktree.highlight_positions = positions;
                                worktree_matched = true;
                            }
                        }
                    }
                    if workspace_matched
                        || !thread.highlight_positions.is_empty()
                        || worktree_matched
                    {
                        matched_threads.push(thread);
                    }
                }

                let mut matched_terminals: Vec<TerminalEntry> = Vec::new();
                for mut terminal in terminals {
                    let mut terminal_matched = false;
                    let terminal_title = terminal.metadata.display_title();
                    if let Some(positions) = fuzzy_match_positions(&query, terminal_title.as_ref())
                    {
                        terminal.highlight_positions = positions;
                        terminal_matched = true;
                    }
                    let mut worktree_matched = false;
                    for worktree in &mut terminal.worktrees {
                        let Some(name) = worktree.worktree_name.as_ref() else {
                            continue;
                        };
                        if let Some(positions) = fuzzy_match_positions(&query, name) {
                            worktree.highlight_positions = positions;
                            worktree_matched = true;
                        }
                    }
                    if workspace_matched || terminal_matched || worktree_matched {
                        matched_terminals.push(terminal);
                    }
                }

                if matched_threads.is_empty() && matched_terminals.is_empty() && !workspace_matched
                {
                    continue;
                }

                // Check for notifications: threads that completed while not active.
                let has_thread_notifications = matched_threads
                    .iter()
                    .any(|t| notified_threads.contains(&t.metadata.thread_id));
                let has_terminal_notifications = matched_terminals
                    .iter()
                    .any(|t| notified_terminals.contains(&t.metadata.terminal_id));

                project_header_indices.push(entries.len());
                entries.push(ListEntry::ProjectHeader {
                    key: group_key.clone(),
                    label,
                    highlight_positions: workspace_highlight_positions,
                    has_running_threads,
                    waiting_thread_count,
                    has_notifications: has_thread_notifications || has_terminal_notifications,
                    is_active,
                    has_threads,
                });

                Self::push_entries_by_display_time(
                    &mut entries,
                    matched_terminals,
                    matched_threads,
                    group_key,
                    fallback_label,
                    Vec::new(),
                    &branch_by_path,
                    &self.worktree_pull_requests,
                    &self.worktree_group_order,
                    &HashSet::new(),
                    &self.deleting_worktree_paths.borrow(),
                    &active_agent_paths,
                    &notified_threads,
                    &mut current_session_ids,
                    &mut current_thread_ids,
                );
            } else {
                let has_terminal_notifications = terminals
                    .iter()
                    .any(|t| notified_terminals.contains(&t.metadata.terminal_id));

                // When collapsed, threads aren't loaded into `threads`, so we
                // query the store for thread IDs to check notifications and
                // to prevent the retain below from purging them.
                let has_thread_notifications = if threads.is_empty() && !notified_threads.is_empty()
                {
                    let thread_store = ThreadMetadataStore::global(cx);
                    let store = thread_store.read(cx);
                    let group_thread_ids = store
                        .entries_for_main_worktree_path(group_key.path_list(), group_host)
                        .chain(store.entries_for_path(group_key.path_list(), group_host))
                        .map(|m| m.thread_id)
                        .collect::<HashSet<_>>();
                    current_thread_ids.extend(group_thread_ids.iter());
                    group_thread_ids
                        .iter()
                        .any(|id| notified_threads.contains(id))
                } else {
                    threads
                        .iter()
                        .any(|t| notified_threads.contains(&t.metadata.thread_id))
                };

                project_header_indices.push(entries.len());
                entries.push(ListEntry::ProjectHeader {
                    key: group_key.clone(),
                    label,
                    highlight_positions: Vec::new(),
                    has_running_threads,
                    waiting_thread_count,
                    has_notifications: has_thread_notifications || has_terminal_notifications,
                    is_active,
                    has_threads,
                });

                if is_collapsed {
                    continue;
                }

                // Rows are grouped by their first root in its original order.
                let ordered_roots: Vec<&PathBuf> = group_key.path_list().ordered_paths().collect();
                let known_worktrees = match ordered_roots.as_slice() {
                    [] => vec![KnownWorktree {
                        path: SharedString::default(),
                        roots: vec![SharedString::default()],
                        label: fallback_label.clone(),
                        is_linked: false,
                        extra_worktrees: Vec::new(),
                    }],
                    [main_path, other_roots @ ..] => {
                        let main = KnownWorktree {
                            path: SharedString::from(main_path.display().to_string()),
                            roots: ordered_roots
                                .iter()
                                .map(|root| SharedString::from(root.display().to_string()))
                                .collect(),
                            label: main_path.file_name().map_or_else(
                                || fallback_label.clone(),
                                |name| SharedString::from(name.to_string_lossy().into_owned()),
                            ),
                            is_linked: false,
                            extra_worktrees: other_roots
                                .iter()
                                .map(|root| {
                                    (
                                        root.file_name().map_or_else(
                                            || fallback_label.clone(),
                                            |name| {
                                                SharedString::from(
                                                    name.to_string_lossy().into_owned(),
                                                )
                                            },
                                        ),
                                        branch_by_path.get(root.as_path()).cloned(),
                                    )
                                })
                                .collect(),
                        };
                        let linked = linked_worktree_path_lists
                            .iter()
                            .filter(|_| other_roots.is_empty())
                            .filter_map(|paths| {
                                let path = paths.paths().first()?;
                                Some(KnownWorktree {
                                    path: SharedString::from(path.display().to_string()),
                                    roots: vec![SharedString::from(path.display().to_string())],
                                    label: project::linked_worktree_short_name(
                                        main_path,
                                        path,
                                        PathStyle::local(),
                                    )
                                    .unwrap_or_default(),
                                    is_linked: true,
                                    extra_worktrees: Vec::new(),
                                })
                            });
                        let saved = self
                            .multi_root_worktree_groups
                            .iter()
                            .filter(|(key, _)| key == group_key)
                            .filter_map(|(_, paths)| {
                                let worktrees =
                                    worktree_info_from_thread_paths(paths, &branch_by_path);
                                let first = worktrees.first()?;
                                Some(KnownWorktree {
                                    path: first.full_path.clone(),
                                    roots: worktrees
                                        .iter()
                                        .map(|worktree| worktree.full_path.clone())
                                        .collect(),
                                    label: first.worktree_name.clone()?,
                                    is_linked: paths
                                        .ordered_pairs()
                                        .any(|(main_path, folder_path)| main_path != folder_path),
                                    extra_worktrees: worktrees
                                        .iter()
                                        .skip(1)
                                        .filter_map(|worktree| {
                                            Some((
                                                worktree.worktree_name.clone()?,
                                                worktree.branch_name.clone(),
                                            ))
                                        })
                                        .collect(),
                                })
                            });
                        std::iter::once(main).chain(linked).chain(saved).collect()
                    }
                };

                Self::push_entries_by_display_time(
                    &mut entries,
                    terminals,
                    threads,
                    group_key,
                    fallback_label,
                    known_worktrees,
                    &branch_by_path,
                    &self.worktree_pull_requests,
                    &self.worktree_group_order,
                    &self.collapsed_worktree_groups,
                    &self.deleting_worktree_paths.borrow(),
                    &active_agent_paths,
                    &notified_threads,
                    &mut current_session_ids,
                    &mut current_thread_ids,
                );
            }
        }

        notified_threads.retain(|id| current_thread_ids.contains(id));

        self.thread_last_accessed
            .retain(|id, _| current_thread_ids.contains(id));
        self.terminal_last_accessed
            .retain(|id, _| current_terminal_ids.contains(id));

        self.live_thread_statuses = new_live_statuses;

        self.contents = SidebarContents {
            entries,
            notified_threads,
            notified_terminals,
            project_header_indices,
            has_open_projects,
        };
    }

    fn schedule_update_entries(&mut self, select_first_after_update: bool, cx: &mut Context<Self>) {
        if self.update_task.is_some() && !select_first_after_update {
            return;
        }

        self.update_task = Some(cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| {
                this.update_task = None;
                this.update_entries(cx);
                if select_first_after_update {
                    this.select_first_entry();
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Rebuilds the sidebar's visible entries from already-cached state.
    fn update_entries(&mut self, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        if !multi_workspace.read(cx).multi_workspace_enabled(cx) {
            return;
        }

        let had_notifications = self.has_notifications(cx);
        let previous_shapes: Vec<EntryShape> =
            self.entry_shapes(multi_workspace.read(cx)).collect();

        self.rebuild_contents(cx);
        let saved_multi_root_groups = self.save_multi_root_worktree_groups(cx);
        if saved_multi_root_groups {
            // The new groups get their headers.
            self.rebuild_contents(cx);
        }
        if (!self.has_filter_query(cx) && self.save_new_worktree_group_positions())
            || saved_multi_root_groups
        {
            self.serialize(cx);
        }
        self.refresh_refilled_draft_times(cx);
        self.refresh_draft_editor_observations(cx);

        // Preserve measurements for unchanged entries so sticky headers do not flicker.
        self.apply_list_state_diff(&previous_shapes, multi_workspace.read(cx));

        self.fetch_closed_group_worktrees(false, cx);
        self.refresh_worktree_pull_requests(false, cx);

        if had_notifications != self.has_notifications(cx) {
            multi_workspace.update(cx, |_, cx| {
                cx.notify();
            });
        }

        cx.notify();
    }

    /// Lists the worktrees of local projects that have no open workspace straight from
    /// git. `refresh` also re-lists the projects that already have a listing.
    fn fetch_closed_group_worktrees(&mut self, refresh: bool, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let groups = multi_workspace.read(cx).project_groups(cx);
        // An open project uses live git state, so its listing would go stale.
        let awaits_live_git_state = |key: &ProjectGroupKey| {
            groups
                .iter()
                .any(|group| group.key == *key && !has_live_git_state(&group.workspaces, cx))
        };
        self.closed_group_worktrees
            .retain(|key, _| awaits_live_git_state(key));
        self.closed_group_worktree_tasks
            .retain(|key, _| awaits_live_git_state(key));
        let fs = multi_workspace
            .read(cx)
            .workspace()
            .read(cx)
            .app_state()
            .fs
            .clone();
        if refresh || mem::take(&mut self.multi_root_worktree_groups_unchecked) {
            let local_groups: Vec<WorktreePaths> = self
                .multi_root_worktree_groups
                .iter()
                .filter(|(key, _)| key.host().is_none())
                .map(|(_, paths)| paths.clone())
                .collect();
            let fs = fs.clone();
            self.multi_root_worktree_groups_check = (!local_groups.is_empty()).then(|| {
                cx.spawn(async move |this, cx| {
                    let mut deleted = Vec::new();
                    for paths in local_groups {
                        let mut exists = true;
                        for folder_path in paths.folder_path_list().paths() {
                            exists &= fs.is_dir(folder_path).await;
                        }
                        if !exists {
                            deleted.push(paths);
                        }
                    }
                    if deleted.is_empty() {
                        return;
                    }
                    this.update(cx, |this, cx| {
                        this.multi_root_worktree_groups.retain(|(key, paths)| {
                            key.host().is_some() || !deleted.contains(paths)
                        });
                        this.serialize(cx);
                        this.update_entries(cx);
                    })
                    .ok();
                })
            });
        }
        for group in groups {
            let [main_path] = group.key.path_list().paths() else {
                continue;
            };
            if !group.workspaces.is_empty()
                || group.key.host().is_some()
                || (!refresh && self.closed_group_worktree_tasks.contains_key(&group.key))
            {
                continue;
            }
            let dot_git = main_path.join(".git");
            let key = group.key;
            let listing = cx.background_spawn({
                let fs = fs.clone();
                async move {
                    // A project that is not a git repository simply has no worktrees to list.
                    // A failed listing leaves no entry, or keeps the previous one.
                    match fs.open_repo(&dot_git, None) {
                        Ok(repository) => repository.worktrees().await.log_err(),
                        Err(_) => Some(Vec::new()),
                    }
                }
            });
            self.closed_group_worktree_tasks.insert(
                key.clone(),
                cx.spawn(async move |this, cx| {
                    let Some(worktrees) = listing.await else {
                        return;
                    };
                    this.update(cx, |this, cx| {
                        if this.closed_group_worktrees.get(&key) != Some(&worktrees) {
                            this.closed_group_worktrees.insert(key, worktrees);
                            this.update_entries(cx);
                        }
                    })
                    .ok();
                }),
            );
        }
    }

    fn schedule_pull_request_refresh(&mut self, cx: &mut Context<Self>) {
        if self.pull_request_refresh.is_some() {
            return;
        }
        self.pull_request_refresh = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(PULL_REQUEST_REFRESH_DEBOUNCE)
                .await;
            this.update(cx, |this, cx| {
                this.pull_request_refresh = None;
                this.refresh_worktree_pull_requests(true, cx);
            })
            .ok();
        }));
    }

    fn refresh_worktree_pull_requests(&mut self, refresh: bool, cx: &mut Context<Self>) {
        let targets: HashMap<PathBuf, SharedString> = self
            .contents
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::WorktreeHeader {
                    pull_request_targets,
                    ..
                } => Some(pull_request_targets),
                _ => None,
            })
            .flatten()
            .cloned()
            .collect();
        // Results are kept: a filter or a collapsed project only hides their headers.
        self.worktree_pull_request_tasks
            .retain(|path, _| targets.contains_key(path));
        if targets.is_empty() {
            return;
        }
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let workspace = multi_workspace.read(cx).workspace().clone();
        if workspace.read(cx).app_state().fs.is_fake() {
            return;
        }

        if self.gh_available != Some(true) {
            if self.gh_available.is_none() && self.gh_availability_check.is_none() {
                // Zed's own PATH lacks `gh` until the login shell environment has loaded.
                let environment = workspace
                    .read(cx)
                    .project()
                    .read(cx)
                    .environment()
                    .clone()
                    .update(cx, |environment, cx| environment.default_environment(cx));
                self.gh_availability_check = Some(cx.spawn(async move |this, cx| {
                    let environment: Arc<HashMap<String, String>> =
                        Arc::new(environment.await.into_iter().flatten().collect());
                    let available = cx
                        .background_spawn({
                            let environment = environment.clone();
                            async move { gh_is_authenticated(&environment).await }
                        })
                        .await;
                    this.update(cx, |this, cx| {
                        this.gh_availability_check = None;
                        this.gh_available = Some(available);
                        this.gh_environment = environment;
                        if available {
                            this.refresh_worktree_pull_requests(false, cx);
                        }
                    })
                    .ok();
                }));
            }
            return;
        }

        for (path, branch) in targets {
            let is_current = self
                .worktree_pull_requests
                .get(&path)
                .is_some_and(|cached| cached.branch == branch);
            let is_running = self
                .worktree_pull_request_tasks
                .get(&path)
                .is_some_and(|(task_branch, _)| *task_branch == branch);
            if !refresh && (is_running || is_current) {
                continue;
            }
            let lookup = cx.background_spawn({
                let path = path.clone();
                let environment = self.gh_environment.clone();
                async move { look_up_pull_request(&path, &environment).await }
            });
            self.worktree_pull_request_tasks.insert(
                path.clone(),
                (
                    branch.clone(),
                    cx.spawn(async move |this, cx| {
                        let result = lookup.await;
                        this.update(cx, |this, cx| {
                            this.worktree_pull_request_tasks.remove(&path);
                            this.record_pull_request_lookup(path, branch, result, cx);
                        })
                        .ok();
                    }),
                ),
            );
        }
    }

    fn record_pull_request_lookup(
        &mut self,
        path: PathBuf,
        branch: SharedString,
        result: anyhow::Result<Option<(PullRequestState, SharedString)>>,
        cx: &mut Context<Self>,
    ) {
        let cached = self.worktree_pull_requests.get(&path);
        // A failed lookup counts as no pull request, so that a stale one is never shown.
        let state_and_url = result.unwrap_or_else(|error| {
            let already_failed = cached
                .is_some_and(|cached| cached.branch == branch && cached.state_and_url.is_none());
            if !already_failed {
                log::warn!("Pull request lookup for {branch} in {path:?} failed: {error:#}");
            }
            None
        });
        let pull_request = WorktreePullRequest {
            branch,
            state_and_url,
        };
        if cached != Some(&pull_request) {
            self.worktree_pull_requests.insert(path, pull_request);
            self.schedule_update_entries(false, cx);
        }
    }

    /// Splices only the changed entry range, leaving unchanged item measurements intact.
    fn apply_list_state_diff(
        &self,
        previous_shapes: &[EntryShape],
        multi_workspace: &MultiWorkspace,
    ) {
        let mut new_iter = self.entry_shapes(multi_workspace);
        let mut prefix_len = 0;
        let leading_new = loop {
            match (previous_shapes.get(prefix_len), new_iter.next()) {
                (Some(prev), Some(next)) if *prev == next => prefix_len += 1,
                (None, None) => return,
                (_, leading) => break leading,
            }
        };

        let new_tail: Vec<EntryShape> = leading_new.into_iter().chain(new_iter).collect();
        let prev_tail = &previous_shapes[prefix_len..];
        let suffix_len = prev_tail
            .iter()
            .rev()
            .zip(new_tail.iter().rev())
            .take_while(|(prev, next)| prev == next)
            .count();

        let old_changed = prefix_len..previous_shapes.len() - suffix_len;
        let new_changed_count = new_tail.len() - suffix_len;
        self.list_state.splice(old_changed, new_changed_count);
    }

    fn entry_shapes<'a>(
        &'a self,
        multi_workspace: &'a MultiWorkspace,
    ) -> impl Iterator<Item = EntryShape> + 'a {
        self.contents.entries.iter().map(move |entry| match entry {
            ListEntry::ProjectHeader {
                key, has_threads, ..
            } => EntryShape::ProjectHeader {
                key: key.clone(),
                has_threads: *has_threads,
                is_collapsed: multi_workspace
                    .group_state_by_key(key)
                    .map(|state| !state.expanded)
                    .unwrap_or(false),
            },
            ListEntry::WorktreeHeader {
                id,
                has_rows,
                is_collapsed,
                ..
            } => EntryShape::WorktreeHeader {
                id: id.clone(),
                has_rows: *has_rows,
                is_collapsed: *is_collapsed,
            },
            ListEntry::Thread(thread) => EntryShape::Thread(thread.metadata.thread_id),
            ListEntry::Terminal(terminal) => EntryShape::Terminal(terminal.metadata.terminal_id),
        })
    }

    /// Detects drafts that just went from empty back to having content and
    /// refreshes their interaction time to now, so a re-filled draft sorts to
    /// the top of the list instead of falling back to its original creation time.
    fn refresh_refilled_draft_times(&mut self, cx: &mut Context<Self>) {
        let mut new_kinds: HashMap<ThreadId, DraftKind> = HashMap::new();
        let mut refilled: Vec<ThreadId> = Vec::new();

        for entry in &self.contents.entries {
            let ListEntry::Thread(thread) = entry else {
                continue;
            };
            let Some(kind) = thread.draft else {
                continue;
            };
            let thread_id = thread.metadata.thread_id;

            if kind == DraftKind::WithContent
                && self.draft_kinds.get(&thread_id) == Some(&DraftKind::Empty)
            {
                refilled.push(thread_id);
            }
            new_kinds.insert(thread_id, kind);
        }
        self.draft_kinds = new_kinds;

        if refilled.is_empty() {
            return;
        }

        let now = Utc::now();

        ThreadMetadataStore::global(cx).update(cx, |store, store_cx| {
            for thread_id in refilled {
                store.update_interacted_at(&thread_id, now, store_cx);
            }
        });
    }

    /// Re-establishes subscriptions to each visible draft's message editor
    /// so we rebuild entries (and their displayed titles) as the user types.
    fn refresh_draft_editor_observations(&mut self, cx: &mut Context<Self>) {
        self._draft_editor_observations.clear();
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let draft_conversation_views: Vec<Entity<agent_ui::ConversationView>> = multi_workspace
            .read(cx)
            .workspaces()
            .filter_map(|ws| ws.read(cx).panel::<AgentPanel>(cx))
            .flat_map(|panel| panel.read(cx).conversation_views())
            .collect();

        for cv in draft_conversation_views {
            if let Some(thread_view) = cv.read(cx).active_thread() {
                let editor = thread_view.read(cx).message_editor.clone();
                self._draft_editor_observations.push(cx.subscribe(
                    &editor,
                    |this, _editor, event, cx| match event {
                        MessageEditorEvent::Edited => this.schedule_update_entries(false, cx),
                        _ => (),
                    },
                ));
            }
            // Also subscribe to the ConversationView itself so that editor
            // replacements during lifecycle transitions (Loading →
            // Connected) re-wire the editor observation above.
            self._draft_editor_observations.push(cx.subscribe(
                &cv,
                |this, _cv, _event: &StateChange, cx| {
                    this.schedule_update_entries(false, cx);
                },
            ));
        }
    }

    fn select_first_entry(&mut self) {
        self.selection = self
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::Thread(_) | ListEntry::Terminal(_)))
            .or_else(|| {
                if self.contents.entries.is_empty() {
                    None
                } else {
                    Some(0)
                }
            });
    }

    fn render_list_entry(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(entry) = self.contents.entries.get(ix) else {
            return div().into_any_element();
        };
        let is_focused = self.focus_handle.is_focused(window);
        // is_selected means the keyboard selector is here.
        let is_selected = is_focused && self.selection == Some(ix);

        let is_group_header_after_first =
            ix > 0 && matches!(entry, ListEntry::ProjectHeader { .. });

        let is_active = self
            .active_entry
            .as_ref()
            .is_some_and(|active| active.matches_entry(entry));

        let rendered = match entry {
            ListEntry::ProjectHeader {
                key,
                label,
                highlight_positions,
                has_running_threads,
                waiting_thread_count,
                has_notifications,
                is_active: is_active_group,
                has_threads,
            } => {
                self.project_header_menu_handles.entry(ix).or_default();
                self.render_project_header(
                    ix,
                    false,
                    key,
                    label,
                    highlight_positions,
                    *has_running_threads,
                    *waiting_thread_count,
                    *has_notifications,
                    *is_active_group,
                    is_selected,
                    *has_threads,
                    // has_active_draft,
                    cx,
                )
            }
            ListEntry::WorktreeHeader { .. } => {
                self.worktree_header_menu_handles.entry(ix).or_default();
                self.render_worktree_header(ix, is_selected, cx)
            }
            ListEntry::Thread(thread) => self.render_thread(ix, thread, is_active, is_selected, cx),
            ListEntry::Terminal(terminal) => {
                self.render_terminal(ix, terminal, is_active, is_selected, cx)
            }
        };

        // The line marks the edge of the group the dragged group lands on.
        let rendered = match self.drop_line_for_entry(ix) {
            Some(at_bottom) => div()
                .relative()
                .w_full()
                .child(rendered)
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .h(px(2.))
                        .map(|line| {
                            if at_bottom {
                                line.bottom_0()
                            } else {
                                line.top_0()
                            }
                        })
                        .bg(cx.theme().colors().drop_target_border),
                )
                .into_any_element(),
            None => rendered,
        };

        if is_group_header_after_first {
            v_flex()
                .w_full()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .child(rendered)
                .into_any_element()
        } else if let Some(group_key) = self.group_key_for_entry(ix) {
            // Rows below a header accept drops for their group, so a group
            // can be reordered without aiming at another header.
            let worktree_group_id = match entry {
                ListEntry::Thread(_) | ListEntry::Terminal(_) => {
                    self.worktree_group_id_for_entry(ix)
                }
                ListEntry::ProjectHeader { .. } | ListEntry::WorktreeHeader { .. } => None,
            };
            div()
                .w_full()
                .child(rendered)
                .on_drop(
                    cx.listener(move |this, dragged: &DraggedProjectHeader, _window, cx| {
                        this.drop_project_header(dragged, &group_key, cx);
                    }),
                )
                .when_some(worktree_group_id, |this, worktree_group_id| {
                    this.on_drop(cx.listener(
                        move |this, dragged: &DraggedWorktreeHeader, _window, cx| {
                            this.move_worktree_group(&dragged.id, &worktree_group_id, cx);
                        },
                    ))
                })
                .into_any_element()
        } else {
            rendered
        }
    }

    fn group_key_for_entry(&self, ix: usize) -> Option<ProjectGroupKey> {
        self.contents
            .entries
            .get(..ix)?
            .iter()
            .rev()
            .find_map(|entry| match entry {
                ListEntry::ProjectHeader { key, .. } => Some(key.clone()),
                _ => None,
            })
    }

    fn worktree_group_id_for_entry(&self, ix: usize) -> Option<SharedString> {
        self.contents
            .entries
            .get(..=ix)?
            .iter()
            .rev()
            .find_map(|entry| match entry {
                ListEntry::WorktreeHeader { id, .. } => Some(Some(id.clone())),
                ListEntry::ProjectHeader { .. } => Some(None),
                ListEntry::Thread(_) | ListEntry::Terminal(_) => None,
            })?
    }

    /// Whether a reorder drop line is drawn on this entry, and if so whether on its bottom edge.
    /// A group dropped below its target lands after the target's last row.
    fn drop_line_for_entry(&self, ix: usize) -> Option<bool> {
        let entries = &self.contents.entries;
        let entry = entries.get(ix)?;
        let next = entries.get(ix + 1);
        if let Some((target, lands_below)) = &self.project_drop_target {
            let is_target_header =
                matches!(entry, ListEntry::ProjectHeader { key, .. } if key == target);
            if !*lands_below {
                if is_target_header {
                    return Some(false);
                }
            } else if matches!(next, None | Some(ListEntry::ProjectHeader { .. }))
                && (is_target_header
                    || (!matches!(entry, ListEntry::ProjectHeader { .. })
                        && self.group_key_for_entry(ix).as_ref() == Some(target)))
            {
                return Some(true);
            }
        }
        if let Some((target, lands_below)) = &self.worktree_drop_target {
            if !*lands_below {
                if matches!(entry, ListEntry::WorktreeHeader { id, .. } if id == target) {
                    return Some(false);
                }
            } else if matches!(
                next,
                None | Some(ListEntry::ProjectHeader { .. } | ListEntry::WorktreeHeader { .. })
            ) && self.worktree_group_id_for_entry(ix).as_ref() == Some(target)
            {
                return Some(true);
            }
        }
        None
    }

    fn entry_ix_at(&self, position: Point<Pixels>) -> Option<usize> {
        (0..self.contents.entries.len()).find(|&ix| {
            self.list_state
                .bounds_for_item(ix)
                .is_some_and(|bounds| bounds.contains(&position))
        })
    }

    fn update_worktree_drop_target(
        &mut self,
        dragged_id: &SharedString,
        position: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let target = position
            .filter(|_| !self.has_filter_query(cx))
            .and_then(|position| self.entry_ix_at(position))
            .and_then(|ix| self.worktree_group_id_for_entry(ix))
            .and_then(|target_id| {
                let project = worktree_group_project(dragged_id);
                let ids: Vec<&SharedString> = worktree_group_ids(&self.contents.entries)
                    .filter(|id| worktree_group_project(id) == project)
                    .collect();
                let from = ids.iter().position(|id| *id == dragged_id)?;
                let to = ids.iter().position(|id| **id == target_id)?;
                (from != to).then_some((target_id, from < to))
            });
        if self.worktree_drop_target != target || self.project_drop_target.is_some() {
            self.worktree_drop_target = target;
            // A target left behind by an earlier drag would draw a second line.
            self.project_drop_target = None;
            cx.notify();
        }
    }

    fn drop_project_header(
        &mut self,
        dragged: &DraggedProjectHeader,
        target_key: &ProjectGroupKey,
        cx: &mut Context<Self>,
    ) {
        self.project_drop_target = None;
        if self.has_filter_query(cx) {
            return;
        }
        if let Some(multi_workspace) = self.multi_workspace.upgrade() {
            multi_workspace.update(cx, |multi_workspace, cx| {
                multi_workspace.move_project_group_to(&dragged.key, target_key, cx);
            });
        }
    }

    fn update_project_drop_target(
        &mut self,
        dragged_key: &ProjectGroupKey,
        position: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let hovered_key = position
            .filter(|_| !self.has_filter_query(cx))
            .and_then(|position| self.entry_ix_at(position))
            .and_then(|ix| match self.contents.entries.get(ix) {
                Some(ListEntry::ProjectHeader { key, .. }) => Some(key.clone()),
                _ => self.group_key_for_entry(ix),
            });
        let target = hovered_key.and_then(|target_key| {
            let keys = self
                .multi_workspace
                .upgrade()?
                .read(cx)
                .project_group_keys();
            let from = keys.iter().position(|key| key == dragged_key)?;
            let to = keys.iter().position(|key| *key == target_key)?;
            (from != to).then_some((target_key, from < to))
        });
        if self.project_drop_target != target || self.worktree_drop_target.is_some() {
            self.project_drop_target = target;
            // A target left behind by an earlier drag would draw a second line.
            self.worktree_drop_target = None;
            cx.notify();
        }
    }

    fn render_remote_project_icon(
        &self,
        ix: usize,
        host: Option<&RemoteConnectionOptions>,
    ) -> Option<AnyElement> {
        let remote_icon_per_type = match host? {
            RemoteConnectionOptions::Wsl(_) => IconName::Linux,
            RemoteConnectionOptions::Docker(_) => IconName::Box,
            _ => IconName::Server,
        };

        Some(
            div()
                .id(format!("remote-project-icon-{}", ix))
                .child(
                    Icon::new(remote_icon_per_type)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .tooltip(Tooltip::text("Remote Project"))
                .into_any_element(),
        )
    }

    fn render_project_header(
        &self,
        ix: usize,
        is_sticky: bool,
        key: &ProjectGroupKey,
        label: &SharedString,
        highlight_positions: &[usize],
        has_running_threads: bool,
        waiting_thread_count: usize,
        has_notifications: bool,
        is_active: bool,
        is_focused: bool,
        has_threads: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let host = key.host();

        let has_filter = self.has_filter_query(cx);

        let id_prefix = if is_sticky { "sticky-" } else { "" };
        let id = SharedString::from(format!("{id_prefix}project-header-{ix}"));
        let group_name = SharedString::from(format!("{id_prefix}header-group-{ix}"));

        let is_collapsed = self.is_group_collapsed(key, cx);
        let is_creating_worktree = self.is_creating_worktree(key, cx);
        let disclosure_icon = if is_collapsed {
            IconName::ChevronRight
        } else {
            IconName::ChevronDown
        };

        let key_for_toggle = key.clone();
        let key_for_focus = key.clone();

        // The editor entity can only be rendered once per frame.
        let rename_editor = (!is_sticky
            && matches!(&self.rename_target, Some(RenameTarget::Project(target)) if target == key))
        .then(|| self.render_rename_title_editor(cx));

        let color = cx.theme().colors();
        let sidebar_base_bg = if is_sticky {
            color.panel_background_for_overlay()
        } else {
            color.panel_background
        };

        // The fade gradient renders as a visible patch on transparent windows,
        // so truncate the label instead.
        let opaque_window = cx.theme().window_background_appearance()
            == WindowBackgroundAppearance::Opaque
            && sidebar_base_bg.a >= 1.0;

        let can_drag = !has_filter && rename_editor.is_none();
        let dragged_label = label.clone();

        let label = if let Some(rename_editor) = rename_editor {
            rename_editor
        } else if highlight_positions.is_empty() {
            Label::new(label.clone())
                .when(!opaque_window, |this| this.truncate())
                .into_any_element()
        } else {
            HighlightedLabel::new(label.clone(), highlight_positions.to_vec())
                .when(!opaque_window, |this| this.truncate())
                .into_any_element()
        };

        let base_bg = color.background.blend(sidebar_base_bg);

        let hover_solid = base_bg.blend(color.ghost_element_hover);
        let active_solid = base_bg.blend(color.ghost_element_active);

        let group_name_for_gradient = group_name.clone();
        let gradient_overlay = move || {
            GradientFade::new(base_bg, hover_solid, active_solid)
                .width(px(92.0))
                .right(px(-2.0))
                .gradient_stop(0.7)
                .when(!has_filter, |this| {
                    this.group_name(group_name_for_gradient.clone())
                })
        };

        let header = h_flex()
            .id(id)
            .group(&group_name)
            .when(!has_filter, |this| this.cursor_pointer())
            .relative()
            .h(Tab::content_height(cx))
            .w_full()
            .pl_2()
            .pr_1p5()
            .justify_between()
            .border_1()
            .border_r_2()
            .map(|this| {
                if is_focused {
                    this.border_color(color.panel_focused_border)
                } else {
                    this.border_color(gpui::transparent_black())
                }
            })
            .when(!has_filter, |this| {
                this.hover(|s| s.bg(color.ghost_element_hover))
                    .group_active(&group_name, |s| s.bg(color.ghost_element_active))
            })
            .child(
                h_flex()
                    .relative()
                    .min_w_0()
                    .w_full()
                    .gap_1()
                    .child(label)
                    .when_some(self.render_remote_project_icon(ix, host), |this, icon| {
                        this.child(icon)
                    })
                    .when(is_collapsed, |this| {
                        this.children(collapsed_group_status(
                            format!("{id_prefix}waiting-indicator-{ix}"),
                            has_running_threads,
                            waiting_thread_count,
                            has_notifications,
                        ))
                    })
                    .when(!has_filter, |this| {
                        this.child(
                            div()
                                .when(!is_focused && !is_creating_worktree, |this| {
                                    this.visible_on_hover(&group_name)
                                })
                                .child(
                                    Icon::new(disclosure_icon)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                    }),
            )
            .children(opaque_window.then(|| gradient_overlay()))
            .child(
                h_flex()
                    .children(opaque_window.then(|| gradient_overlay()))
                    .child(
                        h_flex()
                            .gap_px()
                            .pr(SIDEBAR_ACTION_RIGHT_INSET - px(8.))
                            .when(self.can_create_worktree(key, cx), |this| {
                                this.child(self.render_new_worktree_button(
                                    ix,
                                    id_prefix,
                                    key,
                                    &group_name,
                                    is_creating_worktree,
                                    cx,
                                ))
                            })
                            .child(self.render_project_header_ellipsis_menu(
                                ix,
                                id_prefix,
                                key,
                                is_active,
                                has_threads,
                                &group_name,
                                is_creating_worktree,
                                cx,
                            ))
                            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            }),
                    ),
            )
            .on_mouse_down(gpui::MouseButton::Right, {
                let menu_handle = self
                    .project_header_menu_handles
                    .get(&ix)
                    .cloned()
                    .unwrap_or_default();
                move |_, window, cx| {
                    cx.stop_propagation();
                    menu_handle.toggle(window, cx);
                }
            })
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    if event.modifiers().secondary(cx) {
                        this.activate_or_open_workspace_for_group(&key_for_focus, window, cx);
                    } else if !this.has_filter_query(cx)
                        && !matches!(&this.rename_target, Some(RenameTarget::Project(target)) if *target == key_for_toggle)
                    {
                        this.toggle_collapse(&key_for_toggle, window, cx);
                    }
                }),
            )
            .when(can_drag, |this| {
                let dragged_header = DraggedProjectHeader {
                    key: key.clone(),
                    label: dragged_label,
                };
                this.on_drag(dragged_header, |dragged_header, _, _, cx| {
                    cx.new(|_| dragged_header.clone())
                })
            })
            .on_drop(cx.listener({
                let key = key.clone();
                move |this, dragged: &DraggedProjectHeader, _window, cx| {
                    this.drop_project_header(dragged, &key, cx);
                }
            }))
            .block_mouse_except_scroll();

        header.into_any_element()
    }

    fn render_worktree_header(
        &self,
        ix: usize,
        is_selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(ListEntry::WorktreeHeader {
            id,
            label,
            branch,
            extra_worktrees,
            is_linked,
            has_rows,
            is_collapsed,
            previous,
            next,
            has_running_threads,
            waiting_thread_count,
            has_active_agent,
            has_notifications,
            pull_request,
            ..
        }) = self.contents.entries.get(ix).cloned()
        else {
            return div().into_any_element();
        };
        let has_filter = self.has_filter_query(cx);
        let has_sibling_groups = previous.is_some() || next.is_some();
        // Deleting a worktree out from under a running agent would break its run.
        let can_delete = is_linked && !has_active_agent;
        let has_menu = !has_filter && (has_sibling_groups || can_delete);
        let menu_handle = self
            .worktree_header_menu_handles
            .get(&ix)
            .cloned()
            .unwrap_or_default();
        let is_menu_open = menu_handle.is_deployed();
        let right_click_handle = menu_handle.clone();
        let sidebar = cx.entity();
        let group_name = SharedString::from(format!("worktree-header-{ix}"));
        let colors = cx.theme().colors();
        let dragged_header = DraggedWorktreeHeader {
            id: id.clone(),
            label: std::iter::once(label.as_ref())
                .chain(extra_worktrees.iter().map(|(name, _)| name.as_ref()))
                .collect::<Vec<_>>()
                .join(" / ")
                .into(),
        };
        let panel_bg = colors.background.blend(colors.panel_background);
        let base_bg = if is_selected {
            panel_bg.blend(colors.ghost_element_selected)
        } else {
            panel_bg
        };
        // The fade gradient renders as a visible patch on transparent windows.
        let opaque_window = cx.theme().window_background_appearance()
            == WindowBackgroundAppearance::Opaque
            && colors.panel_background.a >= 1.0;
        let has_pull_request = pull_request.is_some();
        let hovered_fade_width = match (has_pull_request, has_filter) {
            (true, true) => px(72.0),
            (true, false) => px(168.0),
            (false, _) => px(120.0),
        };
        let buttons_fade = GradientFade::new(
            base_bg,
            panel_bg.blend(colors.ghost_element_hover),
            panel_bg.blend(colors.ghost_element_active),
        )
        .width(if has_pull_request && !is_menu_open {
            px(72.0)
        } else {
            hovered_fade_width
        })
        .width_hovered(hovered_fade_width)
        .gradient_stop(0.5)
        .group_name(group_name.clone());

        let header = h_flex()
            .id(("worktree-header", ix))
            .group(group_name.clone())
            .relative()
            .w_full()
            .h(Tab::content_height(cx))
            .flex_none()
            .pr_1p5()
            .border_r_2()
            .border_color(gpui::transparent_black())
            .pl(px(10. + WORKTREE_GROUP_INDENT - 2.))
            .cursor_pointer()
            .hover(|style| style.bg(colors.ghost_element_hover))
            .when(is_selected, |this| this.bg(colors.ghost_element_selected))
            .child(
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .overflow_hidden()
                    .child(Icon::new(IconName::GitWorktree).size(IconSize::Small))
                    .child(Label::new(label).truncate())
                    .when_some(branch, |this, branch| {
                        this.child(
                            Label::new("•").color(Color::Custom(colors.text_muted.opacity(0.4))),
                        )
                        .child(Icon::new(IconName::GitBranch).size(IconSize::Small))
                        .child(Label::new(branch).truncate())
                    })
                    .children(extra_worktrees.into_iter().flat_map(|(name, branch)| {
                        [
                            Label::new("/")
                                .color(Color::Custom(colors.text_muted.opacity(0.4)))
                                .into_any_element(),
                            Icon::new(IconName::GitWorktree)
                                .size(IconSize::Small)
                                .into_any_element(),
                            Label::new(name).truncate().into_any_element(),
                        ]
                        .into_iter()
                        .chain(branch.into_iter().flat_map(|branch| {
                            [
                                Label::new("•")
                                    .color(Color::Custom(colors.text_muted.opacity(0.4)))
                                    .into_any_element(),
                                Icon::new(IconName::GitBranch)
                                    .size(IconSize::Small)
                                    .into_any_element(),
                                Label::new(branch).truncate().into_any_element(),
                            ]
                        }))
                    }))
                    .when(is_collapsed, |this| {
                        this.children(collapsed_group_status(
                            ("worktree-header-waiting", ix),
                            has_running_threads,
                            waiting_thread_count,
                            has_notifications,
                        ))
                    })
                    .child(
                        div()
                            .when(!is_selected, |this| {
                                this.visible_on_hover(group_name.clone())
                            })
                            .child(
                                Icon::new(if is_collapsed {
                                    IconName::ChevronRight
                                } else {
                                    IconName::ChevronDown
                                })
                                .size(IconSize::Small)
                                .color(Color::Muted),
                            ),
                    ),
            )
            // The buttons float over the labels so the labels can use the full width.
            // The zero-width anchor keeps them inside the header's padding and border.
            .when(!has_filter || has_pull_request, |this| {
                this.child(
                    div().relative().flex_none().w_0().h_full().child(
                        h_flex()
                            .absolute()
                            .top_0()
                            .right_0()
                            .h_full()
                            .pr(SIDEBAR_ACTION_RIGHT_INSET - px(8.))
                            .gap_0p5()
                            .when(opaque_window, |this| {
                                this.child(
                                    div()
                                        .absolute()
                                        .inset_0()
                                        .when(!has_pull_request && !is_menu_open, |this| {
                                            this.visible_on_hover(group_name.clone())
                                        })
                                        .child(buttons_fade),
                                )
                            })
                            .when(!has_filter, |this| {
                                this.child(
                                    h_flex()
                                        .gap_0p5()
                                        // Pressing a button must not start a header drag.
                                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                            cx.stop_propagation()
                                        })
                                        .child(
                                            IconButton::new(
                                                SharedString::from(format!(
                                                    "worktree-header-new-thread-{ix}"
                                                )),
                                                IconName::Plus,
                                            )
                                            .icon_size(IconSize::Small)
                                            .hover_background(colors.element_background)
                                            .active_background(colors.element_active)
                                            .tooltip(Tooltip::text("New Thread In Worktree"))
                                            .when(!is_menu_open, |this| {
                                                this.visible_on_hover(group_name.clone())
                                            })
                                            .on_click(
                                                cx.listener(move |this, _, window, cx| {
                                                    cx.stop_propagation();
                                                    this.new_entry_in_worktree_group(
                                                        ix,
                                                        NewEntryTarget::LastCreatedKind,
                                                        window,
                                                        cx,
                                                    );
                                                }),
                                            ),
                                        )
                                        .when(has_menu, |this| {
                                            this.child(
                                                PopoverMenu::new(format!(
                                                    "worktree-header-menu-{ix}"
                                                ))
                                                .with_handle(menu_handle)
                                                .trigger(
                                                    IconButton::new(
                                                        SharedString::from(format!(
                                                            "worktree-header-ellipsis-{ix}"
                                                        )),
                                                        IconName::Ellipsis,
                                                    )
                                                    .icon_size(IconSize::Small)
                                                    .hover_background(colors.element_background)
                                                    .active_background(colors.element_active)
                                                    .selected_style(ButtonStyle::Tinted(
                                                        TintColor::Accent,
                                                    ))
                                                    .when(!is_menu_open, |this| {
                                                        this.visible_on_hover(group_name.clone())
                                                    }),
                                                )
                                                .menu({
                                                    let id = id.clone();
                                                    move |window, cx| {
                                                        let sidebar = sidebar.clone();
                                                        let id = id.clone();
                                                        let previous = previous.clone();
                                                        let next = next.clone();
                                                        let has_move_entries =
                                                            previous.is_some() || next.is_some();
                                                        Some(ContextMenu::build(
                                                            window,
                                                            cx,
                                                            move |mut menu, _window, _cx| {
                                                                for (label, action, target) in [
                                                                    (
                                                                        "Move Up",
                                                                        Box::new(MoveProjectUp)
                                                                            as Box<
                                                                                dyn gpui::Action,
                                                                            >,
                                                                        previous,
                                                                    ),
                                                                    (
                                                                        "Move Down",
                                                                        Box::new(MoveProjectDown),
                                                                        next,
                                                                    ),
                                                                ] {
                                                                    let Some(target) = target
                                                                    else {
                                                                        continue;
                                                                    };
                                                                    let sidebar = sidebar.clone();
                                                                    let id = id.clone();
                                                                    menu = menu.entry(
                                                                        label,
                                                                        Some(action),
                                                                        move |_, cx| {
                                                                            sidebar.update(
                                                                                cx,
                                                                                |sidebar, cx| {
                                                                                    sidebar
                                                                            .move_worktree_group(
                                                                                &id, &target, cx,
                                                                            );
                                                                                },
                                                                            );
                                                                        },
                                                                    );
                                                                }
                                                                if can_delete {
                                                                    if has_move_entries {
                                                                        menu = menu.separator();
                                                                    }
                                                                    menu = menu.entry(
                                                                        "Delete Worktree",
                                                                        None,
                                                                        move |window, cx| {
                                                                            sidebar.update(
                                                                                cx,
                                                                                |sidebar, cx| {
                                                                                    sidebar
                                                                            .delete_worktree_group(
                                                                                &id, window, cx,
                                                                            );
                                                                                },
                                                                            );
                                                                        },
                                                                    );
                                                                }
                                                                menu
                                                            },
                                                        ))
                                                    }
                                                }),
                                            )
                                        }),
                                )
                            })
                            .when_some(pull_request, |this, pull_request| {
                                let state_color = match pull_request.state {
                                    PullRequestState::Open => Color::Success,
                                    PullRequestState::Merged => {
                                        Color::Custom(colors.terminal_ansi_magenta)
                                    }
                                    PullRequestState::Closed => Color::Error,
                                    PullRequestState::Draft => Color::Muted,
                                };
                                this.child(
                                    div()
                                        .relative()
                                        // Pressing the button must not start a header drag.
                                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                            cx.stop_propagation()
                                        })
                                        .child(
                                            IconButton::new(
                                                SharedString::from(format!(
                                                    "worktree-header-pull-request-{ix}"
                                                )),
                                                IconName::PullRequest,
                                            )
                                            .icon_size(IconSize::Small)
                                            .hover_background(colors.element_background)
                                            .active_background(colors.element_active)
                                            .tooltip(Tooltip::text(
                                                if pull_request.urls.len() > 1 {
                                                    "Open Pull Requests"
                                                } else {
                                                    "Open Pull Request"
                                                },
                                            ))
                                            .on_click(
                                                move |_, _, cx| {
                                                    cx.stop_propagation();
                                                    for url in &pull_request.urls {
                                                        cx.open_url(url);
                                                    }
                                                },
                                            ),
                                        )
                                        .child(div().absolute().top_px().right_px().child(
                                            Indicator::dot().dot_size(px(5.)).color(state_color),
                                        )),
                                )
                            }),
                    ),
                )
            })
            .when(has_menu, |this| {
                this.on_mouse_down(gpui::MouseButton::Right, move |_, window, cx| {
                    cx.stop_propagation();
                    right_click_handle.toggle(window, cx);
                })
            })
            .on_click(cx.listener({
                let id = id.clone();
                move |this, _, _window, cx| {
                    this.selection = Some(ix);
                    this.toggle_worktree_group(&id, cx);
                }
            }))
            .when(!has_filter, |this| {
                this.on_drag(dragged_header, |dragged_header, _, _, cx| {
                    cx.new(|_| dragged_header.clone())
                })
            })
            .on_drop(cx.listener({
                let id = id.clone();
                move |this, dragged: &DraggedWorktreeHeader, _window, cx| {
                    this.move_worktree_group(&dragged.id, &id, cx);
                }
            }));

        if is_collapsed || has_rows {
            return header.into_any_element();
        }
        v_flex()
            .w_full()
            .on_drop(
                cx.listener(move |this, dragged: &DraggedWorktreeHeader, _window, cx| {
                    this.move_worktree_group(&dragged.id, &id, cx);
                }),
            )
            .child(header)
            .child(
                h_flex()
                    .pr_2()
                    .pl(px(11. + 2. * WORKTREE_GROUP_INDENT))
                    .pt_1()
                    .pb_2()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::Circle)
                            .size(IconSize::Small)
                            .color(Color::Custom(colors.icon_placeholder.opacity(0.1))),
                    )
                    .child(Label::new("No threads yet").color(Color::Placeholder)),
            )
            .into_any_element()
    }

    fn is_creating_worktree(&self, key: &ProjectGroupKey, cx: &App) -> bool {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return false;
        };
        self.groups_creating_worktree.contains(key)
            || multi_workspace
                .read(cx)
                .workspaces_for_project_group(key, cx)
                .any(|workspace| {
                    workspace
                        .read(cx)
                        .active_worktree_creation()
                        .label
                        .is_some()
                })
    }

    fn can_create_worktree(&self, key: &ProjectGroupKey, cx: &App) -> bool {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return false;
        };
        let mut workspaces = multi_workspace
            .read(cx)
            .workspaces_for_project_group(key, cx)
            .peekable();
        if workspaces.peek().is_none() {
            // Git lists at least the main worktree of any repository.
            // Only local single-path groups get a listing; until it arrives we can't
            // tell whether the project is a Git repository, so hide the button.
            let is_listed = key.host().is_none() && key.path_list().paths().len() == 1;
            return self
                .closed_group_worktrees
                .get(key)
                .map_or(!is_listed, |worktrees| !worktrees.is_empty());
        }
        workspaces.any(|workspace| {
            let project = workspace.read(cx).project().read(cx);
            !project.is_via_collab() && !project.repositories(cx).is_empty()
        })
    }

    fn render_new_worktree_button(
        &self,
        ix: usize,
        id_prefix: &str,
        key: &ProjectGroupKey,
        group_name: &SharedString,
        is_creating: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if is_creating {
            return h_flex()
                .size_6()
                .justify_center()
                .child(
                    Icon::new(IconName::LoadCircle)
                        .size(IconSize::Small)
                        .color(Color::Muted)
                        .with_rotate_animation(2),
                )
                .into_any_element();
        }
        let key = key.clone();
        IconButton::new(
            SharedString::from(format!("{id_prefix}project-header-new-worktree-{ix}")),
            IconName::Plus,
        )
        // A filtered header has no hover background for these to contrast with.
        .when(!self.has_filter_query(cx), |button| {
            let color = cx.theme().colors();
            button
                .hover_background(color.element_background)
                .active_background(color.element_active)
        })
        .icon_size(IconSize::Small)
        .visible_on_hover(group_name)
        .tooltip(Tooltip::text("Create New Worktree"))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.create_worktree_for_group(&key, window, cx);
        }))
        .into_any_element()
    }

    fn create_worktree_for_group(
        &mut self,
        key: &ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        self.set_group_expanded(key, true, cx);
        self.selection = None;
        self.groups_creating_worktree.insert(key.clone());
        cx.notify();

        let open_workspace = {
            let multi_workspace = multi_workspace.read(cx);
            let active = multi_workspace.workspace();
            let group_workspaces: Vec<_> = multi_workspace
                .workspaces_for_project_group(key, cx)
                .collect();
            group_workspaces
                .iter()
                .find(|workspace| **workspace == active)
                .or(group_workspaces.first())
                .map(|workspace| (*workspace).clone())
        };
        let workspace_task = match open_workspace {
            Some(workspace) => Task::ready(Ok(workspace)),
            None => {
                let active_workspace = multi_workspace.read(cx).workspace().clone();
                multi_workspace.update(cx, |this, cx| {
                    this.find_or_create_workspace(
                        key.path_list().clone(),
                        key.host().cloned(),
                        Some(key.clone()),
                        |options, window, cx| connect_remote(active_workspace, options, window, cx),
                        None,
                        OpenMode::Activate,
                        None,
                        window,
                        cx,
                    )
                })
            }
        };

        let key = key.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let workspace = workspace_task.await?;
                // A workspace that was just opened has no repositories until its scan finishes.
                Self::wait_for_archive_workspace_metadata(&workspace, cx).await;
                let repository = workspace.read_with(cx, |workspace, cx| {
                    let project = workspace.project().read(cx);
                    let repository = project.active_repository(cx)?;
                    let current_branch = repository
                        .read(cx)
                        .branch
                        .as_ref()
                        .map(|branch| branch.name().to_string());
                    Some((
                        repository,
                        project.repositories(cx).len() > 1,
                        current_branch,
                    ))
                });
                let branch_target = match repository {
                    Some((repository, has_multiple_repositories, current_branch)) => {
                        let default_branch = repository
                            .update(cx, |repository, _| repository.default_branch(true))
                            .await
                            .log_err()
                            .and_then(|default_branch| default_branch.log_err())
                            .flatten();
                        worktree_create_targets(
                            has_multiple_repositories,
                            default_branch.as_deref().and_then(RemoteBranchName::parse),
                            current_branch.as_deref(),
                        )
                        .first()
                        .map(|target| target.branch_target())
                        .unwrap_or_default()
                    }
                    None => NewWorktreeBranchTarget::default(),
                };
                let creation = cx.update(|window, cx| {
                    create_worktree_in_workspace(&workspace, branch_target, window, cx)
                })?;
                let new_workspace = creation.await?;
                this.update_in(cx, |this, window, cx| {
                    let workspace = new_workspace.read(cx);
                    let is_agent_panel_open =
                        workspace.panel::<AgentPanel>(cx).is_some_and(|panel| {
                            workspace.all_docks().iter().any(|dock| {
                                dock.read(cx)
                                    .visible_panel()
                                    .is_some_and(|visible| visible.panel_id() == panel.entity_id())
                            })
                        });
                    if !is_agent_panel_open {
                        this.create_new_thread(&new_workspace, window, cx);
                    }
                })?;
                anyhow::Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                this.groups_creating_worktree.remove(&key);
                cx.notify();
            })
            .log_err();
            result
        })
        .detach_and_log_err(cx);
    }

    fn render_project_header_ellipsis_menu(
        &self,
        ix: usize,
        id_prefix: &str,
        project_group_key: &ProjectGroupKey,
        is_active: bool,
        has_threads: bool,
        group_name: &SharedString,
        keep_visible: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let multi_workspace = self.multi_workspace.clone();
        let project_group_key = project_group_key.clone();

        let show_multi_project_entries = multi_workspace
            .read_with(cx, |mw, _| {
                project_group_key.host().is_none() && mw.project_group_keys().len() >= 2
            })
            .unwrap_or(false);

        let this = cx.weak_entity();
        let deleting_worktree_paths = self.deleting_worktree_paths.clone();

        let trigger_id = SharedString::from(format!("{id_prefix}-ellipsis-menu-{ix}"));
        let menu_handle = self
            .project_header_menu_handles
            .get(&ix)
            .cloned()
            .unwrap_or_default();
        let is_menu_open = menu_handle.is_deployed();

        PopoverMenu::new(format!("{id_prefix}project-header-menu-{ix}"))
            .with_handle(menu_handle)
            .trigger(
                IconButton::new(trigger_id, IconName::Ellipsis)
                    .when(!is_menu_open && !self.has_filter_query(cx), |button| {
                        let color = cx.theme().colors();
                        button
                            .hover_background(color.element_background)
                            .active_background(color.element_active)
                    })
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .icon_size(IconSize::Small)
                    .when(!is_menu_open && !keep_visible, |el| {
                        el.visible_on_hover(group_name)
                    }),
            )
            .on_open(Rc::new({
                let this = this.clone();
                move |_window, cx| {
                    this.update(cx, |sidebar, cx| {
                        sidebar.project_header_menu_ix = Some(ix);
                        cx.notify();
                    })
                    .ok();
                }
            }))
            .menu(move |window, cx| {
                let multi_workspace = multi_workspace.clone();
                let project_group_key = project_group_key.clone();
                let this_for_menu = this.clone();

                // A worktree being deleted stays open until its state is saved.
                let open_workspaces = multi_workspace
                    .read_with(cx, |multi_workspace, cx| {
                        let deleting_worktree_paths = deleting_worktree_paths.borrow();
                        multi_workspace
                            .workspaces_for_project_group(&project_group_key, cx)
                            .filter(|workspace| {
                                !workspace
                                    .read(cx)
                                    .root_paths(cx)
                                    .iter()
                                    .any(|root| deleting_worktree_paths.contains(root.as_ref()))
                            })
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                // Compute reorder state at menu-open time so it reflects the
                // most recent group ordering.
                let (group_index, total_groups) = multi_workspace
                    .read_with(cx, |mw, _| {
                        let keys = mw.project_group_keys();
                        let index = keys.iter().position(|k| k == &project_group_key);
                        (index, keys.len())
                    })
                    .unwrap_or((None, 0));
                let show_reorder_entries = total_groups >= 2;
                let can_move_up = group_index.is_some_and(|i| i > 0);
                let can_move_down = group_index.is_some_and(|i| i + 1 < total_groups);

                let active_workspace = multi_workspace
                    .read_with(cx, |multi_workspace, _cx| {
                        multi_workspace.workspace().clone()
                    })
                    .ok();
                let workspace_labels: Vec<_> = open_workspaces
                    .iter()
                    .map(|workspace| workspace_menu_worktree_labels(workspace, cx))
                    .collect();
                let workspace_is_active: Vec<_> = open_workspaces
                    .iter()
                    .map(|workspace| active_workspace.as_ref() == Some(workspace))
                    .collect();

                let menu =
                    ContextMenu::build_persistent(window, cx, move |menu, _window, menu_cx| {
                        let menu = menu.end_slot_action(Box::new(menu::SecondaryConfirm));
                        let weak_menu = menu_cx.weak_entity();

                        let menu = menu.when(show_multi_project_entries, |this| {
                            this.entry(
                                "Open Project in New Window",
                                Some(Box::new(workspace::MoveProjectToNewWindow)),
                                {
                                    let project_group_key = project_group_key.clone();
                                    let multi_workspace = multi_workspace.clone();
                                    move |window, cx| {
                                        multi_workspace
                                            .update(cx, |multi_workspace, cx| {
                                                multi_workspace
                                                    .open_project_group_in_new_window(
                                                        &project_group_key,
                                                        window,
                                                        cx,
                                                    )
                                                    .detach_and_log_err(cx);
                                            })
                                            .ok();
                                    }
                                },
                            )
                        });

                        let menu = menu
                            .custom_entry(
                                {
                                    move |_window, cx| {
                                        let action = h_flex()
                                            .opacity(0.6)
                                            .children(render_modifiers(
                                                &Modifiers::secondary_key(cx),
                                                PlatformStyle::platform(),
                                                None,
                                                Some(TextSize::Default.rems(cx).into()),
                                                false,
                                            ))
                                            .child(Label::new("-click").color(Color::Muted));

                                        let label = if has_threads {
                                            "Focus Last Project"
                                        } else {
                                            "Focus Project"
                                        };

                                        h_flex()
                                            .w_full()
                                            .justify_between()
                                            .gap_4()
                                            .child(
                                                Label::new(label)
                                                    .when(is_active, |s| s.color(Color::Disabled)),
                                            )
                                            .child(action)
                                            .into_any_element()
                                    }
                                },
                                {
                                    let project_group_key = project_group_key.clone();
                                    let this = this_for_menu.clone();
                                    move |window, cx| {
                                        if is_active {
                                            return;
                                        }
                                        this.update(cx, |sidebar, cx| {
                                            sidebar.activate_or_open_workspace_for_group(
                                                &project_group_key,
                                                window,
                                                cx,
                                            );
                                        })
                                        .ok();
                                    }
                                },
                            )
                            .selectable(!is_active);

                        let menu = menu.entry("Rename", None, {
                            let project_group_key = project_group_key.clone();
                            let weak_menu = weak_menu.clone();
                            let this = this_for_menu.clone();
                            move |window, cx| {
                                weak_menu.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
                                this.update(cx, |sidebar, cx| {
                                    sidebar.start_renaming_project(&project_group_key, window, cx);
                                })
                                .ok();
                            }
                        });

                        let menu = if open_workspaces.is_empty() {
                            menu
                        } else {
                            let mut menu = menu.separator().header("Open Worktrees");

                            for (
                                workspace_index,
                                ((workspace, workspace_label), is_active_workspace),
                            ) in open_workspaces
                                .iter()
                                .cloned()
                                .zip(workspace_labels.iter().cloned())
                                .zip(workspace_is_active.iter().copied())
                                .enumerate()
                            {
                                let activate_multi_workspace = multi_workspace.clone();
                                let close_multi_workspace = multi_workspace.clone();
                                let activate_weak_menu = weak_menu.clone();
                                let close_weak_menu = weak_menu.clone();
                                let activate_workspace = workspace.clone();

                                menu = menu.custom_entry(
                                    move |_window, _cx| {
                                        let close_multi_workspace = close_multi_workspace.clone();
                                        let close_weak_menu = close_weak_menu.clone();
                                        let close_workspace = workspace.clone();
                                        let row_group_name = SharedString::from(format!(
                                            "workspace-menu-row-{workspace_index}"
                                        ));

                                        h_flex()
                                            .group(&row_group_name)
                                            .w_full()
                                            .gap_2()
                                            .justify_between()
                                            .child(h_flex().min_w_0().gap_1().children(
                                                workspace_label.iter().enumerate().map(
                                                    |(label_ix, label)| {
                                                        h_flex()
                                                            .gap_1()
                                                            .when(label_ix > 0, |this| {
                                                                this.child(
                                                                    Label::new("•").alpha(0.25),
                                                                )
                                                            })
                                                            .child(label.render())
                                                            .into_any_element()
                                                    },
                                                ),
                                            ))
                                            .when(is_active_workspace, |this| {
                                                this.pr_1().child(
                                                    Icon::new(IconName::Check)
                                                        .size(IconSize::Small)
                                                        .color(Color::Accent),
                                                )
                                            })
                                            .when(!is_active_workspace, |this| {
                                                let close_multi_workspace =
                                                    close_multi_workspace.clone();
                                                let close_weak_menu = close_weak_menu.clone();
                                                let close_workspace = close_workspace.clone();

                                                this.child(
                                                    IconButton::new(
                                                        ("close-workspace", workspace_index),
                                                        IconName::Close,
                                                    )
                                                    .icon_size(IconSize::Small)
                                                    .visible_on_hover(&row_group_name)
                                                    .tooltip(Tooltip::text("Close Worktree"))
                                                    .on_click(move |_, window, cx| {
                                                        cx.stop_propagation();
                                                        window.prevent_default();
                                                        close_multi_workspace
                                                            .update(cx, |multi_workspace, cx| {
                                                                multi_workspace
                                                                    .remove(
                                                                        [close_workspace.clone()],
                                                                        RemovalIntent::CloseProject,
                                                                        window,
                                                                        cx,
                                                                    )
                                                                    .detach_and_log_err(cx);
                                                            })
                                                            .ok();
                                                        close_weak_menu
                                                            .update(cx, |_, cx| {
                                                                cx.emit(DismissEvent)
                                                            })
                                                            .ok();
                                                    }),
                                                )
                                            })
                                            .into_any_element()
                                    },
                                    move |window, cx| {
                                        activate_multi_workspace
                                            .update(cx, |multi_workspace, cx| {
                                                multi_workspace.activate(
                                                    activate_workspace.clone(),
                                                    None,
                                                    window,
                                                    cx,
                                                );
                                            })
                                            .ok();
                                        activate_weak_menu
                                            .update(cx, |_, cx| cx.emit(DismissEvent))
                                            .ok();
                                    },
                                );
                            }

                            menu
                        };

                        let menu = menu.when(show_reorder_entries, |this| {
                            let move_up_multi_workspace = multi_workspace.clone();
                            let move_up_key = project_group_key.clone();
                            let move_up_weak_menu = weak_menu.clone();
                            let move_down_multi_workspace = multi_workspace.clone();
                            let move_down_key = project_group_key.clone();
                            let move_down_weak_menu = weak_menu.clone();

                            this.separator()
                                .when(can_move_up, |this| {
                                    this.item(
                                        ContextMenuEntry::new("Move Up")
                                            .action(Box::new(MoveProjectUp))
                                            .handler(move |_window, cx| {
                                                move_up_multi_workspace
                                                    .update(cx, |mw, cx| {
                                                        mw.move_project_group_up(&move_up_key, cx);
                                                    })
                                                    .ok();
                                                move_up_weak_menu
                                                    .update(cx, |_, cx| cx.emit(DismissEvent))
                                                    .ok();
                                            }),
                                    )
                                })
                                .when(can_move_down, |this| {
                                    this.item(
                                        ContextMenuEntry::new("Move Down")
                                            .action(Box::new(MoveProjectDown))
                                            .handler(move |_window, cx| {
                                                move_down_multi_workspace
                                                    .update(cx, |mw, cx| {
                                                        mw.move_project_group_down(
                                                            &move_down_key,
                                                            cx,
                                                        );
                                                    })
                                                    .ok();
                                                move_down_weak_menu
                                                    .update(cx, |_, cx| cx.emit(DismissEvent))
                                                    .ok();
                                            }),
                                    )
                                })
                        });

                        let project_group_key = project_group_key.clone();
                        let remove_multi_workspace = multi_workspace.clone();
                        let this = this_for_menu.clone();
                        menu.separator().entry("Remove", None, move |window, cx| {
                            let removal = remove_multi_workspace
                                .update(cx, |multi_workspace, cx| {
                                    multi_workspace.remove_project_group(
                                        &project_group_key,
                                        window,
                                        cx,
                                    )
                                })
                                .ok();
                            if let Some(removal) = removal {
                                let this = this.clone();
                                let project_group_key = project_group_key.clone();
                                cx.spawn(async move |cx| {
                                    if removal.await? {
                                        this.update(cx, |sidebar, cx| {
                                            sidebar
                                                .multi_root_worktree_groups
                                                .retain(|(key, _)| *key != project_group_key);
                                            sidebar.serialize(cx);
                                        })?;
                                    }
                                    anyhow::Ok(())
                                })
                                .detach_and_log_err(cx);
                            }
                            weak_menu.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
                        })
                    });

                let this = this.clone();

                window
                    .subscribe(&menu, cx, move |_, _: &gpui::DismissEvent, _window, cx| {
                        this.update(cx, |sidebar, cx| {
                            sidebar.project_header_menu_ix = None;
                            cx.notify();
                        })
                        .ok();
                    })
                    .detach();

                Some(menu)
            })
            .anchor(gpui::Anchor::TopRight)
            .offset(gpui::Point {
                x: px(0.),
                y: px(1.),
            })
            .into_any_element()
    }

    fn render_sticky_header(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let scroll_top = self.list_state.logical_scroll_top();

        let &header_idx = self
            .contents
            .project_header_indices
            .iter()
            .rev()
            .find(|&&idx| idx <= scroll_top.item_ix)?;

        let needs_sticky = header_idx < scroll_top.item_ix
            || (header_idx == scroll_top.item_ix && scroll_top.offset_in_item > px(0.));

        if !needs_sticky {
            return None;
        }

        let ListEntry::ProjectHeader {
            key,
            label,
            highlight_positions,
            has_running_threads,
            waiting_thread_count,
            has_notifications,
            is_active,
            has_threads,
        } = self.contents.entries.get(header_idx)?
        else {
            return None;
        };

        let is_focused = self.focus_handle.is_focused(window);
        let is_selected = is_focused && self.selection == Some(header_idx);

        let header_element = self.render_project_header(
            header_idx,
            true,
            key,
            &label,
            &highlight_positions,
            *has_running_threads,
            *waiting_thread_count,
            *has_notifications,
            *is_active,
            is_selected,
            *has_threads,
            cx,
        );

        let top_offset = self
            .contents
            .project_header_indices
            .iter()
            .find(|&&idx| idx > header_idx)
            .and_then(|&next_idx| {
                let bounds = self.list_state.bounds_for_item(next_idx)?;
                let viewport = self.list_state.viewport_bounds();
                let y_in_viewport = bounds.origin.y - viewport.origin.y;
                let header_height = bounds.size.height;
                (y_in_viewport < header_height).then_some(y_in_viewport - header_height)
            })
            .unwrap_or(px(0.));

        let color = cx.theme().colors();
        let background = color.panel_background_for_overlay();

        let element = v_flex()
            .absolute()
            .top(top_offset)
            .left_0()
            .w_full()
            .bg(background)
            .border_b_1()
            .border_color(color.border.opacity(0.5))
            .child(header_element)
            .shadow_sm()
            .into_any_element();

        Some(element)
    }

    fn toggle_collapse(
        &mut self,
        project_group_key: &ProjectGroupKey,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_collapsed = self.is_group_collapsed(project_group_key, cx);
        self.set_group_expanded(project_group_key, is_collapsed, cx);
        self.update_entries(cx);
    }

    fn toggle_worktree_group(&mut self, id: &SharedString, cx: &mut Context<Self>) {
        // Search shows every group expanded, so a toggle would change saved state invisibly.
        if self.has_filter_query(cx) {
            return;
        }
        if !self.collapsed_worktree_groups.remove(id) {
            self.collapsed_worktree_groups.insert(id.clone());
        }
        self.prune_worktree_groups(id);
        self.serialize(cx);
        self.update_entries(cx);
    }

    /// Remembers the root combinations in use in multi-root projects. Returns whether
    /// any were new.
    fn save_multi_root_worktree_groups(&mut self, cx: &App) -> bool {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return false;
        };
        let thread_store = ThreadMetadataStore::global(cx);
        let terminal_store = TerminalThreadMetadataStore::try_global(cx);
        let mut in_use: Vec<(ProjectGroupKey, WorktreePaths)> = Vec::new();
        for group in multi_workspace.read(cx).project_groups(cx) {
            if group.key.path_list().paths().len() <= 1 {
                continue;
            }
            for workspace in &group.workspaces {
                let paths = workspace.read(cx).project().read(cx).worktree_paths(cx);
                in_use.push((group.key.clone(), paths));
            }
            for thread in thread_store
                .read(cx)
                .entries_for_main_worktree_path(group.key.path_list(), group.key.host())
            {
                in_use.push((group.key.clone(), thread.worktree_paths.clone()));
            }
            if let Some(terminal_store) = &terminal_store {
                for terminal in terminal_store
                    .read(cx)
                    .entries_for_main_worktree_path(group.key.path_list(), group.key.host())
                {
                    in_use.push((group.key.clone(), terminal.worktree_paths.clone()));
                }
            }
        }

        let mut changed = false;
        for (key, paths) in in_use {
            let is_group_of_key = key.path_list().paths().len() > 1
                && paths.main_worktree_path_list().paths() == key.path_list().paths();
            // The group of the main worktrees always has a header.
            let has_linked_root = paths
                .ordered_pairs()
                .any(|(main_path, folder_path)| main_path != folder_path);
            let is_saved = self
                .multi_root_worktree_groups
                .iter()
                .any(|(saved_key, saved)| {
                    *saved_key == key
                        && saved.folder_path_list().paths() == paths.folder_path_list().paths()
                });
            if is_group_of_key && has_linked_root && !is_saved {
                self.multi_root_worktree_groups.push((key, paths));
                changed = true;
            }
        }
        changed
    }

    /// Saves the displayed position of groups that have none, so that a group does
    /// not move when its rows come and go. Returns whether anything was saved.
    fn save_new_worktree_group_positions(&mut self) -> bool {
        let order = &mut self.worktree_group_order;
        let mut changed = false;
        // New groups are shown above the saved ones of their project, in this order.
        for id in worktree_group_ids(&self.contents.entries).rev() {
            if order.contains(id) {
                continue;
            }
            let project = worktree_group_project(id);
            let position = order
                .iter()
                .position(|saved| worktree_group_project(saved) == project)
                .unwrap_or(order.len());
            order.insert(position, id.clone());
            changed = true;
        }
        changed
    }

    /// Drops saved state for groups that no longer exist in the project `id` belongs to.
    /// Only called from user actions on that project, when all of its groups are listed.
    fn prune_worktree_groups(&mut self, id: &SharedString) {
        let project = worktree_group_project(id);
        let listed: HashSet<&SharedString> = worktree_group_ids(&self.contents.entries).collect();
        let is_live = |saved: &SharedString| {
            worktree_group_project(saved) != project || listed.contains(saved)
        };
        self.worktree_group_order.retain(is_live);
        self.collapsed_worktree_groups.retain(is_live);
    }

    fn move_worktree_group(
        &mut self,
        moving: &SharedString,
        target: &SharedString,
        cx: &mut Context<Self>,
    ) {
        self.worktree_drop_target = None;
        if moving == target || self.has_filter_query(cx) {
            return;
        }
        let project = worktree_group_project(moving);
        // The displayed order, since groups without a saved position are shown first.
        let mut ids: Vec<SharedString> = worktree_group_ids(&self.contents.entries)
            .filter(|id| worktree_group_project(id) == project)
            .cloned()
            .collect();
        let positions = (
            ids.iter().position(|id| id == moving),
            ids.iter().position(|id| id == target),
        );
        let (Some(moving_position), Some(target_position)) = positions else {
            return;
        };
        let moved = ids.remove(moving_position);
        ids.insert(target_position, moved);
        let is_moving = |entry: &ListEntry| matches!(entry, ListEntry::WorktreeHeader { id, .. } if id == moving);
        let was_selected = self
            .selection
            .and_then(|ix| self.contents.entries.get(ix))
            .is_some_and(is_moving);
        self.prune_worktree_groups(moving);
        self.worktree_group_order.retain(|id| !ids.contains(id));
        self.worktree_group_order.extend(ids);
        self.serialize(cx);
        self.update_entries(cx);
        if was_selected {
            self.selection = self.contents.entries.iter().position(is_moving);
            cx.notify();
        }
    }

    fn workspace_for_worktree_group(
        &self,
        ix: usize,
        roots: &[SharedString],
        cx: &App,
    ) -> Option<Entity<Workspace>> {
        let key = self.group_key_for_entry(ix)?;
        let multi_workspace = self.multi_workspace.upgrade()?;
        multi_workspace
            .read(cx)
            .workspaces_for_project_group(&key, cx)
            .find(|workspace| {
                let workspace_roots = workspace.read(cx).root_paths(cx);
                workspace_roots.len() == roots.len()
                    && workspace_roots.iter().all(|workspace_root| {
                        roots
                            .iter()
                            .any(|root| workspace_root.to_string_lossy() == root.as_ref())
                    })
            })
            .cloned()
    }

    fn move_selected_worktree_group(&mut self, offset: isize, cx: &mut Context<Self>) -> bool {
        let Some(ListEntry::WorktreeHeader {
            id, previous, next, ..
        }) = self
            .selection
            .and_then(|selected| self.contents.entries.get(selected))
        else {
            return false;
        };
        let id = id.clone();
        if let Some(target) = if offset < 0 { previous } else { next }.clone() {
            self.move_worktree_group(&id, &target, cx);
        }
        true
    }

    /// Starts a new entry in the worktree group headed by entry `ix`, opening its
    /// workspace first when it is closed. Returns false when `ix` is not a worktree header.
    fn new_entry_in_worktree_group(
        &mut self,
        ix: usize,
        target: NewEntryTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(ListEntry::WorktreeHeader { path, roots, .. }) = self.contents.entries.get(ix)
        else {
            return false;
        };
        let path = path.clone();
        let roots = roots.clone();
        let Some(key) = self.group_key_for_entry(ix) else {
            return false;
        };
        self.selection = None;
        if let Some(workspace) = self.workspace_for_worktree_group(ix, &roots, cx) {
            match target {
                NewEntryTarget::LastCreatedKind => self.create_new_entry(&workspace, window, cx),
                NewEntryTarget::Terminal => self.create_new_terminal(&workspace, window, cx),
            }
        } else {
            let is_project_root = roots.iter().all(|root| {
                key.path_list()
                    .paths()
                    .iter()
                    .any(|key_root| key_root.to_string_lossy() == root.as_ref())
            });
            if path.is_empty() || is_project_root {
                let path_list = key.path_list().clone();
                self.open_workspace_and_create_entry(&key, path_list, target, window, cx);
                return true;
            }
            let Some(multi_workspace) = self.multi_workspace.upgrade() else {
                return true;
            };
            let active_workspace = multi_workspace.read(cx).workspace().clone();
            let fs = active_workspace.read(cx).app_state().fs.clone();
            let is_local = key.host().is_none();
            let roots: Vec<PathBuf> = roots
                .iter()
                .map(|root| PathBuf::from(root.as_ref()))
                .collect();
            cx.spawn_in(window, async move |this, cx| {
                // A worktree removed outside Zed keeps its header while it has threads.
                let mut exists = true;
                if is_local {
                    for root in &roots {
                        exists &= fs.is_dir(root).await;
                    }
                }
                if !exists {
                    cx.update(|_window, cx| {
                        Self::show_warning_toast(
                            active_workspace,
                            "This worktree no longer exists",
                            cx,
                        )
                    })?;
                    return anyhow::Ok(());
                }
                this.update_in(cx, |this, window, cx| {
                    this.open_workspace_and_create_entry(
                        &key,
                        PathList::new(&roots),
                        target,
                        window,
                        cx,
                    )
                })
            })
            .detach_and_log_err(cx);
        }
        true
    }

    fn delete_worktree_group(
        &mut self,
        id: &SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((ix, path, roots, display_name)) = self
            .contents
            .entries
            .iter()
            .enumerate()
            .find_map(|(ix, entry)| match entry {
                ListEntry::WorktreeHeader {
                    id: entry_id,
                    path,
                    roots,
                    label,
                    is_linked: true,
                    ..
                } if entry_id == id => Some((
                    ix,
                    PathBuf::from(path.as_ref()),
                    roots
                        .iter()
                        .map(|root| PathBuf::from(root.as_ref()))
                        .collect::<Vec<_>>(),
                    label.clone(),
                )),
                _ => None,
            })
        else {
            return;
        };
        let Some(key) = self.group_key_for_entry(ix) else {
            return;
        };
        let targets = self.worktree_deletion_targets(id, &key, &path);
        if targets.is_empty() {
            return;
        }
        let active_agent_paths = self.worktree_paths_with_active_agents(cx);
        if targets
            .iter()
            .any(|target| active_agent_paths.contains(&target.path))
        {
            return;
        }
        let deletion = WorktreeDeletion {
            path,
            roots,
            targets,
        };

        let archive_workspaces = self.archive_workspaces(cx);
        let mut draft_ids = HashSet::<ThreadId>::default();
        for target in &deletion.targets {
            draft_ids.extend(Self::entries_in_worktree(&target.path, key.host(), cx).1);
        }
        let unsent_draft_count = draft_ids
            .into_iter()
            .filter(|draft_id| {
                agent_ui::draft_prompt_store::draft_has_user_content(
                    *draft_id,
                    &archive_workspaces,
                    cx,
                )
            })
            .count();
        if unsent_draft_count == 0 {
            self.start_worktree_deletion(deletion, key, window, cx);
            return;
        }
        let detail = if unsent_draft_count == 1 {
            "Its unsent draft will be discarded.".to_string()
        } else {
            format!("Its {unsent_draft_count} unsent drafts will be discarded.")
        };
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("Delete worktree \"{display_name}\"?"),
            Some(&detail),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                this.update_in(cx, |this, window, cx| {
                    this.start_worktree_deletion(deletion, key, window, cx);
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// The linked worktrees a group is made of. Roots that are main worktrees stay.
    fn worktree_deletion_targets(
        &self,
        id: &SharedString,
        key: &ProjectGroupKey,
        path: &Path,
    ) -> Vec<WorktreeTarget> {
        let saved_group = self
            .multi_root_worktree_groups
            .iter()
            .filter(|(saved_key, _)| saved_key == key)
            .find(|(_, paths)| {
                let roots = paths
                    .ordered_pairs()
                    .filter_map(|(_, folder_path)| folder_path.to_str());
                &worktree_group_id_for_roots(key, roots) == id
            });
        if let Some((_, paths)) = saved_group {
            return paths
                .ordered_pairs()
                .filter(|(main_path, folder_path)| main_path != folder_path)
                .map(|(main_path, folder_path)| WorktreeTarget {
                    main_path: main_path.clone(),
                    path: folder_path.clone(),
                })
                .collect();
        }
        key.path_list()
            .paths()
            .first()
            .map(|main_path| WorktreeTarget {
                main_path: main_path.clone(),
                path: path.to_path_buf(),
            })
            .into_iter()
            .collect()
    }

    fn start_worktree_deletion(
        &mut self,
        deletion: WorktreeDeletion,
        key: ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let active_agent_paths = self.worktree_paths_with_active_agents(cx);
        if deletion
            .targets
            .iter()
            .any(|target| active_agent_paths.contains(&target.path))
        {
            return;
        }
        // Each worktree is hidden too, since a single-root project of its repository
        // lists it once the group it was part of is gone.
        let deleting_paths: Vec<PathBuf> = std::iter::once(deletion.path.clone())
            .chain(deletion.targets.iter().map(|target| target.path.clone()))
            .collect();
        // A deletion that shares a worktree with one in flight would unhide it early.
        if deleting_paths
            .iter()
            .any(|path| self.deleting_worktree_paths.borrow().contains(path))
        {
            return;
        }
        let deleting = DeletingWorktree {
            paths: self.deleting_worktree_paths.clone(),
            deleting: deleting_paths,
        };
        self.deleting_worktree_paths
            .borrow_mut()
            .extend(deleting.deleting.iter().cloned());
        self.update_entries(cx);
        let folder_paths = PathList::new(&deletion.roots);
        let is_loaded = multi_workspace
            .read(cx)
            .workspace_for_paths(&folder_paths, key.host(), cx)
            .is_some();
        if is_loaded {
            self.delete_loaded_worktree(deletion, key, deleting, false, window, cx);
        } else {
            // Saving the worktree's state needs it loaded in a workspace.
            self.open_workspace_for_archive(
                folder_paths,
                key.clone(),
                window,
                cx,
                move |this, _workspace, window, cx| {
                    this.delete_loaded_worktree(deletion, key, deleting, false, window, cx);
                },
                |this, cx| this.update_entries(cx),
            );
        }
    }

    /// A worktree Zed created has its state saved before it is removed, so restoring
    /// one of its archived threads brings it back. Any other worktree is removed by git,
    /// which refuses when it has changes, and forced only once the user confirms.
    fn delete_loaded_worktree(
        &mut self,
        deletion: WorktreeDeletion,
        key: ProjectGroupKey,
        deleting: DeletingWorktree,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let host = key.host().cloned();
        let targets = deletion.targets.clone();

        // Saved state is only reachable through a thread, so a worktree without one
        // takes the plain git removal, which never discards changes silently.
        let has_threads = ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .any(|thread| {
                !thread.is_draft()
                    && thread.matches_remote_connection(host.as_ref())
                    && targets
                        .iter()
                        .any(|target| thread.references_folder_path(&target.path))
            });
        let archive_workspaces = self.archive_workspaces(cx);
        let roots: Vec<thread_worktree_archive::RootPlan> = if has_threads {
            targets
                .iter()
                .filter_map(|target| {
                    thread_worktree_archive::build_root_plan(
                        &target.path,
                        host.as_ref(),
                        &archive_workspaces,
                        cx,
                    )
                })
                .collect()
        } else {
            Vec::new()
        };
        let plain_targets: Vec<WorktreeTarget> = targets
            .iter()
            .filter(|target| !roots.iter().any(|root| root.root_path == target.path))
            .cloned()
            .collect();

        // Asked before any workspace closes, so cancelling leaves everything open.
        let has_changes = archive_workspaces.iter().any(|workspace| {
            let project = workspace.read(cx).project().read(cx);
            project.repositories(cx).values().any(|repository| {
                let snapshot = repository.read(cx).snapshot();
                plain_targets.iter().any(|target| {
                    snapshot.work_directory_abs_path.as_ref() == target.path.as_path()
                }) && snapshot.status_summary().count > 0
            })
        });
        if !force && has_changes {
            let answer = window.prompt(
                gpui::PromptLevel::Warning,
                "This worktree has uncommitted changes. Delete it anyway?",
                Some("Its uncommitted changes will be lost."),
                &["Delete Anyway", "Cancel"],
                cx,
            );
            cx.spawn_in(window, async move |this, cx| {
                if answer.await == Ok(0) {
                    this.update_in(cx, |this, window, cx| {
                        this.delete_loaded_worktree(deletion, key, deleting, true, window, cx);
                    })?;
                } else {
                    drop(deleting);
                    this.update(cx, |this, cx| this.update_entries(cx))?;
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
            return;
        }

        let mut workspaces_to_remove = Vec::new();
        let close_item_tasks =
            self.close_items_for_archived_worktrees(&roots, &mut workspaces_to_remove, window, cx);
        for workspace in multi_workspace
            .read(cx)
            .workspaces_for_project_group(&key, cx)
            .filter(|workspace| {
                workspace.read(cx).root_paths(cx).iter().any(|root| {
                    plain_targets
                        .iter()
                        .any(|target| root.as_ref() == target.path.as_path())
                })
            })
        {
            if !workspaces_to_remove.contains(workspace) {
                workspaces_to_remove.push(workspace.clone());
            }
        }

        // Workspaces close first, so unsaved edits are saved or the deletion is
        // cancelled before anything is removed from disk.
        self.remove_workspaces_then(
            workspaces_to_remove,
            close_item_tasks,
            window,
            cx,
            move |_this, window, cx| {
                let mut thread_ids = Vec::new();
                let mut draft_ids = Vec::new();
                let mut terminal_ids = Vec::new();
                for target in &targets {
                    let (threads, drafts, terminals) =
                        Self::entries_in_worktree(&target.path, host.as_ref(), cx);
                    for (ids, found) in [(&mut thread_ids, threads), (&mut draft_ids, drafts)] {
                        for id in found {
                            if !ids.contains(&id) {
                                ids.push(id);
                            }
                        }
                    }
                    for id in terminals {
                        if !terminal_ids.contains(&id) {
                            terminal_ids.push(id);
                        }
                    }
                }
                // Removing a worktree strips its path from threads that are not yet archived.
                // Restoring any of them while the removal runs cancels it.
                let mut cancel_receivers = Vec::new();
                ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                    for thread_id in &thread_ids {
                        let (cancel_tx, cancel_rx) = async_channel::bounded::<()>(1);
                        cancel_receivers.push(cancel_rx);
                        store.archive(*thread_id, Some((Task::ready(()), cancel_tx)), cx);
                    }
                });
                cx.spawn_in(window, async move |this, cx| {
                    let mut removed = anyhow::Ok(true);
                    if !roots.is_empty() {
                        removed = Self::archive_worktree_roots(roots, cancel_receivers.clone(), cx)
                            .await
                            .map(|outcome| matches!(outcome, ArchiveWorktreeOutcome::Success));
                    }
                    for target in &plain_targets {
                        if !matches!(removed, Ok(true)) {
                            break;
                        }
                        removed = match thread_worktree_archive::find_or_create_repository(
                            &target.main_path,
                            host.as_ref(),
                            cx,
                        )
                        .await
                        {
                            // The project is kept alive until the repository has finished the removal.
                            Ok((repository, _project)) => {
                                let remove = async |force, cx: &mut gpui::AsyncWindowContext| {
                                    repository
                                        .update(cx, |repository, _| {
                                            repository
                                                .remove_worktree(target.path.to_path_buf(), force)
                                        })
                                        .await
                                        .map_err(|_| anyhow::anyhow!("worktree removal canceled"))
                                        .and_then(|result| result)
                                };
                                // A thread restored while the repository was loading still
                                // needs the worktree.
                                let is_cancelled =
                                    || cancel_receivers.iter().any(|receiver| receiver.is_closed());
                                if is_cancelled() {
                                    Ok(false)
                                } else if let Err(error) = remove(force, cx).await {
                                    if force {
                                        Err(error)
                                    } else {
                                        let answer = cx.prompt(
                                            gpui::PromptLevel::Warning,
                                            "Git could not remove this worktree. Delete it anyway?",
                                            Some(&format!(
                                                "{error:#}\n\nUncommitted changes will be lost."
                                            )),
                                            &["Delete Anyway", "Cancel"],
                                        );
                                        if answer.await == Ok(0) && !is_cancelled() {
                                            remove(true, cx).await.map(|()| true)
                                        } else {
                                            Ok(false)
                                        }
                                    }
                                } else {
                                    Ok(true)
                                }
                            }
                            Err(error) => Err(error),
                        };
                    }
                    if matches!(removed, Ok(true)) {
                        this.update(cx, |this, cx| {
                            ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                                for thread_id in thread_ids {
                                    store.cleanup_completed_archive(thread_id);
                                }
                                for draft_id in draft_ids {
                                    store.delete(draft_id, cx);
                                }
                            });
                            if let Some(store) = TerminalThreadMetadataStore::try_global(cx) {
                                store.update(cx, |store, cx| {
                                    for terminal_id in terminal_ids {
                                        store.delete(terminal_id, cx);
                                    }
                                });
                            }
                            this.closed_group_worktrees.remove(&key);
                            this.closed_group_worktree_tasks.remove(&key);
                            let removed_paths: Vec<PathBuf> =
                                targets.iter().map(|target| target.path.clone()).collect();
                            this.forget_worktrees(&removed_paths);
                            this.serialize(cx);
                            this.update_entries(cx);
                        })?;
                        return anyhow::Ok(());
                    }
                    drop(deleting);
                    cx.update(|_window, cx| {
                        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                            for thread_id in thread_ids {
                                store.unarchive(thread_id, cx);
                            }
                        });
                    })?;
                    this.update(cx, |this, cx| this.update_entries(cx))?;
                    match removed {
                        Ok(_) => {}
                        Err(error) => {
                            let workspace = multi_workspace.read_with(cx, |multi_workspace, _| {
                                multi_workspace.workspace().clone()
                            });
                            cx.update(|_window, cx| {
                                show_error_toast(
                                    workspace,
                                    format!("worktree remove {}", deletion.path.display()),
                                    error,
                                    cx,
                                )
                            })?;
                        }
                    }
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
            },
            |this, cx| this.update_entries(cx),
        );
    }

    fn forget_worktrees(&mut self, removed_paths: &[PathBuf]) {
        // A closed project of one of the repositories lists them too.
        for worktrees in self.closed_group_worktrees.values_mut() {
            worktrees.retain(|worktree| !removed_paths.contains(&worktree.path));
        }
        self.multi_root_worktree_groups.retain(|(_, paths)| {
            !paths
                .folder_path_list()
                .paths()
                .iter()
                .any(|path| removed_paths.contains(path))
        });
    }

    fn clean_up_unlisted_worktrees(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let mut candidates: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut add_linked_worktrees = |paths: &WorktreePaths| {
            for (main_path, folder_path) in paths.ordered_pairs() {
                let candidate = (main_path.clone(), folder_path.clone());
                if main_path != folder_path && !candidates.contains(&candidate) {
                    candidates.push(candidate);
                }
            }
        };
        for thread in ThreadMetadataStore::global(cx).read(cx).entries() {
            if !thread.archived && thread.remote_connection.is_none() {
                add_linked_worktrees(&thread.worktree_paths);
            }
        }
        if let Some(terminal_store) = TerminalThreadMetadataStore::try_global(cx) {
            for terminal in terminal_store.read(cx).entries() {
                if terminal.remote_connection.is_none() {
                    add_linked_worktrees(&terminal.worktree_paths);
                }
            }
        }
        for workspace in multi_workspace.read(cx).workspaces() {
            let project = workspace.read(cx).project().read(cx);
            if project.is_local() {
                add_linked_worktrees(&project.worktree_paths(cx));
            }
        }
        candidates
            .retain(|(_, folder_path)| !self.unlisted_worktrees_kept_open.contains(folder_path));
        if candidates.is_empty() {
            self.unlisted_worktrees_check = None;
            return;
        }

        let fs = multi_workspace
            .read(cx)
            .workspace()
            .read(cx)
            .app_state()
            .fs
            .clone();
        let listing = cx.background_spawn(async move {
            let mut listings: HashMap<PathBuf, Vec<git::repository::Worktree>> = HashMap::new();
            let mut unlisted = Vec::new();
            for (main_path, folder_path) in candidates {
                if !listings.contains_key(&main_path) {
                    // A bare repository is its own git directory.
                    let repository = fs
                        .open_repo(&main_path.join(".git"), None)
                        .or_else(|_| fs.open_repo(&main_path, None));
                    let worktrees = match repository {
                        Ok(repository) => repository.worktrees().await.log_err(),
                        Err(_) => None,
                    };
                    listings.insert(main_path.clone(), worktrees.unwrap_or_default());
                }
                let Some(worktrees) = listings.get(&main_path) else {
                    continue;
                };
                // Git lists at least the main worktree, so an empty listing is a failed one.
                if worktrees.is_empty() {
                    continue;
                }
                // Git lists resolved paths, which differ from a path that has a symlink in
                // it. A removed worktree cannot be resolved itself, so its nearest existing
                // ancestor is.
                let mut resolved_path = None;
                for ancestor in folder_path.ancestors() {
                    if let Ok(resolved_ancestor) = fs.canonicalize(ancestor).await {
                        resolved_path = folder_path
                            .strip_prefix(ancestor)
                            .ok()
                            .map(|rest| resolved_ancestor.join(rest));
                        break;
                    }
                    // A dangling symlink, such as one into an unmounted volume, hides
                    // where the path led.
                    if fs.read_link(ancestor).await.is_ok() {
                        break;
                    }
                }
                let Some(resolved_path) = resolved_path else {
                    continue;
                };
                let is_listed = worktrees
                    .iter()
                    .any(|worktree| worktree.path == folder_path || worktree.path == resolved_path);
                if !is_listed {
                    unlisted.push(folder_path);
                }
            }
            unlisted
        });
        self.unlisted_worktrees_check = Some(cx.spawn_in(window, async move |this, cx| {
            let unlisted = listing.await;
            if unlisted.is_empty() {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                this.remove_unlisted_worktrees(unlisted, window, cx);
            })
            .ok();
        }));
    }

    fn remove_unlisted_worktrees(
        &mut self,
        mut paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        // A worktree being deleted is cleaned up by its deletion, and one with a
        // running agent is left alone, as when deleting it by hand.
        let active_agent_paths = self.worktree_paths_with_active_agents(cx);
        {
            let deleting_worktree_paths = self.deleting_worktree_paths.borrow();
            paths.retain(|path| {
                !active_agent_paths.contains(path) && !deleting_worktree_paths.contains(path)
            });
        }
        if paths.is_empty() {
            return;
        }

        // A workspace or thread with another root that still exists is left alone.
        fn is_removed(folder_paths: &[PathBuf], removed_paths: &[PathBuf]) -> bool {
            !folder_paths.is_empty() && folder_paths.iter().all(|path| removed_paths.contains(path))
        }
        let workspaces_to_remove: Vec<Entity<Workspace>> = multi_workspace
            .read(cx)
            .workspaces()
            .filter(|workspace| {
                workspace.read(cx).project().read(cx).is_local()
                    && is_removed(workspace_path_list(workspace, cx).paths(), &paths)
            })
            .cloned()
            .collect();
        let workspace_paths: Vec<PathBuf> = workspaces_to_remove
            .iter()
            .flat_map(|workspace| workspace_path_list(workspace, cx).paths().to_vec())
            .collect();
        self.remove_workspaces_then(
            workspaces_to_remove,
            Vec::new(),
            window,
            cx,
            move |this, _window, cx| {
                ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                    let (draft_ids, thread_ids): (Vec<ThreadId>, Vec<ThreadId>) = store
                        .entries()
                        .filter(|thread| {
                            !thread.archived
                                && thread.remote_connection.is_none()
                                && is_removed(thread.folder_paths().paths(), &paths)
                        })
                        .partition_map(|thread| {
                            if thread.is_draft() {
                                itertools::Either::Left(thread.thread_id)
                            } else {
                                itertools::Either::Right(thread.thread_id)
                            }
                        });
                    for thread_id in thread_ids {
                        store.archive(thread_id, None, cx);
                    }
                    for draft_id in draft_ids {
                        store.delete(draft_id, cx);
                    }
                });
                if let Some(store) = TerminalThreadMetadataStore::try_global(cx) {
                    store.update(cx, |store, cx| {
                        let terminal_ids: Vec<TerminalId> = store
                            .entries()
                            .filter(|terminal| {
                                terminal.remote_connection.is_none()
                                    && is_removed(terminal.folder_paths().paths(), &paths)
                            })
                            .map(|terminal| terminal.terminal_id)
                            .collect();
                        for terminal_id in terminal_ids {
                            store.delete(terminal_id, cx);
                        }
                    });
                }
                this.forget_worktrees(&paths);
                this.serialize(cx);
                this.sync_active_entry_from_active_workspace(cx);
                this.update_entries(cx);
            },
            move |this, _cx| this.unlisted_worktrees_kept_open.extend(workspace_paths),
        );
    }

    /// The roots of every open workspace in which an agent is running, waiting or
    /// possibly still running.
    fn worktree_paths_with_active_agents(&self, cx: &App) -> HashSet<PathBuf> {
        // An agent in another window uses the same directory on disk.
        self.archive_workspaces(cx)
            .into_iter()
            .filter(|workspace| {
                all_thread_infos_for_workspace(workspace, cx).any(|info| {
                    matches!(
                        info.status,
                        AgentThreadStatus::Running
                            | AgentThreadStatus::WaitingForConfirmation
                            | AgentThreadStatus::Unconfirmed
                    )
                })
            })
            .flat_map(|workspace| workspace.read(cx).root_paths(cx))
            .map(|path| path.to_path_buf())
            .collect()
    }

    fn entries_in_worktree(
        path: &Path,
        host: Option<&RemoteConnectionOptions>,
        cx: &App,
    ) -> (Vec<ThreadId>, Vec<ThreadId>, Vec<TerminalId>) {
        let (draft_ids, thread_ids) = ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .filter(|thread| {
                !thread.archived
                    && thread.matches_remote_connection(host)
                    && thread.references_folder_path(path)
            })
            .partition_map(|thread| {
                if thread.is_draft() {
                    itertools::Either::Left(thread.thread_id)
                } else {
                    itertools::Either::Right(thread.thread_id)
                }
            });
        let terminal_ids = TerminalThreadMetadataStore::try_global(cx)
            .map(|store| {
                store
                    .read(cx)
                    .entries()
                    .filter(|terminal| {
                        terminal.remote_connection.as_ref().map(|o| o.identity())
                            == host.map(|o| o.identity())
                            && terminal
                                .folder_paths()
                                .paths()
                                .iter()
                                .any(|folder_path| folder_path.as_path() == path)
                    })
                    .map(|terminal| terminal.terminal_id)
                    .collect()
            })
            .unwrap_or_default();
        (thread_ids, draft_ids, terminal_ids)
    }

    /// A new entry must not land in a collapsed worktree group, where it would be hidden.
    fn expand_worktree_groups_for(
        &mut self,
        workspace: &Entity<Workspace>,
        cx: &mut Context<Self>,
    ) {
        let key = workspace.read(cx).project_group_key(cx);
        let roots = workspace.read(cx).root_paths(cx);
        let root_strings: Vec<String> = roots
            .iter()
            .map(|root| root.to_string_lossy().into_owned())
            .collect();
        let id = worktree_group_id_for_roots(&key, root_strings.iter().map(String::as_str));
        if self.collapsed_worktree_groups.remove(&id) {
            self.serialize(cx);
        }
    }

    fn dispatch_context(&self, window: &Window, cx: &Context<Self>) -> KeyContext {
        let mut dispatch_context = KeyContext::new_with_defaults(cx.platform_info().os);
        dispatch_context.add("ThreadsSidebar");
        dispatch_context.add("menu");

        let is_archived_search_focused = matches!(&self.view, SidebarView::Archive(archive) if archive.read(cx).is_filter_editor_focused(window, cx));

        let is_renaming = self.rename_editor.focus_handle(cx).is_focused(window);

        let identifier = if self.filter_editor.focus_handle(cx).is_focused(window)
            || is_archived_search_focused
        {
            "searching"
        } else if is_renaming {
            "editing"
        } else {
            "not_searching"
        };

        dispatch_context.add(identifier);
        dispatch_context
    }

    fn focus_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_handle.is_focused(window) {
            return;
        }

        if let SidebarView::Archive(archive) = &self.view {
            let has_selection = archive.read(cx).has_selection();
            if !has_selection {
                archive.update(cx, |view, cx| view.focus_filter_editor(window, cx));
            }
        } else if self.selection.is_none() {
            self.filter_editor.focus_handle(cx).focus(window, cx);
        }
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.rename_target.is_some() {
            self.finish_entry_rename(window, cx);
            return;
        }

        if self.filter_editor.read(cx).is_focused(window) {
            if self.reset_filter_editor_text(window, cx) {
                self.selection = None;
                self.update_entries(cx);
                return;
            }

            if self.selection.is_none() {
                self.select_first_entry();
            }
            if self.selection.is_some() {
                self.focus_handle.focus(window, cx);
                cx.notify();
            }
            return;
        }

        if self.reset_filter_editor_text(window, cx) {
            self.update_entries(cx);
        } else {
            self.selection = None;
            self.filter_editor.focus_handle(cx).focus(window, cx);
            cx.notify();
        }
    }

    fn focus_sidebar_filter(
        &mut self,
        _: &FocusSidebarFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selection = None;
        if let SidebarView::Archive(archive) = &self.view {
            archive.update(cx, |view, cx| {
                view.clear_selection();
                view.focus_filter_editor(window, cx);
            });
        } else {
            self.filter_editor.focus_handle(cx).focus(window, cx);
        }

        cx.notify();
    }

    fn reset_filter_editor_text(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.filter_editor.update(cx, |editor, cx| {
            if editor.buffer().read(cx).len(cx).0 > 0 {
                editor.set_text("", window, cx);
                true
            } else {
                false
            }
        })
    }

    fn has_filter_query(&self, cx: &App) -> bool {
        !self.filter_editor.read(cx).text(cx).is_empty()
    }

    fn start_renaming_entry(
        &mut self,
        ix: usize,
        target: RenameTarget,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.rename_target.as_ref() == Some(&target) {
            self.rename_editor.update(cx, |editor, cx| {
                editor.select_all(&editor::actions::SelectAll, window, cx);
                editor.focus_handle(cx).focus(window, cx);
            });
            return;
        }
        if self.rename_target.is_some() {
            self.finish_entry_rename(window, cx);
        }

        self.selection = Some(ix);
        self.rename_target = Some(target);
        self.suppress_next_rename_edit = true;
        self.list_state.scroll_to_reveal_item(ix);
        self.rename_editor.update(cx, |editor, cx| {
            editor.set_text(title, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }

    fn handle_rename_editor_event(
        &mut self,
        title_editor: &Entity<Editor>,
        event: &editor::EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            editor::EditorEvent::BufferEdited => {
                if self.suppress_next_rename_edit {
                    self.suppress_next_rename_edit = false;
                    return;
                }
                if !title_editor.read(cx).is_focused(window) {
                    return;
                }
                let new_title = title_editor.read(cx).text(cx);
                let Some(target) = self.rename_target.clone() else {
                    return;
                };
                if matches!(target, RenameTarget::Thread(_)) && new_title.is_empty() {
                    return;
                }
                let new_title = SharedString::from(new_title);
                match target {
                    RenameTarget::Thread(thread_id) => {
                        self.apply_thread_rename(thread_id, new_title, window, cx);
                    }
                    RenameTarget::Terminal(terminal_id) => {
                        self.apply_terminal_rename(terminal_id, new_title, cx);
                    }
                    // Applied once in `finish_entry_rename` so Escape can discard it.
                    RenameTarget::Project(_) => {}
                }
            }
            editor::EditorEvent::Blurred => {
                self.finish_entry_rename(window, cx);
            }
            _ => {}
        }
    }

    fn apply_thread_rename(
        &mut self,
        thread_id: ThreadId,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut found = false;
        if let Some(multi_workspace) = self.multi_workspace.upgrade() {
            let workspaces: Vec<_> = multi_workspace.read(cx).workspaces().cloned().collect();
            for workspace in workspaces {
                if let Some(agent_panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
                    if let Some(view) = agent_panel
                        .read(cx)
                        .conversation_view_for_id(&thread_id, cx)
                        && let Some(thread_view) = view.read(cx).root_thread_view()
                    {
                        thread_view.update(cx, |thread_view, cx| {
                            thread_view.rename(title.clone(), window, cx);
                        });
                        found = true;
                    }
                }
            }
        }

        if !found {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.set_title_override(thread_id, title, cx);
            });
        }
    }

    fn apply_terminal_rename(
        &mut self,
        terminal_id: TerminalId,
        title: SharedString,
        cx: &mut Context<Self>,
    ) {
        let mut found = false;
        if let Some(multi_workspace) = self.multi_workspace.upgrade() {
            let workspaces: Vec<_> = multi_workspace.read(cx).workspaces().cloned().collect();
            for workspace in workspaces {
                if let Some(agent_panel) = workspace.read(cx).panel::<AgentPanel>(cx)
                    && agent_panel.update(cx, |agent_panel, cx| {
                        agent_panel.rename_terminal(terminal_id, title.clone(), cx)
                    })
                {
                    found = true;
                }
            }
        }

        if !found {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.rename_terminal(terminal_id, title, cx);
            });
        }
    }

    fn start_renaming_project(
        &mut self,
        key: &ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let header = self
            .contents
            .entries
            .iter()
            .enumerate()
            .find_map(|(ix, entry)| match entry {
                ListEntry::ProjectHeader {
                    key: entry_key,
                    label,
                    ..
                } if entry_key == key => Some((ix, label.clone())),
                _ => None,
            });
        if let Some((ix, label)) = header {
            self.start_renaming_entry(ix, RenameTarget::Project(key.clone()), label, window, cx);
        }
    }

    fn apply_project_rename(
        &mut self,
        key: &ProjectGroupKey,
        name: SharedString,
        cx: &mut Context<Self>,
    ) {
        let name = name.trim();
        // Confirming the generated name unchanged must not pin it as a custom one.
        let is_generated_name = self.contents.entries.iter().any(|entry| {
            matches!(entry, ListEntry::ProjectHeader { key: entry_key, label, .. }
                if entry_key == key && label.as_ref() == name)
        });
        let has_custom_name = self
            .multi_workspace
            .upgrade()
            .is_some_and(|multi_workspace| {
                multi_workspace
                    .read(cx)
                    .group_state_by_key(key)
                    .is_some_and(|state| state.custom_name.is_some())
            });
        if is_generated_name && !has_custom_name {
            return;
        }
        let custom_name = (!name.is_empty()).then(|| SharedString::from(name.to_string()));
        if let Some(multi_workspace) = self.multi_workspace.upgrade() {
            multi_workspace.update(cx, |multi_workspace, cx| {
                if let Some(state) = multi_workspace.group_state_by_key_mut(key) {
                    state.custom_name = custom_name;
                }
                multi_workspace.serialize(cx);
            });
        }
    }

    fn finish_entry_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(target) = self.rename_target.take() else {
            return false;
        };
        if let RenameTarget::Project(key) = target {
            let name = self.rename_editor.read(cx).text(cx);
            self.apply_project_rename(&key, name.into(), cx);
        }
        self.focus_handle.focus(window, cx);
        self.update_entries(cx);
        true
    }

    fn editor_move_down(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        self.select_next(&SelectNext, window, cx);
        if self.selection.is_some() {
            self.focus_handle.focus(window, cx);
        }
    }

    fn editor_move_up(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        self.select_previous(&SelectPrevious, window, cx);
        if self.selection.is_some() {
            self.focus_handle.focus(window, cx);
        }
    }

    fn editor_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_none() {
            self.select_next(&SelectNext, window, cx);
        }
        if self.selection.is_some() {
            self.focus_handle.focus(window, cx);
        }
    }

    fn select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        let next = match self.selection {
            Some(ix) if ix + 1 < self.contents.entries.len() => ix + 1,
            Some(_) if !self.contents.entries.is_empty() => 0,
            None if !self.contents.entries.is_empty() => 0,
            _ => return,
        };
        self.selection = Some(next);
        self.list_state.scroll_to_reveal_item(next);
        cx.notify();
    }

    fn select_previous(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        match self.selection {
            Some(0) => {
                self.selection = None;
                self.filter_editor.focus_handle(cx).focus(window, cx);
                cx.notify();
            }
            Some(ix) => {
                self.selection = Some(ix - 1);
                self.list_state.scroll_to_reveal_item(ix - 1);
                cx.notify();
            }
            None if !self.contents.entries.is_empty() => {
                let last = self.contents.entries.len() - 1;
                self.selection = Some(last);
                self.list_state.scroll_to_reveal_item(last);
                cx.notify();
            }
            None => {}
        }
    }

    fn select_first(&mut self, _: &SelectFirst, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.contents.entries.is_empty() {
            self.selection = Some(0);
            self.list_state.scroll_to_reveal_item(0);
            cx.notify();
        }
    }

    fn select_last(&mut self, _: &SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(last) = self.contents.entries.len().checked_sub(1) {
            self.selection = Some(last);
            self.list_state.scroll_to_reveal_item(last);
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.finish_entry_rename(window, cx) {
            return;
        }

        let Some(ix) = self.selection else { return };
        let Some(entry) = self.contents.entries.get(ix) else {
            return;
        };

        match entry {
            ListEntry::ProjectHeader { key, .. } => {
                let key = key.clone();
                self.toggle_collapse(&key, window, cx);
            }
            ListEntry::Thread(thread) => {
                let metadata = thread.metadata.clone();
                match &thread.workspace {
                    ThreadEntryWorkspace::Open(workspace) => {
                        let workspace = workspace.clone();
                        self.activate_thread(metadata, &workspace, false, window, cx);
                    }
                    ThreadEntryWorkspace::Closed {
                        folder_paths,
                        project_group_key,
                    } => {
                        let folder_paths = folder_paths.clone();
                        let project_group_key = project_group_key.clone();
                        self.open_workspace_and_activate_thread(
                            metadata,
                            folder_paths,
                            &project_group_key,
                            window,
                            cx,
                        );
                    }
                }
            }
            ListEntry::Terminal(terminal) => {
                let metadata = terminal.metadata.clone();
                let workspace = terminal.workspace.clone();
                self.activate_terminal_entry(metadata, workspace, false, window, cx);
            }
            ListEntry::WorktreeHeader { id, .. } => {
                let id = id.clone();
                self.toggle_worktree_group(&id, cx);
            }
        }
    }

    fn load_agent_thread_in_workspace(
        workspace: &Entity<Workspace>,
        metadata: &ThreadMetadata,
        focus: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let load_thread = |agent_panel: Entity<AgentPanel>,
                           metadata: &ThreadMetadata,
                           focus: bool,
                           window: &mut Window,
                           cx: &mut App| {
            agent_panel.update(cx, |panel, cx| {
                panel.load_agent_thread(
                    Agent::from(metadata.agent_id.clone()),
                    metadata.thread_id,
                    Some(metadata.folder_paths().clone()),
                    metadata.title.clone(),
                    focus,
                    AgentThreadSource::Sidebar,
                    window,
                    cx,
                );
            });
        };

        let mut existing_panel = None;
        workspace.update(cx, |workspace, cx| {
            if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                existing_panel = Some(panel);
            }
        });

        if let Some(agent_panel) = existing_panel {
            load_thread(agent_panel, metadata, focus, window, cx);
            workspace.update(cx, |workspace, cx| {
                if focus {
                    workspace.focus_panel::<AgentPanel>(window, cx);
                } else {
                    workspace.reveal_panel::<AgentPanel>(window, cx);
                }
            });
            return;
        }

        let workspace = workspace.downgrade();
        let metadata = metadata.clone();
        let mut async_window_cx = window.to_async(cx);
        cx.spawn(async move |_cx| {
            let panel = AgentPanel::load(workspace.clone(), async_window_cx.clone()).await?;

            workspace.update_in(&mut async_window_cx, |workspace, window, cx| {
                let panel = workspace.panel::<AgentPanel>(cx).unwrap_or_else(|| {
                    workspace.add_panel(panel.clone(), window, cx);
                    panel.clone()
                });
                load_thread(panel, &metadata, focus, window, cx);
                if focus {
                    workspace.focus_panel::<AgentPanel>(window, cx);
                } else {
                    workspace.reveal_panel::<AgentPanel>(window, cx);
                }
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn open_closed_native_thread_as_markdown(
        session_id: &acp::SessionId,
        title: Option<SharedString>,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let thread_store = ThreadStore::global(cx);
        let load_task =
            thread_store.update(cx, |store, cx| store.load_thread(session_id.clone(), cx));

        let thread_title = title
            .map(|t| t.to_string())
            .unwrap_or_else(|| DEFAULT_THREAD_TITLE.to_string());

        let workspace = workspace.clone();

        window
            .spawn(cx, async move |cx| {
                let db_thread = load_task.await?;
                let Some(db_thread) = db_thread else {
                    anyhow::bail!("Thread not found in database");
                };

                let markdown = db_thread.to_markdown();

                cx.update(|window, cx| {
                    agent_ui::open_markdown_in_workspace(
                        thread_title,
                        markdown,
                        workspace,
                        window,
                        cx,
                    )
                })?
                .await
            })
            .detach_and_log_err(cx);
    }

    fn show_warning_toast(workspace: Entity<Workspace>, message: &'static str, cx: &mut App) {
        workspace.update(cx, |workspace, cx| {
            let toast = StatusToast::new(message, cx, |this, _cx| {
                this.icon(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Warning),
                )
                .dismiss_button(true)
            });
            workspace.toggle_status_toast(toast, cx);
        });
    }

    fn show_no_thread_summary_model_toast(workspace: Entity<Workspace>, cx: &mut App) {
        Self::show_warning_toast(
            workspace,
            "No model is configured for summarizing thread titles.",
            cx,
        );
    }

    fn regenerate_thread_title(
        &mut self,
        session_id: &acp::SessionId,
        thread_id: ThreadId,
        folder_paths: PathList,
        thread_workspace: Option<Entity<Workspace>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = thread_workspace
            .as_ref()
            .and_then(|w| w.read(cx).panel::<AgentPanel>(cx))
        {
            match panel.update(cx, |panel, cx| panel.regenerate_thread_title(thread_id, cx)) {
                ThreadTitleRegenerationResult::Started
                | ThreadTitleRegenerationResult::AlreadyGenerating => return,
                ThreadTitleRegenerationResult::NoModel => {
                    if let Some(workspace) = self.active_workspace(cx) {
                        Self::show_no_thread_summary_model_toast(workspace, cx);
                    }
                    return;
                }
                ThreadTitleRegenerationResult::NotOpen => {}
            }
        }

        let Some(model) = LanguageModelRegistry::read_global(cx).thread_summary_model(cx) else {
            if let Some(workspace) = self.active_workspace(cx) {
                Self::show_no_thread_summary_model_toast(workspace, cx);
            }
            return;
        };

        if !self.regenerating_titles.insert(thread_id) {
            return;
        }

        let temperature = AgentSettings::temperature_for_model(&model, cx);

        let thread_store = ThreadStore::global(cx);
        let load_task =
            thread_store.update(cx, |store, cx| store.load_thread(session_id.clone(), cx));
        let session_id = session_id.clone();

        cx.notify();

        cx.spawn(async move |this, cx| {
            let result: anyhow::Result<SharedString> = async {
                let Some(db_thread) = load_task.await? else {
                    anyhow::bail!("Thread not found in database");
                };

                let request = agent::build_thread_title_request(
                    &session_id,
                    &db_thread.messages,
                    temperature,
                );
                let title =
                    SharedString::from(agent::stream_thread_title(model, request, cx).await?);

                let Some(mut db_thread) = thread_store
                    .update(cx, |store, cx| store.load_thread(session_id.clone(), cx))
                    .await?
                else {
                    anyhow::bail!("Thread not found in database");
                };
                db_thread.title = title.clone();

                thread_store
                    .update(cx, |store, cx| {
                        store.save_thread(session_id, db_thread, folder_paths, cx)
                    })
                    .await?;

                anyhow::Ok(title)
            }
            .await;

            this.update(cx, |this, cx| {
                this.regenerating_titles.remove(&thread_id);
                match &result {
                    Ok(title) => {
                        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                            store.set_generated_title(thread_id, title.clone(), cx);
                        });
                    }
                    Err(_) => {
                        if let Some(workspace) = this.active_workspace(cx) {
                            Self::show_warning_toast(
                                workspace,
                                "Failed to regenerate thread title.",
                                cx,
                            );
                        }
                    }
                }
                cx.notify();
            })
            .ok();

            result.map(|_| ())
        })
        .detach_and_log_err(cx);
    }

    fn is_thread_active_in_workspace(
        &self,
        thread_id: &ThreadId,
        workspace: &Entity<Workspace>,
        cx: &App,
    ) -> bool {
        self.active_workspace(cx).as_ref() == Some(workspace)
            && self.active_entry.as_ref().is_some_and(|entry| {
                entry.is_active_thread(thread_id) && entry.workspace() == workspace
            })
    }

    fn activate_thread_locally(
        &mut self,
        metadata: &ThreadMetadata,
        workspace: &Entity<Workspace>,
        retain: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        if self.is_thread_active_in_workspace(&metadata.thread_id, workspace, cx) {
            workspace.update(cx, |workspace, cx| {
                workspace.focus_panel::<AgentPanel>(window, cx);
            });
            return;
        }

        // Set active_entry eagerly so the sidebar highlight updates
        // immediately, rather than waiting for a deferred AgentPanel
        // event which can race with ActiveWorkspaceChanged clearing it.
        self.active_entry = Some(ActiveEntry::Thread {
            thread_id: metadata.thread_id,
            session_id: metadata.session_id.clone(),
            workspace: workspace.clone(),
        });
        self.record_thread_access(&metadata.thread_id);
        self.pending_thread_activation = Some(metadata.thread_id);

        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.activate(workspace.clone(), None, window, cx);
            if retain {
                multi_workspace.retain_active_workspace(cx);
            }
        });

        Self::load_agent_thread_in_workspace(workspace, metadata, true, window, cx);

        self.update_entries(cx);
    }

    fn activate_thread_in_other_window(
        &self,
        metadata: ThreadMetadata,
        workspace: Entity<Workspace>,
        target_window: WindowHandle<MultiWorkspace>,
        cx: &mut Context<Self>,
    ) {
        let target_session_id = metadata.session_id.clone();
        let metadata_thread_id = metadata.thread_id;

        let activated = target_window
            .update(cx, |multi_workspace, window, cx| {
                window.activate_window();
                multi_workspace.activate(workspace.clone(), None, window, cx);
                Self::load_agent_thread_in_workspace(&workspace, &metadata, true, window, cx);
            })
            .log_err()
            .is_some();

        if activated {
            if let Some(target_sidebar) = target_window
                .read(cx)
                .ok()
                .and_then(|multi_workspace| {
                    multi_workspace.sidebar().map(|sidebar| sidebar.to_any())
                })
                .and_then(|sidebar| sidebar.downcast::<Self>().ok())
            {
                target_sidebar.update(cx, |sidebar, cx| {
                    sidebar.pending_thread_activation = Some(metadata_thread_id);
                    sidebar.active_entry = Some(ActiveEntry::Thread {
                        thread_id: metadata_thread_id,
                        session_id: target_session_id,
                        workspace,
                    });
                    sidebar.record_thread_access(&metadata_thread_id);
                    sidebar.update_entries(cx);
                });
            }
        }
    }

    fn activate_thread(
        &mut self,
        metadata: ThreadMetadata,
        workspace: &Entity<Workspace>,
        retain: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .multi_workspace
            .upgrade()
            .is_some_and(|multi_workspace| {
                multi_workspace
                    .read(cx)
                    .workspaces()
                    .any(|candidate| candidate == workspace)
            })
        {
            self.activate_thread_locally(&metadata, &workspace, retain, window, cx);
            return;
        }

        let Some(target_window) = cx
            .windows()
            .into_iter()
            .filter_map(|window| window.downcast::<MultiWorkspace>())
            .find(|window| {
                window.read(cx).is_ok_and(|multi_workspace| {
                    multi_workspace
                        .workspaces()
                        .any(|candidate| candidate == workspace)
                })
            })
        else {
            return;
        };

        self.activate_thread_in_other_window(metadata, workspace.clone(), target_window, cx);
    }

    fn open_workspace_and_activate_thread(
        &mut self,
        metadata: ThreadMetadata,
        folder_paths: PathList,
        project_group_key: &ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let pending_thread_id = metadata.thread_id;
        // Mark the pending thread activation so rebuild_contents
        // preserves the Thread active_entry during loading and
        // reconciliation cannot synthesize an empty fallback draft.
        self.pending_thread_activation = Some(pending_thread_id);

        let host = project_group_key.host().cloned();
        let provisional_key = Some(project_group_key.clone());
        let active_workspace = multi_workspace.read(cx).workspace().clone();
        let modal_workspace = active_workspace.clone();

        let open_task = multi_workspace.update(cx, |this, cx| {
            this.find_or_create_workspace(
                folder_paths,
                host,
                provisional_key,
                |options, window, cx| connect_remote(active_workspace, options, window, cx),
                None,
                OpenMode::Activate,
                None,
                window,
                cx,
            )
        });

        cx.spawn_in(window, async move |this, cx| {
            let result = open_task.await;
            // Dismiss the modal as soon as the open attempt completes so
            // failures or cancellations do not leave a stale connection modal behind.
            remote_connection::dismiss_connection_modal(&modal_workspace, cx);

            if result.is_err() {
                this.update(cx, |this, _cx| {
                    if this.pending_thread_activation == Some(pending_thread_id) {
                        this.pending_thread_activation = None;
                    }
                })
                .ok();
            }

            let workspace = result?;
            this.update_in(cx, |this, window, cx| {
                this.activate_thread(metadata, &workspace, false, window, cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn find_open_workspace_for_thread(
        &self,
        thread: &ThreadMetadata,
        cx: &App,
    ) -> Option<(WindowHandle<MultiWorkspace>, Entity<Workspace>)> {
        cx.windows()
            .into_iter()
            .filter_map(|window| window.downcast::<MultiWorkspace>())
            .find_map(|window| {
                let workspace = find_workspace_for_thread(window.read(cx).ok()?, thread, cx)?;

                Some((window, workspace))
            })
    }

    fn open_thread_from_archive(
        &mut self,
        metadata: ThreadMetadata,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread_id = metadata.thread_id;
        // Its worktree may be gone, and the cleanup would archive the thread again.
        self.unlisted_worktrees_kept_open
            .extend(metadata.folder_paths().paths().iter().cloned());
        let weak_archive_view = match &self.view {
            SidebarView::Archive(view) => Some(view.downgrade()),
            _ => None,
        };

        if metadata.folder_paths().paths().is_empty() {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| store.unarchive(thread_id, cx));

            let active_workspace = self
                .multi_workspace
                .upgrade()
                .map(|w| w.read(cx).workspace().clone());

            if let Some(workspace) = active_workspace {
                self.activate_thread_locally(&metadata, &workspace, false, window, cx);
            } else {
                let path_list = metadata.folder_paths().clone();
                if let Some((target_window, workspace)) =
                    self.find_open_workspace_for_thread(&metadata, cx)
                {
                    self.activate_thread_in_other_window(metadata, workspace, target_window, cx);
                } else {
                    let key = ProjectGroupKey::from_worktree_paths(
                        &metadata.worktree_paths,
                        metadata.remote_connection.clone(),
                    );
                    self.open_workspace_and_activate_thread(metadata, path_list, &key, window, cx);
                }
            }
            self.show_thread_list(window, cx);
            return;
        }

        let store = ThreadMetadataStore::global(cx);
        let task = if metadata.archived {
            store
                .read(cx)
                .get_archived_worktrees_for_thread(thread_id, cx)
        } else {
            Task::ready(Ok(Vec::new()))
        };
        let path_list = metadata.folder_paths().clone();

        let restore_task = cx.spawn_in(window, async move |this, cx| {
            let result: anyhow::Result<()> = async {
                let archived_worktrees = task.await?;

                if archived_worktrees.is_empty() {
                    this.update_in(cx, |this, window, cx| {
                        this.restoring_tasks.remove(&thread_id);
                        if metadata.archived {
                            ThreadMetadataStore::global(cx)
                                .update(cx, |store, cx| store.unarchive(thread_id, cx));
                        }

                        let current_workspace =
                            this.multi_workspace.upgrade().and_then(|multi_workspace| {
                                find_workspace_for_thread(multi_workspace.read(cx), &metadata, cx)
                            });

                        if let Some(workspace) = current_workspace {
                            this.activate_thread_locally(&metadata, &workspace, false, window, cx);
                        } else if let Some((target_window, workspace)) =
                            this.find_open_workspace_for_thread(&metadata, cx)
                        {
                            this.activate_thread_in_other_window(
                                metadata,
                                workspace,
                                target_window,
                                cx,
                            );
                        } else {
                            let key = ProjectGroupKey::from_worktree_paths(
                                &metadata.worktree_paths,
                                metadata.remote_connection.clone(),
                            );
                            this.open_workspace_and_activate_thread(
                                metadata, path_list, &key, window, cx,
                            );
                        }
                        this.show_thread_list(window, cx);
                    })?;
                    return anyhow::Ok(());
                }

                let mut path_replacements: Vec<(PathBuf, PathBuf)> = Vec::new();
                for row in &archived_worktrees {
                    match thread_worktree_archive::restore_worktree_via_git(
                        row,
                        metadata.remote_connection.as_ref(),
                        &mut *cx,
                    )
                    .await
                    {
                        Ok(restored_path) => {
                            thread_worktree_archive::cleanup_archived_worktree_record(
                                row,
                                metadata.remote_connection.as_ref(),
                                &mut *cx,
                            )
                            .await;
                            path_replacements.push((row.worktree_path.clone(), restored_path));
                        }
                        Err(error) => {
                            log::error!("Failed to restore worktree: {error:#}");
                            this.update_in(cx, |this, _window, cx| {
                                this.restoring_tasks.remove(&thread_id);
                                if let Some(weak_archive_view) = &weak_archive_view {
                                    weak_archive_view
                                        .update(cx, |view, cx| {
                                            view.clear_restoring(&thread_id, cx);
                                        })
                                        .ok();
                                }

                                if let Some(multi_workspace) = this.multi_workspace.upgrade() {
                                    let workspace = multi_workspace.read(cx).workspace().clone();
                                    workspace.update(cx, |workspace, cx| {
                                        struct RestoreWorktreeErrorToast;
                                        workspace.show_toast(
                                            Toast::new(
                                                NotificationId::unique::<RestoreWorktreeErrorToast>(
                                                ),
                                                format!("Failed to restore worktree: {error:#}"),
                                            )
                                            .autohide(),
                                            cx,
                                        );
                                    });
                                }
                            })
                            .ok();
                            return anyhow::Ok(());
                        }
                    }
                }

                if !path_replacements.is_empty() {
                    cx.update(|_window, cx| {
                        store.update(cx, |store, cx| {
                            store.update_restored_worktree_paths(thread_id, &path_replacements, cx);
                        });
                    })?;

                    let updated_metadata =
                        cx.update(|_window, cx| store.read(cx).entry(thread_id).cloned())?;

                    if let Some(updated_metadata) = updated_metadata {
                        let new_paths = updated_metadata.folder_paths().clone();
                        let key = ProjectGroupKey::from_worktree_paths(
                            &updated_metadata.worktree_paths,
                            updated_metadata.remote_connection.clone(),
                        );

                        cx.update(|_window, cx| {
                            store.update(cx, |store, cx| {
                                store.unarchive(updated_metadata.thread_id, cx);
                            });
                        })?;

                        this.update_in(cx, |this, window, cx| {
                            this.restoring_tasks.remove(&thread_id);
                            this.open_workspace_and_activate_thread(
                                updated_metadata,
                                new_paths,
                                &key,
                                window,
                                cx,
                            );
                            this.show_thread_list(window, cx);
                        })?;
                    }
                }

                anyhow::Ok(())
            }
            .await;
            if let Err(error) = result {
                log::error!("{error:#}");
            }
        });
        self.restoring_tasks.insert(thread_id, restore_task);
    }

    fn expand_selected_entry(
        &mut self,
        _: &SelectChild,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.selection else { return };

        match self.contents.entries.get(ix) {
            Some(ListEntry::ProjectHeader { key, .. }) => {
                let key = key.clone();
                if self.is_group_collapsed(&key, cx) {
                    self.set_group_expanded(&key, true, cx);
                    self.update_entries(cx);
                } else if ix + 1 < self.contents.entries.len() {
                    self.selection = Some(ix + 1);
                    self.list_state.scroll_to_reveal_item(ix + 1);
                    cx.notify();
                }
            }
            Some(ListEntry::WorktreeHeader {
                id, is_collapsed, ..
            }) => {
                if *is_collapsed {
                    let id = id.clone();
                    self.toggle_worktree_group(&id, cx);
                } else if ix + 1 < self.contents.entries.len() {
                    self.selection = Some(ix + 1);
                    self.list_state.scroll_to_reveal_item(ix + 1);
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    fn collapse_selected_entry(
        &mut self,
        _: &SelectParent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.selection else { return };

        match self.contents.entries.get(ix) {
            Some(ListEntry::ProjectHeader { key, .. }) => {
                let key = key.clone();
                if !self.is_group_collapsed(&key, cx) {
                    self.set_group_expanded(&key, false, cx);
                    self.update_entries(cx);
                }
            }
            Some(ListEntry::WorktreeHeader {
                id,
                is_collapsed: false,
                ..
            }) => {
                let id = id.clone();
                self.toggle_worktree_group(&id, cx);
            }
            Some(
                ListEntry::Thread(_) | ListEntry::Terminal(_) | ListEntry::WorktreeHeader { .. },
            ) => {
                for i in (0..ix).rev() {
                    if let Some(ListEntry::ProjectHeader { key, .. }) = self.contents.entries.get(i)
                    {
                        let key = key.clone();
                        self.selection = Some(i);
                        self.set_group_expanded(&key, false, cx);
                        self.update_entries(cx);
                        break;
                    }
                }
            }
            None => {}
        }
    }

    fn toggle_selected_fold(
        &mut self,
        _: &editor::actions::ToggleFold,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.selection else { return };

        if let Some(ListEntry::WorktreeHeader { id, .. }) = self.contents.entries.get(ix) {
            let id = id.clone();
            self.toggle_worktree_group(&id, cx);
            return;
        }

        // Find the group header for the current selection.
        let header_ix = match self.contents.entries.get(ix) {
            Some(ListEntry::ProjectHeader { .. }) => Some(ix),
            Some(
                ListEntry::Thread(_) | ListEntry::Terminal(_) | ListEntry::WorktreeHeader { .. },
            ) => (0..ix).rev().find(|&i| {
                matches!(
                    self.contents.entries.get(i),
                    Some(ListEntry::ProjectHeader { .. })
                )
            }),
            None => None,
        };

        if let Some(header_ix) = header_ix {
            if let Some(ListEntry::ProjectHeader { key, .. }) = self.contents.entries.get(header_ix)
            {
                let key = key.clone();
                if self.is_group_collapsed(&key, cx) {
                    self.set_group_expanded(&key, true, cx);
                } else {
                    self.selection = Some(header_ix);
                    self.set_group_expanded(&key, false, cx);
                }
                self.update_entries(cx);
            }
        }
    }

    fn fold_all(
        &mut self,
        _: &editor::actions::FoldAll,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(mw) = self.multi_workspace.upgrade() {
            mw.update(cx, |mw, _cx| {
                mw.set_all_groups_expanded(false);
            });
        }
        self.update_entries(cx);
    }

    fn unfold_all(
        &mut self,
        _: &editor::actions::UnfoldAll,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(mw) = self.multi_workspace.upgrade() {
            mw.update(cx, |mw, _cx| {
                mw.set_all_groups_expanded(true);
            });
        }
        self.collapsed_worktree_groups.clear();
        self.serialize(cx);
        self.update_entries(cx);
    }

    fn stop_thread(&mut self, thread_id: &agent_ui::ThreadId, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let workspaces: Vec<_> = multi_workspace.read(cx).workspaces().cloned().collect();
        for workspace in workspaces {
            if let Some(agent_panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
                let cancelled =
                    agent_panel.update(cx, |panel, cx| panel.cancel_thread(thread_id, cx));
                if cancelled {
                    return;
                }
            }
        }
    }

    /// Find the entry to open after the entry at `current_position` is removed: the
    /// top one in its worktree group, else in its project, else in the whole list.
    fn top_entry_after_removal(&self, current_position: usize) -> Option<ListEntry> {
        let entries = &self.contents.entries;
        let section = |is_boundary: fn(&ListEntry) -> bool| {
            let start = entries
                .get(..current_position)?
                .iter()
                .rposition(is_boundary)
                .map_or(0, |header| header + 1);
            let end = entries
                .get(current_position + 1..)?
                .iter()
                .position(is_boundary)
                .map_or(entries.len(), |offset| current_position + 1 + offset);
            Some(start..end)
        };
        let worktree_group = section(|entry| {
            matches!(
                entry,
                ListEntry::ProjectHeader { .. } | ListEntry::WorktreeHeader { .. }
            )
        });
        let project = section(|entry| matches!(entry, ListEntry::ProjectHeader { .. }));

        [worktree_group, project, Some(0..entries.len())]
            .into_iter()
            .flatten()
            .find_map(|section| {
                section
                    .filter(|position| *position != current_position)
                    .filter_map(|position| entries.get(position))
                    .find(|entry| matches!(entry, ListEntry::Thread(_) | ListEntry::Terminal(_)))
            })
            .cloned()
    }

    fn activate_entry(
        &mut self,
        entry: &ActivatableEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match entry {
            ActivatableEntry::Thread { metadata, .. } => {
                let Some(workspace) = self.multi_workspace.upgrade().and_then(|multi_workspace| {
                    multi_workspace.read(cx).workspace_for_paths(
                        metadata.folder_paths(),
                        metadata.remote_connection.as_ref(),
                        cx,
                    )
                }) else {
                    return false;
                };

                self.active_entry = Some(ActiveEntry::Thread {
                    thread_id: metadata.thread_id,
                    session_id: metadata.session_id.clone(),
                    workspace: workspace.clone(),
                });
                self.activate_workspace(&workspace, window, cx);
                Self::load_agent_thread_in_workspace(&workspace, metadata, true, window, cx);
                true
            }
            ActivatableEntry::Terminal {
                metadata,
                workspace,
            } => {
                self.activate_terminal_entry(
                    metadata.clone(),
                    workspace.clone(),
                    false,
                    window,
                    cx,
                );
                true
            }
        }
    }

    fn activate_terminal_entry(
        &mut self,
        metadata: TerminalThreadMetadata,
        workspace: ThreadEntryWorkspace,
        retain: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match workspace {
            ThreadEntryWorkspace::Open(workspace) => {
                self.activate_terminal_in_workspace(&workspace, metadata, retain, window, cx);
            }
            ThreadEntryWorkspace::Closed {
                folder_paths,
                project_group_key,
            } => {
                self.open_workspace_and_activate_terminal(
                    metadata,
                    folder_paths,
                    &project_group_key,
                    window,
                    cx,
                );
            }
        }
    }

    fn load_agent_terminal_in_workspace(
        workspace: &Entity<Workspace>,
        metadata: &TerminalThreadMetadata,
        focus: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let restore_terminal = |agent_panel: Entity<AgentPanel>,
                                metadata: &TerminalThreadMetadata,
                                focus: bool,
                                workspace: Option<&Workspace>,
                                window: &mut Window,
                                cx: &mut App| {
            agent_panel.update(cx, |panel, cx| {
                panel.restore_terminal(
                    metadata.clone(),
                    focus,
                    AgentThreadSource::Sidebar,
                    workspace,
                    window,
                    cx,
                );
            });
        };

        let mut existing_panel = None;
        workspace.update(cx, |workspace, cx| {
            if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                existing_panel = Some(panel);
            }
        });

        if let Some(agent_panel) = existing_panel {
            restore_terminal(agent_panel, metadata, focus, None, window, cx);
            workspace.update(cx, |workspace, cx| {
                if focus {
                    workspace.focus_panel::<AgentPanel>(window, cx);
                } else {
                    workspace.reveal_panel::<AgentPanel>(window, cx);
                }
            });
            return;
        }

        let workspace = workspace.downgrade();
        let metadata = metadata.clone();
        let mut async_window_cx = window.to_async(cx);
        cx.spawn(async move |_cx| {
            let panel = AgentPanel::load(workspace.clone(), async_window_cx.clone()).await?;

            workspace.update_in(&mut async_window_cx, |workspace, window, cx| {
                let panel = workspace.panel::<AgentPanel>(cx).unwrap_or_else(|| {
                    workspace.add_panel(panel.clone(), window, cx);
                    panel.clone()
                });
                restore_terminal(panel, &metadata, focus, Some(workspace), window, cx);
                if focus {
                    workspace.focus_panel::<AgentPanel>(window, cx);
                } else {
                    workspace.reveal_panel::<AgentPanel>(window, cx);
                }
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn activate_terminal_in_workspace(
        &mut self,
        workspace: &Entity<Workspace>,
        metadata: TerminalThreadMetadata,
        retain: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let terminal_id = metadata.terminal_id;
        self.record_terminal_access(terminal_id);
        self.active_entry = Some(ActiveEntry::Terminal {
            terminal_id,
            workspace: workspace.clone(),
        });

        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.activate(workspace.clone(), None, window, cx);
            if retain {
                multi_workspace.retain_active_workspace(cx);
            }
        });

        Self::load_agent_terminal_in_workspace(workspace, &metadata, true, window, cx);

        self.update_entries(cx);
    }

    fn open_workspace_and_activate_terminal(
        &mut self,
        metadata: TerminalThreadMetadata,
        folder_paths: PathList,
        project_group_key: &ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let host = project_group_key.host().cloned();
        let provisional_key = Some(project_group_key.clone());
        let active_workspace = multi_workspace.read(cx).workspace().clone();
        let modal_workspace = active_workspace.clone();

        let open_task = multi_workspace.update(cx, |this, cx| {
            this.find_or_create_workspace(
                folder_paths,
                host,
                provisional_key,
                |options, window, cx| connect_remote(active_workspace, options, window, cx),
                None,
                OpenMode::Activate,
                None,
                window,
                cx,
            )
        });

        cx.spawn_in(window, async move |this, cx| {
            let result = open_task.await;
            remote_connection::dismiss_connection_modal(&modal_workspace, cx);
            let workspace = result?;
            this.update_in(cx, |this, window, cx| {
                this.activate_terminal_in_workspace(&workspace, metadata, false, window, cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn archive_workspaces(&self, cx: &App) -> Vec<Entity<Workspace>> {
        let multi_workspace = self.multi_workspace.upgrade();
        thread_worktree_archive::workspaces_for_archive(multi_workspace.as_ref(), cx)
    }

    async fn wait_for_archive_workspace_metadata(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::AsyncApp,
    ) {
        let scans_complete =
            workspace.read_with(cx, |workspace, cx| workspace.worktree_scans_complete(cx));
        scans_complete.await;

        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
        let barriers = project.update(cx, |project, cx| {
            let repositories = project
                .repositories(cx)
                .values()
                .cloned()
                .collect::<Vec<_>>();
            repositories
                .into_iter()
                .map(|repository| repository.update(cx, |repository, _| repository.barrier()))
                .collect::<Vec<_>>()
        });
        for barrier in barriers {
            let result: anyhow::Result<()> = barrier.await.map_err(|_| {
                anyhow::anyhow!("git repository barrier canceled while archiving worktree")
            });
            result.log_err();
        }
    }

    fn open_workspace_for_archive(
        &mut self,
        folder_paths: PathList,
        project_group_key: ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Entity<Workspace>, &mut Window, &mut Context<Self>) + 'static,
        failed: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            drop(then);
            failed(self, cx);
            return;
        };

        let host = project_group_key.host().cloned();
        let active_workspace = multi_workspace.read(cx).workspace().clone();
        let modal_workspace = active_workspace.clone();

        let open_task = multi_workspace.update(cx, |this, cx| {
            this.find_or_create_workspace(
                folder_paths,
                host,
                Some(project_group_key),
                |options, window, cx| connect_remote(active_workspace, options, window, cx),
                None,
                OpenMode::Add,
                None,
                window,
                cx,
            )
        });

        cx.spawn_in(window, async move |this, cx| {
            let result = open_task.await;
            remote_connection::dismiss_connection_modal(&modal_workspace, cx);
            let workspace = match result {
                Ok(workspace) => workspace,
                Err(error) => {
                    drop(then);
                    this.update(cx, |this, cx| failed(this, cx))?;
                    return Err(error);
                }
            };
            Self::wait_for_archive_workspace_metadata(&workspace, cx).await;

            this.update_in(cx, |this, window, cx| then(this, workspace, window, cx))?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn close_terminal(
        &mut self,
        metadata: &TerminalThreadMetadata,
        workspace: &ThreadEntryWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let terminal_id = metadata.terminal_id;
        let is_active = self
            .active_entry
            .as_ref()
            .is_some_and(|entry| entry.is_active_terminal(terminal_id));
        let neighbor = self
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(
                    entry,
                    ListEntry::Terminal(terminal)
                        if terminal.metadata.terminal_id == terminal_id
                )
            })
            .and_then(|position| self.top_entry_after_removal(position));

        self.close_terminal_entry(
            metadata,
            workspace,
            is_active,
            neighbor.as_ref(),
            window,
            cx,
        );
    }

    fn close_terminal_entry(
        &mut self,
        metadata: &TerminalThreadMetadata,
        workspace: &ThreadEntryWorkspace,
        is_active: bool,
        neighbor: Option<&ListEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let terminal_id = metadata.terminal_id;

        // Closing from the sidebar must not steal focus, since the row's
        // workspace may not be the active workspace.
        let mut was_shown = false;
        if let ThreadEntryWorkspace::Open(workspace) = workspace {
            workspace.update(cx, |workspace, cx| {
                if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                    panel.update(cx, |panel, cx| {
                        was_shown = panel.active_terminal_id() == Some(terminal_id);
                        panel.close_terminal(terminal_id, window, cx);
                    });
                }
            });
        }
        if let Some(store) = TerminalThreadMetadataStore::try_global(cx) {
            store.update(cx, |store, cx| {
                store.delete(terminal_id, cx);
            });
        }

        if is_active {
            self.activate_after_removal(neighbor, window, cx);
        }
        if was_shown {
            Self::close_empty_agent_panel(workspace, window, cx);
        }
        self.update_entries(cx);
    }

    fn remove_workspaces_then(
        &mut self,
        workspaces_to_remove: Vec<Entity<Workspace>>,
        close_item_tasks: Vec<Task<anyhow::Result<()>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
        finish: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cancelled: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        if workspaces_to_remove.is_empty() && close_item_tasks.is_empty() {
            finish(self, window, cx);
            return;
        }

        let remove_task = if workspaces_to_remove.is_empty() {
            None
        } else {
            let Some(multi_workspace) = self.multi_workspace.upgrade() else {
                drop(finish);
                cancelled(self, cx);
                return;
            };
            Some(multi_workspace.update(cx, |multi_workspace, cx| {
                multi_workspace.remove(workspaces_to_remove, RemovalIntent::KeepProject, window, cx)
            }))
        };

        cx.spawn_in(window, async move |this, cx| {
            let removed = match remove_task {
                Some(remove_task) => remove_task.await,
                None => Ok(true),
            };
            if !matches!(removed, Ok(true)) {
                // Dropping `finish` first releases what it holds, such as a deletion guard.
                drop(finish);
                this.update(cx, |this, cx| cancelled(this, cx))?;
                removed?;
                return anyhow::Ok(());
            }

            for task in close_item_tasks {
                let result: anyhow::Result<()> = task.await;
                result.log_err();
            }

            this.update_in(cx, |this, window, cx| finish(this, window, cx))?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn close_items_for_archived_worktrees(
        &self,
        roots_to_archive: &[thread_worktree_archive::RootPlan],
        workspaces_to_remove: &mut Vec<Entity<Workspace>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Task<anyhow::Result<()>>> {
        if roots_to_archive.is_empty() {
            return Vec::new();
        }

        let archive_paths: HashSet<&Path> = roots_to_archive
            .iter()
            .map(|root| root.root_path.as_path())
            .collect();

        let mut mixed_workspaces: Vec<(Entity<Workspace>, Vec<WorktreeId>)> = Vec::new();

        if let Some(multi_workspace) = self.multi_workspace.upgrade() {
            let all_workspaces: Vec<_> = multi_workspace.read(cx).workspaces().cloned().collect();

            for workspace in all_workspaces {
                if workspaces_to_remove.contains(&workspace) {
                    continue;
                }

                let project = workspace.read(cx).project().read(cx);
                let visible_worktrees: Vec<_> = project
                    .visible_worktrees(cx)
                    .map(|worktree| (worktree.read(cx).id(), worktree.read(cx).abs_path()))
                    .collect();

                let archived_worktree_ids: Vec<WorktreeId> = visible_worktrees
                    .iter()
                    .filter(|(_, path)| archive_paths.contains(path.as_ref()))
                    .map(|(id, _)| *id)
                    .collect();

                if archived_worktree_ids.is_empty() {
                    continue;
                }

                if visible_worktrees.len() == archived_worktree_ids.len() {
                    workspaces_to_remove.push(workspace);
                } else {
                    mixed_workspaces.push((workspace, archived_worktree_ids));
                }
            }
        }

        let mut close_item_tasks = Vec::new();
        for (workspace, archived_worktree_ids) in &mixed_workspaces {
            let panes: Vec<_> = workspace.read(cx).panes().to_vec();
            for pane in panes {
                let items_to_close: Vec<EntityId> = pane
                    .read(cx)
                    .items()
                    .filter(|item| {
                        item.project_path(cx)
                            .is_some_and(|pp| archived_worktree_ids.contains(&pp.worktree_id))
                    })
                    .map(|item| item.item_id())
                    .collect();

                if !items_to_close.is_empty() {
                    let task = pane.update(cx, |pane, cx| {
                        pane.close_items(window, cx, SaveIntent::Close, &|item_id| {
                            items_to_close.contains(&item_id)
                        })
                    });
                    close_item_tasks.push(task);
                }
            }
        }

        close_item_tasks
    }

    fn archive_thread(
        &mut self,
        session_id: &acp::SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (metadata, thread_entry) = self.thread_to_archive(session_id, cx);

        self.archive_thread_in_loaded_workspace(session_id, metadata, thread_entry, window, cx);
    }

    fn thread_to_archive(
        &self,
        session_id: &acp::SessionId,
        cx: &App,
    ) -> (Option<ThreadMetadata>, Option<Arc<ThreadEntry>>) {
        let metadata = ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(session_id)
            .cloned();
        let metadata_thread_id = metadata.as_ref().map(|metadata| metadata.thread_id);
        let thread_entry = self.contents.entries.iter().find_map(|entry| match entry {
            ListEntry::Thread(thread) => metadata_thread_id
                .map_or_else(
                    || thread.metadata.session_id.as_ref() == Some(session_id),
                    |thread_id| thread.metadata.thread_id == thread_id,
                )
                .then(|| thread.clone()),
            _ => None,
        });
        (metadata, thread_entry)
    }

    fn archive_thread_in_loaded_workspace(
        &mut self,
        session_id: &acp::SessionId,
        metadata: Option<ThreadMetadata>,
        thread_entry: Option<Arc<ThreadEntry>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let metadata_thread_id = metadata.as_ref().map(|metadata| metadata.thread_id);
        let thread_id = metadata_thread_id.or_else(|| {
            thread_entry
                .as_ref()
                .map(|thread| thread.metadata.thread_id)
        });
        let active_workspace = thread_id.and_then(|thread_id| {
            self.active_entry.as_ref().and_then(|entry| {
                if entry.is_active_thread(&thread_id) {
                    Some(entry.workspace().clone())
                } else {
                    None
                }
            })
        });
        let thread_folder_paths = metadata
            .as_ref()
            .map(|metadata| metadata.folder_paths().clone())
            .or_else(|| {
                thread_entry
                    .as_ref()
                    .map(|thread| thread.metadata.folder_paths().clone())
            })
            .or_else(|| {
                active_workspace
                    .as_ref()
                    .map(|workspace| PathList::new(&workspace.read(cx).root_paths(cx)))
            });

        let current_pos = self.contents.entries.iter().position(|entry| match entry {
            ListEntry::Thread(thread) => thread_id.map_or_else(
                || thread.metadata.session_id.as_ref() == Some(session_id),
                |tid| thread.metadata.thread_id == tid,
            ),
            _ => false,
        });
        let neighbor = current_pos.and_then(|position| self.top_entry_after_removal(position));

        let thread_remote_connection = metadata
            .as_ref()
            .and_then(|metadata| metadata.remote_connection.clone());
        self.archive_and_activate(
            thread_id,
            neighbor.as_ref(),
            thread_folder_paths.as_ref(),
            thread_remote_connection.as_ref(),
            window,
            cx,
        );
    }

    fn archive_and_activate(
        &mut self,
        thread_id: Option<agent_ui::ThreadId>,
        neighbor: Option<&ListEntry>,
        thread_folder_paths: Option<&PathList>,
        thread_remote_connection: Option<&RemoteConnectionOptions>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(thread_id) = thread_id {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.archive(thread_id, None, cx);
            });
        }

        let is_active = self
            .active_entry
            .as_ref()
            .is_some_and(|entry| thread_id.is_some_and(|tid| entry.is_active_thread(&tid)));

        // The archived thread must not stay on screen, and starting a fresh draft in
        // its place would put an empty row where it was.
        let workspace = thread_folder_paths.and_then(|folder_paths| {
            self.multi_workspace.upgrade().and_then(|multi_workspace| {
                multi_workspace.read(cx).workspace_for_paths(
                    folder_paths,
                    thread_remote_connection,
                    cx,
                )
            })
        });
        let panel = workspace
            .as_ref()
            .and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx));
        let shows_archived_thread = panel.as_ref().is_some_and(|panel| {
            panel
                .read(cx)
                .active_conversation_view()
                .map(|conversation_view| conversation_view.read(cx).parent_id())
                .is_some_and(|live_thread_id| thread_id == Some(live_thread_id))
        });
        if shows_archived_thread && let Some(panel) = panel {
            panel.update(cx, |panel, cx| panel.clear_base_view(window, cx));
        }

        if is_active {
            self.activate_after_removal(neighbor, window, cx);
        }
        if shows_archived_thread && let Some(workspace) = workspace {
            Self::close_empty_agent_panel(&ThreadEntryWorkspace::Open(workspace), window, cx);
        }
        self.update_entries(cx);
    }

    async fn archive_worktree_roots(
        roots: Vec<thread_worktree_archive::RootPlan>,
        cancel_receivers: Vec<async_channel::Receiver<()>>,
        cx: &mut gpui::AsyncApp,
    ) -> anyhow::Result<ArchiveWorktreeOutcome> {
        let is_cancelled = || cancel_receivers.iter().any(|receiver| receiver.is_closed());
        let mut completed_persists: Vec<(i64, thread_worktree_archive::RootPlan)> = Vec::new();

        for root in &roots {
            if is_cancelled() {
                for &(id, ref completed_root) in completed_persists.iter().rev() {
                    thread_worktree_archive::rollback_persist(id, completed_root, cx).await;
                }
                return Ok(ArchiveWorktreeOutcome::Cancelled);
            }

            match thread_worktree_archive::persist_worktree_state(root, cx).await {
                Ok(id) => {
                    completed_persists.push((id, root.clone()));
                }
                Err(error) => {
                    for &(id, ref completed_root) in completed_persists.iter().rev() {
                        thread_worktree_archive::rollback_persist(id, completed_root, cx).await;
                    }
                    return Err(error);
                }
            }

            if is_cancelled() {
                for &(id, ref completed_root) in completed_persists.iter().rev() {
                    thread_worktree_archive::rollback_persist(id, completed_root, cx).await;
                }
                return Ok(ArchiveWorktreeOutcome::Cancelled);
            }

            if let Err(error) = thread_worktree_archive::remove_root(root.clone(), cx).await {
                if let Some(&(id, ref completed_root)) = completed_persists.last() {
                    if completed_root.root_path == root.root_path {
                        thread_worktree_archive::rollback_persist(id, completed_root, cx).await;
                        completed_persists.pop();
                    }
                }
                for &(id, ref completed_root) in completed_persists.iter().rev() {
                    thread_worktree_archive::rollback_persist(id, completed_root, cx).await;
                }
                return Err(error);
            }
        }

        Ok(ArchiveWorktreeOutcome::Success)
    }

    fn activate_workspace(
        &self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(multi_workspace) = self.multi_workspace.upgrade() {
            multi_workspace.update(cx, |mw, cx| {
                mw.activate(workspace.clone(), None, window, cx);
            });
        }
    }

    fn archive_selected_thread(
        &mut self,
        _: &ArchiveSelectedThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.selection else {
            return;
        };
        match self.contents.entries.get(ix) {
            Some(ListEntry::Thread(thread)) => {
                match thread.status {
                    AgentThreadStatus::Running
                    | AgentThreadStatus::WaitingForConfirmation
                    | AgentThreadStatus::Unconfirmed => {
                        return;
                    }
                    AgentThreadStatus::Completed | AgentThreadStatus::Error => {}
                }
                if thread.draft.is_some() {
                    let workspace = thread.workspace.clone();
                    let draft_id = thread.metadata.thread_id;
                    self.remove_draft(draft_id, &workspace, window, cx);
                } else if let Some(session_id) = thread.metadata.session_id.clone() {
                    self.archive_thread(&session_id, window, cx);
                }
            }
            Some(ListEntry::Terminal(terminal)) => {
                let metadata = terminal.metadata.clone();
                let workspace = terminal.workspace.clone();
                self.close_terminal(&metadata, &workspace, window, cx);
            }
            _ => {}
        }
    }

    fn rename_selected_thread(
        &mut self,
        _: &RenameSelectedThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.selection else {
            return;
        };
        let Some((target, title)) = self
            .contents
            .entries
            .get(ix)
            .and_then(RenameTarget::from_entry)
        else {
            return;
        };
        self.start_renaming_entry(ix, target, title, window, cx);
    }

    fn record_thread_access(&mut self, id: &ThreadId) {
        self.thread_last_accessed.insert(*id, Utc::now());
    }

    fn record_terminal_access(&mut self, id: TerminalId) {
        self.terminal_last_accessed.insert(id, Utc::now());
    }

    fn record_thread_interacted(&mut self, thread_id: &agent_ui::ThreadId, cx: &mut App) {
        let store = ThreadMetadataStore::global(cx);
        store.update(cx, |store, cx| {
            store.update_interacted_at(thread_id, Utc::now(), cx);
        })
    }

    fn thread_display_time(metadata: &ThreadMetadata) -> DateTime<Utc> {
        metadata.interacted_at.unwrap_or(metadata.updated_at)
    }

    fn push_entries_by_display_time(
        entries: &mut Vec<ListEntry>,
        terminals: Vec<TerminalEntry>,
        threads: Vec<Arc<ThreadEntry>>,
        group_key: &ProjectGroupKey,
        fallback_label: SharedString,
        known_worktrees: Vec<KnownWorktree>,
        branch_by_path: &HashMap<PathBuf, SharedString>,
        worktree_pull_requests: &HashMap<PathBuf, WorktreePullRequest>,
        worktree_group_order: &[SharedString],
        collapsed_worktree_groups: &HashSet<SharedString>,
        deleting_worktree_paths: &HashSet<PathBuf>,
        active_agent_paths: &HashSet<PathBuf>,
        notified_threads: &HashSet<agent_ui::ThreadId>,
        current_session_ids: &mut HashSet<acp::SessionId>,
        current_thread_ids: &mut HashSet<agent_ui::ThreadId>,
    ) {
        fn display_time(entry: &ListEntry) -> DateTime<Utc> {
            match entry {
                ListEntry::Thread(thread) => Sidebar::thread_display_time(&thread.metadata),
                ListEntry::Terminal(terminal) => terminal.metadata.created_at,
                ListEntry::ProjectHeader { .. } | ListEntry::WorktreeHeader { .. } => {
                    unreachable!()
                }
            }
        }

        fn worktree(
            entry: &ListEntry,
        ) -> Option<(
            SharedString,
            Vec<SharedString>,
            SharedString,
            Vec<(SharedString, Option<SharedString>)>,
        )> {
            let worktrees = match entry {
                ListEntry::Thread(thread) => &thread.worktrees,
                ListEntry::Terminal(terminal) => &terminal.worktrees,
                ListEntry::ProjectHeader { .. } | ListEntry::WorktreeHeader { .. } => return None,
            };
            let first = worktrees.first()?;
            let extra_worktrees = worktrees
                .iter()
                .skip(1)
                .filter_map(|info| Some((info.worktree_name.clone()?, info.branch_name.clone())))
                .collect();
            Some((
                first.full_path.clone(),
                worktrees
                    .iter()
                    .map(|info| info.full_path.clone())
                    .collect(),
                first.worktree_name.clone()?,
                extra_worktrees,
            ))
        }

        let row_entries = terminals
            .into_iter()
            .map(ListEntry::Terminal)
            .chain(threads.into_iter().map(ListEntry::Thread))
            .sorted_by_key(|right| std::cmp::Reverse(display_time(right)));

        struct Group {
            id: SharedString,
            path: SharedString,
            roots: Vec<SharedString>,
            label: SharedString,
            extra_worktrees: Vec<(SharedString, Option<SharedString>)>,
            rows: Vec<ListEntry>,
        }
        let mut groups: Vec<Group> = Vec::new();
        for entry in row_entries {
            if let ListEntry::Thread(thread) = &entry {
                if let Some(session_id) = &thread.metadata.session_id {
                    current_session_ids.insert(session_id.clone());
                }
                current_thread_ids.insert(thread.metadata.thread_id);
            }
            let (path, roots, label, extra_worktrees) = worktree(&entry).unwrap_or_else(|| {
                (
                    SharedString::default(),
                    vec![SharedString::default()],
                    fallback_label.clone(),
                    Vec::new(),
                )
            });
            let id = worktree_group_id_for_roots(group_key, roots.iter().map(|root| root.as_ref()));
            match groups.iter_mut().find(|group| group.id == id) {
                Some(group) => group.rows.push(entry),
                None => groups.push(Group {
                    id,
                    path,
                    roots,
                    label,
                    extra_worktrees,
                    rows: vec![entry],
                }),
            }
        }

        for known in &known_worktrees {
            let id = worktree_group_id_for_roots(
                group_key,
                known.roots.iter().map(|root| root.as_ref()),
            );
            if !groups.iter().any(|group| group.id == id) {
                groups.push(Group {
                    id,
                    path: known.path.clone(),
                    roots: known.roots.clone(),
                    label: known.label.clone(),
                    extra_worktrees: known.extra_worktrees.clone(),
                    rows: Vec::new(),
                });
            }
        }

        groups.retain(|group| !deleting_worktree_paths.contains(Path::new(group.path.as_ref())));

        // Stable, so groups without a saved position stay most-recent-first, above the saved ones.
        groups.sort_by_key(|group| {
            worktree_group_order
                .iter()
                .position(|ordered| *ordered == group.id)
                .map_or(0, |position| position + 1)
        });
        let ids: Vec<SharedString> = groups.iter().map(|group| group.id.clone()).collect();
        for (
            index,
            Group {
                id,
                path,
                roots,
                label,
                extra_worktrees,
                rows,
            },
        ) in groups.into_iter().enumerate()
        {
            let is_collapsed = collapsed_worktree_groups.contains(&id);
            let threads = rows.iter().filter_map(|row| match row {
                ListEntry::Thread(thread) => Some(thread),
                _ => None,
            });
            let pull_request_targets: Vec<(PathBuf, SharedString)> = if group_key.host().is_some() {
                Vec::new()
            } else {
                roots
                    .iter()
                    .filter_map(|root| {
                        let root = PathBuf::from(root.as_ref());
                        let branch = branch_by_path.get(&root)?.clone();
                        Some((root, branch))
                    })
                    .collect()
            };
            entries.push(ListEntry::WorktreeHeader {
                pull_request: header_pull_request(&pull_request_targets, worktree_pull_requests),
                pull_request_targets,
                branch: branch_by_path.get(Path::new(path.as_ref())).cloned(),
                extra_worktrees,
                is_linked: known_worktrees
                    .iter()
                    .any(|known| known.is_linked && known.path == path),
                has_rows: !rows.is_empty(),
                is_collapsed,
                previous: index
                    .checked_sub(1)
                    .and_then(|index| ids.get(index).cloned()),
                next: ids.get(index + 1).cloned(),
                has_running_threads: threads
                    .clone()
                    .any(|thread| thread.status == AgentThreadStatus::Running),
                waiting_thread_count: threads
                    .clone()
                    .filter(|thread| thread.status == AgentThreadStatus::WaitingForConfirmation)
                    .count(),
                has_active_agent: active_agent_paths.contains(Path::new(path.as_ref())),
                has_notifications: rows.iter().any(|row| match row {
                    ListEntry::Thread(thread) => {
                        notified_threads.contains(&thread.metadata.thread_id)
                    }
                    ListEntry::Terminal(terminal) => terminal.has_notification,
                    _ => false,
                }),
                collapsed_rows: if is_collapsed {
                    rows.clone()
                } else {
                    Vec::new()
                },
                id,
                path,
                roots,
                label,
            });
            if !is_collapsed {
                entries.extend(rows);
            }
        }
    }

    /// The sort order used by the ctrl-tab switcher
    fn switcher_entry_cmp(
        &self,
        left: &ThreadSwitcherEntry,
        right: &ThreadSwitcherEntry,
    ) -> Ordering {
        let sort_time = |entry: &ThreadSwitcherEntry| match entry {
            ThreadSwitcherEntry::Thread(entry) => self
                .thread_last_accessed
                .get(&entry.metadata.thread_id)
                .copied()
                .or(entry.metadata.interacted_at)
                .unwrap_or(entry.metadata.updated_at),
            ThreadSwitcherEntry::Terminal(entry) => self
                .terminal_last_accessed
                .get(&entry.metadata.terminal_id)
                .copied()
                .unwrap_or(entry.metadata.created_at),
        };

        // .reverse() = most recent first
        sort_time(left).cmp(&sort_time(right)).reverse()
    }

    fn mru_entries_for_switcher(&self, cx: &App) -> Vec<ThreadSwitcherEntry> {
        let worktree_label_mode = cx.flag_value::<AgentThreadWorktreeLabelFlag>();
        let switcher_worktrees = |worktrees: &[ThreadItemWorktreeInfo]| {
            apply_worktree_label_mode(
                worktrees
                    .iter()
                    .cloned()
                    .map(|mut wt| {
                        wt.highlight_positions = Vec::new();
                        wt
                    })
                    .collect(),
                worktree_label_mode,
            )
        };
        let mut current_header_label: Option<SharedString> = None;
        let mut current_header_key: Option<ProjectGroupKey> = None;
        let mut entries: Vec<ThreadSwitcherEntry> = self
            .contents
            .entries
            .iter()
            .flat_map(|entry| {
                let collapsed_rows = match entry {
                    ListEntry::WorktreeHeader { collapsed_rows, .. } => collapsed_rows.as_slice(),
                    _ => &[],
                };
                std::iter::once(entry).chain(collapsed_rows)
            })
            .filter_map(|entry| match entry {
                ListEntry::ProjectHeader { label, key, .. } => {
                    current_header_label = Some(label.clone());
                    current_header_key = Some(key.clone());
                    None
                }
                ListEntry::WorktreeHeader { .. } => None,
                ListEntry::Thread(thread) => {
                    if thread.draft == Some(DraftKind::Empty) {
                        return None;
                    }
                    let workspace = match &thread.workspace {
                        ThreadEntryWorkspace::Open(workspace) => Some(workspace.clone()),
                        ThreadEntryWorkspace::Closed { .. } => {
                            current_header_key.as_ref().and_then(|key| {
                                self.multi_workspace.upgrade().and_then(|mw| {
                                    mw.read(cx)
                                        .workspace_for_paths(key.path_list(), key.host(), cx)
                                })
                            })
                        }
                    }?;
                    let notified = self.contents.is_thread_notified(&thread.metadata.thread_id);
                    let timestamp: SharedString =
                        format_history_entry_timestamp(Self::thread_display_time(&thread.metadata))
                            .into();
                    Some(ThreadSwitcherEntry::Thread(ThreadSwitcherThreadEntry {
                        title: thread.metadata.display_title(),
                        icon: thread.icon,
                        icon_from_external_svg: thread.icon_from_external_svg.clone(),
                        status: thread.status,
                        metadata: thread.metadata.clone(),
                        workspace,
                        project_name: current_header_label.clone(),
                        worktrees: switcher_worktrees(&thread.worktrees),
                        diff_stats: thread.diff_stats,
                        is_title_generating: thread.is_title_generating,
                        notified,
                        timestamp,
                    }))
                }
                ListEntry::Terminal(terminal) => {
                    let timestamp: SharedString =
                        format_history_entry_timestamp(terminal.metadata.created_at).into();
                    Some(ThreadSwitcherEntry::Terminal(ThreadSwitcherTerminalEntry {
                        metadata: terminal.metadata.clone(),
                        workspace: terminal.workspace.clone(),
                        project_name: current_header_label.clone(),
                        worktrees: switcher_worktrees(&terminal.worktrees),
                        notified: self
                            .contents
                            .is_terminal_notified(terminal.metadata.terminal_id),
                        timestamp,
                    }))
                }
            })
            .collect();

        entries.sort_by(|a, b| self.switcher_entry_cmp(a, b));

        entries
    }

    fn dismiss_thread_switcher(&mut self, cx: &mut Context<Self>) {
        self.thread_switcher = None;
        self._thread_switcher_subscriptions.clear();
        if let Some(mw) = self.multi_workspace.upgrade() {
            mw.update(cx, |mw, cx| {
                mw.set_sidebar_overlay(None, cx);
            });
        }
    }

    fn on_toggle_thread_switcher(
        &mut self,
        action: &ToggleThreadSwitcher,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_thread_switcher_impl(action.select_last, window, cx);
    }

    fn preview_switcher_selection(
        &mut self,
        selection: &ThreadSwitcherSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match selection {
            ThreadSwitcherSelection::Thread {
                metadata,
                workspace,
            } => {
                if let Some(multi_workspace) = self.multi_workspace.upgrade() {
                    multi_workspace.update(cx, |multi_workspace, cx| {
                        multi_workspace.activate(workspace.clone(), None, window, cx);
                    });
                }
                self.active_entry = Some(ActiveEntry::Thread {
                    thread_id: metadata.thread_id,
                    session_id: metadata.session_id.clone(),
                    workspace: workspace.clone(),
                });
                self.update_entries(cx);
                Self::load_agent_thread_in_workspace(workspace, metadata, false, window, cx);
            }
            ThreadSwitcherSelection::Terminal {
                metadata,
                workspace,
            } => {
                if let ThreadEntryWorkspace::Open(workspace) = workspace {
                    if let Some(multi_workspace) = self.multi_workspace.upgrade() {
                        multi_workspace.update(cx, |multi_workspace, cx| {
                            multi_workspace.activate(workspace.clone(), None, window, cx);
                        });
                    }
                    self.active_entry = Some(ActiveEntry::Terminal {
                        terminal_id: metadata.terminal_id,
                        workspace: workspace.clone(),
                    });
                    self.update_entries(cx);
                    Self::load_agent_terminal_in_workspace(workspace, metadata, false, window, cx);
                }
            }
        }
    }

    fn confirm_switcher_selection(
        &mut self,
        selection: &ThreadSwitcherSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match selection {
            ThreadSwitcherSelection::Thread {
                metadata,
                workspace,
            } => {
                if let Some(multi_workspace) = self.multi_workspace.upgrade() {
                    multi_workspace.update(cx, |multi_workspace, cx| {
                        multi_workspace.activate(workspace.clone(), None, window, cx);
                        multi_workspace.retain_active_workspace(cx);
                    });
                }
                self.record_thread_access(&metadata.thread_id);
                self.active_entry = Some(ActiveEntry::Thread {
                    thread_id: metadata.thread_id,
                    session_id: metadata.session_id.clone(),
                    workspace: workspace.clone(),
                });
                self.update_entries(cx);
                self.dismiss_thread_switcher(cx);
                Self::load_agent_thread_in_workspace(workspace, metadata, true, window, cx);
            }
            ThreadSwitcherSelection::Terminal {
                metadata,
                workspace,
            } => {
                self.dismiss_thread_switcher(cx);
                self.activate_terminal_entry(metadata.clone(), workspace.clone(), true, window, cx);
            }
        }
    }

    fn toggle_thread_switcher_impl(
        &mut self,
        select_last: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(thread_switcher) = &self.thread_switcher {
            thread_switcher.update(cx, |switcher, cx| {
                if select_last {
                    switcher.select_previous(&SelectPrevious, window, cx);
                } else {
                    switcher.select_next(&SelectNext, window, cx);
                }
            });
            return;
        }

        let entries = self.mru_entries_for_switcher(cx);
        if entries.len() < 2 {
            return;
        }

        let weak_multi_workspace = self.multi_workspace.clone();

        // Snapshot the active entry (thread or terminal) so dismissal can
        // restore it.
        let original_active_entry = self.active_entry.clone();
        let original_metadata = match &original_active_entry {
            Some(ActiveEntry::Thread { thread_id, .. }) => {
                entries.iter().find_map(|entry| match entry {
                    ThreadSwitcherEntry::Thread(entry)
                        if *thread_id == entry.metadata.thread_id =>
                    {
                        Some(entry.metadata.clone())
                    }
                    _ => None,
                })
            }
            _ => None,
        };
        let original_workspace = self
            .multi_workspace
            .upgrade()
            .map(|mw| mw.read(cx).workspace().clone());

        let thread_switcher = cx.new(|cx| ThreadSwitcher::new(entries, select_last, window, cx));

        let mut subscriptions = Vec::new();

        subscriptions.push(cx.subscribe_in(&thread_switcher, window, {
            let thread_switcher = thread_switcher.clone();
            move |this, _emitter, event: &ThreadSwitcherEvent, window, cx| match event {
                ThreadSwitcherEvent::Preview(selection) => {
                    this.preview_switcher_selection(selection, window, cx);
                    let focus = thread_switcher.focus_handle(cx);
                    window.focus(&focus, cx);
                }
                ThreadSwitcherEvent::Confirmed(selection) => {
                    this.confirm_switcher_selection(selection, window, cx);
                }
                ThreadSwitcherEvent::Dismissed => {
                    if let Some(mw) = weak_multi_workspace.upgrade() {
                        if let Some(original_ws) = &original_workspace {
                            mw.update(cx, |mw, cx| {
                                mw.activate(original_ws.clone(), None, window, cx);
                            });
                        }
                    }
                    match &original_active_entry {
                        Some(ActiveEntry::Thread { .. }) => {
                            if let (Some(metadata), Some(original_ws)) =
                                (&original_metadata, &original_workspace)
                            {
                                this.active_entry = Some(ActiveEntry::Thread {
                                    thread_id: metadata.thread_id,
                                    session_id: metadata.session_id.clone(),
                                    workspace: original_ws.clone(),
                                });
                                this.update_entries(cx);
                                Self::load_agent_thread_in_workspace(
                                    original_ws,
                                    metadata,
                                    false,
                                    window,
                                    cx,
                                );
                            }
                        }
                        Some(ActiveEntry::Terminal {
                            terminal_id,
                            workspace,
                        }) => {
                            let terminal_id = *terminal_id;
                            let workspace = workspace.clone();
                            this.active_entry = Some(ActiveEntry::Terminal {
                                terminal_id,
                                workspace: workspace.clone(),
                            });
                            this.update_entries(cx);
                            workspace.update(cx, |workspace, cx| {
                                if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                                    panel.update(cx, |panel, cx| {
                                        panel.activate_terminal(terminal_id, false, window, cx);
                                    });
                                }
                            });
                        }
                        None => {}
                    }
                    this.dismiss_thread_switcher(cx);
                }
            }
        }));

        subscriptions.push(cx.subscribe_in(
            &thread_switcher,
            window,
            |this, _emitter, _event: &gpui::DismissEvent, _window, cx| {
                this.dismiss_thread_switcher(cx);
            },
        ));

        let focus = thread_switcher.focus_handle(cx);
        let overlay_view = gpui::AnyView::from(thread_switcher.clone());

        // Replay the initial preview that was emitted during construction
        // before subscriptions were wired up.
        let initial_preview = thread_switcher
            .read(cx)
            .selected_entry()
            .map(ThreadSwitcherEntry::selection);

        self.thread_switcher = Some(thread_switcher);
        self._thread_switcher_subscriptions = subscriptions;
        if let Some(mw) = self.multi_workspace.upgrade() {
            mw.update(cx, |mw, cx| {
                mw.set_sidebar_overlay(Some(overlay_view), cx);
            });
        }

        if let Some(selection) = initial_preview {
            self.preview_switcher_selection(&selection, window, cx);
        }

        window.focus(&focus, cx);
    }

    fn render_rename_title_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .h_full()
            .min_w_0()
            .flex_1()
            .capture_action(
                cx.listener(|this, _: &editor::actions::Newline, window, cx| {
                    this.finish_entry_rename(window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &Confirm, window, cx| {
                this.finish_entry_rename(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &editor::actions::Cancel, window, cx| {
                    if matches!(this.rename_target, Some(RenameTarget::Project(_))) {
                        this.rename_target = None;
                        this.focus_handle.focus(window, cx);
                        this.update_entries(cx);
                        return;
                    }
                    this.finish_entry_rename(window, cx);
                }),
            )
            .child(self.rename_editor.clone())
            .into_any_element()
    }

    fn render_thread(
        &self,
        ix: usize,
        thread: &ThreadEntry,
        is_active: bool,
        is_focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_notification = self.contents.is_thread_notified(&thread.metadata.thread_id);

        let title: SharedString = thread.metadata.display_title();
        let metadata = thread.metadata.clone();
        let thread_workspace = thread.workspace.clone();

        let is_hovered = self.hovered_thread_index == Some(ix);
        let is_selected = is_active;
        let is_draft = thread.draft.is_some();
        let is_empty_draft = thread.draft == Some(DraftKind::Empty);
        let can_request_stop = matches!(
            thread.status,
            AgentThreadStatus::Running
                | AgentThreadStatus::WaitingForConfirmation
                | AgentThreadStatus::Unconfirmed
        );
        let is_renaming =
            self.rename_target == Some(RenameTarget::Thread(thread.metadata.thread_id));

        let thread_id_for_actions = thread.metadata.thread_id;
        let session_id_for_delete = thread.metadata.session_id.clone();
        let focus_handle = self.focus_handle.clone();
        let rename_title_editor = is_renaming.then(|| self.render_rename_title_editor(cx));

        let id = SharedString::from(format!("thread-entry-{}", ix));

        let color = cx.theme().colors();
        let sidebar_bg = color.panel_background;
        let button_hover_bg = color.element_background;
        let button_active_bg = color.element_active;

        let timestamp: SharedString = if is_empty_draft {
            SharedString::default()
        } else {
            format_history_entry_timestamp(Self::thread_display_time(&thread.metadata)).into()
        };

        let is_remote = thread.workspace.is_remote(cx);

        let title_generating = thread.is_title_generating
            || self
                .regenerating_titles
                .contains(&thread.metadata.thread_id);

        let thread_item = ThreadItem::new(id, title.clone())
            .when(self.worktree_group_id_for_entry(ix).is_some(), |this| {
                this.left_padding(px(10. + 2. * WORKTREE_GROUP_INDENT))
            })
            .base_bg(sidebar_bg)
            .icon(thread.icon)
            .status(thread.status)
            .is_remote(is_remote)
            .when_some(thread.icon_from_external_svg.clone(), |this, svg| {
                this.custom_icon_from_external_svg(svg)
            })
            .timestamp(timestamp)
            .timestamp_in_title_row(true)
            .highlight_positions(thread.highlight_positions.to_vec())
            .title_generating(title_generating)
            .notified(has_notification)
            .selected(is_selected)
            .focused(is_focused)
            .hovered(is_hovered)
            .on_hover(cx.listener(move |this, is_hovered: &bool, _window, cx| {
                if *is_hovered {
                    this.hovered_thread_index = Some(ix);
                } else if this.hovered_thread_index == Some(ix) {
                    this.hovered_thread_index = None;
                }
                cx.notify();
            }))
            .when_some(rename_title_editor, |this, title_editor| {
                this.is_truncated(false).title_slot(title_editor)
            })
            .when(is_hovered && !is_renaming, |this| {
                let rename_button = IconButton::new(("rename-thread", ix), IconName::Pencil)
                    .hover_background(button_hover_bg)
                    .active_background(button_active_bg)
                    .icon_size(IconSize::Small)
                    .tooltip({
                        let focus_handle = focus_handle.clone();
                        move |_window, cx| {
                            Tooltip::for_action_in(
                                "Rename Thread",
                                &RenameSelectedThread,
                                &focus_handle,
                                cx,
                            )
                        }
                    })
                    .on_click({
                        let title = title.clone();
                        cx.listener(move |this, _, window, cx| {
                            this.start_renaming_entry(
                                ix,
                                RenameTarget::Thread(thread_id_for_actions),
                                title.clone(),
                                window,
                                cx,
                            );
                        })
                    });

                let contextual_action: Option<AnyElement> = if can_request_stop {
                    Some(
                        IconButton::new("stop-thread", IconName::Stop)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Error)
                            .style(ButtonStyle::Tinted(TintColor::Error))
                            .tooltip(Tooltip::text(
                                if thread.status == AgentThreadStatus::Unconfirmed {
                                    "Request Stop (Activity Unconfirmed)"
                                } else {
                                    "Stop Generation"
                                },
                            ))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.stop_thread(&thread_id_for_actions, cx);
                            }))
                            .into_any_element(),
                    )
                } else {
                    match thread.draft {
                        Some(_) => Some(
                            IconButton::new("discard_thread", IconName::Close)
                                .hover_background(button_hover_bg)
                                .active_background(button_active_bg)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Discard Draft"))
                                .on_click({
                                    let thread_workspace = thread_workspace.clone();
                                    cx.listener(move |this, _, window, cx| {
                                        this.remove_draft(
                                            thread_id_for_actions,
                                            &thread_workspace,
                                            window,
                                            cx,
                                        );
                                    })
                                })
                                .into_any_element(),
                        ),
                        None => Some(
                            IconButton::new("archive-thread", IconName::Archive)
                                .hover_background(button_hover_bg)
                                .active_background(button_active_bg)
                                .icon_size(IconSize::Small)
                                .tooltip({
                                    let focus_handle = focus_handle.clone();
                                    move |_window, cx| {
                                        Tooltip::for_action_in(
                                            "Archive Thread",
                                            &ArchiveSelectedThread,
                                            &focus_handle,
                                            cx,
                                        )
                                    }
                                })
                                .on_click({
                                    let session_id = session_id_for_delete.clone();
                                    cx.listener(move |this, _, window, cx| {
                                        if let Some(ref session_id) = session_id {
                                            this.archive_thread(session_id, window, cx);
                                        }
                                    })
                                })
                                .into_any_element(),
                        ),
                    }
                };

                this.action_slot(
                    h_flex()
                        .gap_0p5()
                        .child(rename_button)
                        .when_some(contextual_action, |this, action| this.child(action)),
                )
            })
            .on_click({
                let thread_workspace = thread_workspace.clone();
                cx.listener(move |this, _, window, cx| {
                    this.selection = None;
                    match &thread_workspace {
                        ThreadEntryWorkspace::Open(workspace) => {
                            this.activate_thread(metadata.clone(), workspace, false, window, cx);
                        }
                        ThreadEntryWorkspace::Closed {
                            folder_paths,
                            project_group_key,
                        } => {
                            this.open_workspace_and_activate_thread(
                                metadata.clone(),
                                folder_paths.clone(),
                                project_group_key,
                                window,
                                cx,
                            );
                        }
                    }
                })
            });

        if is_draft || thread.metadata.session_id.is_none() {
            return thread_item.into_any_element();
        }

        let Some(session_id) = thread.metadata.session_id.clone() else {
            return thread_item.into_any_element();
        };

        let context_menu_id = SharedString::from(format!("thread-context-menu-{}", ix));
        let sidebar = cx.weak_entity();

        let active_workspace = self.active_workspace(cx);
        let thread_workspace = match &thread_workspace {
            ThreadEntryWorkspace::Open(workspace) => Some(workspace.clone()),
            ThreadEntryWorkspace::Closed { .. } => None,
        };

        let is_zed_thread = thread.metadata.agent_id.as_ref() == ZED_AGENT_ID.as_ref();
        let can_open_as_markdown = thread.is_live || is_zed_thread;
        let folder_paths = thread.metadata.folder_paths().clone();

        right_click_menu(context_menu_id)
            .trigger(move |_, _, _| thread_item)
            .menu({
                let thread_id = thread.metadata.thread_id;
                let markdown_title = Some(thread.metadata.display_title());
                let rename_title = title;
                move |_window, cx| {
                    let session_id = session_id.clone();
                    let sidebar = sidebar.clone();
                    let active_workspace = active_workspace.clone();
                    let thread_workspace = thread_workspace.clone();
                    let markdown_title = markdown_title.clone();
                    let rename_title = rename_title.clone();
                    let folder_paths = folder_paths.clone();
                    ContextMenu::build(_window, cx, move |mut menu, _window, _cx| {
                        menu = menu.entry("Rename Title", None, {
                            let sidebar = sidebar.clone();
                            let rename_title = rename_title.clone();
                            move |window, cx| {
                                sidebar
                                    .update(cx, |sidebar, cx| {
                                        sidebar.start_renaming_entry(
                                            ix,
                                            RenameTarget::Thread(thread_id),
                                            rename_title.clone(),
                                            window,
                                            cx,
                                        );
                                    })
                                    .ok();
                            }
                        });

                        if is_zed_thread {
                            menu = menu.entry("Regenerate Thread Title", None, {
                                let session_id = session_id.clone();
                                let sidebar = sidebar.clone();
                                let thread_workspace = thread_workspace.clone();
                                let folder_paths = folder_paths.clone();
                                move |_window, cx| {
                                    sidebar
                                        .update(cx, |sidebar, cx| {
                                            sidebar.regenerate_thread_title(
                                                &session_id,
                                                thread_id,
                                                folder_paths.clone(),
                                                thread_workspace.clone(),
                                                cx,
                                            );
                                        })
                                        .ok();
                                }
                            });
                        }

                        if can_open_as_markdown {
                            menu = menu.entry("Open Thread as Markdown", None, {
                                let session_id = session_id.clone();
                                let markdown_title = markdown_title.clone();
                                let thread_workspace = thread_workspace.clone();
                                move |window, cx| {
                                    if let Some(thread_workspace) = thread_workspace.as_ref()
                                        && let Some(panel) =
                                            thread_workspace.read(cx).panel::<AgentPanel>(cx)
                                    {
                                        let opened = panel.update(cx, |panel, cx| {
                                            panel.open_thread_as_markdown(
                                                thread_id,
                                                thread_workspace.clone(),
                                                window,
                                                cx,
                                            )
                                        });
                                        if opened {
                                            return;
                                        }
                                    }

                                    if is_zed_thread
                                        && let Some(active_workspace) = &active_workspace
                                    {
                                        Self::open_closed_native_thread_as_markdown(
                                            &session_id,
                                            markdown_title.clone(),
                                            active_workspace,
                                            window,
                                            cx,
                                        );
                                    }
                                }
                            });
                        }

                        menu.separator().entry("Archive Thread", None, {
                            let session_id = session_id.clone();
                            move |window, cx| {
                                sidebar
                                    .update(cx, |sidebar, cx| {
                                        sidebar.archive_thread(&session_id, window, cx);
                                    })
                                    .ok();
                            }
                        })
                    })
                }
            })
            .into_any_element()
    }

    fn render_terminal(
        &self,
        ix: usize,
        terminal: &TerminalEntry,
        is_active: bool,
        is_focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = ElementId::from(format!("terminal-{}", terminal.metadata.terminal_id));
        let timestamp = format_history_entry_timestamp(terminal.metadata.created_at);
        let is_hovered = self.hovered_thread_index == Some(ix);
        let color = cx.theme().colors();
        let sidebar_bg = color.panel_background;
        let button_hover_bg = color.element_background;
        let button_active_bg = color.element_active;
        let metadata = terminal.metadata.clone();
        let workspace = terminal.workspace.clone();
        let focus_handle = self.focus_handle.clone();
        let is_remote = terminal.workspace.is_remote(cx);
        let is_renaming =
            self.rename_target == Some(RenameTarget::Terminal(terminal.metadata.terminal_id));
        let rename_title_editor = is_renaming.then(|| self.render_rename_title_editor(cx));

        let display_title = terminal.metadata.display_title();
        let (icon_char, title, highlight_positions) =
            match split_leading_icon_char(&display_title, &terminal.highlight_positions) {
                Some((icon_char, title, positions)) => (Some(icon_char), title, positions),
                None => (None, display_title, terminal.highlight_positions.clone()),
            };

        let terminal_item = ThreadItem::new(id, title)
            .when(self.worktree_group_id_for_entry(ix).is_some(), |this| {
                this.left_padding(px(10. + 2. * WORKTREE_GROUP_INDENT))
            })
            .base_bg(sidebar_bg)
            .icon(IconName::Terminal)
            .when_some(icon_char, |this, icon_char| this.icon_char(icon_char))
            .is_remote(is_remote)
            .timestamp(timestamp)
            .timestamp_in_title_row(true)
            .notified(terminal.has_notification)
            .highlight_positions(highlight_positions)
            .selected(is_active)
            .focused(is_focused)
            .hovered(is_hovered)
            .on_hover(cx.listener(move |this, is_hovered: &bool, _window, cx| {
                if *is_hovered {
                    this.hovered_thread_index = Some(ix);
                } else if this.hovered_thread_index == Some(ix) {
                    this.hovered_thread_index = None;
                }
                cx.notify();
            }))
            .when_some(rename_title_editor, |this, title_editor| {
                this.is_truncated(false).title_slot(title_editor)
            })
            .when(is_hovered && !is_renaming, |this| {
                this.action_slot(
                    IconButton::new("close-terminal", IconName::Close)
                        .hover_background(button_hover_bg)
                        .active_background(button_active_bg)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip({
                            let focus_handle = focus_handle.clone();
                            move |_window, cx| {
                                Tooltip::for_action_in(
                                    "Close Terminal",
                                    &ArchiveSelectedThread,
                                    &focus_handle,
                                    cx,
                                )
                            }
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.close_terminal(&metadata, &workspace, window, cx);
                        })),
                )
            })
            .on_click(cx.listener({
                let metadata = terminal.metadata.clone();
                let workspace = terminal.workspace.clone();
                move |this, _, window, cx| {
                    this.selection = None;
                    this.activate_terminal_entry(
                        metadata.clone(),
                        workspace.clone(),
                        false,
                        window,
                        cx,
                    );
                }
            }));

        let context_menu_id = SharedString::from(format!("terminal-context-menu-{ix}"));
        let terminal_id = terminal.metadata.terminal_id;
        let rename_title = terminal.metadata.editable_title();
        let sidebar = cx.weak_entity();

        right_click_menu(context_menu_id)
            .trigger(move |_, _, _| terminal_item)
            .menu(move |window, cx| {
                let sidebar = sidebar.clone();
                let rename_title = rename_title.clone();
                ContextMenu::build(window, cx, move |menu, _window, _cx| {
                    menu.entry("Rename Title", None, move |window, cx| {
                        sidebar
                            .update(cx, |sidebar, cx| {
                                sidebar.start_renaming_entry(
                                    ix,
                                    RenameTarget::Terminal(terminal_id),
                                    rename_title.clone(),
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    })
                })
            })
            .into_any_element()
    }

    fn render_filter_input(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .min_w_0()
            .flex_1()
            .capture_action(
                cx.listener(|this, _: &editor::actions::Newline, window, cx| {
                    this.editor_confirm(window, cx);
                }),
            )
            .child(self.filter_editor.clone())
    }

    fn render_recent_projects_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let multi_workspace = self.multi_workspace.upgrade();

        let workspace = multi_workspace
            .as_ref()
            .map(|mw| mw.read(cx).workspace().downgrade());

        let focus_handle = workspace
            .as_ref()
            .and_then(|ws| ws.upgrade())
            .map(|w| w.read(cx).focus_handle(cx))
            .unwrap_or_else(|| cx.focus_handle());

        let window_project_groups: Vec<ProjectGroupKey> = multi_workspace
            .as_ref()
            .map(|mw| mw.read(cx).project_group_keys())
            .unwrap_or_default();

        let popover_handle = self.recent_projects_popover_handle.clone();

        PopoverMenu::new("sidebar-recent-projects-menu")
            .with_handle(popover_handle)
            .menu(move |window, cx| {
                workspace.as_ref().map(|ws| {
                    SidebarRecentProjects::popover(
                        ws.clone(),
                        window_project_groups.clone(),
                        focus_handle.clone(),
                        window,
                        cx,
                    )
                })
            })
            .trigger_with_tooltip(
                IconButton::new("open-project", IconName::FolderAdd)
                    .icon_size(IconSize::Small)
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent)),
                |_window, cx| Tooltip::for_action("Add Project", &OpenRecent::default(), cx),
            )
            .offset(gpui::Point {
                x: px(-2.0),
                y: px(-2.0),
            })
            .anchor(gpui::Anchor::BottomRight)
    }

    fn new_thread_in_group(
        &mut self,
        _: &NewThreadInGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self.selection
            && self.new_entry_in_worktree_group(ix, NewEntryTarget::LastCreatedKind, window, cx)
        {
            return;
        }

        if let Some(key) = self.selected_group_key() {
            self.set_group_expanded(&key, true, cx);
            self.selection = None;
            if let Some(workspace) = self.workspace_for_group(&key, cx) {
                self.create_new_entry(&workspace, window, cx);
            } else {
                self.open_workspace_and_create_entry(
                    &key,
                    key.path_list().clone(),
                    NewEntryTarget::LastCreatedKind,
                    window,
                    cx,
                );
            }
        } else if let Some(workspace) = self.active_workspace(cx) {
            self.create_new_entry(&workspace, window, cx);
        }
    }

    fn new_terminal_thread(
        &mut self,
        _: &NewTerminalThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();

        if let Some(ix) = self.selection
            && self.new_entry_in_worktree_group(ix, NewEntryTarget::Terminal, window, cx)
        {
            return;
        }

        if let Some(key) = self.selected_group_key() {
            self.set_group_expanded(&key, true, cx);
            self.selection = None;
            if let Some(workspace) = self.workspace_for_group(&key, cx) {
                self.create_new_terminal(&workspace, window, cx);
            } else {
                self.open_workspace_and_create_entry(
                    &key,
                    key.path_list().clone(),
                    NewEntryTarget::Terminal,
                    window,
                    cx,
                );
            }
        } else if let Some(workspace) = self.active_workspace(cx) {
            self.create_new_terminal(&workspace, window, cx);
        }
    }

    fn remove_draft(
        &mut self,
        draft_id: ThreadId,
        workspace: &ThreadEntryWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_active = self
            .active_entry
            .as_ref()
            .is_some_and(|entry| entry.is_active_thread(&draft_id));

        let neighbor = self
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(
                    entry,
                    ListEntry::Thread(thread) if thread.metadata.thread_id == draft_id
                )
            })
            .and_then(|position| self.top_entry_after_removal(position));
        self.remove_draft_entry(
            draft_id,
            workspace,
            was_active,
            neighbor.as_ref(),
            window,
            cx,
        );
    }

    fn remove_draft_entry(
        &mut self,
        draft_id: ThreadId,
        workspace: &ThreadEntryWorkspace,
        was_active: bool,
        neighbor: Option<&ListEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut was_shown = false;
        let removed_from_panel = if let ThreadEntryWorkspace::Open(workspace) = workspace {
            workspace.update(cx, |workspace, cx| {
                if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                    // Activating a fresh draft would put an identical row straight back.
                    panel.update(cx, |panel, cx| {
                        was_shown = panel.active_thread_id(cx) == Some(draft_id);
                        panel.remove_thread_without_activating_draft(draft_id, window, cx);
                    });
                    true
                } else {
                    false
                }
            })
        } else {
            false
        };

        if !removed_from_panel {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.delete(draft_id, cx);
            });
        }

        if was_active {
            self.activate_after_removal(neighbor, window, cx);
        }
        if was_shown {
            Self::close_empty_agent_panel(workspace, window, cx);
        }
        self.update_entries(cx);
    }

    fn activate_after_removal(
        &mut self,
        neighbor: Option<&ListEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_entry = None;
        let opened = match neighbor {
            Some(ListEntry::Thread(thread))
                if let ThreadEntryWorkspace::Closed {
                    folder_paths,
                    project_group_key,
                } = &thread.workspace =>
            {
                self.open_workspace_and_activate_thread(
                    thread.metadata.clone(),
                    folder_paths.clone(),
                    project_group_key,
                    window,
                    cx,
                );
                true
            }
            Some(ListEntry::Thread(thread))
                if let ThreadEntryWorkspace::Open(workspace) = &thread.workspace =>
            {
                self.activate_thread(thread.metadata.clone(), workspace, false, window, cx);
                true
            }
            Some(entry) => ActivatableEntry::from_list_entry(entry)
                .is_some_and(|entry| self.activate_entry(&entry, window, cx)),
            None => false,
        };
        if !opened {
            self.sync_active_entry_from_active_workspace(cx);
        }
    }

    /// Closes the agent panel of a workspace whose last entry was removed, so that
    /// it doesn't sit there blank. Reopening it starts a new draft.
    fn close_empty_agent_panel(
        workspace: &ThreadEntryWorkspace,
        window: &mut Window,
        cx: &mut App,
    ) {
        let ThreadEntryWorkspace::Open(workspace) = workspace else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
                return;
            };
            let is_empty = {
                let panel = panel.read(cx);
                panel.active_thread_id(cx).is_none() && panel.active_terminal_id().is_none()
            };
            if !is_empty {
                return;
            }
            let panel_id = panel.entity_id();
            for dock in workspace.all_docks() {
                dock.update(cx, |dock, cx| {
                    if dock
                        .visible_panel()
                        .is_some_and(|visible| visible.panel_id() == panel_id)
                    {
                        dock.set_open(false, window, cx);
                    }
                });
            }
        });
    }

    fn create_new_entry(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if workspace_path_list(workspace, cx).paths().is_empty() {
            return;
        }

        if self.should_create_terminal_for_workspace(workspace, cx) {
            self.create_new_terminal(workspace, window, cx);
        } else {
            self.create_new_thread(workspace, window, cx);
        }
    }

    fn should_create_terminal_for_workspace(
        &self,
        workspace: &Entity<Workspace>,
        cx: &App,
    ) -> bool {
        workspace
            .read(cx)
            .panel::<AgentPanel>(cx)
            .is_some_and(|panel| panel.read(cx).should_create_terminal_for_new_entry(cx))
    }

    fn create_new_thread(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if workspace_path_list(workspace, cx).paths().is_empty() {
            return;
        }

        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        self.expand_worktree_groups_for(workspace, cx);

        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.activate(workspace.clone(), None, window, cx);
        });

        let draft_id = workspace.update(cx, |workspace, cx| {
            let panel = workspace.panel::<AgentPanel>(cx)?;
            let draft_id = panel.update(cx, |panel, cx| {
                panel.activate_new_thread(true, AgentThreadSource::Sidebar, window, cx);
                panel.active_thread_id(cx)
            });
            workspace.focus_panel::<AgentPanel>(window, cx);
            draft_id
        });

        if let Some(draft_id) = draft_id {
            self.active_entry = Some(ActiveEntry::Thread {
                thread_id: draft_id,
                session_id: None,
                workspace: workspace.clone(),
            });
        }
    }

    fn create_new_terminal(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if workspace_path_list(workspace, cx).paths().is_empty() {
            return;
        }

        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        self.expand_worktree_groups_for(workspace, cx);

        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.activate(workspace.clone(), None, window, cx);
        });

        workspace.update(cx, |workspace, cx| {
            if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                panel.update(cx, |panel, cx| {
                    panel.new_terminal(Some(workspace), AgentThreadSource::Sidebar, window, cx);
                });
            }
            workspace.focus_panel::<AgentPanel>(window, cx);
        });
    }

    fn selected_group_key(&self) -> Option<ProjectGroupKey> {
        let ix = self.selection?;
        match self.contents.entries.get(ix) {
            Some(ListEntry::ProjectHeader { key, .. }) => Some(key.clone()),
            Some(
                ListEntry::Thread(_) | ListEntry::Terminal(_) | ListEntry::WorktreeHeader { .. },
            ) => (0..ix)
                .rev()
                .find_map(|i| match self.contents.entries.get(i) {
                    Some(ListEntry::ProjectHeader { key, .. }) => Some(key.clone()),
                    _ => None,
                }),
            _ => None,
        }
    }

    /// Return the active workspace when it belongs to the group
    /// Otherwise, return the workspace for project path.
    fn workspace_for_group(&self, key: &ProjectGroupKey, cx: &App) -> Option<Entity<Workspace>> {
        let mw = self.multi_workspace.upgrade()?;
        let mw = mw.read(cx);
        let active = mw.workspace().clone();
        let active_key = active.read(cx).project_group_key(cx);
        if active_key == *key {
            Some(active)
        } else {
            mw.workspace_for_paths(key.path_list(), key.host(), cx)
        }
    }

    /// Activates or opens a workspace for the group, preferring its last active one.
    pub(crate) fn activate_or_open_workspace_for_group(
        &mut self,
        key: &ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self
            .multi_workspace
            .upgrade()
            .and_then(|mw| mw.read(cx).last_active_workspace_for_group(key, cx))
            .or_else(|| self.workspace_for_group(key, cx));
        if let Some(workspace) = workspace {
            if self.is_active_workspace(&workspace, cx) {
                return;
            }
            self.activate_workspace(&workspace, window, cx);
        } else {
            self.open_workspace_for_group(key, window, cx);
        }
        self.selection = None;
        self.active_entry = None;
    }

    fn active_project_group_key(&self, cx: &App) -> Option<ProjectGroupKey> {
        let multi_workspace = self.multi_workspace.upgrade()?;
        let multi_workspace = multi_workspace.read(cx);
        Some(multi_workspace.project_group_key_for_workspace(multi_workspace.workspace(), cx))
    }

    fn active_project_header_position(&self, cx: &App) -> Option<usize> {
        let active_key = self.active_project_group_key(cx)?;
        self.contents
            .project_header_indices
            .iter()
            .position(|&entry_ix| {
                matches!(
                    &self.contents.entries[entry_ix],
                    ListEntry::ProjectHeader { key, .. } if *key == active_key
                )
            })
    }

    fn cycle_project_impl(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let header_count = self.contents.project_header_indices.len();
        if header_count == 0 {
            return;
        }

        let current_pos = self.active_project_header_position(cx);

        let next_pos = match current_pos {
            Some(pos) => {
                if forward {
                    (pos + 1) % header_count
                } else {
                    (pos + header_count - 1) % header_count
                }
            }
            None => 0,
        };

        let header_entry_ix = self.contents.project_header_indices[next_pos];
        let Some(ListEntry::ProjectHeader { key, .. }) = self.contents.entries.get(header_entry_ix)
        else {
            return;
        };
        let key = key.clone();

        // Uncollapse the target group so that threads become visible.
        self.set_group_expanded(&key, true, cx);

        if let Some(workspace) = self.multi_workspace.upgrade().and_then(|mw| {
            mw.read(cx)
                .workspace_for_paths(key.path_list(), key.host(), cx)
        }) {
            multi_workspace.update(cx, |multi_workspace, cx| {
                multi_workspace.activate(workspace, None, window, cx);
                multi_workspace.retain_active_workspace(cx);
            });
        } else {
            self.open_workspace_for_group(&key, window, cx);
        }
    }

    fn on_next_project(&mut self, _: &NextProject, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_project_impl(true, window, cx);
    }

    fn on_previous_project(
        &mut self,
        _: &PreviousProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_project_impl(false, window, cx);
    }

    fn cycle_thread_impl(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let thread_indices: Vec<usize> = self
            .contents
            .entries
            .iter()
            .enumerate()
            .filter_map(|(ix, entry)| match entry {
                ListEntry::Thread(_) | ListEntry::Terminal(_) => Some(ix),
                _ => None,
            })
            .collect();

        if thread_indices.is_empty() {
            return;
        }

        let current_thread_pos = self.active_entry.as_ref().and_then(|active| {
            thread_indices
                .iter()
                .position(|&ix| active.matches_entry(&self.contents.entries[ix]))
        });

        let next_pos = match current_thread_pos {
            Some(pos) => {
                let count = thread_indices.len();
                if forward {
                    (pos + 1) % count
                } else {
                    (pos + count - 1) % count
                }
            }
            None => 0,
        };

        let entry_ix = thread_indices[next_pos];
        match &self.contents.entries[entry_ix] {
            ListEntry::Thread(thread) => {
                let metadata = thread.metadata.clone();
                match &thread.workspace {
                    ThreadEntryWorkspace::Open(workspace) => {
                        let workspace = workspace.clone();
                        self.activate_thread(metadata, &workspace, true, window, cx);
                    }
                    ThreadEntryWorkspace::Closed {
                        folder_paths,
                        project_group_key,
                    } => {
                        let folder_paths = folder_paths.clone();
                        let project_group_key = project_group_key.clone();
                        self.open_workspace_and_activate_thread(
                            metadata,
                            folder_paths,
                            &project_group_key,
                            window,
                            cx,
                        );
                    }
                }
            }
            ListEntry::Terminal(terminal) => {
                let metadata = terminal.metadata.clone();
                let workspace = terminal.workspace.clone();
                self.activate_terminal_entry(metadata, workspace, true, window, cx);
            }
            ListEntry::ProjectHeader { .. } | ListEntry::WorktreeHeader { .. } => {}
        }
    }

    fn on_next_thread(&mut self, _: &NextThread, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_thread_impl(true, window, cx);
    }

    fn on_previous_thread(
        &mut self,
        _: &PreviousThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_thread_impl(false, window, cx);
    }

    fn render_no_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_query = self.has_filter_query(cx);
        let message = if has_query {
            "No threads match your search."
        } else {
            "No threads yet"
        };

        v_flex()
            .id("sidebar-no-results")
            .p_4()
            .size_full()
            .items_center()
            .justify_center()
            .child(
                Label::new(message)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }

    fn initial_scan_pending(&self, cx: &App) -> bool {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return false;
        };
        multi_workspace.read(cx).workspaces().any(|workspace| {
            let project = workspace.read(cx).project().read(cx);
            !project.is_via_collab() && !project.worktree_store().read(cx).initial_scan_completed()
        })
    }

    fn render_loading_state(&self) -> impl IntoElement {
        v_flex().flex_1().items_center().justify_center().child(
            Label::new("Loading…").color(Color::Muted).with_animation(
                "loading-sidebar-label",
                gpui::Animation::new(std::time::Duration::from_secs(2))
                    .repeat()
                    .with_easing(gpui::pulsating_between(0.3, 0.7)),
                |label, delta| label.alpha(delta),
            ),
        )
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        ProjectEmptyState::new(
            "Threads Sidebar",
            self.focus_handle(cx),
            KeyBinding::for_action(&workspace::Open::default(), cx),
        )
        .on_open_project(|_, window, cx| {
            let side = match AgentSettings::get_global(cx).sidebar_side() {
                SidebarSide::Left => "left",
                SidebarSide::Right => "right",
            };
            telemetry::event!("Sidebar Add Project Clicked", side = side);
            window.dispatch_action(
                Open {
                    create_new_window: Some(false),
                }
                .boxed_clone(),
                cx,
            );
        })
        .on_clone_repo(|_, window, cx| {
            window.dispatch_action(git::Clone.boxed_clone(), cx);
        })
    }

    fn render_sidebar_header(
        &self,
        no_open_projects: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let has_query = self.has_filter_query(cx);
        let SidebarWindowControls {
            traffic_lights,
            left: left_window_controls,
            right: right_window_controls,
        } = SidebarWindowControls::new(self.side(cx), window, cx);
        let title_bar_visible = TitleBarSettings::is_title_bar_visible(window, cx);
        let header_height = if title_bar_visible {
            platform_title_bar_height(window)
        } else {
            Tab::container_height(cx)
        };

        h_flex()
            .debug_selector(|| "sidebar-header".into())
            .h(header_height)
            .map(|header| {
                if !title_bar_visible {
                    return header.mt_px();
                }
                match window.window_decorations() {
                    // Without projects there's no bottom border to match the title bar's.
                    Decorations::Client { .. } => apply_title_bar_insets(
                        header,
                        left_window_controls,
                        right_window_controls,
                        no_open_projects,
                    ),
                    Decorations::Server => header.mt_px().pb_px(),
                }
            })
            .when(left_window_controls, |this| {
                this.children(Self::render_left_window_controls(window, cx))
            })
            .map(|this| {
                if traffic_lights {
                    this.pl(px(ui::utils::TRAFFIC_LIGHT_PADDING))
                } else if !left_window_controls {
                    this.pl_1p5()
                } else {
                    this
                }
            })
            .when(!right_window_controls, |this| this.pr_1p5())
            .gap_1()
            .when(!no_open_projects, |this| {
                this.border_b_1()
                    .border_color(cx.theme().colors().border)
                    .when(traffic_lights, |this| {
                        this.child(Divider::vertical().color(ui::DividerColor::Border))
                    })
                    .child(
                        div()
                            .debug_selector(|| "sidebar-header-search".into())
                            .ml_1()
                            .child(
                                Icon::new(IconName::MagnifyingGlass)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(self.render_filter_input(cx))
                    .child(
                        h_flex()
                            .gap_1()
                            .when(
                                self.selection.is_some()
                                    && !self.filter_editor.focus_handle(cx).is_focused(window),
                                |this| this.child(KeyBinding::for_action(&FocusSidebarFilter, cx)),
                            )
                            .when(has_query, |this| {
                                this.child(
                                    IconButton::new("clear_filter", IconName::Close)
                                        .icon_size(IconSize::Small)
                                        .tooltip(Tooltip::text("Clear Search"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.reset_filter_editor_text(window, cx);
                                            this.update_entries(cx);
                                        })),
                                )
                            }),
                    )
            })
            .when(right_window_controls, |this| {
                this
                    // Without projects, nothing flex-grows to push right-side window controls
                    // to the edge.
                    .when(no_open_projects, |this| this.justify_end())
                    .children(Self::render_right_window_controls(window, cx))
            })
    }

    fn render_left_window_controls(window: &Window, cx: &mut App) -> Option<AnyElement> {
        platform_title_bar::render_left_window_controls(
            cx.button_layout(),
            Box::new(CloseWindow),
            window,
        )
    }

    fn render_right_window_controls(window: &Window, cx: &mut App) -> Option<AnyElement> {
        platform_title_bar::render_right_window_controls(
            cx.button_layout(),
            Box::new(CloseWindow),
            window,
        )
    }

    fn render_sidebar_toggle_button(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let on_right = AgentSettings::get_global(_cx).sidebar_side() == SidebarSide::Right;

        sidebar_side_context_menu("sidebar-toggle-menu", _cx)
            .anchor(if on_right {
                gpui::Anchor::BottomRight
            } else {
                gpui::Anchor::BottomLeft
            })
            .attach(if on_right {
                gpui::Anchor::TopRight
            } else {
                gpui::Anchor::TopLeft
            })
            .trigger(move |_is_active, _window, _cx| {
                let icon = if on_right {
                    IconName::ThreadsSidebarRightOpen
                } else {
                    IconName::ThreadsSidebarLeftOpen
                };
                IconButton::new("sidebar-close-toggle", icon)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::element(move |_window, cx| {
                        v_flex()
                            .gap_1()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .justify_between()
                                    .child(Label::new("Toggle Sidebar"))
                                    .child(KeyBinding::for_action(&ToggleWorkspaceSidebar, cx)),
                            )
                            .child(
                                h_flex()
                                    .pt_1()
                                    .gap_2()
                                    .border_t_1()
                                    .border_color(cx.theme().colors().border_variant)
                                    .justify_between()
                                    .child(Label::new("Focus Sidebar"))
                                    .child(KeyBinding::for_action(&FocusWorkspaceSidebar, cx)),
                            )
                            .into_any_element()
                    }))
                    .on_click(|_, window, cx| {
                        if let Some(multi_workspace) = window.root::<MultiWorkspace>().flatten() {
                            multi_workspace.update(cx, |multi_workspace, cx| {
                                multi_workspace.close_sidebar(window, cx);
                            });
                        }
                    })
            })
    }

    fn render_sidebar_bottom_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_archive = matches!(self.view, SidebarView::Archive(..));
        let on_right = self.side(cx) == SidebarSide::Right;

        h_flex()
            .p_1()
            .gap_1()
            .when(on_right, |this| this.flex_row_reverse())
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(self.render_sidebar_toggle_button(cx))
            .child(
                IconButton::new("history", IconName::Clock)
                    .icon_size(IconSize::Small)
                    .toggle_state(is_archive)
                    .tooltip(move |_, cx| {
                        let label = if is_archive {
                            "Hide Thread History"
                        } else {
                            "Show Thread History"
                        };
                        Tooltip::for_action(label, &ToggleThreadHistory, cx)
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_archive(&ToggleThreadHistory, window, cx);
                    })),
            )
            .child(div().flex_1())
            .child(self.render_recent_projects_button(cx))
    }

    fn active_workspace(&self, cx: &App) -> Option<Entity<Workspace>> {
        self.multi_workspace
            .upgrade()
            .map(|w| w.read(cx).workspace().clone())
    }

    fn show_thread_import_modal(
        &mut self,
        source: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        telemetry::event!(
            "Agent Threads Import Clicked",
            source = source,
            side = match self.side(cx) {
                SidebarSide::Left => "left",
                SidebarSide::Right => "right",
            }
        );

        let Some(active_workspace) = self.active_workspace(cx) else {
            return;
        };

        let Some(agent_registry_store) = AgentRegistryStore::try_global(cx) else {
            return;
        };

        let agent_server_store = active_workspace
            .read(cx)
            .project()
            .read(cx)
            .agent_server_store()
            .clone();

        let workspace_handle = active_workspace.downgrade();
        let multi_workspace = self.multi_workspace.clone();

        active_workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                ThreadImportModal::new(
                    agent_server_store,
                    agent_registry_store,
                    workspace_handle.clone(),
                    multi_workspace.clone(),
                    window,
                    cx,
                )
            });
        });
    }

    fn should_render_acp_import_onboarding(&self, cx: &App) -> bool {
        let has_external_agents = self
            .active_workspace(cx)
            .map(|ws| {
                ws.read(cx)
                    .project()
                    .read(cx)
                    .agent_server_store()
                    .read(cx)
                    .has_external_agents()
            })
            .unwrap_or(false);

        has_external_agents && !AcpThreadImportOnboarding::dismissed(cx)
    }

    fn render_acp_import_onboarding(
        &mut self,
        verbose_labels: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let on_import = cx.listener(|this, _, window, cx| {
            this.show_archive(window, cx);
            this.show_thread_import_modal("external_agent_onboarding", window, cx);
        });
        render_import_onboarding_banner(
            "acp",
            "Looking for threads from external agents?",
            "Import threads from agents like Claude Agent, Codex, and more, whether started in Zed or another client.",
            if verbose_labels {
                "Import Threads from External Agents"
            } else {
                "Import Threads"
            },
            |_, _window, cx| AcpThreadImportOnboarding::dismiss(cx),
            on_import,
            cx,
        )
    }

    fn should_render_cross_channel_import_onboarding(&self, cx: &App) -> bool {
        !CrossChannelImportOnboarding::dismissed(cx)
            && !self.cross_channel_import_channels.is_empty()
    }

    fn render_cross_channel_import_onboarding(
        &mut self,
        verbose_labels: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let channel_names = self
            .cross_channel_import_channels
            .iter()
            .map(SharedString::as_str)
            .join(" and ");

        let description = format!(
            "Import threads from {} to continue where you left off.",
            channel_names
        );

        let on_import = cx.listener(|this, _, _window, cx| {
            telemetry::event!(
                "Agent Threads Import Clicked",
                source = "cross_channel_onboarding",
                side = match this.side(cx) {
                    SidebarSide::Left => "left",
                    SidebarSide::Right => "right",
                }
            );
            CrossChannelImportOnboarding::dismiss(cx);
            if let Some(workspace) = this.active_workspace(cx) {
                workspace.update(cx, |workspace, cx| {
                    import_threads_from_other_channels(workspace, cx);
                });
            }
        });
        render_import_onboarding_banner(
            "channel",
            "Threads found from other channels",
            description,
            if verbose_labels {
                "Import Threads from Other Channels"
            } else {
                "Import Threads"
            },
            |_, _window, cx| CrossChannelImportOnboarding::dismiss(cx),
            on_import,
            cx,
        )
    }

    fn toggle_archive(
        &mut self,
        _: &ToggleThreadHistory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &self.view {
            SidebarView::ThreadList => {
                self.show_archive(window, cx);
            }
            SidebarView::Archive(_) => self.show_thread_list(window, cx),
        }
    }

    fn show_archive(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let side = match self.side(cx) {
            SidebarSide::Left => "left",
            SidebarSide::Right => "right",
        };
        telemetry::event!("Thread History Viewed", side = side);

        let Some(active_workspace) = self
            .multi_workspace
            .upgrade()
            .map(|w| w.read(cx).workspace().clone())
        else {
            return;
        };
        let Some(agent_panel) = active_workspace.read(cx).panel::<AgentPanel>(cx) else {
            return;
        };

        let agent_server_store = active_workspace
            .read(cx)
            .project()
            .read(cx)
            .agent_server_store()
            .downgrade();

        let agent_connection_store = agent_panel.read(cx).connection_store().downgrade();

        let archive_view = cx.new(|cx| {
            ThreadsArchiveView::new(
                active_workspace.downgrade(),
                agent_connection_store.clone(),
                agent_server_store.clone(),
                window,
                cx,
            )
        });

        let subscription = cx.subscribe_in(
            &archive_view,
            window,
            |this, _, event: &ThreadsArchiveViewEvent, window, cx| match event {
                ThreadsArchiveViewEvent::Close => {
                    this.show_thread_list(window, cx);
                }
                ThreadsArchiveViewEvent::Activate { thread } => {
                    this.open_thread_from_archive(thread.clone(), window, cx);
                }
                ThreadsArchiveViewEvent::CancelRestore { thread_id } => {
                    this.restoring_tasks.remove(thread_id);
                }
                ThreadsArchiveViewEvent::Import => {
                    this.show_thread_import_modal("thread_history", window, cx);
                }
                ThreadsArchiveViewEvent::NewThread => {
                    this.show_thread_list(window, cx);
                    if let Some(workspace) = this.active_workspace(cx) {
                        this.create_new_entry(&workspace, window, cx);
                    }
                }
            },
        );

        self._subscriptions.push(subscription);
        self.view = SidebarView::Archive(archive_view.clone());
        archive_view.update(cx, |view, cx| view.focus_filter_editor(window, cx));
        self.serialize(cx);
        cx.notify();
    }

    fn show_thread_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.view = SidebarView::ThreadList;
        self._subscriptions.clear();
        let handle = self.filter_editor.read(cx).focus_handle(cx);
        handle.focus(window, cx);
        self.serialize(cx);
        cx.notify();
    }
}

fn render_import_onboarding_banner(
    id: impl Into<SharedString>,
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    button_label: impl Into<SharedString>,
    on_dismiss: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_import: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let id: SharedString = id.into();
    let bg = cx.theme().colors().text_accent;

    v_flex()
        .min_w_0()
        .w_full()
        .p_2()
        .border_t_1()
        .border_color(cx.theme().colors().border)
        .bg(linear_gradient(
            360.,
            linear_color_stop(bg.opacity(0.06), 1.),
            linear_color_stop(bg.opacity(0.), 0.),
        ))
        .child(
            h_flex()
                .min_w_0()
                .w_full()
                .gap_1()
                .items_start()
                .justify_between()
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .child(Label::new(title).size(LabelSize::Small)),
                )
                .child(
                    IconButton::new(
                        SharedString::from(format!("close-{id}-onboarding")),
                        IconName::Close,
                    )
                    .icon_size(IconSize::Small)
                    .on_click(on_dismiss),
                ),
        )
        .child(
            Label::new(description)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .mb_2(),
        )
        .child(
            Button::new(SharedString::from(format!("import-{id}")), button_label)
                .full_width()
                .style(ButtonStyle::OutlinedCustom(cx.theme().colors().border))
                .label_size(LabelSize::Small)
                .start_icon(
                    Icon::new(IconName::Download)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .on_click(on_import),
        )
}

impl WorkspaceSidebar for Sidebar {
    fn width(&self, _cx: &App) -> Pixels {
        self.width
    }

    fn set_width(&mut self, width: Option<Pixels>, cx: &mut Context<Self>) {
        // `None` is the reset gesture, which hands the width back to the setting.
        self.width_set_by_user = width.is_some();
        self.width = width
            .unwrap_or_else(|| AgentSettings::get_global(cx).threads_sidebar.default_width)
            .clamp(THREADS_LIST_MIN_WIDTH, THREADS_LIST_MAX_WIDTH);
        cx.notify();
    }

    fn has_notifications(&self, _cx: &App) -> bool {
        !self.contents.notified_threads.is_empty() || !self.contents.notified_terminals.is_empty()
    }

    fn is_threads_list_view_active(&self) -> bool {
        matches!(self.view, SidebarView::ThreadList)
    }

    fn side(&self, cx: &App) -> SidebarSide {
        AgentSettings::get_global(cx).sidebar_side()
    }

    fn prepare_for_focus(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.selection = None;
        cx.notify();
    }

    fn toggle_thread_switcher(
        &mut self,
        select_last: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_thread_switcher_impl(select_last, window, cx);
    }

    fn cycle_project(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_project_impl(forward, window, cx);
    }

    fn cycle_thread(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_thread_impl(forward, window, cx);
    }

    fn serialized_state(&self, _cx: &App) -> Option<String> {
        let serialized = SerializedSidebar {
            width: self.width_set_by_user.then(|| f32::from(self.width)),
            width_set_by_user: self.width_set_by_user,
            active_view: match self.view {
                SidebarView::ThreadList => SerializedSidebarView::ThreadList,
                SidebarView::Archive(_) => SerializedSidebarView::History,
            },
            worktree_group_order: self
                .worktree_group_order
                .iter()
                .map(ToString::to_string)
                .collect(),
            collapsed_worktree_groups: self
                .collapsed_worktree_groups
                .iter()
                .map(ToString::to_string)
                .collect(),
            multi_root_worktree_groups: self
                .multi_root_worktree_groups
                .iter()
                .filter(|(key, _)| key.host().is_none())
                .map(|(_, paths)| {
                    let (main_paths, folder_paths) = paths
                        .ordered_pairs()
                        .map(|(main_path, folder_path)| (main_path.clone(), folder_path.clone()))
                        .unzip();
                    SerializedWorktreeGroup {
                        main_paths,
                        folder_paths,
                    }
                })
                .collect(),
        };
        serde_json::to_string(&serialized).ok()
    }

    fn restore_serialized_state(
        &mut self,
        state: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(serialized) = serde_json::from_str::<SerializedSidebar>(state).log_err() {
            // Legacy widths other than the old 300-pixel default imply a manual resize
            if let Some(width) = serialized
                .width
                .filter(|width| serialized.width_set_by_user || *width != 300.0)
            {
                self.width = px(width).clamp(THREADS_LIST_MIN_WIDTH, THREADS_LIST_MAX_WIDTH);
                self.width_set_by_user = true;
            }
            self.worktree_group_order = serialized
                .worktree_group_order
                .into_iter()
                .map(SharedString::from)
                .collect();
            self.collapsed_worktree_groups = serialized
                .collapsed_worktree_groups
                .into_iter()
                .map(SharedString::from)
                .collect();
            self.multi_root_worktree_groups = serialized
                .multi_root_worktree_groups
                .into_iter()
                .filter_map(|group| {
                    let paths = WorktreePaths::from_path_lists(
                        PathList::new(&group.main_paths),
                        PathList::new(&group.folder_paths),
                    )
                    .log_err()?;
                    Some((ProjectGroupKey::from_worktree_paths(&paths, None), paths))
                })
                .collect();
            self.multi_root_worktree_groups_unchecked = true;
            if serialized.active_view == SerializedSidebarView::History {
                cx.defer_in(window, |this, window, cx| {
                    this.show_archive(window, cx);
                });
            }
            // The caller may be updating the `MultiWorkspace`, which a rebuild reads.
            self.schedule_update_entries(false, cx);
        }
        cx.notify();
    }
}

impl gpui::EventEmitter<workspace::SidebarEvent> for Sidebar {}

impl Focusable for Sidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _titlebar_height = ui::utils::platform_title_bar_height(window);
        let ui_font = theme_settings::setup_ui_font(window, cx);
        // A drag that ends without a drop here leaves the target behind.
        if !cx.has_active_drag() {
            self.project_drop_target = None;
            self.worktree_drop_target = None;
        }
        let sticky_header = self.render_sticky_header(window, cx);

        let color = cx.theme().colors();
        let bg = color.background.blend(color.panel_background);

        let no_open_projects = !self.contents.has_open_projects;
        let no_search_results = self.contents.entries.is_empty();
        let initial_scan_pending = !self.initial_scan_shown && self.initial_scan_pending(cx);
        // Only the first scan gates the list; projects opened later appear in place.
        if !initial_scan_pending && !no_open_projects {
            self.initial_scan_shown = true;
        }

        v_flex()
            .id("workspace-sidebar")
            .key_context(self.dispatch_context(window, cx))
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(|this, _: &MoveProjectUp, _window, cx| {
                if !this.move_selected_worktree_group(-1, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &MoveProjectDown, _window, cx| {
                if !this.move_selected_worktree_group(1, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(Self::editor_move_down))
            .on_action(cx.listener(Self::editor_move_up))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::expand_selected_entry))
            .on_action(cx.listener(Self::collapse_selected_entry))
            .on_action(cx.listener(Self::toggle_selected_fold))
            .on_action(cx.listener(Self::fold_all))
            .on_action(cx.listener(Self::unfold_all))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::archive_selected_thread))
            .on_action(cx.listener(Self::rename_selected_thread))
            .on_action(cx.listener(Self::new_thread_in_group))
            .on_action(cx.listener(Self::new_terminal_thread))
            .on_action(cx.listener(Self::toggle_archive))
            .on_action(cx.listener(Self::focus_sidebar_filter))
            .on_action(cx.listener(Self::on_toggle_thread_switcher))
            .on_action(cx.listener(Self::on_next_project))
            .on_action(cx.listener(Self::on_previous_project))
            .on_action(cx.listener(Self::on_next_thread))
            .on_action(cx.listener(Self::on_previous_thread))
            .on_action(cx.listener(|this, _: &OpenRecent, window, cx| {
                this.recent_projects_popover_handle.toggle(window, cx);
            }))
            .font(ui_font)
            .map(|el| {
                let on_left = self.side(cx) == SidebarSide::Left;
                match window.window_decorations() {
                    Decorations::Server => el.h_full().w(self.width),
                    // With client-side decorations the sidebar owns the window
                    // corners on its side, so round them like the title bar and
                    // status bar do. The sidebar is stretched 1px outwards over
                    // the window border on untiled edges (with compensating
                    // padding) so its rounded background lines up exactly with
                    // the window shape, avoiding a transparent gap in the
                    // rounded corners.
                    Decorations::Client { tiling, .. } => el
                        .absolute()
                        .top(if tiling.top { px(0.) } else { px(-1.) })
                        .bottom(if tiling.bottom { px(0.) } else { px(-1.) })
                        .when(!tiling.top, |el| el.pt_px())
                        .when(!tiling.bottom, |el| el.pb_px())
                        .map(|el| {
                            if on_left {
                                el.right(px(0.))
                                    .left(if tiling.left { px(0.) } else { px(-1.) })
                                    .when(!tiling.left, |el| el.pl(px(1.)))
                            } else {
                                el.left(px(0.))
                                    .right(if tiling.right { px(0.) } else { px(-1.) })
                                    .when(!tiling.right, |el| el.pr(px(1.)))
                            }
                        })
                        .when(on_left && !(tiling.top || tiling.left), |el| {
                            el.rounded_tl(CLIENT_SIDE_DECORATION_ROUNDING)
                        })
                        .when(on_left && !(tiling.bottom || tiling.left), |el| {
                            el.rounded_bl(CLIENT_SIDE_DECORATION_ROUNDING)
                        })
                        .when(!on_left && !(tiling.top || tiling.right), |el| {
                            el.rounded_tr(CLIENT_SIDE_DECORATION_ROUNDING)
                        })
                        .when(!on_left && !(tiling.bottom || tiling.right), |el| {
                            el.rounded_br(CLIENT_SIDE_DECORATION_ROUNDING)
                        }),
                }
            })
            .bg(bg)
            .when(self.side(cx) == SidebarSide::Left, |el| el.border_r_1())
            .when(self.side(cx) == SidebarSide::Right, |el| el.border_l_1())
            .border_color(color.border)
            .map(|this| match &self.view {
                SidebarView::ThreadList => this
                    .child(self.render_sidebar_header(no_open_projects, window, cx))
                    .map(|this| {
                        if initial_scan_pending {
                            this.child(self.render_loading_state())
                        } else if no_open_projects {
                            this.child(self.render_empty_state(cx))
                        } else {
                            this.child(
                                v_flex()
                                    .relative()
                                    .flex_1()
                                    .overflow_hidden()
                                    .on_drag_move(cx.listener(
                                        |this,
                                         event: &DragMoveEvent<DraggedProjectHeader>,
                                         _window,
                                         cx| {
                                            let dragged_key = event.drag(cx).key.clone();
                                            let position = event.event.position;
                                            this.update_project_drop_target(
                                                &dragged_key,
                                                event
                                                    .bounds
                                                    .contains(&position)
                                                    .then_some(position),
                                                cx,
                                            );
                                        },
                                    ))
                                    .on_drag_move(cx.listener(
                                        |this,
                                         event: &DragMoveEvent<DraggedWorktreeHeader>,
                                         _window,
                                         cx| {
                                            let dragged_id = event.drag(cx).id.clone();
                                            let position = event.event.position;
                                            this.update_worktree_drop_target(
                                                &dragged_id,
                                                event
                                                    .bounds
                                                    .contains(&position)
                                                    .then_some(position),
                                                cx,
                                            );
                                        },
                                    ))
                                    .child(
                                        list(
                                            self.list_state.clone(),
                                            cx.processor(Self::render_list_entry),
                                        )
                                        .flex_1()
                                        .size_full(),
                                    )
                                    .when(no_search_results, |this| {
                                        this.child(self.render_no_results(cx))
                                    })
                                    .when_some(sticky_header, |this, header| this.child(header))
                                    .custom_scrollbars(
                                        Scrollbars::new(ScrollAxes::Vertical)
                                            .tracked_scroll_handle(&self.list_state),
                                        window,
                                        cx,
                                    ),
                            )
                        }
                    }),
                SidebarView::Archive(archive_view) => this.child(archive_view.clone()),
            })
            .map(|this| {
                let show_acp = self.should_render_acp_import_onboarding(cx);
                let show_cross_channel = self.should_render_cross_channel_import_onboarding(cx);

                let verbose = *self
                    .import_banners_use_verbose_labels
                    .get_or_insert(show_acp && show_cross_channel);

                this.when(show_acp, |this| {
                    this.child(self.render_acp_import_onboarding(verbose, cx))
                })
                .when(show_cross_channel, |this| {
                    this.child(self.render_cross_channel_import_onboarding(verbose, cx))
                })
            })
            .child(self.render_sidebar_bottom_bar(cx))
    }
}

fn all_thread_infos_for_workspace(
    workspace: &Entity<Workspace>,
    cx: &App,
) -> impl Iterator<Item = ActiveThreadInfo> {
    let Some(agent_panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
        return None.into_iter().flatten();
    };
    let agent_panel = agent_panel.read(cx);
    let threads = agent_panel
        .conversation_views()
        .into_iter()
        .filter_map(|conversation_view| {
            let is_waiting_for_input = conversation_view
                .read(cx)
                .root_thread_is_waiting_for_input(cx);
            let conversation_thread_id = conversation_view.read(cx).parent_id();
            let thread_view = conversation_view.read(cx).root_thread_view()?;
            let thread_view_ref = thread_view.read(cx);
            let thread = thread_view_ref.thread.read(cx);

            let icon = thread_view_ref.agent_icon;
            let icon_from_external_svg = thread_view_ref.agent_icon_from_external_svg.clone();
            let title = thread
                .title()
                .unwrap_or_else(|| DEFAULT_THREAD_TITLE.into());
            let is_title_generating = thread_view_ref
                .as_native_thread(cx)
                .is_some_and(|native_thread| native_thread.read(cx).is_generating_title());
            let session_id = thread.session_id().clone();
            let is_background = agent_panel.is_retained_thread(&conversation_thread_id);

            let status = if is_waiting_for_input {
                AgentThreadStatus::WaitingForConfirmation
            } else if thread.had_error() {
                AgentThreadStatus::Error
            } else {
                match thread.status() {
                    ThreadStatus::Unconfirmed => AgentThreadStatus::Unconfirmed,
                    ThreadStatus::Generating => AgentThreadStatus::Running,
                    ThreadStatus::Idle => AgentThreadStatus::Completed,
                }
            };

            let diff_stats = thread.action_log().read(cx).diff_stats(cx);

            Some(ActiveThreadInfo {
                session_id,
                title,
                status,
                icon,
                icon_from_external_svg,
                is_background,
                is_title_generating,
                diff_stats,
            })
        });

    Some(threads).into_iter().flatten()
}

pub fn dump_workspace_info(
    workspace: &mut Workspace,
    _: &DumpWorkspaceInfo,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    use std::fmt::Write;

    let mut output = String::new();
    let this_entity = cx.entity();

    let multi_workspace = workspace.multi_workspace().and_then(|weak| weak.upgrade());
    let workspaces: Vec<gpui::Entity<Workspace>> = match &multi_workspace {
        Some(mw) => mw.read(cx).workspaces().cloned().collect(),
        None => vec![this_entity.clone()],
    };
    let active_workspace = multi_workspace
        .as_ref()
        .map(|mw| mw.read(cx).workspace().clone());

    writeln!(output, "MultiWorkspace: {} workspace(s)", workspaces.len()).ok();

    if let Some(mw) = &multi_workspace {
        let keys: Vec<_> = mw.read(cx).project_group_keys();
        writeln!(output, "Project group keys ({}):", keys.len()).ok();
        for key in keys {
            writeln!(output, "  - {key:?}").ok();
        }
    }

    writeln!(output).ok();

    for (index, ws) in workspaces.iter().enumerate() {
        let is_active = active_workspace.as_ref() == Some(ws);
        writeln!(
            output,
            "--- Workspace {index}{} ---",
            if is_active { " (active)" } else { "" }
        )
        .ok();

        // project_group_key_for_workspace internally reads the workspace,
        // so we can only call it for workspaces other than this_entity
        // (which is already being updated).
        if let Some(mw) = &multi_workspace {
            if *ws == this_entity {
                let workspace_key = workspace.project_group_key(cx);
                writeln!(output, "ProjectGroupKey: {workspace_key:?}").ok();
            } else {
                let effective_key = mw.read(cx).project_group_key_for_workspace(ws, cx);
                let workspace_key = ws.read(cx).project_group_key(cx);
                if effective_key != workspace_key {
                    writeln!(
                        output,
                        "ProjectGroupKey (multi_workspace): {effective_key:?}"
                    )
                    .ok();
                    writeln!(
                        output,
                        "ProjectGroupKey (workspace, DISAGREES): {workspace_key:?}"
                    )
                    .ok();
                } else {
                    writeln!(output, "ProjectGroupKey: {effective_key:?}").ok();
                }
            }
        } else {
            let workspace_key = workspace.project_group_key(cx);
            writeln!(output, "ProjectGroupKey: {workspace_key:?}").ok();
        }

        // The action handler is already inside an update on `this_entity`,
        // so we must avoid a nested read/update on that same entity.
        if *ws == this_entity {
            dump_single_workspace(workspace, &mut output, cx);
        } else {
            ws.read_with(cx, |ws, cx| {
                dump_single_workspace(ws, &mut output, cx);
            });
        }
    }

    let project = workspace.project().clone();
    cx.spawn_in(window, async move |_this, cx| {
        let buffer = project
            .update(cx, |project, cx| project.create_buffer(None, false, cx))
            .await?;

        buffer.update(cx, |buffer, cx| {
            buffer.set_text(output, cx);
        });

        let buffer = cx.new(|cx| {
            editor::MultiBuffer::singleton(buffer, cx).with_title("Workspace Info".into())
        });

        _this.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(
                Box::new(cx.new(|cx| {
                    let mut editor =
                        editor::Editor::for_multibuffer(buffer, Some(project.clone()), window, cx);
                    editor.set_read_only(true);
                    editor.set_should_serialize(false, cx);
                    editor.set_breadcrumb_header("Workspace Info".into());
                    editor
                })),
                None,
                true,
                window,
                cx,
            );
        })
    })
    .detach_and_log_err(cx);
}

fn dump_single_workspace(workspace: &Workspace, output: &mut String, cx: &gpui::App) {
    use std::fmt::Write;

    let workspace_db_id = workspace.database_id();
    match workspace_db_id {
        Some(id) => writeln!(output, "Workspace DB ID: {id:?}").ok(),
        None => writeln!(output, "Workspace DB ID: (none)").ok(),
    };

    let project = workspace.project().read(cx);

    let repos: Vec<_> = project
        .repositories(cx)
        .values()
        .map(|repo| repo.read(cx).snapshot())
        .collect();

    writeln!(output, "Worktrees:").ok();
    for worktree in project.worktrees(cx) {
        let worktree = worktree.read(cx);
        let abs_path = worktree.abs_path();
        let visible = worktree.is_visible();

        let repo_info = repos
            .iter()
            .find(|snapshot| abs_path.starts_with(&*snapshot.work_directory_abs_path));

        let is_linked = repo_info.map(|s| s.is_linked_worktree()).unwrap_or(false);
        let main_worktree_path = repo_info.and_then(|s| s.main_worktree_abs_path());
        let branch = repo_info.and_then(|s| s.branch.as_ref().map(|b| b.ref_name.clone()));

        write!(output, "  - {}", abs_path.display()).ok();
        if !visible {
            write!(output, " (hidden)").ok();
        }
        if let Some(branch) = &branch {
            write!(output, " [branch: {branch}]").ok();
        }
        if is_linked {
            if let Some(main_worktree_path) = main_worktree_path {
                write!(
                    output,
                    " [linked worktree -> {}]",
                    main_worktree_path.display()
                )
                .ok();
            } else {
                write!(output, " [linked worktree]").ok();
            }
        }
        writeln!(output).ok();
    }

    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
        let panel = panel.read(cx);

        let panel_workspace_id = panel.workspace_id();
        if panel_workspace_id != workspace_db_id {
            writeln!(
                output,
                "  \u{26a0} workspace ID mismatch! panel has {panel_workspace_id:?}, workspace has {workspace_db_id:?}"
            )
            .ok();
        }

        if let Some(thread) = panel.active_agent_thread(cx) {
            let thread = thread.read(cx);
            let title = thread.title().unwrap_or_else(|| "(untitled)".into());
            let session_id = thread.session_id();
            let status = match thread.status() {
                ThreadStatus::Unconfirmed => "unconfirmed",
                ThreadStatus::Idle => "idle",
                ThreadStatus::Generating => "generating",
            };
            let entry_count = thread.entries().len();
            write!(output, "Active thread: {title} (session: {session_id})").ok();
            write!(output, " [{status}, {entry_count} entries").ok();
            if panel
                .active_conversation_view()
                .is_some_and(|conversation_view| {
                    conversation_view
                        .read(cx)
                        .root_thread_is_waiting_for_input(cx)
                })
            {
                write!(output, ", awaiting confirmation").ok();
            }
            writeln!(output, "]").ok();
        } else {
            writeln!(output, "Active thread: (none)").ok();
        }

        let background_threads = panel.retained_threads();
        if !background_threads.is_empty() {
            writeln!(
                output,
                "Background threads ({}): ",
                background_threads.len()
            )
            .ok();
            for (session_id, conversation_view) in background_threads {
                if let Some(thread_view) = conversation_view.read(cx).root_thread_view() {
                    let thread = thread_view.read(cx).thread.read(cx);
                    let title = thread.title().unwrap_or_else(|| "(untitled)".into());
                    let status = match thread.status() {
                        ThreadStatus::Unconfirmed => "unconfirmed",
                        ThreadStatus::Idle => "idle",
                        ThreadStatus::Generating => "generating",
                    };
                    let entry_count = thread.entries().len();
                    write!(output, "  - {title} (thread: {session_id:?})").ok();
                    write!(output, " [{status}, {entry_count} entries").ok();
                    if conversation_view
                        .read(cx)
                        .root_thread_is_waiting_for_input(cx)
                    {
                        write!(output, ", awaiting confirmation").ok();
                    }
                    writeln!(output, "]").ok();
                } else {
                    writeln!(output, "  - (not connected) (thread: {session_id:?})").ok();
                }
            }
        }
    } else {
        writeln!(output, "Agent panel: not loaded").ok();
    }

    writeln!(output).ok();
}
