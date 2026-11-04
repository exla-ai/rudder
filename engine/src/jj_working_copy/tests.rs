use std::collections::BTreeSet;
use std::process::Command;
use std::time::Instant;

use jj_lib::config::ConfigLayer;
use jj_lib::config::ConfigSource;
use jj_lib::config::StackedConfig;
use jj_lib::matchers::NothingMatcher;
use jj_lib::repo::ReadonlyRepo;
use jj_lib::repo::Repo as _;
use jj_lib::workspace::Workspace;
use pollster::FutureExt as _;

use super::*;

fn settings() -> UserSettings {
    let mut config = StackedConfig::with_defaults();
    let mut layer = ConfigLayer::empty(ConfigSource::User);
    layer.set_value("user.name", "Overlay test").unwrap();
    layer.set_value("user.email", "overlay@invalid.example").unwrap();
    config.add_layer(layer);
    UserSettings::from_config(config).unwrap()
}

fn rp(path: &str) -> RepoPathBuf {
    RepoPathBuf::from_internal_string(path).unwrap()
}

fn tree_with(store: &Arc<Store>, files: &[(String, &str, bool)]) -> MergedTree {
    let mut builder = MergedTreeBuilder::new(store.empty_merged_tree());
    for (path, contents, executable) in files {
        let path = rp(path);
        let id = store.write_file(&path, &mut contents.as_bytes()).block_on().unwrap();
        builder.set_or_remove(
            path,
            Merge::normal(TreeValue::File { id, executable: *executable, copy_id: CopyId::placeholder() }),
        );
    }
    builder.write_tree().block_on().unwrap()
}

fn commit(repo: &Arc<ReadonlyRepo>, tree: MergedTree) -> (Arc<ReadonlyRepo>, Commit) {
    let mut tx = repo.start_transaction();
    let commit = tx
        .repo_mut()
        .new_commit(vec![repo.store().root_commit_id().clone()], tree)
        .write()
        .block_on()
        .unwrap();
    let repo = tx.commit("test commit").block_on().unwrap();
    (repo, commit)
}

fn changed_paths(a: &MergedTree, b: &MergedTree) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut diff = a.diff_stream(b, &EverythingMatcher);
    while let Some(entry) = diff.next().block_on() {
        entry.values.unwrap();
        out.insert(entry.path.as_internal_file_string().to_owned());
    }
    out
}

fn snapshot(locked: &mut dyn LockedWorkingCopy) -> (MergedTree, SnapshotStats) {
    let options = SnapshotOptions {
        base_ignores: GitIgnoreFile::empty(),
        progress: None,
        start_tracking_matcher: &EverythingMatcher,
        force_tracking_matcher: &NothingMatcher,
        max_new_file_size: 1 << 20,
    };
    locked.snapshot(&options).block_on().unwrap()
}

fn count_files(dir: &Path) -> usize {
    let mut n = 0;
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            n += count_files(&entry.path());
        } else {
            n += 1;
        }
    }
    n
}

/// Runs `script` with the overlay mounted at $M in a private user+mount
/// namespace, the same rootless mount the workspace helper makes.
fn in_overlay(mount: &OverlayMount, script: &str) -> bool {
    fs::create_dir_all(&mount.merged).unwrap();
    let full = format!("mount -t overlay overlay -o {} \"$M\" && cd \"$M\" && {script}", mount.options());
    let status = Command::new("unshare")
        .args(["-Urm", "sh", "-ec", &full])
        .env("M", &mount.merged)
        .status();
    matches!(status, Ok(s) if s.success())
}

#[test]
fn overlay_fork_on_local_backend() {
    run_fork_test(false);
}

/// Same flow on the Rudder content-store backend (`jj_store`).
#[test]
fn overlay_fork_on_rudder_backend() {
    run_fork_test(true);
}

fn run_fork_test(rudder_backend: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let settings = settings();
    let repo_root = tmp.path().join("repo");
    fs::create_dir(&repo_root).unwrap();
    let (_ws, repo) = if rudder_backend {
        crate::jj_store::init_workspace(&settings, &repo_root, &tmp.path().join("cas")).unwrap()
    } else {
        Workspace::init_simple(&settings, &repo_root).block_on().unwrap()
    };
    let store = repo.store().clone();

    // A tree with many files in nested directories.
    let mut files: Vec<(String, &str, bool)> = (0..2000)
        .map(|i| (format!("src/d{}/f{i}.txt", i % 40), "original\n", false))
        .collect();
    files.push(("keep.txt".into(), "keep\n", false));
    files.push(("edit.txt".into(), "before\n", false));
    files.push(("gone.txt".into(), "bye\n", false));
    files.push(("tool.sh".into(), "#!/bin/sh\n", false));
    files.push(("swap/inner.txt".into(), "dir becomes file\n", false));
    let tree1 = tree_with(&store, &files);
    let (repo, commit1) = commit(&repo, tree1.clone());

    let lowers = tmp.path().join("lowers");
    let factory = OverlayWorkingCopyFactory { lowers_root: Some(lowers.clone()) };
    let new_wc = |name: &str| {
        factory
            .init_working_copy(
                store.clone(),
                tmp.path().join(format!("{name}-merged")),
                tmp.path().join(format!("{name}-state")),
                repo.op_id().clone(),
                WorkspaceNameBuf::from(name.to_owned()),
                &settings,
            )
            .unwrap()
    };

    // First fork of tree1 writes the shared lower once.
    let wc = new_wc("a");
    let mut locked = wc.start_mutation().unwrap();
    locked.check_out(&commit1).block_on().unwrap();
    let wc = locked.finish(repo.op_id().clone()).block_on().unwrap();
    let lower1 = wc.downcast_ref::<OverlayWorkingCopy>().unwrap().mount().lower;
    assert_eq!(count_files(&lower1), files.len());

    // Second fork of the same tree: no file is copied.
    let started = Instant::now();
    let fork = new_wc("b");
    let mut locked = fork.start_mutation().unwrap();
    let stats = locked.check_out(&commit1).block_on().unwrap();
    let fork = locked.finish(repo.op_id().clone()).block_on().unwrap();
    let elapsed = started.elapsed();
    let mount = fork.downcast_ref::<OverlayWorkingCopy>().unwrap().mount();
    assert_eq!(mount.lower, lower1, "forks share one lower directory");
    assert_eq!(count_files(&mount.upper), 0, "fork upper starts empty");
    assert_eq!(count_files(&lowers), files.len(), "no second copy of the tree");
    assert_eq!(stats.added_files as usize, files.len());
    eprintln!("fork of {} files took {elapsed:?}, copied 0 files", files.len());
    assert_eq!(fork.tree().unwrap().tree_ids(), tree1.tree_ids());

    // Edit through a real overlay mount.
    let script = "printf after > edit.txt; rm gone.txt; mkdir -p new/dir; printf hi > new/dir/x.txt; \
         chmod +x tool.sh; rm -r src/d7; rm -r swap; printf file > swap; \
         printf '*.log\\n' > .gitignore; printf noise > debug.log; ln -s keep.txt link";
    if !in_overlay(&mount, script) {
        eprintln!("skipping overlay edits: rootless OverlayFS unavailable here");
        return;
    }
    assert!(count_files(&mount.upper) < 20, "upper holds only the changes");

    let mut locked = fork.start_mutation().unwrap();
    let (tree2, _stats) = snapshot(locked.as_mut());
    let mut expected: BTreeSet<String> = [
        "edit.txt", "gone.txt", "new/dir/x.txt", "tool.sh", "swap", "swap/inner.txt", ".gitignore", "link",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    expected.extend((0..2000).filter(|i| i % 40 == 7).map(|i| format!("src/d7/f{i}.txt")));
    assert_eq!(changed_paths(&tree1, &tree2), expected);
    match tree2.path_value(&rp("tool.sh")).block_on().unwrap().into_resolved() {
        Ok(Some(TreeValue::File { executable, .. })) => assert!(executable),
        other => panic!("unexpected tool.sh value {other:?}"),
    }
    // Snapshot is repeatable.
    let (again, _) = snapshot(locked.as_mut());
    assert_eq!(again.tree_ids(), tree2.tree_ids());
    let fork = locked.finish(repo.op_id().clone()).block_on().unwrap();
    assert_eq!(fork.tree().unwrap().tree_ids(), tree2.tree_ids());

    // Check out another commit: lower moves, upper resets, no copies.
    let tree3 = tree_with(&store, &[("only.txt".into(), "three\n", false)]);
    let (repo, commit3) = commit(&repo, tree3.clone());
    let mut locked = fork.start_mutation().unwrap();
    let stats = locked.check_out(&commit3).block_on().unwrap();
    assert_eq!(stats.added_files, 1);
    let (snap3, _) = snapshot(locked.as_mut());
    assert_eq!(snap3.tree_ids(), tree3.tree_ids());
    let fork = locked.finish(repo.op_id().clone()).block_on().unwrap();
    let mount3 = fork.downcast_ref::<OverlayWorkingCopy>().unwrap().mount();
    assert_ne!(mount3.lower, lower1);
    assert_eq!(count_files(&mount3.upper), 0);
    assert_eq!(read_mount(fork.downcast_ref::<OverlayWorkingCopy>().unwrap().state_path()).unwrap(), mount3);
}
