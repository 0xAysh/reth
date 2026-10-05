//! The corpus manifest: the pinned set of fixture files, their digests, and their content counts.
//!
//! Loading reads the manifest and every fixture from disk and compares both directions, so a
//! fixture that is present but not manifested, manifested but missing, or manifested but changed
//! fails the corpus instead of silently going untested.

use super::parser::{
    git_revision, identifier, lowercase_hex, number, parse_fixture, Fixture, FixtureClass, Lines,
    ParamsName, ParseResult, GENERATOR_REVISION, GETH_REVISION,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

/// Per-fixture content counts recorded by the manifest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Counts {
    pub(super) completed_maps: usize,
    pub(super) partial_maps: usize,
    pub(super) rows: usize,
    pub(super) marks: usize,
    pub(super) queries: usize,
    pub(super) potential_indices: usize,
}

impl Counts {
    fn of(fixture: &Fixture) -> Self {
        Self {
            completed_maps: fixture.completed_maps.len(),
            partial_maps: fixture.partial_maps.len(),
            rows: fixture.row_count(),
            marks: fixture.mark_count(),
            queries: fixture.queries.len(),
            potential_indices: fixture.potential_count(),
        }
    }

    const fn accumulate(&mut self, other: Self) {
        self.completed_maps += other.completed_maps;
        self.partial_maps += other.partial_maps;
        self.rows += other.rows;
        self.marks += other.marks;
        self.queries += other.queries;
        self.potential_indices += other.potential_indices;
    }

    fn from_fields(fields: &[&str]) -> ParseResult<Self> {
        let [completed_maps, partial_maps, rows, marks, queries, potential_indices] = fields else {
            return Err("expected six content counts".to_owned())
        };
        Ok(Self {
            completed_maps: number(completed_maps, "completed map count")?,
            partial_maps: number(partial_maps, "private partial count")?,
            rows: number(rows, "row count")?,
            marks: number(marks, "mark count")?,
            queries: number(queries, "query count")?,
            potential_indices: number(potential_indices, "potential index count")?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ManifestEntry {
    /// Path relative to the corpus root, with `/` separators.
    pub(super) path: String,
    pub(super) class: FixtureClass,
    pub(super) params_name: ParamsName,
    pub(super) bytes: usize,
    /// Lowercase hex SHA-256 of the file's bytes.
    pub(super) digest: String,
    pub(super) counts: Counts,
}

/// Checks a manifested path against the corpus layout and returns the class its directory
/// implies together with the file stem.
fn classify_path(path: &str) -> ParseResult<(FixtureClass, &str)> {
    let segments = path.split('/').collect::<Vec<_>>();
    let (class, name) = match segments.as_slice() {
        ["curated", "end_to_end", name] => (FixtureClass::EndToEnd, *name),
        ["curated", "focused", name] => (FixtureClass::Focused, *name),
        ["stress", name] => (FixtureClass::Stress, *name),
        _ => return Err(format!("fixture path {path} is outside the corpus layout")),
    };
    let Some(stem) = name.strip_suffix(".txt") else {
        return Err(format!("fixture path {path} is not a .txt file"))
    };
    identifier(stem, "fixture stem")?;
    Ok((class, stem))
}

fn entry_from(fields: &[&str]) -> ParseResult<ManifestEntry> {
    let [_, path, kind, set, bytes, digest, counts @ ..] = fields else {
        return Err("malformed FILE".to_owned())
    };
    let (directory_class, _) = classify_path(path)?;
    let class = FixtureClass::parse(kind).ok_or_else(|| format!("invalid class: {kind}"))?;
    if class != directory_class {
        return Err(format!("{path} is filed under the wrong class directory"))
    }
    let params_name = ParamsName::parse(set).ok_or_else(|| format!("invalid params: {set}"))?;
    if digest.len() != 64 || !lowercase_hex(digest) {
        return Err(format!("malformed digest: {digest}"))
    }
    Ok(ManifestEntry {
        path: String::from(*path),
        class,
        params_name,
        bytes: number(bytes, "byte count")?,
        digest: String::from(*digest),
        counts: Counts::from_fields(counts)?,
    })
}

/// Parses the manifest text; `path` only labels error messages.
pub(super) fn parse_manifest(path: &str, text: &str) -> ParseResult<Vec<ManifestEntry>> {
    let mut lines = Lines::new(path, text)?;
    lines.exact(&["PIPELINE_MANIFEST", "1"])?;
    lines.exact(&["FIXTURE_FORMAT", "2"])?;
    let fields = lines.record("GETH")?;
    let [_, geth] = fields.as_slice() else { return Err(lines.error("malformed GETH")) };
    if lines.located(git_revision(geth, "Geth revision"))? != GETH_REVISION {
        return Err(lines.error("manifest names an unpinned Geth revision"))
    }
    let fields = lines.record("GENERATOR")?;
    let [_, generator] = fields.as_slice() else { return Err(lines.error("malformed GENERATOR")) };
    if lines.located(git_revision(generator, "generator revision"))? != GENERATOR_REVISION {
        return Err(lines.error("manifest names an unpinned generator revision"))
    }
    let fields = lines.record("FILES")?;
    let [_, declared] = fields.as_slice() else { return Err(lines.error("malformed FILES")) };
    let count: usize = lines.located(number(declared, "file count"))?;

    let mut entries: Vec<ManifestEntry> = Vec::new();
    let mut totals = Counts::default();
    let mut total_bytes = 0;
    for _ in 0..count {
        let fields = lines.record("FILE")?;
        let entry = lines.located(entry_from(&fields))?;
        if entries.last().is_some_and(|previous| previous.path >= entry.path) {
            return Err(lines.error("FILE records must be strictly ascending by path"))
        }
        totals.accumulate(entry.counts);
        total_bytes += entry.bytes;
        entries.push(entry);
    }
    let fields = lines.record("TOTALS")?;
    let [_, counts @ .., bytes] = fields.as_slice() else {
        return Err(lines.error("malformed TOTALS"))
    };
    let declared = lines.located(Counts::from_fields(counts))?;
    let declared_bytes: usize = lines.located(number(bytes, "total bytes"))?;
    if declared != totals || declared_bytes != total_bytes {
        return Err(lines.error("TOTALS disagree with the FILE records"))
    }
    lines.exact(&["END_MANIFEST"])?;
    lines.done()?;
    Ok(entries)
}

/// The directory holding `MANIFEST.txt` and the manifested fixtures.
fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/it/golden_pipeline/fixtures")
}

fn io<T>(result: std::io::Result<T>, path: &Path) -> ParseResult<T> {
    result.map_err(|error| format!("{}: {error}", path.display()))
}

/// Lists every regular file below `dir` as a `/`-separated path relative to the corpus root.
fn list_files(dir: &Path, prefix: &str, out: &mut Vec<String>) -> ParseResult<()> {
    for entry in io(fs::read_dir(dir), dir)? {
        let entry = io(entry, dir)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(format!("{}: non-UTF-8 file name", dir.display()))
        };
        let relative = if prefix.is_empty() { name.to_owned() } else { format!("{prefix}/{name}") };
        let kind = io(entry.file_type(), &entry.path())?;
        if kind.is_dir() {
            list_files(&entry.path(), &relative, out)?;
        } else if kind.is_file() {
            out.push(relative);
        } else {
            return Err(format!("{relative}: not a regular file or directory"))
        }
    }
    Ok(())
}

/// Checks that a manifest entry describes the fixture it names.
fn check_entry(entry: &ManifestEntry, fixture: &Fixture) -> ParseResult<()> {
    let path = &entry.path;
    let (_, stem) = classify_path(path)?;
    if fixture.scenario != stem {
        return Err(format!("{path}: scenario {} is not the file stem", fixture.scenario))
    }
    if fixture.class != entry.class || fixture.params_name != entry.params_name {
        return Err(format!("{path}: CLASS or PARAMS disagree with the manifest"))
    }
    if let Some(ordinal) = fixture.stress_ordinal &&
        stem != format!("stress-{ordinal:02}")
    {
        return Err(format!("{path}: stress ordinal {ordinal} is not the file stem"))
    }
    let counts = Counts::of(fixture);
    let manifested = entry.counts;
    if counts != manifested {
        return Err(format!("{path}: counts {counts:?} differ from manifested {manifested:?}"))
    }
    Ok(())
}

/// Loads the manifest, reconciles it with the directory, and parses every manifested fixture.
pub(super) fn load_and_validate_corpus() -> ParseResult<Vec<(ManifestEntry, Fixture)>> {
    let root = corpus_dir();
    let manifest_path = root.join("MANIFEST.txt");
    let text = io(fs::read_to_string(&manifest_path), &manifest_path)?;
    let entries = parse_manifest("MANIFEST.txt", &text)?;

    let mut on_disk = Vec::new();
    list_files(&root, "", &mut on_disk)?;
    on_disk.sort();
    let mut manifested: Vec<String> = entries.iter().map(|entry| entry.path.clone()).collect();
    manifested.push("MANIFEST.txt".to_owned());
    manifested.sort();
    let unlisted = on_disk.iter().filter(|path| !manifested.contains(path)).collect::<Vec<_>>();
    if !unlisted.is_empty() {
        return Err(format!("files present but not manifested: {unlisted:?}"))
    }
    let missing = manifested.iter().filter(|path| !on_disk.contains(path)).collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!("files manifested but not present: {missing:?}"))
    }

    let mut corpus = Vec::new();
    let mut scenarios = HashSet::new();
    for entry in entries {
        let path = root.join(&entry.path);
        let bytes = io(fs::read(&path), &path)?;
        if bytes.len() != entry.bytes {
            let (actual, listed) = (bytes.len(), entry.bytes);
            return Err(format!("{}: {actual} bytes on disk, manifest lists {listed}", entry.path))
        }
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if digest != entry.digest {
            let listed = &entry.digest;
            return Err(format!("{}: digest {digest} on disk, manifest lists {listed}", entry.path))
        }
        let text = String::from_utf8(bytes).map_err(|_| format!("{}: not UTF-8", entry.path))?;
        let fixture = parse_fixture(&entry.path, &text)?;
        check_entry(&entry, &fixture)?;
        if !scenarios.insert(fixture.scenario.clone()) {
            return Err(format!("{}: duplicate scenario identity {}", entry.path, fixture.scenario))
        }
        corpus.push((entry, fixture));
    }
    Ok(corpus)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-file manifest whose digests are all zeros and all `f`s.
    fn valid() -> String {
        let zero = "0".repeat(64);
        let full = "f".repeat(64);
        let lines: [&str; 10] = [
            "PIPELINE_MANIFEST 1",
            "FIXTURE_FORMAT 2",
            "GETH af7c0fd8ee09de71b1034dbe6d1112556b49b59f",
            "GENERATOR ce40051a0c308cb01df7005cb25e6481da78616f",
            "FILES 2",
            &format!("FILE curated/focused/a.txt FOCUSED DEFAULT 10 {zero} 1 1 2 2 0 0"),
            &format!("FILE stress/stress-00.txt STRESS RANGE 5 {full} 4 0 2 2 2 1"),
            "TOTALS 5 1 4 4 2 1 15",
            "END_MANIFEST",
            "",
        ];
        lines.join("\n")
    }

    fn rejects(text: &str, fragment: &str) {
        match parse_manifest("MANIFEST.txt", text) {
            Ok(_) => panic!("accepted a manifest that should fail with {fragment:?}"),
            Err(error) => assert!(error.contains(fragment), "{error:?} lacks {fragment:?}"),
        }
    }

    /// Replaces the unique occurrence of `old` in the valid manifest.
    fn with(old: &str, new: &str) -> String {
        let text = valid();
        assert_eq!(text.matches(old).count(), 1, "{old:?} must occur exactly once");
        text.replace(old, new)
    }

    #[test]
    fn accepts_a_canonical_manifest() {
        let entries = parse_manifest("MANIFEST.txt", &valid()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "curated/focused/a.txt");
        assert_eq!(entries[0].class, FixtureClass::Focused);
        assert_eq!(entries[0].counts.rows, 2);
        assert_eq!(entries[1].params_name, ParamsName::Range);
        assert_eq!(entries[1].bytes, 5);
    }

    #[test]
    fn rejects_unpinned_headers() {
        rejects(&with("MANIFEST 1", "MANIFEST 2"), "expected PIPELINE_MANIFEST 1");
        rejects(&with("FIXTURE_FORMAT 2", "FIXTURE_FORMAT 1"), "expected FIXTURE_FORMAT 2");
        rejects(&with("GETH af7c", "GETH 0000"), "unpinned Geth revision");
        rejects(&with("GETH af7c0fd8", "GETH af7c"), "malformed Geth");
        rejects(&with("GENERATOR ce40", "GENERATOR 0000"), "unpinned generator revision");
        rejects(&with("FILES 2", "FILES 3"), "expected FILE, found TOTALS");
        rejects(&with("FILES 2", "FILES 1"), "expected TOTALS, found FILE");
    }

    #[test]
    fn rejects_malformed_file_records() {
        rejects(&with("FOCUSED DEFAULT 10", "OTHER DEFAULT 10"), "invalid class: OTHER");
        rejects(&with("FOCUSED DEFAULT 10", "STRESS DEFAULT 10"), "wrong class directory");
        rejects(&with("FOCUSED DEFAULT 10", "FOCUSED OTHER 10"), "invalid params: OTHER");
        rejects(&with("FOCUSED DEFAULT 10", "FOCUSED DEFAULT 010"), "non-canonical byte count");
        rejects(&with("curated/focused/a.txt", "curated/other/a.txt"), "outside the corpus layout");
        rejects(&with("curated/focused/a.txt", "curated/focused/a.md"), "not a .txt file");
        rejects(&with("curated/focused/a.txt", "curated/focused/A.txt"), "invalid fixture stem");
        let tainted = format!("{}G", "0".repeat(63));
        rejects(&with(&"0".repeat(64), &tainted), "malformed digest");
        rejects(&with(&"0".repeat(64), &"0".repeat(63)), "malformed digest");
        rejects(&with(" 1 1 2 2 0 0\n", " 1 1 2 2 0\n"), "expected six content counts");
        rejects(&with(" 1 1 2 2 0 0\n", " 1 1 2 2 0 0 0\n"), "expected six content counts");
        rejects(&with(" 1 1 2 2 0 0\n", " 1 1 2 2 0 x\n"), "non-canonical potential index count");
    }

    #[test]
    fn rejects_unordered_or_inconsistent_totals() {
        let swapped = with("FILE curated/focused/a.txt FOCUSED", "FILE stress/z.txt STRESS");
        rejects(&swapped, "strictly ascending by path");
        rejects(&with("TOTALS 5 1 4 4 2 1 15", "TOTALS 5 1 4 4 2 1 16"), "TOTALS disagree");
        rejects(&with("TOTALS 5 1 4 4 2 1 15", "TOTALS 4 1 4 4 2 1 15"), "TOTALS disagree");
        rejects(&with("2 1 15", "2 15"), "expected six content counts");
        rejects(&with("END_MANIFEST\n", "END_MANIFEST\nEXTRA\n"), "unexpected trailing records");
        rejects(&with("END_MANIFEST\n", "END_MANIFEST"), "missing final newline");
        rejects(&with("END_MANIFEST\n", "END_MANIFEST \n"), "non-canonical line");
    }
}
