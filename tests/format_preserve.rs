//! Format-preserving save verification against the interdiction_array fixture.

use dmm_tools::dmm::{allocate_unused_key, save_preserve_format, Map, MapFormatSnapshot, Prefab};
use std::path::{Path, PathBuf};

fn fixture_source() -> PathBuf {
    PathBuf::from(r"C:\Users\robot\dev\Blastwave-content\_maps\BlastwaveRuins\interdiction_array.dmm")
}

fn copy_fixture(dir: &Path) -> PathBuf {
    let src = fixture_source();
    let dst = dir.join("interdiction_array.dmm");
    std::fs::copy(&src, &dst).expect("copy fixture");
    dst
}


#[test]
fn verification_a_round_trip_is_byte_identical() {
    let src = fixture_source();
    if !src.is_file() {
        eprintln!("Skipping: fixture not found at {}", src.display());
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let copy = copy_fixture(temp.path());
    let out = temp.path().join("saved.dmm");

    let map = Map::from_file(&copy).expect("open");
    let snapshot = MapFormatSnapshot::from_file(&copy, &map).expect("snapshot");
    save_preserve_format(&map, &snapshot, &out, false).expect("save");

    let original = std::fs::read(&copy).expect("read copy");
    let saved = std::fs::read(&out).expect("read saved");
    assert_eq!(original, saved);
}

#[test]
fn verification_b_single_object_small_diff() {
    let src = fixture_source();
    if !src.is_file() {
        eprintln!("Skipping: fixture not found at {}", src.display());
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let copy = copy_fixture(temp.path());
    let out = temp.path().join("saved.dmm");

    let mut map = Map::from_file(&copy).expect("open");
    let snapshot = MapFormatSnapshot::from_file(&copy, &map).expect("snapshot");

    // BYOND (30, 30, 1) → ndarray raw (z=0, y=29, x=29)
    let grid_raw = (0, 29, 29);
    let mut prefabs = map.dictionary[&map.grid[grid_raw]].clone();
    prefabs.insert(0, Prefab::from_path("/obj/effect/decal/cleanable/cigarette"));
    let new_key = allocate_unused_key(&map, &Default::default());
    map.dictionary.insert(new_key, prefabs);
    map.grid[grid_raw] = new_key;

    save_preserve_format(&map, &snapshot, &out, false).expect("save");

    Map::from_file(&out).expect("saved map must parse");

    let changed = diff_changed_lines(&copy, &out);
    eprintln!("verification B changed lines: {changed}");
    assert!(changed < 50, "expected small diff, got {changed} changed lines");
}

fn diff_changed_lines(a: &Path, b: &Path) -> usize {
    use std::process::Command;
    let output = Command::new("diff")
        .arg("-u")
        .arg(a)
        .arg(b)
        .output()
        .expect("run diff");
    // diff exits 1 when files differ
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| (line.starts_with('+') || line.starts_with('-')) && !line.starts_with("+++") && !line.starts_with("---"))
        .count()
}

#[test]
fn verification_c_set_turf_small_diff() {
    let src = fixture_source();
    if !src.is_file() {
        eprintln!("Skipping: fixture not found at {}", src.display());
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let copy = copy_fixture(temp.path());
    let out = temp.path().join("saved.dmm");

    let mut map = Map::from_file(&copy).expect("open");
    let snapshot = MapFormatSnapshot::from_file(&copy, &map).expect("snapshot");

    // BYOND (30, 30, 1) → ndarray raw (z=0, y=29, x=29)
    let grid_raw = (0, 29, 29);
    let mut prefabs = map.dictionary[&map.grid[grid_raw]].clone();
    prefabs.retain(|p| !p.path.starts_with("/turf/"));
    let insert_at = prefabs
        .iter()
        .position(|p| p.path.starts_with("/area/"))
        .unwrap_or(prefabs.len());
    prefabs.insert(insert_at, Prefab::from_path("/turf/open/floor/plating"));
    let new_key = allocate_unused_key(&map, &Default::default());
    map.dictionary.insert(new_key, prefabs);
    map.grid[grid_raw] = new_key;

    save_preserve_format(&map, &snapshot, &out, false).expect("save");

    let changed = diff_changed_lines(&copy, &out);
    eprintln!("verification C changed lines: {changed}");

    let saved_map = Map::from_file(&out).expect("re-parse saved");
    let saved_prefabs = &saved_map.dictionary[&saved_map.grid[grid_raw]];
    assert!(
        saved_prefabs.iter().any(|p| p.path == "/turf/open/floor/plating"),
        "saved tile must contain the new turf"
    );
    assert!(changed < 50, "expected small diff, got {changed} changed lines");
}
