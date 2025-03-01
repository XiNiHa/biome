use super::process_file::{process_file, DiffKind, FileStatus, Message};
use super::ExecutionEnvironment;
use crate::cli_options::CliOptions;
use crate::execute::diagnostics::{
    CIFormatDiffDiagnostic, CIOrganizeImportsDiffDiagnostic, ContentDiffAdvice,
    FormatDiffDiagnostic, OrganizeImportsDiffDiagnostic, PanicDiagnostic,
};
use crate::{CliDiagnostic, CliSession, Execution, FormatterReportSummary, Report, TraversalMode};
use biome_console::fmt::Formatter;
use biome_console::{fmt, markup, Console, ConsoleExt};
use biome_diagnostics::DiagnosticTags;
use biome_diagnostics::PrintGitHubDiagnostic;
use biome_diagnostics::{category, DiagnosticExt, Error, PrintDiagnostic, Resource, Severity};
use biome_fs::{BiomePath, FileSystem, PathInterner};
use biome_fs::{TraversalContext, TraversalScope};
use biome_service::workspace::{FeaturesBuilder, IsPathIgnoredParams};
use biome_service::{extension_error, workspace::SupportsFeatureParams, Workspace, WorkspaceError};
use crossbeam::channel::{unbounded, Receiver, Sender};
use rustc_hash::FxHashSet;
use std::sync::atomic::AtomicU32;
use std::{
    ffi::OsString,
    io,
    panic::catch_unwind,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU16, AtomicUsize, Ordering},
        Once,
    },
    thread,
    time::{Duration, Instant},
};

struct SummaryResult<'a> {
    changed: usize,
    unchanged: usize,
    duration: Duration,
    errors: u32,
    warnings: u32,
    traversal: &'a TraversalMode,
}

struct Files(usize);

impl fmt::Display for Files {
    fn fmt(&self, fmt: &mut Formatter) -> io::Result<()> {
        fmt.write_markup(markup!({self.0} " "))?;
        if self.0 == 1 {
            fmt.write_str("file")
        } else {
            fmt.write_str("files")
        }
    }
}

struct SummaryDetail(usize);

impl fmt::Display for SummaryDetail {
    fn fmt(&self, fmt: &mut Formatter) -> io::Result<()> {
        if self.0 > 0 {
            fmt.write_markup(markup! {
                ". Fixed "{Files(self.0)}"."
            })
        } else {
            fmt.write_markup(markup! {
                ". No fixes needed."
            })
        }
    }
}

struct SummaryTotal<'a>(&'a TraversalMode, usize, &'a Duration);

impl<'a> fmt::Display for SummaryTotal<'a> {
    fn fmt(&self, fmt: &mut Formatter) -> io::Result<()> {
        let files = Files(self.1);
        match self.0 {
            TraversalMode::Check { .. } | TraversalMode::Lint { .. } | TraversalMode::CI { .. } => {
                fmt.write_markup(markup! {
                    "Checked "{files}" in "{self.2}
                })
            }
            TraversalMode::Format { write, .. } => {
                if *write {
                    fmt.write_markup(markup! {
                        "Formatted "{files}" in "{self.2}
                    })
                } else {
                    fmt.write_markup(markup! {
                        "Checked "{files}" in "{self.2}
                    })
                }
            }

            TraversalMode::Migrate { write, .. } => {
                if *write {
                    fmt.write_markup(markup! {
                      "Migrated your configuration file in "{self.2}
                    })
                } else {
                    fmt.write_markup(markup! {
                        "Checked your configuration file in "{self.2}
                    })
                }
            }
        }
    }
}

impl<'a> fmt::Display for SummaryResult<'a> {
    fn fmt(&self, fmt: &mut Formatter) -> io::Result<()> {
        let summary = SummaryTotal(
            self.traversal,
            self.changed + self.unchanged,
            &self.duration,
        );
        let detail = SummaryDetail(self.changed);
        fmt.write_markup(markup!(<Info>{summary}{detail}</Info>))?;

        if self.errors > 0 {
            if self.errors == 1 {
                fmt.write_markup(markup!("\n"<Error>"Found "{self.errors}" error."</Error>))?;
            } else {
                fmt.write_markup(markup!("\n"<Error>"Found "{self.errors}" errors."</Error>))?;
            }
        }
        if self.warnings > 0 {
            if self.warnings == 1 {
                fmt.write_markup(markup!("\n"<Warn>"Found "{self.warnings}" warning."</Warn>))?;
            } else {
                fmt.write_markup(markup!("\n"<Warn>"Found "{self.warnings}" warnings."</Warn>))?;
            }
        }
        Ok(())
    }
}

///
pub(crate) fn traverse(
    execution: Execution,
    session: CliSession,
    cli_options: &CliOptions,
    inputs: Vec<OsString>,
) -> Result<(), CliDiagnostic> {
    init_thread_pool();
    if inputs.is_empty()
        && execution.as_stdin_file().is_none()
        && !cli_options.no_errors_on_unmatched
    {
        return Err(CliDiagnostic::missing_argument(
            "<INPUT>",
            format!("{}", execution.traversal_mode),
        ));
    }

    let (interner, recv_files) = PathInterner::new();
    let (sender, receiver) = unbounded();

    let changed = AtomicUsize::new(0);
    let unchanged = AtomicUsize::new(0);
    let skipped = AtomicUsize::new(0);

    let fs = &*session.app.fs;
    let workspace = &*session.app.workspace;
    let console = &mut *session.app.console;

    let max_diagnostics = execution.get_max_diagnostics();
    let remaining_diagnostics = AtomicU16::new(max_diagnostics);

    let mut report = Report::default();

    let printer = DiagnosticsPrinter::new(&execution)
        .with_verbose(cli_options.verbose)
        .with_diagnostic_level(cli_options.diagnostic_level)
        .with_max_diagnostics(max_diagnostics);

    let duration = thread::scope(|s| {
        let handler = thread::Builder::new()
            .name(String::from("biome::console"))
            .spawn_scoped(s, || {
                printer.run(receiver, recv_files, console);
            })
            .expect("failed to spawn console thread");

        // The traversal context is scoped to ensure all the channels it
        // contains are properly closed once the traversal finishes
        let elapsed = traverse_inputs(
            fs,
            inputs,
            &TraversalOptions {
                fs,
                workspace,
                execution: &execution,
                interner,
                changed: &changed,
                unchanged: &unchanged,
                skipped: &skipped,
                messages: sender,
                remaining_diagnostics: &remaining_diagnostics,
            },
        );
        // wait for the main thread to finish
        handler.join().unwrap();

        elapsed
    });

    let errors = printer.errors();
    let warnings = printer.warnings();
    let changed = changed.load(Ordering::Relaxed);
    let unchanged = unchanged.load(Ordering::Relaxed);
    let count = changed + unchanged;
    let skipped = skipped.load(Ordering::Relaxed);

    if execution.should_report_to_terminal() {
        console.log(markup! {
            {SummaryResult {
                changed,
                unchanged,
                duration,
                errors,
                warnings,
                traversal: execution.traversal_mode()
            }}
        });
    } else {
        if let TraversalMode::Format { write, .. } = execution.traversal_mode() {
            let mut summary = FormatterReportSummary::default();
            if *write {
                summary.set_files_written(count);
            } else {
                summary.set_files_compared(count);
            }
            report.set_formatter_summary(summary);
        }

        let to_print = report.as_serialized_reports()?;
        console.log(markup! {
            {to_print}
        });
        return Ok(());
    }

    if skipped > 0 {
        if skipped == 1 {
            console.log(markup! {
                <Warn>"Skipped "{skipped}" file."</Warn>
            });
        } else {
            console.log(markup! {
                <Warn>"Skipped "{skipped}" files."</Warn>
            });
        }
    }

    let should_exit_on_warnings = warnings > 0 && cli_options.error_on_warnings;
    // Processing emitted error diagnostics, exit with a non-zero code
    if count.saturating_sub(skipped) == 0 && !cli_options.no_errors_on_unmatched {
        Err(CliDiagnostic::no_files_processed())
    } else if errors > 0 || should_exit_on_warnings {
        let category = execution.as_diagnostic_category();
        if should_exit_on_warnings {
            if execution.is_check_apply() {
                Err(CliDiagnostic::apply_warnings(category))
            } else {
                Err(CliDiagnostic::check_warnings(category))
            }
        } else if execution.is_check_apply() {
            Err(CliDiagnostic::apply_error(category))
        } else {
            Err(CliDiagnostic::check_error(category))
        }
    } else {
        Ok(())
    }
}

/// This function will setup the global Rayon thread pool the first time it's called
///
/// This is currently only used to assign friendly debug names to the threads of the pool
fn init_thread_pool() {
    static INIT_ONCE: Once = Once::new();
    INIT_ONCE.call_once(|| {
        rayon::ThreadPoolBuilder::new()
            .thread_name(|index| format!("biome::worker_{index}"))
            .build_global()
            .expect("failed to initialize the global thread pool");
    });
}

/// Initiate the filesystem traversal tasks with the provided input paths and
/// run it to completion, returning the duration of the process
fn traverse_inputs(fs: &dyn FileSystem, inputs: Vec<OsString>, ctx: &TraversalOptions) -> Duration {
    let start = Instant::now();
    fs.traversal(Box::new(move |scope: &dyn TraversalScope| {
        for input in inputs {
            scope.spawn(ctx, PathBuf::from(input));
        }
    }));

    start.elapsed()
}

// struct DiagnosticsReporter<'ctx> {}

struct DiagnosticsPrinter<'ctx> {
    ///  Execution of the traversal
    execution: &'ctx Execution,
    /// The maximum number of diagnostics the console thread is allowed to print
    max_diagnostics: u32,
    /// The approximate number of diagnostics the console will print before
    /// folding the rest into the "skipped diagnostics" counter
    remaining_diagnostics: AtomicU32,
    /// Mutable reference to a boolean flag tracking whether the console thread
    /// printed any error-level message
    errors: AtomicU32,
    /// Mutable reference to a boolean flag tracking whether the console thread
    /// printed any warnings-level message
    warnings: AtomicU32,
    /// Whether the console thread should print diagnostics in verbose mode
    verbose: bool,
    /// The diagnostic level the console thread should print
    diagnostic_level: Severity,

    not_printed_diagnostics: AtomicU32,
    printed_diagnostics: AtomicU32,
}

impl<'ctx> DiagnosticsPrinter<'ctx> {
    fn new(execution: &'ctx Execution) -> Self {
        Self {
            errors: AtomicU32::new(0),
            warnings: AtomicU32::new(0),
            remaining_diagnostics: AtomicU32::new(0),
            execution,
            diagnostic_level: Severity::Hint,
            verbose: false,
            max_diagnostics: 20,
            not_printed_diagnostics: AtomicU32::new(0),
            printed_diagnostics: AtomicU32::new(0),
        }
    }

    fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    fn with_max_diagnostics(mut self, value: u16) -> Self {
        self.max_diagnostics = value as u32;
        self
    }

    fn with_diagnostic_level(mut self, value: Severity) -> Self {
        self.diagnostic_level = value;
        self
    }

    fn errors(&self) -> u32 {
        self.errors.load(Ordering::Relaxed)
    }

    fn warnings(&self) -> u32 {
        self.warnings.load(Ordering::Relaxed)
    }

    /// Checks if the diagnostic we received from the thread should be considered or not. Logic:
    /// - it should not be considered if its severity level is lower than the one provided via CLI;
    /// - it should not be considered if it's a verbose diagnostic and the CLI **didn't** request a `--verbose` option.
    fn should_skip_diagnostic(&self, severity: Severity, diagnostic_tags: DiagnosticTags) -> bool {
        if severity < self.diagnostic_level {
            return true;
        }

        if diagnostic_tags.is_verbose() && !self.verbose {
            return true;
        }

        false
    }

    /// Count the diagnostic, and then returns a boolean that tells if it should be printed
    fn should_print(&self) -> bool {
        let printed_diagnostics = self.printed_diagnostics.load(Ordering::Relaxed);
        let should_print = printed_diagnostics < self.max_diagnostics;
        if should_print {
            self.printed_diagnostics.fetch_add(1, Ordering::Relaxed);
            self.remaining_diagnostics.store(
                self.max_diagnostics.saturating_sub(printed_diagnostics),
                Ordering::Relaxed,
            );
        } else {
            self.not_printed_diagnostics.fetch_add(1, Ordering::Relaxed);
        }

        should_print
    }

    fn run(
        &self,
        receiver: Receiver<Message>,
        interner: Receiver<PathBuf>,
        console: &'ctx mut dyn Console,
    ) {
        let mut paths: FxHashSet<String> = FxHashSet::default();
        let mut total_skipped_suggested_fixes = 0;

        let mut diagnostics_to_print = vec![];

        while let Ok(msg) = receiver.recv() {
            match msg {
                Message::SkippedFixes {
                    skipped_suggested_fixes,
                } => {
                    total_skipped_suggested_fixes += skipped_suggested_fixes;
                }

                Message::Failure => {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                }

                Message::Error(mut err) => {
                    let location = err.location();
                    if self.should_skip_diagnostic(err.severity(), err.tags()) {
                        continue;
                    }
                    if err.severity() == Severity::Warning {
                        // *warnings += 1;
                        self.warnings.fetch_add(1, Ordering::Relaxed);
                        // self.warnings.set(self.warnings.get() + 1)
                    }
                    if let Some(Resource::File(file_path)) = location.resource.as_ref() {
                        // Retrieves the file name from the file ID cache, if it's a miss
                        // flush entries from the interner channel until it's found
                        let file_name = match paths.get(*file_path) {
                            Some(path) => Some(path),
                            None => loop {
                                match interner.recv() {
                                    Ok(path) => {
                                        paths.insert(path.display().to_string());
                                        if path.display().to_string() == *file_path {
                                            break paths.get(&path.display().to_string());
                                        }
                                    }
                                    // In case the channel disconnected without sending
                                    // the path we need, print the error without a file
                                    // name (normally this should never happen)
                                    Err(_) => break None,
                                }
                            },
                        };

                        if let Some(path) = file_name {
                            err = err.with_file_path(path.as_str());
                        }
                    }

                    let should_print = self.should_print();

                    if self.execution.should_report_to_terminal() && should_print {
                        diagnostics_to_print.push(err);
                    }
                }

                Message::Diagnostics {
                    name,
                    content,
                    diagnostics,
                    skipped_diagnostics,
                } => {
                    self.not_printed_diagnostics
                        .fetch_add(skipped_diagnostics, Ordering::Relaxed);

                    // is CI mode we want to print all the diagnostics
                    if self.execution.is_ci() {
                        for diag in diagnostics {
                            let severity = diag.severity();
                            if self.should_skip_diagnostic(severity, diag.tags()) {
                                continue;
                            }

                            if severity == Severity::Error {
                                self.errors.fetch_add(1, Ordering::Relaxed);
                            }
                            if severity == Severity::Warning {
                                self.warnings.fetch_add(1, Ordering::Relaxed);
                            }

                            let diag = diag.with_file_path(&name).with_file_source_code(&content);
                            diagnostics_to_print.push(diag);
                        }
                    } else {
                        for diag in diagnostics {
                            let severity = diag.severity();
                            if self.should_skip_diagnostic(severity, diag.tags()) {
                                continue;
                            }
                            if severity == Severity::Error {
                                self.errors.fetch_add(1, Ordering::Relaxed);
                            }
                            if severity == Severity::Warning {
                                self.warnings.fetch_add(1, Ordering::Relaxed);
                            }

                            let should_print = self.should_print();

                            if self.execution.should_report_to_terminal() && should_print {
                                let diag =
                                    diag.with_file_path(&name).with_file_source_code(&content);
                                diagnostics_to_print.push(diag)
                            }
                        }
                    }
                }
                Message::Diff {
                    file_name,
                    old,
                    new,
                    diff_kind,
                } => {
                    let is_error = self.execution.is_ci() || !self.execution.is_format_write();
                    // A diff is an error in CI mode and in format check mode
                    if self.execution.is_ci() || !self.execution.is_format_write() {
                        self.errors.fetch_add(1, Ordering::Relaxed);
                    }

                    let severity: Severity = if is_error {
                        Severity::Error
                    } else {
                        // we set lowest
                        Severity::Hint
                    };

                    if self.should_skip_diagnostic(severity, DiagnosticTags::empty()) {
                        continue;
                    }

                    let should_print = self.should_print();

                    if self.execution.should_report_to_terminal() && should_print {
                        if self.execution.is_ci() {
                            match diff_kind {
                                DiffKind::Format => {
                                    let diag = CIFormatDiffDiagnostic {
                                        file_name: file_name.clone(),
                                        diff: ContentDiffAdvice {
                                            old: old.clone(),
                                            new: new.clone(),
                                        },
                                    };
                                    diagnostics_to_print.push(Error::from(diag))
                                }
                                DiffKind::OrganizeImports => {
                                    let diag = CIOrganizeImportsDiffDiagnostic {
                                        file_name: file_name.clone(),
                                        diff: ContentDiffAdvice {
                                            old: old.clone(),
                                            new: new.clone(),
                                        },
                                    };
                                    diagnostics_to_print.push(Error::from(diag))
                                }
                            };
                        } else {
                            match diff_kind {
                                DiffKind::Format => {
                                    let diag = FormatDiffDiagnostic {
                                        file_name: file_name.clone(),
                                        diff: ContentDiffAdvice {
                                            old: old.clone(),
                                            new: new.clone(),
                                        },
                                    };
                                    diagnostics_to_print.push(Error::from(diag))
                                }
                                DiffKind::OrganizeImports => {
                                    let diag = OrganizeImportsDiffDiagnostic {
                                        file_name: file_name.clone(),
                                        diff: ContentDiffAdvice {
                                            old: old.clone(),
                                            new: new.clone(),
                                        },
                                    };
                                    diagnostics_to_print.push(Error::from(diag))
                                }
                            };
                        }
                    }
                }
            }
        }

        let running_on_github = matches!(
            self.execution.traversal_mode(),
            TraversalMode::CI {
                environment: Some(ExecutionEnvironment::GitHub),
            }
        );

        for diagnostic in diagnostics_to_print {
            if diagnostic.severity() >= self.diagnostic_level {
                if diagnostic.tags().is_verbose() {
                    if self.verbose {
                        console.error(markup! {{PrintDiagnostic::verbose(&diagnostic)}})
                    }
                } else {
                    console.error(markup! {{PrintDiagnostic::simple(&diagnostic)}})
                }
            }

            if running_on_github {
                console.log(markup! {{PrintGitHubDiagnostic::simple(&diagnostic)}});
            }
        }

        if self.execution.is_check() && total_skipped_suggested_fixes > 0 {
            console.log(markup! {
                <Warn>"Skipped "{total_skipped_suggested_fixes}" suggested fixes.\n"</Warn>
                <Info>"If you wish to apply the suggested (unsafe) fixes, use the command "<Emphasis>"biome check --apply-unsafe\n"</Emphasis></Info>
            })
        }

        let not_printed_diagnostics = self.not_printed_diagnostics.load(Ordering::Relaxed);
        if !self.execution.is_ci() && not_printed_diagnostics > 0 {
            console.log(markup! {
                <Warn>"The number of diagnostics exceeds the number allowed by Biome.\n"</Warn>
                <Info>"Diagnostics not shown: "</Info><Emphasis>{not_printed_diagnostics}</Emphasis><Info>"."</Info>
            })
        }
    }
}

/// Context object shared between directory traversal tasks
pub(crate) struct TraversalOptions<'ctx, 'app> {
    /// Shared instance of [FileSystem]
    pub(crate) fs: &'app dyn FileSystem,
    /// Instance of [Workspace] used by this instance of the CLI
    pub(crate) workspace: &'ctx dyn Workspace,
    /// Determines how the files should be processed
    pub(crate) execution: &'ctx Execution,
    /// File paths interner cache used by the filesystem traversal
    interner: PathInterner,
    /// Shared atomic counter storing the number of changed files
    changed: &'ctx AtomicUsize,
    /// Shared atomic counter storing the number of unchanged files
    unchanged: &'ctx AtomicUsize,
    /// Shared atomic counter storing the number of skipped files
    skipped: &'ctx AtomicUsize,
    /// Channel sending messages to the display thread
    pub(crate) messages: Sender<Message>,
    /// The approximate number of diagnostics the console will print before
    /// folding the rest into the "skipped diagnostics" counter
    pub(crate) remaining_diagnostics: &'ctx AtomicU16,
}

impl<'ctx, 'app> TraversalOptions<'ctx, 'app> {
    pub(crate) fn increment_changed(&self) {
        self.changed.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn increment_unchanged(&self) {
        self.unchanged.fetch_add(1, Ordering::Relaxed);
    }

    /// Send a message to the display thread
    pub(crate) fn push_message(&self, msg: impl Into<Message>) {
        self.messages.send(msg.into()).ok();
    }

    pub(crate) fn miss_handler_err(&self, err: WorkspaceError, biome_path: &BiomePath) {
        self.push_diagnostic(
            err.with_category(category!("files/missingHandler"))
                .with_file_path(biome_path.display().to_string())
                .with_tags(DiagnosticTags::VERBOSE),
        );
    }

    pub(crate) fn protected_file(&self, biome_path: &BiomePath) {
        self.push_diagnostic(
            WorkspaceError::protected_file(biome_path.display().to_string()).into(),
        )
    }
}

impl<'ctx, 'app> TraversalContext for TraversalOptions<'ctx, 'app> {
    fn interner(&self) -> &PathInterner {
        &self.interner
    }

    fn push_diagnostic(&self, error: Error) {
        self.push_message(error);
    }

    fn can_handle(&self, biome_path: &BiomePath) -> bool {
        if !self.fs.path_is_file(biome_path.as_path()) {
            // handle:
            // - directories
            // - symlinks
            // - unresolved symlinks
            //   e.g `symlink/subdir` where symlink points to a directory that includes `subdir`.
            //   Note that `symlink/subdir` is not an existing file.
            let can_handle = !self
                .workspace
                .is_path_ignored(IsPathIgnoredParams {
                    biome_path: biome_path.clone(),
                    feature: self.execution.as_feature_name(),
                })
                .unwrap_or_else(|err| {
                    self.push_diagnostic(err.into());
                    false
                });
            return can_handle;
        }

        let file_features = self.workspace.file_features(SupportsFeatureParams {
            path: biome_path.clone(),
            feature: FeaturesBuilder::new()
                .with_linter()
                .with_formatter()
                .with_organize_imports()
                .build(),
        });

        let file_features = match file_features {
            Ok(file_features) => {
                if file_features.is_protected() {
                    self.protected_file(biome_path);
                    return false;
                }

                if file_features.is_not_supported() && !file_features.is_ignored() {
                    // we should throw a diagnostic if we can't handle a file that isn't ignored
                    self.miss_handler_err(extension_error(biome_path), biome_path);
                    return false;
                }
                file_features
            }
            Err(err) => {
                self.miss_handler_err(err, biome_path);

                return false;
            }
        };
        match self.execution.traversal_mode() {
            TraversalMode::Check { .. } | TraversalMode::CI { .. } => {
                file_features.supports_lint()
                    || file_features.supports_format()
                    || file_features.supports_organize_imports()
            }
            TraversalMode::Format { .. } => file_features.supports_format(),
            TraversalMode::Lint { .. } => file_features.supports_lint(),
            // Imagine if Biome can't handle its own configuration file...
            TraversalMode::Migrate { .. } => true,
        }
    }

    fn handle_file(&self, path: &Path) {
        handle_file(self, path)
    }
}

/// This function wraps the [process_file] function implementing the traversal
/// in a [catch_unwind] block and emit diagnostics in case of error (either the
/// traversal function returns Err or panics)
fn handle_file(ctx: &TraversalOptions, path: &Path) {
    match catch_unwind(move || process_file(ctx, path)) {
        Ok(Ok(FileStatus::Changed)) => {
            ctx.increment_changed();
        }
        Ok(Ok(FileStatus::Unchanged)) => {
            ctx.increment_unchanged();
        }
        Ok(Ok(FileStatus::Message(msg))) => {
            ctx.increment_unchanged();
            ctx.push_message(msg);
        }
        Ok(Ok(FileStatus::Protected(file_path))) => {
            ctx.increment_unchanged();
            ctx.push_diagnostic(WorkspaceError::protected_file(file_path).into());
        }
        Ok(Ok(FileStatus::Ignored)) => {}
        Ok(Err(err)) => {
            ctx.increment_unchanged();
            ctx.skipped.fetch_add(1, Ordering::Relaxed);
            ctx.push_message(err);
        }
        Err(err) => {
            let message = match err.downcast::<String>() {
                Ok(msg) => format!("processing panicked: {msg}"),
                Err(err) => match err.downcast::<&'static str>() {
                    Ok(msg) => format!("processing panicked: {msg}"),
                    Err(_) => String::from("processing panicked"),
                },
            };

            ctx.push_message(
                PanicDiagnostic { message }.with_file_path(path.display().to_string()),
            );
        }
    }
}
