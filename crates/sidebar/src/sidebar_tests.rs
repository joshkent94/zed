use super::*;
use acp_thread::{AcpThread, PermissionOptions, StubAgentConnection};
use agent::ThreadStore;
use agent_client_protocol::{v1::schema as acp_v1, v2::schema as acp_v2};
use agent_settings::AgentSettings;
use agent_ui::{
    NewThread, ThreadId,
    terminal_thread_metadata_store::{
        TerminalThreadMetadata, TerminalThreadMetadataStore, TestTerminalMetadataDbName,
    },
    test_support::{
        active_session_id, active_thread_id, open_thread_with_connection,
        open_thread_with_custom_connection, send_message,
    },
    thread_metadata_store::{ThreadMetadata, WorktreePaths},
};
use chrono::DateTime;
use fs::{FakeFs, Fs};
use git_ui_core::worktree_labels::WorkspaceMenuWorktreeLabel;
use gpui::{
    AnyView, App, Bounds, Corners, Decorations, Hsla, Point, Rgba, ScaledPixels, TestAppContext,
    Tiling, UpdateGlobal, VisualTestContext, point,
};
use platform_title_bar::PlatformTitleBar;
use pretty_assertions::assert_eq;
use project::AgentId;
use settings::{
    FontSize, Settings, SettingsStore, SidebarDockPosition, TitleBarVisibility, UiDensity,
};
use std::{
    cell::Cell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};
use util::path_list::PathList;
use workspace::{
    dock::{DockPosition, PanelEvent, test::TestPanel},
    item::test::TestItem,
};

fn use_unique_metadata_databases(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEST_DATABASE: AtomicUsize = AtomicUsize::new(0);
    let test_database_id = NEXT_TEST_DATABASE.fetch_add(1, Ordering::SeqCst);
    cx.update(|cx| {
        cx.set_global(agent_ui::thread_metadata_store::TestMetadataDbName(
            format!("SIDEBAR_THREAD_METADATA_{test_database_id}"),
        ));
        cx.set_global(TestTerminalMetadataDbName(format!(
            "SIDEBAR_TERMINAL_THREAD_METADATA_{test_database_id}"
        )));
    });
}

fn set_max_idle_retained_threads(max_idle_retained_threads: usize, cx: &mut App) {
    AgentSettings::override_global(
        AgentSettings {
            max_idle_retained_threads,
            ..AgentSettings::get_global(cx).clone()
        },
        cx,
    );
}

fn init_test(cx: &mut TestAppContext) {
    use_unique_metadata_databases(cx);
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        // Use an isolated DB so parallel tests can't see each other's
        // persisted records (e.g. created-worktree records).
        cx.set_global(db::AppDatabase::test_new());
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        TerminalThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });
}

#[track_caller]
fn assert_active_thread(sidebar: &Sidebar, session_id: &acp::SessionId, msg: &str) {
    let active = sidebar.active_entry.as_ref();
    let matches = active.is_some_and(|entry| {
        matches!(entry, ActiveEntry::Thread { session_id: Some(active_session_id), .. } if active_session_id == session_id)
            || sidebar.contents.entries.iter().any(|list_entry| {
                matches!(list_entry, ListEntry::Thread(t)
                    if t.metadata.session_id.as_ref() == Some(session_id)
                        && entry.matches_entry(list_entry))
            })
    });
    assert!(
        matches,
        "{msg}: expected active_entry for session {session_id:?}, got {:?}",
        active,
    );
}

#[track_caller]
fn is_active_session(sidebar: &Sidebar, session_id: &acp::SessionId) -> bool {
    let thread_id = sidebar
        .contents
        .entries
        .iter()
        .find_map(|entry| match entry {
            ListEntry::Thread(t) if t.metadata.session_id.as_ref() == Some(session_id) => {
                Some(t.metadata.thread_id)
            }
            _ => None,
        });
    match thread_id {
        Some(tid) => {
            matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { thread_id, .. }) if *thread_id == tid)
        }
        // Thread not in sidebar entries — can't confirm it's active.
        None => false,
    }
}

#[track_caller]
fn assert_active_draft(sidebar: &Sidebar, workspace: &Entity<Workspace>, msg: &str) {
    assert!(
        matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { workspace: ws, .. }) if ws == workspace),
        "{msg}: expected active_entry to be Draft for workspace {:?}, got {:?}",
        workspace.entity_id(),
        sidebar.active_entry,
    );
}

fn has_thread_entry(sidebar: &Sidebar, session_id: &acp::SessionId) -> bool {
    sidebar
        .contents
        .entries
        .iter()
        .any(|entry| matches!(entry, ListEntry::Thread(t) if t.metadata.session_id.as_ref() == Some(session_id)))
}

#[track_caller]
fn assert_project_header_has_threads(
    sidebar: &Entity<Sidebar>,
    project_name: &str,
    expected_has_threads: bool,
    cx: &mut gpui::VisualTestContext,
) {
    sidebar.read_with(cx, |sidebar, _cx| {
        let has_threads = sidebar.contents.entries.iter().find_map(|entry| {
            if let ListEntry::ProjectHeader {
                label, has_threads, ..
            } = entry
                && label.as_ref() == project_name
            {
                Some(*has_threads)
            } else {
                None
            }
        });

        assert_eq!(
            has_threads,
            Some(expected_has_threads),
            "expected project header `{project_name}` to have has_threads={expected_has_threads}, got {has_threads:?}"
        );
    });
}

#[track_caller]
fn assert_remote_project_integration_sidebar_state(
    sidebar: &mut Sidebar,
    main_thread_id: &acp::SessionId,
    remote_thread_id: &acp::SessionId,
) {
    let mut project_headers = sidebar.contents.entries.iter().filter_map(|entry| {
        if let ListEntry::ProjectHeader { label, .. } = entry {
            Some(label.as_ref())
        } else {
            None
        }
    });

    let Some(project_header) = project_headers.next() else {
        panic!("expected exactly one sidebar project header named `project`, found none");
    };
    assert_eq!(
        project_header, "project",
        "expected the only sidebar project header to be `project`"
    );
    if let Some(unexpected_header) = project_headers.next() {
        panic!(
            "expected exactly one sidebar project header named `project`, found extra header `{unexpected_header}`"
        );
    }

    let mut saw_main_thread = false;
    let mut saw_remote_thread = false;
    for entry in &sidebar.contents.entries {
        match entry {
            ListEntry::ProjectHeader { label, .. } => {
                assert_eq!(
                    label.as_ref(),
                    "project",
                    "expected the only sidebar project header to be `project`"
                );
            }
            ListEntry::Thread(thread)
                if thread.metadata.session_id.as_ref() == Some(main_thread_id) =>
            {
                saw_main_thread = true;
            }
            ListEntry::Thread(thread)
                if thread.metadata.session_id.as_ref() == Some(remote_thread_id) =>
            {
                saw_remote_thread = true;
            }
            ListEntry::Thread(thread) => {
                let title = thread.metadata.display_title();
                panic!(
                    "unexpected sidebar thread while simulating remote project integration flicker: title=`{}`",
                    title
                );
            }
            ListEntry::Terminal(terminal) => {
                panic!(
                    "unexpected sidebar terminal while simulating remote project integration flicker: title=`{}`",
                    terminal.metadata.title
                );
            }
            ListEntry::WorktreeHeader { .. } => {}
        }
    }

    assert!(
        saw_main_thread,
        "expected the sidebar to keep showing `Main Thread` under `project`"
    );
    assert!(
        saw_remote_thread,
        "expected the sidebar to keep showing `Worktree Thread` under `project`"
    );
}

async fn init_test_project(
    worktree_path: &str,
    cx: &mut TestAppContext,
) -> Entity<project::Project> {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(worktree_path, serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    project::Project::test(fs, [worktree_path.as_ref()], cx).await
}

#[gpui::test]
async fn test_workspace_menu_uses_bare_repository_worktree_name(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/zed/.bare",
        serde_json::json!({
            "worktrees": {
                "glossy-walrus": {
                    "commondir": "../..",
                    "HEAD": "ref: refs/heads/glossy-walrus",
                },
            },
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/zed/glossy-walrus/zed",
        serde_json::json!({
            ".git": "gitdir: /zed/.bare/worktrees/glossy-walrus",
            "src": {},
        }),
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project =
        project::Project::test(fs, [Path::new("/worktrees/zed/glossy-walrus/zed")], cx).await;
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });
    let labels = cx.update(|_window, cx| workspace_menu_worktree_labels(&workspace, cx));

    assert_eq!(labels.len(), 1);
    assert_eq!(labels[0].primary_name.as_ref(), "glossy-walrus");
    assert_eq!(labels[0].secondary_name, None);
}

#[gpui::test]
async fn test_delta_checkout_subfolders_are_named_after_their_checkout(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        util::path!("/repo"),
        serde_json::json!({
            ".git": { "HEAD": "ref: refs/heads/main\n" },
            ".delta": {
                "clones": {
                    "a": {
                        "repo.git": {
                            "HEAD": "ref: refs/heads/feature\n",
                            "config": "[core]\n\tbare = false\n",
                            "worktrees": {
                                "review": {
                                    "HEAD": "ref: refs/heads/review\n",
                                    "commondir": "../..",
                                    "gitdir": util::path!("/review/repo/.git"),
                                },
                            },
                            "delta": { "copy-baseline": "" },
                            "objects": {
                                "info": {
                                    "alternates": format!("{}\n", util::path!("/repo/.git/objects")),
                                },
                            },
                        },
                    },
                    "b": {
                        "repo.git": {
                            "HEAD": "ref: refs/heads/feature\n",
                            "config": "[core]\n\tbare = false\n",
                            "delta": { "copy-baseline": "" },
                            "objects": {
                                "info": {
                                    "alternates": format!("{}\n", util::path!("/repo/.git/objects")),
                                },
                            },
                        },
                    },
                },
                "worktrees": {
                    "a": {
                        "repo": {
                            ".git": format!("gitdir: {}\n", util::path!("/repo/.delta/clones/a/repo.git")),
                            "src": {},
                        },
                    },
                    "review-b": {
                        "repo": {
                            ".git": format!("gitdir: {}\n", util::path!("/repo/.delta/clones/b/repo.git")),
                            "src": {},
                        },
                    },
                },
            },
        }),
    )
    .await;
    fs.insert_tree(
        util::path!("/review/repo"),
        serde_json::json!({
            ".git": format!(
                "gitdir: {}\n",
                util::path!("/repo/.delta/clones/a/repo.git/worktrees/review")
            ),
        }),
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    for (name, folder_name, minute) in [("a", "a", 2), ("b", "review-b", 1)] {
        save_thread_metadata_with_main_paths(
            name,
            &format!("Thread {name}"),
            PathList::new(&[PathBuf::from(util::path!("/repo/.delta/worktrees"))
                .join(folder_name)
                .join("repo")
                .join("src")]),
            PathList::new(&[Path::new(util::path!("/repo"))]),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, minute, 0).unwrap(),
            cx,
        );
    }
    let project_a = project::Project::test(
        fs.clone(),
        [Path::new(util::path!("/repo/.delta/worktrees/a/repo/src"))],
        cx,
    )
    .await;
    let project_b = project::Project::test(
        fs.clone(),
        [Path::new(util::path!(
            "/repo/.delta/worktrees/review-b/repo/src"
        ))],
        cx,
    )
    .await;
    let project_review =
        project::Project::test(fs.clone(), [Path::new(util::path!("/review/repo"))], cx).await;
    for project in [&project_a, &project_b, &project_review] {
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
    }

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let workspace_b = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(project_review, window, cx);
        multi_workspace.test_add_workspace(project_b, window, cx)
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
    let mut labels = cx.update(|_window, cx| {
        multi_workspace
            .read(cx)
            .workspaces()
            .flat_map(|workspace| workspace_menu_worktree_labels(workspace, cx))
            .map(|label| {
                (
                    label.primary_name.to_string(),
                    label.secondary_name.map(|name| name.to_string()),
                )
            })
            .collect::<Vec<_>>()
    });
    labels.sort();

    assert_eq!(
        (labels, visible_entries_as_strings(&sidebar, cx)),
        (
            vec![
                ("a/repo/review/repo".to_string(), None),
                ("src".to_string(), Some("a".to_string())),
                ("src".to_string(), Some("b/repo".to_string())),
            ],
            vec![
                "v [repo]".to_string(),
                "  Thread a {a}".to_string(),
                "  Thread b {b/repo}".to_string(),
            ]
        )
    );

    multi_workspace
        .update_in(cx, |multi_workspace, window, cx| {
            multi_workspace.remove([workspace_b], RemovalIntent::KeepProject, window, cx)
        })
        .await
        .unwrap();
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [repo]", "  Thread a {a}", "  Thread b {b/repo}"]
    );
}

#[gpui::test]
async fn test_project_menu_of_a_group_with_delta_checkouts(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let delta_clone = || {
        serde_json::json!({
            "HEAD": "ref: refs/heads/feature\n",
            "config": "[core]\n\tbare = false\n",
            "delta": { "copy-baseline": "" },
            "objects": {
                "info": {
                    "alternates": format!("{}\n", util::path!("/repo/.git/objects")),
                },
            },
        })
    };
    fs.insert_tree(
        util::path!("/repo"),
        serde_json::json!({
            ".git": {
                "HEAD": "ref: refs/heads/main\n",
                "worktrees": {
                    "feature": {
                        "HEAD": "ref: refs/heads/feature\n",
                        "commondir": "../..",
                        "gitdir": util::path!("/worktrees/feature/repo/.git"),
                    },
                },
            },
            ".delta": {
                "clones": {
                    "a": { "repo.git": delta_clone() },
                    "b": { "repo.git": delta_clone() },
                },
                "worktrees": {
                    "a": {
                        "repo": {
                            ".git": format!("gitdir: {}\n", util::path!("/repo/.delta/clones/a/repo.git")),
                            "src": {},
                        },
                    },
                    "b": {
                        "repo": {
                            ".git": format!("gitdir: {}\n", util::path!("/repo/.delta/clones/b/repo.git")),
                        },
                    },
                },
            },
        }),
    )
    .await;
    fs.insert_tree(
        util::path!("/worktrees/feature/repo"),
        serde_json::json!({
            ".git": format!("gitdir: {}\n", util::path!("/repo/.git/worktrees/feature")),
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(util::path!("/tools"), serde_json::json!({ "bin": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let checkout_subfolder = PathBuf::from(util::path!("/repo/.delta/worktrees/a/repo/src"));
    let root_sets = [
        vec![PathBuf::from(util::path!("/repo/.delta/worktrees/b/repo"))],
        vec![checkout_subfolder.clone()],
        vec![PathBuf::from(util::path!("/worktrees/feature/repo"))],
        vec![
            PathBuf::from(util::path!("/repo")),
            PathBuf::from(util::path!("/tools")),
        ],
        vec![
            PathBuf::from(util::path!("/repo/.delta/worktrees/b/repo")),
            PathBuf::from(util::path!("/tools")),
        ],
        vec![PathBuf::from(util::path!("/worktrees/feature/repo/src"))],
        vec![PathBuf::from(util::path!("/repo"))],
    ];
    let mut projects = Vec::new();
    for roots in &root_sets {
        let project = project::Project::test(
            fs.clone(),
            roots.iter().map(|root| root.as_path()).collect::<Vec<_>>(),
            cx,
        )
        .await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        projects.push(project);
    }
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(projects[0].clone(), window, cx));
    let sibling_workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let mut workspaces = vec![sibling_workspace.clone()];
    for project in &projects[1..] {
        workspaces.push(
            multi_workspace.update_in(cx, |multi_workspace, window, cx| {
                multi_workspace.test_add_workspace(project.clone(), window, cx)
            }),
        );
    }
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.activate(sibling_workspace.clone(), None, window, cx);
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let open_labels = workspaces
        .iter()
        .map(|workspace| {
            cx.update(|_window, cx| {
                menu_label_names(&workspace_menu_worktree_labels(workspace, cx))
            })
        })
        .collect::<Vec<_>>();
    let expected_labels = vec![
        vec![("b".to_string(), None)],
        vec![("src".to_string(), Some("a".to_string()))],
        vec![("feature".to_string(), None)],
        vec![
            ("repo".to_string(), Some("main".to_string())),
            ("tools".to_string(), None),
        ],
        vec![
            ("repo".to_string(), Some("b".to_string())),
            ("tools".to_string(), None),
        ],
        vec![("src".to_string(), None)],
        vec![("main".to_string(), None)],
    ];
    assert_eq!(open_labels, expected_labels);

    let project_group_keys = [&workspaces[0], &workspaces[3]]
        .map(|workspace| workspace.read_with(cx, |workspace, cx| workspace.project_group_key(cx)));
    let group_workspaces = multi_workspace.read_with(cx, |multi_workspace, cx| {
        multi_workspace
            .workspaces_for_project_group(&project_group_keys[0], cx)
            .cloned()
            .collect::<Vec<_>>()
    });
    cx.update(|_, cx| {
        let creation_repositories = [&workspaces[0], &workspaces[6]].map(|active_workspace| {
            let base = worktree_creation_base_workspace(active_workspace, group_workspaces.iter())
                .expect("the project group has open workspaces");
            git_project_group_key(base, cx).path_list().paths().to_vec()
        });
        assert_eq!(
            creation_repositories,
            [
                vec![PathBuf::from(util::path!("/repo/.delta/clones/b/repo.git"))],
                vec![PathBuf::from(util::path!("/repo"))],
            ],
            "worktree creation uses the active checkout's own repository"
        );
        assert_eq!(
            worktree_creation_base_workspace(&workspaces[3], group_workspaces.iter()),
            group_workspaces.first(),
            "a project without an active checkout uses its first open workspace"
        );
    });

    cx.run_until_parked();
    let open_worktree_roots = group_workspaces
        .iter()
        .map(|workspace| {
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .root_paths(cx)
                    .iter()
                    .map(|path| path.to_path_buf())
                    .collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();
    let active_roots = active_root_paths(&multi_workspace, cx);
    let inactive_worktree_row = open_worktree_roots
        .iter()
        .position(|roots| *roots != active_roots)
        .unwrap();
    let menu_handle = project_menu_handle(&sidebar, &project_group_keys[0], cx);
    // Open Project in New Window and Rename precede the selectable worktree rows.
    let is_menu_open_after_open_worktree =
        confirm_project_menu_row(&menu_handle, 2 + inactive_worktree_row, cx);
    cx.run_until_parked();
    assert_eq!(
        (
            is_menu_open_after_open_worktree,
            active_root_paths(&multi_workspace, cx),
        ),
        (false, open_worktree_roots[inactive_worktree_row].clone()),
        "Enter on an open worktree's row closes the menu at once and activates that worktree"
    );
}

#[gpui::test]
async fn test_project_menu_opens_the_worktree_picker_of_its_group(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let delta_clone = |branch: &str| {
        serde_json::json!({
            "repo.git": {
                "HEAD": format!("ref: refs/heads/{branch}\n"),
                "config": "[core]\n\tbare = false\n",
                "delta": { "copy-baseline": "" },
                "objects": {
                    "info": {
                        "alternates": format!("{}\n", util::path!("/repo/.git/objects")),
                    },
                },
            },
        })
    };
    let delta_checkout = |namespace: &str| {
        serde_json::json!({
            "repo": {
                ".git": format!(
                    "gitdir: {}/{namespace}/repo.git\n",
                    util::path!("/repo/.delta/clones")
                ),
            },
        })
    };
    fs.insert_tree(
        util::path!("/repo"),
        serde_json::json!({
            ".delta": {
                "clones": { "a": delta_clone("feature"), "b": delta_clone("feature-b") },
                "worktrees": { "a": delta_checkout("a"), "b": delta_checkout("b") },
            },
        }),
    )
    .await;
    fs.insert_tree(util::path!("/other"), serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let mut projects = Vec::new();
    for root in [
        util::path!("/other"),
        util::path!("/repo/.delta/worktrees/a/repo"),
        util::path!("/repo"),
        util::path!("/repo/.delta/worktrees/b/repo"),
    ] {
        let project = project::Project::test(fs.clone(), [Path::new(root)], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        projects.push(project);
    }
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(projects[0].clone(), window, cx));
    let checkout_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(projects[1].clone(), window, cx)
    });
    let source_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(projects[2].clone(), window, cx)
    });
    let second_checkout_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(projects[3].clone(), window, cx)
    });
    let other_workspace = multi_workspace.read_with(cx, |multi_workspace, _| {
        multi_workspace.workspaces().next().unwrap().clone()
    });
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.activate(source_workspace.clone(), None, window, cx);
        multi_workspace.activate(other_workspace.clone(), None, window, cx);
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);
    cx.run_until_parked();

    let project_group_key =
        checkout_workspace.read_with(cx, |workspace, cx| workspace.project_group_key(cx));
    let other_group_key =
        other_workspace.read_with(cx, |workspace, cx| workspace.project_group_key(cx));
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, cx| {
            (
                source_workspace.read(cx).project_group_key(cx) == project_group_key,
                worktree_picker_workspace(multi_workspace, &project_group_key, cx),
                worktree_picker_workspace(multi_workspace, &other_group_key, cx),
            )
        }),
        (true, Some(checkout_workspace.clone()), None),
        "the group's last active workspace has no repository, so its checkout is used; \
         a group without a Git repository gets no worktree picker row"
    );
    checkout_workspace.update_in(cx, |workspace, window, cx| {
        let project = workspace.project().clone();
        let workspace_handle = workspace.weak_handle();
        workspace.toggle_modal(window, cx, |window, cx| {
            WorktreePicker::new_modal(project, workspace_handle, None, window, cx)
        });
    });
    cx.run_until_parked();
    let stale_picker = checkout_workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_modal::<WorktreePicker>(cx)
            .map(|picker| picker.entity_id())
    });
    let menu_handle = project_menu_handle(&sidebar, &project_group_key, cx);
    let is_menu_open = confirm_project_menu_row(&menu_handle, 6, cx);
    cx.run_until_parked();
    assert_eq!(
        (
            is_menu_open,
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone()),
            checkout_workspace.read_with(cx, |workspace, cx| {
                let picker = workspace
                    .active_modal::<WorktreePicker>(cx)
                    .map(|picker| picker.entity_id());
                picker.is_some() && picker != stale_picker
            }),
        ),
        (false, checkout_workspace, true),
        "the group's own workspace is activated and shows the worktree picker"
    );

    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.activate(second_checkout_workspace.clone(), None, window, cx);
        multi_workspace.activate(other_workspace.clone(), None, window, cx);
    });
    let host_of_last_active_checkout = multi_workspace.read_with(cx, |multi_workspace, cx| {
        worktree_picker_workspace(multi_workspace, &project_group_key, cx)
    });
    cx.update_global::<SettingsStore, _>(|store, cx| {
        store.update_user_settings(cx, |settings| {
            settings
                .git
                .get_or_insert_default()
                .enabled
                .get_or_insert_default()
                .enable_status = Some(false);
        });
    });
    let host_without_git_status = multi_workspace.read_with(cx, |multi_workspace, cx| {
        worktree_picker_workspace(multi_workspace, &project_group_key, cx)
    });
    assert_eq!(
        (host_of_last_active_checkout, host_without_git_status),
        (Some(second_checkout_workspace), None),
        "the group's last active checkout hosts the picker; without Git status there is no row"
    );
}

#[gpui::test]
async fn test_key_change_keeps_the_group_of_closed_rows_that_stay_listed(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let delta_clone = || {
        serde_json::json!({
            "HEAD": "ref: refs/heads/feature\n",
            "config": "[core]\n\tbare = false\n",
            "delta": { "copy-baseline": "" },
            "objects": {
                "info": {
                    "alternates": format!("{}\n", util::path!("/repo/.git/objects")),
                },
            },
        })
    };
    fs.insert_tree(
        util::path!("/repo"),
        serde_json::json!({
            ".git": { "HEAD": "ref: refs/heads/main\n" },
            ".delta": {
                "clones": {
                    "a": { "repo.git": delta_clone() },
                    "b": { "repo.git": delta_clone() },
                },
                "worktrees": {
                    "a": {
                        "repo": {
                            ".git": format!("gitdir: {}\n", util::path!("/repo/.delta/clones/a/repo.git")),
                            "src": {},
                        },
                    },
                    "b": {
                        "repo": {
                            ".git": format!("gitdir: {}\n", util::path!("/repo/.delta/clones/b/repo.git")),
                        },
                    },
                },
            },
        }),
    )
    .await;
    fs.insert_tree(util::path!("/tools"), serde_json::json!({ "bin": {} }))
        .await;
    fs.insert_tree(util::path!("/other"), serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree(
        util::path!("/next"),
        serde_json::json!({ ".git": { "HEAD": "ref: refs/heads/main\n" } }),
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let checkout_a = PathBuf::from(util::path!("/repo/.delta/worktrees/a/repo"));
    let checkout_b = PathBuf::from(util::path!("/repo/.delta/worktrees/b/repo"));
    let clone_a = PathBuf::from(util::path!("/repo/.delta/clones/a/repo.git"));
    let source = PathList::new(&[Path::new(util::path!("/repo"))]);
    let saved_at =
        |minute| chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, minute, 0).unwrap();
    save_thread_metadata_with_worktree_paths(
        "own-subfolder",
        "Own subfolder",
        WorktreePaths::from_path_lists(source.clone(), PathList::new(&[checkout_a.join("src")]))
            .unwrap()
            .with_delta_clones([(
                checkout_a.join("src"),
                project::DeltaClone {
                    path: clone_a.clone(),
                    is_linked_worktree: false,
                    checkout_path: Some(checkout_a.clone()),
                },
            )]),
        saved_at(1),
        cx,
    );
    let project = project::Project::test(fs.clone(), [checkout_a.as_path()], cx).await;
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    let other_project =
        project::Project::test(fs.clone(), [Path::new(util::path!("/other"))], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let checkout_workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(other_project, window, cx);
        multi_workspace.activate(checkout_workspace, None, window, cx);
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);
    save_thread_metadata_with_main_paths(
        "own-root",
        "Own root",
        PathList::new(std::slice::from_ref(&checkout_a)),
        source.clone(),
        saved_at(3),
        cx,
    );

    let mut entries = Vec::new();
    entries.push(entries_after_folder_change(&sidebar, &project, true, cx).await);
    entries.push(entries_after_folder_change(&sidebar, &project, false, cx).await);

    let marker_path = clone_a.join("delta/copy-baseline");
    fs.remove_file(&marker_path, fs::RemoveOptions::default())
        .await
        .unwrap();
    entries.push(updated_entries(&sidebar, cx));
    fs.insert_file(&marker_path, Vec::new()).await;
    entries.push(updated_entries(&sidebar, cx));

    let alternates_path = clone_a.join("objects/info/alternates");
    fs.insert_file(
        &alternates_path,
        format!("{}\n", util::path!("/next/.git/objects")).into_bytes(),
    )
    .await;
    entries.push(updated_entries(&sidebar, cx));
    fs.insert_file(
        &alternates_path,
        format!("{}\n", util::path!("/repo/.git/objects")).into_bytes(),
    )
    .await;
    entries.push(updated_entries(&sidebar, cx));

    let other_draft_id = save_draft_metadata_with_main_paths(
        None,
        PathList::new(std::slice::from_ref(&checkout_b)),
        source.clone(),
        saved_at(2),
        cx,
    );
    let with_empty_draft = entries_after_folder_change(&sidebar, &project, true, cx).await;
    assert_eq!(
        with_empty_draft,
        vec![
            "v [other]",
            "v [repo / tools]",
            "  Own root {repo:a}",
            "v [repo]",
            "  New Zed Agent Thread {b}",
            "  Own subfolder {a}",
        ]
    );
    entries_after_folder_change(&sidebar, &project, false, cx).await;
    cx.update(|_, cx| {
        agent_ui::draft_prompt_store::write(
            other_draft_id,
            &[acp_v2::ContentBlock::Text(acp_v2::TextContent::new(
                "Other draft",
            ))],
            cx,
        )
    })
    .await
    .unwrap();
    entries.push(entries_after_folder_change(&sidebar, &project, true, cx).await);
    entries.push(entries_after_folder_change(&sidebar, &project, false, cx).await);

    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.delete(other_draft_id, cx));
    });
    save_terminal_metadata_with_main_paths(
        "Other terminal",
        PathList::new(&[checkout_b]),
        source,
        saved_at(4),
        cx,
    );
    entries.push(entries_after_folder_change(&sidebar, &project, true, cx).await);

    assert_eq!(
        entries,
        vec![
            vec!["v [other]", "v [repo / tools]", "  Own root {repo:a}"],
            vec![
                "v [other]",
                "v [repo]",
                "  Own subfolder {a}",
                "  Own root {a}"
            ],
            vec![
                "v [other]",
                "v [repo]",
                "  Own root {repo}",
                "v [repo]",
                "  Own subfolder {a}"
            ],
            vec![
                "v [other]",
                "v [repo]",
                "  Own subfolder {a}",
                "  Own root {a}"
            ],
            vec![
                "v [other]",
                "v [next]",
                "  Own root {a}",
                "v [repo]",
                "  Own subfolder {a}"
            ],
            vec![
                "v [other]",
                "v [repo]",
                "  Own subfolder {a}",
                "  Own root {a}"
            ],
            vec![
                "v [other]",
                "v [repo / tools]",
                "  Own root {repo:a}",
                "v [repo]",
                "  Other draft {b}",
                "  Own subfolder {a}"
            ],
            vec![
                "v [other]",
                "v [repo]",
                "  Other draft {b}",
                "  Own subfolder {a}",
                "  Own root {a}"
            ],
            vec![
                "v [other]",
                "v [repo / tools]",
                "  Own root {repo:a}",
                "v [repo]",
                "  Other terminal {b}",
                "  Own subfolder {a}"
            ],
        ]
    );
}

pub(super) fn setup_sidebar(
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut gpui::VisualTestContext,
) -> Entity<Sidebar> {
    let sidebar = setup_sidebar_closed(multi_workspace, cx);
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.toggle_sidebar(window, cx);
    });
    cx.run_until_parked();
    sidebar
}

pub(super) fn setup_sidebar_closed(
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut gpui::VisualTestContext,
) -> Entity<Sidebar> {
    let multi_workspace = multi_workspace.clone();
    let sidebar =
        cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    multi_workspace.update(cx, |mw, cx| {
        mw.register_sidebar(sidebar.clone(), cx);
    });
    cx.run_until_parked();
    sidebar
}

fn set_threads_sidebar_default_width(width: f32, cx: &mut App) {
    SettingsStore::update_global(cx, |store, cx| {
        store
            .set_user_settings(
                &format!(r#"{{"agent": {{"threads_sidebar": {{"default_width": {width}}}}}}}"#),
                cx,
            )
            .unwrap();
    });
}

async fn save_n_test_threads(
    count: u32,
    project: &Entity<project::Project>,
    cx: &mut gpui::VisualTestContext,
) {
    for i in 0..count {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(format!("thread-{}", i))),
            Some(format!("Thread {}", i + 1).into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, i).unwrap(),
            None,
            None,
            project,
            cx,
        )
    }
    cx.run_until_parked();
}

async fn save_test_thread_metadata(
    session_id: &acp::SessionId,
    project: &Entity<project::Project>,
    cx: &mut TestAppContext,
) {
    save_thread_metadata(
        session_id.clone(),
        Some("Test".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        project,
        cx,
    )
}

async fn save_named_thread_metadata(
    session_id: &str,
    title: &str,
    project: &Entity<project::Project>,
    cx: &mut gpui::VisualTestContext,
) {
    save_thread_metadata(
        acp::SessionId::new(Arc::from(session_id)),
        Some(SharedString::from(title.to_string())),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        project,
        cx,
    );
    cx.run_until_parked();
}

/// Seeds a pre-built [`ThreadMetadata`] into the global store so tests can
/// exercise flows that resolve a thread by id.
fn seed_thread_metadata(metadata: ThreadMetadata, cx: &mut TestAppContext) {
    cx.update(|cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();
}

/// Spins up a fresh remote project backed by a headless server sharing
/// `server_fs`, opens the given worktree path on it, and returns the
/// project together with the headless entity (which the caller must keep
/// alive for the duration of the test) and the `RemoteConnectionOptions`
/// used for the fake server. Passing those options back into
/// `reuse_opts` on a subsequent call makes the new project share the
/// same `RemoteConnectionIdentity`, matching how Zed treats multiple
/// projects on the same SSH host.
async fn start_remote_project(
    server_fs: &Arc<FakeFs>,
    worktree_path: &Path,
    app_state: &Arc<workspace::AppState>,
    reuse_opts: Option<&remote::RemoteConnectionOptions>,
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (
    Entity<project::Project>,
    Entity<remote_server::HeadlessProject>,
    remote::RemoteConnectionOptions,
) {
    // Bare `_` on the guard so it's dropped immediately; holding onto it
    // would deadlock `connect_mock` below since the client waits on the
    // guard before completing the mock handshake.
    let (opts, server_session) = match reuse_opts {
        Some(existing) => {
            let (session, _) = remote::RemoteClient::fake_server_with_opts(existing, cx, server_cx);
            (existing.clone(), session)
        }
        None => {
            let (opts, session, _) = remote::RemoteClient::fake_server(cx, server_cx);
            (opts, session)
        }
    };

    server_cx.update(remote_server::HeadlessProject::init);
    let server_executor = server_cx.executor();
    let fs = server_fs.clone();
    let headless = server_cx.new(|cx| {
        remote_server::HeadlessProject::new(
            remote_server::HeadlessAppState {
                session: server_session,
                fs,
                http_client: Arc::new(http_client::BlockedHttpClient),
                node_runtime: node_runtime::NodeRuntime::unavailable(),
                languages: Arc::new(language::LanguageRegistry::new(server_executor.clone())),
                extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });

    let remote_client = remote::RemoteClient::connect_mock(opts.clone(), cx).await;
    let project = cx.update(|cx| {
        let project_client = client::Client::new(
            Arc::new(clock::FakeSystemClock::new()),
            http_client::FakeHttpClient::with_404_response(),
            cx,
        );
        let user_store = cx.new(|cx| client::UserStore::new(project_client.clone(), cx));
        project::Project::remote(
            remote_client,
            project_client,
            node_runtime::NodeRuntime::unavailable(),
            user_store,
            app_state.languages.clone(),
            app_state.fs.clone(),
            false,
            cx,
        )
    });

    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(worktree_path, true, cx)
        })
        .await
        .expect("should open remote worktree");
    cx.run_until_parked();

    (project, headless, opts)
}

fn save_thread_metadata(
    session_id: acp::SessionId,
    title: Option<SharedString>,
    updated_at: DateTime<Utc>,
    created_at: Option<DateTime<Utc>>,
    interacted_at: Option<DateTime<Utc>>,
    project: &Entity<project::Project>,
    cx: &mut TestAppContext,
) {
    cx.update(|cx| {
        let worktree_paths = project.read(cx).worktree_paths(cx);
        let remote_connection = project.read(cx).remote_connection_options(cx).cloned();
        let thread_id = ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .unwrap_or_else(ThreadId::new);
        let metadata = ThreadMetadata {
            thread_id,
            session_id: Some(session_id),
            agent_id: agent::ZED_AGENT_ID.clone(),
            title,
            title_override: None,
            updated_at,
            created_at,
            interacted_at,
            worktree_paths,
            archived: false,
            remote_connection,
        };
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();
}

fn save_thread_metadata_with_main_paths(
    session_id: &str,
    title: &str,
    folder_paths: PathList,
    main_worktree_paths: PathList,
    updated_at: DateTime<Utc>,
    cx: &mut TestAppContext,
) {
    save_thread_metadata_with_worktree_paths(
        session_id,
        title,
        WorktreePaths::from_path_lists(main_worktree_paths, folder_paths).unwrap(),
        updated_at,
        cx,
    );
}

fn save_thread_metadata_with_worktree_paths(
    session_id: &str,
    title: &str,
    worktree_paths: WorktreePaths,
    updated_at: DateTime<Utc>,
    cx: &mut TestAppContext,
) {
    let session_id = acp::SessionId::new(Arc::from(session_id));
    let title = SharedString::from(title.to_string());
    let thread_id = cx.update(|cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .unwrap_or_else(ThreadId::new)
    });
    let metadata = ThreadMetadata {
        thread_id,
        session_id: Some(session_id),
        agent_id: agent::ZED_AGENT_ID.clone(),
        title: Some(title),
        title_override: None,
        updated_at,
        created_at: None,
        interacted_at: None,
        worktree_paths,
        archived: false,
        remote_connection: None,
    };
    cx.update(|cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();
}

fn save_draft_metadata_with_main_paths(
    title: Option<SharedString>,
    folder_paths: PathList,
    main_worktree_paths: PathList,
    updated_at: DateTime<Utc>,
    cx: &mut TestAppContext,
) -> ThreadId {
    let thread_id = ThreadId::new();
    let metadata = ThreadMetadata {
        thread_id,
        session_id: None,
        agent_id: agent::ZED_AGENT_ID.clone(),
        title,
        title_override: None,
        updated_at,
        created_at: None,
        interacted_at: None,
        worktree_paths: WorktreePaths::from_path_lists(main_worktree_paths, folder_paths).unwrap(),
        archived: false,
        remote_connection: None,
    };
    cx.update(|cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();
    thread_id
}

fn focus_sidebar(sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext) {
    sidebar.update_in(cx, |_, window, cx| {
        cx.focus_self(window);
    });
    cx.run_until_parked();
}

fn enter_renamed_title(
    sidebar: &Entity<Sidebar>,
    target: RenameTarget,
    renamed_title: &str,
    cx: &mut gpui::VisualTestContext,
) {
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(sidebar.rename_target, Some(target));
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.rename_editor.update(cx, |editor, cx| {
            editor.set_text(renamed_title, window, cx);
        });
    });
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.finish_entry_rename(window, cx);
    });
    cx.run_until_parked();
}

fn request_test_tool_authorization(
    thread: &Entity<AcpThread>,
    tool_call_id: &str,
    option_id: &str,
    cx: &mut gpui::VisualTestContext,
) {
    let tool_call_id = acp::ToolCallId::new(tool_call_id);
    let label = format!("Tool {tool_call_id}");
    let option_id = acp_v2::PermissionOptionId::new(option_id);
    let _authorization_task = cx.update(|_, cx| {
        thread.update(cx, |thread, cx| {
            thread
                .request_tool_call_authorization(
                    acp_v1::ToolCall::new(acp_v1::ToolCallId::new(tool_call_id.0), label)
                        .kind(acp_v1::ToolKind::Edit)
                        .into(),
                    PermissionOptions::Flat(vec![acp_v2::PermissionOption::new(
                        option_id,
                        "Allow",
                        acp_v2::PermissionOptionKind::AllowOnce,
                    )]),
                    acp_thread::AuthorizationKind::PermissionGrant,
                    cx,
                )
                .unwrap()
        })
    });
    cx.run_until_parked();
}

fn format_linked_worktree_chips(worktrees: &[ThreadItemWorktreeInfo]) -> String {
    let mut seen = Vec::new();
    let mut chips = Vec::new();
    for wt in worktrees {
        if wt.kind == ui::WorktreeKind::Main {
            continue;
        }
        let Some(name) = wt.worktree_name.as_ref() else {
            continue;
        };
        if !seen.contains(name) {
            seen.push(name.clone());
            chips.push(format!("{{{}}}", name));
        }
    }
    if chips.is_empty() {
        String::new()
    } else {
        format!(" {}", chips.join(", "))
    }
}

fn visible_entries_as_strings(
    sidebar: &Entity<Sidebar>,
    cx: &mut gpui::VisualTestContext,
) -> Vec<String> {
    visible_entries(sidebar, false, cx)
}

fn visible_entries_with_worktree_headers(
    sidebar: &Entity<Sidebar>,
    cx: &mut gpui::VisualTestContext,
) -> Vec<String> {
    visible_entries(sidebar, true, cx)
}

fn visible_entries(
    sidebar: &Entity<Sidebar>,
    include_worktree_headers: bool,
    cx: &mut gpui::VisualTestContext,
) -> Vec<String> {
    sidebar.read_with(cx, |sidebar, cx| {
        sidebar
            .contents
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                include_worktree_headers || !matches!(entry, ListEntry::WorktreeHeader { .. })
            })
            .map(|(ix, entry)| {
                let selected = if sidebar.selection == Some(ix) {
                    "  <== selected"
                } else {
                    ""
                };
                match entry {
                    ListEntry::ProjectHeader { label, key, .. } => {
                        let icon = if sidebar.is_group_collapsed(key, cx) {
                            ">"
                        } else {
                            "v"
                        };
                        format!("{} [{}]{}", icon, label, selected)
                    }
                    ListEntry::Thread(thread) => {
                        let title = thread.metadata.display_title();
                        let worktree = format_linked_worktree_chips(&thread.worktrees);

                        {
                            let live = if thread.is_live { " *" } else { "" };
                            let status_str = match thread.status {
                                AgentThreadStatus::Running => " (running)",
                                AgentThreadStatus::Error => " (error)",
                                AgentThreadStatus::WaitingForConfirmation => " (waiting)",
                                _ => "",
                            };
                            let notified = if sidebar
                                .contents
                                .is_thread_notified(&thread.metadata.thread_id)
                            {
                                " (!)"
                            } else {
                                ""
                            };
                            format!("  {title}{worktree}{live}{status_str}{notified}{selected}")
                        }
                    }
                    ListEntry::Terminal(terminal) => {
                        let title = terminal.metadata.display_title();
                        let worktree = format_linked_worktree_chips(&terminal.worktrees);
                        format!("  {title}{worktree}{selected}")
                    }
                    ListEntry::WorktreeHeader {
                        label,
                        is_collapsed,
                        ..
                    } => {
                        let icon = if *is_collapsed { "~>" } else { "~" };
                        format!("  {icon} {label}{selected}")
                    }
                }
            })
            .collect()
    })
}

#[gpui::test]
async fn test_sidebar_background_with_transparent_panel(cx: &mut TestAppContext) {
    let (_, project) = init_multi_project_test(&["/my-project"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    for panel_alpha in [0.0, 0.4] {
        set_sidebar_test_panel_alpha(panel_alpha, cx);
        let background = cx.update(|_, cx| {
            let colors = cx.theme().colors();
            colors.background.blend(colors.panel_background)
        });
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(400.), px(240.)),
            |_, _| sidebar.clone().into_any_element(),
        );

        cx.update(|window, _| {
            let quads = window.painted_quads();
            let sidebar_background = quads.first().expect("Sidebar should paint its background");
            assert_eq!(sidebar_background.background.as_solid(), Some(background));
        });
    }
}

#[gpui::test]
async fn test_sidebar_action_hover_contrasts_with_row(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    cx.update(|cx| AgentRegistryStore::init_test_global(cx, Vec::new()));
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    save_n_test_threads(1, &project, cx).await;

    for (query, panel_alpha) in [("my-project", 1.0), ("", 1.0), ("", 0.0), ("", 0.2)] {
        set_sidebar_test_panel_alpha(panel_alpha, cx);
        type_in_search(&sidebar, query, cx);
        let row_bounds = sidebar.read_with(cx, |sidebar, _| {
            sidebar
                .list_state
                .bounds_for_item(0)
                .expect("rendered project header")
        });
        if query.is_empty() {
            cx.simulate_mouse_move(row_bounds.center(), None, Modifiers::default());
            let hover =
                cx.update(|_, cx| u32::from(Rgba::from(cx.theme().colors().ghost_element_hover)));
            assert_eq!(
                sidebar_painted_background_at(row_bounds.center(), cx),
                hover
            );
        }
        for selector in ["ICON-Plus", "ICON-Ellipsis"] {
            // Debug bounds are keyed by icon, so the worktree header's own "+" hides the project's.
            let bounds = if query.is_empty() && selector == "ICON-Plus" {
                sidebar.read_with(cx, |sidebar, _| {
                    sidebar
                        .list_state
                        .bounds_for_item(1)
                        .expect("rendered worktree header")
                })
            } else {
                row_bounds
            };
            assert_sidebar_action_hover(selector, bounds, cx);
        }
        if query.is_empty() {
            let thread_bounds = sidebar.read_with(cx, |sidebar, _| {
                sidebar
                    .list_state
                    .bounds_for_item(2)
                    .expect("rendered thread")
            });
            for selector in ["ICON-Pencil", "ICON-Archive"] {
                assert_sidebar_action_hover(selector, thread_bounds, cx);
            }
        }
    }

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.show_archive(window, cx);
        let SidebarView::Archive(archive) = &sidebar.view else {
            panic!("Thread History should be open");
        };
        window.focus(&archive.focus_handle(cx), cx);
    });
    cx.run_until_parked();

    for (archived, selector) in [(false, "ICON-Archive"), (true, "ICON-Trash")] {
        if archived {
            cx.update(|_, cx| {
                ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                    let thread_id = store.entries().next().expect("seeded thread").thread_id;
                    store.archive(thread_id, None, cx);
                });
            });
            cx.run_until_parked();
        }
        cx.dispatch_action(SelectFirst);
        let row_bounds = cx.update(|window, cx| {
            window
                .painted_quads()
                .into_iter()
                .find(|quad| quad.border_color == cx.theme().colors().panel_focused_border)
                .expect("selected Thread History row")
                .bounds
                .map(|value| px(value.as_f32() / window.scale_factor()))
        });
        for panel_alpha in [1.0, 0.0, 0.2] {
            set_sidebar_test_panel_alpha(panel_alpha, cx);
            assert_sidebar_action_hover(selector, row_bounds, cx);
        }
    }
}

#[gpui::test]
async fn test_thread_metadata_update_preserves_sticky_header_measurements(cx: &mut TestAppContext) {
    let (fs, project_a) = init_multi_project_test(&["/project-a", "/project-b"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    add_test_project("/project-b", &fs, &multi_workspace, cx).await;

    save_thread_metadata(
        acp::SessionId::new(Arc::from("project-a-thread")),
        Some("Project A Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project_a,
        cx,
    );
    save_thread_metadata_with_main_paths(
        "project-b-thread",
        "Project B Thread",
        PathList::new(&[PathBuf::from("/project-b")]),
        PathList::new(&[PathBuf::from("/project-b")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        cx,
    );

    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(400.), px(240.)),
        |_, _| sidebar.clone().into_any_element(),
    );
    cx.run_until_parked();

    let next_header_ix = sidebar.read_with(cx, |sidebar, _| {
        assert!(
            sidebar.contents.project_header_indices.len() == 2,
            "test setup should render exctly two project headers"
        );
        sidebar.contents.project_header_indices[1]
    });

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.list_state.scroll_to(gpui::ListOffset {
            item_ix: next_header_ix - 1,
            offset_in_item: px(24.),
        });
        cx.notify();
    });
    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(400.), px(240.)),
        |_, _| sidebar.clone().into_any_element(),
    );
    cx.run_until_parked();

    let bounds_before = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .list_state
            .bounds_for_item(next_header_ix)
            .expect("next project header should be measured before metadata update")
    });

    save_thread_metadata(
        acp::SessionId::new(Arc::from("project-a-thread")),
        Some("Renamed Project A Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 1, 0).unwrap(),
        None,
        None,
        &project_a,
        cx,
    );

    let bounds_after = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .list_state
            .bounds_for_item(next_header_ix)
            .expect("same-shape metadata update should preserve next header measurements")
    });
    assert_eq!(bounds_before, bounds_after);
}

#[gpui::test]
async fn test_thread_status_update_does_not_reset_list_measurements(cx: &mut TestAppContext) {
    // When a thread's status changes (e.g. Running -> Completed after sending a message), the
    // shape sequence is unchanged, so `update_entries` should not reset the underlying
    // `ListState`. Resetting throws away measured item bounds for one frame, which makes the
    // sticky project header flicker between its pushed-off and fully-on-screen positions.
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(2, &project, cx).await;
    cx.run_until_parked();

    let before = sidebar.read_with(cx, |sidebar, app| {
        sidebar
            .entry_shapes(multi_workspace.read(app), app)
            .collect::<Vec<_>>()
    });
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
    let after = sidebar.read_with(cx, |sidebar, app| {
        sidebar
            .entry_shapes(multi_workspace.read(app), app)
            .collect::<Vec<_>>()
    });

    assert_eq!(
        before, after,
        "a no-op rebuild should produce an identical shape sequence"
    );
}

#[gpui::test]
async fn test_collapse_changes_entry_shape(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(2, &project, cx).await;
    cx.run_until_parked();

    let project_group_key = project.read_with(cx, |project, cx| project.project_group_key(cx));

    let before = sidebar.read_with(cx, |sidebar, app| {
        sidebar
            .entry_shapes(multi_workspace.read(app), app)
            .collect::<Vec<_>>()
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.toggle_collapse(&project_group_key, window, cx);
    });
    cx.run_until_parked();
    let after = sidebar.read_with(cx, |sidebar, app| {
        sidebar
            .entry_shapes(multi_workspace.read(app), app)
            .collect::<Vec<_>>()
    });

    assert_ne!(
        before, after,
        "collapsing the project group should change the shape sequence so the list resets"
    );
}

#[gpui::test]
async fn test_serialization_round_trip(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(3, &project, cx).await;

    let project_group_key = project.read_with(cx, |project, cx| project.project_group_key(cx));

    // Set a custom width and collapse the group.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.set_width(Some(px(420.0)), cx);
        sidebar.toggle_collapse(&project_group_key, window, cx);
    });
    cx.run_until_parked();

    // Capture the serialized state from the first sidebar.
    let serialized = sidebar.read_with(cx, |sidebar, cx| sidebar.serialized_state(cx));
    let serialized = serialized.expect("serialized_state should return Some");

    // Create a fresh sidebar and restore into it.
    let sidebar2 =
        cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    cx.run_until_parked();

    sidebar2.update_in(cx, |sidebar, window, cx| {
        sidebar.restore_serialized_state(&serialized, window, cx);
    });
    cx.run_until_parked();

    // Assert all serialized fields match.
    let width1 = sidebar.read_with(cx, |s, _| s.width);
    let width2 = sidebar2.read_with(cx, |s, _| s.width);

    assert_eq!(width1, width2);
    assert_eq!(width1, px(420.0));
}

#[gpui::test]
async fn test_width_reset_returns_configured_default(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    cx.update(|cx| set_threads_sidebar_default_width(360.0, cx));
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    assert_eq!(
        sidebar.read_with(cx, |sidebar, _| sidebar.width),
        px(360.0),
        "a fresh sidebar should open at the configured width"
    );

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.set_width(Some(px(420.0)), cx);
    });
    cx.run_until_parked();
    assert_eq!(sidebar.read_with(cx, |sidebar, _| sidebar.width), px(420.0));

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.set_width(None, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        sidebar.read_with(cx, |sidebar, _| sidebar.width),
        px(360.0),
        "resetting the width should return to the configured width, not a fixed default"
    );
}

#[gpui::test]
async fn test_width_setting_overrides_manual_resize(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    cx.update(|_window, cx| set_threads_sidebar_default_width(360.0, cx));
    cx.run_until_parked();
    assert_eq!(sidebar.read_with(cx, |sidebar, _| sidebar.width), px(360.0));
    assert!(!sidebar.read_with(cx, |sidebar, _| sidebar.width_set_by_user));

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.set_width(Some(px(420.0)), cx);
    });
    cx.update(|_window, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{"agent":{"threads_sidebar":{"default_width":360,"position":"right"}}}"#,
                    cx,
                )
                .expect("settings are valid");
        });
    });
    cx.run_until_parked();
    assert_eq!(sidebar.read_with(cx, |sidebar, _| sidebar.width), px(420.0));
    assert!(sidebar.read_with(cx, |sidebar, _| sidebar.width_set_by_user));

    cx.update(|_window, cx| set_threads_sidebar_default_width(500.0, cx));
    cx.run_until_parked();
    assert_eq!(sidebar.read_with(cx, |sidebar, _| sidebar.width), px(500.0));
    assert!(!sidebar.read_with(cx, |sidebar, _| sidebar.width_set_by_user));

    for (configured, expected) in [
        (5.0, THREADS_LIST_MIN_WIDTH),
        (360.0, px(360.0)),
        (5000.0, THREADS_LIST_MAX_WIDTH),
    ] {
        cx.update(|_window, cx| set_threads_sidebar_default_width(configured, cx));
        cx.run_until_parked();
        assert_eq!(sidebar.read_with(cx, |sidebar, _| sidebar.width), expected);
        let serialized = sidebar
            .read_with(cx, |sidebar, cx| sidebar.serialized_state(cx))
            .expect("sidebar state is serialized");
        let serialized: SerializedSidebar =
            serde_json::from_str(&serialized).expect("sidebar state is valid");
        assert_eq!(serialized.width, None);
        assert!(!serialized.width_set_by_user);
    }
}

#[gpui::test]
async fn test_restored_width_preserves_legacy_resizes(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

    for (state, expected, width_set_by_user) in [
        (r#"{"width":null}"#, 360.0, false),
        (r#"{"width":300.0}"#, 360.0, false),
        (r#"{"width":420.0}"#, 420.0, true),
        (r#"{"width":300.0,"width_set_by_user":true}"#, 300.0, true),
    ] {
        cx.update(|_window, cx| set_threads_sidebar_default_width(360.0, cx));
        let sidebar =
            cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
        cx.run_until_parked();
        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.restore_serialized_state(state, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar.width),
            px(expected)
        );
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar.width_set_by_user),
            width_set_by_user
        );

        let serialized = sidebar
            .read_with(cx, |sidebar, cx| sidebar.serialized_state(cx))
            .expect("sidebar state is serialized");
        let serialized: SerializedSidebar =
            serde_json::from_str(&serialized).expect("sidebar state is valid");
        assert_eq!(serialized.width, width_set_by_user.then_some(expected));
        assert_eq!(serialized.width_set_by_user, width_set_by_user);

        cx.update(|_window, cx| set_threads_sidebar_default_width(500.0, cx));
        cx.run_until_parked();
        assert_eq!(sidebar.read_with(cx, |sidebar, _| sidebar.width), px(500.0));
        assert!(!sidebar.read_with(cx, |sidebar, _| sidebar.width_set_by_user));
    }
}

#[gpui::test]
async fn test_configured_width_is_clamped_into_range(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    cx.update(|cx| set_threads_sidebar_default_width(5000.0, cx));
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

    let wide_sidebar =
        cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    cx.run_until_parked();
    assert_eq!(
        wide_sidebar.read_with(cx, |sidebar, _| sidebar.width),
        THREADS_LIST_MAX_WIDTH,
        "a configured width above the maximum should be clamped at construction"
    );

    wide_sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.set_width(None, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        wide_sidebar.read_with(cx, |sidebar, _| sidebar.width),
        THREADS_LIST_MAX_WIDTH,
        "resetting the width should not restore the unclamped configured width"
    );

    cx.update(|_window, cx| set_threads_sidebar_default_width(5.0, cx));
    let narrow_sidebar =
        cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    cx.run_until_parked();
    assert_eq!(
        narrow_sidebar.read_with(cx, |sidebar, _| sidebar.width),
        THREADS_LIST_MIN_WIDTH,
        "a configured width below the minimum should be clamped at construction"
    );
}

#[gpui::test]
async fn test_only_a_user_chosen_width_is_persisted(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    cx.update(|cx| set_threads_sidebar_default_width(360.0, cx));
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Serialization runs on many triggers besides resizing, so a sidebar the
    // user has never resized must not record a width at all.
    let untouched = sidebar
        .read_with(cx, |sidebar, cx| sidebar.serialized_state(cx))
        .expect("serialized_state should return Some");
    assert!(
        !untouched.contains("360"),
        "an unresized sidebar should not persist its width, got {untouched}"
    );

    // A legacy width matching the old default is ambiguous, so the setting takes precedence
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.restore_serialized_state(r#"{"width":300.0}"#, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        sidebar.read_with(cx, |sidebar, _| sidebar.width),
        px(360.0),
        "the legacy default width falls back to the setting"
    );

    // A width the user picked survives, and keeps surviving across restores.
    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.set_width(Some(px(420.0)), cx);
    });
    cx.run_until_parked();
    let resized = sidebar
        .read_with(cx, |sidebar, cx| sidebar.serialized_state(cx))
        .expect("serialized_state should return Some");

    let restored =
        cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    cx.run_until_parked();
    restored.update_in(cx, |sidebar, window, cx| {
        sidebar.restore_serialized_state(&resized, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        restored.read_with(cx, |sidebar, _| sidebar.width),
        px(420.0),
        "a user-chosen width should survive a restore"
    );

    // Resetting gives the width back to the setting, and stops persisting it.
    restored.update_in(cx, |sidebar, _window, cx| {
        sidebar.set_width(None, cx);
    });
    cx.run_until_parked();
    let after_reset = restored
        .read_with(cx, |sidebar, cx| sidebar.serialized_state(cx))
        .expect("serialized_state should return Some");
    assert!(
        !after_reset.contains("420"),
        "resetting should stop persisting the width, got {after_reset}"
    );
}

#[gpui::test]
async fn test_restore_serialized_archive_view_does_not_panic(cx: &mut TestAppContext) {
    // A regression test to ensure that restoring a serialized archive view does not panic.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.update(|_window, cx| {
        AgentRegistryStore::init_test_global(cx, vec![]);
    });

    let serialized = serde_json::to_string(&SerializedSidebar {
        width: Some(400.0),
        width_set_by_user: true,
        active_view: SerializedSidebarView::History,
        ..Default::default()
    })
    .expect("serialization should succeed");

    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        if let Some(sidebar) = multi_workspace.sidebar() {
            sidebar.restore_serialized_state(&serialized, window, cx);
        }
    });
    cx.run_until_parked();

    // After the deferred `show_archive` runs, the view should be Archive.
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(sidebar.view, SidebarView::Archive(_)),
            "expected sidebar view to be Archive after restore, got ThreadList"
        );
    });
}

#[gpui::test]
async fn test_entities_released_on_window_close(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let weak_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().downgrade());
    let weak_sidebar = sidebar.downgrade();
    let weak_multi_workspace = multi_workspace.downgrade();

    drop(sidebar);
    drop(multi_workspace);
    cx.update(|window, _cx| window.remove_window());
    cx.run_until_parked();

    weak_multi_workspace.assert_released();
    weak_sidebar.assert_released();
    weak_workspace.assert_released();
}

#[gpui::test]
async fn test_single_workspace_no_threads(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let (_sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    assert_eq!(
        visible_entries_as_strings(&_sidebar, cx),
        vec!["v [my-project]"]
    );
}

#[gpui::test]
async fn test_single_workspace_with_saved_threads(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-1")),
        Some("Fix crash in project panel".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-2")),
        Some("Add inline diff view".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Fix crash in project panel",
            "  Add inline diff view",
        ]
    );
}

#[gpui::test]
async fn test_workspace_lifecycle(cx: &mut TestAppContext) {
    let project = init_test_project("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Single workspace with a thread
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-a1")),
        Some("Thread A1".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project-a]",
            "  Thread A1",
        ]
    );

    // Add a second workspace
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.create_test_workspace(window, cx).detach();
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project-a]",
            "  Thread A1",
        ]
    );
}

#[gpui::test]
async fn test_rename_project_group(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(1, &project, cx).await;

    let project_group_key = project.read_with(cx, |project, cx| project.project_group_key(cx));

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.apply_project_rename(&project_group_key, " Renamed ".into(), cx);
        sidebar.update_entries(cx);
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [Renamed]",
            "  Thread 1",
        ]
    );

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.apply_project_rename(&project_group_key, "".into(), cx);
        sidebar.update_entries(cx);
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Thread 1",
        ]
    );
}

#[gpui::test]
async fn test_unchanged_project_rename_keeps_the_generated_name(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    save_n_test_threads(1, &project, cx).await;
    let key = project.read_with(cx, |project, cx| project.project_group_key(cx));
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.start_renaming_project(&key, window, cx);
    });
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.finish_entry_rename(window, cx);
    });
    cx.run_until_parked();

    let custom_name = multi_workspace.read_with(cx, |multi_workspace, cx| {
        multi_workspace
            .group_state_by_key(&key, cx)
            .and_then(|state| state.custom_name.clone())
    });
    assert_eq!(custom_name, None);
}

#[gpui::test]
async fn test_project_group_custom_name_survives_merge(cx: &mut TestAppContext) {
    let (_fs, project) = init_multi_project_test(&["/project-a", "/project-b"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    save_n_test_threads(1, &project, cx).await;

    let key = project.read_with(cx, |project, cx| project.project_group_key(cx));
    multi_workspace.update(cx, |multi_workspace, cx| {
        multi_workspace.test_add_project_group(
            workspace::ProjectGroup {
                key: ProjectGroupKey::new(
                    None,
                    PathList::new(&[
                        std::path::PathBuf::from("/project-a"),
                        std::path::PathBuf::from("/project-b"),
                    ]),
                ),
                workspaces: Vec::new(),
                expanded: true,
            },
            cx,
        );
    });
    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.apply_project_rename(&key, "Work".into(), cx);
    });

    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project-b", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [Work]",
            "  Thread 1",
        ]
    );
}

#[gpui::test]
async fn test_drag_project_header_to_reorder(cx: &mut TestAppContext) {
    let project = init_test_project("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    save_n_test_threads(1, &project, cx).await;
    multi_workspace.update(cx, |multi_workspace, cx| {
        for path in ["/project-b", "/project-c"] {
            multi_workspace.test_add_project_group(
                workspace::ProjectGroup {
                    key: ProjectGroupKey::new(
                        None,
                        PathList::new(&[std::path::PathBuf::from(path)]),
                    ),
                    workspaces: Vec::new(),
                    expanded: true,
                },
                cx,
            );
        }
    });
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    let header_names = |cx: &mut gpui::VisualTestContext| {
        visible_entries_as_strings(&sidebar, cx)
            .into_iter()
            .filter(|entry| entry.contains('['))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        header_names(cx),
        vec!["v [project-a]", "v [project-b]", "v [project-c]"]
    );

    let drag = |from: &str,
                to: &str,
                rows_below_target: usize,
                cx: &mut gpui::VisualTestContext| {
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(400.), px(400.)),
            |_, _| sidebar.clone().into_any_element(),
        );
        let center = |label: &str, rows_below: usize, cx: &mut gpui::VisualTestContext| {
            sidebar.read_with(cx, |sidebar, _| {
                let index = sidebar
                    .contents
                    .entries
                    .iter()
                    .position(|entry| {
                        matches!(entry, ListEntry::ProjectHeader { label: entry_label, .. } if entry_label.as_ref() == label)
                    })
                    .expect("header exists");
                sidebar
                    .list_state
                    .bounds_for_item(index + rows_below)
                    .expect("row is measured")
                    .origin
                    + gpui::point(px(40.), px(8.))
            })
        };
        let from = center(from, 0, cx);
        let to = center(to, rows_below_target, cx);
        cx.simulate_mouse_down(from, gpui::MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            from + gpui::point(px(0.), px(6.)),
            Some(gpui::MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_move(to, Some(gpui::MouseButton::Left), Modifiers::default());
        cx.run_until_parked();
        let drop_target = sidebar.read_with(cx, |sidebar, _| sidebar.project_drop_target.clone());
        cx.simulate_mouse_up(to, gpui::MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        drop_target.map(|(key, lands_below)| (key.path_list().paths()[0].clone(), lands_below))
    };

    let drop_target = drag("project-a", "project-c", 0, cx);
    assert_eq!(drop_target, Some(("/project-c".into(), true)));
    assert_eq!(
        header_names(cx),
        vec!["v [project-b]", "v [project-c]", "v [project-a]"]
    );

    let drop_target = drag("project-a", "project-b", 0, cx);
    assert_eq!(drop_target, Some(("/project-b".into(), false)));
    assert_eq!(
        header_names(cx),
        vec!["v [project-a]", "v [project-b]", "v [project-c]"]
    );

    // A thread row targets its group, and the group's header shows the line.
    let drop_target = drag("project-c", "project-a", 1, cx);
    assert_eq!(drop_target, Some(("/project-a".into(), false)));
    assert_eq!(
        sidebar.read_with(cx, |sidebar, _| sidebar.project_drop_target.clone()),
        None
    );

    sidebar.update_in(cx, |sidebar, _window, cx| {
        let dragged_key = ProjectGroupKey::new(
            None,
            PathList::new(&[std::path::PathBuf::from("/project-b")]),
        );
        sidebar.project_drop_target = Some((dragged_key.clone(), true));
        sidebar.update_project_drop_target(&dragged_key, None, cx);
        assert_eq!(sidebar.project_drop_target, None);
    });
    assert_eq!(
        header_names(cx),
        vec!["v [project-c]", "v [project-a]", "v [project-b]"]
    );
}

#[gpui::test]
async fn test_collapse_and_expand_group(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(1, &project, cx).await;

    let project_group_key = project.read_with(cx, |project, cx| project.project_group_key(cx));

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Thread 1",
        ]
    );

    // Collapse
    sidebar.update_in(cx, |s, window, cx| {
        s.toggle_collapse(&project_group_key, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "> [my-project]",
        ]
    );

    // Expand
    sidebar.update_in(cx, |s, window, cx| {
        s.toggle_collapse(&project_group_key, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Thread 1",
        ]
    );
}

#[gpui::test]
async fn test_collapse_state_survives_worktree_key_change(cx: &mut TestAppContext) {
    // When a worktree is added to a project, the project group key changes.
    // The sidebar's collapsed/expanded state is keyed by ProjectGroupKey, so
    // UI state must survive the key change.
    let (_fs, project) = init_multi_project_test(&["/project-a", "/project-b"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(2, &project, cx).await;
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [project-a]", "  Thread 2", "  Thread 1",]
    );

    // Collapse the group.
    let old_key = project.read_with(cx, |project, cx| project.project_group_key(cx));
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.toggle_collapse(&old_key, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["> [project-a]"]
    );

    // Add a second worktree — the key changes from [/project-a] to
    // [/project-a, /project-b].
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project-b", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    // The group should still be collapsed under the new key.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["> [project-a / project-b]"]
    );
}

#[gpui::test]
async fn test_visible_entries_as_strings(cx: &mut TestAppContext) {
    use workspace::ProjectGroup;

    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let expanded_path = PathList::new(&[std::path::PathBuf::from("/expanded")]);
    let collapsed_path = PathList::new(&[std::path::PathBuf::from("/collapsed")]);

    // Set the collapsed group state through multi_workspace
    multi_workspace.update(cx, |multi_workspace, cx| {
        multi_workspace.test_add_project_group(
            ProjectGroup {
                key: ProjectGroupKey::new(None, collapsed_path.clone()),
                workspaces: Vec::new(),
                expanded: false,
            },
            cx,
        );
    });

    sidebar.update_in(cx, |s, _window, _cx| {
        let notified_thread_id = ThreadId::new();
        s.contents.notified_threads.insert(notified_thread_id);
        s.contents.entries = vec![
            // Expanded project header
            ListEntry::ProjectHeader {
                key: ProjectGroupKey::new(None, expanded_path.clone()),
                label: "expanded-project".into(),
                highlight_positions: Vec::new(),
                has_running_threads: false,
                waiting_thread_count: 0,
                has_notifications: false,
                is_active: true,
                has_threads: true,
            },
            ListEntry::Thread(Arc::new(ThreadEntry {
                metadata: ThreadMetadata {
                    thread_id: ThreadId::new(),
                    session_id: Some(acp::SessionId::new(Arc::from("t-1"))),
                    agent_id: AgentId::new("zed-agent"),
                    worktree_paths: WorktreePaths::default(),
                    title: Some("Completed thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: Some(Utc::now()),
                    interacted_at: None,
                    archived: false,
                    remote_connection: None,
                },
                icon: IconName::ZedAgent,
                icon_from_external_svg: None,
                status: AgentThreadStatus::Completed,
                workspace: ThreadEntryWorkspace::Open(workspace.clone()),
                is_live: false,
                is_background: false,
                is_title_generating: false,
                draft: None,
                highlight_positions: Vec::new(),
                worktrees: Vec::new(),
                diff_stats: DiffStats::default(),
            })),
            // Active thread with Running status
            ListEntry::Thread(Arc::new(ThreadEntry {
                metadata: ThreadMetadata {
                    thread_id: ThreadId::new(),
                    session_id: Some(acp::SessionId::new(Arc::from("t-2"))),
                    agent_id: AgentId::new("zed-agent"),
                    worktree_paths: WorktreePaths::default(),
                    title: Some("Running thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: Some(Utc::now()),
                    interacted_at: None,
                    archived: false,
                    remote_connection: None,
                },
                icon: IconName::ZedAgent,
                icon_from_external_svg: None,
                status: AgentThreadStatus::Running,
                workspace: ThreadEntryWorkspace::Open(workspace.clone()),
                is_live: true,
                is_background: false,
                is_title_generating: false,
                draft: None,
                highlight_positions: Vec::new(),
                worktrees: Vec::new(),
                diff_stats: DiffStats::default(),
            })),
            // Active thread with Error status
            ListEntry::Thread(Arc::new(ThreadEntry {
                metadata: ThreadMetadata {
                    thread_id: ThreadId::new(),
                    session_id: Some(acp::SessionId::new(Arc::from("t-3"))),
                    agent_id: AgentId::new("zed-agent"),
                    worktree_paths: WorktreePaths::default(),
                    title: Some("Error thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: Some(Utc::now()),
                    interacted_at: None,
                    archived: false,
                    remote_connection: None,
                },
                icon: IconName::ZedAgent,
                icon_from_external_svg: None,
                status: AgentThreadStatus::Error,
                workspace: ThreadEntryWorkspace::Open(workspace.clone()),
                is_live: true,
                is_background: false,
                is_title_generating: false,
                draft: None,
                highlight_positions: Vec::new(),
                worktrees: Vec::new(),
                diff_stats: DiffStats::default(),
            })),
            // Thread with WaitingForConfirmation status, not active
            // remote_connection: None,
            ListEntry::Thread(Arc::new(ThreadEntry {
                metadata: ThreadMetadata {
                    thread_id: ThreadId::new(),
                    session_id: Some(acp::SessionId::new(Arc::from("t-4"))),
                    agent_id: AgentId::new("zed-agent"),
                    worktree_paths: WorktreePaths::default(),
                    title: Some("Waiting thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: Some(Utc::now()),
                    interacted_at: None,
                    archived: false,
                    remote_connection: None,
                },
                icon: IconName::ZedAgent,
                icon_from_external_svg: None,
                status: AgentThreadStatus::WaitingForConfirmation,
                workspace: ThreadEntryWorkspace::Open(workspace.clone()),
                is_live: false,
                is_background: false,
                is_title_generating: false,
                draft: None,
                highlight_positions: Vec::new(),
                worktrees: Vec::new(),
                diff_stats: DiffStats::default(),
            })),
            // Background thread that completed (should show notification)
            // remote_connection: None,
            ListEntry::Thread(Arc::new(ThreadEntry {
                metadata: ThreadMetadata {
                    thread_id: notified_thread_id,
                    session_id: Some(acp::SessionId::new(Arc::from("t-5"))),
                    agent_id: AgentId::new("zed-agent"),
                    worktree_paths: WorktreePaths::default(),
                    title: Some("Notified thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: Some(Utc::now()),
                    interacted_at: None,
                    archived: false,
                    remote_connection: None,
                },
                icon: IconName::ZedAgent,
                icon_from_external_svg: None,
                status: AgentThreadStatus::Completed,
                workspace: ThreadEntryWorkspace::Open(workspace.clone()),
                is_live: true,
                is_background: true,
                is_title_generating: false,
                draft: None,
                highlight_positions: Vec::new(),
                worktrees: Vec::new(),
                diff_stats: DiffStats::default(),
            })),
            // Collapsed project header
            ListEntry::ProjectHeader {
                key: ProjectGroupKey::new(None, collapsed_path.clone()),
                label: "collapsed-project".into(),
                highlight_positions: Vec::new(),
                has_running_threads: false,
                waiting_thread_count: 0,
                has_notifications: false,
                is_active: false,
                has_threads: false,
            },
        ];

        // Select the Running thread (index 2)
        s.selection = Some(2);
    });

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [expanded-project]",
            "  Completed thread",
            "  Running thread * (running)  <== selected",
            "  Error thread * (error)",
            "  Waiting thread (waiting)",
            "  Notified thread * (!)",
            "> [collapsed-project]",
        ]
    );

    // Move selection to the collapsed header
    sidebar.update_in(cx, |s, _window, _cx| {
        s.selection = Some(6);
    });

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx).last().cloned(),
        Some("> [collapsed-project]  <== selected".to_string()),
    );

    // Clear selection
    sidebar.update_in(cx, |s, _window, _cx| {
        s.selection = None;
    });

    // No entry should have the selected marker
    let entries = visible_entries_as_strings(&sidebar, cx);
    for entry in &entries {
        assert!(
            !entry.contains("<== selected"),
            "unexpected selection marker in: {}",
            entry
        );
    }
}

#[gpui::test]
async fn test_keyboard_select_next_and_previous(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(3, &project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Entries: [header, worktree header, thread3, thread2, thread1]
    // Focusing the sidebar does not set a selection; select_next/select_previous
    // handle None gracefully by starting from the first or last entry.
    focus_sidebar(&sidebar, cx);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);

    // First SelectNext from None starts at index 0
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));

    // Move down through remaining entries
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));

    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(2));

    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(3));

    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(4));

    // At the end, wraps back to first entry
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));

    // Navigate back to the end
    for expected in 1..=4 {
        cx.dispatch_action(SelectNext);
        assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(expected));
    }

    // Move back up
    for expected in (1..=3).rev() {
        cx.dispatch_action(SelectPrevious);
        assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(expected));
    }

    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));

    // At the top, selection clears (focus returns to editor)
    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);
}

#[gpui::test]
async fn test_keyboard_select_first_and_last(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(3, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);

    // SelectLast jumps to the end
    cx.dispatch_action(SelectLast);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(4));

    // SelectFirst jumps to the beginning
    cx.dispatch_action(SelectFirst);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));
}

#[gpui::test]
async fn test_refocus_sidebar_with_no_selection_focuses_search(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_handle(cx).focus(window, cx);
    });
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        assert!(!sidebar.focus_handle.is_focused(window));
        assert!(!sidebar.filter_editor.read(cx).is_focused(window));
        assert_eq!(sidebar.selection, None);
    });

    // Refocusing with no selection sends focus to search without selecting a row.
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        assert!(sidebar.filter_editor.read(cx).is_focused(window));
        assert_eq!(sidebar.selection, None);
    });
}

#[gpui::test]
async fn test_refocus_sidebar_with_selection_preserves_it(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    sidebar.update(cx, |sidebar, cx| {
        sidebar.selection = Some(0);
        cx.notify();
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_handle(cx).focus(window, cx);
    });
    cx.run_until_parked();

    // Refocusing with a selection returns focus to the list and keeps that row selected.
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, _cx| {
        assert!(sidebar.focus_handle.is_focused(window));
        assert_eq!(sidebar.selection, Some(0));
    });
}

#[gpui::test]
async fn test_leaving_sidebar_after_previous_focus_target_is_removed(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar_closed(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });
    let title_bar = cx.new(TestItem::new);
    let panel = workspace.update_in(cx, |workspace, window, cx| {
        workspace.set_titlebar_item(AnyView::from(title_bar.clone()), window, cx);
        let panel = cx.new(|cx| TestPanel::new(DockPosition::Right, 100, cx));
        workspace.add_panel(panel.clone(), window, cx);
        workspace.toggle_dock(DockPosition::Right, window, cx);
        panel
    });
    cx.run_until_parked();
    panel.update(cx, |_, cx| cx.emit(PanelEvent::ZoomIn));
    cx.run_until_parked();
    title_bar.update_in(cx, |title_bar, window, cx| {
        title_bar.focus_handle(cx).focus(window, cx);
    });
    cx.run_until_parked();
    let focus_state = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            let focused_region = [
                (FocusedRegion::TitleBar, title_bar.focus_handle(cx)),
                (FocusedRegion::Sidebar, sidebar.focus_handle(cx)),
                (FocusedRegion::Panel, panel.focus_handle(cx)),
                (FocusedRegion::Pane, workspace.focus_handle(cx)),
            ]
            .into_iter()
            .find_map(|(region, focus_handle)| {
                focus_handle.contains_focused(window, cx).then_some(region)
            });
            (focused_region, panel.read(cx).zoomed)
        })
    };
    assert_eq!(focus_state(cx), (Some(FocusedRegion::TitleBar), true));

    cx.dispatch_action(FocusWorkspaceSidebar);
    cx.run_until_parked();
    assert_eq!(focus_state(cx), (Some(FocusedRegion::Sidebar), true));

    cx.dispatch_action(FocusWorkspaceSidebar);
    cx.run_until_parked();
    assert_eq!(focus_state(cx), (Some(FocusedRegion::TitleBar), true));

    cx.dispatch_action(FocusWorkspaceSidebar);
    cx.run_until_parked();
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.clear_titlebar_item(window, cx);
    });
    cx.run_until_parked();
    assert_eq!(focus_state(cx), (Some(FocusedRegion::Sidebar), true));

    cx.dispatch_action(FocusWorkspaceSidebar);
    cx.run_until_parked();
    assert_eq!(focus_state(cx), (Some(FocusedRegion::Panel), true));
}

#[gpui::test]
async fn test_window_frame_corners_follow_content(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    setup_sidebar_closed(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });
    let window_handle = cx.update(|window, _| window.window_handle());
    cx.simulate_window_decorations_change(
        window_handle,
        Decorations::Client {
            tiling: Tiling::default(),
        },
    );
    let title_bar = cx.new(|cx| {
        PlatformTitleBar::new("title-bar", cx).with_multi_workspace(multi_workspace.downgrade())
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.set_titlebar_item(AnyView::from(title_bar), window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        window_corners(cx),
        WindowCorners {
            top_left: WindowCorner::Rounded,
            top_right: WindowCorner::Rounded,
            bottom_left: WindowCorner::Rounded,
            bottom_right: WindowCorner::Rounded,
        },
        "title bar and status bar shown"
    );

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.clear_titlebar_item(window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        window_corners(cx),
        WindowCorners {
            top_left: WindowCorner::Square,
            top_right: WindowCorner::Square,
            bottom_left: WindowCorner::Rounded,
            bottom_right: WindowCorner::Rounded,
        },
        "title bar hidden"
    );

    cx.update_global::<SettingsStore, _>(|store, cx| {
        store.update_user_settings(cx, |settings| {
            settings.status_bar.get_or_insert_default().show = Some(false);
        });
    });
    cx.run_until_parked();
    assert_eq!(
        window_corners(cx),
        WindowCorners {
            top_left: WindowCorner::Square,
            top_right: WindowCorner::Square,
            bottom_left: WindowCorner::Square,
            bottom_right: WindowCorner::Square,
        },
        "title bar and status bar hidden"
    );

    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.toggle_sidebar(window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        window_corners(cx),
        WindowCorners {
            top_left: WindowCorner::Rounded,
            top_right: WindowCorner::Square,
            bottom_left: WindowCorner::Rounded,
            bottom_right: WindowCorner::Square,
        },
        "title bar and status bar hidden, sidebar open on the left"
    );

    cx.update(|_, cx| {
        let mut settings = AgentSettings::get_global(cx).clone();
        settings.threads_sidebar.position = SidebarDockPosition::Right;
        AgentSettings::override_global(settings, cx);
    });
    multi_workspace.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    assert_eq!(
        window_corners(cx),
        WindowCorners {
            top_left: WindowCorner::Square,
            top_right: WindowCorner::Rounded,
            bottom_left: WindowCorner::Square,
            bottom_right: WindowCorner::Rounded,
        },
        "title bar and status bar hidden, sidebar open on the right"
    );
}

#[cfg(target_os = "macos")]
#[gpui::test]
async fn test_headers_follow_title_bar_visibility(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    cx.update(|cx| AgentRegistryStore::init_test_global(cx, Vec::new()));
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let thread_list = header_search_offsets("sidebar-header-search", cx);
    assert_eq!(thread_list.hidden, thread_list.fullscreen);
    assert_ne!(thread_list.hidden, thread_list.shown);

    sidebar.update_in(cx, |sidebar, window, cx| sidebar.show_archive(window, cx));
    cx.run_until_parked();
    let archive = header_search_offsets("threads-archive-header-search", cx);
    assert_eq!(archive.hidden, archive.fullscreen);
    assert_ne!(archive.hidden, archive.shown);
}

#[gpui::test]
async fn test_header_alignment_with_title_bar_and_tabs(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    cx.update(|cx| AgentRegistryStore::init_test_global(cx, Vec::new()));
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let title_bar = cx.new(|cx| {
        PlatformTitleBar::new("title-bar", cx).with_multi_workspace(multi_workspace.downgrade())
    });
    workspace.update_in(cx, |workspace, window, cx| {
        let item = cx.new(TestItem::new);
        workspace.add_item_to_active_pane(Box::new(item), None, false, window, cx);
    });
    let window_handle = cx.update(|window, _| window.window_handle());

    for (header_selector, search_selector) in [
        ("sidebar-header", "sidebar-header-search"),
        ("threads-archive-header", "threads-archive-header-search"),
    ] {
        if header_selector == "threads-archive-header" {
            sidebar.update_in(cx, |sidebar, window, cx| sidebar.show_archive(window, cx));
        }
        for ((font_size, density, decorations, position), scale_factor) in [
            (
                12.,
                UiDensity::Compact,
                Decorations::Server,
                SidebarDockPosition::Left,
            ),
            (
                16.,
                UiDensity::Default,
                Decorations::Client {
                    tiling: Tiling::default(),
                },
                SidebarDockPosition::Left,
            ),
            (
                24.,
                UiDensity::Comfortable,
                Decorations::Client {
                    tiling: Tiling::default(),
                },
                SidebarDockPosition::Right,
            ),
            (
                16.,
                UiDensity::Comfortable,
                Decorations::Client {
                    tiling: Tiling {
                        top: true,
                        left: true,
                        right: true,
                        bottom: true,
                    },
                },
                SidebarDockPosition::Right,
            ),
        ]
        .into_iter()
        .cartesian_product([1., 2.])
        {
            cx.simulate_scale_factor_change(scale_factor);
            cx.simulate_window_decorations_change(window_handle, decorations);
            for (visibility, fullscreen, title_bar_visible) in [
                (TitleBarVisibility::Always, false, true),
                (TitleBarVisibility::Never, false, false),
                (TitleBarVisibility::HideInFullScreen, false, true),
                (TitleBarVisibility::HideInFullScreen, true, false),
                (TitleBarVisibility::Always, true, true),
            ] {
                cx.update_global::<SettingsStore, _>(|store, cx| {
                    store.update_user_settings(cx, |settings| {
                        settings.title_bar.get_or_insert_default().show = Some(visibility);
                        settings.theme.ui_font_size = Some(FontSize(font_size));
                        settings.theme.ui_density = Some(density);
                        settings
                            .agent
                            .get_or_insert_default()
                            .threads_sidebar
                            .get_or_insert_default()
                            .position = Some(position);
                    });
                });
                workspace.update_in(cx, |workspace, window, cx| {
                    if window.is_fullscreen() != fullscreen {
                        window.toggle_fullscreen();
                    }
                    if title_bar_visible {
                        workspace.set_titlebar_item(AnyView::from(title_bar.clone()), window, cx);
                    } else {
                        workspace.clear_titlebar_item(window, cx);
                    }
                    cx.refresh_windows();
                });
                cx.run_until_parked();

                let header = cx
                    .debug_bounds(header_selector)
                    .expect("header is rendered");
                let tab = cx.debug_bounds("TAB-0").expect("tab is rendered");
                let search = cx
                    .debug_bounds(search_selector)
                    .expect("search is rendered");
                let center = if title_bar_visible {
                    assert_eq!(header.bottom(), tab.top());
                    match decorations {
                        Decorations::Client { .. } => header.center().y,
                        Decorations::Server => header.center().y - px(1.),
                    }
                } else {
                    assert_eq!(header.top(), tab.top());
                    assert_eq!(header.bottom(), tab.bottom());
                    tab.center().y - px(0.5)
                };
                let expected_top =
                    ((center - search.size.height / 2.) * scale_factor).floor() / scale_factor;
                assert_eq!(
                    search.top(),
                    expected_top,
                    "{header_selector}, {visibility:?}, {font_size}px, {scale_factor}x"
                );
            }
        }
    }
}

#[gpui::test]
async fn test_keyboard_confirm_on_project_header_toggles_collapse(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Thread 1",
        ]
    );

    // Focus the sidebar and select the header
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(0);
    });

    // Confirm on project header collapses the group
    cx.dispatch_action(Confirm);
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "> [my-project]  <== selected",
        ]
    );

    // Confirm again expands the group
    cx.dispatch_action(Confirm);
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]  <== selected",
            "  Thread 1",
        ]
    );
}

#[gpui::test]
async fn test_keyboard_expand_and_collapse_selected_entry(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Thread 1",
        ]
    );

    // Focus sidebar and manually select the header (index 0). Press left to collapse.
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(0);
    });

    cx.dispatch_action(SelectParent);
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "> [my-project]  <== selected",
        ]
    );

    // Press right to expand
    cx.dispatch_action(SelectChild);
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]  <== selected",
            "  Thread 1",
        ]
    );

    // Press right again on already-expanded header moves selection down
    cx.dispatch_action(SelectChild);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));
}

#[gpui::test]
async fn test_keyboard_collapse_from_child_selects_parent(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Focus sidebar (selection starts at None), then navigate down to the thread (child)
    focus_sidebar(&sidebar, cx);
    cx.dispatch_action(SelectNext);
    cx.dispatch_action(SelectNext);
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(2));

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Thread 1  <== selected",
        ]
    );

    // Pressing left on a child collapses the parent group and selects it
    cx.dispatch_action(SelectParent);
    cx.run_until_parked();

    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "> [my-project]  <== selected",
        ]
    );
}

#[gpui::test]
async fn test_keyboard_navigation_on_empty_list(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/empty-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // An empty project has only the header (no auto-created draft).
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [empty-project]"]
    );

    // Focus sidebar — focus_in does not set a selection
    focus_sidebar(&sidebar, cx);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);

    // First SelectNext from None starts at index 0 (header)
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));

    // The project's main worktree always has a header, even without threads.
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));

    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));

    // SelectPrevious from first entry clears selection (returns to editor)
    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);

    // SelectPrevious from None selects the last entry
    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));
}

#[gpui::test]
async fn test_headers_truncate_without_fades_on_hover(cx: &mut TestAppContext) {
    let path = "/a-really-long-project-name-that-cannot-fit-in-a-narrow-sidebar";
    let project = init_test_project_with_agent_panel(path, cx).await;
    project.read_with(cx, |project, _| {
        project.fs().as_fake().set_branch_name(
            &Path::new(path).join(".git"),
            Some("a-long-feature-branch-name"),
        );
    });
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    save_n_test_threads(1, &project, cx).await;
    sidebar.update(cx, |sidebar, cx| {
        sidebar.set_width(Some(THREADS_LIST_MIN_WIDTH), cx)
    });
    set_sidebar_test_panel_alpha(1., cx);
    let projectless = cx
        .debug_bounds("projectless-header")
        .expect("No Project header");
    let project = sidebar.read_with(cx, |sidebar, _| {
        sidebar.list_state.bounds_for_item(0).unwrap()
    });
    let has_fade = |row: Bounds<Pixels>, cx: &mut VisualTestContext| {
        cx.update(|window, _| {
            let inset = if row == project { px(100.) } else { px(15.) };
            let probe = point(row.right() - inset, row.center().y).scale(window.scale_factor());
            window.painted_quads().iter().any(|quad| {
                quad.background.as_solid().is_none()
                    && quad
                        .bounds
                        .intersect(&quad.content_mask.bounds)
                        .contains(&probe)
            })
        })
    };
    for (hovered, idle) in [(projectless, project), (project, projectless)] {
        cx.simulate_mouse_move(hovered.center(), None, Modifiers::none());
        assert!(!has_fade(hovered, cx));
        assert!(!has_fade(idle, cx));
    }
    let hovered_width = cx.debug_bounds("header-group-0-labels").unwrap().size.width;
    let disclosure = cx
        .debug_bounds("header-group-0-disclosure")
        .expect("disclosure");
    let menu = cx.debug_bounds("ICON-Ellipsis").expect("project menu");
    assert!(project.contains(&disclosure.center()));
    assert!(disclosure.right() <= menu.left());
    cx.simulate_mouse_move(projectless.center(), None, Modifiers::none());
    let idle_labels = cx.debug_bounds("header-group-0-labels").unwrap();
    assert!(
        idle_labels.size.width > hovered_width,
        "idle {idle_labels:?}, hovered {hovered_width:?}"
    );
    assert!(project.right() - idle_labels.right() <= px(8.));
    let worktree = sidebar.read_with(cx, |sidebar, _| {
        sidebar.list_state.bounds_for_item(1).unwrap()
    });
    let worktree_labels =
        |cx: &mut VisualTestContext| cx.debug_bounds("worktree-header-1-labels").unwrap();
    let idle_width = worktree_labels(cx).size.width;
    assert!(worktree.right() - worktree_labels(cx).right() <= px(8.));
    cx.simulate_mouse_move(worktree.center(), None, Modifiers::none());
    assert!(!has_fade(worktree, cx));
    assert!(worktree_labels(cx).size.width < idle_width);
    let disclosure = cx
        .debug_bounds("worktree-header-1-disclosure")
        .expect("worktree disclosure");
    // Debug bounds are keyed by icon, so the worktree header's own "+" hides the project's.
    let new_thread = cx.debug_bounds("ICON-Plus").expect("new thread button");
    assert!(worktree_labels(cx).right() <= disclosure.left());
    assert!(disclosure.right() <= new_thread.left());
    assert!(new_thread.right() <= worktree.right());
    cx.simulate_mouse_move(projectless.center(), None, Modifiers::none());
    assert_eq!(worktree_labels(cx).size.width, idle_width);
    assert_eq!(project.top() - projectless.bottom(), px(2.));
}

#[gpui::test]
async fn test_new_projectless_entry(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
    let project = project::Project::test(fs, [], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    assert!(panel.read_with(cx, |panel, _| panel.active_conversation_view().is_none()));
    let header = cx
        .debug_bounds("projectless-header")
        .expect("No Project header");
    let new_thread_button = gpui::point(header.right() - px(18.), header.center().y);
    cx.simulate_mouse_move(header.center(), None, Modifiers::none());
    cx.simulate_click(new_thread_button, Modifiers::none());
    cx.run_until_parked();
    let thread_id = panel
        .read_with(cx, |panel, cx| panel.active_thread_id(cx))
        .expect("new thread");

    let shows_thread = |cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _| {
            assert!(!sidebar.contents.has_open_projects);
            assert!(sidebar.contents.has_projectless_threads);
            matches!(sidebar.contents.entries.first(), Some(ListEntry::Thread(thread))
                if thread.metadata.thread_id == thread_id && thread.metadata.is_projectless())
        })
    };
    assert!(shows_thread(cx));
    let divider = |cx: &mut VisualTestContext| cx.debug_bounds("projectless-divider").unwrap();
    let thread_icon = |cx: &mut VisualTestContext| cx.debug_bounds("icon-thread-entry-0").unwrap();
    assert!(thread_icon(cx).bottom() <= divider(cx).top());
    save_named_thread_metadata("older", "Older folderless thread", &project, cx).await;
    let second_icon = cx.debug_bounds("icon-thread-entry-1").unwrap();
    assert!(thread_icon(cx).bottom() <= second_icon.top());
    assert!(second_icon.bottom() <= divider(cx).top());
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.select_next(&SelectNext, window, cx);
        assert_eq!(sidebar.selection, Some(0));
    });

    cx.simulate_click(header.center(), Modifiers::none());
    cx.run_until_parked();
    assert!(!shows_thread(cx));
    assert_eq!(
        cx.debug_bounds("projectless-header").unwrap().bottom(),
        divider(cx).top()
    );
    assert_eq!(sidebar.read_with(cx, |sidebar, _| sidebar.selection), None);
    let collapsed_state =
        sidebar.read_with(cx, |sidebar, cx| sidebar.serialized_state(cx).unwrap());

    type_in_search(&sidebar, "No Project", cx);
    assert!(shows_thread(cx));
    assert!(thread_icon(cx).bottom() <= divider(cx).top());
    type_in_search(&sidebar, "", cx);
    assert!(!shows_thread(cx));

    cx.simulate_click(header.center(), Modifiers::none());
    cx.run_until_parked();
    assert!(shows_thread(cx));

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.restore_serialized_state(&collapsed_state, window, cx);
    });
    cx.run_until_parked();
    assert!(!shows_thread(cx));
    cx.simulate_click(new_thread_button, Modifiers::none());
    cx.run_until_parked();
    assert!(sidebar.read_with(cx, |sidebar, _| !sidebar.projectless_collapsed));
}

#[gpui::test]
async fn test_create_projectless_thread_from_open_project(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/repo", cx).await;
    let (window_group, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, project_panel) = setup_sidebar_with_agent_panel(&window_group, cx);
    save_n_test_threads(40, &project, cx).await;
    let project_workspace = window_group.read_with(cx, |group, _| group.workspace().clone());
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.new_projectless_thread(&NewProjectlessThread, window, cx);
    });
    cx.run_until_parked();
    let host = window_group.read_with(cx, |group, cx| {
        assert!(group.is_workspace_retained(&project_workspace));
        let host = group.workspace();
        assert_ne!(host, &project_workspace);
        assert!(host.read(cx).root_paths(cx).is_empty());
        host.clone()
    });
    assert!(project_panel.read_with(cx, |panel, _| panel.active_conversation_view().is_none()));
    let id = host
        .read_with(cx, |ws, cx| {
            ws.panel::<AgentPanel>(cx)
                .unwrap()
                .read(cx)
                .active_thread_id(cx)
        })
        .unwrap();
    sidebar.read_with(cx, |sidebar, _| {
        assert!(matches!(sidebar.contents.entries.first(), Some(ListEntry::Thread(thread)) if thread.metadata.thread_id == id && thread.metadata.is_projectless()));
        assert_eq!(sidebar.contents.project_header_indices.first(), Some(&1));
    });
    let header = cx.debug_bounds("projectless-header").unwrap();
    let divider = cx.debug_bounds("projectless-divider").unwrap();
    let folderless_icon = cx.debug_bounds("icon-thread-entry-0").unwrap();
    let project_icon = cx.debug_bounds("icon-thread-entry-3").unwrap();
    assert_eq!(folderless_icon.left(), project_icon.left());
    assert!(header.bottom() <= folderless_icon.top());
    assert!(folderless_icon.bottom() <= divider.top());
    assert_eq!(divider.size.height, px(2.));
    assert_eq!(
        sidebar.read_with(cx, |sidebar, _| sidebar
            .list_state
            .bounds_for_item(0)
            .unwrap()
            .top()),
        divider.bottom()
    );
    sidebar.update_in(cx, |sidebar, window, cx| {
        assert_eq!(
            sidebar.list_state.item_count(),
            sidebar.contents.entries.len() - 1
        );
        sidebar.select_last(&SelectLast, window, cx);
    });
    cx.run_until_parked();
    let scroll_top = sidebar.read_with(cx, |sidebar, _| sidebar.list_state.logical_scroll_top());
    assert!(scroll_top.item_ix > 0);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.select_first(&SelectFirst, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        sidebar.read_with(cx, |sidebar, _| sidebar.list_state.logical_scroll_top()),
        scroll_top
    );
    assert_eq!(cx.debug_bounds("projectless-header"), Some(header));
    assert_eq!(
        cx.debug_bounds("icon-thread-entry-0"),
        Some(folderless_icon)
    );
    assert_eq!(cx.debug_bounds("projectless-divider"), Some(divider));
    sidebar.update(cx, |sidebar, cx| {
        sidebar.selection = Some(1);
        sidebar.set_projectless_collapsed(true, cx);
        assert_eq!(sidebar.selection, Some(0));
        sidebar.set_projectless_collapsed(false, cx);
        assert_eq!(sidebar.selection, Some(1));
    });
}

#[gpui::test]
async fn test_projectless_threads_keep_their_workspace_across_windows(cx: &mut TestAppContext) {
    use_unique_metadata_databases(cx);
    agent_ui::test_support::init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/notes", serde_json::json!({})).await;
    cx.update(|cx| {
        <dyn Fs>::set_global(fs.clone(), cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let first = project::Project::test(fs.clone(), [], cx).await;
    let second = project::Project::test(fs, [], cx).await;
    let mut other_context = cx.clone();
    let (first_window, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(first, window, cx));
    let (other_window, other_cx) =
        other_context.add_window_view(|window, cx| MultiWorkspace::test_new(second, window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&first_window, cx);
    let (_, other_panel) = setup_sidebar_with_agent_panel(&other_window, other_cx);
    let first_host =
        first_window.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let other_host = other_window.read_with(other_cx, |multi_workspace, _| {
        multi_workspace.workspace().clone()
    });

    let connection = StubAgentConnection::new()
        .with_agent_id("folderless-agent".into())
        .with_supports_load_session(true);
    for (panel, cx) in [(&panel, &mut *cx), (&other_panel, &mut *other_cx)] {
        open_thread_with_custom_connection(panel, connection.clone(), cx);
        send_message(panel, cx);
    }
    let session = active_session_id(&panel, cx);
    let other_session = active_session_id(&other_panel, other_cx);
    let store = cx.update(|_, cx| ThreadMetadataStore::global(cx));
    let metadata = |session: &acp::SessionId, cx: &gpui::VisualTestContext| {
        store
            .read_with(cx, |store, _| store.entry_by_session(session).cloned())
            .expect("saved thread")
    };
    let first_id = metadata(&session, cx).thread_id;
    let other_metadata = metadata(&other_session, cx);
    let notes = first_window
        .update_in(cx, |multi_workspace, window, cx| {
            multi_workspace.open_project(vec!["/notes".into()], OpenMode::Activate, window, cx)
        })
        .await
        .expect("open plain folder");
    cx.run_until_parked();
    assert_ne!(notes, first_host);
    assert!(first_window.read_with(cx, |multi_workspace, cx| {
        multi_workspace.workspaces().any(|ws| ws == &first_host)
            && first_host.read(cx).root_paths(cx).is_empty()
    }));
    assert!(metadata(&session, cx).is_projectless());
    assert!(metadata(&other_session, cx).is_projectless());
    assert_eq!(
        other_window.read_with(other_cx, |multi_workspace, _| multi_workspace
            .workspace()
            .clone()),
        other_host
    );
    sidebar.read_with(cx, |sidebar, cx| {
        assert!(sidebar.contents.entries.iter().any(|entry| matches!(entry, ListEntry::Thread(thread)
            if thread.metadata.thread_id == other_metadata.thread_id && thread.status == AgentThreadStatus::Running)));
        assert_eq!(sidebar.contents.project_header_indices.first(), Some(&2));
        assert_eq!(
            sidebar
                .find_open_workspace_for_thread(&other_metadata, cx)
                .expect("live owner")
                .1,
            other_host
        );
    });
    connection.end_turn(session.clone(), acp_v1::StopReason::EndTurn);
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session, window, cx)
    });
    cx.run_until_parked();
    let archived = metadata(&session, cx);
    assert!(archived.archived);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(archived, window, cx)
    });
    cx.run_until_parked();
    let restored = metadata(&session, cx);
    assert!(restored.is_projectless() && !restored.archived);
    assert_eq!(restored.thread_id, first_id);
    assert_eq!(
        panel.read_with(cx, |panel, cx| panel.active_thread_id(cx)),
        Some(first_id)
    );
    assert_eq!(
        first_window.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone()),
        first_host
    );
}

#[gpui::test]
async fn test_selection_clamps_after_entry_removal(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_threads(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Focus sidebar (selection starts at None), navigate down to the thread (index 1)
    focus_sidebar(&sidebar, cx);
    cx.dispatch_action(SelectNext);
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));

    // Collapse the group, which removes the thread from the list
    cx.dispatch_action(SelectParent);
    cx.run_until_parked();

    // Selection should be clamped to the last valid index (0 = header)
    let selection = sidebar.read_with(cx, |s, _| s.selection);
    let entry_count = sidebar.read_with(cx, |s, _| s.contents.entries.len());
    assert!(
        selection.unwrap_or(0) < entry_count,
        "selection {} should be within bounds (entries: {})",
        selection.unwrap_or(0),
        entry_count,
    );
}

pub(super) async fn init_test_project_with_agent_panel(
    worktree_path: &str,
    cx: &mut TestAppContext,
) -> Entity<project::Project> {
    use_unique_metadata_databases(cx);
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        set_max_idle_retained_threads(1, cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(worktree_path, serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    project::Project::test(fs, [worktree_path.as_ref()], cx).await
}

pub(super) fn add_agent_panel(
    workspace: &Entity<Workspace>,
    cx: &mut gpui::VisualTestContext,
) -> Entity<AgentPanel> {
    workspace.update_in(cx, |workspace, window, cx| {
        let panel = cx.new(|cx| AgentPanel::test_new(workspace, window, cx));
        workspace.add_panel(panel.clone(), window, cx);
        panel
    })
}

fn setup_sidebar_with_agent_panel(
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut gpui::VisualTestContext,
) -> (Entity<Sidebar>, Entity<AgentPanel>) {
    let sidebar = setup_sidebar(multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());
    let panel = add_agent_panel(&workspace, cx);
    (sidebar, panel)
}

#[gpui::test]
async fn test_agent_panel_terminals_appear_in_sidebar_and_search(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  Dev Server"]
    );
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Terminal { terminal_id: active_terminal_id, .. }) if *active_terminal_id == terminal_id),
            "expected active terminal entry, got {:?}",
            sidebar.active_entry,
        );
        assert!(
            sidebar.contents.entries.iter().any(|entry| {
                matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id && terminal.metadata.display_title().as_ref() == "Dev Server")
            }),
            "expected the inserted terminal to appear in sidebar contents",
        );
    });
    sidebar.read_with(cx, |_sidebar, cx| {
        let store = TerminalThreadMetadataStore::global(cx).read(cx);
        let metadata = store
            .entry(terminal_id)
            .expect("terminal metadata should be persisted");
        assert_eq!(metadata.title.as_ref(), "");
        assert_eq!(
            metadata.custom_title.as_ref().map(|title| title.as_ref()),
            Some("Dev Server")
        );
        assert_eq!(metadata.display_title().as_ref(), "Dev Server");
        assert!(
            metadata
                .folder_paths()
                .paths()
                .iter()
                .any(|path| path.as_path() == Path::new("/my-project"))
        );
    });

    type_in_search(&sidebar, "server", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  Dev Server  <== selected"]
    );

    type_in_search(&sidebar, "missing", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        Vec::<String>::new()
    );
}

#[gpui::test]
async fn test_closing_last_agent_panel_terminal_restores_empty_header(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    assert_project_header_has_threads(&sidebar, "my-project", false, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    assert_project_header_has_threads(&sidebar, "my-project", true, cx);

    let (terminal_metadata, terminal_workspace) = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id => {
                    Some((terminal.metadata.clone(), terminal.workspace.clone()))
                }
                _ => None,
            })
            .expect("terminal should be visible in sidebar")
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.close_terminal(&terminal_metadata, &terminal_workspace, window, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, cx| {
        assert!(!panel.has_terminal(terminal_id));
        assert!(
            panel.active_thread_id(cx).is_none(),
            "closing the last terminal must not start a fresh draft"
        );
    });
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]"]
    );
    assert_project_header_has_threads(&sidebar, "my-project", false, cx);

    let project_group_key = multi_workspace.read_with(cx, |multi_workspace, cx| {
        multi_workspace.workspace().read(cx).project_group_key(cx)
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.toggle_collapse(&project_group_key, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["> [my-project]"]
    );
    assert_project_header_has_threads(&sidebar, "my-project", false, cx);
}

#[gpui::test]
async fn test_agent_panel_terminal_metadata_remains_visible_after_panel_is_removed(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.remove_panel(&panel, window, cx);
    });
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert!(workspace.read_with(cx, |workspace, cx| {
        workspace.panel::<AgentPanel>(cx).is_none()
    }));
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  Dev Server"]
    );

    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(sidebar.contents.entries.iter().any(|entry| {
            matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id)
        }));
    });
}

#[gpui::test]
async fn test_terminal_metadata_is_deduped_across_project_groups(cx: &mut TestAppContext) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        set_max_idle_retained_threads(1, cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let workspace_a = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(project_b, window, cx);
    });
    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Original", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    workspace_a.update_in(cx, |workspace, window, cx| {
        workspace.remove_panel(&panel, window, cx);
    });
    let now = Utc::now();
    let metadata = TerminalThreadMetadata {
        terminal_id,
        title: "Dev Server".into(),
        custom_title: None,
        created_at: now,
        worktree_paths: WorktreePaths::from_path_lists(
            PathList::new(&[PathBuf::from("/project-a")]),
            PathList::new(&[PathBuf::from("/project-b")]),
        )
        .unwrap(),
        remote_connection: None,
        working_directory: None,
    };

    cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(metadata, cx);
        });
    });
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(
            sidebar
                .contents
                .entries
                .iter()
                .filter(|entry| {
                    matches!(
                        entry,
                        ListEntry::Terminal(terminal)
                            if terminal.metadata.terminal_id == terminal_id
                    )
                })
                .count(),
            1
        );
    });
}

#[gpui::test]
async fn test_agent_panel_terminal_shows_project_and_linked_worktree(cx: &mut TestAppContext) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        set_max_idle_retained_threads(1, cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;

    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let worktree_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(worktree_project.clone(), window, cx)
    });
    let panel = add_agent_panel(&worktree_workspace, cx);

    panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [project]", "  Dev Server {wt-feature-a}"]
    );

    type_in_search(&sidebar, "wt-feature-a", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [project]", "  Dev Server {wt-feature-a}  <== selected"]
    );
}

#[gpui::test]
async fn test_terminal_close_event_keeps_linked_worktree_workspace_with_live_editor_draft(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/project/feature-a/project",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/project/feature-a/project"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(
        fs.clone(),
        ["/worktrees/project/feature-a/project".as_ref()],
        cx,
    )
    .await;

    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);
    let worktree_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(worktree_project.clone(), window, cx)
    });
    let worktree_panel = add_agent_panel(&worktree_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("main-thread")),
        Some("Main Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );

    let worktree_folder_paths =
        PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]);
    let draft_id = save_draft_metadata_with_main_paths(
        Some("Worktree Draft".into()),
        worktree_folder_paths.clone(),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        cx,
    );

    worktree_panel.update_in(cx, |panel, window, cx| {
        panel.load_agent_thread(
            Agent::Stub,
            draft_id,
            Some(worktree_folder_paths.clone()),
            None,
            false,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
    });
    cx.run_until_parked();
    let editor_text =
        worktree_panel.read_with(cx, |panel, cx| panel.editor_text_if_in_memory(draft_id, cx));
    assert_eq!(
        editor_text,
        Some(None),
        "draft should be in memory with empty editor text before editing"
    );

    let message_editor = worktree_panel.read_with(cx, |panel, cx| {
        panel
            .active_thread_view(cx)
            .expect("draft should be loaded in the agent panel")
            .read(cx)
            .message_editor
            .clone()
    });
    message_editor.update_in(cx, |editor, window, cx| {
        editor.set_text("keep this draft", window, cx);
    });

    let terminal_id = worktree_panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();
    let live_blocks = worktree_panel.read_with(cx, |panel, cx| {
        panel.draft_prompt_blocks_if_in_memory(draft_id, cx)
    });
    assert!(
        matches!(
            live_blocks.as_deref(),
            Some([acp_v2::ContentBlock::Text(text)]) if text.text == "keep this draft"
        ),
        "edited draft should still be readable from the panel after opening the terminal"
    );

    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        2,
        "should start with main and linked worktree workspaces"
    );

    worktree_panel.update(cx, |panel, cx| {
        panel.emit_test_terminal_close(terminal_id, cx);
    });
    for _ in 0..4 {
        cx.run_until_parked();
    }

    let terminal_metadata_deleted = cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx)
            .read(cx)
            .entry(terminal_id)
            .is_none()
    });
    assert!(
        terminal_metadata_deleted,
        "terminal metadata should be deleted after close"
    );
    let unarchived_worktree_threads = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries_for_path(&worktree_folder_paths, None)
            .count()
    });
    assert_eq!(
        unarchived_worktree_threads, 1,
        "edited draft should remain as a worktree thread reference"
    );
    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(&worktree_folder_paths, None, cx)
            })
            .is_some(),
        "linked worktree workspace should stay open while an edited draft references it"
    );
    assert!(
        fs.is_dir(Path::new("/worktrees/project/feature-a/project"))
            .await,
        "linked worktree directory should remain on disk while an edited draft references it"
    );
}

#[gpui::test]
async fn test_terminal_close_event_closes_sidebar_terminal(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  Dev Server"]
    );

    panel.update(cx, |panel, cx| {
        panel.emit_test_terminal_close(terminal_id, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert!(!panel.has_terminal(terminal_id));
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(sidebar.contents.entries.iter().all(|entry| {
            !matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id)
        }));
    });
    sidebar.read_with(cx, |_sidebar, cx| {
        assert!(
            TerminalThreadMetadataStore::global(cx)
                .read(cx)
                .entry(terminal_id)
                .is_none(),
            "terminal metadata should be deleted when the terminal requests close"
        );
    });
}

#[gpui::test]
async fn test_terminal_close_event_activates_neighbor(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let build_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Build", true, window, cx)
        })
        .expect("build test terminal should be inserted");
    let server_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Server", true, window, cx)
        })
        .expect("server test terminal should be inserted");
    cx.run_until_parked();

    panel.update(cx, |panel, cx| {
        panel.emit_test_terminal_close(server_terminal_id, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert!(!panel.has_terminal(server_terminal_id));
        assert_eq!(panel.active_terminal_id(), Some(build_terminal_id));
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Terminal { terminal_id, .. }) if *terminal_id == build_terminal_id),
            "expected remaining terminal to become active, got {:?}",
            sidebar.active_entry,
        );
    });
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  Build"]
    );
}

#[gpui::test]
async fn test_agent_panel_terminal_notifications_update_sidebar(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let build_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Build", true, window, cx)
        })
        .expect("build test terminal should be inserted");
    let server_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Server", true, window, cx)
        })
        .expect("server test terminal should be inserted");
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert_eq!(panel.active_terminal_id(), Some(server_terminal_id));
    });

    panel.update(cx, |panel, cx| {
        panel.emit_test_terminal_bell(build_terminal_id, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, cx| {
        assert!(sidebar.has_notifications(cx));
        assert!(sidebar.contents.notified_terminals.contains(&build_terminal_id));
        assert!(sidebar.contents.entries.iter().any(|entry| {
            matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == build_terminal_id && terminal.has_notification)
        }));
    });

    panel.update_in(cx, |panel, window, cx| {
        panel.activate_terminal(build_terminal_id, true, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, cx| {
        assert!(!sidebar.has_notifications(cx));
        assert!(
            !sidebar
                .contents
                .notified_terminals
                .contains(&build_terminal_id)
        );
    });
}

#[gpui::test]
async fn test_thread_switcher_can_activate_agent_panel_terminal(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let build_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Build", true, window, cx)
        })
        .expect("build test terminal should be inserted");
    let server_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Server", true, window, cx)
        })
        .expect("server test terminal should be inserted");
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    let (entry_terminal_ids, selected_terminal_id) = sidebar.read_with(cx, |sidebar, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        let switcher = switcher.read(cx);
        let entry_terminal_ids = switcher
            .entries()
            .iter()
            .map(|entry| {
                entry
                    .terminal_id()
                    .expect("expected terminal switcher entry")
            })
            .collect::<Vec<_>>();
        let selected_terminal_id = switcher
            .selected_entry()
            .expect("switcher should have selected entry")
            .terminal_id()
            .expect("expected selected terminal switcher entry");
        (entry_terminal_ids, selected_terminal_id)
    });

    assert_eq!(entry_terminal_ids.len(), 2);
    assert!(entry_terminal_ids.contains(&build_terminal_id));
    assert!(entry_terminal_ids.contains(&server_terminal_id));

    sidebar.update_in(cx, |sidebar, window, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        let focus = switcher.focus_handle(cx);
        focus.dispatch_action(&menu::Confirm, window, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert_eq!(panel.active_terminal_id(), Some(selected_terminal_id));
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Terminal { terminal_id, .. }) if *terminal_id == selected_terminal_id),
            "expected selected terminal to become active, got {:?}",
            sidebar.active_entry,
        );
    });
}

#[gpui::test]
async fn test_thread_switcher_includes_terminal_metadata_for_open_project_group(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Feature Terminal", true, window, cx)
        })
        .expect("test terminal should be inserted");
    panel.update_in(cx, |panel, window, cx| {
        panel.close_terminal(terminal_id, window, cx);
    });
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-newer")),
        Some("Newer Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-older")),
        Some("Older Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );

    let created_at = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap();
    let metadata = TerminalThreadMetadata {
        terminal_id,
        title: "Feature Terminal".into(),
        custom_title: None,
        created_at,
        worktree_paths: WorktreePaths::from_path_lists(
            PathList::new(&[PathBuf::from("/project")]),
            PathList::new(&[PathBuf::from("/project-feature")]),
        )
        .unwrap(),
        remote_connection: None,
        working_directory: None,
    };
    cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(metadata, cx);
        });
    });
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        assert!(
            switcher
                .read(cx)
                .entries()
                .iter()
                .any(|entry| entry.terminal_id() == Some(terminal_id)),
            "terminal metadata row should be included like a closed thread row"
        );
    });
}

#[gpui::test]
async fn test_thread_switcher_preserves_closed_terminal_linked_worktree_workspace(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/project/feature-a/project",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/project/feature-a/project"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Feature Terminal", true, window, cx)
        })
        .expect("test terminal should be inserted");
    panel.update_in(cx, |panel, window, cx| {
        panel.close_terminal(terminal_id, window, cx);
    });
    let created_at = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap();
    let worktree_folder_paths =
        PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]);
    let metadata = TerminalThreadMetadata {
        terminal_id,
        title: "Feature Terminal".into(),
        custom_title: None,
        created_at,
        worktree_paths: WorktreePaths::from_path_lists(
            PathList::new(&[PathBuf::from("/project")]),
            worktree_folder_paths.clone(),
        )
        .unwrap(),
        remote_connection: None,
        working_directory: None,
    };
    cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(metadata, cx);
        });
    });
    save_thread_metadata(
        acp::SessionId::new(Arc::from("main-thread")),
        Some("Main Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(&worktree_folder_paths, None, cx)
            })
            .is_none(),
        "linked worktree workspace should start closed"
    );

    cx.update(|_, cx| {
        feature_flags::FeatureFlagStore::init(cx);
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.feature_flags = Some(settings::settings_content::FeatureFlagsMap(
                    [(
                        "agent-thread-worktree-label".to_string(),
                        "branch".to_string(),
                    )]
                    .into_iter()
                    .collect(),
                ));
            });
        });
    });

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        match switcher
            .read(cx)
            .selected_entry()
            .expect("switcher should select the terminal row by default")
        {
            ThreadSwitcherEntry::Terminal(entry) => {
                assert_eq!(entry.metadata.terminal_id, terminal_id);
                assert_eq!(entry.worktrees.len(), 1);
                assert_eq!(entry.worktrees[0].worktree_name, None);
                assert_eq!(entry.worktrees[0].branch_name, Some("feature-a".into()));
                match &entry.workspace {
                    ThreadEntryWorkspace::Closed {
                        folder_paths,
                        project_group_key,
                    } => {
                        assert_eq!(folder_paths, &worktree_folder_paths);
                        assert_eq!(
                            project_group_key.path_list(),
                            &PathList::new(&[PathBuf::from("/project")])
                        );
                    }
                    ThreadEntryWorkspace::Open(_) => {
                        panic!("closed terminal row should retain its linked worktree target")
                    }
                }
            }
            ThreadSwitcherEntry::Thread(_) => {
                panic!("terminal row should be selected by default")
            }
        }
    });
}

#[gpui::test]
async fn test_thread_switcher_opens_closed_linked_worktree_thread_in_its_worktree(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                    "feature-b": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-b",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/project/feature-a/project",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/project/feature-b/project",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-b",
            "src": {},
        }),
    )
    .await;
    for name in ["feature-a", "feature-b"] {
        fs.add_linked_worktree_for_repo(
            Path::new("/project/.git"),
            false,
            git::repository::Worktree {
                path: PathBuf::from(format!("/worktrees/project/{name}/project")),
                ref_name: Some(format!("refs/heads/{name}").into()),
                sha: "aaa".into(),
                is_main: false,
                is_bare: false,
                is_prunable: false,
            },
        )
        .await;
    }
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let main_paths = PathList::new(&[PathBuf::from("/project")]);
    let worktree_folder_paths =
        PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]);
    save_thread_metadata_with_main_paths(
        "feature-thread",
        "Feature Thread",
        worktree_folder_paths.clone(),
        main_paths.clone(),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        cx,
    );
    save_thread_metadata_with_main_paths(
        "main-thread",
        "Main Thread",
        main_paths.clone(),
        main_paths,
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );

    let sibling_project = project::Project::test(
        fs.clone(),
        ["/worktrees/project/feature-b/project".as_ref()],
        cx,
    )
    .await;
    sibling_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
        MultiWorkspace::test_new(sibling_project.clone(), window, cx)
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();
    let feature_selection = sidebar.read_with(cx, |sidebar, cx| {
        sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open")
            .read(cx)
            .entries()
            .iter()
            .find_map(|entry| match entry {
                ThreadSwitcherEntry::Thread(thread)
                    if thread.metadata.title.as_ref().map(|title| title.as_ref())
                        == Some("Feature Thread") =>
                {
                    Some(entry.selection())
                }
                _ => None,
            })
            .expect("the closed worktree thread should be listed")
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.confirm_switcher_selection(&feature_selection, window, cx);
    });
    cx.run_until_parked();

    let active_root_paths = multi_workspace.read_with(cx, |multi_workspace, cx| {
        PathList::new(&multi_workspace.workspace().read(cx).root_paths(cx))
    });
    assert_eq!(active_root_paths, worktree_folder_paths);
}

#[gpui::test]
async fn test_closing_terminal_in_closed_linked_worktree_keeps_draft_and_checkout(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/project/feature-a/project",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/project/feature-a/project"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;
    agent_ui::test_support::record_zed_created_worktree(
        fs.as_ref(),
        Path::new("/worktrees/project/feature-a/project"),
        None,
        cx,
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Feature Terminal", true, window, cx)
        })
        .expect("test terminal should be inserted");
    panel.update_in(cx, |panel, window, cx| {
        panel.close_terminal(terminal_id, window, cx);
    });
    let worktree_folder_paths =
        PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]);
    let metadata = TerminalThreadMetadata {
        terminal_id,
        title: "Feature Terminal".into(),
        custom_title: None,
        created_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        worktree_paths: WorktreePaths::from_path_lists(
            PathList::new(&[PathBuf::from("/project")]),
            worktree_folder_paths.clone(),
        )
        .unwrap(),
        remote_connection: None,
        working_directory: None,
    };
    cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(metadata, cx);
        });
    });
    let empty_draft_id = save_draft_metadata_with_main_paths(
        None,
        worktree_folder_paths.clone(),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        cx,
    );
    cx.update(|_, cx| {
        assert!(
            agent_ui::draft_prompt_store::read(empty_draft_id, cx).is_none(),
            "empty draft should not have persisted prompt content"
        );
    });
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    let terminal_index = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id))
            .expect("terminal should be visible in sidebar")
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        match &sidebar.contents.entries[terminal_index] {
            ListEntry::Terminal(terminal) => match &terminal.workspace {
                ThreadEntryWorkspace::Closed { folder_paths, .. } => {
                    assert_eq!(folder_paths, &worktree_folder_paths);
                }
                ThreadEntryWorkspace::Open(_) => {
                    panic!("linked worktree terminal should start closed")
                }
            },
            _ => panic!("expected terminal row"),
        }
    });

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(terminal_index);
    });
    cx.dispatch_action(ArchiveSelectedThread);
    for _ in 0..8 {
        cx.run_until_parked();
    }

    let terminal_metadata_deleted = cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx)
            .read(cx)
            .entry(terminal_id)
            .is_none()
    });
    assert!(
        terminal_metadata_deleted,
        "terminal metadata should be deleted after closing from the sidebar"
    );
    let empty_draft_is_listed = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(empty_draft_id)
            .is_some_and(|draft| {
                draft.is_draft()
                    && !draft.archived
                    && draft.folder_paths() == &worktree_folder_paths
            })
    });
    assert!(
        empty_draft_is_listed,
        "closing the terminal should preserve the neighboring draft"
    );
    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(&worktree_folder_paths, None, cx)
            })
            .is_none(),
        "closing the terminal should leave the linked workspace closed"
    );
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        1,
        "closing a closed linked worktree terminal should leave only the main workspace"
    );
    assert!(
        fs.is_dir(Path::new("/worktrees/project/feature-a/project"))
            .await,
        "closing the terminal should preserve the linked checkout"
    );
}

const DELETED_WORKTREE_PATH: &str = "/worktrees/project/feature-a/project";

pub(super) async fn init_delete_worktree_test(
    zed_created: bool,
    cx: &mut TestAppContext,
) -> (Arc<FakeFs>, Entity<project::Project>) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        DELETED_WORKTREE_PATH,
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from(DELETED_WORKTREE_PATH),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;
    if zed_created {
        agent_ui::test_support::record_zed_created_worktree(
            fs.as_ref(),
            Path::new(DELETED_WORKTREE_PATH),
            None,
            cx,
        )
        .await;
    }
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    (fs, main_project)
}

fn delete_linked_worktree_group(sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext) {
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
                } if path.as_ref() == DELETED_WORKTREE_PATH => Some(id.clone()),
                _ => None,
            })
            .expect("linked worktree should have a header");
        sidebar.delete_worktree_group(&id, window, cx);
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }
}

#[gpui::test]
async fn test_delete_worktree_group_archives_its_threads(cx: &mut TestAppContext) {
    let (fs, main_project) = init_delete_worktree_test(true, cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_session_id = acp::SessionId::new(Arc::from("worktree-thread"));
    let main_session_id = acp::SessionId::new(Arc::from("main-thread"));
    save_thread_metadata_with_main_paths(
        "worktree-thread",
        "Worktree Thread",
        PathList::new(&[PathBuf::from(DELETED_WORKTREE_PATH)]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    save_thread_metadata(
        main_session_id.clone(),
        Some("Main Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );

    delete_linked_worktree_group(&sidebar, cx);

    let archived = |session_id: &acp::SessionId, cx: &mut gpui::VisualTestContext| {
        cx.update(|_, cx| {
            ThreadMetadataStore::global(cx)
                .read(cx)
                .entry_by_session(session_id)
                .map(|thread| thread.archived)
        })
    };
    assert_eq!(archived(&worktree_session_id, cx), Some(true));
    assert_eq!(archived(&main_session_id, cx), Some(false));
    assert!(!fs.is_dir(Path::new(DELETED_WORKTREE_PATH)).await);
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        1,
        "the workspace opened to save the worktree's state should be closed again"
    );
}

#[gpui::test]
async fn test_delete_worktree_group_without_threads_removes_it_with_git(cx: &mut TestAppContext) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    delete_linked_worktree_group(&sidebar, cx);

    assert!(!fs.is_dir(Path::new(DELETED_WORKTREE_PATH)).await);
    assert_eq!(
        visible_entries_with_worktree_headers(&sidebar, cx),
        vec!["v [project]", "  ~ project"]
    );
}

#[gpui::test]
async fn test_deleted_worktree_group_is_forgotten_by_other_open_worktrees(cx: &mut TestAppContext) {
    const OTHER_WORKTREE_PATH: &str = "/worktrees/project/feature-b/project";

    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    fs.insert_tree(
        "/project/.git/worktrees/feature-b",
        serde_json::json!({
            "commondir": "../../",
            "HEAD": "ref: refs/heads/feature-b",
        }),
    )
    .await;
    fs.insert_tree(
        OTHER_WORKTREE_PATH,
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-b",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        true,
        git::repository::Worktree {
            path: PathBuf::from(OTHER_WORKTREE_PATH),
            ref_name: Some("refs/heads/feature-b".into()),
            sha: "bbb".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;
    let other_project =
        project::Project::test(fs.clone(), [OTHER_WORKTREE_PATH.as_ref()], cx).await;
    other_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(other_project.clone(), window, cx)
    });
    cx.run_until_parked();

    let other_worktree_lists_deleted = |cx: &mut gpui::VisualTestContext| {
        other_project.read_with(cx, |project, cx| {
            project.repositories(cx).values().any(|repository| {
                repository
                    .read(cx)
                    .snapshot()
                    .linked_worktrees()
                    .iter()
                    .any(|worktree| worktree.path == Path::new(DELETED_WORKTREE_PATH))
            })
        })
    };
    let deleted_worktree_is_shown = |cx: &mut gpui::VisualTestContext| {
        sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
        visible_entries_with_worktree_headers(&sidebar, cx)
            .iter()
            .any(|entry| entry.contains("feature-a"))
    };
    assert!(other_worktree_lists_deleted(cx));
    assert!(deleted_worktree_is_shown(cx));

    // No file watcher tells the other worktree about the removal.
    fs.pause_events();
    delete_linked_worktree_group(&sidebar, cx);

    assert!(!fs.is_dir(Path::new(DELETED_WORKTREE_PATH)).await);
    assert!(
        !other_worktree_lists_deleted(cx),
        "the other open worktree should stop listing the removed one"
    );
    assert!(!deleted_worktree_is_shown(cx));

    fs.unpause_events_and_flush();
    cx.run_until_parked();
    assert!(!deleted_worktree_is_shown(cx));
    sidebar.read_with(cx, |sidebar, _| {
        assert!(sidebar.deleting_worktree_paths.borrow().is_empty());
    });
}

#[gpui::test]
async fn test_delete_worktree_group_asks_before_discarding_changes(cx: &mut TestAppContext) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    fs.with_git_state(Path::new("/project/.git"), false, |state| {
        state
            .worktrees_requiring_force_delete
            .insert(PathBuf::from(DELETED_WORKTREE_PATH));
    })
    .unwrap();
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    delete_linked_worktree_group(&sidebar, cx);
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert!(fs.is_dir(Path::new(DELETED_WORKTREE_PATH)).await);

    delete_linked_worktree_group(&sidebar, cx);
    cx.simulate_prompt_answer("Delete Anyway");
    for _ in 0..8 {
        cx.run_until_parked();
    }
    assert!(!fs.is_dir(Path::new(DELETED_WORKTREE_PATH)).await);
}

const SECOND_DELETED_WORKTREE_PATH: &str = "/worktrees/zz-other/feature-b/other";

/// A closed group of two linked worktrees of different repositories, with one thread.
/// Git refuses to remove the second worktree without force.
async fn init_delete_two_root_worktree_group_test(
    cx: &mut TestAppContext,
) -> (
    Arc<FakeFs>,
    Entity<MultiWorkspace>,
    Entity<Sidebar>,
    acp::SessionId,
    &mut gpui::VisualTestContext,
) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    fs.insert_tree("/other", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/other/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from(SECOND_DELETED_WORKTREE_PATH),
            ref_name: Some("refs/heads/feature-b".into()),
            sha: "bbb".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;
    fs.with_git_state(Path::new("/other/.git"), false, |state| {
        state
            .worktrees_requiring_force_delete
            .insert(PathBuf::from(SECOND_DELETED_WORKTREE_PATH));
    })
    .unwrap();
    let multi_root_project = project::Project::test(
        fs.clone(),
        [
            DELETED_WORKTREE_PATH.as_ref(),
            SECOND_DELETED_WORKTREE_PATH.as_ref(),
        ],
        cx,
    )
    .await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let main_workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let multi_root_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(multi_root_project.clone(), window, cx)
    });
    cx.run_until_parked();
    let session_id = acp::SessionId::new(Arc::from("two-root-thread"));
    save_test_thread_metadata(&session_id, &multi_root_project, cx).await;
    cx.run_until_parked();
    multi_workspace
        .update_in(cx, |multi_workspace, window, cx| {
            // Removing the active workspace would reopen its project.
            multi_workspace.activate(main_workspace, None, window, cx);
            multi_workspace.remove(
                vec![multi_root_workspace],
                RemovalIntent::KeepProject,
                window,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();
    (fs, multi_workspace, sidebar, session_id, cx)
}

#[gpui::test]
async fn test_cancelling_a_dirty_two_root_worktree_deletion_removes_nothing(
    cx: &mut TestAppContext,
) {
    let (fs, _multi_workspace, sidebar, session_id, cx) =
        init_delete_two_root_worktree_group_test(cx).await;
    fs.set_status_for_repo(
        Path::new("/other/.git/worktrees/feature-b"),
        &[("src/wip.rs", git::status::FileStatus::Untracked)],
    );

    delete_linked_worktree_group(&sidebar, cx);
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    for _ in 0..8 {
        cx.run_until_parked();
    }

    assert!(!cx.has_pending_prompt());
    assert!(fs.is_dir(Path::new(DELETED_WORKTREE_PATH)).await);
    assert!(fs.is_dir(Path::new(SECOND_DELETED_WORKTREE_PATH)).await);
    assert_eq!(
        cx.update(|_, cx| {
            ThreadMetadataStore::global(cx)
                .read(cx)
                .entry_by_session(&session_id)
                .map(|thread| thread.archived)
        }),
        Some(false)
    );

    delete_linked_worktree_group(&sidebar, cx);
    cx.simulate_prompt_answer("Delete Anyway");
    for _ in 0..8 {
        cx.run_until_parked();
    }
    assert!(!cx.has_pending_prompt());
    assert!(!fs.is_dir(Path::new(DELETED_WORKTREE_PATH)).await);
    assert!(!fs.is_dir(Path::new(SECOND_DELETED_WORKTREE_PATH)).await);
}

#[gpui::test]
async fn test_two_root_worktree_deletion_failing_part_way_keeps_its_thread_archived(
    cx: &mut TestAppContext,
) {
    // The second worktree has no changes to warn about, yet git refuses to remove it.
    let (fs, _multi_workspace, sidebar, session_id, cx) =
        init_delete_two_root_worktree_group_test(cx).await;

    delete_linked_worktree_group(&sidebar, cx);

    assert!(
        !cx.has_pending_prompt(),
        "asking after the first worktree is gone could not be cancelled cleanly"
    );
    assert!(!fs.is_dir(Path::new(DELETED_WORKTREE_PATH)).await);
    assert!(fs.is_dir(Path::new(SECOND_DELETED_WORKTREE_PATH)).await);
    assert_eq!(
        cx.update(|_, cx| {
            ThreadMetadataStore::global(cx)
                .read(cx)
                .entry_by_session(&session_id)
                .map(|thread| thread.archived)
        }),
        Some(true),
        "a thread of a removed worktree must not come back without it"
    );
}

#[gpui::test]
async fn test_cancelling_a_dirty_worktree_deletion_keeps_its_workspace_open(
    cx: &mut TestAppContext,
) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    fs.set_status_for_repo(
        Path::new("/project/.git/worktrees/feature-a"),
        &[("src/wip.rs", git::status::FileStatus::Untracked)],
    );
    let linked_project =
        project::Project::test(fs.clone(), [DELETED_WORKTREE_PATH.as_ref()], cx).await;
    linked_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let linked_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(linked_project.clone(), window, cx)
    });
    cx.run_until_parked();

    delete_linked_worktree_group(&sidebar, cx);
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();

    assert!(fs.is_dir(Path::new(DELETED_WORKTREE_PATH)).await);
    assert!(multi_workspace.read_with(cx, |multi_workspace, _| {
        multi_workspace
            .workspaces()
            .any(|workspace| *workspace == linked_workspace)
    }));
}

#[gpui::test]
async fn test_delete_worktree_group_is_cancelled_by_restoring_its_thread(cx: &mut TestAppContext) {
    let (fs, main_project) = init_delete_worktree_test(true, cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let session_id = acp::SessionId::new(Arc::from("worktree-thread"));
    save_thread_metadata_with_main_paths(
        "worktree-thread",
        "Worktree Thread",
        PathList::new(&[PathBuf::from(DELETED_WORKTREE_PATH)]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    // Restores the thread as soon as the deletion archives it.
    cx.update(|_, cx| {
        let store = ThreadMetadataStore::global(cx);
        cx.subscribe(
            &store,
            |store, event: &agent_ui::thread_metadata_store::ThreadMetadataStoreEvent, cx| {
                let agent_ui::thread_metadata_store::ThreadMetadataStoreEvent::ThreadArchived(
                    thread_id,
                ) = event;
                store.update(cx, |store, cx| store.unarchive(*thread_id, cx));
            },
        )
        .detach();
    });

    delete_linked_worktree_group(&sidebar, cx);

    assert!(fs.is_dir(Path::new(DELETED_WORKTREE_PATH)).await);
    let archived = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&session_id)
            .map(|thread| thread.archived)
    });
    assert_eq!(archived, Some(false));
}

#[gpui::test]
async fn test_delete_worktree_group_without_saved_state_is_cancelled_by_restoring_its_thread(
    cx: &mut TestAppContext,
) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata_with_main_paths(
        "worktree-thread",
        "Worktree Thread",
        PathList::new(&[PathBuf::from(DELETED_WORKTREE_PATH)]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    // Restores the thread as soon as the deletion archives it.
    cx.update(|_, cx| {
        let store = ThreadMetadataStore::global(cx);
        cx.subscribe(
            &store,
            |store, event: &agent_ui::thread_metadata_store::ThreadMetadataStoreEvent, cx| {
                let agent_ui::thread_metadata_store::ThreadMetadataStoreEvent::ThreadArchived(
                    thread_id,
                ) = event;
                store.update(cx, |store, cx| store.unarchive(*thread_id, cx));
            },
        )
        .detach();
    });

    delete_linked_worktree_group(&sidebar, cx);

    assert!(fs.is_dir(Path::new(DELETED_WORKTREE_PATH)).await);
}

#[gpui::test]
async fn test_delete_worktree_group_is_blocked_by_a_running_agent(cx: &mut TestAppContext) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    let linked_project =
        project::Project::test(fs.clone(), [DELETED_WORKTREE_PATH.as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let linked_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(linked_project.clone(), window, cx)
    });
    let panel = add_agent_panel(&linked_workspace, cx);
    open_thread_with_connection(&panel, StubAgentConnection::new(), cx);
    send_message(&panel, cx);
    cx.run_until_parked();

    delete_linked_worktree_group(&sidebar, cx);

    assert!(
        fs.is_dir(Path::new(DELETED_WORKTREE_PATH)).await,
        "a worktree must not be deleted while an agent is running in it"
    );
}

#[gpui::test]
async fn test_delete_worktree_group_asks_before_discarding_unsent_drafts(cx: &mut TestAppContext) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let draft_id = save_draft_metadata_with_main_paths(
        None,
        PathList::new(&[PathBuf::from(DELETED_WORKTREE_PATH)]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    cx.update(|_, cx| {
        agent_ui::draft_prompt_store::write(
            draft_id,
            &[acp_v2::ContentBlock::Text(acp_v2::TextContent::new(
                "unsent",
            ))],
            cx,
        )
    })
    .await
    .expect("draft prompt should persist");
    let draft_exists = |cx: &mut gpui::VisualTestContext| {
        cx.update(|_, cx| {
            ThreadMetadataStore::global(cx)
                .read(cx)
                .entry(draft_id)
                .is_some()
        })
    };

    delete_linked_worktree_group(&sidebar, cx);
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert!(fs.is_dir(Path::new(DELETED_WORKTREE_PATH)).await);
    assert!(draft_exists(cx));

    delete_linked_worktree_group(&sidebar, cx);
    cx.simulate_prompt_answer("Delete");
    for _ in 0..8 {
        cx.run_until_parked();
    }
    assert!(!fs.is_dir(Path::new(DELETED_WORKTREE_PATH)).await);
    assert!(!draft_exists(cx));
}

#[gpui::test]
async fn test_linked_worktree_header_is_hidden_when_a_multi_root_project_uses_it(
    cx: &mut TestAppContext,
) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    fs.insert_tree("/other", serde_json::json!({ "src": {} }))
        .await;
    let multi_root_project = project::Project::test(
        fs.clone(),
        ["/other".as_ref(), DELETED_WORKTREE_PATH.as_ref()],
        cx,
    )
    .await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let multi_root_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(multi_root_project.clone(), window, cx)
    });
    let panel = add_agent_panel(&multi_root_workspace, cx);
    open_thread_with_connection(&panel, StubAgentConnection::new(), cx);
    send_message(&panel, cx);
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.update_entries(cx);
        assert!(
            !sidebar.contents.entries.iter().any(|entry| matches!(
                entry,
                ListEntry::WorktreeHeader { path, is_linked: true, .. }
                    if path.as_ref() == DELETED_WORKTREE_PATH
            )),
            "a worktree used by a multi-root project must not get its own header"
        );
    });
}

#[gpui::test]
async fn test_empty_multi_root_worktree_group_stays_until_its_worktree_is_deleted(
    cx: &mut TestAppContext,
) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    fs.insert_tree("/other", serde_json::json!({ "src": {} }))
        .await;
    let multi_root_project = project::Project::test(
        fs.clone(),
        ["/other".as_ref(), DELETED_WORKTREE_PATH.as_ref()],
        cx,
    )
    .await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let main_workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let multi_root_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(multi_root_project.clone(), window, cx)
    });
    let panel = add_agent_panel(&multi_root_workspace, cx);
    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);
    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &multi_root_project, cx).await;
    cx.run_until_parked();

    // Whether each header that has the linked worktree as a root has rows, and
    // whether it belongs to a multi-root group.
    let linked_worktree_headers = |sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _| {
            sidebar
                .contents
                .entries
                .iter()
                .filter_map(|entry| match entry {
                    ListEntry::WorktreeHeader {
                        roots, has_rows, ..
                    } if roots
                        .iter()
                        .any(|root| root.as_ref() == DELETED_WORKTREE_PATH) =>
                    {
                        Some((*has_rows, roots.len() > 1))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(linked_worktree_headers(&sidebar, cx), vec![(true, true)]);

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        linked_worktree_headers(&sidebar, cx),
        vec![(false, true)],
        "the group must outlive its last thread"
    );

    multi_workspace
        .update_in(cx, |multi_workspace, window, cx| {
            // Removing the active workspace would reopen its project.
            multi_workspace.activate(main_workspace, None, window, cx);
            multi_workspace.remove(
                vec![multi_root_workspace],
                RemovalIntent::KeepProject,
                window,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        linked_worktree_headers(&sidebar, cx),
        vec![(false, true)],
        "the group must outlive its workspace, and stay out of the single-root project"
    );

    let serialized = sidebar
        .read_with(cx, |sidebar, cx| sidebar.serialized_state(cx))
        .expect("serialized_state should return Some");
    let restored_sidebar =
        cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    restored_sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.restore_serialized_state(&serialized, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        linked_worktree_headers(&restored_sidebar, cx),
        vec![(false, true)],
        "the group must survive a restart"
    );

    fs.remove_dir(
        Path::new(DELETED_WORKTREE_PATH),
        fs::RemoveOptions {
            recursive: true,
            ignore_if_not_exists: true,
        },
    )
    .await
    .unwrap();
    restored_sidebar.update(cx, |sidebar, cx| {
        sidebar.fetch_closed_group_worktrees(true, cx)
    });
    cx.run_until_parked();
    assert!(
        linked_worktree_headers(&restored_sidebar, cx)
            .iter()
            .all(|(_, is_multi_root)| !is_multi_root),
        "a group whose worktree is gone from disk must be forgotten"
    );
}

#[gpui::test]
async fn test_changing_folders_forgets_the_old_multi_root_worktree_group(cx: &mut TestAppContext) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    fs.insert_tree("/other", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/third", serde_json::json!({ "src": {} }))
        .await;
    let multi_root_project = project::Project::test(
        fs.clone(),
        ["/other".as_ref(), DELETED_WORKTREE_PATH.as_ref()],
        cx,
    )
    .await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(multi_root_project.clone(), window, cx)
    });
    cx.run_until_parked();

    let saved_group_sizes = |cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _| {
            sidebar
                .multi_root_worktree_groups
                .iter()
                .map(|(_, paths)| paths.folder_path_list().paths().len())
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(saved_group_sizes(cx), vec![2]);

    multi_root_project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/third", true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    assert_eq!(saved_group_sizes(cx), vec![3]);
}

#[gpui::test]
async fn test_multi_root_project_has_one_worktree_header(cx: &mut TestAppContext) {
    let (_fs, project) = init_multi_project_test(&["/project-z", "/project-a"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project-a", true, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    save_n_test_threads(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_with_worktree_headers(&sidebar, cx),
        vec!["v [project-z / project-a]", "  ~ project-z", "  Thread 1"]
    );
}

#[gpui::test]
async fn test_archiving_the_only_thread_does_not_start_a_draft(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);
    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]"]
    );
    panel.read_with(cx, |panel, cx| {
        assert!(panel.active_thread_id(cx).is_none());
    });
}

#[gpui::test]
async fn test_removing_the_last_thread_keeps_its_worktree(cx: &mut TestAppContext) {
    let (fs, main_project) = init_delete_worktree_test(true, cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let session_id = acp::SessionId::new(Arc::from("worktree-thread"));
    save_thread_metadata_with_main_paths(
        "worktree-thread",
        "Worktree Thread",
        PathList::new(&[PathBuf::from(DELETED_WORKTREE_PATH)]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    let draft_id = save_draft_metadata_with_main_paths(
        None,
        PathList::new(&[PathBuf::from(DELETED_WORKTREE_PATH)]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        cx,
    );
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
        let workspace = sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Thread(thread) if thread.metadata.thread_id == draft_id => {
                    Some(thread.workspace.clone())
                }
                _ => None,
            })
            .expect("draft should be listed");
        sidebar.remove_draft(draft_id, &workspace, window, cx);
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }

    let (archived, draft_exists) = cx.update(|_, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        (
            store
                .entry_by_session(&session_id)
                .map(|thread| thread.archived),
            store.entry(draft_id).is_some(),
        )
    });
    assert_eq!(archived, Some(true));
    assert!(!draft_exists);
    assert!(
        fs.is_dir(Path::new(DELETED_WORKTREE_PATH)).await,
        "a worktree is only removed when it is deleted explicitly"
    );
    assert_eq!(
        visible_entries_with_worktree_headers(&sidebar, cx),
        vec!["v [project]", "  ~ project", "  ~ feature-a"]
    );
}

#[gpui::test]
async fn test_worktree_group_stays_in_place_when_its_last_thread_is_removed(
    cx: &mut TestAppContext,
) {
    let (_fs, main_project) = init_delete_worktree_test(true, cx).await;
    let session_id = acp::SessionId::new(Arc::from("worktree-thread"));
    save_thread_metadata_with_main_paths(
        "worktree-thread",
        "Worktree Thread",
        PathList::new(&[PathBuf::from(DELETED_WORKTREE_PATH)]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    cx.run_until_parked();
    assert_eq!(
        visible_entries_with_worktree_headers(&sidebar, cx),
        vec![
            "v [project]",
            "  ~ feature-a",
            "  Worktree Thread {feature-a}",
            "  ~ project"
        ]
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_with_worktree_headers(&sidebar, cx),
        vec!["v [project]", "  ~ feature-a", "  ~ project"]
    );
}

#[gpui::test]
async fn test_linked_worktree_stays_listed_when_only_its_workspace_is_open(
    cx: &mut TestAppContext,
) {
    let (fs, _main_project) = init_delete_worktree_test(false, cx).await;
    let linked_project =
        project::Project::test(fs.clone(), [DELETED_WORKTREE_PATH.as_ref()], cx).await;
    linked_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    let session_id = acp::SessionId::new(Arc::from("worktree-thread"));
    save_thread_metadata_with_main_paths(
        "worktree-thread",
        "Worktree Thread",
        PathList::new(&[PathBuf::from(DELETED_WORKTREE_PATH)]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    let draft_id = save_draft_metadata_with_main_paths(
        Some("Worktree Draft".into()),
        PathList::new(&[PathBuf::from(DELETED_WORKTREE_PATH)]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        cx,
    );
    let (multi_workspace, cx) = cx
        .add_window_view(|window, cx| MultiWorkspace::test_new(linked_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
        let draft_workspace = sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Thread(thread) if thread.metadata.thread_id == draft_id => {
                    Some(thread.workspace.clone())
                }
                _ => None,
            })
            .expect("the draft should have a row");
        sidebar.remove_draft(draft_id, &draft_workspace, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        let headers: Vec<(&str, bool, bool)> = sidebar
            .contents
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::WorktreeHeader {
                    path,
                    is_linked,
                    has_rows,
                    ..
                } => Some((path.as_ref(), *is_linked, *has_rows)),
                _ => None,
            })
            .collect();
        assert_eq!(headers.len(), 2, "headers: {headers:?}");
        assert!(headers.contains(&(DELETED_WORKTREE_PATH, true, false)));
        assert!(headers.contains(&("/project", false, false)));
    });
}

#[gpui::test]
async fn test_worktree_removed_outside_zed_is_cleaned_up(cx: &mut TestAppContext) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    let linked_project =
        project::Project::test(fs.clone(), [DELETED_WORKTREE_PATH.as_ref()], cx).await;
    let session_id = acp::SessionId::new(Arc::from("worktree-thread"));
    save_thread_metadata_with_main_paths(
        "worktree-thread",
        "Worktree Thread",
        PathList::new(&[PathBuf::from(DELETED_WORKTREE_PATH)]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    let draft_id = save_draft_metadata_with_main_paths(
        Some("Worktree Draft".into()),
        PathList::new(&[PathBuf::from(DELETED_WORKTREE_PATH)]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        cx,
    );
    // Its repository cannot be listed, so nothing is known about its worktree.
    let unknown_session_id = acp::SessionId::new(Arc::from("unknown-thread"));
    save_thread_metadata_with_main_paths(
        "unknown-thread",
        "Unknown Thread",
        PathList::new(&[PathBuf::from("/unknown-worktree")]),
        PathList::new(&[PathBuf::from("/not-a-repository")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        cx,
    );
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(linked_project.clone(), window, cx)
    });
    cx.run_until_parked();
    let is_archived = |session_id: &acp::SessionId, cx: &mut gpui::VisualTestContext| {
        cx.update(|_, cx| {
            ThreadMetadataStore::global(cx)
                .read(cx)
                .entry_by_session(session_id)
                .map(|thread| thread.archived)
        })
    };

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.clean_up_unlisted_worktrees(window, cx);
    });
    cx.run_until_parked();
    assert_eq!(is_archived(&session_id, cx), Some(false));
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        2
    );

    fs.remove_worktree_for_repo(Path::new("/project/.git"), true, "refs/heads/feature-a")
        .await;
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.clean_up_unlisted_worktrees(window, cx);
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }

    assert_eq!(is_archived(&session_id, cx), Some(true));
    assert_eq!(is_archived(&unknown_session_id, cx), Some(false));

    // A thread the user restores is not archived again.
    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&session_id)
            .cloned()
            .unwrap()
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.clean_up_unlisted_worktrees(window, cx);
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }
    assert_eq!(is_archived(&session_id, cx), Some(false));
    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            let thread_id = store.entry_by_session(&session_id).unwrap().thread_id;
            store.archive(thread_id, None, cx);
        })
    });
    cx.run_until_parked();
    assert!(cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(draft_id)
            .is_none()
    }));
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        1,
        "the workspace of the removed worktree should be closed"
    );
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            !sidebar.contents.entries.iter().any(|entry| matches!(
                entry,
                ListEntry::WorktreeHeader { path, .. } if path.as_ref() == DELETED_WORKTREE_PATH
            )),
            "a worktree git no longer lists must not keep its header"
        );
    });
}

#[gpui::test]
async fn test_worktree_with_missing_folder_still_registered_in_git_is_only_archived(
    cx: &mut TestAppContext,
) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    let linked_project =
        project::Project::test(fs.clone(), [DELETED_WORKTREE_PATH.as_ref()], cx).await;
    let session_id = acp::SessionId::new(Arc::from("worktree-thread"));
    save_thread_metadata_with_main_paths(
        "worktree-thread",
        "Worktree Thread",
        PathList::new(&[PathBuf::from(DELETED_WORKTREE_PATH)]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    let draft_id = save_draft_metadata_with_main_paths(
        Some("Worktree Draft".into()),
        PathList::new(&[PathBuf::from(DELETED_WORKTREE_PATH)]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        cx,
    );
    save_terminal_metadata_with_main_paths(
        "Worktree Terminal",
        PathList::new(&[PathBuf::from(DELETED_WORKTREE_PATH)]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        cx,
    );
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(linked_project.clone(), window, cx)
    });
    cx.run_until_parked();

    // The folder is gone, as with an unmounted volume, but git still registers it.
    fs.remove_dir(
        Path::new(DELETED_WORKTREE_PATH),
        fs::RemoveOptions {
            recursive: true,
            ignore_if_not_exists: false,
        },
    )
    .await
    .unwrap();
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.clean_up_unlisted_worktrees(window, cx);
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }

    cx.update(|_, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        assert_eq!(
            store
                .entry_by_session(&session_id)
                .map(|thread| thread.archived),
            Some(true)
        );
        assert!(
            store.entry(draft_id).is_some(),
            "the draft of a worktree that may come back must be kept"
        );
        assert_eq!(
            TerminalThreadMetadataStore::global(cx)
                .read(cx)
                .entries()
                .count(),
            1,
            "the terminal entry of a worktree that may come back must be kept"
        );
    });
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        1,
        "the workspace of the missing worktree should be closed"
    );
    assert_eq!(
        visible_entries_with_worktree_headers(&sidebar, cx),
        vec!["v [project]", "  ~ project"],
        "kept entries stay hidden while their worktree's folder is missing"
    );

    fs.create_dir(Path::new(DELETED_WORKTREE_PATH))
        .await
        .unwrap();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.clean_up_unlisted_worktrees(window, cx);
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }
    assert_eq!(
        visible_entries_with_worktree_headers(&sidebar, cx),
        vec![
            "v [project]",
            "  ~ feature-a",
            "  Worktree Terminal {feature-a}",
            "  New Zed Agent Thread {feature-a}",
            "  ~ project",
        ],
        "kept entries are shown again once the folder is back"
    );
}

#[gpui::test]
async fn test_worktree_removed_outside_zed_leaves_multi_root_entries_alone(
    cx: &mut TestAppContext,
) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    fs.insert_tree("/other", serde_json::json!({ "src": {} }))
        .await;
    let multi_root_project = project::Project::test(
        fs.clone(),
        ["/other".as_ref(), DELETED_WORKTREE_PATH.as_ref()],
        cx,
    )
    .await;
    let session_id = acp::SessionId::new(Arc::from("multi-root-thread"));
    save_thread_metadata_with_main_paths(
        "multi-root-thread",
        "Multi Root Thread",
        PathList::new(&[
            PathBuf::from("/other"),
            PathBuf::from(DELETED_WORKTREE_PATH),
        ]),
        PathList::new(&[PathBuf::from("/other"), PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(multi_root_project.clone(), window, cx)
    });
    cx.run_until_parked();

    fs.remove_worktree_for_repo(Path::new("/project/.git"), true, "refs/heads/feature-a")
        .await;
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.clean_up_unlisted_worktrees(window, cx);
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }

    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        2,
        "a workspace with a root that still exists must stay open"
    );
    assert_eq!(
        cx.update(|_, cx| {
            ThreadMetadataStore::global(cx)
                .read(cx)
                .entry_by_session(&session_id)
                .map(|thread| thread.archived)
        }),
        Some(false)
    );
}

#[gpui::test]
async fn test_worktree_removed_outside_zed_is_kept_while_an_agent_runs_in_it(
    cx: &mut TestAppContext,
) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    let linked_project =
        project::Project::test(fs.clone(), [DELETED_WORKTREE_PATH.as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let linked_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(linked_project.clone(), window, cx)
    });
    let panel = add_agent_panel(&linked_workspace, cx);
    open_thread_with_connection(&panel, StubAgentConnection::new(), cx);
    send_message(&panel, cx);
    cx.run_until_parked();

    fs.remove_worktree_for_repo(Path::new("/project/.git"), true, "refs/heads/feature-a")
        .await;
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.clean_up_unlisted_worktrees(window, cx);
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }

    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        2,
        "a workspace must not be closed while an agent is running in it"
    );
}

#[gpui::test]
async fn test_top_entry_after_removal_prefers_the_nearest_section(cx: &mut TestAppContext) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    fs.insert_tree("/other", serde_json::json!({ "src": {} }))
        .await;
    let other_project = project::Project::test(fs.clone(), ["/other".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(other_project.clone(), window, cx)
    });

    let time = |day| chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, day, 0, 0, 0).unwrap();
    for (name, day) in [("worktree-old", 1), ("worktree-new", 2)] {
        save_thread_metadata_with_main_paths(
            name,
            name,
            PathList::new(&[PathBuf::from(DELETED_WORKTREE_PATH)]),
            PathList::new(&[PathBuf::from("/project")]),
            time(day),
            cx,
        );
    }
    for (name, day, project) in [("main", 3, &main_project), ("other", 4, &other_project)] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(name)),
            Some(name.into()),
            time(day),
            None,
            None,
            project,
            cx,
        );
    }
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    let fallback = |removed: &str, cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _| {
            let title = |entry: &ListEntry| match entry {
                ListEntry::Thread(thread) => {
                    thread.metadata.title.as_ref().map(ToString::to_string)
                }
                _ => None,
            };
            let position = sidebar
                .contents
                .entries
                .iter()
                .position(|entry| title(entry).as_deref() == Some(removed))
                .expect("thread should be listed");
            sidebar
                .top_entry_after_removal(position)
                .and_then(|entry| title(&entry))
        })
    };
    assert_eq!(
        fallback("worktree-new", cx).as_deref(),
        Some("worktree-old"),
        "same worktree group first"
    );
    assert_eq!(
        fallback("main", cx).as_deref(),
        Some("worktree-new"),
        "then the same project"
    );
    assert_eq!(
        fallback("other", cx).as_deref(),
        Some("main"),
        "then the top project"
    );
}

#[gpui::test]
async fn test_closed_project_lists_worktrees_and_branches_from_git(cx: &mut TestAppContext) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    fs.insert_tree("/other", serde_json::json!({ "src": {} }))
        .await;
    let other_project = project::Project::test(fs.clone(), ["/other".as_ref()], cx).await;
    let (multi_workspace, cx) = cx
        .add_window_view(|window, cx| MultiWorkspace::test_new(other_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let other_workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let main_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(main_project.clone(), window, cx)
    });
    cx.run_until_parked();
    multi_workspace
        .update_in(cx, |multi_workspace, window, cx| {
            // Removing the active workspace would reopen its project.
            multi_workspace.activate(other_workspace, None, window, cx);
            multi_workspace.remove(vec![main_workspace], RemovalIntent::KeepProject, window, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let linked_headers = |cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _| {
            sidebar
                .contents
                .entries
                .iter()
                .filter_map(|entry| match entry {
                    ListEntry::WorktreeHeader {
                        label,
                        branch,
                        is_linked: true,
                        ..
                    } => Some((label.to_string(), branch.as_ref().map(ToString::to_string))),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(
        linked_headers(cx),
        vec![("feature-a".to_string(), Some("feature-a".to_string()))]
    );

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/project/feature-b/project"),
            ref_name: Some("refs/heads/feature-b".into()),
            sha: "bbb".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;
    sidebar.update(cx, |sidebar, cx| {
        sidebar.fetch_closed_group_worktrees(true, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        linked_headers(cx),
        vec![
            ("feature-b".to_string(), Some("feature-b".to_string())),
            ("feature-a".to_string(), Some("feature-a".to_string())),
        ]
    );

    // The listing stays in use while a reopened project is still loading its git state.
    let reopened_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(reopened_project.clone(), window, cx)
    });
    for _ in 0..8 {
        sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
        assert_eq!(linked_headers(cx).len(), 2);
        cx.executor().tick();
    }
    cx.run_until_parked();
    assert_eq!(linked_headers(cx).len(), 2);
}

#[gpui::test]
async fn test_create_worktree_for_closed_project(cx: &mut TestAppContext) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    fs.insert_tree("/other", serde_json::json!({ "src": {} }))
        .await;
    let other_project = project::Project::test(fs.clone(), ["/other".as_ref()], cx).await;
    let (multi_workspace, cx) = cx
        .add_window_view(|window, cx| MultiWorkspace::test_new(other_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let other_workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let main_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(main_project.clone(), window, cx)
    });
    cx.run_until_parked();
    multi_workspace
        .update_in(cx, |multi_workspace, window, cx| {
            // Removing the active workspace would reopen its project.
            multi_workspace.activate(other_workspace, None, window, cx);
            multi_workspace.remove(vec![main_workspace], RemovalIntent::KeepProject, window, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let key = multi_workspace.read_with(cx, |multi_workspace, cx| {
        multi_workspace
            .project_groups(cx)
            .into_iter()
            .find(|group| group.key.path_list().paths() == [PathBuf::from("/project")])
            .map(|group| (group.key, group.workspaces.len()))
            .expect("the main project should have a group")
    });
    let (key, open_workspace_count) = key;
    assert_eq!(open_workspace_count, 0);
    let repository = fs.open_repo(Path::new("/project/.git"), None).unwrap();
    let worktree_count_before = repository.worktrees().await.unwrap().len();

    let base_was_shown = Rc::new(Cell::new(false));
    let _subscription = cx.update(|_, cx| {
        cx.observe(&multi_workspace, {
            let base_was_shown = base_was_shown.clone();
            move |multi_workspace, cx| {
                let shown = multi_workspace.read(cx).workspace().read(cx).root_paths(cx);
                if shown
                    .iter()
                    .any(|root| root.as_ref() == Path::new("/project"))
                {
                    base_was_shown.set(true);
                }
            }
        })
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        assert!(sidebar.can_create_worktree(&key, cx));
        sidebar.create_worktree_for_group(&key, window, cx);
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }

    assert_eq!(
        repository.worktrees().await.unwrap().len(),
        worktree_count_before + 1
    );
    assert!(
        !base_was_shown.get(),
        "the base checkout only lends its repository, so it should stay out of sight"
    );
    assert!(
        multi_workspace.read_with(cx, |multi_workspace, cx| {
            multi_workspace
                .workspace_for_paths(key.path_list(), None, cx)
                .is_none()
        }),
        "the base checkout only lends its repository, so it should be closed again"
    );
}

#[gpui::test]
async fn test_archive_thread_with_deleted_closed_linked_worktree(cx: &mut TestAppContext) {
    let (main_project, _fs) = init_test_project_with_git("/project", cx).await;
    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let main_workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());

    // A thread on a linked worktree that no longer exists on disk.
    let worktree_session_id = acp::SessionId::new(Arc::from("worktree-thread"));
    let worktree_folder_paths =
        PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]);
    save_thread_metadata_with_main_paths(
        "worktree-thread",
        "Worktree Thread",
        worktree_folder_paths,
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        let thread = sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Thread(thread)
                    if thread.metadata.session_id.as_ref() == Some(&worktree_session_id) =>
                {
                    Some(thread)
                }
                _ => None,
            })
            .expect("worktree thread should be visible in sidebar");
        assert!(
            matches!(thread.workspace, ThreadEntryWorkspace::Closed { .. }),
            "thread should start closed so archiving has to open its workspace"
        );
    });

    // Opening the missing worktree falls back to the main workspace, so the
    // thread's entry is still closed after archiving opens it.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&worktree_session_id, window, cx);
    });
    cx.run_until_parked();

    let thread_archived = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&worktree_session_id)
            .map(|thread| thread.archived)
    });
    assert_eq!(
        thread_archived,
        Some(true),
        "thread should be archived even though its worktree no longer exists"
    );
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .cloned()
            .collect::<Vec<_>>()),
        vec![main_workspace],
        "archiving should leave only the main workspace open"
    );
}

#[gpui::test]
async fn test_archive_selected_thread_closes_selected_agent_panel_terminal(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);
    let terminal_index = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id))
            .expect("terminal should be visible in sidebar")
    });
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(terminal_index);
    });
    cx.dispatch_action(ArchiveSelectedThread);
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert!(!panel.has_terminal(terminal_id));
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(sidebar.contents.entries.iter().all(|entry| {
            !matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id)
        }));
    });
    sidebar.read_with(cx, |_sidebar, cx| {
        let store = TerminalThreadMetadataStore::global(cx).read(cx);
        assert!(
            store.entry(terminal_id).is_none(),
            "terminal metadata should be deleted when closing from the sidebar"
        );
    });
}

#[gpui::test]
async fn test_closing_active_agent_panel_terminal_activates_neighbor(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let build_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Build", true, window, cx)
        })
        .expect("build test terminal should be inserted");
    let server_terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Server", true, window, cx)
        })
        .expect("server test terminal should be inserted");
    cx.run_until_parked();

    let (server_metadata, server_workspace) = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Terminal(terminal)
                    if terminal.metadata.terminal_id == server_terminal_id =>
                {
                    Some((terminal.metadata.clone(), terminal.workspace.clone()))
                }
                _ => None,
            })
            .expect("server terminal should be visible in sidebar")
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.close_terminal(&server_metadata, &server_workspace, window, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, _cx| {
        assert!(!panel.has_terminal(server_terminal_id));
        assert_eq!(panel.active_terminal_id(), Some(build_terminal_id));
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Terminal { terminal_id, .. }) if *terminal_id == build_terminal_id),
            "expected remaining terminal to become active, got {:?}",
            sidebar.active_entry,
        );
    });
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  Build"]
    );
}

#[gpui::test]
async fn test_running_project_thread_in_another_window(cx: &mut TestAppContext) {
    let owner_project = init_test_project_with_agent_panel("/project", cx).await;
    let fs = cx.update(|cx| <dyn Fs>::global(cx));
    let observer_project = project::Project::test(fs, [], cx).await;
    let mut other_context = cx.clone();
    let (owner, owner_cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(owner_project, window, cx));
    let (observer, cx) = other_context
        .add_window_view(|window, cx| MultiWorkspace::test_new(observer_project, window, cx));
    let (_, panel) = setup_sidebar_with_agent_panel(&owner, owner_cx);
    let sidebar = setup_sidebar(&observer, cx);
    let connection = StubAgentConnection::new();
    open_thread_with_connection(&panel, connection.clone(), owner_cx);
    send_message(&panel, owner_cx);
    let session = active_session_id(&panel, owner_cx);
    let thread_id = panel.read_with(owner_cx, |panel, cx| panel.active_thread_id(cx).unwrap());
    cx.run_until_parked();

    let store = cx.update(|_, cx| ThreadMetadataStore::global(cx));
    let archived = |cx: &App| store.read(cx).entry(thread_id).unwrap().archived;
    let status = |cx: &gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _| {
            sidebar
                .contents
                .entries
                .iter()
                .find_map(|entry| match entry {
                    ListEntry::Thread(thread) if thread.metadata.thread_id == thread_id => {
                        Some(thread.status)
                    }
                    _ => None,
                })
        })
    };
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.selection = sidebar.contents.entries.iter().position(|entry| {
            matches!(entry,
                ListEntry::Thread(thread) if thread.metadata.thread_id == thread_id
            )
        });
        assert!(sidebar.selection.is_some());
        sidebar.archive_selected_thread(&ArchiveSelectedThread, window, cx);
        assert!(!archived(cx));
    });
    assert_eq!(status(cx), Some(AgentThreadStatus::Running));
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session, window, cx);
        assert!(!archived(cx));
        sidebar.stop_thread(&thread_id, cx);
    });
    cx.run_until_parked();
    assert_eq!(connection.take_cancel_calls(), vec![session.clone()]);
    assert_eq!(status(cx), Some(AgentThreadStatus::Completed));
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session, window, cx);
        assert!(archived(cx));
    });
}

#[gpui::test]
async fn test_parallel_threads_shown_with_live_status(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Open thread A and keep it generating.
    let connection = StubAgentConnection::new();
    open_thread_with_connection(&panel, connection.clone(), cx);
    send_message(&panel, cx);

    let session_id_a = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id_a, &project, cx).await;

    cx.update(|_, cx| {
        connection.send_update(
            session_id_a.clone(),
            acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new(
                "working...".into(),
            )),
            cx,
        );
    });
    cx.run_until_parked();

    // Open thread B (idle, default response) — thread A goes to background.
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let session_id_b = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id_b, &project, cx).await;

    cx.run_until_parked();

    let mut entries = visible_entries_as_strings(&sidebar, cx);
    entries[1..].sort();
    assert_eq!(
        entries,
        vec![
            //
            "v [my-project]",
            "  Hello *",
            "  Hello * (running)",
        ]
    );

    // Collapsing the worktree group hides the rows, so its header carries the status.
    let ids = worktree_header_ids(&sidebar, cx);
    let [id] = ids.as_slice() else {
        panic!("expected one worktree header, found {ids:?}");
    };
    sidebar.update(cx, |sidebar, cx| sidebar.toggle_worktree_group(id, cx));
    cx.run_until_parked();
    sidebar.read_with(cx, |sidebar, _| {
        let [
            ListEntry::ProjectHeader { .. },
            ListEntry::WorktreeHeader {
                is_collapsed: true,
                has_running_threads: true,
                waiting_thread_count: 0,
                ..
            },
        ] = sidebar.contents.entries.as_slice()
        else {
            panic!("expected a collapsed worktree header that reports the running thread");
        };
    });
}

#[gpui::test]
async fn test_subagent_permission_request_marks_parent_sidebar_thread_waiting(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let fs = cx.update(|cx| <dyn Fs>::global(cx));
    let observing_project = project::Project::test(fs, [], cx).await;
    let mut observer = cx.clone();
    let (observing_window, observer) = observer
        .add_window_view(|window, cx| MultiWorkspace::test_new(observing_project, window, cx));
    let observing_sidebar = setup_sidebar(&observing_window, observer);
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let connection = StubAgentConnection::new().with_supports_load_session(true);
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let parent_session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&parent_session_id, &project, cx).await;

    let subagent_session_id = acp::SessionId::new("subagent-session");
    let parent_thread = cx.update(|_, cx| panel.read(cx).active_agent_thread(cx).unwrap());
    let subagent = cx
        .update(|_, cx| {
            let parent = parent_thread.read(cx);
            parent.connection().clone().load_session(
                subagent_session_id.clone(),
                parent.project().clone(),
                PathList::default(),
                None,
                cx,
            )
        })
        .await
        .expect("subagent session");
    parent_thread.update(cx, |thread: &mut AcpThread, cx| {
        thread.register_subagent(subagent, cx);
    });
    cx.run_until_parked();

    let subagent_thread = panel.read_with(cx, |panel, cx| {
        panel
            .active_conversation_view()
            .and_then(|conversation| conversation.read(cx).thread_view(&subagent_session_id))
            .map(|thread_view| thread_view.read(cx).thread.clone())
            .expect("Expected subagent thread to be loaded into the conversation")
    });
    request_test_tool_authorization(&subagent_thread, "subagent-tool-call", "allow-subagent", cx);

    for sidebar in [sidebar, observing_sidebar] {
        let parent_status = sidebar.read_with(cx, |sidebar, _cx| {
            sidebar
                .contents
                .entries
                .iter()
                .find_map(|entry| match entry {
                    ListEntry::Thread(thread)
                        if thread.metadata.session_id.as_ref() == Some(&parent_session_id) =>
                    {
                        Some(thread.status)
                    }
                    _ => None,
                })
                .expect("Expected parent thread entry in sidebar")
        });

        assert_eq!(parent_status, AgentThreadStatus::WaitingForConfirmation);
    }
}

#[gpui::test]
async fn test_elicitation_marks_sidebar_thread_waiting_until_responded(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    open_thread_with_connection(&panel, StubAgentConnection::new(), cx);
    send_message(&panel, cx);
    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    let thread = cx.update(|_, cx| panel.read(cx).active_agent_thread(cx).unwrap());
    let thread_status = |cx: &mut VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _cx| {
            sidebar
                .contents
                .entries
                .iter()
                .find_map(|entry| match entry {
                    ListEntry::Thread(thread)
                        if thread.metadata.session_id.as_ref() == Some(&session_id) =>
                    {
                        Some(thread.status)
                    }
                    _ => None,
                })
                .expect("Expected thread entry in sidebar")
        })
    };
    assert_eq!(thread_status(cx), AgentThreadStatus::Running);

    for action in [
        acp_v2::ElicitationAction::Accept(acp_v2::ElicitationAcceptAction::new().content(
            agent_client_protocol::schema::IndexMap::from([(
                "choice".to_string(),
                acp_v2::ElicitationContentValue::from("first"),
            )]),
        )),
        acp_v2::ElicitationAction::Decline,
        acp_v2::ElicitationAction::Cancel,
    ] {
        let (elicitation_id, response_task) = thread.update(cx, |thread, cx| {
            thread
                .request_elicitation_with_id(
                    acp_v2::CreateElicitationRequest::new(
                        acp_v2::ElicitationFormMode::new(
                            acp_v2::ElicitationSessionScope::new(session_id.clone()),
                            serde_json::from_value(serde_json::json!({
                                "type": "object",
                                "required": ["choice"],
                                "properties": {
                                    "choice": {
                                        "type": "string",
                                        "oneOf": [
                                            { "const": "first", "title": "First" },
                                            { "const": "second", "title": "Second" }
                                        ]
                                    }
                                }
                            }))
                            .expect("Valid single-select schema"),
                        ),
                        "Choose one",
                    ),
                    cx,
                )
                .expect("Elicitation should be accepted")
        });
        cx.run_until_parked();
        assert_eq!(thread_status(cx), AgentThreadStatus::WaitingForConfirmation);

        thread.update(cx, |thread, cx| {
            thread.respond_to_elicitation(
                &elicitation_id,
                acp_v2::CreateElicitationResponse::new(action),
                cx,
            );
        });
        response_task.await;
        cx.run_until_parked();
        assert_eq!(thread_status(cx), AgentThreadStatus::Running);
    }
}

#[gpui::test]
async fn test_background_thread_completion_triggers_notification(cx: &mut TestAppContext) {
    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Open thread on workspace A and keep it generating.
    let connection_a = StubAgentConnection::new();
    open_thread_with_connection(&panel_a, connection_a.clone(), cx);
    send_message(&panel_a, cx);

    let session_id_a = active_session_id(&panel_a, cx);
    save_test_thread_metadata(&session_id_a, &project_a, cx).await;

    cx.update(|_, cx| {
        connection_a.send_update(
            session_id_a.clone(),
            acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new("chunk".into())),
            cx,
        );
    });
    cx.run_until_parked();

    // Add a second workspace and activate it (making workspace A the background).
    let fs = cx.update(|_, cx| <dyn fs::Fs>::global(cx));
    let project_b = project::Project::test(fs, [], cx).await;
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx);
    });
    cx.run_until_parked();

    // Thread A is still running; no notification yet.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project-a]",
            "  Hello * (running)",
        ]
    );

    // Complete thread A's turn (transition Running → Completed).
    connection_a.end_turn(session_id_a.clone(), acp_v1::StopReason::EndTurn);
    cx.run_until_parked();

    // The completed background thread shows a notification indicator.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project-a]",
            "  Hello * (!)",
        ]
    );
}

fn type_in_search(sidebar: &Entity<Sidebar>, query: &str, cx: &mut gpui::VisualTestContext) {
    sidebar.update_in(cx, |sidebar, window, cx| {
        window.focus(&sidebar.filter_editor.focus_handle(cx), cx);
        sidebar.filter_editor.update(cx, |editor, cx| {
            editor.set_text(query, window, cx);
        });
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn test_search_narrows_visible_threads_to_matches(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    for (id, title, hour) in [
        ("t-1", "Fix crash in project panel", 3),
        ("t-2", "Add inline diff view", 2),
        ("t-3", "Refactor settings module", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project,
            cx,
        );
    }
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Fix crash in project panel",
            "  Add inline diff view",
            "  Refactor settings module",
        ]
    );

    // User types "diff" in the search box — only the matching thread remains,
    // with its workspace header preserved for context.
    type_in_search(&sidebar, "diff", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Add inline diff view  <== selected",
        ]
    );

    // User changes query to something with no matches — list is empty.
    type_in_search(&sidebar, "nonexistent", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        Vec::<String>::new()
    );
}

#[gpui::test]
async fn test_search_matches_regardless_of_case(cx: &mut TestAppContext) {
    // Scenario: A user remembers a thread title but not the exact casing.
    // Search should match case-insensitively so they can still find it.
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-1")),
        Some("Fix Crash In Project Panel".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    // Lowercase query matches mixed-case title.
    type_in_search(&sidebar, "fix crash", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Fix Crash In Project Panel  <== selected",
        ]
    );

    // Uppercase query also matches the same title.
    type_in_search(&sidebar, "FIX CRASH", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Fix Crash In Project Panel  <== selected",
        ]
    );
}

#[gpui::test]
async fn test_escape_from_search_focuses_first_thread(cx: &mut TestAppContext) {
    // Scenario: A user searches, finds what they need, then presses Escape
    // in the search field to hand keyboard control back to the thread list.
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    for (id, title, hour) in [("t-1", "Alpha thread", 2), ("t-2", "Beta thread", 1)] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project,
            cx,
        )
    }
    cx.run_until_parked();

    // Confirm the full list is showing.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Alpha thread",
            "  Beta thread",
        ]
    );

    // User types a search query to filter down.
    focus_sidebar(&sidebar, cx);
    type_in_search(&sidebar, "alpha", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Alpha thread  <== selected",
        ]
    );

    // First Escape clears the search text, restoring the full list.
    // Focus stays on the filter editor.
    cx.dispatch_action(Cancel);
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Alpha thread",
            "  Beta thread",
        ]
    );
    sidebar.update_in(cx, |sidebar, window, cx| {
        assert!(sidebar.filter_editor.read(cx).is_focused(window));
        assert!(!sidebar.focus_handle.is_focused(window));
    });

    // Second Escape moves focus from the empty search field to the thread list.
    cx.dispatch_action(Cancel);
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        assert_eq!(sidebar.selection, Some(2));
        assert!(sidebar.focus_handle.is_focused(window));
        assert!(!sidebar.filter_editor.read(cx).is_focused(window));
    });
}

#[gpui::test]
async fn test_search_only_shows_workspace_headers_with_matches(cx: &mut TestAppContext) {
    let project_a = init_test_project("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    for (id, title, hour) in [
        ("a1", "Fix bug in sidebar", 2),
        ("a2", "Add tests for editor", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project_a,
            cx,
        )
    }

    // Add a workspace with no folder.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.create_test_workspace(window, cx).detach();
    });
    cx.run_until_parked();

    let project_b = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces().nth(1).unwrap().read(cx).project().clone()
    });

    for (id, title, hour) in [
        ("b1", "Refactor sidebar layout", 3),
        ("b2", "Fix typo in README", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project_b,
            cx,
        )
    }
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Refactor sidebar layout",
            "  Fix typo in README",
            "v [project-a]",
            "  Fix bug in sidebar",
            "  Add tests for editor",
        ]
    );

    // "sidebar" matches a thread in each workspace — both sections contribute a row.
    type_in_search(&sidebar, "sidebar", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Refactor sidebar layout  <== selected",
            "v [project-a]",
            "  Fix bug in sidebar",
        ]
    );

    // "typo" only matches in the second workspace — the first header disappears.
    type_in_search(&sidebar, "typo", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["  Fix typo in README  <== selected"]
    );

    // "project-a" matches the first workspace name — the header appears
    // with all child threads included.
    type_in_search(&sidebar, "project-a", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project-a]",
            "  Fix bug in sidebar  <== selected",
            "  Add tests for editor",
        ]
    );
}

#[gpui::test]
async fn test_search_matches_workspace_name(cx: &mut TestAppContext) {
    let project_a = init_test_project("/alpha-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    for (id, title, hour) in [
        ("a1", "Fix bug in sidebar", 2),
        ("a2", "Add tests for editor", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project_a,
            cx,
        )
    }

    // Add a workspace with no folder.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.create_test_workspace(window, cx).detach();
    });
    cx.run_until_parked();

    let project_b = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces().nth(1).unwrap().read(cx).project().clone()
    });

    for (id, title, hour) in [
        ("b1", "Refactor sidebar layout", 3),
        ("b2", "Fix typo in README", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project_b,
            cx,
        )
    }
    cx.run_until_parked();

    // "alpha" matches the workspace name "alpha-project" but no thread titles.
    // The workspace header should appear with all child threads included.
    type_in_search(&sidebar, "alpha", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [alpha-project]",
            "  Fix bug in sidebar  <== selected",
            "  Add tests for editor",
        ]
    );

    // "sidebar" matches thread titles in both workspaces but not workspace names.
    // Both headers appear with their matching threads.
    type_in_search(&sidebar, "sidebar", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Refactor sidebar layout  <== selected",
            "v [alpha-project]",
            "  Fix bug in sidebar",
        ]
    );

    // "alpha sidebar" matches the workspace name "alpha-project" (fuzzy: a-l-p-h-a-s-i-d-e-b-a-r
    // doesn't match) — but does not match either workspace name or any thread.
    // Actually let's test something simpler: a query that matches both a workspace
    // name AND some threads in that workspace. Matching threads should still appear.
    type_in_search(&sidebar, "fix", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "  Fix typo in README  <== selected",
            "v [alpha-project]",
            "  Fix bug in sidebar",
        ]
    );

    // A query that matches a workspace name AND a thread in that same workspace.
    // Both the header (highlighted) and all child threads should appear.
    type_in_search(&sidebar, "alpha", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [alpha-project]",
            "  Fix bug in sidebar  <== selected",
            "  Add tests for editor",
        ]
    );

    // Now search for something that matches only a workspace name when there
    // are also threads with matching titles — the non-matching workspace's
    // threads should still appear if their titles match.
    type_in_search(&sidebar, "alp", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [alpha-project]",
            "  Fix bug in sidebar  <== selected",
            "  Add tests for editor",
        ]
    );
}

#[gpui::test]
async fn test_search_finds_threads_inside_collapsed_groups(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-1")),
        Some("Important thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    // User focuses the sidebar and collapses the group using keyboard:
    // manually select the header, then press SelectParent to collapse.
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(0);
    });
    cx.dispatch_action(SelectParent);
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "> [my-project]  <== selected",
        ]
    );

    // User types a search — the thread appears even though its group is collapsed.
    type_in_search(&sidebar, "important", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "> [my-project]",
            "  Important thread  <== selected",
        ]
    );
}

#[gpui::test]
async fn test_search_then_keyboard_navigate_and_confirm(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    for (id, title, hour) in [
        ("t-1", "Fix crash in panel", 3),
        ("t-2", "Fix lint warnings", 2),
        ("t-3", "Add new feature", 1),
    ] {
        save_thread_metadata(
            acp::SessionId::new(Arc::from(id)),
            Some(title.into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, hour, 0, 0).unwrap(),
            None,
            None,
            &project,
            cx,
        )
    }
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);

    // User types "fix" — two threads match.
    type_in_search(&sidebar, "fix", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Fix crash in panel  <== selected",
            "  Fix lint warnings",
        ]
    );

    // Selection starts on the first matching thread. User presses
    // SelectNext to move to the second match.
    cx.dispatch_action(SelectNext);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Fix crash in panel",
            "  Fix lint warnings  <== selected",
        ]
    );

    // User can also jump back with SelectPrevious.
    cx.dispatch_action(SelectPrevious);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Fix crash in panel  <== selected",
            "  Fix lint warnings",
        ]
    );
}

#[gpui::test]
async fn test_confirm_on_historical_thread_activates_workspace(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.create_test_workspace(window, cx).detach();
    });
    cx.run_until_parked();

    let (workspace_0, workspace_1) = multi_workspace.read_with(cx, |mw, _| {
        (
            mw.workspaces().next().unwrap().clone(),
            mw.workspaces().nth(1).unwrap().clone(),
        )
    });

    save_thread_metadata(
        acp::SessionId::new(Arc::from("hist-1")),
        Some("Historical Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Historical Thread",
        ]
    );

    // Switch to workspace 1 so we can verify the confirm switches back.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().nth(1).unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_1
    );

    // Before a previous fix, the workspace field was Option<usize> and
    // historical threads had None, so activate_thread early-returned
    // without switching the workspace.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.selection = Some(2);
        sidebar.confirm(&Confirm, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_0
    );
}

#[gpui::test]
async fn test_confirm_on_historical_thread_preserves_historical_timestamp_and_order(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let newer_session_id = acp::SessionId::new(Arc::from("newer-historical-thread"));
    let newer_timestamp = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 2, 0, 0, 0).unwrap();
    save_thread_metadata(
        newer_session_id,
        Some("Newer Historical Thread".into()),
        newer_timestamp,
        Some(newer_timestamp),
        None,
        &project,
        cx,
    );

    let older_session_id = acp::SessionId::new(Arc::from("older-historical-thread"));
    let older_timestamp = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 1, 0, 0, 0).unwrap();
    save_thread_metadata(
        older_session_id.clone(),
        Some("Older Historical Thread".into()),
        older_timestamp,
        Some(older_timestamp),
        None,
        &project,
        cx,
    );

    cx.run_until_parked();
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    let historical_entries_before: Vec<_> = visible_entries_as_strings(&sidebar, cx)
        .into_iter()
        .filter(|entry| entry.contains("Historical Thread"))
        .collect();
    assert_eq!(
        historical_entries_before,
        vec![
            "  Newer Historical Thread".to_string(),
            "  Older Historical Thread".to_string(),
        ],
        "expected the sidebar to sort historical threads by their saved timestamp before activation"
    );

    let older_entry_index = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(entry, ListEntry::Thread(thread)
                    if thread.metadata.session_id.as_ref() == Some(&older_session_id))
            })
            .expect("expected Older Historical Thread to appear in the sidebar")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.selection = Some(older_entry_index);
        sidebar.confirm(&Confirm, window, cx);
    });
    cx.run_until_parked();

    let older_metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&older_session_id)
            .cloned()
            .expect("expected metadata for Older Historical Thread after activation")
    });
    assert_eq!(
        older_metadata.created_at,
        Some(older_timestamp),
        "activating a historical thread should not rewrite its saved created_at timestamp"
    );

    let historical_entries_after: Vec<_> = visible_entries_as_strings(&sidebar, cx)
        .into_iter()
        .filter(|entry| entry.contains("Historical Thread"))
        .collect();
    assert_eq!(
        historical_entries_after,
        vec![
            "  Newer Historical Thread".to_string(),
            "  Older Historical Thread  <== selected".to_string(),
        ],
        "activating an older historical thread should not reorder it ahead of a newer historical thread"
    );
}

#[gpui::test]
async fn test_confirm_on_historical_thread_in_new_project_group_opens_real_thread(
    cx: &mut TestAppContext,
) {
    use workspace::ProjectGroup;

    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        set_max_idle_retained_threads(1, cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let project_b_key = project_b.read_with(cx, |project, cx| project.project_group_key(cx));
    multi_workspace.update(cx, |multi_workspace, cx| {
        multi_workspace.test_add_project_group(
            ProjectGroup {
                key: project_b_key.clone(),
                workspaces: Vec::new(),
                expanded: true,
            },
            cx,
        );
    });

    let session_id = acp::SessionId::new(Arc::from("historical-new-project-group"));
    save_thread_metadata(
        session_id.clone(),
        Some("Historical Thread in New Group".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project_b,
        cx,
    );
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    let entries_before = visible_entries_as_strings(&sidebar, cx);
    assert_eq!(
        entries_before,
        vec![
            "v [project-a]",
            "v [project-b]",
            "  Historical Thread in New Group",
        ],
        "expected the closed project group to show the historical thread before first open"
    );

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "should start without an open workspace for the new project group"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        // Both projects have a worktree header between their header and rows.
        sidebar.selection = Some(4);
        sidebar.confirm(&Confirm, window, cx);
    });

    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "confirming the historical thread should open a workspace for the new project group"
    );

    let workspace_b = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces()
            .find(|workspace| {
                PathList::new(&workspace.read(cx).root_paths(cx))
                    == project_b_key.path_list().clone()
            })
            .cloned()
            .expect("expected workspace for project-b after opening the historical thread")
    });

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b,
        "opening the historical thread should activate the new project's workspace"
    );

    let panel = workspace_b.read_with(cx, |workspace, cx| {
        workspace
            .panel::<AgentPanel>(cx)
            .expect("expected first-open activation to bootstrap the agent panel")
    });

    let expected_thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("metadata should still map session id to thread id")
    });

    assert_eq!(
        panel.read_with(cx, |panel, cx| panel.active_thread_id(cx)),
        Some(expected_thread_id),
        "expected the agent panel to activate the real historical thread rather than a draft"
    );

    let entries_after = visible_entries_as_strings(&sidebar, cx);
    let matching_rows: Vec<_> = entries_after
        .iter()
        .filter(|entry| entry.contains("Historical Thread in New Group") || entry.contains("Draft"))
        .cloned()
        .collect();
    assert_eq!(
        matching_rows.len(),
        1,
        "expected only one matching row after first open into a new project group, got entries: {entries_after:?}"
    );
    assert!(
        matching_rows[0].contains("Historical Thread in New Group"),
        "expected the surviving row to be the real historical thread, got entries: {entries_after:?}"
    );
    assert!(
        !matching_rows[0].contains("Draft"),
        "expected no draft row after first open into a new project group, got entries: {entries_after:?}"
    );
}

struct SidebarClickFixture {
    sidebar: Entity<Sidebar>,
    panel: Entity<AgentPanel>,
}

impl SidebarClickFixture {
    fn new(multi_workspace: &Entity<MultiWorkspace>, cx: &mut gpui::VisualTestContext) -> Self {
        let (sidebar, panel) = setup_sidebar_with_agent_panel(multi_workspace, cx);
        Self { sidebar, panel }
    }

    fn insert_terminal(&self, title: &str, cx: &mut gpui::VisualTestContext) -> TerminalId {
        let terminal_id = self
            .panel
            .update_in(cx, |panel, window, cx| {
                panel.insert_test_terminal(title, true, window, cx)
            })
            .expect("test terminal should be inserted");
        cx.run_until_parked();
        terminal_id
    }

    fn terminal_index(&self, terminal_id: TerminalId, cx: &mut gpui::VisualTestContext) -> usize {
        self.sidebar.read_with(cx, |sidebar, _cx| {
            sidebar
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
                .expect("terminal should be visible in sidebar")
        })
    }

    fn thread_index(&self, session_id: &acp::SessionId, cx: &mut gpui::VisualTestContext) -> usize {
        self.sidebar.read_with(cx, |sidebar, _cx| {
            sidebar
                .contents
                .entries
                .iter()
                .position(|entry| {
                    matches!(
                        entry,
                        ListEntry::Thread(thread)
                            if thread.metadata.session_id.as_ref() == Some(session_id)
                    )
                })
                .expect("thread should be visible in sidebar")
        })
    }

    fn select_and_focus(&self, entry_index: usize, cx: &mut gpui::VisualTestContext) {
        self.sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.selection = Some(entry_index);
            sidebar.focus_handle.focus(window, cx);
            cx.notify();
        });
        cx.run_until_parked();
    }

    fn click(&self, entry_index: usize, cx: &mut gpui::VisualTestContext) {
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(400.), px(400.)),
            |_, _| self.sidebar.clone().into_any_element(),
        );
        let entry_bounds = self.sidebar.read_with(cx, |sidebar, _cx| {
            sidebar
                .list_state
                .bounds_for_item(entry_index)
                .expect("sidebar entry should be measured")
        });
        cx.simulate_click(entry_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    }
}

#[gpui::test]
async fn test_keyboard_confirm_on_terminal_preserves_selection(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let fixture = SidebarClickFixture::new(&multi_workspace, cx);
    let terminal_id = fixture.insert_terminal("Terminal A", cx);
    let other_terminal_id = fixture.insert_terminal("Terminal B", cx);
    let terminal_index = fixture.terminal_index(terminal_id, cx);
    fixture.select_and_focus(terminal_index, cx);

    fixture.sidebar.update_in(cx, |sidebar, window, _cx| {
        assert!(sidebar.focus_handle.is_focused(window));
        assert_eq!(sidebar.selection, Some(terminal_index));
        assert!(
            matches!(
                &sidebar.active_entry,
                Some(ActiveEntry::Terminal { terminal_id: active_terminal_id, .. })
                    if *active_terminal_id == other_terminal_id
            ),
            "Terminal B should be active before confirming Terminal A, got {:?}",
            sidebar.active_entry,
        );
    });

    // Confirm switches from Terminal B to Terminal A without clearing selection.
    cx.dispatch_action(Confirm);
    cx.run_until_parked();
    fixture.sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(sidebar.selection, Some(terminal_index));
        assert!(
            matches!(
                &sidebar.active_entry,
                Some(ActiveEntry::Terminal { terminal_id: active_terminal_id, .. })
                    if *active_terminal_id == terminal_id
            ),
            "confirmed terminal should be active, got {:?}",
            sidebar.active_entry,
        );
    });
}

#[gpui::test]
async fn test_keyboard_confirm_on_thread_preserves_selection(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let fixture = SidebarClickFixture::new(&multi_workspace, cx);
    let session_id = acp::SessionId::new(Arc::from("thread-a"));
    save_thread_metadata(
        session_id.clone(),
        Some("Thread A".into()),
        Utc::now(),
        None,
        None,
        &project,
        cx,
    );
    let thread_index = fixture.thread_index(&session_id, cx);
    fixture.select_and_focus(thread_index, cx);

    // Confirm must preserve selection without the test setting it again.
    cx.dispatch_action(Confirm);
    cx.run_until_parked();
    fixture.sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(sidebar.selection, Some(thread_index));
        assert_active_thread(sidebar, &session_id, "confirmed thread should be active");
    });
}

#[gpui::test]
async fn test_clicking_different_terminal_clears_sidebar_selection(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let fixture = SidebarClickFixture::new(&multi_workspace, cx);
    let terminal_a_id = fixture.insert_terminal("Terminal A", cx);
    let terminal_b_id = fixture.insert_terminal("Terminal B", cx);
    let terminal_a_index = fixture.terminal_index(terminal_a_id, cx);
    let terminal_b_index = fixture.terminal_index(terminal_b_id, cx);

    fixture.panel.update_in(cx, |panel, window, cx| {
        panel.activate_terminal(terminal_a_id, true, window, cx);
    });
    cx.run_until_parked();
    fixture.select_and_focus(terminal_a_index, cx);

    fixture.sidebar.update_in(cx, |sidebar, window, _cx| {
        assert!(sidebar.focus_handle.is_focused(window));
        assert_eq!(sidebar.selection, Some(terminal_a_index));
        assert!(
            matches!(
                &sidebar.active_entry,
                Some(ActiveEntry::Terminal { terminal_id, .. })
                    if *terminal_id == terminal_a_id
            ),
            "Terminal A should be active before clicking Terminal B, got {:?}",
            sidebar.active_entry,
        );
    });

    // Clicking a different terminal clears keyboard selection and activates that terminal.
    fixture.click(terminal_b_index, cx);

    fixture.sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(sidebar.selection, None);
        assert!(
            matches!(
                &sidebar.active_entry,
                Some(ActiveEntry::Terminal { terminal_id, .. })
                    if *terminal_id == terminal_b_id
            ),
            "Terminal B should be active after the click, got {:?}",
            sidebar.active_entry,
        );
    });
}

#[gpui::test]
async fn test_clicking_active_terminal_clears_sidebar_selection(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let fixture = SidebarClickFixture::new(&multi_workspace, cx);
    let terminal_id = fixture.insert_terminal("Terminal A", cx);
    let terminal_index = fixture.terminal_index(terminal_id, cx);
    fixture.select_and_focus(terminal_index, cx);

    fixture.sidebar.update_in(cx, |sidebar, window, _cx| {
        assert!(sidebar.focus_handle.is_focused(window));
        assert_eq!(sidebar.selection, Some(terminal_index));
        assert!(
            matches!(
                &sidebar.active_entry,
                Some(ActiveEntry::Terminal { terminal_id: active_terminal_id, .. })
                    if *active_terminal_id == terminal_id
            ),
            "Terminal A should be active before clicking it, got {:?}",
            sidebar.active_entry,
        );
    });

    // Clicking the active terminal clears keyboard selection without changing the active terminal.
    fixture.click(terminal_index, cx);

    fixture.sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(sidebar.selection, None);
        assert!(
            matches!(
                &sidebar.active_entry,
                Some(ActiveEntry::Terminal { terminal_id: active_terminal_id, .. })
                    if *active_terminal_id == terminal_id
            ),
            "Terminal A should remain active after the click, got {:?}",
            sidebar.active_entry,
        );
    });
}

#[gpui::test]
async fn test_clicking_different_thread_clears_sidebar_selection(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let fixture = SidebarClickFixture::new(&multi_workspace, cx);
    let thread_a_session_id = acp::SessionId::new(Arc::from("thread-a"));
    let thread_b_session_id = acp::SessionId::new(Arc::from("thread-b"));
    save_thread_metadata(
        thread_a_session_id.clone(),
        Some("Thread A".into()),
        Utc::now() + chrono::Duration::days(2),
        None,
        None,
        &project,
        cx,
    );
    save_thread_metadata(
        thread_b_session_id.clone(),
        Some("Thread B".into()),
        Utc::now() + chrono::Duration::days(1),
        None,
        None,
        &project,
        cx,
    );
    let thread_a_index = fixture.thread_index(&thread_a_session_id, cx);
    let thread_b_index = fixture.thread_index(&thread_b_session_id, cx);
    fixture.select_and_focus(thread_a_index, cx);
    cx.dispatch_action(Confirm);
    cx.run_until_parked();
    fixture.select_and_focus(thread_a_index, cx);

    fixture.sidebar.update_in(cx, |sidebar, window, _cx| {
        assert!(sidebar.focus_handle.is_focused(window));
        assert_eq!(sidebar.selection, Some(thread_a_index));
        assert_active_thread(
            sidebar,
            &thread_a_session_id,
            "Thread A should be active before clicking Thread B",
        );
    });

    // Clicking a different thread clears keyboard selection and activates that thread.
    fixture.click(thread_b_index, cx);

    fixture.sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(sidebar.selection, None);
        assert_active_thread(
            sidebar,
            &thread_b_session_id,
            "Thread B should be active after the click",
        );
    });
}

#[gpui::test]
async fn test_thread_title_update_propagates_to_sidebar(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Hi there!".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Hello *",
        ]
    );

    // Simulate the agent generating a title. The notification chain is:
    // AcpThread::set_title emits TitleUpdated →
    // ConnectionView::handle_thread_event calls cx.notify() →
    // AgentPanel observer fires and emits AgentPanelEvent →
    // Sidebar subscription calls update_entries / rebuild_contents.
    //
    // Before the fix, handle_thread_event did NOT call cx.notify() for
    // TitleUpdated, so the AgentPanel observer never fired and the
    // sidebar kept showing the old title.
    let thread = panel.read_with(cx, |panel, cx| panel.active_agent_thread(cx).unwrap());
    thread.update(cx, |thread, cx| {
        thread
            .set_title("Friendly Greeting with AI".into(), cx)
            .detach();
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Friendly Greeting with AI *",
        ]
    );
}

#[gpui::test]
async fn test_rename_thread_from_sidebar_updates_title_override(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Hi there!".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    let (entry_ix, thread_id, title) = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .enumerate()
            .find_map(|(ix, entry)| match entry {
                ListEntry::Thread(thread) => Some((
                    ix,
                    thread.metadata.thread_id,
                    thread.metadata.display_title(),
                )),
                ListEntry::ProjectHeader { .. }
                | ListEntry::WorktreeHeader { .. }
                | ListEntry::Terminal(_) => None,
            })
            .expect("sidebar should have a thread entry")
    });

    let renamed_title = "abcdefghijklmnopqrstuvwxyé renamed";
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.start_renaming_entry(entry_ix, RenameTarget::Thread(thread_id), title, window, cx);
    });
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.rename_editor.update(cx, |editor, cx| {
            editor.set_text(renamed_title, window, cx);
        });
    });
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.finish_entry_rename(window, cx);
    });
    cx.run_until_parked();

    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("thread metadata should exist")
    });
    assert_eq!(metadata.title_override.as_deref(), Some(renamed_title));
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  abcdefghijklmnopqrstuvwxyé renamed *  <== selected",
        ]
    );

    let active_thread = panel.read_with(cx, |panel, cx| panel.active_agent_thread(cx).unwrap());
    assert_eq!(
        active_thread.read_with(cx, |thread, _| thread.title()),
        Some(renamed_title.into())
    );
    let active_thread_view = panel.read_with(cx, |panel, cx| panel.active_thread_view(cx).unwrap());
    let title_editor_text =
        active_thread_view.read_with(cx, |view, cx| view.title_editor.read(cx).text(cx));
    assert_eq!(title_editor_text, renamed_title);

    active_thread.update(cx, |thread, cx| {
        thread
            .set_title("abcdefghijklmnopqrstuvwxyz0".into(), cx)
            .detach();
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  abcdefghijklmnopqrstuvwxyé renamed *  <== selected",
        ]
    );

    type_in_search(&sidebar, "0", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        Vec::<String>::new()
    );

    type_in_search(&sidebar, "é", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  abcdefghijklmnopqrstuvwxyé renamed *  <== selected",
        ]
    );
    sidebar.read_with(cx, |sidebar, _cx| {
        let thread = sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Thread(thread) => Some(thread),
                ListEntry::ProjectHeader { .. }
                | ListEntry::WorktreeHeader { .. }
                | ListEntry::Terminal(_) => None,
            })
            .expect("renamed thread should match the search");
        let title = thread.metadata.display_title();
        assert!(
            thread
                .highlight_positions
                .iter()
                .all(|position| { title.is_char_boundary(*position) })
        );
    });
}

#[gpui::test]
async fn test_rename_selected_thread_action_renames_selected_thread(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Hi there!".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    let (entry_ix, thread_id) = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .enumerate()
            .find_map(|(ix, entry)| match entry {
                ListEntry::Thread(thread) => Some((ix, thread.metadata.thread_id)),
                ListEntry::ProjectHeader { .. }
                | ListEntry::WorktreeHeader { .. }
                | ListEntry::Terminal(_) => None,
            })
            .expect("sidebar should have a thread entry")
    });

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(entry_ix);
    });
    cx.dispatch_action(RenameSelectedThread);
    cx.run_until_parked();

    let renamed_title = "Renamed via action";
    enter_renamed_title(&sidebar, RenameTarget::Thread(thread_id), renamed_title, cx);

    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("thread metadata should exist")
    });
    assert_eq!(metadata.title_override.as_deref(), Some(renamed_title));
}

#[gpui::test]
async fn test_rename_selected_thread_action_renames_terminal(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let terminal_id = panel
        .update_in(cx, |panel, window, cx| {
            panel.insert_test_terminal("Dev Server", true, window, cx)
        })
        .expect("test terminal should be inserted");
    cx.run_until_parked();

    let entry_ix = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
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
            .expect("sidebar should have a terminal entry")
    });

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(entry_ix);
    });
    cx.dispatch_action(RenameSelectedThread);
    cx.run_until_parked();

    let renamed_title = "Renamed Terminal";
    enter_renamed_title(
        &sidebar,
        RenameTarget::Terminal(terminal_id),
        renamed_title,
        cx,
    );

    panel.read_with(cx, |panel, cx| {
        let terminal = panel
            .terminals(cx)
            .into_iter()
            .find(|terminal| terminal.id == terminal_id)
            .expect("terminal should remain open after renaming");
        assert_eq!(terminal.custom_title.as_deref(), Some(renamed_title));
    });
    sidebar.read_with(cx, |_sidebar, cx| {
        let metadata = TerminalThreadMetadataStore::global(cx)
            .read(cx)
            .entry(terminal_id)
            .cloned()
            .expect("renamed terminal metadata should exist");
        assert_eq!(metadata.custom_title.as_deref(), Some(renamed_title));
    });
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  Renamed Terminal  <== selected"]
    );
}

#[gpui::test]
async fn test_focused_thread_tracks_user_intent(cx: &mut TestAppContext) {
    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Save a thread so it appears in the list.
    let connection_a = StubAgentConnection::new();
    connection_a.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel_a, connection_a, cx);
    send_message(&panel_a, cx);
    let session_id_a = active_session_id(&panel_a, cx);
    save_test_thread_metadata(&session_id_a, &project_a, cx).await;

    // Add a second workspace with its own agent panel.
    let fs = cx.update(|_, cx| <dyn fs::Fs>::global(cx));
    fs.as_fake()
        .insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    let project_b = project::Project::test(fs, ["/project-b".as_ref()], cx).await;
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    let workspace_a =
        multi_workspace.read_with(cx, |mw, _cx| mw.workspaces().next().unwrap().clone());

    // ── 1. Initial state: focused thread derived from active panel ─────
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_a,
            "The active panel's thread should be focused on startup",
        );
    });

    let thread_metadata_a = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&session_id_a)
            .cloned()
            .expect("session_id_a should exist in metadata store")
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.activate_thread(thread_metadata_a, &workspace_a, false, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_a,
            "After clicking a thread, it should be the focused thread",
        );
        assert!(
            has_thread_entry(sidebar, &session_id_a),
            "The clicked thread should be present in the entries"
        );
    });

    workspace_a.read_with(cx, |workspace, cx| {
        assert!(
            workspace.panel::<AgentPanel>(cx).is_some(),
            "Agent panel should exist"
        );
        let dock = workspace.left_dock().read(cx);
        assert!(
            dock.is_open(),
            "Clicking a thread should open the agent panel dock"
        );
    });

    let connection_b = StubAgentConnection::new();
    connection_b.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Thread B".into()),
    )]);
    open_thread_with_connection(&panel_b, connection_b, cx);
    send_message(&panel_b, cx);
    let session_id_b = active_session_id(&panel_b, cx);
    save_test_thread_metadata(&session_id_b, &project_b, cx).await;
    cx.run_until_parked();

    // Workspace A is currently active. Click a thread in workspace B,
    // which also triggers a workspace switch.
    let thread_metadata_b = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&session_id_b)
            .cloned()
            .expect("session_id_b should exist in metadata store")
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.activate_thread(thread_metadata_b, &workspace_b, false, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_b,
            "Clicking a thread in another workspace should focus that thread",
        );
        assert!(
            has_thread_entry(sidebar, &session_id_b),
            "The cross-workspace thread should be present in the entries"
        );
    });

    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().next().unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_a,
            "Switching workspace should seed focused_thread from the new active panel",
        );
        assert!(
            has_thread_entry(sidebar, &session_id_a),
            "The seeded thread should be present in the entries"
        );
    });

    let connection_b2 = StubAgentConnection::new();
    connection_b2.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new(DEFAULT_THREAD_TITLE.into()),
    )]);
    open_thread_with_connection(&panel_b, connection_b2, cx);
    send_message(&panel_b, cx);
    let session_id_b2 = active_session_id(&panel_b, cx);
    save_test_thread_metadata(&session_id_b2, &project_b, cx).await;
    cx.run_until_parked();

    // Panel B is not the active workspace's panel (workspace A is
    // active), so opening a thread there should not change focused_thread.
    // This prevents running threads in background workspaces from causing
    // the selection highlight to jump around.
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_a,
            "Opening a thread in a non-active panel should not change focused_thread",
        );
    });

    workspace_b.update_in(cx, |workspace, window, cx| {
        workspace.focus_handle(cx).focus(window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_a,
            "Defocusing the sidebar should not change focused_thread",
        );
    });

    // Switching workspaces via the multi_workspace (simulates clicking
    // a workspace header) should clear focused_thread.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().find(|w| *w == &workspace_b).cloned();
        if let Some(workspace) = workspace {
            mw.activate(workspace, None, window, cx);
        }
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_b2,
            "Switching workspace should seed focused_thread from the new active panel",
        );
        assert!(
            has_thread_entry(sidebar, &session_id_b2),
            "The seeded thread should be present in the entries"
        );
    });

    // ── 8. Focusing the agent panel thread keeps focused_thread ────
    // Workspace B still has session_id_b2 loaded in the agent panel.
    // Clicking into the thread (simulated by focusing its view) should
    // keep focused_thread since it was already seeded on workspace switch.
    panel_b.update_in(cx, |panel, window, cx| {
        if let Some(thread_view) = panel.active_conversation_view() {
            thread_view.read(cx).focus_handle(cx).focus(window, cx);
        }
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id_b2,
            "Focusing the agent panel thread should set focused_thread",
        );
        assert!(
            has_thread_entry(sidebar, &session_id_b2),
            "The focused thread should be present in the entries"
        );
    });
}

#[gpui::test]
async fn test_new_thread_button_works_after_adding_folder(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/project-a", cx).await;
    let fs = cx.update(|cx| <dyn fs::Fs>::global(cx));
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Start a thread and send a message so it has history.
    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);
    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    // Verify the thread appears in the sidebar.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project-a]",
            "  Hello *",
        ]
    );

    // The "New Thread" button should NOT be in "active/draft" state
    // because the panel has a thread with messages.
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { .. })),
            "Panel has a thread with messages, so active_entry should be Thread, got {:?}",
            sidebar.active_entry,
        );
    });

    // Now add a second folder to the workspace, changing the path_list.
    fs.as_fake()
        .insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project-b", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();

    // The workspace path_list is now [project-a / project-b]. The active
    // thread's metadata was re-saved with the new paths by the agent panel's
    // project subscription. The old [project-a] key is replaced by the new
    // key since no other workspace claims it.
    let entries = visible_entries_as_strings(&sidebar, cx);
    // After adding a worktree, the thread migrates to the new group key.
    // A reconciliation draft may appear during the transition.
    assert!(
        entries.contains(&"  Hello *".to_string()),
        "thread should still be present after adding folder: {entries:?}"
    );
    assert_eq!(entries[0], "v [project-a / project-b]");

    // The "New Thread" button must still be clickable (not stuck in
    // "active/draft" state). Verify that `active_thread_is_draft` is
    // false — the panel still has the old thread with messages.
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { .. })),
            "After adding a folder the panel still has a thread with messages, \
                 so active_entry should be Thread, got {:?}",
            sidebar.active_entry,
        );
    });

    // Actually click "New Thread" by calling create_new_thread and
    // verify a new draft is created.
    let workspace = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_new_thread(&workspace, window, cx);
    });
    cx.run_until_parked();

    // After creating a new thread, the panel should now be in draft
    // state (no messages on the new thread).
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_draft(
            sidebar,
            &workspace,
            "After creating a new thread active_entry should be Draft",
        );
    });
}

#[gpui::test]
async fn test_draft_title_updates_from_editor_text(cx: &mut TestAppContext) {
    // When the user types into a draft, the parked draft entry's title in
    // the sidebar should reflect the editor's text — both while the
    // draft's `ConversationView` is still loaded (source: live message
    // editor) and after it has been evicted (source: kvp draft prompt
    // store, the same path used when drafts are restored from disk).
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Open an ephemeral draft via a stub connection so the conversation
    // view reaches Connected synchronously and the panel's draft_thread
    // pointer is populated.
    let connection = StubAgentConnection::new();
    agent_ui::test_support::open_draft_with_connection(&panel, connection, cx);
    cx.run_until_parked();
    let draft_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());

    // Type into the (active) draft's message editor. The helper drains the
    // kvp-write debounce, so by the time it returns the prompt is on disk
    // — important for Phase 2 below, which exercises the kvp fallback.
    agent_ui::test_support::type_draft_prompt(&panel, "Fix the login bug", cx);

    // Park the draft by pressing Cmd-N while it has content.
    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    cx.run_until_parked();

    let draft_title = |sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _cx| {
            sidebar
                .contents
                .entries
                .iter()
                .find_map(|entry| match entry {
                    ListEntry::Thread(thread)
                        if thread.draft.is_some() && thread.metadata.thread_id == draft_id =>
                    {
                        Some(thread.metadata.display_title())
                    }
                    _ => None,
                })
                .expect("parked draft entry should be present")
        })
    };

    // Phase 1: ConversationView is still loaded in `retained_threads`;
    // the title comes from its live message editor.
    assert_eq!(
        draft_title(&sidebar, cx).as_ref(),
        "Fix the login bug",
        "parked draft title should match its editor text while loaded"
    );
    panel.read_with(cx, |panel, _cx| {
        assert!(
            panel.retained_threads().contains_key(&draft_id),
            "draft should be in retained_threads while loaded"
        );
    });

    // Phase 2: drop the draft's ConversationView from memory, mirroring
    // the state the sidebar sees immediately after a process restart
    // — the metadata row and the kvp draft prompt are on disk, but no
    // ConversationView has been rehydrated yet.
    let unloaded = panel.update(cx, |panel, _cx| panel.test_unload_retained_thread(draft_id));
    assert!(unloaded, "draft should have been present before unload");
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert_eq!(
        draft_title(&sidebar, cx).as_ref(),
        "Fix the login bug",
        "parked draft title should still come from the kvp draft prompt store \
         even after its ConversationView is unloaded"
    );
}

#[gpui::test]
async fn test_thread_switcher_includes_parked_draft(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-existing")),
        Some("Existing Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );

    let connection = StubAgentConnection::new();
    agent_ui::test_support::open_draft_with_connection(&panel, connection, cx);
    cx.run_until_parked();
    let draft_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());
    agent_ui::test_support::type_draft_prompt(&panel, "Fix the login bug", cx);

    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(sidebar.contents.entries.iter().any(|entry| {
            matches!(entry, ListEntry::Thread(thread) if thread.metadata.thread_id == draft_id)
        }));
    });

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        assert!(switcher.read(cx).entries().iter().any(|entry| {
            matches!(entry.thread_id(), Some(thread_id) if thread_id == draft_id)
        }));
    });
}

#[gpui::test]
async fn test_plus_button_keeps_empty_draft(cx: &mut TestAppContext) {
    // Clicking `+` when an empty draft is active creates a new thread, and
    // the empty draft it leaves behind stays in the sidebar.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Open an initial draft against a stub so it connects synchronously.
    let connection = StubAgentConnection::new();
    agent_ui::test_support::open_draft_with_connection(&panel, connection, cx);
    cx.run_until_parked();

    let first_id = panel.read_with(cx, |panel, cx| {
        panel
            .active_thread_id(cx)
            .expect("draft should be active after open_draft_with_connection")
    });

    // Cmd-N with an empty draft should park it and create another.
    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    cx.run_until_parked();

    let second_id = panel.read_with(cx, |panel, cx| {
        panel
            .active_thread_id(cx)
            .expect("draft should be active after Cmd-N")
    });
    assert_ne!(
        first_id, second_id,
        "Cmd-N should create a new thread rather than reuse the empty draft"
    );
    let draft_rows: Vec<_> = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::Thread(t) if t.draft.is_some() => Some(t.clone()),
                _ => None,
            })
            .collect()
    });
    assert_eq!(
        draft_rows.len(),
        2,
        "the empty draft should stay in the sidebar after switching away from it"
    );
    assert!(
        draft_rows
            .iter()
            .any(|row| row.metadata.thread_id == first_id && row.draft == Some(DraftKind::Empty))
    );
    assert!(
        draft_rows
            .iter()
            .any(|row| row.metadata.thread_id == second_id)
    );
}

#[gpui::test]
async fn test_plus_button_parks_nonempty_draft(cx: &mut TestAppContext) {
    // Clicking `+` while the current draft has content should park the
    // current draft (surface it as a sidebar row) and create a new empty
    // draft as active.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Open a draft via a stub so the ConversationView reaches Connected and
    // we can type into its editor.
    let connection = StubAgentConnection::new();
    agent_ui::test_support::open_draft_with_connection(&panel, connection, cx);
    cx.run_until_parked();
    let first_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());
    let thread_view = panel.read_with(cx, |panel, cx| panel.active_thread_view(cx).unwrap());
    let editor = thread_view.read_with(cx, |view, _| view.message_editor.clone());
    editor.update_in(cx, |editor, window, cx| {
        editor.set_text("something the user typed", window, cx);
    });
    cx.run_until_parked();

    // Cmd-N parks the first draft and creates a new empty draft.
    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    cx.run_until_parked();

    let second_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());
    assert_ne!(
        first_id, second_id,
        "non-empty draft should be parked and a fresh draft activated"
    );

    // Both drafts now appear as sidebar rows: the parked one with its
    // editor-derived title (real user state), and the newly-created empty
    // draft as a "New {agent} Thread" placeholder. The placeholder mirrors
    // the panel's current view; the parked row preserves typed content.
    let draft_rows: Vec<_> = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::Thread(t) if t.draft.is_some() => Some(t.clone()),
                _ => None,
            })
            .collect()
    });
    assert_eq!(
        draft_rows.len(),
        2,
        "expected two draft rows (parked + new empty placeholder), got {:?}",
        draft_rows
            .iter()
            .map(|t| t.metadata.display_title())
            .collect::<Vec<_>>()
    );
    let parked = draft_rows
        .iter()
        .find(|t| t.metadata.thread_id == first_id)
        .expect("parked draft should be present");
    assert_eq!(
        parked.draft,
        Some(DraftKind::WithContent),
        "the parked draft has user content and is not an empty placeholder"
    );
    let new_empty = draft_rows
        .iter()
        .find(|t| t.metadata.thread_id == second_id)
        .expect("new empty draft should be present");
    assert_eq!(
        new_empty.draft,
        Some(DraftKind::Empty),
        "the freshly-created draft should be an empty placeholder"
    );
    assert_eq!(
        parked.metadata.display_title().as_ref(),
        "something the user typed"
    );
}

#[gpui::test]
async fn test_remove_draft_deletes_metadata_row(cx: &mut TestAppContext) {
    // The close-draft button deletes the metadata row and the kvp draft prompt,
    // and the draft disappears from the sidebar.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Open a draft with content, park it by pressing Cmd-N.
    let connection = StubAgentConnection::new();
    agent_ui::test_support::open_draft_with_connection(&panel, connection, cx);
    cx.run_until_parked();
    let draft_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());
    agent_ui::test_support::type_draft_prompt(&panel, "will be discarded", cx);
    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    cx.run_until_parked();

    // The parked draft is visible.
    let draft_index = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|e| matches!(e, ListEntry::Thread(t) if t.metadata.thread_id == draft_id))
            .expect("parked draft should be visible before removal")
    });

    // Select the parked draft and dispatch the action a real user would
    // (Shift-Backspace, bound to `ArchiveSelectedThread`). The handler
    // routes to `remove_draft` for parked drafts.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.selection = Some(draft_index);
        sidebar.archive_selected_thread(&agent_ui::ArchiveSelectedThread, window, cx);
    });
    cx.run_until_parked();

    // Metadata row and persisted draft prompt should both be gone.
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        assert!(
            store.entry(draft_id).is_none(),
            "removed draft metadata should be deleted"
        );
        assert!(
            agent_ui::draft_prompt_store::read(draft_id, cx).is_none(),
            "removed draft's kvp prompt should also be deleted"
        );
    });
    // And the row should be gone from the sidebar.
    let still_visible = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .any(|e| matches!(e, ListEntry::Thread(t) if t.metadata.thread_id == draft_id))
    });
    assert!(
        !still_visible,
        "removed draft should no longer appear in the sidebar"
    );
}

#[gpui::test]
async fn test_remove_only_draft_does_not_create_a_replacement(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let connection = StubAgentConnection::new();
    agent_ui::test_support::open_draft_with_connection(&panel, connection, cx);
    cx.run_until_parked();
    let draft_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());
    let thread_count = |cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _| {
            sidebar
                .contents
                .entries
                .iter()
                .filter(|entry| matches!(entry, ListEntry::Thread(_)))
                .count()
        })
    };
    assert_eq!(thread_count(cx), 1);
    let panel_is_open = |cx: &mut gpui::VisualTestContext| {
        multi_workspace.read_with(cx, |multi_workspace, cx| {
            let workspace = multi_workspace.workspace().read(cx);
            workspace.all_docks().iter().any(|dock| {
                let dock = dock.read(cx);
                dock.is_open()
                    && dock
                        .visible_panel()
                        .is_some_and(|visible| visible.panel_id() == panel.entity_id())
            })
        })
    };
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.workspace().update(cx, |workspace, cx| {
            workspace.focus_panel::<AgentPanel>(window, cx);
        });
    });
    cx.run_until_parked();
    assert!(panel_is_open(cx));
    assert_eq!(thread_count(cx), 1);

    let workspace = multi_workspace.read_with(cx, |multi_workspace, _| {
        ThreadEntryWorkspace::Open(multi_workspace.workspace().clone())
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.remove_draft(draft_id, &workspace, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(thread_count(cx), 0);
    assert!(
        !panel_is_open(cx),
        "an agent panel with nothing to show is closed"
    );
}

#[gpui::test]
async fn test_sending_message_from_draft_promotes_in_place(cx: &mut TestAppContext) {
    // Sending a message from a draft should keep the same ThreadId, set the
    // session_id on its metadata row, and clear the `draft_thread` pointer.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (_sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("ok".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    let draft_id = panel.read_with(cx, |panel, cx| panel.active_thread_id(cx).unwrap());

    // Before sending: draft metadata row exists with session_id = None.
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        let entry = store.entry(draft_id).expect("draft metadata row");
        assert!(entry.is_draft(), "expected draft row before sending");
    });

    send_message(&panel, cx);
    cx.run_until_parked();

    // After sending: draft_thread is cleared, metadata row has a session_id.
    panel.read_with(cx, |panel, cx| {
        assert!(
            !panel.active_thread_is_draft(cx),
            "should no longer be a draft after send"
        );
        assert!(
            panel.ephemeral_draft_thread_id(cx).is_none(),
            "ephemeral draft pointer should be cleared after promotion"
        );
        assert_eq!(
            panel.active_thread_id(cx),
            Some(draft_id),
            "ThreadId stays the same across promotion"
        );
    });
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        let entry = store.entry(draft_id).expect("promoted metadata row");
        assert!(
            !entry.is_draft(),
            "promoted thread should have a session_id"
        );
    });
}

#[gpui::test]
async fn test_cmd_n_shows_new_thread_entry(cx: &mut TestAppContext) {
    // When the user presses Cmd-N (NewThread action) while viewing a
    // non-empty thread, the panel should switch to the draft thread and
    // the sidebar should surface a "New {agent} Thread" placeholder row
    // that mirrors the active empty draft.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Create a non-empty thread (has messages).
    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);

    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Hello *",
        ]
    );

    // Simulate cmd-n
    let workspace = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());
    panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_panel::<AgentPanel>(window, cx);
    });
    cx.run_until_parked();

    // After Cmd-N the sidebar surfaces the active empty draft as a
    // placeholder row above the real thread.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  New stub Thread", "  Hello *"],
        "After Cmd-N the sidebar should show a placeholder row for the active empty draft"
    );

    // The panel should be on the draft and active_entry should track it.
    panel.read_with(cx, |panel, cx| {
        assert!(
            panel.active_thread_is_draft(cx),
            "panel should be showing the draft after Cmd-N",
        );
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_draft(
            sidebar,
            &workspace,
            "active_entry should be Draft after Cmd-N",
        );
    });
}

#[gpui::test]
async fn test_cmd_n_shows_new_thread_entry_in_absorbed_worktree(cx: &mut TestAppContext) {
    // When the active workspace is an absorbed git worktree, cmd-n
    // should activate the draft thread in the panel and the sidebar
    // should surface a placeholder row for the active empty draft.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());

    // Main repo with a linked worktree.
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    // Worktree checkout pointing back to the main repo.
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    let worktree_panel = add_agent_panel(&worktree_workspace, cx);

    // Switch to the worktree workspace.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().nth(1).unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });

    // Create a non-empty thread in the worktree workspace.
    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&worktree_panel, connection, cx);
    send_message(&worktree_panel, cx);

    let session_id = active_session_id(&worktree_panel, cx);
    save_test_thread_metadata(&session_id, &worktree_project, cx).await;
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project]",
            "  Hello {wt-feature-a} *",
        ]
    );

    // Simulate Cmd-N in the worktree workspace.
    worktree_panel.update_in(cx, |panel, window, cx| {
        panel.new_thread(&NewThread, window, cx);
    });
    worktree_workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_panel::<AgentPanel>(window, cx);
    });
    cx.run_until_parked();

    // After Cmd-N the sidebar surfaces the active empty draft as a
    // placeholder row. Its worktree chip identifies which workspace it
    // belongs to (the linked worktree).
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project]",
            "  New stub Thread {wt-feature-a}",
            "  Hello {wt-feature-a} *",
        ],
        "After Cmd-N the sidebar should show a placeholder row for the active empty draft"
    );

    // The panel should be on the draft and active_entry should track it.
    worktree_panel.read_with(cx, |panel, cx| {
        assert!(
            panel.active_thread_is_draft(cx),
            "panel should be showing the draft after Cmd-N",
        );
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_draft(
            sidebar,
            &worktree_workspace,
            "active_entry should be Draft after Cmd-N",
        );
    });
}

async fn init_test_project_with_git(
    worktree_path: &str,
    cx: &mut TestAppContext,
) -> (Entity<project::Project>, Arc<dyn fs::Fs>) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        worktree_path,
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = project::Project::test(fs.clone(), [worktree_path.as_ref()], cx).await;
    (project, fs)
}

#[gpui::test]
async fn test_search_matches_worktree_name(cx: &mut TestAppContext) {
    let (project, fs) = init_test_project_with_git("/project", cx).await;

    fs.as_fake()
        .add_linked_worktree_for_repo(
            Path::new("/project/.git"),
            false,
            git::repository::Worktree {
                path: std::path::PathBuf::from("/wt/rosewood"),
                ref_name: Some("refs/heads/rosewood".into()),
                sha: "abc".into(),
                is_main: false,
                is_bare: false,
                is_prunable: false,
            },
        )
        .await;

    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let worktree_project = project::Project::test(fs.clone(), ["/wt/rosewood".as_ref()], cx).await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_named_thread_metadata("main-t", "Unrelated Thread", &project, cx).await;
    save_named_thread_metadata("wt-t", "Fix Bug", &worktree_project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Search for "rosewood" — should match the worktree name, not the title.
    type_in_search(&sidebar, "rosewood", cx);

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project]",
            "  Fix Bug {rosewood}  <== selected",
        ],
    );
}

#[gpui::test]
async fn test_git_worktree_added_live_updates_sidebar(cx: &mut TestAppContext) {
    let (project, fs) = init_test_project_with_git("/project", cx).await;

    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let worktree_project = project::Project::test(fs.clone(), ["/wt/rosewood".as_ref()], cx).await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Save a thread against a worktree path with the correct main
    // worktree association (as if the git state had been resolved).
    save_thread_metadata_with_main_paths(
        "wt-thread",
        "Worktree Thread",
        PathList::new(&[PathBuf::from("/wt/rosewood")]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Thread is visible because its main_worktree_paths match the group.
    // The chip name is derived from the path even before git discovery.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [project]", "  Worktree Thread {rosewood}"]
    );

    // Now add the worktree to the git state and trigger a rescan.
    fs.as_fake()
        .add_linked_worktree_for_repo(
            Path::new("/project/.git"),
            true,
            git::repository::Worktree {
                path: std::path::PathBuf::from("/wt/rosewood"),
                ref_name: Some("refs/heads/rosewood".into()),
                sha: "abc".into(),
                is_main: false,
                is_bare: false,
                is_prunable: false,
            },
        )
        .await;

    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project]",
            "  Worktree Thread {rosewood}",
        ]
    );
}

#[gpui::test]
async fn test_two_worktree_workspaces_absorbed_when_main_added(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    // Create the main repo directory (not opened as a workspace yet).
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
            },
            "src": {},
        }),
    )
    .await;

    // Two worktree checkouts whose .git files point back to the main repo.
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-b"),
            ref_name: Some("refs/heads/feature-b".into()),
            sha: "bbb".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/wt-feature-b".as_ref()], cx).await;

    project_a.update(cx, |p, cx| p.git_scans_complete(cx)).await;
    project_b.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    // Open both worktrees as workspaces — no main repo yet.
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx);
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-a")),
        Some("Thread A".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project_a,
        cx,
    );
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-b")),
        Some("Thread B".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 1).unwrap(),
        None,
        None,
        &project_b,
        cx,
    );

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Each worktree has its own header, and the main worktree has one even without threads.
    assert_eq!(
        visible_entries_with_worktree_headers(&sidebar, cx),
        vec![
            //
            "v [project]",
            "  ~ project",
            "  ~ wt-feature-a",
            "  Thread A {wt-feature-a}",
            "  ~ wt-feature-b",
            "  Thread B {wt-feature-b}",
        ]
    );

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(main_project.clone(), window, cx);
    });
    cx.run_until_parked();

    // Both worktree workspaces should now be absorbed under the main
    // repo header, with worktree chips.
    assert_eq!(
        visible_entries_with_worktree_headers(&sidebar, cx),
        vec![
            //
            "v [project]",
            "  ~ project",
            "  ~ wt-feature-a",
            "  Thread A {wt-feature-a}",
            "  ~ wt-feature-b",
            "  Thread B {wt-feature-b}",
        ]
    );

    let worktree_paths = worktree_header_ids(&sidebar, cx);
    let [path_main, path_a, path_b] = worktree_paths.as_slice() else {
        panic!("expected three worktree headers, found {worktree_paths:?}");
    };

    sidebar.update(cx, |sidebar, cx| sidebar.toggle_worktree_group(path_b, cx));
    cx.run_until_parked();
    assert_eq!(
        visible_entries_with_worktree_headers(&sidebar, cx),
        vec![
            //
            "v [project]",
            "  ~ project",
            "  ~ wt-feature-a",
            "  Thread A {wt-feature-a}",
            "  ~> wt-feature-b",
        ]
    );

    sidebar.update(cx, |sidebar, cx| sidebar.toggle_worktree_group(path_b, cx));
    sidebar.update(cx, |sidebar, cx| {
        sidebar.move_worktree_group(path_b, path_a, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        visible_entries_with_worktree_headers(&sidebar, cx),
        vec![
            //
            "v [project]",
            "  ~ project",
            "  ~ wt-feature-b",
            "  Thread B {wt-feature-b}",
            "  ~ wt-feature-a",
            "  Thread A {wt-feature-a}",
        ]
    );

    // The drop line sits on the outer edge of the target group, not on its header.
    sidebar.update(cx, |sidebar, _| {
        let lines = |sidebar: &Sidebar| {
            (0..sidebar.contents.entries.len())
                .map(|ix| sidebar.drop_line_for_entry(ix))
                .collect::<Vec<_>>()
        };
        sidebar.worktree_drop_target = Some((path_a.clone(), true));
        assert_eq!(lines(sidebar), [None, None, None, None, None, Some(true)]);
        sidebar.worktree_drop_target = Some((path_b.clone(), false));
        assert_eq!(lines(sidebar), [None, None, Some(false), None, None, None]);
        sidebar.worktree_drop_target = None;
        let project = sidebar.group_key_for_entry(1).expect("project header");
        sidebar.project_drop_target = Some((project.clone(), true));
        assert_eq!(lines(sidebar), [None, None, None, None, None, Some(true)]);
        sidebar.project_drop_target = Some((project, false));
        assert_eq!(lines(sidebar), [Some(false), None, None, None, None, None]);
        sidebar.project_drop_target = None;
    });

    // Acting on a project drops saved state for its groups that no longer exist.
    sidebar.update(cx, |sidebar, cx| {
        let stale = SharedString::from(format!("{}\n/gone", worktree_group_project(path_a)));
        sidebar.worktree_group_order.push(stale.clone());
        sidebar.collapsed_worktree_groups.insert(stale.clone());
        sidebar.toggle_worktree_group(path_a, cx);
        sidebar.toggle_worktree_group(path_a, cx);
        assert!(!sidebar.worktree_group_order.contains(&stale));
        assert!(sidebar.collapsed_worktree_groups.is_empty());
    });

    // A group without a saved position is shown first and must still be movable.
    sidebar.update(cx, |sidebar, cx| {
        sidebar.worktree_group_order = vec![path_a.clone()];
        sidebar.update_entries(cx);
        sidebar.move_worktree_group(path_b, path_a, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        visible_entries_with_worktree_headers(&sidebar, cx),
        vec![
            //
            "v [project]",
            "  ~ project",
            "  ~ wt-feature-a",
            "  Thread A {wt-feature-a}",
            "  ~ wt-feature-b",
            "  Thread B {wt-feature-b}",
        ]
    );

    let serialized = sidebar.read_with(cx, |sidebar, cx| sidebar.serialized_state(cx));
    let serialized: SerializedSidebar =
        serde_json::from_str(&serialized.expect("sidebar state should serialize"))
            .expect("sidebar state should deserialize");
    assert_eq!(
        serialized.worktree_group_order,
        vec![
            path_main.to_string(),
            path_a.to_string(),
            path_b.to_string()
        ]
    );
}

#[gpui::test]
async fn test_threadless_workspace_shows_new_thread_with_worktree_chip(cx: &mut TestAppContext) {
    // When a group has two workspaces — one with threads and one
    // without — the threadless workspace should appear as a
    // "New Thread" button with its worktree chip.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    // Main repo with two linked worktrees.
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-b"),
            ref_name: Some("refs/heads/feature-b".into()),
            sha: "bbb".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    // Workspace A: worktree feature-a (has threads).
    let project_a = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    project_a.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    // Workspace B: worktree feature-b (no threads).
    let project_b = project::Project::test(fs.clone(), ["/wt-feature-b".as_ref()], cx).await;
    project_b.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx);
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Only save a thread for workspace A.
    save_named_thread_metadata("thread-a", "Thread A", &project_a, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Workspace A's thread appears normally. Workspace B (threadless)
    // appears as a "New Thread" button with its worktree chip.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [project]", "  Thread A {wt-feature-a}",]
    );
}

#[gpui::test]
async fn test_multi_worktree_thread_shows_multiple_chips(cx: &mut TestAppContext) {
    // A thread created in a workspace with roots from different git
    // worktrees should show a chip for each distinct worktree name.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    // Two main repos.
    fs.insert_tree(
        "/project_a",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/project_b",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    // Worktree checkouts.
    for repo in &["project_a", "project_b"] {
        let git_path = format!("/{repo}/.git");
        for branch in &["olivetti", "selectric"] {
            fs.add_linked_worktree_for_repo(
                Path::new(&git_path),
                false,
                git::repository::Worktree {
                    path: std::path::PathBuf::from(format!("/worktrees/{repo}/{branch}/{repo}")),
                    ref_name: Some(format!("refs/heads/{branch}").into()),
                    sha: "aaa".into(),
                    is_main: false,
                    is_bare: false,
                    is_prunable: false,
                },
            )
            .await;
        }
    }

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    // Open a workspace with the worktree checkout paths as roots
    // (this is the workspace the thread was created in).
    let project = project::Project::test(
        fs.clone(),
        [
            "/worktrees/project_a/olivetti/project_a".as_ref(),
            "/worktrees/project_b/selectric/project_b".as_ref(),
        ],
        cx,
    )
    .await;
    project.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Save a thread under the same paths as the workspace roots.
    save_named_thread_metadata("wt-thread", "Cross Worktree Thread", &project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Should show two distinct worktree chips.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project_a / project_b]",
            "  Cross Worktree Thread {project_a:olivetti}, {project_b:selectric}",
        ]
    );
}

#[gpui::test]
async fn test_same_named_worktree_chips_are_deduplicated(cx: &mut TestAppContext) {
    // When a thread's roots span multiple repos but share the same
    // worktree name (e.g. both in "olivetti"), only one chip should
    // appear.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project_a",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/project_b",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    for repo in &["project_a", "project_b"] {
        let git_path = format!("/{repo}/.git");
        fs.add_linked_worktree_for_repo(
            Path::new(&git_path),
            false,
            git::repository::Worktree {
                path: std::path::PathBuf::from(format!("/worktrees/{repo}/olivetti/{repo}")),
                ref_name: Some("refs/heads/olivetti".into()),
                sha: "aaa".into(),
                is_main: false,
                is_bare: false,
                is_prunable: false,
            },
        )
        .await;
    }

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project = project::Project::test(
        fs.clone(),
        [
            "/worktrees/project_a/olivetti/project_a".as_ref(),
            "/worktrees/project_b/olivetti/project_b".as_ref(),
        ],
        cx,
    )
    .await;
    project.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Thread with roots in both repos' "olivetti" worktrees.
    save_named_thread_metadata("wt-thread", "Same Branch Thread", &project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Both worktree paths have the name "olivetti", so only one chip.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project_a / project_b]",
            "  Same Branch Thread {olivetti}",
        ]
    );
}

#[gpui::test]
async fn test_absorbed_worktree_running_thread_shows_live_status(cx: &mut TestAppContext) {
    // When a worktree workspace is absorbed under the main repo, a
    // running thread in the worktree's agent panel should still show
    // live status (spinner + "(running)") in the sidebar.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());

    // Main repo with a linked worktree.
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    // Worktree checkout pointing back to the main repo.
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    // Create the MultiWorkspace with both projects.
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    // Add an agent panel to the worktree workspace so we can run a
    // thread inside it.
    let worktree_panel = add_agent_panel(&worktree_workspace, cx);

    // Switch back to the main workspace before setting up the sidebar.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().next().unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });

    // Start a thread in the worktree workspace's panel and keep it
    // generating (don't resolve it).
    let connection = StubAgentConnection::new();
    open_thread_with_connection(&worktree_panel, connection.clone(), cx);
    send_message(&worktree_panel, cx);

    let session_id = active_session_id(&worktree_panel, cx);

    // Save metadata so the sidebar knows about this thread.
    save_test_thread_metadata(&session_id, &worktree_project, cx).await;

    // Keep the thread generating by sending a chunk without ending
    // the turn.
    cx.update(|_, cx| {
        connection.send_update(
            session_id.clone(),
            acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new(
                "working...".into(),
            )),
            cx,
        );
    });
    cx.run_until_parked();

    // The worktree thread should be absorbed under the main project
    // and show live running status.
    let entries = visible_entries_as_strings(&sidebar, cx);
    assert_eq!(
        entries,
        vec!["v [project]", "  Hello {wt-feature-a} * (running)",]
    );
}

#[gpui::test]
async fn test_absorbed_worktree_completion_triggers_notification(cx: &mut TestAppContext) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    let worktree_panel = add_agent_panel(&worktree_workspace, cx);

    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().next().unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });

    let connection = StubAgentConnection::new();
    open_thread_with_connection(&worktree_panel, connection.clone(), cx);
    send_message(&worktree_panel, cx);

    let session_id = active_session_id(&worktree_panel, cx);
    save_test_thread_metadata(&session_id, &worktree_project, cx).await;

    cx.update(|_, cx| {
        connection.send_update(
            session_id.clone(),
            acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new(
                "working...".into(),
            )),
            cx,
        );
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [project]", "  Hello {wt-feature-a} * (running)",]
    );

    connection.end_turn(session_id, acp_v1::StopReason::EndTurn);
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [project]", "  Hello {wt-feature-a} * (!)",]
    );
}

#[gpui::test]
async fn test_clicking_worktree_thread_opens_workspace_when_none_exists(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    // Only open the main repo — no workspace for the worktree.
    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Save a thread for the worktree path (no workspace for it).
    save_named_thread_metadata("thread-wt", "WT Thread", &worktree_project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Thread should appear under the main repo with a worktree chip.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project]",
            "  WT Thread {wt-feature-a}",
        ],
    );

    // Only 1 workspace should exist.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
    );

    // Focus the sidebar and select the worktree thread.
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(3); // 0 is the project header, 1 and 2 the worktree headers
    });

    // Confirm to open the worktree thread.
    cx.dispatch_action(Confirm);
    cx.run_until_parked();

    // A new workspace should have been created for the worktree path.
    let new_workspace = multi_workspace.read_with(cx, |mw, _| {
        assert_eq!(
            mw.workspaces().count(),
            2,
            "confirming a worktree thread without a workspace should open one",
        );
        mw.workspaces().nth(1).unwrap().clone()
    });

    let new_path_list =
        new_workspace.read_with(cx, |_, cx| workspace_path_list(&new_workspace, cx));
    assert_eq!(
        new_path_list,
        PathList::new(&[std::path::PathBuf::from("/wt-feature-a")]),
        "the new workspace should have been opened for the worktree path",
    );
}

#[gpui::test]
async fn test_clicking_worktree_thread_does_not_briefly_render_as_separate_project(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_named_thread_metadata("thread-wt", "WT Thread", &worktree_project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project]",
            "  WT Thread {wt-feature-a}",
        ],
    );

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(3); // 0 is the project header, 1 and 2 the worktree headers
    });

    let assert_sidebar_state = |sidebar: &mut Sidebar, _cx: &mut Context<Sidebar>| {
        let mut project_headers = sidebar.contents.entries.iter().filter_map(|entry| {
            if let ListEntry::ProjectHeader { label, .. } = entry {
                Some(label.as_ref())
            } else {
                None
            }
        });

        let Some(project_header) = project_headers.next() else {
            panic!("expected exactly one sidebar project header named `project`, found none");
        };
        assert_eq!(
            project_header, "project",
            "expected the only sidebar project header to be `project`"
        );
        if let Some(unexpected_header) = project_headers.next() {
            panic!(
                "expected exactly one sidebar project header named `project`, found extra header `{unexpected_header}`"
            );
        }

        let mut saw_expected_thread = false;
        for entry in &sidebar.contents.entries {
            match entry {
                ListEntry::ProjectHeader { label, .. } => {
                    assert_eq!(
                        label.as_ref(),
                        "project",
                        "expected the only sidebar project header to be `project`"
                    );
                }
                ListEntry::Thread(thread)
                    if thread.metadata.title.as_ref().map(|t| t.as_ref()) == Some("WT Thread")
                        && thread
                            .worktrees
                            .first()
                            .and_then(|wt| wt.worktree_name.as_ref().map(|n| n.as_ref()))
                            == Some("wt-feature-a") =>
                {
                    saw_expected_thread = true;
                }
                ListEntry::Thread(thread) => {
                    let title = thread.metadata.display_title();
                    let worktree_name = thread
                        .worktrees
                        .first()
                        .and_then(|wt| wt.worktree_name.as_ref().map(|n| n.as_ref()))
                        .unwrap_or("<none>");
                    panic!(
                        "unexpected sidebar thread while opening linked worktree thread: title=`{}`, worktree=`{}`",
                        title, worktree_name
                    );
                }
                ListEntry::Terminal(terminal) => {
                    panic!(
                        "unexpected sidebar terminal while opening linked worktree thread: title=`{}`",
                        terminal.metadata.title
                    );
                }
                ListEntry::WorktreeHeader { .. } => {}
            }
        }

        assert!(
            saw_expected_thread,
            "expected the sidebar to keep showing `WT Thread {{wt-feature-a}}` under `project`"
        );
    };

    sidebar
        .update(cx, |_, cx| cx.observe_self(assert_sidebar_state))
        .detach();

    let window = cx.windows()[0];
    cx.update_window(window, |_, window, cx| {
        window.dispatch_action(Confirm.boxed_clone(), cx);
    })
    .unwrap();

    cx.run_until_parked();

    sidebar.update(cx, assert_sidebar_state);
}

#[gpui::test]
async fn test_clicking_absorbed_worktree_thread_activates_worktree_workspace(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    // Activate the main workspace before setting up the sidebar.
    let main_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().next().unwrap().clone();
        mw.activate(workspace.clone(), None, window, cx);
        workspace
    });

    save_named_thread_metadata("thread-main", "Main Thread", &main_project, cx).await;
    save_named_thread_metadata("thread-wt", "WT Thread", &worktree_project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // The worktree workspace should be absorbed under the main repo.
    let entries = visible_entries_as_strings(&sidebar, cx);
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0], "v [project]");
    assert!(entries.contains(&"  Main Thread".to_string()));
    assert!(entries.contains(&"  WT Thread {wt-feature-a}".to_string()));

    let wt_thread_index = visible_entries_with_worktree_headers(&sidebar, cx)
        .iter()
        .position(|e| e.contains("WT Thread"))
        .expect("should find the worktree thread entry");

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        main_workspace,
        "main workspace should be active initially"
    );

    // Focus the sidebar and select the absorbed worktree thread.
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(wt_thread_index);
    });

    // Confirm to activate the worktree thread.
    cx.dispatch_action(Confirm);
    cx.run_until_parked();

    // The worktree workspace should now be active, not the main one.
    let active_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    assert_eq!(
        active_workspace, worktree_workspace,
        "clicking an absorbed worktree thread should activate the worktree workspace"
    );
}

// Reproduces the core of the user-reported bug: a thread belonging to
// a multi-root workspace that mixes a standalone project and a linked
// git worktree can become invisible in the sidebar when its stored
// `main_worktree_paths` don't match the workspace's project group
// key. The metadata still exists and Thread History still shows it,
// but the sidebar rebuild's lookups all miss.
//
// Real-world setup: a single multi-root workspace whose roots are
// `[/cloud, /worktrees/zed/wt_a/zed]`, where:
//   - `/cloud` is a standalone git repo (main == folder).
//   - `/worktrees/zed/wt_a/zed` is a linked worktree of `/zed`.
//
// Once git scans complete the project group key is
// `[/cloud, /zed]` — the main paths of the two roots. A thread
// created in this workspace is written with
// `main=[/cloud, /zed], folder=[/cloud, /worktrees/zed/wt_a/zed]`
// and the sidebar finds it via `entries_for_main_worktree_path`.
//
// If some other code path (stale data on reload, a path-less archive
// restored via the project picker, a legacy write …) persists the
// thread with `main == folder` instead, the stored
// `main_worktree_paths` is
// `[/cloud, /worktrees/zed/wt_a/zed]` ≠ `[/cloud, /zed]`. The three
// lookups in `rebuild_contents` all miss:
//
//   1. `entries_for_main_worktree_path([/cloud, /zed])` — the
//      thread's stored main doesn't equal the group key.
//   2. `entries_for_path([/cloud, /zed])` — the thread's folder paths
//      don't equal the group key either.
//   3. The linked-worktree fallback iterates the group's workspaces'
//      `linked_worktrees()` snapshots. Those yield *sibling* linked
//      worktrees of the repo, not the workspace's own roots, so the
//      thread's folder `/worktrees/zed/wt_a/zed` doesn't match.
//
// The row falls out of the sidebar entirely — matching the user's
// symptom of a thread visible in the agent panel but missing from
// the sidebar. It only reappears once something re-writes the
// thread's metadata in the good shape (e.g. `handle_conversation_event`
// firing after the user sends a message).
//
// We directly persist the bad shape via `store.save(...)` rather
// than trying to reproduce the original writer. The bug is
// ultimately about the sidebar's tolerance for any stale row whose
// folder paths correspond to an open workspace's roots, regardless
// of how that row came to be in the store.
#[gpui::test]
async fn test_sidebar_keeps_multi_root_thread_with_stale_main_paths(cx: &mut TestAppContext) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        set_max_idle_retained_threads(1, cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());

    // Standalone repo — one of the workspace's two roots, main
    // worktree of its own .git.
    fs.insert_tree(
        "/cloud",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    // Separate /zed repo whose linked worktree will form the second
    // workspace root. /zed itself is NOT opened as a workspace root.
    fs.insert_tree(
        "/zed",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/zed/wt_a/zed",
        serde_json::json!({
            ".git": "gitdir: /zed/.git/worktrees/wt_a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/zed/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/worktrees/zed/wt_a/zed"),
            ref_name: Some("refs/heads/wt_a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    // Single multi-root project with both /cloud and the linked
    // worktree of /zed.
    let project = project::Project::test(
        fs.clone(),
        ["/cloud".as_ref(), "/worktrees/zed/wt_a/zed".as_ref()],
        cx,
    )
    .await;
    project.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());
    let _panel = add_agent_panel(&workspace, cx);
    cx.run_until_parked();

    // Sanity-check the shapes the rest of the test depends on.
    let group_key = workspace.read_with(cx, |ws, cx| ws.project_group_key(cx));
    let expected_main_paths = PathList::new(&[PathBuf::from("/cloud"), PathBuf::from("/zed")]);
    assert_eq!(
        group_key.path_list(),
        &expected_main_paths,
        "expected the multi-root workspace's project group key to normalize to \
         [/cloud, /zed] (main of the standalone repo + main of the linked worktree)"
    );

    let folder_paths = PathList::new(&[
        PathBuf::from("/cloud"),
        PathBuf::from("/worktrees/zed/wt_a/zed"),
    ]);
    let workspace_root_paths = workspace.read_with(cx, |ws, cx| PathList::new(&ws.root_paths(cx)));
    assert_eq!(
        workspace_root_paths, folder_paths,
        "expected the workspace's root paths to equal [/cloud, /worktrees/zed/wt_a/zed]"
    );

    let session_id = acp::SessionId::new(Arc::from("multi-root-stale-paths"));
    let thread_id = ThreadId::new();

    // Persist the thread in the "bad" shape that the bug manifests as:
    // main == folder for every root. Any stale row where
    // `main_worktree_paths` no longer equals the group key produces
    // the same user-visible symptom; this is the concrete shape
    // produced by `WorktreePaths::from_folder_paths` on the workspace
    // roots.
    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(
                ThreadMetadata {
                    thread_id,
                    session_id: Some(session_id.clone()),
                    agent_id: agent::ZED_AGENT_ID.clone(),
                    title: Some("Stale Multi-Root Thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: None,
                    interacted_at: None,
                    worktree_paths: WorktreePaths::from_folder_paths(&folder_paths),
                    archived: false,
                    remote_connection: None,
                },
                cx,
            )
        });
    });
    cx.run_until_parked();

    let entries = visible_entries_as_strings(&sidebar, cx);
    let visible = sidebar.read_with(cx, |sidebar, _cx| has_thread_entry(sidebar, &session_id));

    // If this assert fails, we've reproduced the bug: the sidebar's
    // rebuild queries can't locate the thread under the current
    // project group, even though the metadata is intact and the
    // thread's folder paths exactly equal the open workspace's roots.
    assert!(
        visible,
        "thread disappeared from the sidebar when its main_worktree_paths \
         ({folder_paths:?}) diverged from the project group key ({expected_main_paths:?}); \
         sidebar entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_activate_archived_thread_with_saved_paths_activates_matching_workspace(
    cx: &mut TestAppContext,
) {
    // Thread has saved metadata in ThreadStore. A matching workspace is
    // already open. Expected: activates the matching workspace.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let workspace_a =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());

    // Save a thread with path_list pointing to project-b.
    let session_id = acp::SessionId::new(Arc::from("archived-1"));
    save_test_thread_metadata(&session_id, &project_b, cx).await;

    // Ensure workspace A is active.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().next().unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_a
    );

    // Call activate_archived_thread – should resolve saved paths and
    // switch to the workspace for project-b.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(
            ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(session_id.clone()),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some("Archived Thread".into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
                    "/project-b",
                )])),
                archived: false,
                remote_connection: None,
            },
            window,
            cx,
        );
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b,
        "should have switched to the workspace matching the saved paths"
    );
}

#[gpui::test]
async fn test_activate_archived_thread_cwd_fallback_with_matching_workspace(
    cx: &mut TestAppContext,
) {
    // Thread has no saved metadata but session_info has cwd. A matching
    // workspace is open. Expected: uses cwd to find and activate it.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx)
    });
    let workspace_a =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());

    // Start with workspace A active.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().next().unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_a
    );

    // No thread saved to the store – cwd is the only path hint.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(
            ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(acp::SessionId::new(Arc::from("unknown-session"))),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some("CWD Thread".into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[
                    std::path::PathBuf::from("/project-b"),
                ])),
                archived: false,
                remote_connection: None,
            },
            window,
            cx,
        );
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b,
        "should have activated the workspace matching the cwd"
    );
}

#[gpui::test]
async fn test_projectless_history_opens_folderless_workspace(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx)
    });

    // Activate workspace B (index 1) to make it the active one.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().nth(1).unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(
            ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(acp::SessionId::new(Arc::from("no-context-session"))),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some("Contextless Thread".into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::default(),
                archived: false,
                remote_connection: None,
            },
            window,
            cx,
        );
    });
    cx.run_until_parked();

    multi_workspace.read_with(cx, |multi_workspace, cx| {
        assert_ne!(multi_workspace.workspace(), &workspace_b);
        assert!(
            multi_workspace
                .workspace()
                .read(cx)
                .root_paths(cx)
                .is_empty()
        );
        assert!(multi_workspace.is_workspace_retained(&workspace_b));
    });
}

#[gpui::test]
async fn test_activate_archived_thread_saved_paths_opens_new_workspace(cx: &mut TestAppContext) {
    // Thread has saved metadata pointing to a path with no open workspace.
    // Expected: opens a new workspace for that path.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Save a thread with path_list pointing to project-b – which has no
    // open workspace.
    let path_list_b = PathList::new(&[std::path::PathBuf::from("/project-b")]);
    let session_id = acp::SessionId::new(Arc::from("archived-new-ws"));

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "should start with one workspace"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(
            ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(session_id.clone()),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some("New WS Thread".into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::from_folder_paths(&path_list_b),
                archived: false,
                remote_connection: None,
            },
            window,
            cx,
        );
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "should have opened a second workspace for the archived thread's saved paths"
    );
}

#[gpui::test]
async fn test_activate_archived_thread_reuses_workspace_in_another_window(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let multi_workspace_a =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let multi_workspace_b =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_b, window, cx));

    let multi_workspace_a_entity = multi_workspace_a.root(cx).unwrap();
    let multi_workspace_b_entity = multi_workspace_b.root(cx).unwrap();

    let cx_b = &mut gpui::VisualTestContext::from_window(multi_workspace_b.into(), cx);
    let _sidebar_b = setup_sidebar(&multi_workspace_b_entity, cx_b);

    let cx_a = &mut gpui::VisualTestContext::from_window(multi_workspace_a.into(), cx);
    let sidebar = setup_sidebar(&multi_workspace_a_entity, cx_a);

    let session_id = acp::SessionId::new(Arc::from("archived-cross-window"));

    sidebar.update_in(cx_a, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(
            ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(session_id.clone()),
                agent_id: agent::ZED_AGENT_ID.clone(),
                title: Some("Cross Window Thread".into()),
                title_override: None,
                updated_at: Utc::now(),
                created_at: None,
                interacted_at: None,
                worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
                    "/project-b",
                )])),
                archived: false,
                remote_connection: None,
            },
            window,
            cx,
        );
    });
    cx_a.run_until_parked();

    assert_eq!(
        multi_workspace_a
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "should not add the other window's workspace into the current window"
    );
    assert_eq!(
        multi_workspace_b
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "should reuse the existing workspace in the other window"
    );
    assert!(
        cx_a.read(|cx| cx.active_window().unwrap()) == *multi_workspace_b,
        "should activate the window that already owns the matching workspace"
    );
    sidebar.read_with(cx_a, |sidebar, _| {
            assert!(
                !is_active_session(&sidebar, &session_id),
                "source window's sidebar should not eagerly claim focus for a thread opened in another window"
            );
        });
}

#[gpui::test]
async fn test_activate_archived_thread_reuses_workspace_in_another_window_with_target_sidebar(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let multi_workspace_a =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let multi_workspace_b =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_b.clone(), window, cx));

    let multi_workspace_a_entity = multi_workspace_a.root(cx).unwrap();
    let multi_workspace_b_entity = multi_workspace_b.root(cx).unwrap();

    let cx_a = &mut gpui::VisualTestContext::from_window(multi_workspace_a.into(), cx);
    let sidebar_a = setup_sidebar(&multi_workspace_a_entity, cx_a);

    let cx_b = &mut gpui::VisualTestContext::from_window(multi_workspace_b.into(), cx);
    let sidebar_b = setup_sidebar(&multi_workspace_b_entity, cx_b);
    let workspace_b = multi_workspace_b_entity.read_with(cx_b, |mw, _| mw.workspace().clone());
    let _panel_b = add_agent_panel(&workspace_b, cx_b);

    let session_id = acp::SessionId::new(Arc::from("archived-cross-window-with-sidebar"));
    let metadata = ThreadMetadata {
        thread_id: ThreadId::new(),
        session_id: Some(session_id.clone()),
        agent_id: agent::ZED_AGENT_ID.clone(),
        title: Some("Cross Window Thread".into()),
        title_override: None,
        updated_at: Utc::now(),
        created_at: None,
        interacted_at: None,
        worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
            "/project-b",
        )])),
        archived: false,
        remote_connection: None,
    };
    seed_thread_metadata(metadata.clone(), cx_a);

    sidebar_a.update_in(cx_a, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx_a.run_until_parked();

    assert_eq!(
        multi_workspace_a
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "should not add the other window's workspace into the current window"
    );
    assert_eq!(
        multi_workspace_b
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "should reuse the existing workspace in the other window"
    );
    assert!(
        cx_a.read(|cx| cx.active_window().unwrap()) == *multi_workspace_b,
        "should activate the window that already owns the matching workspace"
    );
    sidebar_a.read_with(cx_a, |sidebar, _| {
            assert!(
                !is_active_session(&sidebar, &session_id),
                "source window's sidebar should not eagerly claim focus for a thread opened in another window"
            );
        });
    sidebar_b.read_with(cx_b, |sidebar, _| {
        assert_active_thread(
            sidebar,
            &session_id,
            "target window's sidebar should eagerly focus the activated archived thread",
        );
    });
}

#[gpui::test]
async fn test_activate_archived_thread_prefers_current_window_for_matching_paths(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_b = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;

    let multi_workspace_b =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_b, window, cx));
    let multi_workspace_a =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    let multi_workspace_a_entity = multi_workspace_a.root(cx).unwrap();
    let multi_workspace_b_entity = multi_workspace_b.root(cx).unwrap();

    let cx_b = &mut gpui::VisualTestContext::from_window(multi_workspace_b.into(), cx);
    let _sidebar_b = setup_sidebar(&multi_workspace_b_entity, cx_b);

    let cx_a = &mut gpui::VisualTestContext::from_window(multi_workspace_a.into(), cx);
    let sidebar_a = setup_sidebar(&multi_workspace_a_entity, cx_a);

    let session_id = acp::SessionId::new(Arc::from("archived-current-window"));
    let metadata = ThreadMetadata {
        thread_id: ThreadId::new(),
        session_id: Some(session_id.clone()),
        agent_id: agent::ZED_AGENT_ID.clone(),
        title: Some("Current Window Thread".into()),
        title_override: None,
        updated_at: Utc::now(),
        created_at: None,
        interacted_at: None,
        worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
            "/project-a",
        )])),
        archived: false,
        remote_connection: None,
    };
    seed_thread_metadata(metadata.clone(), cx_a);

    sidebar_a.update_in(cx_a, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx_a.run_until_parked();

    assert!(
        cx_a.read(|cx| cx.active_window().unwrap()) == *multi_workspace_a,
        "should keep activation in the current window when it already has a matching workspace"
    );
    sidebar_a.read_with(cx_a, |sidebar, _| {
        assert_active_thread(
            sidebar,
            &session_id,
            "current window's sidebar should eagerly focus the activated archived thread",
        );
    });
    assert_eq!(
        multi_workspace_a
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "current window should continue reusing its existing workspace"
    );
    assert_eq!(
        multi_workspace_b
            .read_with(cx_a, |mw, _| mw.workspaces().count())
            .unwrap(),
        1,
        "other windows should not be activated just because they also match the saved paths"
    );
}

#[gpui::test]
async fn test_archive_thread_uses_next_threads_own_workspace(cx: &mut TestAppContext) {
    // Regression test: archive_thread previously always loaded the next thread
    // through group_workspace (the main workspace's ProjectHeader), even when
    // the next thread belonged to an absorbed linked-worktree workspace. That
    // caused the worktree thread to be loaded in the main panel, which bound it
    // to the main project and corrupted its stored folder_paths.
    //
    // The fix: use next.workspace (ThreadEntryWorkspace::Open) when available,
    // falling back to group_workspace only for Closed workspaces.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    // Activate main workspace so the sidebar tracks the main panel.
    multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = mw.workspaces().next().unwrap().clone();
        mw.activate(workspace, None, window, cx);
    });

    let main_workspace =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());
    let main_panel = add_agent_panel(&main_workspace, cx);
    let _worktree_panel = add_agent_panel(&worktree_workspace, cx);

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&main_panel, connection, cx);
    send_message(&main_panel, cx);

    let thread2_session_id = active_session_id(&main_panel, cx);

    // Save thread 2's metadata with a newer timestamp so it sorts above thread 1.
    save_thread_metadata(
        thread2_session_id.clone(),
        Some("Thread 2".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );

    // Save thread 1's metadata with the worktree path and an older timestamp so
    // it sorts below thread 2. archive_thread will find it as the "next" candidate.
    let thread1_session_id = acp::SessionId::new(Arc::from("thread1-worktree-session"));
    save_thread_metadata(
        thread1_session_id,
        Some("Thread 1".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &worktree_project,
        cx,
    );

    cx.run_until_parked();

    // Verify the sidebar absorbed thread 1 under [project] with the worktree chip.
    let entries_before = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries_before.iter().any(|s| s.contains("{wt-feature-a}")),
        "Thread 1 should appear with the linked-worktree chip before archiving: {:?}",
        entries_before
    );

    // The sidebar should track T2 as the focused thread (derived from the
    // main panel's active view).
    sidebar.read_with(cx, |s, _| {
        assert_active_thread(
            s,
            &thread2_session_id,
            "focused thread should be Thread 2 before archiving",
        );
    });

    // Archive thread 2.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&thread2_session_id, window, cx);
    });

    cx.run_until_parked();

    // The archived thread leaves the main panel, and Thread 1 must not be loaded there.
    let main_active = main_panel.read_with(cx, |panel, cx| {
        panel
            .active_agent_thread(cx)
            .map(|t| t.read(cx).session_id().clone())
    });
    assert_eq!(
        main_active, None,
        "main panel should not have been taken over by loading the linked-worktree thread T1; \
             before the fix, archive_thread used group_workspace instead of next.workspace, \
             causing T1 to be loaded in the wrong panel"
    );

    // Thread 1 should still appear in the sidebar with its worktree chip
    // (Thread 2 was archived so it is gone from the list).
    let entries_after = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries_after.iter().any(|s| s.contains("{wt-feature-a}")),
        "T1 should still carry its linked-worktree chip after archiving T2: {:?}",
        entries_after
    );
}

#[gpui::test]
async fn test_restore_worktree_when_branch_has_moved(cx: &mut TestAppContext) {
    // restore_worktree_via_git should succeed when the branch has moved
    // to a different SHA since archival. The worktree stays in detached
    // HEAD and the moved branch is left untouched.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/wt-feature-a",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "original-sha".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, _cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    multi_workspace.update_in(_cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    let wt_repo = worktree_project.read_with(cx, |project, cx| {
        project.repositories(cx).values().next().unwrap().clone()
    });
    let (staged_hash, unstaged_hash) = cx
        .update(|cx| wt_repo.update(cx, |repo, _| repo.create_archive_checkpoint()))
        .await
        .unwrap()
        .unwrap();

    // Move the branch to a different SHA.
    fs.with_git_state(Path::new("/project/.git"), false, |state| {
        state
            .refs
            .insert("refs/heads/feature-a".into(), "moved-sha".into());
    })
    .unwrap();

    let result = cx
        .spawn(|mut cx| async move {
            agent_ui::thread_worktree_archive::restore_worktree_via_git(
                &agent_ui::thread_metadata_store::ArchivedGitWorktree {
                    id: 1,
                    worktree_path: PathBuf::from("/wt-feature-a"),
                    main_repo_path: PathBuf::from("/project"),
                    branch_name: Some("feature-a".to_string()),
                    staged_commit_hash: staged_hash,
                    unstaged_commit_hash: unstaged_hash,
                    original_commit_hash: "original-sha".to_string(),
                },
                None,
                &mut cx,
            )
            .await
        })
        .await;

    assert!(
        result.is_ok(),
        "restore should succeed even when branch has moved: {:?}",
        result.err()
    );

    // The moved branch ref should be completely untouched.
    let branch_sha = fs
        .with_git_state(Path::new("/project/.git"), false, |state| {
            state.refs.get("refs/heads/feature-a").cloned()
        })
        .unwrap();
    assert_eq!(
        branch_sha.as_deref(),
        Some("moved-sha"),
        "the moved branch ref should not be modified by the restore"
    );
}

#[gpui::test]
async fn test_restore_worktree_when_branch_has_not_moved(cx: &mut TestAppContext) {
    // restore_worktree_via_git should succeed when the branch still
    // points at the same SHA as at archive time.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-b": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-b",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/wt-feature-b",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-b",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/wt-feature-b"),
            ref_name: Some("refs/heads/feature-b".into()),
            sha: "original-sha".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-b".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, _cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    multi_workspace.update_in(_cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    let wt_repo = worktree_project.read_with(cx, |project, cx| {
        project.repositories(cx).values().next().unwrap().clone()
    });
    let (staged_hash, unstaged_hash) = cx
        .update(|cx| wt_repo.update(cx, |repo, _| repo.create_archive_checkpoint()))
        .await
        .unwrap()
        .unwrap();

    // refs/heads/feature-b already points at "original-sha" (set by
    // add_linked_worktree_for_repo), matching original_commit_hash.

    let result = cx
        .spawn(|mut cx| async move {
            agent_ui::thread_worktree_archive::restore_worktree_via_git(
                &agent_ui::thread_metadata_store::ArchivedGitWorktree {
                    id: 1,
                    worktree_path: PathBuf::from("/wt-feature-b"),
                    main_repo_path: PathBuf::from("/project"),
                    branch_name: Some("feature-b".to_string()),
                    staged_commit_hash: staged_hash,
                    unstaged_commit_hash: unstaged_hash,
                    original_commit_hash: "original-sha".to_string(),
                },
                None,
                &mut cx,
            )
            .await
        })
        .await;

    assert!(
        result.is_ok(),
        "restore should succeed when branch has not moved: {:?}",
        result.err()
    );
}

#[gpui::test]
async fn test_restore_worktree_when_branch_does_not_exist(cx: &mut TestAppContext) {
    // restore_worktree_via_git should succeed when the branch no longer
    // exists (e.g. it was deleted while the thread was archived). The
    // code should attempt to recreate the branch.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-d": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-d",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/wt-feature-d",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-d",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/wt-feature-d"),
            ref_name: Some("refs/heads/feature-d".into()),
            sha: "original-sha".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-d".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, _cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    multi_workspace.update_in(_cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    let wt_repo = worktree_project.read_with(cx, |project, cx| {
        project.repositories(cx).values().next().unwrap().clone()
    });
    let (staged_hash, unstaged_hash) = cx
        .update(|cx| wt_repo.update(cx, |repo, _| repo.create_archive_checkpoint()))
        .await
        .unwrap()
        .unwrap();

    // Remove the branch ref so change_branch will fail.
    fs.with_git_state(Path::new("/project/.git"), false, |state| {
        state.refs.remove("refs/heads/feature-d");
    })
    .unwrap();

    let result = cx
        .spawn(|mut cx| async move {
            agent_ui::thread_worktree_archive::restore_worktree_via_git(
                &agent_ui::thread_metadata_store::ArchivedGitWorktree {
                    id: 1,
                    worktree_path: PathBuf::from("/wt-feature-d"),
                    main_repo_path: PathBuf::from("/project"),
                    branch_name: Some("feature-d".to_string()),
                    staged_commit_hash: staged_hash,
                    unstaged_commit_hash: unstaged_hash,
                    original_commit_hash: "original-sha".to_string(),
                },
                None,
                &mut cx,
            )
            .await
        })
        .await;

    assert!(
        result.is_ok(),
        "restore should succeed when branch does not exist: {:?}",
        result.err()
    );
}

#[gpui::test]
async fn test_restore_worktree_thread_uses_main_repo_project_group_key(cx: &mut TestAppContext) {
    // Activating an archived linked worktree thread whose directory has
    // been deleted should reuse the existing main repo workspace, not
    // create a new one. The provisional ProjectGroupKey must be derived
    // from main_worktree_paths so that find_or_create_local_workspace
    // matches the main repo workspace when the worktree path is absent.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-c": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-c",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;

    fs.insert_tree(
        "/wt-feature-c",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-c",
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/wt-feature-c"),
            ref_name: Some("refs/heads/feature-c".into()),
            sha: "original-sha".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-c".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    // Save thread metadata for the linked worktree.
    let wt_session_id = acp::SessionId::new(Arc::from("wt-thread-c"));
    save_thread_metadata(
        wt_session_id.clone(),
        Some("Worktree Thread C".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &worktree_project,
        cx,
    );
    cx.run_until_parked();

    let thread_id = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&wt_session_id)
            .unwrap()
            .thread_id
    });

    // Archive the thread without creating ArchivedGitWorktree records.
    let store = cx.update(|_window, cx| ThreadMetadataStore::global(cx));
    cx.update(|_window, cx| {
        store.update(cx, |store, cx| store.archive(thread_id, None, cx));
    });
    cx.run_until_parked();

    // Remove the worktree workspace and delete the worktree from disk.
    let remove_task = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.remove(
            vec![worktree_workspace],
            RemovalIntent::KeepProject,
            window,
            cx,
        )
    });
    remove_task.await.ok();
    cx.run_until_parked();
    cx.run_until_parked();
    fs.remove_dir(
        Path::new("/wt-feature-c"),
        fs::RemoveOptions {
            recursive: true,
            ignore_if_not_exists: true,
        },
    )
    .await
    .unwrap();

    let workspace_count_before = multi_workspace.read_with(cx, |mw, _| mw.workspaces().count());
    assert_eq!(
        workspace_count_before, 1,
        "should have only the main workspace"
    );

    // Activate the archived thread. The worktree path is missing from
    // disk, so find_or_create_local_workspace falls back to the
    // provisional ProjectGroupKey to find a matching workspace.
    let metadata = cx.update(|_window, cx| store.read(cx).entry(thread_id).unwrap().clone());
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx.run_until_parked();

    // The provisional key should use [/project] (the main repo),
    // which matches the existing main workspace. If it incorrectly
    // used [/wt-feature-c] (the linked worktree path), no workspace
    // would match and a spurious new one would be created.
    let workspace_count_after = multi_workspace.read_with(cx, |mw, _| mw.workspaces().count());
    assert_eq!(
        workspace_count_after, 1,
        "restoring a linked worktree thread should reuse the main repo workspace, \
         not create a new one (workspace count went from {workspace_count_before} to \
         {workspace_count_after})"
    );
}

#[gpui::test]
async fn test_linked_worktree_threads_not_duplicated_across_groups(cx: &mut TestAppContext) {
    // When a multi-root workspace (e.g. [/other, /project]) shares a
    // repo with a single-root workspace (e.g. [/project]), linked
    // worktree threads from the shared repo should only appear under
    // the dedicated group [project], not under [other / project].
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });
    let fs = FakeFs::new(cx.executor());

    // Two independent repos, each with their own git history.
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/other",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    // Register the linked worktree in the main repo.
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    // Workspace 1: just /project.
    let project_only = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    project_only
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    // Workspace 2: /other and /project together (multi-root).
    let multi_root =
        project::Project::test(fs.clone(), ["/other".as_ref(), "/project".as_ref()], cx).await;
    multi_root
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    // Save a thread under the linked worktree path BEFORE setting up
    // the sidebar and panels, so that reconciliation sees the [project]
    // group as non-empty and doesn't create a spurious draft there.
    let wt_session_id = acp::SessionId::new(Arc::from("wt-thread"));
    save_thread_metadata(
        wt_session_id,
        Some("Worktree Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &worktree_project,
        cx,
    );

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_only.clone(), window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let multi_root_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(multi_root.clone(), window, cx)
    });
    add_agent_panel(&multi_root_workspace, cx);
    cx.run_until_parked();

    // The thread should appear only under [project] (the dedicated
    // group for the /project repo), not under [other / project].
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [other / project]",
            "v [project]",
            "  Worktree Thread {wt-feature-a}",
        ]
    );
}

fn thread_id_for(session_id: &acp::SessionId, cx: &mut TestAppContext) -> ThreadId {
    cx.read(|cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(session_id)
            .map(|m| m.thread_id)
            .expect("thread metadata should exist")
    })
}

#[gpui::test]
async fn test_thread_switcher_ordering(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let switcher_ids =
        |sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext| -> Vec<ThreadId> {
            sidebar.read_with(cx, |sidebar, cx| {
                let switcher = sidebar
                    .thread_switcher
                    .as_ref()
                    .expect("switcher should be open");
                switcher
                    .read(cx)
                    .entries()
                    .iter()
                    .map(|entry| entry.thread_id().expect("expected thread switcher entry"))
                    .collect()
            })
        };

    let switcher_selected_id =
        |sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext| -> ThreadId {
            sidebar.read_with(cx, |sidebar, cx| {
                let switcher = sidebar
                    .thread_switcher
                    .as_ref()
                    .expect("switcher should be open");
                let s = switcher.read(cx);
                s.selected_entry()
                    .expect("should have selection")
                    .thread_id()
                    .expect("expected selected thread entry")
            })
        };

    // ── Setup: create three threads with distinct created_at times ──────
    // Thread C (oldest), Thread B, Thread A (newest) — by created_at.
    // We send messages in each so they also get last_message_sent_or_queued timestamps.
    let connection_c = StubAgentConnection::new();
    connection_c.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done C".into()),
    )]);
    open_thread_with_connection(&panel, connection_c, cx);
    send_message(&panel, cx);
    let session_id_c = active_session_id(&panel, cx);
    let thread_id_c = active_thread_id(&panel, cx);
    save_thread_metadata(
        session_id_c.clone(),
        Some("Thread C".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        Some(chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap()),
        None,
        &project,
        cx,
    );

    let connection_b = StubAgentConnection::new();
    connection_b.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done B".into()),
    )]);
    open_thread_with_connection(&panel, connection_b, cx);
    send_message(&panel, cx);
    let session_id_b = active_session_id(&panel, cx);
    let thread_id_b = active_thread_id(&panel, cx);
    save_thread_metadata(
        session_id_b.clone(),
        Some("Thread B".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        Some(chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap()),
        None,
        &project,
        cx,
    );

    let connection_a = StubAgentConnection::new();
    connection_a.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done A".into()),
    )]);
    open_thread_with_connection(&panel, connection_a, cx);
    send_message(&panel, cx);
    let session_id_a = active_session_id(&panel, cx);
    let thread_id_a = active_thread_id(&panel, cx);
    save_thread_metadata(
        session_id_a.clone(),
        Some("Thread A".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        Some(chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap()),
        None,
        &project,
        cx,
    );

    // All three threads are now live. Thread A was opened last, so it's
    // the one being viewed. Opening each thread called record_thread_access,
    // so all three have last_accessed_at set.
    // Access order is: A (most recent), B, C (oldest).

    // ── 1. Open switcher: threads sorted by last_accessed_at ─────────────────
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    // All three have last_accessed_at, so they sort by access time.
    // A was accessed most recently (it's the currently viewed thread),
    // then B, then C.
    assert_eq!(
        switcher_ids(&sidebar, cx),
        vec![thread_id_a, thread_id_b, thread_id_c,],
    );
    // First ctrl-tab selects the second entry (B).
    assert_eq!(switcher_selected_id(&sidebar, cx), thread_id_b);

    // Dismiss the switcher without confirming.
    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.dismiss_thread_switcher(cx);
    });
    cx.run_until_parked();

    // ── 2. Confirm on Thread C: it becomes most-recently-accessed ──────
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    // Cycle twice to land on Thread C (index 2).
    sidebar.read_with(cx, |sidebar, cx| {
        let switcher = sidebar.thread_switcher.as_ref().unwrap();
        assert_eq!(switcher.read(cx).selected_index(), 1);
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar
            .thread_switcher
            .as_ref()
            .unwrap()
            .update(cx, |s, cx| s.select_next(&menu::SelectNext, window, cx));
    });
    cx.run_until_parked();
    assert_eq!(switcher_selected_id(&sidebar, cx), thread_id_c);

    assert!(sidebar.update(cx, |sidebar, _cx| sidebar.thread_last_accessed.is_empty()));

    // Confirm on Thread C.
    sidebar.update_in(cx, |sidebar, window, cx| {
        let switcher = sidebar.thread_switcher.as_ref().unwrap();
        let focus = switcher.focus_handle(cx);
        focus.dispatch_action(&menu::Confirm, window, cx);
    });
    cx.run_until_parked();

    // Switcher should be dismissed after confirm.
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            sidebar.thread_switcher.is_none(),
            "switcher should be dismissed"
        );
    });

    sidebar.update(cx, |sidebar, _cx| {
        let last_accessed = sidebar
            .thread_last_accessed
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(last_accessed.len(), 1);
        assert!(last_accessed.contains(&thread_id_c));
        assert!(
            is_active_session(&sidebar, &session_id_c),
            "active_entry should be Thread({session_id_c:?})"
        );
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        switcher_ids(&sidebar, cx),
        vec![thread_id_c, thread_id_a, thread_id_b],
    );

    // Confirm on Thread A.
    sidebar.update_in(cx, |sidebar, window, cx| {
        let switcher = sidebar.thread_switcher.as_ref().unwrap();
        let focus = switcher.focus_handle(cx);
        focus.dispatch_action(&menu::Confirm, window, cx);
    });
    cx.run_until_parked();

    sidebar.update(cx, |sidebar, _cx| {
        let last_accessed = sidebar
            .thread_last_accessed
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(last_accessed.len(), 2);
        assert!(last_accessed.contains(&thread_id_c));
        assert!(last_accessed.contains(&thread_id_a));
        assert!(
            is_active_session(&sidebar, &session_id_a),
            "active_entry should be Thread({session_id_a:?})"
        );
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        switcher_ids(&sidebar, cx),
        vec![thread_id_a, thread_id_c, thread_id_b,],
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        let switcher = sidebar.thread_switcher.as_ref().unwrap();
        switcher.update(cx, |switcher, cx| {
            switcher.select_next(&menu::SelectNext, window, cx)
        });
    });
    cx.run_until_parked();

    // Confirm on Thread B.
    sidebar.update_in(cx, |sidebar, window, cx| {
        let switcher = sidebar.thread_switcher.as_ref().unwrap();
        let focus = switcher.focus_handle(cx);
        focus.dispatch_action(&menu::Confirm, window, cx);
    });
    cx.run_until_parked();

    sidebar.update(cx, |sidebar, _cx| {
        let last_accessed = sidebar
            .thread_last_accessed
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(last_accessed.len(), 3);
        assert!(last_accessed.contains(&thread_id_c));
        assert!(last_accessed.contains(&thread_id_a));
        assert!(last_accessed.contains(&thread_id_b));
        assert!(
            is_active_session(&sidebar, &session_id_b),
            "active_entry should be Thread({session_id_b:?})"
        );
    });

    // ── 3. Add a historical thread (no last_accessed_at, no message sent) ──
    // This thread was never opened in a panel — it only exists in metadata.
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-historical")),
        Some("Historical Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 1, 0, 0, 0).unwrap(),
        Some(chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 6, 1, 0, 0, 0).unwrap()),
        None,
        &project,
        cx,
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    // Historical Thread has no last_accessed_at and no last_message_sent_or_queued,
    // so it falls to tier 3 (sorted by created_at). It should appear after all
    // accessed threads, even though its created_at (June 2024) is much later
    // than the others.
    //
    // But the live threads (A, B, C) each had send_message called which sets
    // last_message_sent_or_queued. So for the accessed threads (tier 1) the
    // sort key is last_accessed_at; for Historical Thread (tier 3) it's created_at.
    let session_id_hist = acp::SessionId::new(Arc::from("thread-historical"));
    let thread_id_hist = thread_id_for(&session_id_hist, cx);

    let ids = switcher_ids(&sidebar, cx);
    assert_eq!(
        ids,
        vec![thread_id_b, thread_id_a, thread_id_c, thread_id_hist],
    );

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.dismiss_thread_switcher(cx);
    });
    cx.run_until_parked();

    // ── 4. Add another historical thread with older created_at ─────────
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-old-historical")),
        Some("Old Historical Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2023, 6, 1, 0, 0, 0).unwrap(),
        Some(chrono::TimeZone::with_ymd_and_hms(&Utc, 2023, 6, 1, 0, 0, 0).unwrap()),
        None,
        &project,
        cx,
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    // Both historical threads have no access or message times. They should
    // appear after accessed threads, sorted by created_at (newest first).
    let session_id_old_hist = acp::SessionId::new(Arc::from("thread-old-historical"));
    let thread_id_old_hist = thread_id_for(&session_id_old_hist, cx);
    let ids = switcher_ids(&sidebar, cx);
    assert_eq!(
        ids,
        vec![
            thread_id_b,
            thread_id_a,
            thread_id_c,
            thread_id_hist,
            thread_id_old_hist,
        ],
    );

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.dismiss_thread_switcher(cx);
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn test_thread_switcher_menu_navigation_wraps_without_confirming(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Opened oldest first, so the switcher lists them as A, B, C.
    let mut session_ids = Vec::new();
    for (day, title) in [(1, "Thread C"), (2, "Thread B"), (3, "Thread A")] {
        let connection = StubAgentConnection::new();
        connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
            acp_v1::ContentChunk::new("Done".into()),
        )]);
        open_thread_with_connection(&panel, connection, cx);
        send_message(&panel, cx);
        let session_id = active_session_id(&panel, cx);
        let timestamp = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, day, 0, 0, 0).unwrap();
        save_thread_metadata(
            session_id.clone(),
            Some(title.into()),
            timestamp,
            Some(timestamp),
            None,
            &project,
            cx,
        );
        session_ids.push(session_id);
    }
    let [session_id_c, session_id_b, session_id_a] =
        session_ids.try_into().expect("three threads were opened");

    let selected_session_id = |cx: &mut gpui::VisualTestContext| -> acp::SessionId {
        sidebar.read_with(cx, |sidebar, cx| {
            let switcher = sidebar
                .thread_switcher
                .as_ref()
                .expect("switcher should be open");
            switcher
                .read(cx)
                .selected_entry()
                .and_then(|entry| match entry {
                    ThreadSwitcherEntry::Thread(entry) => entry.metadata.session_id.clone(),
                    ThreadSwitcherEntry::Terminal(_) => None,
                })
                .expect("a thread should be selected")
        })
    };
    let dispatch_to_switcher = |action: &dyn gpui::Action, cx: &mut gpui::VisualTestContext| {
        sidebar.update_in(cx, |sidebar, window, cx| {
            let switcher = sidebar
                .thread_switcher
                .as_ref()
                .expect("switcher should be open");
            switcher
                .focus_handle(cx)
                .dispatch_action(action, window, cx);
        });
        cx.run_until_parked();
    };

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();
    assert_eq!(selected_session_id(cx), session_id_b);

    dispatch_to_switcher(&menu::SelectNext, cx);
    assert_eq!(selected_session_id(cx), session_id_c);
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            is_active_session(sidebar, &session_id_c),
            "stepping should preview the newly selected thread"
        );
    });

    dispatch_to_switcher(&menu::SelectNext, cx);
    assert_eq!(selected_session_id(cx), session_id_a);

    dispatch_to_switcher(&menu::SelectPrevious, cx);
    assert_eq!(selected_session_id(cx), session_id_c);

    dispatch_to_switcher(&menu::SelectPrevious, cx);
    assert_eq!(selected_session_id(cx), session_id_b);

    dispatch_to_switcher(&menu::SelectLast, cx);
    assert_eq!(selected_session_id(cx), session_id_c);

    dispatch_to_switcher(&menu::SelectFirst, cx);
    assert_eq!(selected_session_id(cx), session_id_a);

    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            sidebar.thread_switcher.is_some(),
            "navigating should not confirm or dismiss the switcher"
        );
        assert!(
            sidebar.thread_last_accessed.is_empty(),
            "navigating should not record a thread access"
        );
    });

    dispatch_to_switcher(&menu::Confirm, cx);
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(sidebar.thread_switcher.is_none());
        assert!(is_active_session(sidebar, &session_id_a));
    });
}

#[gpui::test]
async fn test_archive_thread_keeps_metadata_but_hides_from_sidebar(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-to-archive")),
        Some("Thread To Archive".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries.iter().any(|e| e.contains("Thread To Archive")),
        "expected thread to be visible before archiving, got: {entries:?}"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(
            &acp::SessionId::new(Arc::from("thread-to-archive")),
            window,
            cx,
        );
    });
    cx.run_until_parked();

    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        !entries.iter().any(|e| e.contains("Thread To Archive")),
        "expected thread to be hidden after archiving, got: {entries:?}"
    );

    cx.update(|_, cx| {
        let store = ThreadMetadataStore::global(cx);
        let archived: Vec<_> = store.read(cx).archived_entries().collect();
        assert_eq!(archived.len(), 1);
        assert_eq!(
            archived[0].session_id.as_ref().unwrap().0.as_ref(),
            "thread-to-archive"
        );
        assert!(archived[0].archived);
    });
}

#[gpui::test]
async fn test_archive_thread_drops_retained_conversation_view(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);
    let session_id = active_session_id(&panel, cx);
    let thread_id = active_thread_id(&panel, cx);
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _| {
        assert!(
            is_active_session(sidebar, &session_id),
            "expected the newly created thread to be active before archiving",
        );
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });
    cx.run_until_parked();

    panel.read_with(cx, |panel, _| {
        assert!(
            !panel.is_retained_thread(&thread_id),
            "archiving a thread must drop its ConversationView from retained_threads, \
             but the archived thread id {thread_id:?} is still retained",
        );
    });
}

#[gpui::test]
async fn test_archive_thread_active_entry_management(cx: &mut TestAppContext) {
    // Tests two archive scenarios:
    // 1. Archiving a thread in a non-active workspace leaves active_entry
    //    as the current draft.
    // 2. Archiving the thread the user is looking at falls back to a draft
    //    on the same workspace.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    // Explicitly create a draft on workspace_b so the sidebar tracks one.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_new_thread(&workspace_b, window, cx);
    });
    cx.run_until_parked();

    // --- Scenario 1: archive a thread in the non-active workspace ---

    // Create a thread in project-a (non-active — project-b is active).
    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel_a, connection, cx);
    agent_ui::test_support::send_message(&panel_a, cx);
    let thread_a = agent_ui::test_support::active_session_id(&panel_a, cx);
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&thread_a, window, cx);
    });
    cx.run_until_parked();

    // active_entry should still be a draft on workspace_b (the active one).
    sidebar.read_with(cx, |sidebar, _| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { workspace: ws, .. }) if ws == &workspace_b),
            "expected Draft(workspace_b) after archiving non-active thread, got: {:?}",
            sidebar.active_entry,
        );
    });

    // --- Scenario 2: archive the thread the user is looking at ---

    // Create a thread in project-b (the active workspace) and verify it
    // becomes the active entry.
    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel_b, connection, cx);
    agent_ui::test_support::send_message(&panel_b, cx);
    let thread_b = agent_ui::test_support::active_session_id(&panel_b, cx);
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _| {
        assert!(
            is_active_session(&sidebar, &thread_b),
            "expected active_entry to be Thread({thread_b}), got: {:?}",
            sidebar.active_entry,
        );
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&thread_b, window, cx);
    });
    cx.run_until_parked();

    // Archiving the active thread moves to another entry on the same workspace.
    sidebar.read_with(cx, |sidebar, _| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { workspace: ws, .. }) if ws == &workspace_b),
            "expected draft on workspace_b after archiving active thread, got: {:?}",
            sidebar.active_entry,
        );
    });
}

#[gpui::test]
async fn test_unarchive_only_shows_restored_thread(cx: &mut TestAppContext) {
    // Full flow: create a thread, archive it (removing the workspace),
    // then unarchive. Only the restored thread should appear — no
    // leftover drafts or previously-serialized threads.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Create a thread and send a message so it's a real thread.
    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Hello".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel, connection, cx);
    agent_ui::test_support::send_message(&panel, cx);
    let session_id = agent_ui::test_support::active_session_id(&panel, cx);
    cx.run_until_parked();

    // Archive it.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });
    cx.run_until_parked();

    // Grab metadata for unarchive.
    let thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("thread should exist")
    });
    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("metadata should exist")
    });

    // Unarchive it — the draft should be replaced by the restored thread.
    let restored_title = metadata.display_title();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx.run_until_parked();

    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries.iter().any(|e| e.contains(restored_title.as_ref())),
        "expected the restored thread to be visible, got entries: {entries:?}"
    );
    let thread_count = entries
        .iter()
        .filter(|e| !e.starts_with("v ") && !e.starts_with("> "))
        .count();
    assert!(
        thread_count <= 2,
        "expected at most the restored thread plus a parked draft, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_first_thread_in_group_does_not_create_spurious_draft(
    cx: &mut TestAppContext,
) {
    // When a thread is unarchived into a project group that has no open
    // workspace, the sidebar opens a new workspace and loads the thread.
    // No spurious draft should appear alongside the unarchived thread.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    cx.run_until_parked();

    // Save an archived thread whose folder_paths point to project-b,
    // which has no open workspace.
    let session_id = acp::SessionId::new(Arc::from("archived-thread"));
    let path_list_b = PathList::new(&[std::path::PathBuf::from("/project-b")]);
    let thread_id = ThreadId::new();
    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(
                ThreadMetadata {
                    thread_id,
                    session_id: Some(session_id.clone()),
                    agent_id: agent::ZED_AGENT_ID.clone(),
                    title: Some("Unarchived Thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: None,
                    interacted_at: None,
                    worktree_paths: WorktreePaths::from_folder_paths(&path_list_b),
                    archived: true,
                    remote_connection: None,
                },
                cx,
            )
        });
    });
    cx.run_until_parked();

    // Verify no workspace for project-b exists yet.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "should start with only the project-a workspace"
    );

    // Un-archive the thread — should open project-b workspace and load it.
    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("metadata should exist")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx.run_until_parked();

    // A second workspace should have been created for project-b.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "should have opened a workspace for the unarchived thread"
    );

    // The sidebar should show the unarchived thread without a spurious draft
    // in the project-b group.
    let entries = visible_entries_as_strings(&sidebar, cx);
    let draft_count = entries.iter().filter(|e| e.contains("Draft")).count();
    // project-a gets a draft (it's the active workspace with no threads),
    // but project-b should NOT have one — only the unarchived thread.
    assert!(
        draft_count <= 1,
        "expected at most one draft (for project-a), got entries: {entries:?}"
    );
    assert!(
        entries.iter().any(|e| e.contains("Unarchived Thread")),
        "expected unarchived thread to appear, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_into_new_workspace_does_not_create_duplicate_real_thread(
    cx: &mut TestAppContext,
) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    cx.run_until_parked();

    let session_id = acp::SessionId::new(Arc::from("restore-into-new-workspace"));
    let path_list_b = PathList::new(&[PathBuf::from("/project-b")]);
    let original_thread_id = ThreadId::new();
    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(
                ThreadMetadata {
                    thread_id: original_thread_id,
                    session_id: Some(session_id.clone()),
                    agent_id: agent::ZED_AGENT_ID.clone(),
                    title: Some("Unarchived Thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: None,
                    interacted_at: None,
                    worktree_paths: WorktreePaths::from_folder_paths(&path_list_b),
                    archived: true,
                    remote_connection: None,
                },
                cx,
            )
        });
    });
    cx.run_until_parked();

    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(original_thread_id)
            .cloned()
            .expect("metadata should exist before unarchive")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });

    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "expected unarchive to open the target workspace"
    );

    let restored_workspace = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces()
            .find(|workspace| PathList::new(&workspace.read(cx).root_paths(cx)) == path_list_b)
            .cloned()
            .expect("expected restored workspace for unarchived thread")
    });
    let restored_panel = restored_workspace.read_with(cx, |workspace, cx| {
        workspace
            .panel::<AgentPanel>(cx)
            .expect("expected unarchive to install an agent panel in the new workspace")
    });

    let restored_thread_id = restored_panel.read_with(cx, |panel, cx| panel.active_thread_id(cx));
    assert_eq!(
        restored_thread_id,
        Some(original_thread_id),
        "expected the new workspace's agent panel to target the restored archived thread id"
    );

    let session_entries = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .filter(|entry| entry.session_id.as_ref() == Some(&session_id))
            .cloned()
            .collect::<Vec<_>>()
    });
    assert_eq!(
        session_entries.len(),
        1,
        "expected exactly one metadata row for restored session after opening a new workspace, got: {session_entries:?}"
    );
    assert_eq!(
        session_entries[0].thread_id, original_thread_id,
        "expected restore into a new workspace to reuse the original thread id"
    );
    assert!(
        !session_entries[0].archived,
        "expected restored thread metadata to be unarchived, got: {:?}",
        session_entries[0]
    );

    let mapped_thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
    });
    assert_eq!(
        mapped_thread_id,
        Some(original_thread_id),
        "expected session mapping to remain stable after opening the new workspace"
    );

    let entries = visible_entries_as_strings(&sidebar, cx);
    let real_thread_rows = entries
        .iter()
        .filter(|entry| !entry.starts_with("v ") && !entry.starts_with("> "))
        .filter(|entry| !entry.contains("Draft"))
        .count();
    assert_eq!(
        real_thread_rows, 1,
        "expected exactly one visible real thread row after restore into a new workspace, got entries: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.contains("Unarchived Thread")),
        "expected restored thread row to be visible, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_into_existing_workspace_replaces_draft(cx: &mut TestAppContext) {
    // When a workspace already exists with an empty draft and a thread
    // is unarchived into it, the draft should be replaced — not kept
    // alongside the loaded thread.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/my-project", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project = project::Project::test(fs.clone(), ["/my-project".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    // Create a thread and send a message so it's no longer a draft.
    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel, connection, cx);
    agent_ui::test_support::send_message(&panel, cx);
    let session_id = agent_ui::test_support::active_session_id(&panel, cx);
    cx.run_until_parked();

    // Archive the thread — the group is left empty (no draft created).
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });
    cx.run_until_parked();

    // Un-archive the thread.
    let thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("thread should exist in store")
    });
    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("metadata should exist")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx.run_until_parked();

    // The draft should be gone — only the unarchived thread remains.
    let entries = visible_entries_as_strings(&sidebar, cx);
    let draft_count = entries.iter().filter(|e| e.contains("Draft")).count();
    assert_eq!(
        draft_count, 0,
        "expected no drafts after unarchiving, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_into_inactive_existing_workspace_does_not_leave_active_draft(
    cx: &mut TestAppContext,
) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        set_max_idle_retained_threads(1, cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let _panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a.clone(), None, window, cx);
    });
    cx.run_until_parked();

    let session_id = acp::SessionId::new(Arc::from("unarchive-into-inactive-existing-workspace"));
    let thread_id = ThreadId::new();
    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(
                ThreadMetadata {
                    thread_id,
                    session_id: Some(session_id.clone()),
                    agent_id: agent::ZED_AGENT_ID.clone(),
                    title: Some("Restored In Inactive Workspace".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: None,
                    interacted_at: None,
                    worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[
                        PathBuf::from("/project-b"),
                    ])),
                    archived: true,
                    remote_connection: None,
                },
                cx,
            )
        });
    });
    cx.run_until_parked();

    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(thread_id)
            .cloned()
            .expect("archived metadata should exist before restore")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });

    let panel_b_before_settle = workspace_b.read_with(cx, |workspace, cx| {
        workspace.panel::<AgentPanel>(cx).expect(
            "target workspace should still have an agent panel immediately after activation",
        )
    });
    let immediate_active_thread_id =
        panel_b_before_settle.read_with(cx, |panel, cx| panel.active_thread_id(cx));

    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id,
            "unarchiving into an inactive existing workspace should end on the restored thread",
        );
    });

    let panel_b = workspace_b.read_with(cx, |workspace, cx| {
        workspace
            .panel::<AgentPanel>(cx)
            .expect("target workspace should still have an agent panel")
    });
    assert_eq!(
        panel_b.read_with(cx, |panel, cx| panel.active_thread_id(cx)),
        Some(thread_id),
        "expected target panel to activate the restored thread id"
    );
    assert!(
        immediate_active_thread_id.is_none() || immediate_active_thread_id == Some(thread_id),
        "expected immediate panel state to be either still loading or already on the restored thread, got active_thread_id={immediate_active_thread_id:?}"
    );

    let entries = visible_entries_as_strings(&sidebar, cx);
    let target_rows: Vec<_> = entries
        .iter()
        .filter(|entry| entry.contains("Restored In Inactive Workspace") || entry.contains("Draft"))
        .cloned()
        .collect();
    assert_eq!(
        target_rows.len(),
        1,
        "expected only the restored row and no surviving draft in the target group, got entries: {entries:?}"
    );
    assert!(
        target_rows[0].contains("Restored In Inactive Workspace"),
        "expected the remaining row to be the restored thread, got entries: {entries:?}"
    );
    assert!(
        !target_rows[0].contains("Draft"),
        "expected no surviving draft row after unarchive into inactive existing workspace, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_after_removing_parent_project_group_restores_real_thread(
    cx: &mut TestAppContext,
) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        set_max_idle_retained_threads(1, cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel_b, connection, cx);
    agent_ui::test_support::send_message(&panel_b, cx);
    let session_id = agent_ui::test_support::active_session_id(&panel_b, cx);
    save_test_thread_metadata(&session_id, &project_b, cx).await;
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });

    cx.run_until_parked();

    let archived_metadata = cx.update(|_, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        let thread_id = store
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("archived thread should still exist in metadata store");
        let metadata = store
            .entry(thread_id)
            .cloned()
            .expect("archived metadata should still exist after archive");
        assert!(
            metadata.archived,
            "thread should be archived before project removal"
        );
        metadata
    });

    let group_key_b =
        project_b.read_with(cx, |project, cx| ProjectGroupKey::from_project(project, cx));
    let remove_task = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.remove_project_group(&group_key_b, window, cx)
    });
    remove_task
        .await
        .expect("remove project group task should complete");
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
        "removing the archived thread's parent project group should remove its workspace"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(archived_metadata.clone(), window, cx);
    });
    cx.run_until_parked();

    let restored_workspace = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces()
            .find(|workspace| {
                PathList::new(&workspace.read(cx).root_paths(cx))
                    == PathList::new(&[PathBuf::from("/project-b")])
            })
            .cloned()
            .expect("expected unarchive to recreate the removed project workspace")
    });
    let restored_panel = restored_workspace.read_with(cx, |workspace, cx| {
        workspace
            .panel::<AgentPanel>(cx)
            .expect("expected restored workspace to bootstrap an agent panel")
    });

    let restored_thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("session should still map to restored thread id")
    });
    assert_eq!(
        restored_panel.read_with(cx, |panel, cx| panel.active_thread_id(cx)),
        Some(restored_thread_id),
        "expected unarchive after project removal to activate the restored real thread"
    );

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_active_thread(
            sidebar,
            &session_id,
            "expected sidebar active entry to track the restored thread after project removal",
        );
    });

    let entries = visible_entries_as_strings(&sidebar, cx);
    let restored_title = archived_metadata.display_title().to_string();
    let matching_rows: Vec<_> = entries
        .iter()
        .filter(|entry| entry.contains(&restored_title) || entry.contains("Draft"))
        .cloned()
        .collect();
    assert_eq!(
        matching_rows.len(),
        1,
        "expected only one restored row and no surviving draft after unarchive following project removal, got entries: {entries:?}"
    );
    assert!(
        !matching_rows[0].contains("Draft"),
        "expected no draft row after unarchive following project removal, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_unarchive_does_not_create_duplicate_real_thread_metadata(cx: &mut TestAppContext) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/my-project", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project = project::Project::test(fs.clone(), ["/my-project".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    cx.run_until_parked();

    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel, connection, cx);
    agent_ui::test_support::send_message(&panel, cx);
    let session_id = agent_ui::test_support::active_session_id(&panel, cx);
    cx.run_until_parked();

    let original_thread_id = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .find(|e| e.session_id.as_ref() == Some(&session_id))
            .map(|e| e.thread_id)
            .expect("thread should exist in store before archiving")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });
    cx.run_until_parked();

    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(original_thread_id)
            .cloned()
            .expect("metadata should exist after archiving")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx.run_until_parked();

    let session_entries = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .filter(|entry| entry.session_id.as_ref() == Some(&session_id))
            .cloned()
            .collect::<Vec<_>>()
    });

    assert_eq!(
        session_entries.len(),
        1,
        "expected exactly one metadata row for the restored session, got: {session_entries:?}"
    );
    assert_eq!(
        session_entries[0].thread_id, original_thread_id,
        "expected unarchive to reuse the original thread id instead of creating a duplicate row"
    );
    assert!(
        session_entries[0].session_id.is_some(),
        "expected restored metadata to be a real thread, got: {:?}",
        session_entries[0]
    );

    let entries = visible_entries_as_strings(&sidebar, cx);
    let real_thread_rows = entries
        .iter()
        .filter(|entry| !entry.starts_with("v ") && !entry.starts_with("> "))
        .filter(|entry| !entry.contains("Draft"))
        // Parked drafts render with the default title until the user types.
        .filter(|entry| !entry.contains(DEFAULT_THREAD_TITLE))
        // Archiving the active thread leaves an empty fallback thread behind.
        .filter(|entry| !entry.contains("New stub Thread"))
        .count();
    assert_eq!(
        real_thread_rows, 1,
        "expected exactly one visible real thread row after unarchive, got entries: {entries:?}"
    );
    assert!(
        !entries.iter().any(|entry| entry.contains("Draft")),
        "expected no draft rows after restoring, got entries: {entries:?}"
    );
}

#[gpui::test]
async fn test_switch_to_workspace_with_archived_thread_shows_no_active_entry(
    cx: &mut TestAppContext,
) {
    // When a thread is archived while the user is in a different workspace,
    // that workspace's panel is cleared. Switching back shows no active entry.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let _panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    // Create a thread in project-a's panel (currently non-active).
    let connection = acp_thread::StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    agent_ui::test_support::open_thread_with_connection(&panel_a, connection, cx);
    agent_ui::test_support::send_message(&panel_a, cx);
    let thread_a = agent_ui::test_support::active_session_id(&panel_a, cx);
    cx.run_until_parked();

    // Archive it while project-b is active.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&thread_a, window, cx);
    });
    cx.run_until_parked();

    // Switch back to project-a. Its panel was cleared during archiving
    // without starting a draft, so nothing is active there.
    let workspace_a =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a.clone(), None, window, cx);
    });
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, _window, cx| {
        sidebar.update_entries(cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _| {
        assert!(
            sidebar.active_entry.is_none(),
            "after switching to workspace with archived thread, nothing should be active"
        );
    });
}

#[gpui::test]
async fn test_archived_threads_excluded_from_sidebar_entries(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_thread_metadata(
        acp::SessionId::new(Arc::from("visible-thread")),
        Some("Visible Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );

    let archived_thread_session_id = acp::SessionId::new(Arc::from("archived-thread"));
    save_thread_metadata(
        archived_thread_session_id.clone(),
        Some("Archived Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );

    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            let thread_id = store
                .entries()
                .find(|e| e.session_id.as_ref() == Some(&archived_thread_session_id))
                .map(|e| e.thread_id)
                .unwrap();
            store.archive(thread_id, None, cx)
        })
    });
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries.iter().any(|e| e.contains("Visible Thread")),
        "expected visible thread in sidebar, got: {entries:?}"
    );
    assert!(
        !entries.iter().any(|e| e.contains("Archived Thread")),
        "expected archived thread to be hidden from sidebar, got: {entries:?}"
    );

    cx.update(|_, cx| {
        let store = ThreadMetadataStore::global(cx);
        let all: Vec<_> = store.read(cx).entries().collect();
        assert_eq!(
            all.len(),
            2,
            "expected 2 total entries in the store, got: {}",
            all.len()
        );

        let archived: Vec<_> = store.read(cx).archived_entries().collect();
        assert_eq!(archived.len(), 1);
        assert_eq!(
            archived[0].session_id.as_ref().unwrap().0.as_ref(),
            "archived-thread"
        );
    });
}

#[gpui::test]
async fn test_archive_last_thread_on_linked_worktree_does_not_create_new_thread_on_worktree(
    cx: &mut TestAppContext,
) {
    // When a linked worktree has a single thread and that thread is archived,
    // the sidebar must NOT create a new thread on the same worktree (which
    // would prevent the worktree from being cleaned up on disk). Instead,
    // archive_thread switches to a sibling thread on the main workspace (or
    // creates a draft there) before archiving the metadata.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-ochre-drift"),
            ref_name: Some("refs/heads/ochre-drift".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project =
        project::Project::test(fs.clone(), ["/wt-ochre-drift".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    // Set up both workspaces with agent panels.
    let main_workspace =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());
    let _main_panel = add_agent_panel(&main_workspace, cx);
    let worktree_panel = add_agent_panel(&worktree_workspace, cx);

    // Activate the linked worktree workspace so the sidebar tracks it.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(worktree_workspace.clone(), None, window, cx);
    });

    // Open a thread in the linked worktree panel and send a message
    // so it becomes the active thread.
    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&worktree_panel, connection, cx);
    send_message(&worktree_panel, cx);

    let worktree_thread_id = active_session_id(&worktree_panel, cx);

    // Save the worktree thread's metadata.
    save_thread_metadata(
        worktree_thread_id.clone(),
        Some("Ochre Drift Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &worktree_project,
        cx,
    );

    // Also save a thread on the main project so there's a sibling in the
    // group that can be selected after archiving.
    save_thread_metadata(
        acp::SessionId::new(Arc::from("main-project-thread")),
        Some("Main Project Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );

    cx.run_until_parked();

    // Verify the linked worktree thread appears with its chip.
    // The live thread title comes from the message text ("Hello"), not
    // the metadata title we saved.
    let entries_before = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries_before
            .iter()
            .any(|s| s.contains("{wt-ochre-drift}")),
        "expected worktree thread with chip before archiving, got: {entries_before:?}"
    );
    assert!(
        entries_before
            .iter()
            .any(|s| s.contains("Main Project Thread")),
        "expected main project thread before archiving, got: {entries_before:?}"
    );

    // Confirm the worktree thread is the active entry.
    sidebar.read_with(cx, |s, _| {
        assert_active_thread(
            s,
            &worktree_thread_id,
            "worktree thread should be active before archiving",
        );
    });

    // Archive the worktree thread — it's the only thread using ochre-drift.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&worktree_thread_id, window, cx);
    });

    cx.run_until_parked();

    // The archived thread should no longer appear in the sidebar.
    let entries_after = visible_entries_as_strings(&sidebar, cx);
    assert!(
        !entries_after
            .iter()
            .any(|s| s.contains("Ochre Drift Thread")),
        "archived thread should be hidden, got: {entries_after:?}"
    );

    // No "+ New Thread" entry should appear with the ochre-drift worktree
    // chip — that would keep the worktree alive and prevent cleanup.
    assert!(
        !entries_after.iter().any(|s| s.contains("{wt-ochre-drift}")),
        "no entry should reference the archived worktree, got: {entries_after:?}"
    );

    // The main project thread should still be visible.
    assert!(
        entries_after
            .iter()
            .any(|s| s.contains("Main Project Thread")),
        "main project thread should still be visible, got: {entries_after:?}"
    );
}

#[gpui::test]
async fn test_unarchive_linked_worktree_thread_into_project_group_shows_only_restored_real_thread(
    cx: &mut TestAppContext,
) {
    // When an archived thread belongs to a linked worktree whose main repo is
    // already open, unarchiving should reopen the linked workspace into the
    // same project group and show only the restored real thread row.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    fs.insert_tree(
        "/wt-ochre-drift",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/ochre-drift",
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-ochre-drift"),
            ref_name: Some("refs/heads/ochre-drift".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project =
        project::Project::test(fs.clone(), ["/wt-ochre-drift".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);
    let main_workspace =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());
    let _main_panel = add_agent_panel(&main_workspace, cx);
    cx.run_until_parked();

    let session_id = acp::SessionId::new(Arc::from("linked-worktree-unarchive"));
    let original_thread_id = ThreadId::new();
    let main_paths = PathList::new(&[PathBuf::from("/project")]);
    let folder_paths = PathList::new(&[PathBuf::from("/wt-ochre-drift")]);

    cx.update(|_, cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(
                ThreadMetadata {
                    thread_id: original_thread_id,
                    session_id: Some(session_id.clone()),
                    agent_id: agent::ZED_AGENT_ID.clone(),
                    title: Some("Unarchived Linked Thread".into()),
                    title_override: None,
                    updated_at: Utc::now(),
                    created_at: None,
                    interacted_at: None,
                    worktree_paths: WorktreePaths::from_path_lists(
                        main_paths.clone(),
                        folder_paths.clone(),
                    )
                    .expect("main and folder paths should be well-formed"),
                    archived: true,
                    remote_connection: None,
                },
                cx,
            )
        });
    });
    cx.run_until_parked();

    let metadata = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry(original_thread_id)
            .cloned()
            .expect("archived linked-worktree metadata should exist before restore")
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_thread_from_archive(metadata, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2,
        "expected unarchive to open the linked worktree workspace into the project group"
    );

    let session_entries = cx.update(|_, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entries()
            .filter(|entry| entry.session_id.as_ref() == Some(&session_id))
            .cloned()
            .collect::<Vec<_>>()
    });
    assert_eq!(
        session_entries.len(),
        1,
        "expected exactly one metadata row for restored linked worktree session, got: {session_entries:?}"
    );
    assert_eq!(
        session_entries[0].thread_id, original_thread_id,
        "expected unarchive to reuse the original linked worktree thread id"
    );
    assert!(
        !session_entries[0].archived,
        "expected restored linked worktree metadata to be unarchived, got: {:?}",
        session_entries[0]
    );

    let assert_no_extra_rows = |entries: &[String]| {
        let real_thread_rows = entries
            .iter()
            .filter(|entry| !entry.starts_with("v ") && !entry.starts_with("> "))
            .filter(|entry| !entry.contains("Draft"))
            .count();
        assert_eq!(
            real_thread_rows, 1,
            "expected exactly one visible real thread row after linked-worktree unarchive, got entries: {entries:?}"
        );
        assert!(
            !entries.iter().any(|entry| entry.contains("Draft")),
            "expected no draft rows after linked-worktree unarchive, got entries: {entries:?}"
        );
        assert!(
            !entries
                .iter()
                .any(|entry| entry.contains(DEFAULT_THREAD_TITLE)),
            "expected no default-titled real placeholder row after linked-worktree unarchive, got entries: {entries:?}"
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.contains("Unarchived Linked Thread")),
            "expected restored linked worktree thread row to be visible, got entries: {entries:?}"
        );
    };

    let entries_after_restore = visible_entries_as_strings(&sidebar, cx);
    assert_no_extra_rows(&entries_after_restore);

    // The reported bug may only appear after an extra scheduling turn.
    cx.run_until_parked();

    let entries_after_extra_turns = visible_entries_as_strings(&sidebar, cx);
    assert_no_extra_rows(&entries_after_extra_turns);
}

#[gpui::test]
async fn test_archive_thread_on_linked_worktree_selects_sibling_thread(cx: &mut TestAppContext) {
    // When a linked worktree thread is archived but the group has other
    // threads (e.g. on the main project), archive_thread should select
    // the nearest sibling.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-ochre-drift"),
            ref_name: Some("refs/heads/ochre-drift".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project =
        project::Project::test(fs.clone(), ["/wt-ochre-drift".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    let main_workspace =
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().next().unwrap().clone());
    let _main_panel = add_agent_panel(&main_workspace, cx);
    let worktree_panel = add_agent_panel(&worktree_workspace, cx);

    // Activate the linked worktree workspace.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(worktree_workspace.clone(), None, window, cx);
    });

    // Open a thread on the linked worktree.
    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&worktree_panel, connection, cx);
    send_message(&worktree_panel, cx);

    let worktree_thread_id = active_session_id(&worktree_panel, cx);

    save_thread_metadata(
        worktree_thread_id.clone(),
        Some("Ochre Drift Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        None,
        None,
        &worktree_project,
        cx,
    );

    // Save a sibling thread on the main project.
    let main_thread_id = acp::SessionId::new(Arc::from("main-project-thread"));
    save_thread_metadata(
        main_thread_id,
        Some("Main Project Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &main_project,
        cx,
    );

    cx.run_until_parked();

    // Confirm the worktree thread is active.
    sidebar.read_with(cx, |s, _| {
        assert_active_thread(
            s,
            &worktree_thread_id,
            "worktree thread should be active before archiving",
        );
    });

    // Archive the worktree thread.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&worktree_thread_id, window, cx);
    });

    cx.run_until_parked();

    // The worktree workspace was removed and a draft was created on the
    // main workspace. No entry should reference the linked worktree.
    let entries_after = visible_entries_as_strings(&sidebar, cx);
    assert!(
        !entries_after.iter().any(|s| s.contains("{wt-ochre-drift}")),
        "no entry should reference the archived worktree, got: {entries_after:?}"
    );

    // The main project thread should still be visible.
    assert!(
        entries_after
            .iter()
            .any(|s| s.contains("Main Project Thread")),
        "main project thread should still be visible, got: {entries_after:?}"
    );
}

#[gpui::test]
async fn test_linked_worktree_workspace_shows_main_worktree_threads(cx: &mut TestAppContext) {
    // When only a linked worktree workspace is open (not the main repo),
    // threads saved against the main repo should still appear in the sidebar.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    // Create the main repo with a linked worktree.
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;

    fs.insert_tree(
        "/wt-feature-a",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        std::path::Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "abc".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    // Only open the linked worktree as a workspace — NOT the main repo.
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
        MultiWorkspace::test_new(worktree_project.clone(), window, cx)
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Save a thread against the MAIN repo path.
    save_named_thread_metadata("main-thread", "Main Repo Thread", &main_project, cx).await;

    // Save a thread against the linked worktree path.
    save_named_thread_metadata("wt-thread", "Worktree Thread", &worktree_project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Both threads should be visible: the worktree thread by direct lookup,
    // and the main repo thread because the workspace is a linked worktree
    // and we also query the main repo path.
    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries.iter().any(|e| e.contains("Main Repo Thread")),
        "expected main repo thread to be visible in linked worktree workspace, got: {entries:?}"
    );
    assert!(
        entries.iter().any(|e| e.contains("Worktree Thread")),
        "expected worktree thread to be visible, got: {entries:?}"
    );
}

async fn init_multi_project_test(
    paths: &[&str],
    cx: &mut TestAppContext,
) -> (Arc<FakeFs>, Entity<project::Project>) {
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        set_max_idle_retained_threads(1, cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });
    let fs = FakeFs::new(cx.executor());
    for path in paths {
        fs.insert_tree(path, serde_json::json!({ ".git": {}, "src": {} }))
            .await;
    }
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project =
        project::Project::test(fs.clone() as Arc<dyn fs::Fs>, [paths[0].as_ref()], cx).await;
    (fs, project)
}

async fn add_test_project(
    path: &str,
    fs: &Arc<FakeFs>,
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut gpui::VisualTestContext,
) -> Entity<Workspace> {
    let project = project::Project::test(fs.clone() as Arc<dyn fs::Fs>, [path.as_ref()], cx).await;
    let workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project, window, cx)
    });
    cx.run_until_parked();
    workspace
}

#[gpui::test]
async fn test_workspace_lifecycle_retains_projects_when_sidebar_is_closed(cx: &mut TestAppContext) {
    let (fs, project_a) =
        init_multi_project_test(&["/project-a", "/project-b", "/project-c"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let _sidebar = setup_sidebar_closed(&multi_workspace, cx);

    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    assert!(!multi_workspace.read_with(cx, |mw, _| mw.sidebar_open()));
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_a));

    let workspace_b = add_test_project("/project-b", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_b));
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_a)));

    let workspace_c = add_test_project("/project-c", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        3
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_c));
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_b)));
}

#[gpui::test]
async fn test_workspaces_remain_retained_after_sidebar_closes(cx: &mut TestAppContext) {
    let (fs, project_a) = init_multi_project_test(
        &["/project-a", "/project-b", "/project-c", "/project-d"],
        cx,
    )
    .await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);
    assert!(multi_workspace.read_with(cx, |mw, _| mw.sidebar_open()));
    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let workspace_b = add_test_project("/project-b", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a, None, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_b)));

    multi_workspace.update_in(cx, |mw, window, cx| mw.close_sidebar(window, cx));
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );

    let workspace_c = add_test_project("/project-c", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        3
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_c));

    let workspace_d = add_test_project("/project-d", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        4
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_d));
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_c)));
}

#[gpui::test]
async fn test_sidebar_opening_keeps_existing_retained_workspaces(cx: &mut TestAppContext) {
    let (fs, project_a) =
        init_multi_project_test(&["/project-a", "/project-b", "/project-c"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    setup_sidebar_closed(&multi_workspace, cx);

    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let workspace_b = add_test_project("/project-b", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_b));
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_a)));

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.toggle_sidebar(window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_b)));

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.toggle_sidebar(window, cx);
    });

    let workspace_c = add_test_project("/project-c", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        3
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_c));
}

#[gpui::test]
async fn test_legacy_thread_with_canonical_path_opens_main_repo_workspace(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;

    fs.insert_tree(
        "/wt-feature-a",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "abc".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    // Only a linked worktree workspace is open — no workspace for /project.
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
        MultiWorkspace::test_new(worktree_project.clone(), window, cx)
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Save a legacy thread: folder_paths = main repo, main_worktree_paths = empty.
    let legacy_session = acp::SessionId::new(Arc::from("legacy-main-thread"));
    cx.update(|_, cx| {
        let metadata = ThreadMetadata {
            thread_id: ThreadId::new(),
            session_id: Some(legacy_session.clone()),
            agent_id: agent::ZED_AGENT_ID.clone(),
            title: Some("Legacy Main Thread".into()),
            title_override: None,
            updated_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
            created_at: None,
            interacted_at: None,
            worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
                "/project",
            )])),
            archived: false,
            remote_connection: None,
        };
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // The legacy thread should appear in the sidebar under the project group.
    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        entries.iter().any(|e| e.contains("Legacy Main Thread")),
        "legacy thread should be visible: {entries:?}",
    );

    // Verify only 1 workspace before clicking.
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1,
    );

    // Focus and select the legacy thread, then confirm.
    focus_sidebar(&sidebar, cx);
    let thread_index = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|e| e.session_id().is_some_and(|id| id == &legacy_session))
            .expect("legacy thread should be in entries")
    });
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(thread_index);
    });
    cx.dispatch_action(Confirm);
    cx.run_until_parked();

    let new_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let new_path_list =
        new_workspace.read_with(cx, |_, cx| workspace_path_list(&new_workspace, cx));
    assert_eq!(
        new_path_list,
        PathList::new(&[PathBuf::from("/project")]),
        "the new workspace should be for the main repo, not the linked worktree",
    );
}

#[gpui::test]
async fn test_linked_worktree_workspace_reachable_after_adding_unrelated_project(
    cx: &mut TestAppContext,
) {
    // Regression test for a property-test finding:
    //   AddLinkedWorktree { project_group_index: 0 }
    //   AddProject { use_worktree: true }
    //   AddProject { use_worktree: false }
    // After these three steps, the linked-worktree workspace was not
    // reachable from any sidebar entry.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);

        cx.observe_new(
            |workspace: &mut Workspace,
             window: Option<&mut Window>,
             cx: &mut gpui::Context<Workspace>| {
                if let Some(window) = window {
                    let panel = cx.new(|cx| AgentPanel::test_new(workspace, window, cx));
                    workspace.add_panel(panel, window, cx);
                }
            },
        )
        .detach();
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/my-project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project =
        project::Project::test(fs.clone() as Arc<dyn fs::Fs>, ["/my-project".as_ref()], cx).await;
    project.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Step 1: Create a linked worktree for the main project.
    let worktree_name = "wt-0";
    let worktree_path = "/worktrees/wt-0";

    fs.insert_tree(
        worktree_path,
        serde_json::json!({
            ".git": "gitdir: /my-project/.git/worktrees/wt-0",
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/my-project/.git/worktrees/wt-0",
        serde_json::json!({
            "commondir": "../../",
            "HEAD": "ref: refs/heads/wt-0",
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/my-project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from(worktree_path),
            ref_name: Some(format!("refs/heads/{}", worktree_name).into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    let main_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let main_project = main_workspace.read_with(cx, |ws, _| ws.project().clone());
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    cx.run_until_parked();

    // Step 2: Open the linked worktree as its own workspace.
    let worktree_project =
        project::Project::test(fs.clone() as Arc<dyn fs::Fs>, [worktree_path.as_ref()], cx).await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    let worktree_workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });
    cx.run_until_parked();

    // Step 3: Add an unrelated project.
    fs.insert_tree(
        "/other-project",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    let other_project = project::Project::test(
        fs.clone() as Arc<dyn fs::Fs>,
        ["/other-project".as_ref()],
        cx,
    )
    .await;
    other_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(other_project.clone(), window, cx);
    });
    cx.run_until_parked();

    // Force a full sidebar rebuild with all groups expanded.
    sidebar.update_in(cx, |sidebar, _window, cx| {
        if let Some(multi_workspace) = sidebar.multi_workspace.upgrade() {
            multi_workspace.update(cx, |multi_workspace, cx| {
                multi_workspace.test_expand_all_groups(cx)
            });
        }
        sidebar.update_entries(cx);
    });
    cx.run_until_parked();

    // The linked-worktree workspace must be reachable from at least one
    // sidebar entry — otherwise the user has no way to navigate to it.
    let worktree_ws_id = worktree_workspace.entity_id();
    let (all_ids, reachable_ids) = sidebar.read_with(cx, |sidebar, cx| {
        let mw = multi_workspace.read(cx);

        let all: HashSet<gpui::EntityId> = mw.workspaces().map(|ws| ws.entity_id()).collect();
        let reachable: HashSet<gpui::EntityId> = sidebar
            .contents
            .entries
            .iter()
            .flat_map(|entry| entry.reachable_workspaces(mw, cx))
            .map(|ws| ws.entity_id())
            .collect();
        (all, reachable)
    });

    let unreachable = &all_ids - &reachable_ids;
    eprintln!("{}", visible_entries_as_strings(&sidebar, cx).join("\n"));

    assert!(
        unreachable.is_empty(),
        "workspaces not reachable from any sidebar entry: {:?}\n\
         (linked-worktree workspace id: {:?})",
        unreachable,
        worktree_ws_id,
    );
}

#[gpui::test]
async fn test_startup_failed_restoration_shows_no_draft(cx: &mut TestAppContext) {
    // Empty project groups no longer auto-create drafts via reconciliation.
    // A fresh startup with no restorable thread should show only the header.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    let _workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let entries = visible_entries_as_strings(&sidebar, cx);
    assert_eq!(
        entries,
        vec!["v [my-project]"],
        "empty group should show only the header, no auto-created draft"
    );
}

#[gpui::test]
async fn test_startup_successful_restoration_no_spurious_draft(cx: &mut TestAppContext) {
    // Rule 5: When the app starts and the AgentPanel successfully loads
    // a thread, no spurious draft should appear.
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Create and send a message to make a real thread.
    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel, connection, cx);
    send_message(&panel, cx);
    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &project, cx).await;
    cx.run_until_parked();

    // Should show the thread, NOT a spurious draft.
    let entries = visible_entries_as_strings(&sidebar, cx);
    assert_eq!(entries, vec!["v [my-project]", "  Hello *"]);

    // active_entry should be Thread, not Draft.
    sidebar.read_with(cx, |sidebar, _| {
        assert_active_thread(sidebar, &session_id, "should be on the thread, not a draft");
    });
}

#[gpui::test]
async fn test_project_header_click_restores_last_viewed(cx: &mut TestAppContext) {
    // Rule 9: Clicking a project header should restore whatever the
    // user was last looking at in that group, not create new drafts
    // or jump to the first entry.
    let project_a = init_test_project_with_agent_panel("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let (sidebar, panel_a) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    // Create two threads in project-a.
    let conn1 = StubAgentConnection::new();
    conn1.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel_a, conn1, cx);
    send_message(&panel_a, cx);
    let thread_a1 = active_session_id(&panel_a, cx);
    save_test_thread_metadata(&thread_a1, &project_a, cx).await;

    let conn2 = StubAgentConnection::new();
    conn2.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Done".into()),
    )]);
    open_thread_with_connection(&panel_a, conn2, cx);
    send_message(&panel_a, cx);
    let thread_a2 = active_session_id(&panel_a, cx);
    save_test_thread_metadata(&thread_a2, &project_a, cx).await;
    cx.run_until_parked();

    // The user is now looking at thread_a2.
    sidebar.read_with(cx, |sidebar, _| {
        assert_active_thread(sidebar, &thread_a2, "should be on thread_a2");
    });

    // Add project-b and switch to it.
    let fs = cx.update(|_window, cx| <dyn fs::Fs>::global(cx));
    fs.as_fake()
        .insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    let project_b =
        project::Project::test(fs.clone() as Arc<dyn Fs>, ["/project-b".as_ref()], cx).await;
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let _panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    // Now switch BACK to project-a by activating its workspace.
    let workspace_a = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspaces()
            .find(|ws| {
                ws.read(cx)
                    .project()
                    .read(cx)
                    .visible_worktrees(cx)
                    .any(|wt| {
                        wt.read(cx)
                            .abs_path()
                            .to_string_lossy()
                            .contains("project-a")
                    })
            })
            .unwrap()
            .clone()
    });
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a.clone(), None, window, cx);
    });
    cx.run_until_parked();

    // The panel should still show thread_a2 (the last thing the user
    // was viewing in project-a), not a draft or thread_a1.
    sidebar.read_with(cx, |sidebar, _| {
        assert_active_thread(
            sidebar,
            &thread_a2,
            "switching back to project-a should restore thread_a2",
        );
    });

    // No spurious draft entries should have been created in
    // project-a's group (project-b may have a placeholder).
    let entries = visible_entries_as_strings(&sidebar, cx);
    // Find project-a's section and check it has no drafts.
    let project_a_start = entries
        .iter()
        .position(|e| e.contains("project-a"))
        .unwrap();
    let project_a_end = entries[project_a_start + 1..]
        .iter()
        .position(|e| e.starts_with("v "))
        .map(|i| i + project_a_start + 1)
        .unwrap_or(entries.len());
    let project_a_drafts = entries[project_a_start..project_a_end]
        .iter()
        .filter(|e| e.contains("Draft"))
        .count();
    assert_eq!(
        project_a_drafts, 0,
        "switching back to project-a should not create drafts in its group"
    );
}

#[gpui::test]
async fn test_activating_workspace_with_draft_does_not_create_extras(cx: &mut TestAppContext) {
    // When a workspace has a draft (from the panel's load fallback)
    // and the user activates it (e.g. by clicking the placeholder or
    // the project header), no extra drafts should be created.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a =
        project::Project::test(fs.clone() as Arc<dyn Fs>, ["/project-a".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let _panel_a = add_agent_panel(&workspace_a, cx);
    cx.run_until_parked();

    // Add project-b with its own workspace and agent panel.
    let project_b =
        project::Project::test(fs.clone() as Arc<dyn Fs>, ["/project-b".as_ref()], cx).await;
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });
    let _panel_b = add_agent_panel(&workspace_b, cx);
    cx.run_until_parked();

    // Explicitly create a draft on workspace_b so the sidebar tracks one.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_new_thread(&workspace_b, window, cx);
    });
    cx.run_until_parked();

    // Count project-b's drafts.
    let count_b_drafts = |cx: &mut gpui::VisualTestContext| {
        let entries = visible_entries_as_strings(&sidebar, cx);
        entries
            .iter()
            .skip_while(|e| !e.contains("project-b"))
            .take_while(|e| !e.starts_with("v ") || e.contains("project-b"))
            .filter(|e| e.contains("Draft"))
            .count()
    };
    let drafts_before = count_b_drafts(cx);

    // Switch away from project-b, then back.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a.clone(), None, window, cx);
    });
    cx.run_until_parked();
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_b.clone(), None, window, cx);
    });
    cx.run_until_parked();

    let drafts_after = count_b_drafts(cx);
    assert_eq!(
        drafts_before, drafts_after,
        "activating workspace should not create extra drafts"
    );

    // The draft should be highlighted as active after switching back.
    sidebar.read_with(cx, |sidebar, _| {
        assert_active_draft(
            sidebar,
            &workspace_b,
            "draft should be active after switching back to its workspace",
        );
    });
}

#[gpui::test]
async fn test_unused_legacy_delta_headers_follow_their_threads(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let clone = |name: &str| {
        serde_json::json!({
            ".dotfiles.git": {
                "HEAD": "ref: refs/heads/feature\n",
                "config": "[core]\n\tbare = false\n",
                "delta": { "copy-baseline": "" },
                "objects": {
                    "info": {
                        "alternates": format!("{}\n", util::path!("/.dotfiles/.git/objects")),
                    },
                },
            },
            "name": name,
        })
    };
    let checkout = |name: &str| {
        serde_json::json!({
            ".dotfiles": {
                ".git": format!(
                    "gitdir: {}\n",
                    PathBuf::from(util::path!("/.dotfiles/.delta/clones"))
                        .join(name)
                        .join(".dotfiles.git")
                        .display()
                ),
            },
        })
    };
    fs.insert_tree(
        util::path!("/.dotfiles"),
        serde_json::json!({
            ".git": { "HEAD": "ref: refs/heads/main\n" },
            ".delta": {
                "clones": { "one": clone("one"), "two": clone("two"), "three": clone("three") },
                "worktrees": {
                    "one": checkout("one"),
                    "two": checkout("two"),
                    "three": checkout("three"),
                },
            },
        }),
    )
    .await;
    fs.insert_tree(util::path!("/other"), serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let legacy_key_path =
        |name: &str| PathBuf::from(util::path!("/.dotfiles/.delta/clones")).join(name);
    let checkout_path = |name: &str| {
        PathBuf::from(util::path!("/.dotfiles/.delta/worktrees"))
            .join(name)
            .join(".dotfiles")
    };
    for (session_id, folder_path, main_path, minute) in [
        ("one", checkout_path("one"), legacy_key_path("one"), 3),
        ("two", checkout_path("two"), legacy_key_path("two"), 2),
        (
            "container",
            legacy_key_path("two"),
            legacy_key_path("two"),
            1,
        ),
    ] {
        save_thread_metadata_with_main_paths(
            session_id,
            session_id,
            PathList::new(&[folder_path]),
            PathList::new(&[main_path]),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, minute, 0).unwrap(),
            cx,
        );
    }
    let project = project::Project::test(fs.clone(), [Path::new(util::path!("/other"))], cx).await;
    let mut open_checkout_projects = Vec::new();
    for name in ["one", "three"] {
        let open_checkout_project =
            project::Project::test(fs.clone(), [checkout_path(name).as_path()], cx).await;
        open_checkout_project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        open_checkout_projects.push(open_checkout_project);
    }
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        for open_checkout_project in open_checkout_projects {
            multi_workspace.test_add_workspace(open_checkout_project, window, cx);
        }
        multi_workspace.restore_project_groups(
            ["one", "two", "three"]
                .map(|name| workspace::SerializedProjectGroupState {
                    key: ProjectGroupKey::new(None, PathList::new(&[legacy_key_path(name)])),
                    expanded: true,
                    custom_name: (name == "one").then(|| "My Delta project".into()),
                })
                .into(),
            cx,
        );
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);

    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            "v [two]",
            "  two {.dotfiles}",
            "  container",
            "v [My Delta project]",
            "  one {one}",
            "v [other]",
        ],
        "rows of an open checkout join its source and its unused old header goes; \
         rows of a closed checkout stay under their old header"
    );

    multi_workspace
        .update_in(cx, |multi_workspace, window, cx| {
            multi_workspace.remove_project_group(
                &ProjectGroupKey::new(None, PathList::new(&[Path::new(util::path!("/.dotfiles"))])),
                window,
                cx,
            )
        })
        .await
        .unwrap();
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [two]", "  two {.dotfiles}", "  container", "v [other]"]
    );

    let checkout_project =
        project::Project::test(fs.clone(), [checkout_path("two").as_path()], cx).await;
    checkout_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(checkout_project, window, cx);
        multi_workspace.test_add_project_group(
            workspace::ProjectGroup {
                key: ProjectGroupKey::new(None, PathList::new(&[legacy_key_path("one")])),
                workspaces: Vec::new(),
                expanded: true,
            },
            cx,
        );
    });
    save_terminal_metadata_with_main_paths(
        "container terminal",
        PathList::new(&[legacy_key_path("one")]),
        PathList::new(&[legacy_key_path("one")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 4, 0).unwrap(),
        cx,
    );
    let entries_with_loaded_stores = updated_entries(&sidebar, cx);
    let mut entries_after_each_load = Vec::new();
    let mut legacy_groups_before_the_stores_load = Vec::new();
    for reload_thread_store in [true, false] {
        cx.update(|_window, cx| {
            if reload_thread_store {
                ThreadMetadataStore::init_global(cx);
            } else {
                TerminalThreadMetadataStore::init_global(cx);
            }
            sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
        });
        legacy_groups_before_the_stores_load.push(multi_workspace.read_with(
            cx,
            |multi_workspace, cx| {
                ["one", "two"].map(|name| {
                    multi_workspace
                        .project_group_keys(cx)
                        .contains(&ProjectGroupKey::new(
                            None,
                            PathList::new(&[legacy_key_path(name)]),
                        ))
                })
            },
        ));
        entries_after_each_load.push(updated_entries(&sidebar, cx));
    }
    assert_eq!(
        (
            legacy_groups_before_the_stores_load,
            entries_after_each_load,
        ),
        (
            vec![[true, true], [true, true]],
            vec![
                entries_with_loaded_stores.clone(),
                entries_with_loaded_stores
            ],
        ),
        "a store that has not loaded yet is not evidence that a group is unused"
    );
}

#[gpui::test]
async fn test_delta_checkout_groups_with_its_source(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let source_path = Path::new(util::path!("/repo"));
    let next_source_path = Path::new(util::path!("/next"));
    let first_clone_path = Path::new(util::path!("/repo/.delta/clones/abc/repo.git"));
    let first_checkout_path = Path::new(util::path!("/repo/.delta/worktrees/abc/repo"));
    let second_clone_path = Path::new(util::path!("/repo/.delta/clones/def/repo.git"));
    let second_checkout_path = Path::new(util::path!("/repo/.delta/worktrees/def/repo"));
    let delta_clone = || {
        serde_json::json!({
            "HEAD": "ref: refs/heads/feature\n",
            "config": "[core]\n\tbare = false\n",
            "delta": { "copy-baseline": "" },
            "objects": {
                "info": {
                    "alternates": format!("{}\n", util::path!("/repo/.git/objects")),
                },
            },
        })
    };
    fs.insert_tree(
        source_path,
        serde_json::json!({
            ".git": { "HEAD": "ref: refs/heads/main\n" },
            ".delta": {
                "clones": {
                    "abc": { "repo.git": delta_clone() },
                    "def": { "repo.git": delta_clone() },
                },
                "worktrees": {
                    "abc": {
                        "repo": { ".git": format!("gitdir: {}\n", first_clone_path.display()) },
                    },
                    "def": {
                        "repo": { ".git": format!("gitdir: {}\n", second_clone_path.display()) },
                    },
                },
            },
        }),
    )
    .await;
    fs.insert_tree(
        next_source_path,
        serde_json::json!({ ".git": { "HEAD": "ref: refs/heads/main\n" } }),
    )
    .await;
    save_thread_metadata_with_main_paths(
        "First",
        "First",
        PathList::new(&[first_checkout_path]),
        PathList::new(&[source_path]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    let source_project = project::Project::test(fs.clone(), [source_path], cx).await;
    let first_project = project::Project::test(fs.clone(), [first_checkout_path], cx).await;
    let second_project = project::Project::test(fs.clone(), [second_checkout_path], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(source_project, window, cx));
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.retain_active_workspace(cx);
        multi_workspace.test_add_workspace(first_project, window, cx);
        multi_workspace.test_add_workspace(second_project, window, cx);
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let group_paths = |cx: &mut gpui::VisualTestContext| {
        cx.run_until_parked();
        multi_workspace.read_with(cx, |multi_workspace, cx| {
            multi_workspace
                .project_group_keys(cx)
                .into_iter()
                .map(|key| key.path_list().clone())
                .collect::<Vec<_>>()
        })
    };
    let first_main_paths = |cx: &mut gpui::VisualTestContext| {
        cx.run_until_parked();
        cx.update(|_, cx| {
            ThreadMetadataStore::global(cx)
                .read(cx)
                .entries()
                .find(|thread| thread.display_title().as_ref() == "First")
                .map(|thread| thread.main_worktree_paths().paths().to_vec())
        })
    };

    let group_paths_with_source = group_paths(cx);
    let entries_with_source = visible_entries_as_strings(&sidebar, cx);
    let mut menu_labels = cx.update(|_, cx| {
        multi_workspace
            .read(cx)
            .workspaces()
            .flat_map(|workspace| workspace_menu_worktree_labels(workspace, cx))
            .map(|label| label.primary_name.to_string())
            .collect::<Vec<_>>()
    });
    menu_labels.sort();

    fs.insert_file(
        first_clone_path.join("objects/info/alternates"),
        format!("{}\n", util::path!("/next/.git/objects")).into_bytes(),
    )
    .await;
    let group_paths_with_next_source = group_paths(cx);
    let first_main_paths_with_next_source = first_main_paths(cx);
    fs.remove_file(
        &first_clone_path.join("delta/copy-baseline"),
        fs::RemoveOptions::default(),
    )
    .await
    .unwrap();

    assert_eq!(
        (
            group_paths_with_source,
            entries_with_source,
            menu_labels,
            group_paths_with_next_source,
            first_main_paths_with_next_source,
            group_paths(cx),
            first_main_paths(cx),
        ),
        (
            vec![PathList::new(&[source_path])],
            vec!["v [repo]".to_string(), "  First {abc}".to_string()],
            vec!["abc".to_string(), "def".to_string(), "main".to_string()],
            vec![
                PathList::new(&[source_path]),
                PathList::new(&[next_source_path]),
            ],
            Some(vec![next_source_path.to_path_buf()]),
            vec![
                PathList::new(&[source_path]),
                PathList::new(&[first_clone_path]),
            ],
            Some(vec![first_clone_path.to_path_buf()]),
        )
    );
}

#[gpui::test]
async fn test_non_archive_thread_paths_migrate_on_worktree_add_and_remove(cx: &mut TestAppContext) {
    // Historical threads (not open in any agent panel) should have their
    // worktree paths updated when a folder is added to or removed from the
    // project.
    let (_fs, project) = init_multi_project_test(&["/project-a", "/project-b"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Save two threads directly into the metadata store (not via the agent
    // panel), so they are purely historical — no open views hold them.
    // Use different timestamps so sort order is deterministic.
    save_thread_metadata(
        acp::SessionId::new(Arc::from("hist-1")),
        Some("Historical 1".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    save_thread_metadata(
        acp::SessionId::new(Arc::from("hist-2")),
        Some("Historical 2".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 1).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    // Sanity-check: both threads exist under the initial key [/project-a].
    let old_key_paths = PathList::new(&[PathBuf::from("/project-a")]);
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        assert_eq!(
            store
                .entries_for_main_worktree_path(&old_key_paths, None)
                .count(),
            2,
            "should have 2 historical threads under old key before worktree add"
        );
    });

    // Add a second worktree to the project.
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project-b", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();

    // The historical threads should now be indexed under the new combined
    // key [/project-a, /project-b].
    let new_key_paths = PathList::new(&[PathBuf::from("/project-a"), PathBuf::from("/project-b")]);
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        assert_eq!(
            store
                .entries_for_main_worktree_path(&old_key_paths, None)
                .count(),
            0,
            "should have 0 historical threads under old key after worktree add"
        );
        assert_eq!(
            store
                .entries_for_main_worktree_path(&new_key_paths, None)
                .count(),
            2,
            "should have 2 historical threads under new key after worktree add"
        );
    });

    // Sidebar should show threads under the new header.
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            "v [project-a / project-b]",
            "  Historical 2",
            "  Historical 1",
        ]
    );
    assert_eq!(
        visible_entries_with_worktree_headers(&sidebar, cx),
        vec![
            "v [project-a / project-b]",
            "  ~ project-a",
            "  Historical 2",
            "  Historical 1",
        ],
        "a multi-root project has one worktree group, headed by its first root"
    );

    // Now remove the second worktree.
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .find(|wt| wt.read(cx).abs_path().as_ref() == Path::new("/project-b"))
            .map(|wt| wt.read(cx).id())
            .expect("should find project-b worktree")
    });
    project.update(cx, |project, cx| {
        project.remove_worktree(worktree_id, cx);
    });
    cx.run_until_parked();

    // Historical threads should migrate back to the original key.
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        assert_eq!(
            store
                .entries_for_main_worktree_path(&new_key_paths, None)
                .count(),
            0,
            "should have 0 historical threads under new key after worktree remove"
        );
        assert_eq!(
            store
                .entries_for_main_worktree_path(&old_key_paths, None)
                .count(),
            2,
            "should have 2 historical threads under old key after worktree remove"
        );
    });

    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [project-a]", "  Historical 2", "  Historical 1",]
    );
}

#[gpui::test]
async fn test_worktree_add_only_regroups_threads_for_changed_workspace(cx: &mut TestAppContext) {
    // When two workspaces share the same project group (same main path)
    // but have different folder paths (main repo vs linked worktree),
    // adding a worktree to the main workspace should regroup only that
    // workspace and its threads into the new project group. Threads for the
    // linked worktree workspace should remain under the original group.
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        set_max_idle_retained_threads(1, cx);
        ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature"),
            ref_name: Some("refs/heads/feature".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    // Workspace A: main repo at /project.
    let main_project =
        project::Project::test(fs.clone() as Arc<dyn fs::Fs>, ["/project".as_ref()], cx).await;
    // Workspace B: linked worktree of the same repo (same group, different folder).
    let worktree_project =
        project::Project::test(fs.clone() as Arc<dyn fs::Fs>, ["/wt-feature".as_ref()], cx).await;

    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx);
    });
    cx.run_until_parked();

    // Save a thread for each workspace's folder paths.
    let time_main = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 1).unwrap();
    let time_wt = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 2).unwrap();
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-main")),
        Some("Main Thread".into()),
        time_main,
        Some(time_main),
        None,
        &main_project,
        cx,
    );
    save_thread_metadata(
        acp::SessionId::new(Arc::from("thread-wt")),
        Some("Worktree Thread".into()),
        time_wt,
        Some(time_wt),
        None,
        &worktree_project,
        cx,
    );
    cx.run_until_parked();

    let folder_paths_main = PathList::new(&[PathBuf::from("/project")]);
    let folder_paths_wt = PathList::new(&[PathBuf::from("/wt-feature")]);

    // Sanity-check: each thread is indexed under its own folder paths, but
    // both appear under the shared sidebar group keyed by the main worktree.
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        assert_eq!(
            store.entries_for_path(&folder_paths_main, None).count(),
            1,
            "one thread under [/project]"
        );
        assert_eq!(
            store.entries_for_path(&folder_paths_wt, None).count(),
            1,
            "one thread under [/wt-feature]"
        );
    });
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
    assert_eq!(
        visible_entries_with_worktree_headers(&sidebar, cx),
        vec![
            "v [project]",
            "  ~ project",
            "  Main Thread",
            "  ~ wt-feature",
            "  Worktree Thread {wt-feature}",
        ]
    );

    // Add /project-b to the main project only.
    main_project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project-b", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();

    // Main Thread (folder paths [/project]) should be regrouped to
    // [/project, /project-b]. Worktree Thread should remain under the
    // original [/project] group.
    let folder_paths_main_b =
        PathList::new(&[PathBuf::from("/project"), PathBuf::from("/project-b")]);
    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx).read(cx);
        assert_eq!(
            store.entries_for_path(&folder_paths_main, None).count(),
            0,
            "main thread should no longer be under old folder paths [/project]"
        );
        assert_eq!(
            store.entries_for_path(&folder_paths_main_b, None).count(),
            1,
            "main thread should now be under [/project, /project-b]"
        );
        assert_eq!(
            store.entries_for_path(&folder_paths_wt, None).count(),
            1,
            "worktree thread should remain unchanged under [/wt-feature]"
        );
    });

    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            "v [project]",
            "  Worktree Thread {wt-feature}",
            "v [project / project-b]",
            "  Main Thread",
        ]
    );
}

#[gpui::test]
async fn test_linked_worktree_workspace_reachable_after_adding_worktree_to_project(
    cx: &mut TestAppContext,
) {
    // When a linked worktree is opened as its own workspace and then a new
    // folder is added to the main project group, the linked worktree
    // workspace must still be reachable from some sidebar entry.
    let (_fs, project) = init_multi_project_test(&["/my-project"], cx).await;
    let fs = _fs.clone();

    // Set up git worktree infrastructure.
    fs.insert_tree(
        "/my-project/.git/worktrees/wt-0",
        serde_json::json!({
            "commondir": "../../",
            "HEAD": "ref: refs/heads/wt-0",
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/wt-0",
        serde_json::json!({
            ".git": "gitdir: /my-project/.git/worktrees/wt-0",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/my-project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/wt-0"),
            ref_name: Some("refs/heads/wt-0".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    // Re-scan so the main project discovers the linked worktree.
    project.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Open the linked worktree as its own workspace.
    let worktree_project = project::Project::test(
        fs.clone() as Arc<dyn fs::Fs>,
        ["/worktrees/wt-0".as_ref()],
        cx,
    )
    .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx);
    });
    cx.run_until_parked();

    // Both workspaces should be reachable.
    let workspace_count = multi_workspace.read_with(cx, |mw, _| mw.workspaces().count());
    assert_eq!(workspace_count, 2, "should have 2 workspaces");

    // Add a new folder to the main project, changing the project group key.
    fs.insert_tree(
        "/other-project",
        serde_json::json!({ ".git": {}, "src": {} }),
    )
    .await;
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/other-project", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    // The linked worktree workspace must still be reachable.
    let entries = visible_entries_as_strings(&sidebar, cx);
    let mw_workspaces: Vec<_> = multi_workspace.read_with(cx, |mw, _| {
        mw.workspaces().map(|ws| ws.entity_id()).collect()
    });
    sidebar.read_with(cx, |sidebar, cx| {
        let multi_workspace = multi_workspace.read(cx);
        let reachable: std::collections::HashSet<gpui::EntityId> = sidebar
            .contents
            .entries
            .iter()
            .flat_map(|entry| entry.reachable_workspaces(multi_workspace, cx))
            .map(|ws| ws.entity_id())
            .collect();
        let all: std::collections::HashSet<gpui::EntityId> =
            mw_workspaces.iter().copied().collect();
        let unreachable = &all - &reachable;
        assert!(
            unreachable.is_empty(),
            "all workspaces should be reachable after adding folder; \
             unreachable: {:?}, entries: {:?}",
            unreachable,
            entries,
        );
    });
}

mod property_test {
    use super::*;
    use gpui::proptest::prelude::*;

    struct UnopenedWorktree {
        path: String,
        main_workspace_path: String,
    }

    struct TestState {
        fs: Arc<FakeFs>,
        thread_counter: u32,
        workspace_counter: u32,
        worktree_counter: u32,
        saved_thread_ids: Vec<acp::SessionId>,
        unopened_worktrees: Vec<UnopenedWorktree>,
    }

    impl TestState {
        fn new(fs: Arc<FakeFs>) -> Self {
            Self {
                fs,
                thread_counter: 0,
                workspace_counter: 1,
                worktree_counter: 0,
                saved_thread_ids: Vec::new(),
                unopened_worktrees: Vec::new(),
            }
        }

        fn next_metadata_only_thread_id(&mut self) -> acp::SessionId {
            let id = self.thread_counter;
            self.thread_counter += 1;
            acp::SessionId::new(Arc::from(format!("prop-thread-{id}")))
        }

        fn next_workspace_path(&mut self) -> String {
            let id = self.workspace_counter;
            self.workspace_counter += 1;
            format!("/prop-project-{id}")
        }

        fn next_worktree_name(&mut self) -> String {
            let id = self.worktree_counter;
            self.worktree_counter += 1;
            format!("wt-{id}")
        }
    }

    #[derive(Debug)]
    enum Operation {
        SaveThread { project_group_index: usize },
        SaveWorktreeThread { worktree_index: usize },
        ToggleAgentPanel,
        CreateDraftThread,
        AddProject { use_worktree: bool },
        ArchiveThread { index: usize },
        SwitchToThread { index: usize },
        SwitchToProjectGroup { index: usize },
        AddLinkedWorktree { project_group_index: usize },
        AddWorktreeToProject { project_group_index: usize },
        RemoveWorktreeFromProject { project_group_index: usize },
    }

    // Distribution (out of 24 slots):
    //   SaveThread:                5 slots (~21%)
    //   SaveWorktreeThread:        2 slots (~8%)
    //   ToggleAgentPanel:          1 slot  (~4%)
    //   CreateDraftThread:         1 slot  (~4%)
    //   AddProject:                1 slot  (~4%)
    //   ArchiveThread:             2 slots (~8%)
    //   SwitchToThread:            2 slots (~8%)
    //   SwitchToProjectGroup:      2 slots (~8%)
    //   AddLinkedWorktree:         4 slots (~17%)
    //   AddWorktreeToProject:      2 slots (~8%)
    //   RemoveWorktreeFromProject: 2 slots (~8%)
    const DISTRIBUTION_SLOTS: u32 = 24;

    impl TestState {
        fn generate_operation(&self, raw: u32, project_group_count: usize) -> Operation {
            let extra = (raw / DISTRIBUTION_SLOTS) as usize;

            match raw % DISTRIBUTION_SLOTS {
                0..=4 => Operation::SaveThread {
                    project_group_index: extra % project_group_count,
                },
                5..=6 if !self.unopened_worktrees.is_empty() => Operation::SaveWorktreeThread {
                    worktree_index: extra % self.unopened_worktrees.len(),
                },
                5..=6 => Operation::SaveThread {
                    project_group_index: extra % project_group_count,
                },
                7 => Operation::ToggleAgentPanel,
                8 => Operation::CreateDraftThread,
                9 => Operation::AddProject {
                    use_worktree: !self.unopened_worktrees.is_empty(),
                },
                10..=11 if !self.saved_thread_ids.is_empty() => Operation::ArchiveThread {
                    index: extra % self.saved_thread_ids.len(),
                },
                10..=11 => Operation::AddProject {
                    use_worktree: !self.unopened_worktrees.is_empty(),
                },
                12..=13 if !self.saved_thread_ids.is_empty() => Operation::SwitchToThread {
                    index: extra % self.saved_thread_ids.len(),
                },
                12..=13 => Operation::SwitchToProjectGroup {
                    index: extra % project_group_count,
                },
                14..=15 => Operation::SwitchToProjectGroup {
                    index: extra % project_group_count,
                },
                16..=19 if project_group_count > 0 => Operation::AddLinkedWorktree {
                    project_group_index: extra % project_group_count,
                },
                16..=19 => Operation::SaveThread {
                    project_group_index: extra % project_group_count,
                },
                20..=21 if project_group_count > 0 => Operation::AddWorktreeToProject {
                    project_group_index: extra % project_group_count,
                },
                20..=21 => Operation::SaveThread {
                    project_group_index: extra % project_group_count,
                },
                22..=23 if project_group_count > 0 => Operation::RemoveWorktreeFromProject {
                    project_group_index: extra % project_group_count,
                },
                22..=23 => Operation::SaveThread {
                    project_group_index: extra % project_group_count,
                },
                _ => unreachable!(),
            }
        }
    }

    fn save_thread_to_path_with_main(
        state: &mut TestState,
        path_list: PathList,
        main_worktree_paths: PathList,
        cx: &mut gpui::VisualTestContext,
    ) {
        let session_id = state.next_metadata_only_thread_id();
        let title: SharedString = format!("Thread {}", session_id).into();
        let updated_at = chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2024, 1, 1, 0, 0, 0)
            .unwrap()
            + chrono::Duration::seconds(state.thread_counter as i64);
        let metadata = ThreadMetadata {
            thread_id: ThreadId::new(),
            session_id: Some(session_id),
            agent_id: agent::ZED_AGENT_ID.clone(),
            title: Some(title),
            title_override: None,
            updated_at,
            created_at: None,
            interacted_at: None,
            worktree_paths: WorktreePaths::from_path_lists(main_worktree_paths, path_list).unwrap(),
            archived: false,
            remote_connection: None,
        };
        cx.update(|_, cx| {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx))
        });
        cx.run_until_parked();
    }

    async fn perform_operation(
        operation: Operation,
        state: &mut TestState,
        multi_workspace: &Entity<MultiWorkspace>,
        sidebar: &Entity<Sidebar>,
        cx: &mut gpui::VisualTestContext,
    ) {
        match operation {
            Operation::SaveThread {
                project_group_index,
            } => {
                // Find a workspace for this project group and create a real
                // thread via its agent panel.
                let (workspace, project) = multi_workspace.read_with(cx, |multi_workspace, cx| {
                    let keys = multi_workspace.project_group_keys(cx);
                    let key = &keys[project_group_index];
                    let ws = multi_workspace
                        .workspaces_for_project_group(key, cx)
                        .next()
                        .cloned()
                        .unwrap_or_else(|| multi_workspace.workspace().clone());
                    let project = ws.read(cx).project().clone();
                    (ws, project)
                });

                let panel =
                    workspace.read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx));
                if let Some(panel) = panel {
                    let agent_id = AgentId::new(format!("prop-agent-{}", state.thread_counter));
                    let connection = StubAgentConnection::new().with_agent_id(agent_id.clone());
                    open_thread_with_custom_connection(&panel, connection.clone(), cx);
                    let thread_id = active_thread_id(&panel, cx);
                    let session_id = active_session_id(&panel, cx);
                    // Make the thread non-draft without exercising the prompt
                    // send path; these invariants are about sidebar state.
                    cx.update(|_, cx| {
                        connection.send_update(
                            session_id.clone(),
                            acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new(
                                "Done".into(),
                            )),
                            cx,
                        );
                    });
                    cx.run_until_parked();
                    state.saved_thread_ids.push(session_id.clone());

                    let title: SharedString = format!("Thread {}", state.thread_counter).into();
                    state.thread_counter += 1;
                    let updated_at =
                        chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2024, 1, 1, 0, 0, 0)
                            .unwrap()
                            + chrono::Duration::seconds(state.thread_counter as i64);
                    let metadata = cx.update(|_, cx| ThreadMetadata {
                        thread_id,
                        session_id: Some(session_id),
                        agent_id,
                        title: Some(title),
                        title_override: None,
                        updated_at,
                        created_at: None,
                        interacted_at: None,
                        worktree_paths: project.read(cx).worktree_paths(cx),
                        archived: false,
                        remote_connection: project.read(cx).remote_connection_options(cx).cloned(),
                    });
                    cx.update(|_, cx| {
                        ThreadMetadataStore::global(cx)
                            .update(cx, |store, cx| store.save(metadata, cx))
                    });
                    cx.run_until_parked();
                }
            }
            Operation::SaveWorktreeThread { worktree_index } => {
                let worktree = &state.unopened_worktrees[worktree_index];
                let path_list = PathList::new(&[std::path::PathBuf::from(&worktree.path)]);
                let main_worktree_paths =
                    PathList::new(&[std::path::PathBuf::from(&worktree.main_workspace_path)]);
                save_thread_to_path_with_main(state, path_list, main_worktree_paths, cx);
            }

            Operation::ToggleAgentPanel => {
                let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
                let panel_open =
                    workspace.read_with(cx, |_, cx| AgentPanel::is_visible(&workspace, cx));
                workspace.update_in(cx, |workspace, window, cx| {
                    if panel_open {
                        workspace.close_panel::<AgentPanel>(window, cx);
                    } else {
                        workspace.open_panel::<AgentPanel>(window, cx);
                    }
                });
            }
            Operation::CreateDraftThread => {
                let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
                let panel =
                    workspace.read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx));
                if let Some(panel) = panel {
                    panel.update_in(cx, |panel, window, cx| {
                        panel.new_thread(&NewThread, window, cx);
                    });
                    cx.run_until_parked();
                }
                workspace.update_in(cx, |workspace, window, cx| {
                    workspace.focus_panel::<AgentPanel>(window, cx);
                });
            }
            Operation::AddProject { use_worktree } => {
                let path = if use_worktree {
                    // Open an existing linked worktree as a project (simulates Cmd+O
                    // on a worktree directory).
                    state.unopened_worktrees.remove(0).path
                } else {
                    // Create a brand new project.
                    let path = state.next_workspace_path();
                    state
                        .fs
                        .insert_tree(
                            &path,
                            serde_json::json!({
                                ".git": {},
                                "src": {},
                            }),
                        )
                        .await;
                    path
                };
                let project = project::Project::test(
                    state.fs.clone() as Arc<dyn fs::Fs>,
                    [path.as_ref()],
                    cx,
                )
                .await;
                project.update(cx, |p, cx| p.git_scans_complete(cx)).await;
                multi_workspace.update_in(cx, |mw, window, cx| {
                    mw.test_add_workspace(project.clone(), window, cx)
                });
            }

            Operation::ArchiveThread { index } => {
                let session_id = state.saved_thread_ids[index].clone();
                sidebar.update_in(cx, |sidebar: &mut Sidebar, window, cx| {
                    sidebar.archive_thread(&session_id, window, cx);
                });
                cx.run_until_parked();
                state.saved_thread_ids.remove(index);
            }
            Operation::SwitchToThread { index } => {
                let session_id = state.saved_thread_ids[index].clone();
                // Find the thread's position in the sidebar entries and select it.
                let thread_index = sidebar.read_with(cx, |sidebar, _| {
                    sidebar.contents.entries.iter().position(|entry| {
                        matches!(
                            entry,
                            ListEntry::Thread(t) if t.metadata.session_id.as_ref() == Some(&session_id)
                        )
                    })
                });
                if let Some(ix) = thread_index {
                    sidebar.update_in(cx, |sidebar, window, cx| {
                        sidebar.selection = Some(ix);
                        sidebar.confirm(&Confirm, window, cx);
                    });
                    cx.run_until_parked();
                }
            }
            Operation::SwitchToProjectGroup { index } => {
                let workspace = multi_workspace.read_with(cx, |multi_workspace, cx| {
                    let keys = multi_workspace.project_group_keys(cx);
                    let key = &keys[index];
                    multi_workspace
                        .workspaces_for_project_group(key, cx)
                        .next()
                        .cloned()
                        .unwrap_or_else(|| multi_workspace.workspace().clone())
                });
                multi_workspace.update_in(cx, |mw, window, cx| {
                    mw.activate(workspace, None, window, cx);
                });
            }
            Operation::AddLinkedWorktree {
                project_group_index,
            } => {
                // Get the main worktree path from the project group key.
                let main_path = multi_workspace.read_with(cx, |multi_workspace, cx| {
                    let keys = multi_workspace.project_group_keys(cx);
                    let key = &keys[project_group_index];
                    key.path_list()
                        .paths()
                        .first()
                        .unwrap()
                        .to_string_lossy()
                        .to_string()
                });
                let dot_git = format!("{}/.git", main_path);
                let worktree_name = state.next_worktree_name();
                let worktree_path = format!("/worktrees/{}", worktree_name);

                state.fs
                    .insert_tree(
                        &worktree_path,
                        serde_json::json!({
                            ".git": format!("gitdir: {}/.git/worktrees/{}", main_path, worktree_name),
                            "src": {},
                        }),
                    )
                    .await;

                // Also create the worktree metadata dir inside the main repo's .git
                state
                    .fs
                    .insert_tree(
                        &format!("{}/.git/worktrees/{}", main_path, worktree_name),
                        serde_json::json!({
                            "commondir": "../../",
                            "HEAD": format!("ref: refs/heads/{}", worktree_name),
                        }),
                    )
                    .await;

                let dot_git_path = std::path::Path::new(&dot_git);
                let worktree_pathbuf = std::path::PathBuf::from(&worktree_path);
                state
                    .fs
                    .add_linked_worktree_for_repo(
                        dot_git_path,
                        false,
                        git::repository::Worktree {
                            path: worktree_pathbuf,
                            ref_name: Some(format!("refs/heads/{}", worktree_name).into()),
                            sha: "aaa".into(),
                            is_main: false,
                            is_bare: false,
                            is_prunable: false,
                        },
                    )
                    .await;

                // Re-scan the main workspace's project so it discovers the new worktree.
                let main_workspace = multi_workspace.read_with(cx, |multi_workspace, cx| {
                    let keys = multi_workspace.project_group_keys(cx);
                    let key = &keys[project_group_index];
                    multi_workspace
                        .workspaces_for_project_group(key, cx)
                        .next()
                        .cloned()
                        .unwrap()
                });
                let main_project = main_workspace.read_with(cx, |ws, _| ws.project().clone());
                main_project
                    .update(cx, |p, cx| p.git_scans_complete(cx))
                    .await;

                state.unopened_worktrees.push(UnopenedWorktree {
                    path: worktree_path,
                    main_workspace_path: main_path.clone(),
                });
            }
            Operation::AddWorktreeToProject {
                project_group_index,
            } => {
                let workspace = multi_workspace.read_with(cx, |multi_workspace, cx| {
                    let keys = multi_workspace.project_group_keys(cx);
                    let key = &keys[project_group_index];
                    multi_workspace
                        .workspaces_for_project_group(key, cx)
                        .next()
                        .cloned()
                });
                let Some(workspace) = workspace else { return };
                let project = workspace.read_with(cx, |ws, _| ws.project().clone());

                let new_path = state.next_workspace_path();
                state
                    .fs
                    .insert_tree(&new_path, serde_json::json!({ ".git": {}, "src": {} }))
                    .await;

                let result = project
                    .update(cx, |project, cx| {
                        project.find_or_create_worktree(&new_path, true, cx)
                    })
                    .await;
                if result.is_err() {
                    return;
                }
                cx.run_until_parked();
            }
            Operation::RemoveWorktreeFromProject {
                project_group_index,
            } => {
                let workspace = multi_workspace.read_with(cx, |multi_workspace, cx| {
                    let keys = multi_workspace.project_group_keys(cx);
                    let key = &keys[project_group_index];
                    multi_workspace
                        .workspaces_for_project_group(key, cx)
                        .next()
                        .cloned()
                });
                let Some(workspace) = workspace else { return };
                let project = workspace.read_with(cx, |ws, _| ws.project().clone());

                let worktree_count = project.read_with(cx, |p, cx| p.visible_worktrees(cx).count());
                if worktree_count <= 1 {
                    return;
                }

                let worktree_id = project.read_with(cx, |p, cx| {
                    p.visible_worktrees(cx).last().map(|wt| wt.read(cx).id())
                });
                if let Some(worktree_id) = worktree_id {
                    project.update(cx, |project, cx| {
                        project.remove_worktree(worktree_id, cx);
                    });
                    cx.run_until_parked();
                }
            }
        }
    }

    fn update_sidebar(sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext) {
        sidebar.update_in(cx, |sidebar, _window, cx| {
            if let Some(multi_workspace) = sidebar.multi_workspace.upgrade() {
                multi_workspace.update(cx, |multi_workspace, cx| {
                    multi_workspace.test_expand_all_groups(cx)
                });
            }
            sidebar.update_entries(cx);
        });
    }

    fn validate_sidebar_properties(sidebar: &Sidebar, cx: &App) -> anyhow::Result<()> {
        verify_every_group_in_multiworkspace_is_shown(sidebar, cx)?;
        verify_no_duplicate_threads(sidebar)?;
        verify_all_threads_are_shown(sidebar, cx)?;
        verify_active_state_matches_current_workspace(sidebar, cx)?;
        verify_all_workspaces_are_reachable(sidebar, cx)?;
        verify_workspace_group_key_integrity(sidebar, cx)?;
        Ok(())
    }

    fn verify_no_duplicate_threads(sidebar: &Sidebar) -> anyhow::Result<()> {
        let mut seen: HashSet<acp::SessionId> = HashSet::default();
        let mut duplicates: Vec<(acp::SessionId, String)> = Vec::new();

        for entry in &sidebar.contents.entries {
            if let Some(session_id) = entry.session_id() {
                if !seen.insert(session_id.clone()) {
                    let title = match entry {
                        ListEntry::Thread(thread) => thread.metadata.display_title().to_string(),
                        _ => "<unknown>".to_string(),
                    };
                    duplicates.push((session_id.clone(), title));
                }
            }
        }

        anyhow::ensure!(
            duplicates.is_empty(),
            "threads appear more than once in sidebar: {:?}",
            duplicates,
        );
        Ok(())
    }

    fn verify_every_group_in_multiworkspace_is_shown(
        sidebar: &Sidebar,
        cx: &App,
    ) -> anyhow::Result<()> {
        let Some(multi_workspace) = sidebar.multi_workspace.upgrade() else {
            anyhow::bail!("sidebar should still have an associated multi-workspace");
        };

        let multi_workspace = multi_workspace.read(cx);

        // Every project group key in the multi-workspace that has a
        // non-empty path list should appear as a ProjectHeader in the
        // sidebar.
        let all_keys = multi_workspace.project_group_keys(cx);
        let expected_keys: HashSet<&ProjectGroupKey> = all_keys
            .iter()
            .filter(|k| !k.path_list().paths().is_empty())
            .collect();

        let sidebar_keys: HashSet<&ProjectGroupKey> = sidebar
            .contents
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::ProjectHeader { key, .. } => Some(key),
                _ => None,
            })
            .collect();

        let missing = &expected_keys - &sidebar_keys;
        let stray = &sidebar_keys - &expected_keys;

        anyhow::ensure!(
            missing.is_empty() && stray.is_empty(),
            "sidebar project groups don't match multi-workspace.\n\
             Only in multi-workspace (missing): {:?}\n\
             Only in sidebar (stray): {:?}",
            missing,
            stray,
        );

        Ok(())
    }

    fn verify_all_threads_are_shown(sidebar: &Sidebar, cx: &App) -> anyhow::Result<()> {
        let Some(multi_workspace) = sidebar.multi_workspace.upgrade() else {
            anyhow::bail!("sidebar should still have an associated multi-workspace");
        };
        let workspaces = multi_workspace
            .read(cx)
            .workspaces()
            .cloned()
            .collect::<Vec<_>>();
        let thread_store = ThreadMetadataStore::global(cx);

        let sidebar_thread_ids: HashSet<acp::SessionId> = sidebar
            .contents
            .entries
            .iter()
            .filter_map(|entry| entry.session_id().cloned())
            .collect();

        let mut metadata_thread_ids: HashSet<acp::SessionId> = HashSet::default();

        // Query using the same approach as the sidebar: iterate project
        // group keys, then do main + legacy queries per group.
        let multi_workspace = multi_workspace.read(cx);
        let mut workspaces_by_group: HashMap<ProjectGroupKey, Vec<Entity<Workspace>>> =
            HashMap::default();
        for workspace in &workspaces {
            let key = workspace.read(cx).project_group_key(cx);
            workspaces_by_group
                .entry(key)
                .or_default()
                .push(workspace.clone());
        }

        for group_key in multi_workspace.project_group_keys(cx) {
            let path_list = group_key.path_list().clone();
            if path_list.paths().is_empty() {
                continue;
            }

            let group_workspaces = workspaces_by_group
                .get(&group_key)
                .map(|ws| ws.as_slice())
                .unwrap_or_default();

            // Main code path queries (run for all groups, even without workspaces).
            // Skip drafts (session_id: None) — they are not shown in the
            // sidebar entries.
            for metadata in thread_store
                .read(cx)
                .entries_for_main_worktree_path(&path_list, None)
            {
                if let Some(sid) = metadata.session_id.clone() {
                    metadata_thread_ids.insert(sid);
                }
            }
            for metadata in thread_store.read(cx).entries_for_path(&path_list, None) {
                if let Some(sid) = metadata.session_id.clone() {
                    metadata_thread_ids.insert(sid);
                }
            }

            // Legacy: per-workspace queries for different root paths.
            let covered_paths: HashSet<std::path::PathBuf> = group_workspaces
                .iter()
                .flat_map(|ws| {
                    ws.read(cx)
                        .root_paths(cx)
                        .into_iter()
                        .map(|p| p.to_path_buf())
                })
                .collect();

            for workspace in group_workspaces {
                let ws_path_list = workspace_path_list(workspace, cx);
                if ws_path_list != path_list {
                    for metadata in thread_store.read(cx).entries_for_path(&ws_path_list, None) {
                        if let Some(sid) = metadata.session_id.clone() {
                            metadata_thread_ids.insert(sid);
                        }
                    }
                }
            }

            for workspace in group_workspaces {
                for snapshot in root_repository_snapshots(workspace, cx) {
                    let Some(main_worktree_abs_path) = snapshot.main_worktree_abs_path() else {
                        continue;
                    };
                    let repo_path_list = PathList::new(&[main_worktree_abs_path.to_path_buf()]);
                    if repo_path_list != path_list {
                        continue;
                    }
                    for linked_worktree in snapshot.linked_worktrees() {
                        if covered_paths.contains(&*linked_worktree.path) {
                            continue;
                        }
                        let worktree_path_list =
                            PathList::new(std::slice::from_ref(&linked_worktree.path));
                        for metadata in thread_store
                            .read(cx)
                            .entries_for_path(&worktree_path_list, None)
                        {
                            if let Some(sid) = metadata.session_id.clone() {
                                metadata_thread_ids.insert(sid);
                            }
                        }
                    }
                }
            }
        }

        anyhow::ensure!(
            sidebar_thread_ids == metadata_thread_ids,
            "sidebar threads don't match metadata store: sidebar has {:?}, store has {:?}",
            sidebar_thread_ids,
            metadata_thread_ids,
        );
        Ok(())
    }

    fn verify_active_state_matches_current_workspace(
        sidebar: &Sidebar,
        cx: &App,
    ) -> anyhow::Result<()> {
        let Some(multi_workspace) = sidebar.multi_workspace.upgrade() else {
            anyhow::bail!("sidebar should still have an associated multi-workspace");
        };

        let active_workspace = multi_workspace.read(cx).workspace();

        // 1. active_entry should be Some when the panel has content.
        //    It may be None when the panel is uninitialized (no drafts,
        //    no threads), which is fine.
        //    It may also temporarily point at a different workspace
        //    when the workspace just changed and the new panel has no
        //    content yet.
        let panel = active_workspace.read(cx).panel::<AgentPanel>(cx).unwrap();
        let panel_has_content = panel.read(cx).active_thread_id(cx).is_some()
            || panel.read(cx).active_conversation_view().is_some()
            || panel.read(cx).active_terminal_id().is_some();

        let Some(entry) = sidebar.active_entry.as_ref() else {
            if panel_has_content {
                anyhow::bail!("active_entry is None but panel has content");
            }
            return Ok(());
        };

        // If the entry workspace doesn't match the active workspace
        // and the panel has no content, this is a transient state that
        // will resolve when the panel gets content.
        if entry.workspace().entity_id() != active_workspace.entity_id() && !panel_has_content {
            return Ok(());
        }

        // 2. The entry's workspace must agree with the multi-workspace's
        //    active workspace.
        anyhow::ensure!(
            entry.workspace().entity_id() == active_workspace.entity_id(),
            "active_entry workspace ({:?}) != active workspace ({:?})",
            entry.workspace().entity_id(),
            active_workspace.entity_id(),
        );

        // 3. The entry must match the agent panel's current state.
        if panel.read(cx).active_thread_id(cx).is_some() {
            anyhow::ensure!(
                matches!(entry, ActiveEntry::Thread { .. }),
                "panel shows a tracked draft but active_entry is {:?}",
                entry,
            );
        } else if let Some(thread_id) = panel
            .read(cx)
            .active_conversation_view()
            .map(|cv| cv.read(cx).parent_id())
        {
            anyhow::ensure!(
                matches!(entry, ActiveEntry::Thread { thread_id: tid, .. } if *tid == thread_id),
                "panel has thread {:?} but active_entry is {:?}",
                thread_id,
                entry,
            );
        }

        // 4. Exactly one entry in sidebar contents must be uniquely
        //    identified by the active_entry — unless the panel is showing
        //    the new-draft slot (which is represented by the + button's
        //    active state rather than a sidebar row) or nothing at all.
        // Active terminals must still match a row, so don't treat the absence
        // of a conversation view as "new-draft" when a terminal is active.
        let hidden_from_sidebar = panel.read(cx).active_terminal_id().is_none()
            && (panel.read(cx).active_view_is_new_draft(cx)
                || panel.read(cx).active_conversation_view().is_none());
        if hidden_from_sidebar {
            return Ok(());
        }
        let matching_count = sidebar
            .contents
            .entries
            .iter()
            .filter(|e| entry.matches_entry(e))
            .count();
        if matching_count != 1 {
            let thread_entries: Vec<_> = sidebar
                .contents
                .entries
                .iter()
                .filter_map(|e| match e {
                    ListEntry::Thread(t) => Some(format!(
                        "tid={:?} sid={:?}",
                        t.metadata.thread_id, t.metadata.session_id
                    )),
                    _ => None,
                })
                .collect();
            let store = agent_ui::thread_metadata_store::ThreadMetadataStore::global(cx).read(cx);
            let store_entries: Vec<_> = store
                .entries()
                .map(|m| {
                    format!(
                        "tid={:?} sid={:?} archived={} paths={:?}",
                        m.thread_id,
                        m.session_id,
                        m.archived,
                        m.folder_paths()
                    )
                })
                .collect();
            anyhow::bail!(
                "expected exactly 1 sidebar entry matching active_entry {:?}, found {}. sidebar threads: {:?}. store: {:?}",
                entry,
                matching_count,
                thread_entries,
                store_entries,
            );
        }

        Ok(())
    }

    /// Every workspace in the multi-workspace should be "reachable" from
    /// the sidebar — meaning there is at least one entry (thread, draft,
    /// new-thread, or project header) that, when clicked, would activate
    /// that workspace.
    fn verify_all_workspaces_are_reachable(sidebar: &Sidebar, cx: &App) -> anyhow::Result<()> {
        let Some(multi_workspace) = sidebar.multi_workspace.upgrade() else {
            anyhow::bail!("sidebar should still have an associated multi-workspace");
        };

        let multi_workspace = multi_workspace.read(cx);

        let reachable_workspaces: HashSet<gpui::EntityId> = sidebar
            .contents
            .entries
            .iter()
            .flat_map(|entry| entry.reachable_workspaces(multi_workspace, cx))
            .map(|ws| ws.entity_id())
            .collect();

        let all_workspace_ids: HashSet<gpui::EntityId> = multi_workspace
            .workspaces()
            .map(|ws| ws.entity_id())
            .collect();

        let unreachable = &all_workspace_ids - &reachable_workspaces;

        anyhow::ensure!(
            unreachable.is_empty(),
            "The following workspaces are not reachable from any sidebar entry: {:?}",
            unreachable,
        );

        Ok(())
    }

    fn verify_workspace_group_key_integrity(sidebar: &Sidebar, cx: &App) -> anyhow::Result<()> {
        let Some(multi_workspace) = sidebar.multi_workspace.upgrade() else {
            anyhow::bail!("sidebar should still have an associated multi-workspace");
        };
        multi_workspace
            .read(cx)
            .assert_project_group_key_integrity(cx)
    }

    #[gpui::property_test(config = ProptestConfig {
        cases: 20,
        ..Default::default()
    })]
    async fn test_sidebar_invariants(
        #[strategy = gpui::proptest::collection::vec(0u32..DISTRIBUTION_SLOTS * 10, 1..10)]
        raw_operations: Vec<u32>,
        cx: &mut TestAppContext,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT_PROPTEST_DB: AtomicUsize = AtomicUsize::new(0);

        let test_db_id = NEXT_PROPTEST_DB.fetch_add(1, Ordering::SeqCst);
        cx.update(|cx| {
            cx.set_global(TestTerminalMetadataDbName(format!(
                "PROPTEST_TERMINAL_THREAD_METADATA_{test_db_id}"
            )));
        });

        agent_ui::test_support::init_test(cx);
        cx.update(|cx| {
            cx.set_global(db::AppDatabase::test_new());
            set_max_idle_retained_threads(1, cx);
            cx.set_global(agent_ui::thread_metadata_store::TestMetadataDbName(
                format!("PROPTEST_THREAD_METADATA_{test_db_id}"),
            ));

            ThreadStore::init_global(cx);
            ThreadMetadataStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            prompt_store::init(cx);

            // Auto-add an AgentPanel to every workspace so that implicitly
            // created workspaces (e.g. from thread activation) also have one.
            cx.observe_new(
                |workspace: &mut Workspace,
                 window: Option<&mut Window>,
                 cx: &mut gpui::Context<Workspace>| {
                    if let Some(window) = window {
                        let panel = cx.new(|cx| AgentPanel::test_new(workspace, window, cx));
                        workspace.add_panel(panel, window, cx);
                    }
                },
            )
            .detach();
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/my-project",
            serde_json::json!({
                ".git": {},
                "src": {},
            }),
        )
        .await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let project =
            project::Project::test(fs.clone() as Arc<dyn fs::Fs>, ["/my-project".as_ref()], cx)
                .await;
        project.update(cx, |p, cx| p.git_scans_complete(cx)).await;

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let sidebar = setup_sidebar(&multi_workspace, cx);

        let mut state = TestState::new(fs);
        let mut executed: Vec<String> = Vec::new();

        for &raw_op in &raw_operations {
            let project_group_count = multi_workspace.read_with(cx, |multi_workspace, cx| {
                multi_workspace.project_group_keys(cx).len()
            });
            let operation = state.generate_operation(raw_op, project_group_count);
            executed.push(format!("{:?}", operation));
            perform_operation(operation, &mut state, &multi_workspace, &sidebar, cx).await;
            cx.run_until_parked();

            update_sidebar(&sidebar, cx);
            cx.run_until_parked();

            let result =
                sidebar.read_with(cx, |sidebar, cx| validate_sidebar_properties(sidebar, cx));
            if let Err(err) = result {
                let log = executed.join("\n  ");
                panic!(
                    "Property violation after step {}:\n{err}\n\nOperations:\n  {log}",
                    executed.len(),
                );
            }
        }
    }
}

#[gpui::test]
async fn test_remote_project_integration_does_not_briefly_render_as_separate_project(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);

    cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });

    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        workspace::init(app_state.clone(), cx);
        app_state
    });

    // Set up the remote server side.
    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree(
            "/project",
            serde_json::json!({
                ".git": {},
                "src": { "main.rs": "fn main() {}" }
            }),
        )
        .await;
    server_fs.set_branch_name(Path::new("/project/.git"), Some("main"));

    // Create the linked worktree checkout path on the remote server,
    // but do not yet register it as a git-linked worktree. The real
    // regrouping update in this test should happen only after the
    // sidebar opens the closed remote thread.
    server_fs
        .insert_tree(
            "/project-wt-1",
            serde_json::json!({
                "src": { "main.rs": "fn main() {}" }
            }),
        )
        .await;

    server_cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });

    let (original_opts, server_session, _) = remote::RemoteClient::fake_server(cx, server_cx);

    server_cx.update(remote_server::HeadlessProject::init);
    let server_executor = server_cx.executor();
    let _headless = server_cx.new(|cx| {
        remote_server::HeadlessProject::new(
            remote_server::HeadlessAppState {
                session: server_session,
                fs: server_fs.clone(),
                http_client: Arc::new(http_client::BlockedHttpClient),
                node_runtime: node_runtime::NodeRuntime::unavailable(),
                languages: Arc::new(language::LanguageRegistry::new(server_executor.clone())),
                extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });

    // Connect the client side and build a remote project.
    let remote_client = remote::RemoteClient::connect_mock(original_opts.clone(), cx).await;
    let project = cx.update(|cx| {
        let project_client = client::Client::new(
            Arc::new(clock::FakeSystemClock::new()),
            http_client::FakeHttpClient::with_404_response(),
            cx,
        );
        let user_store = cx.new(|cx| client::UserStore::new(project_client.clone(), cx));
        project::Project::remote(
            remote_client,
            project_client,
            node_runtime::NodeRuntime::unavailable(),
            user_store,
            app_state.languages.clone(),
            app_state.fs.clone(),
            false,
            cx,
        )
    });

    // Open the remote worktree.
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(Path::new("/project"), true, cx)
        })
        .await
        .expect("should open remote worktree");
    cx.run_until_parked();

    // Verify the project is remote.
    project.read_with(cx, |project, cx| {
        assert!(!project.is_local(), "project should be remote");
        assert!(
            project.remote_connection_options(cx).is_some(),
            "project should have remote connection options"
        );
    });

    cx.update(|cx| <dyn fs::Fs>::set_global(app_state.fs.clone(), cx));

    // Create MultiWorkspace with the remote project.
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    cx.run_until_parked();

    // Save a thread for the main remote workspace (folder_paths match
    // the open workspace, so it will be classified as Open).
    let main_thread_id = acp::SessionId::new(Arc::from("main-thread"));
    save_thread_metadata(
        main_thread_id.clone(),
        Some("Main Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    // Save a thread whose folder_paths point to a linked worktree path
    // that doesn't have an open workspace ("/project-wt-1"), but whose
    // main_worktree_paths match the project group key so it appears
    // in the sidebar under the same remote group. This simulates a
    // linked worktree workspace that was closed.
    let remote_thread_id = acp::SessionId::new(Arc::from("remote-thread"));
    let (main_worktree_paths, remote_connection) = project.read_with(cx, |p, cx| {
        (
            p.project_group_key(cx).path_list().clone(),
            p.remote_connection_options(cx).cloned(),
        )
    });
    cx.update(|_window, cx| {
        let metadata = ThreadMetadata {
            thread_id: ThreadId::new(),
            session_id: Some(remote_thread_id.clone()),
            agent_id: agent::ZED_AGENT_ID.clone(),
            title: Some("Worktree Thread".into()),
            title_override: None,
            updated_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 1).unwrap(),
            created_at: None,
            interacted_at: None,
            worktree_paths: WorktreePaths::from_path_lists(
                main_worktree_paths,
                PathList::new(&[PathBuf::from("/project-wt-1")]),
            )
            .unwrap(),
            archived: false,
            remote_connection,
        };
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = sidebar.contents.entries.iter().position(|entry| {
            matches!(
                entry,
                ListEntry::Thread(thread) if thread.metadata.session_id.as_ref() == Some(&remote_thread_id)
            )
        });
    });

    let saw_separate_project_header = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw_separate_project_header_for_observer = saw_separate_project_header.clone();

    sidebar
        .update(cx, |_, cx| {
            cx.observe_self(move |sidebar, _cx| {
                let mut project_headers = sidebar.contents.entries.iter().filter_map(|entry| {
                    if let ListEntry::ProjectHeader { label, .. } = entry {
                        Some(label.as_ref())
                    } else {
                        None
                    }
                });

                let Some(project_header) = project_headers.next() else {
                    saw_separate_project_header_for_observer
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    return;
                };

                if project_header != "project" || project_headers.next().is_some() {
                    saw_separate_project_header_for_observer
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
            })
        })
        .detach();

    multi_workspace.update(cx, |multi_workspace, cx| {
        let workspace = multi_workspace.workspace().clone();
        workspace.update(cx, |workspace: &mut Workspace, cx| {
            let remote_client = workspace
                .project()
                .read(cx)
                .remote_client()
                .expect("main remote project should have a remote client");
            remote_client.update(cx, |remote_client: &mut remote::RemoteClient, cx| {
                remote_client.force_server_not_running(cx);
            });
        });
    });
    cx.run_until_parked();

    let (server_session_2, connect_guard_2) =
        remote::RemoteClient::fake_server_with_opts(&original_opts, cx, server_cx);
    let _headless_2 = server_cx.new(|cx| {
        remote_server::HeadlessProject::new(
            remote_server::HeadlessAppState {
                session: server_session_2,
                fs: server_fs.clone(),
                http_client: Arc::new(http_client::BlockedHttpClient),
                node_runtime: node_runtime::NodeRuntime::unavailable(),
                languages: Arc::new(language::LanguageRegistry::new(server_executor.clone())),
                extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });
    drop(connect_guard_2);

    let window = cx.windows()[0];
    cx.update_window(window, |_, window, cx| {
        window.dispatch_action(Confirm.boxed_clone(), cx);
    })
    .unwrap();

    cx.run_until_parked();

    let new_workspace = multi_workspace.read_with(cx, |mw, _| {
        assert_eq!(
            mw.workspaces().count(),
            2,
            "confirming a closed remote thread should open a second workspace"
        );
        mw.workspaces()
            .find(|workspace| workspace.entity_id() != mw.workspace().entity_id())
            .unwrap()
            .clone()
    });

    server_fs
        .add_linked_worktree_for_repo(
            Path::new("/project/.git"),
            true,
            git::repository::Worktree {
                path: PathBuf::from("/project-wt-1"),
                ref_name: Some("refs/heads/feature-wt".into()),
                sha: "abc123".into(),
                is_main: false,
                is_bare: false,
                is_prunable: false,
            },
        )
        .await;

    server_cx.run_until_parked();
    cx.run_until_parked();
    server_cx.run_until_parked();
    cx.run_until_parked();

    let entries_after_update = visible_entries_as_strings(&sidebar, cx);
    let group_after_update = new_workspace.read_with(cx, |workspace, cx| {
        workspace.project().read(cx).project_group_key(cx)
    });

    assert_eq!(
        group_after_update,
        project.read_with(cx, |project, cx| ProjectGroupKey::from_project(project, cx)),
        "expected the remote worktree workspace to be grouped under the main remote project after the real update; \
         final sidebar entries: {:?}",
        entries_after_update,
    );

    sidebar.update(cx, |sidebar, _cx| {
        assert_remote_project_integration_sidebar_state(
            sidebar,
            &main_thread_id,
            &remote_thread_id,
        );
    });

    assert!(
        !saw_separate_project_header.load(std::sync::atomic::Ordering::SeqCst),
        "sidebar briefly rendered the remote worktree as a separate project during the real remote open/update sequence; \
         final group: {:?}; final sidebar entries: {:?}",
        group_after_update,
        entries_after_update,
    );
}

#[test]
fn test_worktree_info_branch_names_for_main_worktrees() {
    let folder_paths = PathList::new(&[PathBuf::from("/projects/myapp")]);
    let worktree_paths = WorktreePaths::from_folder_paths(&folder_paths);

    let branch_by_path: HashMap<PathBuf, SharedString> =
        [(PathBuf::from("/projects/myapp"), "feature-x".into())]
            .into_iter()
            .collect();

    let infos = worktree_info_from_thread_paths(&worktree_paths, &branch_by_path);
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].kind, ui::WorktreeKind::Main);
    assert_eq!(infos[0].branch_name, Some(SharedString::from("feature-x")));
    assert_eq!(infos[0].worktree_name, Some(SharedString::from("myapp")));
}

#[test]
fn test_worktree_info_branch_names_for_linked_worktrees() {
    let main_paths = PathList::new(&[PathBuf::from("/projects/myapp")]);
    let folder_paths = PathList::new(&[PathBuf::from("/projects/myapp-feature")]);
    let worktree_paths =
        WorktreePaths::from_path_lists(main_paths, folder_paths).expect("same length");

    let branch_by_path: HashMap<PathBuf, SharedString> = [(
        PathBuf::from("/projects/myapp-feature"),
        "feature-branch".into(),
    )]
    .into_iter()
    .collect();

    let infos = worktree_info_from_thread_paths(&worktree_paths, &branch_by_path);
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].kind, ui::WorktreeKind::Linked);
    assert_eq!(
        infos[0].branch_name,
        Some(SharedString::from("feature-branch"))
    );
}

#[test]
fn test_worktree_info_names_only_delta_checkouts_after_their_checkout() {
    let worktree_names = |paths: &[(&str, &str, Option<(&str, bool, Option<&str>)>)]| {
        let (main_paths, folder_paths) = paths
            .iter()
            .map(|(main_path, folder_path, _)| {
                (PathBuf::from(main_path), PathBuf::from(folder_path))
            })
            .unzip::<_, _, Vec<_>, Vec<_>>();
        let worktree_paths = WorktreePaths::from_path_lists(
            PathList::new(&main_paths),
            PathList::new(&folder_paths),
        )
        .expect("same length")
        .with_delta_clones(paths.iter().filter_map(|(_, folder_path, clone)| {
            let (clone_path, is_linked_worktree, checkout_path) = clone.as_ref()?;
            Some((
                PathBuf::from(folder_path),
                project::DeltaClone {
                    path: PathBuf::from(clone_path),
                    is_linked_worktree: *is_linked_worktree,
                    checkout_path: checkout_path.map(PathBuf::from),
                },
            ))
        }));
        worktree_info_from_thread_paths(&worktree_paths, &HashMap::<PathBuf, SharedString>::new())
            .into_iter()
            .map(|info| info.worktree_name.unwrap_or_default().to_string())
            .collect::<Vec<_>>()
    };

    assert_eq!(
        [
            worktree_names(&[
                (
                    "/repo_a",
                    "/storage/worktrees/red/repo_a/src",
                    Some((
                        "/storage/clones/red/repo_a.git",
                        false,
                        Some("/storage/worktrees/red/repo_a"),
                    )),
                ),
                (
                    "/repo_b",
                    "/storage/worktrees/blue/repo_b/lib",
                    Some((
                        "/storage/clones/blue/repo_b.git",
                        false,
                        Some("/storage/worktrees/blue/repo_b"),
                    )),
                ),
            ]),
            worktree_names(&[(
                "/repos/My Project",
                "/storage/worktrees/checkout-a/My_Project",
                Some((
                    "/storage/clones/checkout-a/My_Project.git",
                    false,
                    Some("/storage/worktrees/checkout-a/My_Project"),
                )),
            )]),
            worktree_names(&[("/repos/My Project", "/checkouts/review/My_Project", None)]),
            worktree_names(&[("/repos/My Project", "/checkouts/review/My Project", None)]),
            worktree_names(&[(
                "/repo",
                "/tmp/review/repo",
                Some(("/repo/.delta/clones/abc/repo.git", true, None)),
            )]),
            worktree_names(&[(
                "/repo",
                "/tmp/review/repo",
                Some(("/repo/.delta/clones/def/repo.git", true, None)),
            )]),
            worktree_names(&[(
                "/repo",
                "/worktrees/red/repo/src",
                Some((
                    "/repo/.delta/clones/abc/repo.git",
                    true,
                    Some("/worktrees/red/repo"),
                )),
            )]),
            worktree_names(&[(
                "/repo",
                "/worktrees/blue/repo/src",
                Some((
                    "/repo/.delta/clones/abc/repo.git",
                    true,
                    Some("/worktrees/blue/repo"),
                )),
            )]),
            worktree_names(&[(
                "/repo",
                "/home/abc/checkouts/red",
                Some(("/repo/.delta/clones/abc/repo.git", false, None)),
            )]),
            worktree_names(&[(
                "/repo",
                "/home/abc/checkouts/blue",
                Some(("/repo/.delta/clones/abc/repo.git", false, None)),
            )]),
        ],
        [
            vec!["repo_a:red".to_string(), "repo_b:blue".to_string()],
            vec!["checkout-a".to_string()],
            vec!["My_Project".to_string()],
            vec!["review".to_string()],
            vec!["abc/repo/review/repo".to_string()],
            vec!["def/repo/review/repo".to_string()],
            vec!["abc/repo/red/repo".to_string()],
            vec!["abc/repo/blue/repo".to_string()],
            vec!["abc/repo/red".to_string()],
            vec!["abc/repo/blue".to_string()],
        ]
    );
}

#[test]
fn test_worktree_info_missing_branch_returns_none() {
    let folder_paths = PathList::new(&[PathBuf::from("/projects/myapp")]);
    let worktree_paths = WorktreePaths::from_folder_paths(&folder_paths);

    let branch_by_path: HashMap<PathBuf, SharedString> = HashMap::new();

    let infos = worktree_info_from_thread_paths(&worktree_paths, &branch_by_path);
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].kind, ui::WorktreeKind::Main);
    assert_eq!(infos[0].branch_name, None);
    assert_eq!(infos[0].worktree_name, Some(SharedString::from("myapp")));
}

#[gpui::test]
async fn test_remote_archive_thread_with_disconnected_remote(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    // When a remote thread has no linked-worktree state to archive (only
    // a main worktree), archival is a pure metadata operation: no RPCs
    // are issued against the remote server. This must succeed even when
    // the connection has dropped out, because losing connectivity should
    // not block users from cleaning up their thread list.
    //
    // Threads that *do* have linked-worktree state require a live
    // connection to run the git worktree removal on the server; that
    // path is covered by `test_remote_archive_thread_with_active_connection`.
    init_test(cx);

    cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });

    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        workspace::init(app_state.clone(), cx);
        app_state
    });

    server_cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });

    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree(
            "/project",
            serde_json::json!({
                ".git": {},
                "src": { "main.rs": "fn main() {}" },
            }),
        )
        .await;
    server_fs.set_branch_name(Path::new("/project/.git"), Some("main"));

    let (project, _headless, _opts) = start_remote_project(
        &server_fs,
        Path::new("/project"),
        &app_state,
        None,
        cx,
        server_cx,
    )
    .await;
    let remote_client = project
        .read_with(cx, |project, _cx| project.remote_client())
        .expect("remote project should expose its client");

    cx.update(|cx| <dyn fs::Fs>::set_global(app_state.fs.clone(), cx));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let thread_id = acp::SessionId::new(Arc::from("remote-thread"));
    save_thread_metadata(
        thread_id.clone(),
        Some("Remote Thread".into()),
        chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    cx.run_until_parked();

    // Sanity-check: there is nothing on the remote fs outside the main
    // repo, so archival should not need to touch the server.
    assert!(
        !server_fs.is_dir(Path::new("/worktrees")).await,
        "no linked worktrees on the server before archiving"
    );

    // Disconnect the remote connection before archiving. We don't
    // `run_until_parked` here because the disconnect itself triggers
    // reconnection work that can't complete in the test environment.
    remote_client.update(cx, |client, cx| {
        client.simulate_disconnect(cx).detach();
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&thread_id, window, cx);
    });
    cx.run_until_parked();

    let is_archived = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&thread_id)
            .map(|t| t.archived)
            .unwrap_or(false)
    });
    assert!(
        is_archived,
        "thread should be archived even when remote is disconnected"
    );

    let entries = visible_entries_as_strings(&sidebar, cx);
    assert!(
        !entries.iter().any(|e| e.contains("Remote Thread")),
        "archived thread should be hidden from sidebar: {entries:?}"
    );
}

#[gpui::test]
async fn test_collab_guest_move_thread_paths_is_noop(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = project::Project::test(fs, ["/project-a".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

    // Set up the sidebar while the project is local. This registers the
    // WorktreePathsChanged subscription for the project.
    let _sidebar = setup_sidebar(&multi_workspace, cx);

    let session_id = acp::SessionId::new(Arc::from("test-thread"));
    save_named_thread_metadata("test-thread", "My Thread", &project, cx).await;

    let thread_id = cx.update(|_window, cx| {
        ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(&session_id)
            .map(|e| e.thread_id)
            .expect("thread must be in the store")
    });

    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx);
        let entry = store.read(cx).entry(thread_id).unwrap();
        assert_eq!(
            entry.folder_paths().paths(),
            &[PathBuf::from("/project-a")],
            "thread must be saved with /project-a before collab"
        );
    });

    // Transition the project into collab mode. The sidebar's subscription is
    // still active from when the project was local.
    project.update(cx, |project, _cx| {
        project.mark_as_collab_for_testing();
    });

    // Adding a worktree fires WorktreePathsChanged with old_paths = {/project-a}.
    // The sidebar's subscription is still active, so move_thread_paths is called.
    // Without the is_via_collab() guard inside move_thread_paths, this would
    // update the stored thread paths from {/project-a} to {/project-a, /project-b}.
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project-b", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();

    cx.update(|_window, cx| {
        let store = ThreadMetadataStore::global(cx);
        let entry = store
            .read(cx)
            .entry(thread_id)
            .expect("thread must still exist");
        assert_eq!(
            entry.folder_paths().paths(),
            &[PathBuf::from("/project-a")],
            "thread path must not change when project is via collab"
        );
    });
}

#[gpui::test]
async fn test_cmd_click_project_header_returns_to_last_active_linked_worktree_workspace(
    cx: &mut TestAppContext,
) {
    // Regression test for: cmd-clicking a project group header should return
    // the user to the workspace they most recently had active in that group,
    // including workspaces rooted at a linked worktree.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project-a",
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;

    fs.add_linked_worktree_for_repo(
        Path::new("/project-a/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
            is_prunable: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let worktree_project_a =
        project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;

    main_project_a
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project_a
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    // The multi-workspace starts with the main-paths workspace of group A
    // as the initially active workspace.
    let (multi_workspace, cx) = cx
        .add_window_view(|window, cx| MultiWorkspace::test_new(main_project_a.clone(), window, cx));

    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Capture the initially active workspace (group A's main-paths workspace)
    // *before* registering additional workspaces, since `workspaces()` returns
    // retained workspaces in registration order — not activation order — and
    // the multi-workspace's starting workspace may not be retained yet.
    let main_workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    // Register the linked-worktree workspace (group A) and the group-B
    // workspace. Both get retained by the multi-workspace.
    let worktree_workspace_a = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project_a.clone(), window, cx)
    });
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx)
    });

    cx.run_until_parked();

    // Step 1: activate the linked-worktree workspace. The MultiWorkspace
    // records this as the last-active workspace for group A on its
    // ProjectGroupState. (We don't assert on the initial active workspace
    // because `test_add_workspace` may auto-activate newly registered
    // workspaces — what matters for this test is the explicit sequence of
    // activations below.)
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(worktree_workspace_a.clone(), None, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        worktree_workspace_a,
        "linked-worktree workspace should be active after step 1"
    );

    // Step 2: switch to the workspace for group B. Group A's last-active
    // workspace remains the linked-worktree one (group B getting activated
    // records *its own* last-active workspace, not group A's).
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_b.clone(), None, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        workspace_b,
        "group B's workspace should be active after step 2"
    );

    // Step 3: simulate cmd-click on group A's header. The project group key
    // for group A is derived from the *main-paths* workspace (linked-worktree
    // workspaces share the same key because it normalizes to main-worktree
    // paths).
    let group_a_key = main_workspace_a.read_with(cx, |ws, cx| ws.project_group_key(cx));
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.activate_or_open_workspace_for_group(&group_a_key, window, cx);
    });
    cx.run_until_parked();

    // Expected: we're back in the linked-worktree workspace, not the
    // main-paths one.
    let active_after_cmd_click = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    assert_eq!(
        active_after_cmd_click, worktree_workspace_a,
        "cmd-click on group A's header should return to the last-active \
         linked-worktree workspace, not the main-paths workspace"
    );
    assert_ne!(
        active_after_cmd_click, main_workspace_a,
        "cmd-click must not fall back to the main-paths workspace when a \
         linked-worktree workspace was the last-active one for the group"
    );
}

#[test]
fn test_split_leading_icon_char() {
    // A leading symbol set off by whitespace is pulled out and trimmed from the
    // title.
    let (icon, title, positions) =
        split_leading_icon_char(&"✳ Implement separate config".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "✳");
    assert_eq!(title.as_ref(), "Implement separate config");
    assert_eq!(positions, Vec::<usize>::new());

    // No prefix when the title starts with a letter.
    assert!(split_leading_icon_char(&"Implement separate config".into(), &[]).is_none());

    // Leading whitespace is not treated as a prefix.
    assert!(split_leading_icon_char(&" leading space".into(), &[]).is_none());

    // An alphanumeric prefix such as a version marker is not treated as an icon.
    assert!(split_leading_icon_char(&"v1 Running".into(), &[]).is_none());
    assert!(split_leading_icon_char(&"1 first".into(), &[]).is_none());

    // A title consisting only of a symbol (no whitespace separator) is left
    // untouched.
    assert!(split_leading_icon_char(&"✳".into(), &[]).is_none());
    assert!(split_leading_icon_char(&"✳Thinking".into(), &[]).is_none());

    // A run of the same symbol collapses to a single glyph.
    let (icon, title, _) = split_leading_icon_char(&">>> Thinking".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), ">");
    assert_eq!(title.as_ref(), "Thinking");

    // Surrounding ASCII brackets are stripped so the inner glyph is used.
    let (icon, title, _) = split_leading_icon_char(&"[!] codex waiting".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "!");
    assert_eq!(title.as_ref(), "codex waiting");

    // A run of dots is condensed into an ellipsis.
    let (icon, title, _) = split_leading_icon_char(&"... working".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "\u{2026}");
    assert_eq!(title.as_ref(), "working");

    let (icon, title, _) = split_leading_icon_char(&"[...] working".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "\u{2026}");
    assert_eq!(title.as_ref(), "working");

    let (icon, title, _) = split_leading_icon_char(&"[…] working".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "\u{2026}");
    assert_eq!(title.as_ref(), "working");

    // Multi-codepoint emoji are kept intact rather than sliced mid-cluster.
    let (icon, title, _) = split_leading_icon_char(&"🇺🇸 flag".into(), &[]).unwrap();
    assert_eq!(icon.as_ref(), "🇺🇸");
    assert_eq!(title.as_ref(), "flag");

    // Highlight positions are shifted to account for the stripped prefix, and
    // positions that fall inside the stripped prefix are dropped.
    let title: SharedString = "# abc".into();
    let abc_offset = title.find('a').unwrap();
    let (icon, trimmed, positions) =
        split_leading_icon_char(&title, &[0, abc_offset, abc_offset + 1]).unwrap();
    assert_eq!(icon.as_ref(), "#");
    assert_eq!(trimmed.as_ref(), "abc");
    assert_eq!(positions, vec![0, 1]);
}

#[gpui::test]
async fn test_find_or_create_workspace_returns_the_created_remote_workspace(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let local_project = init_test_project("/local", cx).await;
    cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });
    server_cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(local_project, window, cx));
    let local_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree("/remote-project", serde_json::json!({ "src": {} }))
        .await;
    let (opts, server_session, _) = remote::RemoteClient::fake_server(cx, server_cx);
    server_cx.update(remote_server::HeadlessProject::init);
    let server_executor = server_cx.executor();
    let _headless = server_cx.new(|cx| {
        remote_server::HeadlessProject::new(
            remote_server::HeadlessAppState {
                session: server_session,
                fs: server_fs.clone(),
                http_client: Arc::new(http_client::BlockedHttpClient),
                node_runtime: node_runtime::NodeRuntime::unavailable(),
                languages: Arc::new(language::LanguageRegistry::new(server_executor)),
                extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });
    let remote_client = remote::RemoteClient::connect_mock(opts.clone(), cx).await;

    // Stand in for the save prompt from a concurrent workspace removal: as
    // soon as the remote workspace is activated mid-open, activate the local
    // workspace again. The open must still return the workspace it created,
    // not whichever workspace is active once it finishes.
    multi_workspace.update_in(cx, |_, window, cx| {
        let local_workspace = local_workspace.clone();
        cx.subscribe_in(&cx.entity(), window, move |this, _, event, window, cx| {
            if matches!(event, MultiWorkspaceEvent::WorkspaceAdded(_)) {
                this.activate(local_workspace.clone(), None, window, cx);
            }
        })
        .detach();
    });

    let created = multi_workspace
        .update_in(cx, |mw, window, cx| {
            let key = ProjectGroupKey::new(
                Some(opts.clone()),
                PathList::new(&[PathBuf::from("/remote-project")]),
            );
            mw.find_or_create_workspace(
                PathList::new(&[PathBuf::from("/remote-project")]),
                Some(opts),
                Some(key),
                move |_, _, _| Task::ready(Ok(Some(remote_client))),
                None,
                workspace::OpenMode::Activate,
                None,
                window,
                cx,
            )
        })
        .await
        .expect("opening the remote project should succeed");
    cx.run_until_parked();

    assert_eq!(
        created.read_with(cx, |workspace, cx| PathList::new(&workspace.root_paths(cx))),
        PathList::new(&[PathBuf::from("/remote-project")]),
        "the returned workspace should be the remote workspace that was created"
    );
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone()),
        local_workspace,
        "the local workspace should have re-activated during the open"
    );
}

fn set_sidebar_test_panel_alpha(alpha: f32, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        let mut theme = cx.theme().as_ref().clone();
        theme.styles.colors.background = Hsla::from(gpui::rgb(0xdcdcdd));
        theme.styles.colors.surface_background = Hsla::from(gpui::rgb(0xff00ff));
        theme.styles.colors.panel_background = Hsla::from(gpui::rgb(0xebebec)).alpha(alpha);
        theme.styles.colors.element_background = Hsla::from(gpui::rgb(0xebebec));
        theme.styles.colors.ghost_element_hover = Hsla::from(gpui::rgb(0xdfdfe0));
        theme::GlobalTheme::update_theme(cx, Arc::new(theme));
        window.refresh();
    });
    cx.run_until_parked();
}

fn assert_sidebar_action_hover(
    selector: &'static str,
    row_bounds: Bounds<Pixels>,
    cx: &mut VisualTestContext,
) {
    cx.simulate_mouse_move(row_bounds.center(), None, Modifiers::default());
    let button_bounds = cx.debug_bounds(selector).expect("visible row action");
    assert_eq!(
        row_bounds.intersect(&button_bounds),
        button_bounds,
        "{selector}: action must belong to the row"
    );
    cx.simulate_mouse_move(button_bounds.center(), None, Modifiers::default());
    let background = sidebar_painted_background_at(button_bounds.center(), cx);
    assert_ne!(
        background,
        sidebar_painted_background_at(row_bounds.center(), cx),
        "{selector}: hovered action must contrast with the row"
    );
}

fn sidebar_painted_background_at(position: Point<Pixels>, cx: &mut VisualTestContext) -> u32 {
    cx.update(|window, _| {
        let position = position.scale(window.scale_factor());
        let color = window
            .painted_quads()
            .into_iter()
            .filter(|quad| {
                !quad.background.is_transparent()
                    && quad
                        .bounds
                        .intersect(&quad.content_mask.bounds)
                        .contains(&position)
            })
            .max_by_key(|quad| quad.order)
            .expect("painted background at pointer")
            .background
            .as_solid()
            .expect("solid background at pointer");
        u32::from(Rgba::from(color))
    })
}

#[derive(Debug, PartialEq)]
enum FocusedRegion {
    TitleBar,
    Sidebar,
    Panel,
    Pane,
}

#[derive(Debug, PartialEq)]
struct WindowCorners {
    top_left: WindowCorner,
    top_right: WindowCorner,
    bottom_left: WindowCorner,
    bottom_right: WindowCorner,
}

#[derive(Debug, PartialEq)]
enum WindowCorner {
    Rounded,
    Square,
    Overflow,
    Gap,
}

fn window_corners(cx: &mut VisualTestContext) -> WindowCorners {
    cx.update(|window, _| {
        let scale_factor = window.scale_factor();
        let frame = Bounds::new(Point::default(), window.viewport_size())
            .inset(theme::CLIENT_SIDE_DECORATION_SHADOW)
            .scale(scale_factor);
        let quads = window.painted_quads();
        let outline = quads
            .iter()
            .find(|quad| quad.bounds == frame && !quad.border_color.is_transparent())
            .expect("window frame border");
        let probe_offset = 1.5 * scale_factor;
        let corner = |x: ScaledPixels, y: ScaledPixels, direction_x: f32, direction_y: f32| {
            let probe = point(
                ScaledPixels(x.0 + direction_x * probe_offset),
                ScaledPixels(y.0 + direction_y * probe_offset),
            );
            let inside_outline =
                rounded_bounds_contain(outline.bounds, outline.corner_radii, probe);
            let painted = quads.iter().any(|quad| {
                !quad.background.is_transparent()
                    && quad.content_mask.bounds.contains(&probe)
                    && rounded_bounds_contain(quad.bounds, quad.corner_radii, probe)
            });
            match (inside_outline, painted) {
                (false, false) => WindowCorner::Rounded,
                (true, true) => WindowCorner::Square,
                (false, true) => WindowCorner::Overflow,
                (true, false) => WindowCorner::Gap,
            }
        };
        WindowCorners {
            top_left: corner(frame.left(), frame.top(), 1., 1.),
            top_right: corner(frame.right(), frame.top(), -1., 1.),
            bottom_right: corner(frame.right(), frame.bottom(), -1., -1.),
            bottom_left: corner(frame.left(), frame.bottom(), 1., -1.),
        }
    })
}

fn rounded_bounds_contain(
    bounds: Bounds<ScaledPixels>,
    corner_radii: Corners<ScaledPixels>,
    position: Point<ScaledPixels>,
) -> bool {
    bounds.contains(&position)
        && [
            (corner_radii.top_left, bounds.left(), bounds.top(), 1., 1.),
            (
                corner_radii.top_right,
                bounds.right(),
                bounds.top(),
                -1.,
                1.,
            ),
            (
                corner_radii.bottom_right,
                bounds.right(),
                bounds.bottom(),
                -1.,
                -1.,
            ),
            (
                corner_radii.bottom_left,
                bounds.left(),
                bounds.bottom(),
                1.,
                -1.,
            ),
        ]
        .into_iter()
        .all(
            |(radius, x, y, direction_x, direction_y): (_, _, _, f32, f32)| {
                let center_x = x.0 + direction_x * radius.0;
                let center_y = y.0 + direction_y * radius.0;
                let distance_x = (center_x - position.x.0) * direction_x;
                let distance_y = (center_y - position.y.0) * direction_y;
                distance_x <= 0. || distance_y <= 0. || distance_x.hypot(distance_y) <= radius.0
            },
        )
}

#[cfg(target_os = "macos")]
struct HeaderSearchOffsets {
    shown: Pixels,
    hidden: Pixels,
    fullscreen: Pixels,
}

#[cfg(target_os = "macos")]
fn header_search_offsets(
    selector: &'static str,
    cx: &mut VisualTestContext,
) -> HeaderSearchOffsets {
    let offset = |visibility, cx: &mut VisualTestContext| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.title_bar.get_or_insert_default().show = Some(visibility);
            });
        });
        cx.run_until_parked();
        cx.debug_bounds(selector)
            .expect("header search is rendered")
            .left()
    };
    let shown = offset(TitleBarVisibility::Always, cx);
    let hidden = offset(TitleBarVisibility::Never, cx);
    cx.update(|window, _| window.toggle_fullscreen());
    cx.run_until_parked();
    let fullscreen = offset(TitleBarVisibility::Always, cx);
    cx.update(|window, _| window.toggle_fullscreen());
    cx.run_until_parked();
    HeaderSearchOffsets {
        shown,
        hidden,
        fullscreen,
    }
}

fn menu_label_names(labels: &[WorkspaceMenuWorktreeLabel]) -> Vec<(String, Option<String>)> {
    labels
        .iter()
        .map(|label| {
            (
                label.primary_name.to_string(),
                label.secondary_name.as_ref().map(|name| name.to_string()),
            )
        })
        .collect()
}

fn project_menu_handle(
    sidebar: &Entity<Sidebar>,
    project_group_key: &ProjectGroupKey,
    cx: &mut gpui::VisualTestContext,
) -> PopoverMenuHandle<ContextMenu> {
    sidebar
        .read_with(cx, |sidebar, _| {
            let header_index = sidebar.contents.entries.iter().position(|entry| {
                matches!(entry, ListEntry::ProjectHeader { key, .. } if key == project_group_key)
            })?;
            sidebar
                .project_header_menu_handles
                .get(&header_index)
                .cloned()
        })
        .unwrap()
}

fn simulate_frames(cx: &mut gpui::VisualTestContext) {
    for _ in 0..2 {
        cx.update(|window, cx| window.simulate_next_frame(cx));
        cx.run_until_parked();
    }
}

fn active_root_paths(
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut gpui::VisualTestContext,
) -> Vec<PathBuf> {
    multi_workspace.read_with(cx, |multi_workspace, cx| {
        multi_workspace
            .workspace()
            .read(cx)
            .root_paths(cx)
            .iter()
            .map(|root_path| root_path.to_path_buf())
            .collect()
    })
}

fn worktree_header_ids(
    sidebar: &Entity<Sidebar>,
    cx: &mut gpui::VisualTestContext,
) -> Vec<SharedString> {
    sidebar.read_with(cx, |sidebar, _| {
        worktree_group_ids(&sidebar.contents.entries)
            .cloned()
            .collect()
    })
}

async fn entries_after_folder_change(
    sidebar: &Entity<Sidebar>,
    project: &Entity<project::Project>,
    add_tools_folder: bool,
    cx: &mut gpui::VisualTestContext,
) -> Vec<String> {
    let tools_path = Path::new(util::path!("/tools"));
    if add_tools_folder {
        project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(tools_path, true, cx)
            })
            .await
            .unwrap();
    } else {
        project.update(cx, |project, cx| {
            let worktree_id = project
                .visible_worktrees(cx)
                .find(|worktree| worktree.read(cx).abs_path().as_ref() == tools_path)
                .map(|worktree| worktree.read(cx).id());
            if let Some(worktree_id) = worktree_id {
                project.remove_worktree(worktree_id, cx);
            }
        });
    }
    updated_entries(sidebar, cx)
}

fn save_terminal_metadata_with_main_paths(
    title: &'static str,
    folder_paths: PathList,
    main_worktree_paths: PathList,
    created_at: DateTime<Utc>,
    cx: &mut TestAppContext,
) {
    cx.update(|cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(
                TerminalThreadMetadata {
                    terminal_id: agent_ui::TerminalId::test_new(),
                    title: title.into(),
                    custom_title: None,
                    created_at,
                    worktree_paths: WorktreePaths::from_path_lists(
                        main_worktree_paths,
                        folder_paths,
                    )
                    .unwrap(),
                    remote_connection: None,
                    working_directory: None,
                },
                cx,
            );
        });
    });
    cx.run_until_parked();
}

fn confirm_project_menu_row(
    menu_handle: &PopoverMenuHandle<ContextMenu>,
    row: usize,
    cx: &mut gpui::VisualTestContext,
) -> bool {
    cx.update(|window, cx| {
        menu_handle.hide(cx);
        menu_handle.show(window, cx);
    });
    cx.run_until_parked();
    simulate_frames(cx);
    cx.dispatch_action(SelectFirst);
    for _ in 0..row {
        cx.dispatch_action(SelectNext);
    }
    cx.update(|window, cx| window.dispatch_action(Box::new(Confirm), cx));
    menu_handle.is_deployed()
}

fn updated_entries(sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext) -> Vec<String> {
    cx.run_until_parked();
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
    visible_entries_as_strings(sidebar, cx)
}

#[gpui::test]
async fn test_worktree_header_collapse_with_keyboard_search_and_new_entry(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, _panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);

    save_n_test_threads(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    let expanded = vec!["v [my-project]", "  ~ my-project", "  Thread 1"];
    assert_eq!(
        visible_entries_with_worktree_headers(&sidebar, cx),
        expanded
    );

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| sidebar.selection = Some(1));

    cx.dispatch_action(SelectParent);
    cx.run_until_parked();
    assert_eq!(
        visible_entries_with_worktree_headers(&sidebar, cx),
        vec!["v [my-project]", "  ~> my-project  <== selected"]
    );

    cx.dispatch_action(SelectChild);
    cx.run_until_parked();
    cx.dispatch_action(SelectChild);
    cx.run_until_parked();
    assert_eq!(
        visible_entries_with_worktree_headers(&sidebar, cx),
        vec![
            "v [my-project]",
            "  ~ my-project",
            "  Thread 1  <== selected"
        ]
    );

    let ids = worktree_header_ids(&sidebar, cx);
    let [id] = ids.as_slice() else {
        panic!("expected one worktree header, found {ids:?}");
    };

    type_in_search(&sidebar, "Thread", cx);
    sidebar.update(cx, |sidebar, cx| {
        sidebar.toggle_worktree_group(id, cx);
        assert!(
            sidebar.collapsed_worktree_groups.is_empty(),
            "toggling during a search must not change the saved state"
        );
    });
    type_in_search(&sidebar, "", cx);

    sidebar.update(cx, |sidebar, cx| sidebar.toggle_worktree_group(id, cx));
    cx.run_until_parked();
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    sidebar.update_in(cx, |sidebar, window, cx| {
        assert!(sidebar.collapsed_worktree_groups.contains(id));
        assert_eq!(
            sidebar.mru_entries_for_switcher(cx).len(),
            1,
            "a collapsed worktree group keeps its threads in the thread switcher"
        );
        sidebar.create_new_entry(&workspace, window, cx);
        assert!(
            sidebar.collapsed_worktree_groups.is_empty(),
            "a new entry must expand its worktree group"
        );
    });
}

#[gpui::test]
async fn test_worktree_groups_do_not_move_across_projects(cx: &mut TestAppContext) {
    let (fs, project_a) = init_multi_project_test(&["/project-a", "/project-b"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace_b = add_test_project("/project-b", &fs, &multi_workspace, cx).await;
    let project_b = workspace_b.read_with(cx, |workspace, _| workspace.project().clone());

    save_named_thread_metadata("thread-a", "Thread A", &project_a, cx).await;
    save_named_thread_metadata("thread-b", "Thread B", &project_b, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    let before = visible_entries_with_worktree_headers(&sidebar, cx);
    let ids = worktree_header_ids(&sidebar, cx);
    let [first, second] = ids.as_slice() else {
        panic!("expected one worktree header per project, found {ids:?}");
    };

    sidebar.update(cx, |sidebar, cx| {
        let order = sidebar.worktree_group_order.clone();
        sidebar.move_worktree_group(first, second, cx);
        assert_eq!(sidebar.worktree_group_order, order);
    });
    cx.run_until_parked();
    assert_eq!(visible_entries_with_worktree_headers(&sidebar, cx), before);
}

#[gpui::test]
async fn test_archiving_a_hidden_thread_leaves_the_agent_panel_open(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let (sidebar, panel) = setup_sidebar_with_agent_panel(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());

    let session_id = acp::SessionId::new(Arc::from("hidden-thread"));
    save_thread_metadata(
        session_id.clone(),
        Some("Hidden".into()),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        None,
        None,
        &project,
        cx,
    );
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_panel::<AgentPanel>(window, cx);
    });
    cx.run_until_parked();
    // A panel is briefly blank like this while its first terminal spawns.
    panel.update_in(cx, |panel, window, cx| panel.clear_base_view(window, cx));
    cx.run_until_parked();
    let open_dock_count = |cx: &mut gpui::VisualTestContext| {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .all_docks()
                .iter()
                .filter(|dock| dock.read(cx).is_open())
                .count()
        })
    };
    assert_eq!(open_dock_count(cx), 1);

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.archive_thread(&session_id, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(open_dock_count(cx), 1);
}

#[gpui::test]
async fn test_archiving_a_remote_thread_moves_to_the_next_remote_thread(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        workspace::init(app_state.clone(), cx);
        app_state
    });
    server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree(
            "/project",
            serde_json::json!({ ".git": {}, "src": { "main.rs": "" } }),
        )
        .await;
    let (project, _headless, _opts) = start_remote_project(
        &server_fs,
        Path::new("/project"),
        &app_state,
        None,
        cx,
        server_cx,
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(app_state.fs.clone(), cx));
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());

    let archived_session = acp::SessionId::new(Arc::from("remote-a"));
    let next_session = acp::SessionId::new(Arc::from("remote-b"));
    for (session, day) in [(&archived_session, 2), (&next_session, 1)] {
        save_thread_metadata(
            session.clone(),
            Some("Remote".into()),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, day, 0, 0, 0).unwrap(),
            None,
            None,
            &project,
            cx,
        );
    }
    cx.run_until_parked();
    let thread_id = |session: &acp::SessionId, cx: &mut gpui::VisualTestContext| {
        cx.update(|_, cx| {
            ThreadMetadataStore::global(cx)
                .read(cx)
                .entry_by_session(session)
                .unwrap()
                .thread_id
        })
    };
    let archived_id = thread_id(&archived_session, cx);
    let next_id = thread_id(&next_session, cx);

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.active_entry = Some(ActiveEntry::Thread {
            thread_id: archived_id,
            session_id: Some(archived_session.clone()),
            workspace: workspace.clone(),
        });
        sidebar.archive_thread(&archived_session, window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Thread { thread_id, .. }) if *thread_id == next_id)
        );
    });
}

#[gpui::test]
async fn test_drag_worktree_group_to_reorder_in_multi_root_project(cx: &mut TestAppContext) {
    let (fs, main_project) = init_delete_worktree_test(false, cx).await;
    fs.insert_tree("/other", serde_json::json!({ "src": {} }))
        .await;
    let multi_root_project = project::Project::test(
        fs.clone(),
        ["/other".as_ref(), DELETED_WORKTREE_PATH.as_ref()],
        cx,
    )
    .await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let multi_root_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(multi_root_project.clone(), window, cx)
    });
    let panel = add_agent_panel(&multi_root_workspace, cx);
    open_thread_with_connection(&panel, StubAgentConnection::new(), cx);
    send_message(&panel, cx);
    let session_id = active_session_id(&panel, cx);
    save_test_thread_metadata(&session_id, &multi_root_project, cx).await;
    cx.run_until_parked();

    let group_ids = |cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _| {
            let linked_id = sidebar
                .contents
                .entries
                .iter()
                .find_map(|entry| match entry {
                    ListEntry::WorktreeHeader { id, roots, .. }
                        if roots
                            .iter()
                            .any(|root| root.as_ref() == DELETED_WORKTREE_PATH) =>
                    {
                        Some(id.clone())
                    }
                    _ => None,
                })
                .expect("linked group exists");
            worktree_group_ids(&sidebar.contents.entries)
                .filter(|id| worktree_group_project(id) == worktree_group_project(&linked_id))
                .cloned()
                .collect::<Vec<_>>()
        })
    };
    let before = group_ids(cx);
    assert_eq!(before.len(), 2, "expected a main and a linked group");

    cx.draw(
        gpui::point(px(0.), px(0.)),
        gpui::size(px(400.), px(400.)),
        |_, _| sidebar.clone().into_any_element(),
    );
    let bounds = |id: &SharedString, cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _| {
            let index = sidebar
                .contents
                .entries
                .iter()
                .position(|entry| {
                    matches!(entry, ListEntry::WorktreeHeader { id: entry_id, .. } if entry_id == id)
                })
                .expect("header exists");
            sidebar
                .list_state
                .bounds_for_item(index)
                .expect("row is measured")
        })
    };
    let has_rows = |id: &SharedString, cx: &mut gpui::VisualTestContext| {
        sidebar.read_with(cx, |sidebar, _| {
            sidebar.contents.entries.iter().any(|entry| {
                matches!(entry, ListEntry::WorktreeHeader { id: entry_id, has_rows: true, .. } if entry_id == id)
            })
        })
    };
    let (with_rows, empty) = if has_rows(&before[0], cx) {
        (before[0].clone(), before[1].clone())
    } else {
        (before[1].clone(), before[0].clone())
    };
    // The "No threads yet" placeholder of an empty group is below its header.
    let from = bounds(&with_rows, cx).origin + gpui::point(px(40.), px(8.));
    let to = bounds(&empty, cx).bottom_left() + gpui::point(px(40.), px(-8.));
    cx.simulate_mouse_down(from, gpui::MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(
        from + gpui::point(px(0.), px(6.)),
        Some(gpui::MouseButton::Left),
        Modifiers::default(),
    );
    cx.simulate_mouse_move(to, Some(gpui::MouseButton::Left), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_mouse_up(to, gpui::MouseButton::Left, Modifiers::default());
    cx.run_until_parked();

    assert_eq!(
        group_ids(cx),
        before.iter().rev().cloned().collect::<Vec<_>>(),
        "dropping a group on another must swap them"
    );
}

#[test]
fn test_parse_gh_pull_request() {
    let parse = |state: &str, is_draft: bool| {
        parse_gh_pull_request(
            format!(
                r#"{{"isDraft":{is_draft},"state":"{state}","url":"https://example.com/pull/1"}}"#
            )
            .as_bytes(),
        )
    };
    let url = SharedString::from("https://example.com/pull/1");
    assert_eq!(
        parse("OPEN", false),
        Some((PullRequestState::Open, url.clone()))
    );
    assert_eq!(
        parse("OPEN", true),
        Some((PullRequestState::Draft, url.clone()))
    );
    assert_eq!(
        parse("MERGED", false),
        Some((PullRequestState::Merged, url.clone()))
    );
    assert_eq!(
        parse("CLOSED", false),
        Some((PullRequestState::Closed, url))
    );
    assert_eq!(parse("UNKNOWN", false), None);
    assert_eq!(parse_gh_pull_request(b"not json"), None);
}

#[test]
fn test_header_pull_request() {
    let target =
        |path: &str, branch: &str| (PathBuf::from(path), SharedString::from(branch.to_string()));
    let cached =
        |branch: &str, pull_request: Option<(PullRequestState, &str)>| WorktreePullRequest {
            branch: SharedString::from(branch.to_string()),
            state_and_url: pull_request
                .map(|(state, url)| (state, SharedString::from(url.to_string()))),
        };
    let cache: HashMap<PathBuf, WorktreePullRequest> = [
        (
            PathBuf::from("/a"),
            cached("feature-a", Some((PullRequestState::Open, "url-a"))),
        ),
        (
            PathBuf::from("/b"),
            cached("feature-b", Some((PullRequestState::Merged, "url-b"))),
        ),
        (PathBuf::from("/c"), cached("feature-c", None)),
    ]
    .into_iter()
    .collect();
    let header = |urls: &[&str], state| {
        Some(HeaderPullRequest {
            urls: urls
                .iter()
                .map(|url| SharedString::from(url.to_string()))
                .collect(),
            state,
        })
    };

    assert_eq!(
        header_pull_request(&[target("/a", "feature-a")], &cache),
        header(&["url-a"], PullRequestState::Open)
    );
    assert_eq!(header_pull_request(&[target("/a", "other")], &cache), None);
    assert_eq!(
        header_pull_request(&[target("/c", "feature-c")], &cache),
        None
    );
    assert_eq!(
        header_pull_request(&[target("/unknown", "feature-a")], &cache),
        None
    );
    assert_eq!(
        header_pull_request(
            &[target("/c", "feature-c"), target("/b", "feature-b")],
            &cache
        ),
        header(&["url-b"], PullRequestState::Merged)
    );
    assert_eq!(
        header_pull_request(
            &[target("/b", "feature-b"), target("/a", "feature-a")],
            &cache
        ),
        header(&["url-b", "url-a"], PullRequestState::Open)
    );
}

#[gpui::test]
async fn test_record_pull_request_lookup(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let path = PathBuf::from("/my-project");
    let branch = SharedString::from("feature");
    let open = (PullRequestState::Open, SharedString::from("url"));

    sidebar.update(cx, |sidebar, cx| {
        sidebar.record_pull_request_lookup(
            path.clone(),
            branch.clone(),
            Ok(Some(open.clone())),
            cx,
        );
        assert_eq!(
            sidebar.worktree_pull_requests.get(&path),
            Some(&WorktreePullRequest {
                branch: branch.clone(),
                state_and_url: Some(open.clone()),
            })
        );
        assert!(sidebar.update_task.is_some(), "a new result must rebuild");
    });
    cx.run_until_parked();

    sidebar.update(cx, |sidebar, cx| {
        sidebar.record_pull_request_lookup(path.clone(), branch.clone(), Ok(Some(open)), cx);
        assert!(
            sidebar.update_task.is_none(),
            "an unchanged result must not rebuild"
        );

        sidebar.record_pull_request_lookup(
            path.clone(),
            branch.clone(),
            Err(anyhow::anyhow!("offline")),
            cx,
        );
        assert_eq!(
            sidebar.worktree_pull_requests.get(&path),
            Some(&WorktreePullRequest {
                branch,
                state_and_url: None,
            }),
            "a failed lookup must not leave a stale pull request"
        );
        assert!(sidebar.update_task.is_some());
    });
}
