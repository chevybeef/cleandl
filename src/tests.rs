use super::*;
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

impl SyncData for Vec<u8> {}

async fn request_settings(
    api_url: String,
    api_key: &'static str,
) -> Result<HashMap<String, String>> {
    tokio::task::spawn_blocking(move || {
        let client = api_client(api_key, Duration::from_secs(2))?;
        load_settings(&client, &api_url)
    })
    .await
    .context("Settings request task failed")?
}

// Category 1: Unit Tests
#[test]
fn quiet_copy_progress_is_hidden() {
    assert!(copy_progress_bar(1024, false).is_hidden());
}

#[test]
fn progress_copy_retries_interrupted_reads() {
    struct InterruptedReader {
        interrupted: bool,
        data: io::Cursor<Vec<u8>>,
    }

    impl Read for InterruptedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::ErrorKind::Interrupted.into());
            }
            self.data.read(buffer)
        }
    }

    let payload = vec![123_u8; 2_100_000];
    let input = InterruptedReader {
        interrupted: false,
        data: io::Cursor::new(payload.clone()),
    };
    let mut output = Vec::new();
    let pb = ProgressBar::hidden();
    let copied = copy_with_progress_bar(input, &mut output, &pb).unwrap();
    assert_eq!(copied, 2_100_000);
    assert_eq!(output, payload);
    assert_eq!(pb.position(), copied);
    assert!(pb.is_finished());
}

#[test]
fn progress_copy_clears_bar_on_write_failure() {
    struct FailedWriter;

    impl Write for FailedWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::PermissionDenied.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SyncData for FailedWriter {}

    let pb = ProgressBar::hidden();
    let error = copy_with_progress_bar(&b"payload"[..], FailedWriter, &pb).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(pb.position(), 0);
    assert!(pb.is_finished());
}

fn sequential_test_config(destination: &Path) -> ApiConfig {
    ApiConfig {
        settings: Settings {
            destination_prefix: Some(destination.to_path_buf()),
            ..Settings::default()
        },
        ignore_rules: vec![],
        delete_rules: vec![],
        extensions: vec![ApiExtension {
            name: "pdf".into(),
            default_destination: vec!["sorted".into()],
            append_year_month: false,
            rules: vec![],
        }],
    }
}

#[test]
fn sequential_moves_resolve_collisions_against_completed_moves() {
    for reverse in [false, true] {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        let sorted = destination.join("sorted");
        fs::create_dir(&source).unwrap();
        fs::create_dir_all(&sorted).unwrap();
        fs::write(sorted.join("report.pdf"), "existing").unwrap();
        fs::write(source.join("report.pdf"), "first").unwrap();
        fs::write(source.join("report-1.pdf"), "second").unwrap();
        let mut entries: Vec<_> = fs::read_dir(&source).unwrap().map(Result::unwrap).collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        if reverse {
            entries.reverse();
        }
        process_directory(
            sequential_test_config(&destination),
            None,
            entries.into_iter().map(Ok).collect(),
            None,
        )
        .unwrap();
        let mut contents: Vec<_> = fs::read_dir(&sorted)
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect();
        contents.sort();
        assert_eq!(contents, ["existing", "first", "second"]);
        assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
    }
}

#[test]
fn destination_components_must_stay_inside_the_prefix() {
    let ok = |parts: &[&str]| {
        is_safe_destination(&parts.iter().map(ToString::to_string).collect::<Vec<_>>())
    };
    assert!(ok(&["Documents", "PDF"]));
    assert!(ok(&["Documents/PDF"]));
    assert!(!ok(&[".."]));
    assert!(!ok(&["Documents", "..", ".."]));
    assert!(!ok(&["/etc"]));
    assert!(!ok(&[""]));
    assert!(!ok(&["."]));
}

#[test]
fn unsafe_destination_is_skipped_and_nothing_escapes_the_prefix() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    let outside = temp.path().join("outside");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&destination).unwrap();
    fs::write(source.join("report.pdf"), "data").unwrap();
    let mut config = sequential_test_config(&destination);
    config.extensions[0].default_destination = vec!["..".into(), "outside".into()];
    let entries: Vec<_> = fs::read_dir(&source).unwrap().collect();

    process_directory(config, None, entries, None).unwrap();

    assert!(source.join("report.pdf").exists());
    assert!(!outside.exists());
}

#[test]
#[cfg(target_os = "linux")]
fn non_utf8_filenames_keep_their_exact_name_including_collision_suffixes() {
    use std::os::unix::ffi::OsStrExt as _;

    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    let sorted = destination.join("sorted");
    fs::create_dir(&source).unwrap();
    fs::create_dir_all(&sorted).unwrap();
    let name = OsStr::from_bytes(b"caf\xe9.pdf");
    fs::write(sorted.join(name), "existing").unwrap();
    fs::write(source.join(name), "new").unwrap();
    let entries: Vec<_> = fs::read_dir(&source).unwrap().collect();

    process_directory(sequential_test_config(&destination), None, entries, None).unwrap();

    assert_eq!(fs::read(sorted.join(name)).unwrap(), b"existing");
    let suffixed = OsStr::from_bytes(b"caf\xe9-1.pdf");
    assert_eq!(fs::read(sorted.join(suffixed)).unwrap(), b"new");
    assert!(!source.join(name).exists());
}

#[test]
fn format_size_is_human_readable() {
    assert_eq!(format_size(0), "0 B");
    assert_eq!(format_size(1023), "1023 B");
    assert_eq!(format_size(1024), "1.0 KiB");
    assert_eq!(format_size(1536), "1.5 KiB");
    // Just under a boundary rounds up into the next unit instead of reading "1024.0 KiB".
    assert_eq!(format_size(1_048_575), "1.0 MiB");
    assert_eq!(format_size(1024 * 1024), "1.0 MiB");
    assert_eq!(format_size(5 * 1024 * 1024 * 1024 / 2), "2.5 GiB");
    assert_eq!(format_size(1024_u64.pow(4)), "1.0 TiB");
    // TiB is the largest unit used.
    assert_eq!(format_size(1024_u64.pow(4) * 2048), "2048.0 TiB");
}

#[test]
fn the_log_file_is_appended_to_not_truncated() {
    let directory = TempDir::new().unwrap();
    writeln!(open_log_file(directory.path()).unwrap(), "first run").unwrap();
    writeln!(open_log_file(directory.path()).unwrap(), "second run").unwrap();

    assert_eq!(
        fs::read_to_string(directory.path().join(LOG_FILE)).unwrap(),
        "first run\nsecond run\n"
    );
}

fn read_gzip(path: &Path) -> String {
    let mut text = String::new();
    flate2::read::GzDecoder::new(File::open(path).unwrap())
        .read_to_string(&mut text)
        .unwrap();
    text
}

#[test]
fn filter_argument_is_split_and_normalised() {
    let parse = |value: &str| Cli::try_parse_from(["cleandl", value]);

    let args = parse("--filter=pdf,.XML, Txt").unwrap();
    assert_eq!(
        args.filter.as_deref(),
        Some(&["pdf", "xml", "txt"].map(String::from)[..])
    );
    assert_eq!(filter_extensions(&args).unwrap().len(), 3);

    let repeated = parse("--filter=pdf,PDF").unwrap();
    assert_eq!(filter_extensions(&repeated).unwrap().len(), 1);

    assert!(Cli::try_parse_from(["cleandl"]).unwrap().filter.is_none());
    assert!(filter_extensions(&Cli::try_parse_from(["cleandl"]).unwrap()).is_none());

    for bad in [
        "--filter=",
        "--filter=pdf,,xml",
        "--filter=tar.gz",
        "--filter=a/b",
    ] {
        assert!(parse(bad).is_err(), "{bad} should be rejected");
    }
    // Like the other value options, the equals sign is required.
    assert!(Cli::try_parse_from(["cleandl", "--filter", "pdf"]).is_err());
}

fn two_extension_config(destination: &Path) -> ApiConfig {
    let mut config = sequential_test_config(destination);
    config.extensions.push(ApiExtension {
        name: "txt".into(),
        default_destination: vec!["notes".into()],
        append_year_month: false,
        rules: vec![],
    });
    config.delete_rules = vec![r"\.tmp$".into(), "^scratch$".into()];
    config
}

#[test]
fn filter_only_touches_the_listed_extensions() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    fs::create_dir(&source).unwrap();
    for name in ["a.pdf", "B.PDF", "c.txt", "junk.tmp", "scratch"] {
        fs::write(source.join(name), name).unwrap();
    }
    let entries: Vec<_> = fs::read_dir(&source).unwrap().collect();

    let filter = Some(HashSet::from(["pdf".to_string()]));
    process_directory(two_extension_config(&destination), None, entries, filter).unwrap();

    // Both spellings of pdf moved...
    assert!(destination.join("sorted/a.pdf").exists());
    assert!(destination.join("sorted/B.PDF").exists());
    // ...and everything else stayed, including the file a global delete rule would remove.
    for name in ["c.txt", "junk.tmp", "scratch"] {
        assert!(source.join(name).exists(), "{name} should be untouched");
    }
    assert!(!destination.join("notes").exists());
}

#[test]
fn without_a_filter_everything_is_processed() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    fs::create_dir(&source).unwrap();
    for name in ["a.pdf", "c.txt", "junk.tmp"] {
        fs::write(source.join(name), name).unwrap();
    }
    let entries: Vec<_> = fs::read_dir(&source).unwrap().collect();

    process_directory(two_extension_config(&destination), None, entries, None).unwrap();

    assert!(destination.join("sorted/a.pdf").exists());
    assert!(destination.join("notes/c.txt").exists());
    assert!(!source.join("junk.tmp").exists());
}

#[test]
fn a_filter_still_applies_the_delete_rules_to_its_own_extensions() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("junk.tmp"), "x").unwrap();
    fs::write(source.join("a.pdf"), "x").unwrap();
    let entries: Vec<_> = fs::read_dir(&source).unwrap().collect();

    let filter = Some(HashSet::from(["tmp".to_string()]));
    process_directory(two_extension_config(temp.path()), None, entries, filter).unwrap();

    assert!(!source.join("junk.tmp").exists());
    assert!(source.join("a.pdf").exists());
}

fn move_in_temp_dir(payload: &str) -> (TempDir, PathBuf, PathBuf, Metadata) {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("source.pdf");
    let dst = temp.path().join("destination.pdf");
    fs::write(&src, payload).unwrap();
    let metadata = src.metadata().unwrap();
    (temp, src, dst, metadata)
}

#[test]
#[cfg(unix)]
fn a_move_on_the_same_filesystem_is_a_link_not_a_copy() {
    use std::os::unix::fs::MetadataExt;

    let (_temp, src, dst, metadata) = move_in_temp_dir("payload");
    let past = FileTime::from_unix_time(1_600_000_000, 0);
    filetime::set_file_times(&src, past, past).unwrap();
    let inode = src.metadata().unwrap().ino();

    move_file(&src, &dst, &metadata).unwrap();

    assert!(!src.exists());
    assert_eq!(fs::read_to_string(&dst).unwrap(), "payload");
    // The very same file, so nothing was copied and the times survive untouched.
    let moved = dst.metadata().unwrap();
    assert_eq!(moved.ino(), inode);
    assert_eq!(moved.nlink(), 1);
    assert_eq!(FileTime::from_last_modification_time(&moved), past);
}

#[test]
fn when_linking_is_impossible_the_file_is_copied_instead() {
    for kind in [io::ErrorKind::Unsupported, io::ErrorKind::PermissionDenied] {
        let (_temp, src, dst, _metadata) = move_in_temp_dir("payload");
        let past = FileTime::from_unix_time(1_600_000_000, 0);
        filetime::set_file_times(&src, past, past).unwrap();
        // The copy path restores the times recorded in `metadata`, so read it afterwards.
        let metadata = src.metadata().unwrap();
        let linked = std::cell::Cell::new(false);

        move_file_using(&src, &dst, &metadata, |_, _| {
            linked.set(true);
            Err(io::Error::from(kind))
        })
        .unwrap();

        assert!(linked.get(), "the link should have been tried first");
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(&dst).unwrap(), "payload");
        let moved = dst.metadata().unwrap();
        assert_eq!(FileTime::from_last_modification_time(&moved), past);
    }
}

#[test]
fn a_file_that_grew_since_the_scan_is_not_linked_but_reported() {
    let (_temp, src, dst, metadata) = move_in_temp_dir("short");
    fs::write(&src, "short and then some").unwrap();

    let error = move_file_using(&src, &dst, &metadata, |_, _| {
        panic!("a changed file must not be linked")
    })
    .unwrap_err();

    assert!(error.to_string().contains("size changed"), "{error:#}");
    assert!(src.exists());
    assert!(!dst.exists());
}

#[test]
fn a_failed_link_leaves_the_source_alone() {
    let (_temp, src, dst, metadata) = move_in_temp_dir("payload");
    fs::write(&dst, "someone else's").unwrap();

    // The real hard_link refuses to replace an existing file.
    assert!(!link_move(&src, &dst, &metadata, |from, to| fs::hard_link(from, to)).unwrap());
    assert_eq!(fs::read_to_string(&src).unwrap(), "payload");
    assert_eq!(fs::read_to_string(&dst).unwrap(), "someone else's");
}

/// A terminal that only records what indicatif asks of it.
#[derive(Debug, Clone, Default)]
struct RecordingTerm(Arc<std::sync::Mutex<Vec<String>>>);

impl RecordingTerm {
    fn note(&self, event: &str) {
        self.0.lock().unwrap().push(event.to_string());
    }
    fn events(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl indicatif::TermLike for RecordingTerm {
    fn width(&self) -> u16 {
        80
    }
    fn move_cursor_up(&self, _: usize) -> io::Result<()> {
        self.note("up");
        Ok(())
    }
    fn move_cursor_down(&self, _: usize) -> io::Result<()> {
        self.note("down");
        Ok(())
    }
    fn move_cursor_right(&self, _: usize) -> io::Result<()> {
        Ok(())
    }
    fn move_cursor_left(&self, _: usize) -> io::Result<()> {
        Ok(())
    }
    fn write_line(&self, line: &str) -> io::Result<()> {
        self.note(&format!("draw {line}"));
        Ok(())
    }
    fn write_str(&self, text: &str) -> io::Result<()> {
        self.note(&format!("draw {text}"));
        Ok(())
    }
    fn clear_line(&self) -> io::Result<()> {
        self.note("clear");
        Ok(())
    }
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
}

struct LogToTerm(RecordingTerm);

impl Write for LogToTerm {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.note("LOG");
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn log_lines_are_written_with_the_progress_bar_out_of_the_way() {
    let term = RecordingTerm::default();
    progress().set_draw_target(indicatif::ProgressDrawTarget::term_like(Box::new(
        term.clone(),
    )));
    let bar = copy_progress_bar(100, true);
    bar.set_position(50);

    let logger = DynamicLogger {
        inner: WriteLogger::new(
            LevelFilter::Trace,
            Config::default(),
            LogToTerm(term.clone()),
        ),
        debug_flag: Arc::new(AtomicBool::new(false)),
    };
    log::Log::log(
        &logger,
        &Record::builder()
            .level(log::Level::Info)
            .args(format_args!("a log line"))
            .build(),
    );
    bar.finish_and_clear();
    progress().set_draw_target(indicatif::ProgressDrawTarget::hidden());

    let events = term.events();
    let line = events.iter().position(|event| event == "LOG").unwrap();
    let before = events.get(..line).unwrap();
    // The bar was on screen, and was cleared again before the line went out.
    let drawn = before
        .iter()
        .rposition(|event| event.starts_with("draw"))
        .unwrap();
    assert!(
        before.iter().skip(drawn).any(|event| event == "clear"),
        "the bar was still on screen when the line was written: {events:?}"
    );
}

#[test]
fn a_small_or_missing_log_is_not_rotated() {
    let directory = TempDir::new().unwrap();
    assert!(!rotate_log(directory.path(), 10, 3).unwrap());

    fs::write(directory.path().join(LOG_FILE), "0123456789").unwrap();
    assert!(!rotate_log(directory.path(), 10, 3).unwrap());
    assert!(directory.path().join(LOG_FILE).exists());
    assert!(!rotated_log_path(directory.path(), 1).exists());
}

#[test]
fn a_big_log_is_compressed_and_the_generations_shift() {
    let directory = TempDir::new().unwrap();
    let log = directory.path().join(LOG_FILE);

    for run in ["one", "two", "three", "four"] {
        fs::write(&log, format!("{run} {}", "x".repeat(50))).unwrap();
        assert!(rotate_log(directory.path(), 10, 3).unwrap());
        assert!(!log.exists(), "the rotated log should be gone");
    }

    // Newest first; "one" fell off the end because only three are kept.
    assert!(read_gzip(&rotated_log_path(directory.path(), 1)).starts_with("four "));
    assert!(read_gzip(&rotated_log_path(directory.path(), 2)).starts_with("three "));
    assert!(read_gzip(&rotated_log_path(directory.path(), 3)).starts_with("two "));
    assert!(!rotated_log_path(directory.path(), 4).exists());
    assert!(!directory.path().join(format!("{LOG_FILE}.gz.tmp")).exists());
}

#[test]
fn a_failed_rotation_leaves_the_log_untouched() {
    let directory = TempDir::new().unwrap();
    let log = directory.path().join(LOG_FILE);
    fs::write(&log, "x".repeat(50)).unwrap();
    // A directory where the temporary archive goes makes the compression fail.
    fs::create_dir(directory.path().join(format!("{LOG_FILE}.gz.tmp"))).unwrap();

    assert!(rotate_log(directory.path(), 10, 2).is_err());
    assert_eq!(fs::read_to_string(&log).unwrap(), "x".repeat(50));
    assert!(!rotated_log_path(directory.path(), 1).exists());
}

#[test]
fn log_size_message_reports_the_location_and_size() {
    let directory = TempDir::new().unwrap();
    assert!(log_size_message(directory.path()).contains("Could not read the size"));

    let log = directory.path().join(LOG_FILE);
    fs::write(&log, vec![b'x'; 2048]).unwrap();
    let message = log_size_message(directory.path());

    assert!(message.contains("2.0 KiB"), "{message}");
    assert!(message.contains(&log.display().to_string()), "{message}");
}

#[test]
fn identical_duplicates_are_numbered_and_moved_not_left_in_the_source() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    let sorted = destination.join("sorted");
    fs::create_dir(&source).unwrap();
    fs::create_dir_all(&sorted).unwrap();
    // The destination already holds a file with the same name *and* the same size.
    fs::write(sorted.join("report.pdf"), "same").unwrap();

    for expected in ["report-1.pdf", "report-2.pdf", "report-3.pdf"] {
        fs::write(source.join("report.pdf"), "same").unwrap();
        let entries: Vec<_> = fs::read_dir(&source).unwrap().collect();

        process_directory(sequential_test_config(&destination), None, entries, None).unwrap();

        // Each run lands on the next free number, and nothing stays behind.
        assert_eq!(fs::read_to_string(sorted.join(expected)).unwrap(), "same");
        assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
    }
    assert_eq!(
        fs::read_to_string(sorted.join("report.pdf")).unwrap(),
        "same"
    );
    assert_eq!(fs::read_dir(&sorted).unwrap().count(), 4);
}

#[test]
fn numbering_skips_names_taken_on_disk_or_earlier_in_the_run() {
    let temp = TempDir::new().unwrap();
    let dest_dir = temp.path().join("sorted");
    fs::create_dir(&dest_dir).unwrap();
    fs::write(dest_dir.join("report.pdf"), "same").unwrap();
    fs::write(dest_dir.join("report-1.pdf"), "same").unwrap();
    let mut run = DirectoryRun::new(sequential_test_config(temp.path()), None).unwrap();
    let wanted = dest_dir.join("report.pdf");

    // report.pdf and report-1.pdf exist on disk, so the first free name is -2.
    let first = run
        .resolve_destination(&dest_dir, &wanted)
        .unwrap()
        .unwrap();
    assert_eq!(first, dest_dir.join("report-2.pdf"));

    // A dry run only simulates the move, but must still not hand out -2 twice.
    run.simulated_destinations.insert(first);
    let second = run
        .resolve_destination(&dest_dir, &wanted)
        .unwrap()
        .unwrap();
    assert_eq!(second, dest_dir.join("report-3.pdf"));

    // A name nothing has taken is used as it is.
    let fresh = dest_dir.join("new.pdf");
    assert_eq!(
        run.resolve_destination(&dest_dir, &fresh).unwrap(),
        Some(fresh)
    );
}

#[test]
fn files_that_vanish_after_the_scan_do_not_abort_the_run() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    fs::create_dir(&source).unwrap();
    // Names sort so both vanished files are reached before the surviving one.
    fs::write(source.join("0-vanished.tmp"), "delete rule").unwrap();
    fs::write(source.join("a-vanished.pdf"), "moved rule").unwrap();
    fs::write(source.join("b-survivor.pdf"), "survivor").unwrap();
    let mut entries: Vec<_> = fs::read_dir(&source).unwrap().map(Result::unwrap).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    fs::remove_file(source.join("0-vanished.tmp")).unwrap();
    fs::remove_file(source.join("a-vanished.pdf")).unwrap();
    let mut config = sequential_test_config(&destination);
    config.delete_rules = vec![r"\.tmp$".into()];

    process_directory(config, None, entries.into_iter().map(Ok).collect(), None).unwrap();

    assert_eq!(
        fs::read_to_string(destination.join("sorted/b-survivor.pdf")).unwrap(),
        "survivor"
    );
    assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
}

#[test]
fn month_folders_follow_the_given_time_zone_not_utc() {
    // 2026-09-30 23:30 UTC is already 1 October in UTC+01:00 (and still September in UTC-05:00).
    let mtime = FileTime::from_unix_time(1_790_811_000, 0);
    let utc = chrono::FixedOffset::east_opt(0).unwrap();
    let plus_one = chrono::FixedOffset::east_opt(3_600).unwrap();
    let minus_five = chrono::FixedOffset::west_opt(5 * 3_600).unwrap();

    assert_eq!(
        year_month_folders(mtime, &utc),
        Some(("2026".to_string(), "09".to_string()))
    );
    assert_eq!(
        year_month_folders(mtime, &plus_one),
        Some(("2026".to_string(), "10".to_string()))
    );
    assert_eq!(
        year_month_folders(mtime, &minus_five),
        Some(("2026".to_string(), "09".to_string()))
    );
    // The year rolls over too: 2026-12-31 23:30 UTC is 2027-01 in UTC+01:00.
    let new_year = FileTime::from_unix_time(1_798_759_800, 0);
    assert_eq!(
        year_month_folders(new_year, &plus_one),
        Some(("2027".to_string(), "01".to_string()))
    );
}

#[test]
fn only_real_per_file_errors_count_as_failures() {
    let path = Path::new("example.pdf");
    let mut failed = 0_usize;

    record_file_error(path, "Failed", &io::ErrorKind::NotFound.into(), &mut failed);
    assert_eq!(failed, 0);

    record_file_error(
        path,
        "Failed",
        &io::ErrorKind::PermissionDenied.into(),
        &mut failed,
    );
    assert_eq!(failed, 1);
}

#[test]
fn free_name_search_skips_existing_and_simulated_names() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    fs::write(dir.join("report.pdf"), "").unwrap();
    fs::write(dir.join("report-1.pdf"), "").unwrap();
    let simulated = HashSet::from([dir.join("report-2.pdf")]);

    let found = find_free_name(dir, &simulated, &dir.join("report.pdf")).unwrap();

    assert_eq!(found, Some(dir.join("report-3.pdf")));
}

#[test]
fn exclusive_creation_preserves_a_destination_created_after_planning() {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("source.pdf");
    let dst = temp.path().join("destination.pdf");
    fs::write(&src, "source data").unwrap();
    let metadata = src.metadata().unwrap();
    fs::write(&dst, "other writer").unwrap();
    let error = move_file(&src, &dst, &metadata).unwrap_err();
    assert!(error.to_string().contains("exclusively create"));
    assert_eq!(fs::read_to_string(&src).unwrap(), "source data");
    assert_eq!(fs::read_to_string(&dst).unwrap(), "other writer");
}

#[test]
fn interrupted_or_short_copy_removes_partial_destination_and_keeps_source() {
    for fail in [false, true] {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("source.pdf");
        let dst = temp.path().join("destination.pdf");
        fs::write(&src, "complete source").unwrap();
        let result = move_file_with_copy(&src, &dst, &src.metadata().unwrap(), |_, output| {
            output.write_all(b"partial")?;
            if fail {
                Err(io::Error::other("simulated disconnect"))
            } else {
                Ok(7)
            }
        });
        assert!(result.is_err());
        assert!(!dst.exists());
        assert_eq!(fs::read_to_string(&src).unwrap(), "complete source");
    }
}

#[test]
fn sequential_copy_handles_empty_and_multibuffer_files() {
    for payload in [vec![], vec![123_u8; 2_100_000]] {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("source.pdf");
        let dst = temp.path().join("destination.pdf");
        fs::write(&src, &payload).unwrap();
        copy_move(&src, &dst, &src.metadata().unwrap()).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), payload);
        assert!(!src.exists());
    }
}

#[test]
#[cfg(unix)]
fn failed_move_returns_error_and_remaining_files_are_processed() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    fs::create_dir(&source).unwrap();
    fs::create_dir_all(destination.join("sorted")).unwrap();
    fs::write(source.join("bad.pdf"), "bad").unwrap();
    fs::write(source.join("good.pdf"), "good").unwrap();
    // A dangling link passes try_exists, but exclusive creation must reject it.
    std::os::unix::fs::symlink(
        temp.path().join("missing"),
        destination.join("sorted/bad.pdf"),
    )
    .unwrap();
    let mut entries: Vec<_> = fs::read_dir(&source).unwrap().map(Result::unwrap).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let error = process_directory(
        sequential_test_config(&destination),
        None,
        entries.into_iter().map(Ok).collect(),
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("1 file move(s) failed"));
    assert!(source.join("bad.pdf").exists());
    assert!(!source.join("good.pdf").exists());
    assert_eq!(
        fs::read_to_string(destination.join("sorted/good.pdf")).unwrap(),
        "good"
    );
    assert!(destination.join("sorted/bad.pdf").is_symlink());
}

#[test]
fn test_calculate_mibps_normal() {
    let bytes = 10_485_760; // exactly 10 MiB
    let duration = Duration::from_secs(2);
    let speed = calculate_mibps(bytes, duration);
    assert!((speed - 5.0).abs() < f64::EPSILON);
}

#[test]
fn test_calculate_mibps_zero_duration() {
    let bytes = 10_485_760;
    let duration = Duration::from_secs(0);
    let speed = calculate_mibps(bytes, duration);
    assert!((speed - 0.0).abs() < f64::EPSILON);
}

#[test]
fn test_calculate_mibps_large_file() {
    let bytes = 8_589_934_592; // 8 GiB
    let speed = calculate_mibps(bytes, Duration::from_secs(2));

    assert!((speed - 4096.0).abs() < f64::EPSILON);
}

#[test]
fn test_json_deserialization_defaults() {
    let raw_json = r#"{
        "settings": {"source_folder": "Downloads"},
        "extensions": []
    }"#;

    let config: ApiConfig = serde_json::from_str(raw_json).expect("Deserialization failed");
    assert_eq!(config.ignore_rules, Vec::<String>::new());
    assert_eq!(config.delete_rules, Vec::<String>::new());
    assert_eq!(config.settings.source_folder, "Downloads");
}

#[test]
fn test_cli_boolean_flags_accept_bare_and_explicit_values() {
    let bare = Cli::try_parse_from(["cleandl", "--dry-run", "--debug"]).unwrap();
    assert_eq!(bare.dry_run, Some(true));
    assert_eq!(bare.debug, Some(true));

    let explicit = Cli::try_parse_from(["cleandl", "--dry-run=false", "--debug=false"]).unwrap();
    assert_eq!(explicit.dry_run, Some(false));
    assert_eq!(explicit.debug, Some(false));
}

#[test]
fn test_cli_version_includes_git_revision() {
    let output = Cli::try_parse_from(["cleandl", "--version"])
        .unwrap_err()
        .to_string();
    assert_eq!(output, format!("cleandl {}\n", cli_version()));
}

#[test]
fn test_normalize_api_url_requires_api_v1_root() {
    assert_eq!(
        normalize_api_url(" https://example.test/api/v1/ ").unwrap(),
        "https://example.test/api/v1"
    );
    assert!(normalize_api_url("https://example.test/config").is_err());
    assert!(normalize_api_url("https://example.test").is_err());
}

#[test]
fn test_configuration_cache_is_bound_to_url_and_key() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join(CACHE_FILE);
    let cache = CachedConfiguration {
        schema_version: CACHE_SCHEMA_VERSION,
        api_url: "https://example.test/api/v1".to_string(),
        api_key_fingerprint: api_key_fingerprint("secret-key"),
        configuration_version: 123,
        settings: HashMap::from([("trial_run".to_string(), "true".to_string())]),
        global_rules: Vec::new(),
        extensions: Vec::new(),
        resolved_extensions: HashSet::new(),
    };

    save_cached_configuration(&path, &cache).unwrap();

    assert!(
        load_cached_configuration(&path, "https://example.test/api/v1", "secret-key").is_some()
    );
    assert!(load_cached_configuration(&path, "https://example.test/api/v1", "other-key").is_none());
    assert!(load_cached_configuration(&path, "https://other.test/api/v1", "secret-key").is_none());
}

#[test]
fn test_cached_live_run_confirmation_requires_explicit_yes() {
    let mut accepted_output = Vec::new();
    let mut accepted_input = &b"yes\n"[..];
    confirm_cached_live_run_with(&mut accepted_input, &mut accepted_output).unwrap();

    let mut rejected_output = Vec::new();
    let mut rejected_input = &b"\n"[..];
    let error = confirm_cached_live_run_with(&mut rejected_input, &mut rejected_output)
        .expect_err("an empty response must cancel a live cached run");

    assert!(error.to_string().contains("Run cancelled"));
    assert!(
        String::from_utf8(accepted_output)
            .unwrap()
            .contains("trial_run is false")
    );
}

#[test]
fn test_trial_run_uses_cache_when_control_plane_is_offline() {
    let directory = TempDir::new().unwrap();
    let source = directory.path().join("Downloads");
    fs::create_dir(&source).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let api_url = format!("http://{}/api/v1", listener.local_addr().unwrap());
    drop(listener);
    let cache = CachedConfiguration {
        schema_version: CACHE_SCHEMA_VERSION,
        api_url: api_url.clone(),
        api_key_fingerprint: api_key_fingerprint("secret-key"),
        configuration_version: 42,
        settings: HashMap::from([
            ("source_folder".to_string(), source.display().to_string()),
            ("trial_run".to_string(), "true".to_string()),
            ("debug".to_string(), "false".to_string()),
        ]),
        global_rules: Vec::new(),
        extensions: Vec::new(),
        resolved_extensions: HashSet::new(),
    };
    save_cached_configuration(&directory.path().join(CACHE_FILE), &cache).unwrap();
    let args = Cli::try_parse_from([
        "cleandl".to_string(),
        format!("--api-url={api_url}"),
        "--api-key=secret-key".to_string(),
    ])
    .unwrap();

    let (config, entries) = prepare_api_run(&args, &AtomicBool::new(false), directory.path())
        .unwrap()
        .unwrap();

    assert!(config.settings.trial_run);
    assert!(entries.is_empty());
}

#[test]
fn test_missing_api_url_file_is_created_with_default() {
    let temp_dir = TempDir::new().unwrap();

    let api_url = load_api_url(None, temp_dir.path()).unwrap();

    assert_eq!(api_url, DEFAULT_API_URL);
    assert_eq!(
        fs::read_to_string(temp_dir.path().join("cleandl.url")).unwrap(),
        format!("{DEFAULT_API_URL}\n")
    );
}

#[test]
fn test_missing_api_key_prompts_and_saves_pasted_key() {
    let temp_dir = TempDir::new().unwrap();
    let mut input = &b"cldl_live_pasted-key\n"[..];
    let mut output = Vec::new();

    let setup_result = load_api_key_with(
        None,
        temp_dir.path(),
        DEFAULT_API_URL,
        &mut input,
        &mut output,
    )
    .unwrap();

    assert!(setup_result.is_none());
    assert_eq!(
        fs::read_to_string(temp_dir.path().join("cleandl.key")).unwrap(),
        "cldl_live_pasted-key\n"
    );
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("https://cleandl.chevybeef.com/login"));
    assert!(output.contains("Connection keys"));
    assert!(output.contains("Ctrl+C to cancel"));
    assert!(output.contains("Setup complete"));
}

#[test]
fn test_empty_api_key_input_reprompts_and_does_not_save_on_eof() {
    let temp_dir = TempDir::new().unwrap();
    let mut input = &b"\n"[..];
    let mut output = Vec::new();

    let error = load_api_key_with(
        None,
        temp_dir.path(),
        DEFAULT_API_URL,
        &mut input,
        &mut output,
    )
    .unwrap_err();

    assert!(error.to_string().contains("no key was saved"));
    assert!(!temp_dir.path().join("cleandl.key").exists());
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("The key cannot be empty")
    );
}

#[cfg(unix)]
#[test]
fn test_saved_api_key_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp_dir = TempDir::new().unwrap();
    let mut input = &b"secret-key\n"[..];
    let mut output = Vec::new();
    load_api_key_with(
        None,
        temp_dir.path(),
        DEFAULT_API_URL,
        &mut input,
        &mut output,
    )
    .unwrap();
    let key_path = temp_dir.path().join("cleandl.key");
    let mode = fs::metadata(key_path).unwrap().permissions().mode() & 0o777;

    assert_eq!(mode, 0o600);
}

/// The key the control plane stores this platform's destination prefix under.
fn platform_prefix_key() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows_prefix"
    } else if cfg!(target_os = "macos") {
        "macos_prefix"
    } else {
        "unix_prefix"
    }
}

fn string_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

#[test]
fn test_resolve_settings_applies_defaults() {
    let map = string_map(&[(platform_prefix_key(), "/api-prefix")]);
    let args = Cli::try_parse_from(["cleandl"]).unwrap();
    let debug_flag = AtomicBool::new(false);

    let settings = resolve_settings(&map, &args, &debug_flag);

    assert_eq!(
        settings,
        Settings {
            source_folder: "Downloads".to_string(),
            destination_prefix: Some(PathBuf::from("/api-prefix")),
            trial_run: false,
            debug: false,
        }
    );
    assert!(!debug_flag.load(Ordering::Relaxed));
}

#[test]
fn test_resolve_settings_cli_overrides_api_values() {
    // The control plane may still send keys this client no longer uses (threads,
    // sort_by_size); comparing the whole struct shows none of them leak through.
    let map = string_map(&[
        ("source_folder", "ApiDownloads"),
        ("trial_run", "true"),
        ("debug", "true"),
        ("threads", "4"),
        ("sort_by_size", "Descending"),
    ]);
    let args = Cli::try_parse_from([
        "cleandl",
        "--source=CliDownloads",
        "--prefix=/cli-prefix",
        "--dry-run=false",
        "--debug=false",
        "--threads=12",
        "--trace",
    ])
    .unwrap();
    let debug_flag = AtomicBool::new(false);

    let settings = resolve_settings(&map, &args, &debug_flag);

    assert_eq!(
        settings,
        Settings {
            source_folder: "CliDownloads".to_string(),
            destination_prefix: Some(PathBuf::from("/cli-prefix")),
            trial_run: false,
            debug: false,
        }
    );
    // --trace turns debug logging on even though the debug setting is false.
    assert!(debug_flag.load(Ordering::Relaxed));
}

#[test]
fn test_resolve_settings_reads_api_values_when_the_cli_is_silent() {
    let map = string_map(&[
        ("source_folder", "ApiDownloads"),
        ("trial_run", "true"),
        ("debug", "true"),
    ]);
    let args = Cli::try_parse_from(["cleandl"]).unwrap();
    let debug_flag = AtomicBool::new(false);

    let settings = resolve_settings(&map, &args, &debug_flag);

    assert_eq!(settings.source_folder, "ApiDownloads");
    assert!(settings.trial_run);
    assert!(settings.debug);
    assert!(debug_flag.load(Ordering::Relaxed));
}

#[test]
fn test_boolean_settings_only_accept_true_case_insensitively() {
    let args = Cli::try_parse_from(["cleandl"]).unwrap();
    for (value, expected) in [
        ("true", true),
        ("TRUE", true),
        ("True", true),
        ("false", false),
        ("1", false),
        ("yes", false),
        ("", false),
    ] {
        let map = string_map(&[("trial_run", value), ("debug", value)]);
        let settings = resolve_settings(&map, &args, &AtomicBool::new(false));
        assert_eq!(settings.trial_run, expected, "trial_run = {value:?}");
        assert_eq!(settings.debug, expected, "debug = {value:?}");
    }
}

#[test]
fn test_destination_prefix_comes_from_cli_then_platform_key_and_empty_means_home() {
    let prefix = |pairs: &[(&str, &str)], cli: &[&str]| {
        let args =
            Cli::try_parse_from(std::iter::once("cleandl").chain(cli.iter().copied())).unwrap();
        resolve_settings(&string_map(pairs), &args, &AtomicBool::new(false)).destination_prefix
    };
    let key = platform_prefix_key();

    assert_eq!(prefix(&[(key, "/api")], &[]), Some(PathBuf::from("/api")));
    // The command line wins over the control plane.
    assert_eq!(
        prefix(&[(key, "/api")], &["--prefix=/cli"]),
        Some(PathBuf::from("/cli"))
    );
    // An empty command-line prefix means "home", not "fall back to the API's".
    assert_eq!(prefix(&[(key, "/api")], &["--prefix="]), None);
    // An empty prefix, or one stored for another platform, also means home.
    assert_eq!(prefix(&[(key, "")], &[]), None);
    assert_eq!(prefix(&[("other_prefix", "/other")], &[]), None);
}

#[test]
fn test_settings_json_defaults_apply_to_missing_fields() {
    let settings: Settings = serde_json::from_str(r#"{"trial_run": true}"#).unwrap();
    assert_eq!(
        settings,
        Settings {
            trial_run: true,
            ..Settings::default()
        }
    );
    assert_eq!(settings.source_folder, "Downloads");
}

#[test]
fn test_demo_forces_trial_run() {
    let map = string_map(&[("trial_run", "false")]);
    let args = Cli::try_parse_from(["cleandl", "--demo", "--dry-run=false"]).unwrap();
    let debug_flag = AtomicBool::new(false);

    let settings = resolve_settings(&map, &args, &debug_flag);

    assert!(settings.trial_run);
}

#[test]
fn test_demo_uses_a_realistic_platform_prefix() {
    let args = Cli::try_parse_from(["cleandl", "--demo"]).unwrap();
    let debug_flag = AtomicBool::new(false);

    let settings = resolve_settings(&demo_settings(), &args, &debug_flag);
    let expected = if cfg!(target_os = "windows") {
        r"\\truenas\downloads"
    } else if cfg!(target_os = "macos") {
        "/Volumes/truenas"
    } else {
        "/mnt/truenas"
    };

    assert_eq!(settings.destination_prefix, Some(PathBuf::from(expected)));
}

#[test]
fn test_demo_uses_only_relevant_builtin_extensions() {
    let settings = Settings {
        trial_run: true,
        ..Settings::default()
    };
    let found = HashSet::from(["jpg".to_string(), "pdf".to_string(), "xyz".to_string()]);

    let config = demo_config(settings, &found);
    let names: HashSet<&str> = config
        .extensions
        .iter()
        .map(|extension| extension.name.as_str())
        .collect();

    assert_eq!(names, HashSet::from(["jpg", "pdf"]));
    assert!(
        config
            .extensions
            .iter()
            .find(|extension| extension.name == "pdf")
            .is_some_and(|extension| extension.rules.len() == 2)
    );
}

#[test]
fn test_demo_needs_no_api_configuration_and_never_changes_files() {
    let (_temp, source_path, dest_prefix) = setup_test_environment();
    let invoice = source_path.join("invoice-123.pdf");
    let temporary = source_path.join("cleanup.tmp");
    fs::write(&invoice, "invoice").unwrap();
    fs::write(&temporary, "temporary").unwrap();

    let args = Cli::try_parse_from([
        "cleandl",
        "--demo",
        &format!("--source={}", source_path.display()),
        &format!("--prefix={}", dest_prefix.display()),
        "--dry-run=false",
    ])
    .unwrap();
    let debug_flag = AtomicBool::new(false);

    let (config, entries) = prepare_demo_run(&args, &debug_flag).unwrap();
    assert!(config.settings.trial_run);
    process_directory(config, None, entries, None).unwrap();

    assert!(invoice.exists());
    assert!(temporary.exists());
    assert!(
        !dest_prefix
            .join("Documents/Finance/invoice-123.pdf")
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_load_settings_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/settings"))
        .and(header("X-API-Key", "secret-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"key": "source_folder", "value": "Downloads"},
            {"key": "threads", "value": "8"}
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let settings = request_settings(server.uri(), "secret-key").await.unwrap();

    assert_eq!(settings["source_folder"], "Downloads");
    assert_eq!(settings["threads"], "8");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fetch_configuration_version_uses_lightweight_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/configuration-version"))
        .and(header("X-API-Key", "secret-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "updated_at": 1_786_500_000_123_i64
        })))
        .expect(1)
        .mount(&server)
        .await;

    let api_url = server.uri();
    let version = tokio::task::spawn_blocking(move || {
        let client = api_client("secret-key", Duration::from_secs(2))?;
        fetch_configuration_version(&client, &api_url)
    })
    .await
    .unwrap()
    .unwrap();

    assert_eq!(version.updated_at, 1_786_500_000_123);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_unchanged_version_uses_cache_without_fetching_settings() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/configuration-version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "updated_at": 42
        })))
        .expect(1)
        .mount(&server)
        .await;

    let directory = TempDir::new().unwrap();
    let source = directory.path().join("Downloads");
    fs::create_dir(&source).unwrap();
    let api_url = format!("{}/api/v1", server.uri());
    let cache = CachedConfiguration {
        schema_version: CACHE_SCHEMA_VERSION,
        api_url: api_url.clone(),
        api_key_fingerprint: api_key_fingerprint("secret-key"),
        configuration_version: 42,
        settings: HashMap::from([
            ("source_folder".to_string(), source.display().to_string()),
            ("trial_run".to_string(), "true".to_string()),
            ("debug".to_string(), "false".to_string()),
        ]),
        global_rules: Vec::new(),
        extensions: Vec::new(),
        resolved_extensions: HashSet::new(),
    };
    save_cached_configuration(&directory.path().join(CACHE_FILE), &cache).unwrap();

    let args = Cli::try_parse_from([
        "cleandl".to_string(),
        format!("--api-url={api_url}"),
        "--api-key=secret-key".to_string(),
    ])
    .unwrap();
    let log_dir = directory.path().to_path_buf();
    let (config, entries) = tokio::task::spawn_blocking(move || {
        prepare_api_run(&args, &AtomicBool::new(false), &log_dir)
    })
    .await
    .unwrap()
    .unwrap()
    .unwrap();

    assert!(config.settings.trial_run);
    assert!(entries.is_empty());
}

type PreparedRun = Result<Option<(ApiConfig, Vec<io::Result<fs::DirEntry>>)>>;
type StubResponses<'a> = &'a [(&'a str, &'a str, u16)];

/// Runs `prepare_api_run` against a stub control plane with a valid cache at
/// version 42 and one PDF in the source folder. `responses` lists the
/// `(method, endpoint, status)` stubs to mount besides `configuration-version`;
/// a 200 returns a minimal valid body for that endpoint.
async fn prepare_against_stub(
    remote_version: i64,
    resolved: &[&str],
    responses: &[(&str, &str, u16)],
) -> PreparedRun {
    prepare_against_stub_in(None, remote_version, resolved, responses)
        .await
        .0
}

/// Like [`prepare_against_stub`], but the stubbed `settings` may point at a
/// different `remote_source` folder than the cache, and the directory holding
/// the cache file is returned so tests can inspect what was written.
async fn prepare_against_stub_in(
    remote_source: Option<&str>,
    remote_version: i64,
    resolved: &[&str],
    responses: &[(&str, &str, u16)],
) -> (PreparedRun, TempDir) {
    let server = MockServer::start().await;
    let directory = TempDir::new().unwrap();
    let source = directory.path().join("Downloads");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a.pdf"), "pdf").unwrap();
    let source_text = source.display().to_string();

    Mock::given(method("GET"))
        .and(path("/api/v1/configuration-version"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "updated_at": remote_version })),
        )
        .mount(&server)
        .await;
    for (verb, endpoint, status) in responses {
        let body = if *endpoint == "settings" {
            serde_json::json!([
                {"key": "source_folder", "value": remote_source.unwrap_or(&source_text)},
                {"key": "trial_run", "value": "true"}
            ])
        } else {
            serde_json::json!([])
        };
        Mock::given(method(*verb))
            .and(path(format!("/api/v1/{endpoint}")))
            .respond_with(ResponseTemplate::new(*status).set_body_json(body))
            .mount(&server)
            .await;
    }

    let api_url = format!("{}/api/v1", server.uri());
    let cache = CachedConfiguration {
        schema_version: CACHE_SCHEMA_VERSION,
        api_url: api_url.clone(),
        api_key_fingerprint: api_key_fingerprint("secret-key"),
        configuration_version: 42,
        settings: HashMap::from([
            ("source_folder".to_string(), source_text),
            ("trial_run".to_string(), "true".to_string()),
            ("debug".to_string(), "false".to_string()),
        ]),
        global_rules: Vec::new(),
        extensions: Vec::new(),
        resolved_extensions: resolved.iter().map(ToString::to_string).collect(),
    };
    save_cached_configuration(&directory.path().join(CACHE_FILE), &cache).unwrap();
    let args = Cli::try_parse_from([
        "cleandl".to_string(),
        format!("--api-url={api_url}"),
        "--api-key=secret-key".to_string(),
    ])
    .unwrap();
    let log_dir = directory.path().to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        prepare_api_run(&args, &AtomicBool::new(false), &log_dir)
    })
    .await
    .unwrap();
    (result, directory)
}

fn read_cache(directory: &TempDir) -> CachedConfiguration {
    serde_json::from_slice(&fs::read(directory.path().join(CACHE_FILE)).unwrap()).unwrap()
}

#[test]
fn authentication_errors_are_recognised_through_added_context() {
    let error = anyhow::Error::from(AuthenticationError).context("while fetching");
    assert!(is_authentication_error(&error));
    // Message text alone must not be mistaken for an authentication failure.
    assert!(!is_authentication_error(&anyhow::anyhow!(
        "Authentication failed"
    )));
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_key_is_not_masked_by_the_cache_when_fetching_settings() {
    let error = prepare_against_stub(43, &[], &[("GET", "settings", 401)])
        .await
        .unwrap_err();
    assert!(is_authentication_error(&error));
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_key_is_not_masked_by_the_cache_when_fetching_global_rules() {
    let error = prepare_against_stub(
        43,
        &[],
        &[("GET", "settings", 200), ("GET", "global-rules", 401)],
    )
    .await
    .unwrap_err();
    assert!(is_authentication_error(&error));
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_key_is_not_masked_by_the_cache_when_resolving_extensions() {
    let responses = [
        ("GET", "settings", 200),
        ("GET", "global-rules", 200),
        ("POST", "extensions/resolve", 401),
    ];
    let error = prepare_against_stub(43, &[], &responses).await.unwrap_err();
    assert!(is_authentication_error(&error));
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_key_is_not_masked_by_the_cache_when_resolving_missing_extensions() {
    // Cache is current (version 42) but "pdf" was never resolved, so it is fetched.
    let error = prepare_against_stub(42, &[], &[("POST", "extensions/resolve", 401)])
        .await
        .unwrap_err();
    assert!(is_authentication_error(&error));
}

#[tokio::test(flavor = "multi_thread")]
async fn local_problems_are_not_masked_by_the_cache() {
    // The control plane answers, but its source folder does not exist here. Falling
    // back to the cache (whose folder does exist) would hide a real misconfiguration.
    let all_ok = [
        ("GET", "settings", 200),
        ("GET", "global-rules", 200),
        ("POST", "extensions/resolve", 200),
    ];
    let (result, _directory) =
        prepare_against_stub_in(Some("/nonexistent/cleandl-test-folder"), 43, &[], &all_ok).await;

    let error = result.unwrap_err();
    assert!(format!("{error:#}").contains("Failed to read source directory"));
}

#[tokio::test(flavor = "multi_thread")]
async fn control_plane_failures_at_any_step_fall_back_to_the_cache() {
    // Newer remote version: settings, then global rules, then extensions fail in turn.
    let failing_steps: [StubResponses; 3] = [
        &[("GET", "settings", 500)],
        &[("GET", "settings", 200), ("GET", "global-rules", 500)],
        &[
            ("GET", "settings", 200),
            ("GET", "global-rules", 200),
            ("POST", "extensions/resolve", 500),
        ],
    ];
    for responses in failing_steps {
        let (config, entries) = prepare_against_stub(43, &[], responses)
            .await
            .unwrap()
            .unwrap();
        assert!(config.settings.trial_run);
        assert_eq!(entries.len(), 1);
    }

    // Current cache, but "pdf" was never resolved and resolving it fails.
    let (config, entries) = prepare_against_stub(42, &[], &[("POST", "extensions/resolve", 500)])
        .await
        .unwrap()
        .unwrap();
    assert!(config.settings.trial_run);
    assert_eq!(entries.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_fetch_refreshes_the_cache() {
    let all_ok = [
        ("GET", "settings", 200),
        ("GET", "global-rules", 200),
        ("POST", "extensions/resolve", 200),
    ];
    let (result, directory) = prepare_against_stub_in(None, 43, &[], &all_ok).await;

    let (_config, entries) = result.unwrap().unwrap();
    assert_eq!(entries.len(), 1);
    let cache = read_cache(&directory);
    assert_eq!(cache.configuration_version, 43);
    assert!(cache.resolved_extensions.contains("pdf"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_current_cache_fetches_only_missing_extensions_and_records_them() {
    // No settings or global-rules stubs are mounted: calling them would fail the run.
    let (result, directory) =
        prepare_against_stub_in(None, 42, &[], &[("POST", "extensions/resolve", 200)]).await;

    result.unwrap().unwrap();
    let cache = read_cache(&directory);
    assert_eq!(cache.configuration_version, 42);
    assert!(cache.resolved_extensions.contains("pdf"));
}

#[tokio::test(flavor = "multi_thread")]
async fn server_errors_still_fall_back_to_the_cache() {
    let (config, entries) = prepare_against_stub(
        43,
        &[],
        &[("GET", "settings", 200), ("GET", "global-rules", 500)],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(config.settings.trial_run);
    assert_eq!(entries.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn api_key_header_is_sent_with_every_control_plane_request() {
    let server = MockServer::start().await;
    // Each stub only matches when the key header is present; anything else is a 404.
    for (verb, endpoint, body) in [
        (
            "GET",
            "/configuration-version",
            serde_json::json!({"updated_at": 1}),
        ),
        ("GET", "/settings", serde_json::json!([])),
        ("GET", "/global-rules", serde_json::json!([])),
        ("POST", "/extensions/resolve", serde_json::json!([])),
        (
            "GET",
            "/account/me",
            serde_json::json!({"id": 1, "username": "mark", "role": "user"}),
        ),
    ] {
        Mock::given(method(verb))
            .and(path(endpoint))
            .and(header("X-API-Key", "secret-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
    }

    let api_url = server.uri();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let client = api_client("secret-key", Duration::from_secs(2))?;
        fetch_configuration_version(&client, &api_url).context("configuration-version")?;
        load_settings(&client, &api_url).context("settings")?;
        fetch_global_rules(&client, &api_url).context("global-rules")?;
        fetch_resolve_extensions(&client, &api_url, &["pdf".to_string()])
            .context("extensions/resolve")?;
        fetch_user_me(&client, &api_url)
            .map_err(|error| anyhow::anyhow!(error.to_string()))
            .context("account/me")?;
        Ok(())
    })
    .await
    .unwrap()
    .unwrap();
}

#[test]
fn unsendable_api_key_is_rejected_up_front_without_echoing_it() {
    let error = api_client("line\nbreak-secret", Duration::from_secs(2)).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("API key"));
    assert!(!message.contains("break-secret"));
}

#[test]
fn api_key_header_is_marked_sensitive() {
    let headers = api_key_headers("very-secret-key").unwrap();
    let value = headers.get(API_KEY_HEADER).unwrap();
    assert!(value.is_sensitive());
    assert!(!format!("{headers:?}").contains("very-secret-key"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_load_settings_rejects_unauthorized_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/settings"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;

    let error = request_settings(server.uri(), "invalid-key")
        .await
        .expect_err("an unauthorized response should fail");

    assert!(error.to_string().contains("Authentication failed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_load_settings_rejects_unexpected_json_shape() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/settings"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"source_folder": "Downloads"})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let error = request_settings(server.uri(), "secret-key")
        .await
        .expect_err("a non-array settings response should fail");

    assert!(error.to_string().contains("expected a JSON array"));
}

#[test]
fn test_rule_sorting_order() {
    let raw_json = r#"{
        "settings": {},
        "extensions": [{
            "name": "pdf",
            "default_destination": ["Downloads"],
            "append_year_month": false,
            "rules": [
                {"regex": "second", "destination": ["2"], "order": 2},
                {"regex": "first", "destination": ["1"], "order": 1}
            ]
        }]
    }"#;

    let mut config: ApiConfig = serde_json::from_str(raw_json).unwrap();
    let ext = config.extensions.remove(0);
    let mut rules = ext.rules;
    rules.sort_by_key(|r| r.order);

    assert_eq!(rules[0].order, 1);
    assert_eq!(rules[0].regex, "first");
    assert_eq!(rules[1].order, 2);
    assert_eq!(rules[1].regex, "second");
}

#[test]
fn destination_base_is_the_prefix_or_the_home_directory() {
    let home = Path::new("/home/test");

    assert_eq!(
        destination_base(Some(Path::new("/nas")), Some(home)).unwrap(),
        Path::new("/nas")
    );
    assert_eq!(destination_base(None, Some(home)).unwrap(), home);
}

#[test]
fn extension_rules_compile_in_order_and_invalid_patterns_are_dropped() {
    let rule = |regex: &str, destination: &str, order: i64| ApiRule {
        regex: regex.to_string(),
        destination: vec![destination.to_string()],
        order,
        append_year_month: false,
    };
    let compiled = compile_extensions(vec![ApiExtension {
        name: "pdf".to_string(),
        default_destination: vec!["Docs".to_string()],
        append_year_month: true,
        rules: vec![
            rule("second", "2", 2),
            rule("(unclosed", "bad", 3),
            rule("first", "1", 1),
        ],
    }]);

    let pdf = &compiled["pdf"]; // keyed by the (already lower-case) name
    assert!(pdf.append_year_month);
    let destinations: Vec<_> = pdf
        .rules
        .iter()
        .map(|rule| rule.destination[0].as_str())
        .collect();
    assert_eq!(destinations, ["1", "2"]);

    let patterns = compile_patterns(&["^ok".to_string(), "[bad".to_string()], "ignore");
    assert_eq!(patterns.len(), 1);
}

#[test]
fn extension_names_are_lowercased_once_when_read_from_json() {
    let extension: ApiExtension =
        serde_json::from_str(r#"{"name": "PDF", "default_destination": ["Docs"]}"#).unwrap();
    assert_eq!(extension.name, "pdf");

    let config: ApiConfig = serde_json::from_str(
        r#"{"settings": {}, "extensions": [{"name": "Mp3"}, {"name": "JPG"}]}"#,
    )
    .unwrap();
    let names: Vec<_> = config.extensions.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["mp3", "jpg"]);
}

#[test]
fn cached_extension_names_are_lowercased_when_the_cache_is_loaded() {
    // A cache written by an earlier version may hold names exactly as the server sent them.
    let directory = TempDir::new().unwrap();
    let path = directory.path().join(CACHE_FILE);
    let cache = CachedConfiguration {
        schema_version: CACHE_SCHEMA_VERSION,
        api_url: "https://example.test/api/v1".to_string(),
        api_key_fingerprint: api_key_fingerprint("secret-key"),
        configuration_version: 1,
        settings: HashMap::new(),
        global_rules: Vec::new(),
        extensions: vec![ApiExtension {
            name: "MP3".to_string(),
            default_destination: vec!["Music".to_string()],
            append_year_month: false,
            rules: Vec::new(),
        }],
        resolved_extensions: HashSet::new(),
    };
    save_cached_configuration(&path, &cache).unwrap();

    let loaded =
        load_cached_configuration(&path, "https://example.test/api/v1", "secret-key").unwrap();

    assert_eq!(loaded.extensions[0].name, "mp3");
}

#[tokio::test(flavor = "multi_thread")]
async fn resolved_extension_names_are_lowercased_when_fetched() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/extensions/resolve"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"name": "JPG", "default_destination": ["Pictures"]}
        ])))
        .mount(&server)
        .await;
    let api_url = server.uri();

    let extensions = tokio::task::spawn_blocking(move || {
        let client = api_client("secret-key", Duration::from_secs(2))?;
        fetch_resolve_extensions(&client, &api_url, &["jpg".to_string()])
    })
    .await
    .unwrap()
    .unwrap();

    assert_eq!(extensions[0].name, "jpg");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mixed_case_server_name_replaces_the_cached_entry_instead_of_duplicating_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/configuration-version"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"updated_at": 42})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/extensions/resolve"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"name": "PDF", "default_destination": ["NewDocs"]}
        ])))
        .mount(&server)
        .await;

    let directory = TempDir::new().unwrap();
    let source = directory.path().join("Downloads");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a.pdf"), "pdf").unwrap();
    let api_url = format!("{}/api/v1", server.uri());
    // The cache is current but "pdf" was never resolved, and it holds a stale entry
    // for it whose name has the server's capitalisation.
    let cache = CachedConfiguration {
        schema_version: CACHE_SCHEMA_VERSION,
        api_url: api_url.clone(),
        api_key_fingerprint: api_key_fingerprint("secret-key"),
        configuration_version: 42,
        settings: string_map(&[
            ("source_folder", &source.display().to_string()),
            ("trial_run", "true"),
        ]),
        global_rules: Vec::new(),
        extensions: vec![ApiExtension {
            name: "PDF".to_string(),
            default_destination: vec!["OldDocs".to_string()],
            append_year_month: false,
            rules: Vec::new(),
        }],
        resolved_extensions: HashSet::new(),
    };
    save_cached_configuration(&directory.path().join(CACHE_FILE), &cache).unwrap();
    let args = Cli::try_parse_from([
        "cleandl".to_string(),
        format!("--api-url={api_url}"),
        "--api-key=secret-key".to_string(),
    ])
    .unwrap();
    let log_dir = directory.path().to_path_buf();

    let (config, _entries) = tokio::task::spawn_blocking(move || {
        prepare_api_run(&args, &AtomicBool::new(false), &log_dir)
    })
    .await
    .unwrap()
    .unwrap()
    .unwrap();

    assert_eq!(config.extensions.len(), 1);
    assert_eq!(config.extensions[0].default_destination, ["NewDocs"]);
    let saved = read_cache(&directory);
    assert_eq!(saved.extensions.len(), 1);
    assert_eq!(saved.extensions[0].name, "pdf");
    assert_eq!(saved.extensions[0].default_destination, ["NewDocs"]);
}

#[test]
fn routing_uses_the_first_matching_rule_and_skips_unhandled_files() {
    let rule = |regex: &str, destination: &str, order: i64, month: bool| ApiRule {
        regex: regex.to_string(),
        destination: vec![destination.to_string()],
        order,
        append_year_month: month,
    };
    let mut config = sequential_test_config(Path::new("/dest"));
    config.extensions[0].rules = vec![
        rule("^invoice", "Finance", 2, true),
        rule("invoice", "Other", 1, false),
    ];
    let run = DirectoryRun::new(config, None).unwrap();
    let route = |name: &str| run.route(Path::new(name), name);

    // Rules are tried in `order`, so "Other" (order 1) wins over "Finance" (order 2).
    let invoice = route("invoice-1.pdf").unwrap();
    assert_eq!(invoice.relative_dir, Path::new("Other"));
    assert!(!invoice.append_year_month);
    assert_eq!(route("scan.pdf").unwrap().relative_dir, Path::new("sorted"));
    // The extension match is case-insensitive.
    assert_eq!(route("SCAN.PDF").unwrap().extension, "pdf");
    assert!(route("README").is_none());
    assert!(route("photo.png").is_none());
}

#[test]
fn routing_refuses_empty_and_unsafe_destinations() {
    for destination in [vec![], vec!["..".to_string(), "outside".to_string()]] {
        let mut config = sequential_test_config(Path::new("/dest"));
        config.extensions[0].default_destination = destination;
        let run = DirectoryRun::new(config, None).unwrap();
        assert!(run.route(Path::new("a.pdf"), "a.pdf").is_none());
    }
}

#[test]
fn ignored_files_are_left_alone_and_dry_run_deletes_nothing() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("keep-me.pdf"), "ignored").unwrap();
    fs::write(source.join("move-me.pdf"), "moved").unwrap();
    let mut config = sequential_test_config(&destination);
    config.ignore_rules = vec!["^keep".to_string()];
    let entries: Vec<_> = fs::read_dir(&source).unwrap().collect();
    process_directory(config, None, entries, None).unwrap();
    assert!(source.join("keep-me.pdf").exists());
    assert!(!source.join("move-me.pdf").exists());
    assert!(destination.join("sorted/move-me.pdf").exists());
    assert!(!destination.join("sorted/keep-me.pdf").exists());

    // A dry run only reports what a delete rule would do.
    fs::write(source.join("junk.tmp"), "junk").unwrap();
    let mut config = sequential_test_config(&destination);
    config.settings.trial_run = true;
    config.delete_rules = vec![r"\.tmp$".to_string()];
    let entries: Vec<_> = fs::read_dir(&source).unwrap().collect();
    process_directory(config, None, entries, None).unwrap();
    assert!(source.join("junk.tmp").exists());
}

// Category 2: Integration Tests (Filesystem Sandbox)
fn setup_test_environment() -> (TempDir, PathBuf, PathBuf) {
    let temp_dir = TempDir::new().unwrap();
    let source_path = temp_dir.path().join("Downloads");
    let dest_prefix = temp_dir.path().join("NAS");

    fs::create_dir_all(&source_path).unwrap();
    fs::create_dir_all(&dest_prefix).unwrap();

    (temp_dir, source_path, dest_prefix)
}

#[test]
fn test_directory_processing_and_routing() {
    let (_temp, source_path, dest_prefix) = setup_test_environment();

    let sample_file = source_path.join("Scan 0001.pdf");
    fs::write(&sample_file, "dummy content").unwrap();

    let ignore_file = source_path.join(".stignore");
    fs::write(&ignore_file, "ignore me").unwrap();

    let delete_file = source_path.join("file.sync-conflict-2026.pdf");
    fs::write(&delete_file, "conflict data").unwrap();

    // Capture expected year/month from the source file *before* it is moved
    let metadata = fs::metadata(&sample_file).unwrap();
    let mtime = FileTime::from_last_modification_time(&metadata);
    let dt = DateTime::from_timestamp(mtime.unix_seconds(), mtime.nanoseconds())
        .unwrap()
        .with_timezone(&Local);
    let year = dt.year().to_string();
    let month = format!("{:02}", dt.month());

    let settings = Settings {
        source_folder: source_path.to_str().unwrap().to_string(),
        destination_prefix: Some(dest_prefix.clone()),
        trial_run: false,
        debug: false,
    };

    let pdf_ext = ApiExtension {
        name: "pdf".to_string(),
        default_destination: vec!["Downloads".to_string()],
        append_year_month: false,
        rules: vec![ApiRule {
            regex: r"^Scan ".to_string(),
            destination: vec!["Documents".to_string(), "Scanned".to_string()],
            order: 1,
            append_year_month: true,
        }],
    };

    let config = ApiConfig {
        settings,
        ignore_rules: vec![r"^\.".to_string()],
        delete_rules: vec!["sync-conflict".to_string()],
        extensions: vec![pdf_ext],
    };

    let entries: Vec<std::io::Result<fs::DirEntry>> = fs::read_dir(&source_path).unwrap().collect();
    process_directory(config, None, entries, None).unwrap();

    assert!(!delete_file.exists());
    assert!(ignore_file.exists());
    assert!(!sample_file.exists());

    let expected_dest = dest_prefix
        .join("Documents/Scanned")
        .join(year)
        .join(month)
        .join("Scan 0001.pdf");

    assert!(expected_dest.exists());
}

#[test]
fn test_trial_run_mode() {
    let (_temp, source_path, dest_prefix) = setup_test_environment();

    let sample_file = source_path.join("test.exe");
    fs::write(&sample_file, "exe payload").unwrap();

    let settings = Settings {
        source_folder: source_path.to_str().unwrap().to_string(),
        destination_prefix: Some(dest_prefix.clone()),
        trial_run: true,
        debug: false,
    };

    let exe_ext = ApiExtension {
        name: "exe".to_string(),
        default_destination: vec!["Software".to_string()],
        append_year_month: false,
        rules: vec![],
    };

    let config = ApiConfig {
        settings,
        ignore_rules: vec![],
        delete_rules: vec![],
        extensions: vec![exe_ext],
    };

    let entries: Vec<std::io::Result<fs::DirEntry>> = fs::read_dir(&source_path).unwrap().collect();
    process_directory(config, None, entries, None).unwrap();

    assert!(sample_file.exists());
    let uncreated_dest = dest_prefix.join("Software/test.exe");
    assert!(!uncreated_dest.exists());
}

#[test]
fn test_timestamp_preservation() {
    let temp_dir = TempDir::new().unwrap();
    let src_file = temp_dir.path().join("src.txt");
    let dst_file = temp_dir.path().join("dst.txt");

    fs::write(&src_file, "timestamp payload").unwrap();

    let past_time = FileTime::from_unix_time(1_600_000_000, 0);
    filetime::set_file_times(&src_file, past_time, past_time).unwrap();

    let metadata = fs::metadata(&src_file).unwrap();
    copy_move(&src_file, &dst_file, &metadata).unwrap();

    assert!(!src_file.exists());
    assert!(dst_file.exists());

    let dst_metadata = fs::metadata(&dst_file).unwrap();
    let dst_mtime = FileTime::from_last_modification_time(&dst_metadata);

    assert_eq!(dst_mtime.unix_seconds(), past_time.unix_seconds());
}
