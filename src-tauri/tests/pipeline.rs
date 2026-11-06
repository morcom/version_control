use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::sync::Arc;

use file_history_tracker_lib::app::AppState;
use file_history_tracker_lib::db::{now_millis, Db};
use file_history_tracker_lib::ingest::{
    apply_rename, ingest_path, load_version_bytes, mark_removed, reconcile_folder,
};
use file_history_tracker_lib::model::AppSettings;
use file_history_tracker_lib::restore::{restore_as, restore_in_place};
use file_history_tracker_lib::retention::run_cleanup;
use file_history_tracker_lib::store::BlobStore;
use file_history_tracker_lib::watcher::Job;

struct Harness {
    state: Arc<AppState>,
    root: PathBuf,
    folder_id: i64,
    _temp: tempfile::TempDir,
}

fn harness(retention_days: i64) -> Harness {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("watched");
    fs::create_dir_all(&root).unwrap();
    let db = Arc::new(Db::open(&temp.path().join("history.db")).unwrap());
    let store = Arc::new(BlobStore::new(temp.path().join("objects")).unwrap());
    let (tx, _rx) = channel::<Job>();
    let settings = AppSettings {
        data_dir: temp.path().to_string_lossy().to_string(),
        ..AppSettings::default()
    };
    let state = AppState::new(db, store, settings, tx);
    let folder_id = state
        .db
        .insert_folder(
            &root.to_string_lossy(),
            retention_days,
            true,
            "",
            32,
            now_millis(),
        )
        .unwrap();
    state.reload_folders().unwrap();
    Harness {
        state,
        root,
        folder_id,
        _temp: temp,
    }
}

fn write_file(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
}

#[test]
fn stores_a_version_for_every_save_and_can_restore_them() {
    let harness = harness(90);
    let file = harness.root.join("notes.txt");
    write_file(&file, "first version\n");

    ingest_path(&harness.state, harness.folder_id, &file, "created").unwrap();
    write_file(&file, "second version\n");
    ingest_path(&harness.state, harness.folder_id, &file, "modified").unwrap();
    write_file(&file, "third version\n");
    ingest_path(&harness.state, harness.folder_id, &file, "modified").unwrap();

    let files = harness.state.db.list_files(harness.folder_id, true, "").unwrap();
    assert_eq!(files.len(), 1);
    let versions = harness.state.db.list_versions(files[0].id).unwrap();
    assert_eq!(versions.len(), 3);
    assert_eq!(versions[0].event, "modified");
    assert_eq!(versions[2].event, "created");

    let oldest = versions.iter().find(|v| v.event == "created").unwrap();
    let newest = &versions[0];
    assert_eq!(
        String::from_utf8(load_version_bytes(&harness.state, oldest.id).unwrap()).unwrap(),
        "first version\n"
    );
    assert_eq!(
        String::from_utf8(load_version_bytes(&harness.state, newest.id).unwrap()).unwrap(),
        "third version\n"
    );

    let target = harness.root.join("restored.txt");
    let outcome = restore_as(&harness.state, oldest.id, &target).unwrap();
    assert!(outcome.tracked);
    assert_eq!(fs::read_to_string(&target).unwrap(), "first version\n");
}

#[test]
fn unchanged_content_does_not_create_a_new_version() {
    let harness = harness(90);
    let file = harness.root.join("notes.txt");
    write_file(&file, "stable\n");
    ingest_path(&harness.state, harness.folder_id, &file, "created").unwrap();
    ingest_path(&harness.state, harness.folder_id, &file, "modified").unwrap();
    let files = harness.state.db.list_files(harness.folder_id, true, "").unwrap();
    let versions = harness.state.db.list_versions(files[0].id).unwrap();
    assert_eq!(versions.len(), 1);
}

#[test]
fn small_changes_are_stored_as_deltas() {
    let harness = harness(90);
    let file = harness.root.join("big.txt");
    let base = "line of text that repeats over and over\n".repeat(5_000);
    write_file(&file, &base);
    ingest_path(&harness.state, harness.folder_id, &file, "created").unwrap();
    let mut changed = base.clone();
    changed.push_str("one extra line\n");
    write_file(&file, &changed);
    ingest_path(&harness.state, harness.folder_id, &file, "modified").unwrap();

    let files = harness.state.db.list_files(harness.folder_id, true, "").unwrap();
    let versions = harness.state.db.list_versions(files[0].id).unwrap();
    let newest = &versions[0];
    let blob = harness.state.db.blob_row(newest.blob_id).unwrap().unwrap();
    assert_eq!(blob.kind, "delta");
    assert!(blob.stored_size < blob.raw_size / 2);
    assert_eq!(
        String::from_utf8(load_version_bytes(&harness.state, newest.id).unwrap()).unwrap(),
        changed
    );
}

#[test]
fn renames_keep_the_history_and_deletions_stay_restorable() {
    let harness = harness(90);
    let file = harness.root.join("original.txt");
    write_file(&file, "content\n");
    ingest_path(&harness.state, harness.folder_id, &file, "created").unwrap();

    let renamed = harness.root.join("renamed.txt");
    fs::rename(&file, &renamed).unwrap();
    apply_rename(
        &harness.state,
        harness.folder_id,
        &file,
        &renamed,
    )
    .unwrap();
    ingest_path(&harness.state, harness.folder_id, &renamed, "renamed").unwrap();

    let files = harness.state.db.list_files(harness.folder_id, true, "").unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].rel_path, "renamed.txt");
    assert_eq!(files[0].version_count, 2);

    fs::remove_file(&renamed).unwrap();
    mark_removed(&harness.state, harness.folder_id, &renamed).unwrap();
    let files = harness.state.db.list_files(harness.folder_id, true, "").unwrap();
    assert_eq!(files[0].status, "deleted");

    let versions = harness.state.db.list_versions(files[0].id).unwrap();
    assert!(!versions.is_empty());
    assert_eq!(
        String::from_utf8(load_version_bytes(&harness.state, versions[0].id).unwrap()).unwrap(),
        "content\n"
    );
}

#[test]
fn restore_in_place_writes_the_old_content_back() {
    let harness = harness(90);
    let file = harness.root.join("doc.txt");
    write_file(&file, "original\n");
    ingest_path(&harness.state, harness.folder_id, &file, "created").unwrap();
    write_file(&file, "changed\n");
    ingest_path(&harness.state, harness.folder_id, &file, "modified").unwrap();

    let files = harness.state.db.list_files(harness.folder_id, true, "").unwrap();
    let versions = harness.state.db.list_versions(files[0].id).unwrap();
    let created = versions.iter().find(|v| v.event == "created").unwrap();

    let outcome = restore_in_place(&harness.state, created.id).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "original\n");
    assert_eq!(outcome.path, file.to_string_lossy());

    let versions = harness.state.db.list_versions(files[0].id).unwrap();
    assert!(
        versions.iter().any(|v| v.event == "pre-restore"),
        "the replaced content must be kept as a version"
    );
}

#[test]
fn reconcile_picks_up_changes_made_while_the_app_was_closed() {
    let harness = harness(90);
    let file = harness.root.join("offline.txt");
    write_file(&file, "before\n");
    reconcile_folder(&harness.state, harness.folder_id).unwrap();

    write_file(&file, "after\n");
    reconcile_folder(&harness.state, harness.folder_id).unwrap();

    let files = harness.state.db.list_files(harness.folder_id, true, "").unwrap();
    let versions = harness.state.db.list_versions(files[0].id).unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(
        String::from_utf8(load_version_bytes(&harness.state, versions[0].id).unwrap()).unwrap(),
        "after\n"
    );
}

#[test]
fn cleanup_removes_expired_versions_but_keeps_the_newest_and_pinned() {
    let harness = harness(1);
    let file = harness.root.join("old.txt");
    write_file(&file, "one\n");
    ingest_path(&harness.state, harness.folder_id, &file, "created").unwrap();
    write_file(&file, "two\n");
    ingest_path(&harness.state, harness.folder_id, &file, "modified").unwrap();
    write_file(&file, "three\n");
    ingest_path(&harness.state, harness.folder_id, &file, "modified").unwrap();

    let files = harness.state.db.list_files(harness.folder_id, true, "").unwrap();
    let file_id = files[0].id;
    let versions = harness.state.db.list_versions(file_id).unwrap();
    assert_eq!(versions.len(), 3);
    let oldest = versions
        .iter()
        .find(|v| v.event == "created")
        .unwrap()
        .id;

    harness.state.db.set_version_pin(oldest, true).unwrap();

    let future = now_millis() + 3 * 86_400_000;
    let report = run_cleanup(&harness.state, future).unwrap();
    assert!(report.removed_versions >= 1, "expired versions are removed");

    let remaining = harness.state.db.list_versions(file_id).unwrap();
    assert!(remaining.iter().any(|v| v.id == oldest), "pinned version survives");
    assert!(
        remaining.len() < 3,
        "thinning keeps only one version per day bucket"
    );
}

#[test]
fn excluded_patterns_are_not_tracked() {
    let harness = harness(90);
    let file = harness.root.join("~$draft.docx");
    write_file(&file, "lock file\n");
    ingest_path(&harness.state, harness.folder_id, &file, "created").unwrap();
    let files = harness.state.db.list_files(harness.folder_id, true, "").unwrap();
    assert!(files.is_empty(), "office lock files must be ignored");
}

fn versions_of(harness: &Harness, rel: &str) -> Vec<file_history_tracker_lib::model::VersionInfo> {
    let file = harness
        .state
        .db
        .find_file(harness.folder_id, rel)
        .unwrap()
        .unwrap_or_else(|| panic!("{rel} is not tracked"));
    harness.state.db.list_versions(file.id).unwrap()
}

#[test]
fn atomic_save_via_temp_file_is_versioned_not_deleted() {
    let harness = harness(90);
    let file = harness.root.join("report.txt");
    write_file(&file, "v1\n");
    ingest_path(&harness.state, harness.folder_id, &file, "created").unwrap();

    let temp = harness.root.join("report.txt.new");
    write_file(&temp, "v2\n");
    fs::remove_file(&file).unwrap();
    fs::rename(&temp, &file).unwrap();
    mark_removed(&harness.state, harness.folder_id, &file).unwrap();
    ingest_path(&harness.state, harness.folder_id, &file, "modified").unwrap();

    let files = harness.state.db.list_files(harness.folder_id, false, "report").unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].status, "active");
    assert_eq!(versions_of(&harness, "report.txt").len(), 2);
}

#[test]
fn rename_onto_existing_tracked_path_keeps_history() {
    let harness = harness(90);
    let a = harness.root.join("a.txt");
    let b = harness.root.join("b.txt");
    write_file(&a, "alpha 1\n");
    ingest_path(&harness.state, harness.folder_id, &a, "created").unwrap();
    write_file(&a, "alpha 2\n");
    ingest_path(&harness.state, harness.folder_id, &a, "modified").unwrap();
    write_file(&b, "beta\n");
    ingest_path(&harness.state, harness.folder_id, &b, "created").unwrap();

    fs::remove_file(&b).unwrap();
    fs::rename(&a, &b).unwrap();
    apply_rename(&harness.state, harness.folder_id, &a, &b).unwrap();

    let versions = versions_of(&harness, "b.txt");
    let contents: Vec<String> = versions
        .iter()
        .map(|v| String::from_utf8(load_version_bytes(&harness.state, v.id).unwrap()).unwrap())
        .collect();
    assert!(contents.contains(&"alpha 1\n".to_string()), "history of a.txt survives");
    assert!(contents.contains(&"beta\n".to_string()), "history of b.txt survives");
}

#[test]
fn directory_rename_moves_the_history_of_contained_files() {
    let harness = harness(90);
    let dir = harness.root.join("docs");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("note.txt");
    write_file(&file, "inside\n");
    ingest_path(&harness.state, harness.folder_id, &file, "created").unwrap();

    let moved = harness.root.join("archive");
    fs::rename(&dir, &moved).unwrap();
    apply_rename(&harness.state, harness.folder_id, &dir, &moved).unwrap();

    assert!(harness.state.db.find_file(harness.folder_id, "docs/note.txt").unwrap().is_none());
    let versions = versions_of(&harness, "archive/note.txt");
    assert!(versions.iter().any(|v| v.event == "created"));
}

#[test]
fn removing_a_folder_can_keep_or_delete_history() {
    let harness = harness(90);
    let file = harness.root.join("keep.txt");
    write_file(&file, "keep me\n");
    ingest_path(&harness.state, harness.folder_id, &file, "created").unwrap();

    harness.state.db.remove_folder(harness.folder_id, false).unwrap();
    assert!(harness.state.db.list_folder_rows().unwrap().is_empty());
    assert_eq!(
        harness.state.db.list_files(harness.folder_id, true, "").unwrap().len(),
        1,
        "history is kept"
    );

    harness.state.db.remove_folder(harness.folder_id, true).unwrap();
    assert!(harness.state.db.list_files(harness.folder_id, true, "").unwrap().is_empty());
    assert!(harness.state.db.folder_row(harness.folder_id).unwrap().is_none());
}

#[test]
fn integrity_check_completes_and_reports_no_problems() {
    let harness = harness(90);
    let file = harness.root.join("x.txt");
    write_file(&file, "one\n");
    ingest_path(&harness.state, harness.folder_id, &file, "created").unwrap();
    write_file(&file, "two\n");
    ingest_path(&harness.state, harness.folder_id, &file, "modified").unwrap();
    assert!(harness.state.db.check_integrity().unwrap().is_empty());
}

#[test]
fn global_pause_ignores_changes_and_deletions() {
    let harness = harness(90);
    let file = harness.root.join("p.txt");
    write_file(&file, "before\n");
    ingest_path(&harness.state, harness.folder_id, &file, "created").unwrap();
    harness.state.settings.write().unwrap().paused = true;

    write_file(&file, "after\n");
    ingest_path(&harness.state, harness.folder_id, &file, "modified").unwrap();
    fs::remove_file(&file).unwrap();
    mark_removed(&harness.state, harness.folder_id, &file).unwrap();

    let files = harness.state.db.list_files(harness.folder_id, true, "").unwrap();
    assert_eq!(files[0].status, "active");
    assert_eq!(versions_of(&harness, "p.txt").len(), 1);
}

#[test]
fn point_in_time_restore_skips_files_deleted_earlier() {
    let harness = harness(90);
    let gone = harness.root.join("gone.txt");
    let stays = harness.root.join("stays.txt");
    write_file(&gone, "bye\n");
    write_file(&stays, "hi\n");
    ingest_path(&harness.state, harness.folder_id, &gone, "created").unwrap();
    ingest_path(&harness.state, harness.folder_id, &stays, "created").unwrap();
    fs::remove_file(&gone).unwrap();
    mark_removed(&harness.state, harness.folder_id, &gone).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));

    let target = harness._temp.path().join("restored");
    let written = file_history_tracker_lib::restore::restore_folder_at(
        &harness.state,
        harness.folder_id,
        now_millis(),
        &target,
    )
    .unwrap();
    assert_eq!(written.len(), 1);
    assert!(target.join("stays.txt").exists());
    assert!(!target.join("gone.txt").exists());
}

#[test]
fn file_recreated_with_identical_content_gets_a_new_version_and_is_restored() {
    let harness = harness(90);
    let file = harness.root.join("comeback.txt");
    write_file(&file, "same bytes\n");
    ingest_path(&harness.state, harness.folder_id, &file, "created").unwrap();
    fs::remove_file(&file).unwrap();
    mark_removed(&harness.state, harness.folder_id, &file).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));

    write_file(&file, "same bytes\n");
    ingest_path(&harness.state, harness.folder_id, &file, "modified").unwrap();

    let versions = versions_of(&harness, "comeback.txt");
    assert_eq!(versions[0].event, "created", "latest version must not stay 'deleted'");
    assert!(versions.iter().any(|v| v.event == "deleted"));

    let target = harness._temp.path().join("pit");
    let written = file_history_tracker_lib::restore::restore_folder_at(
        &harness.state,
        harness.folder_id,
        now_millis(),
        &target,
    )
    .unwrap();
    assert_eq!(written.len(), 1);
    assert_eq!(fs::read_to_string(target.join("comeback.txt")).unwrap(), "same bytes\n");
}

#[test]
fn restore_in_place_is_recorded_as_restored_once() {
    let harness = harness(90);
    let file = harness.root.join("r.txt");
    write_file(&file, "first\n");
    ingest_path(&harness.state, harness.folder_id, &file, "created").unwrap();
    write_file(&file, "second\n");
    ingest_path(&harness.state, harness.folder_id, &file, "modified").unwrap();
    let created = versions_of(&harness, "r.txt")
        .into_iter()
        .find(|v| v.event == "created")
        .unwrap();

    restore_in_place(&harness.state, created.id).unwrap();
    ingest_path(&harness.state, harness.folder_id, &file, "modified").unwrap();

    let versions = versions_of(&harness, "r.txt");
    assert_eq!(versions[0].event, "restored");
    assert_eq!(versions.iter().filter(|v| v.event == "restored").count(), 1);
    assert_eq!(versions.iter().filter(|v| v.event == "modified").count(), 1, "watcher echo is deduplicated");
}

#[test]
fn removed_folders_keep_history_but_still_follow_retention() {
    let harness = harness(1);
    let file = harness.root.join("old.txt");
    for content in ["a\n", "b\n", "c\n"] {
        write_file(&file, content);
        ingest_path(&harness.state, harness.folder_id, &file, "modified").unwrap();
    }
    harness.state.db.remove_folder(harness.folder_id, false).unwrap();
    harness.state.reload_folders().unwrap();
    harness.state.settings.write().unwrap().clean_removed_folders = true;

    let report = run_cleanup(&harness.state, now_millis() + 3 * 86_400_000).unwrap();
    assert!(report.removed_versions >= 1);
    let remaining = versions_of(&harness, "old.txt");
    assert_eq!(remaining.len(), 1, "only the newest version survives");
    assert_eq!(
        String::from_utf8(load_version_bytes(&harness.state, remaining[0].id).unwrap()).unwrap(),
        "c\n"
    );
}

#[test]
fn removed_folders_are_not_cleaned_by_default() {
    let harness = harness(1);
    let file = harness.root.join("keep.txt");
    for content in ["a\n", "b\n", "c\n"] {
        write_file(&file, content);
        ingest_path(&harness.state, harness.folder_id, &file, "modified").unwrap();
    }
    harness.state.db.remove_folder(harness.folder_id, false).unwrap();
    harness.state.reload_folders().unwrap();
    assert!(!harness.state.settings.read().unwrap().clean_removed_folders);

    let report = run_cleanup(&harness.state, now_millis() + 3 * 86_400_000).unwrap();
    assert_eq!(report.removed_versions, 0);
    assert_eq!(versions_of(&harness, "keep.txt").len(), 3);
}

#[test]
fn rename_detected_by_content_keeps_the_old_name() {
    let harness = harness(90);
    let old = harness.root.join("draft.txt");
    let new = harness.root.join("final.txt");
    write_file(&old, "content to move\n");
    ingest_path(&harness.state, harness.folder_id, &old, "created").unwrap();

    fs::rename(&old, &new).unwrap();
    mark_removed(&harness.state, harness.folder_id, &old).unwrap();
    ingest_path(&harness.state, harness.folder_id, &new, "modified").unwrap();

    let versions = versions_of(&harness, "final.txt");
    assert_eq!(versions[0].event, "renamed");
    assert_eq!(versions[0].prev_path.as_deref(), Some("draft.txt"));
    assert!(harness.state.db.find_file(harness.folder_id, "draft.txt").unwrap().is_none());
}

#[test]
fn cleanup_preview_matches_what_cleanup_frees() {
    let harness = harness(1);
    let file = harness.root.join("p.txt");
    for i in 0..4 {
        write_file(&file, &format!("version {i} {}\n", "x".repeat(2000 * (i + 1))));
        ingest_path(&harness.state, harness.folder_id, &file, "modified").unwrap();
    }
    let later = now_millis() + 3 * 86_400_000;
    let preview = file_history_tracker_lib::retention::preview_cleanup(&harness.state, later).unwrap();
    let report = run_cleanup(&harness.state, later).unwrap();
    assert_eq!(preview.removable_versions, report.removed_versions);
    assert_eq!(preview.removable_bytes, report.freed_bytes);

    let soon = now_millis();
    let fresh_preview = file_history_tracker_lib::retention::preview_cleanup(&harness.state, soon).unwrap();
    assert_eq!(fresh_preview.removable_bytes, 0, "blobs inside the grace period are never counted");
}

#[cfg(windows)]
#[test]
fn restore_in_place_refuses_to_write_through_a_junction() {
    let harness = harness(90);
    let sub = harness.root.join("sub");
    fs::create_dir_all(&sub).unwrap();
    let file = sub.join("x.txt");
    write_file(&file, "original\n");
    ingest_path(&harness.state, harness.folder_id, &file, "created").unwrap();
    let version = versions_of(&harness, "sub/x.txt")[0].id;

    let outside = harness._temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::remove_dir_all(&sub).unwrap();
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&sub)
        .arg(&outside)
        .output()
        .unwrap();
    assert!(status.status.success(), "mklink /J failed");

    let result = restore_in_place(&harness.state, version);
    assert!(result.is_err(), "restore must refuse to follow the junction");
    assert!(!outside.join("x.txt").exists(), "nothing may be written outside the watched folder");
    let _ = fs::remove_dir(&sub);
}
