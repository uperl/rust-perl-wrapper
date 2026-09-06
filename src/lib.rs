//! A configured Perl interpreter: where `perl` and `make` live, where newly
//! built modules are installed, and which directories go on `PERL5LIB`.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use anyhow::{Context, Result, anyhow};

/// A wrapper around a Perl interpreter and the environment used to build and
/// install CPAN distributions with it.
///
/// Construct one with [`Perl::new`] (which finds `perl` on `PATH`) or
/// [`Perl::with_perl`] (which takes an explicit interpreter), then adjust it
/// with [`with_install_base`](Self::with_install_base) and
/// [`with_lib`](Self::with_lib).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Perl {
    /// Full path to the `perl` executable.
    pub perl: PathBuf,

    /// Full path to `make`, or `None` when no `make` is on `PATH`. Required to
    /// build `ExtUtils::MakeMaker` distributions; `Module::Build` distributions
    /// do not need it.
    pub make: Option<PathBuf>,

    /// Full path to the install location for newly installed modules — the
    /// equivalent of `ExtUtils::MakeMaker`'s `INSTALL_BASE` or `Module::Build`'s
    /// `--install_base`. `None` installs to the interpreter's default location.
    pub install_base: Option<PathBuf>,

    /// Full paths of directories to search for Perl modules. Joined with the
    /// platform path separator by [`perl5lib`](Self::perl5lib) to form the
    /// `PERL5LIB` environment variable.
    pub lib: Vec<PathBuf>,

    /// When `true`, [`execute_perl`](Self::execute_perl) and
    /// [`execute_make`](Self::execute_make) capture the child's merged
    /// stdout+stderr into [`ExecuteResult::output`] instead of letting it write
    /// to this process's streams. Off by default; set with
    /// [`with_capture_output`](Self::with_capture_output).
    pub capture_output: bool,

    /// Directory to run commands in. `None` (the default) uses this process's
    /// working directory; set with [`with_current_dir`](Self::with_current_dir).
    pub current_dir: Option<PathBuf>,
}

impl Perl {
    /// Create a wrapper, resolving `perl` and `make` from `PATH`.
    ///
    /// The first `perl` on `PATH` is used; an error is returned if there is
    /// none. The first `make` on `PATH` is used when present, otherwise
    /// [`make`](Self::make) is `None`. [`install_base`](Self::install_base) is
    /// `None` (install to the default location) and [`lib`](Self::lib) is empty;
    /// set them with [`with_install_base`](Self::with_install_base) and
    /// [`with_lib`](Self::with_lib).
    pub fn new() -> Result<Self> {
        let perl =
            which::which("perl").map_err(|e| anyhow!("no `perl` executable found on PATH: {e}"))?;
        Ok(Self::with_perl(perl))
    }

    /// Create a wrapper that uses `perl` as the interpreter instead of searching
    /// `PATH`.
    ///
    /// `make` is still resolved from `PATH` (first match, or `None`);
    /// [`install_base`](Self::install_base) is `None` and [`lib`](Self::lib) is
    /// empty.
    pub fn with_perl(perl: impl Into<PathBuf>) -> Self {
        Self {
            perl: perl.into(),
            make: which::which("make").ok(),
            install_base: None,
            lib: Vec::new(),
            capture_output: false,
            current_dir: None,
        }
    }

    /// Set the install location for newly installed modules
    /// ([`install_base`](Self::install_base)).
    #[must_use]
    pub fn with_install_base(mut self, install_base: impl Into<PathBuf>) -> Self {
        self.install_base = Some(install_base.into());
        self
    }

    /// Enable or disable capturing the child's merged stdout+stderr
    /// ([`capture_output`](Self::capture_output)).
    #[must_use]
    pub fn with_capture_output(mut self, capture: bool) -> Self {
        self.capture_output = capture;
        self
    }

    /// Run commands in `dir` ([`current_dir`](Self::current_dir)).
    #[must_use]
    pub fn with_current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }

    /// Replace the module search path ([`lib`](Self::lib)).
    #[must_use]
    pub fn with_lib<I, P>(mut self, lib: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.lib = lib.into_iter().map(Into::into).collect();
        self
    }

    /// Override [`make`](Self::make) with an explicit path.
    #[must_use]
    pub fn with_make(mut self, make: impl Into<PathBuf>) -> Self {
        self.make = Some(make.into());
        self
    }

    /// The `PERL5LIB` value: [`lib`](Self::lib) joined with the platform path
    /// separator (`:` on Unix, `;` on Windows). Empty when `lib` is empty.
    pub fn perl5lib(&self) -> OsString {
        let separator: &str = if cfg!(windows) { ";" } else { ":" };
        let mut value = OsString::new();
        for (i, dir) in self.lib.iter().enumerate() {
            if i > 0 {
                value.push(separator);
            }
            value.push(dir);
        }
        value
    }

    /// Apply the CPAN build environment to `cmd`:
    ///
    /// * `PERL` is set to [`perl`](Self::perl).
    /// * `MAKE` is set to [`make`](Self::make), or unset when it is `None`.
    /// * `PERL5LIB` is set from [`lib`](Self::lib), or unset when `lib` is empty.
    /// * `PERLLIB` is always unset (so it cannot shadow `PERL5LIB`).
    /// * `PERL_LOCAL_LIB_ROOT`, `PERL_MB_OPT` and `PERL_MM_OPT` are set from
    ///   [`install_base`](Self::install_base) in the same way `local::lib` does,
    ///   or unset when it is `None`.
    fn apply_env(&self, cmd: &mut Command) {
        cmd.env("PERL", &self.perl);

        match self.make.as_deref() {
            Some(make) => {
                cmd.env("MAKE", make);
            }
            None => {
                cmd.env_remove("MAKE");
            }
        }

        if self.lib.is_empty() {
            cmd.env_remove("PERL5LIB");
        } else {
            cmd.env("PERL5LIB", self.perl5lib());
        }

        cmd.env_remove("PERLLIB");

        match self.install_base.as_deref() {
            Some(base) => {
                cmd.env("PERL_LOCAL_LIB_ROOT", base);

                let mut mb_opt = OsString::from("--install_base ");
                mb_opt.push(base);
                cmd.env("PERL_MB_OPT", mb_opt);

                let mut mm_opt = OsString::from("INSTALL_BASE=");
                mm_opt.push(base);
                cmd.env("PERL_MM_OPT", mm_opt);
            }
            None => {
                cmd.env_remove("PERL_LOCAL_LIB_ROOT");
                cmd.env_remove("PERL_MB_OPT");
                cmd.env_remove("PERL_MM_OPT");
            }
        }
    }

    /// A [`Command`] for [`perl`](Self::perl) with the CPAN build environment
    /// applied (the same one [`execute_perl`](Self::execute_perl) sets up) and no
    /// arguments set. Use it when you need to customise the command (working
    /// directory, captured output, extra environment) before running it.
    pub fn perl_command(&self) -> Command {
        let mut cmd = Command::new(&self.perl);
        self.apply_env(&mut cmd);
        if let Some(dir) = &self.current_dir {
            cmd.current_dir(dir);
        }
        cmd
    }

    /// A [`Command`] for [`make`](Self::make) with the CPAN build environment
    /// applied and no arguments set. Errors when there is no `make` available.
    pub fn make_command(&self) -> Result<Command> {
        let make = self
            .make
            .as_deref()
            .ok_or_else(|| anyhow!("no `make` executable is available"))?;
        let mut cmd = Command::new(make);
        self.apply_env(&mut cmd);
        if let Some(dir) = &self.current_dir {
            cmd.current_dir(dir);
        }
        Ok(cmd)
    }

    /// Run [`perl`](Self::perl) with `args`, after adjusting the environment:
    ///
    /// * `PERL` is set to [`perl`](Self::perl).
    /// * `MAKE` is set to [`make`](Self::make), or unset when it is `None`.
    /// * `PERL5LIB` is set from [`lib`](Self::lib), or unset when `lib` is empty.
    /// * `PERLLIB` is always unset (so it cannot shadow `PERL5LIB`).
    /// * `PERL_LOCAL_LIB_ROOT`, `PERL_MB_OPT` and `PERL_MM_OPT` are set from
    ///   [`install_base`](Self::install_base) the same way `local::lib` does, or
    ///   unset when it is `None`.
    ///
    /// The child inherits this process's stdio, unless
    /// [`capture_output`](Self::capture_output) is set, in which case its merged
    /// stdout+stderr is collected into
    /// [`ExecuteResult::output`](ExecuteResult::output).
    ///
    /// A non-zero exit is reported in the returned [`ExecuteResult`], not as an
    /// error; `Err` is only returned when the process cannot be spawned.
    pub fn execute_perl<I, S>(&self, args: I) -> Result<ExecuteResult>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = self.perl_command();
        cmd.args(args);
        self.run(cmd, &self.perl)
    }

    /// Run [`make`](Self::make) with `args`, after applying the CPAN build
    /// environment. Output is inherited or captured exactly as for
    /// [`execute_perl`](Self::execute_perl).
    ///
    /// Errors when there is no `make` available or the process cannot be
    /// spawned; a non-zero exit is reported in the returned [`ExecuteResult`].
    pub fn execute_make<I, S>(&self, args: I) -> Result<ExecuteResult>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = self.make_command()?;
        cmd.args(args);
        let program = self.make.clone().unwrap_or_else(|| PathBuf::from("make"));
        self.run(cmd, &program)
    }

    /// Spawn `cmd`, honouring [`capture_output`](Self::capture_output).
    /// `program` is only used to describe the executable in error messages.
    fn run(&self, mut cmd: Command, program: &Path) -> Result<ExecuteResult> {
        let context = || format!("failed to run `{}`", program.display());

        if !self.capture_output {
            let status = cmd.status().with_context(context)?;
            return Ok(ExecuteResult::new(status, None));
        }

        // Point both stdout and stderr at the write end of a single pipe so the
        // captured buffer preserves the order the child wrote its lines in.
        let (mut reader, writer) = os_pipe::pipe().context("failed to create an output pipe")?;
        let writer_clone = writer
            .try_clone()
            .context("failed to set up output capture")?;
        cmd.stdout(writer);
        cmd.stderr(writer_clone);

        let mut child = cmd.spawn().with_context(context)?;
        // Drop this process's copies of the pipe's write end, so that reading
        // reaches EOF once the child exits.
        drop(cmd);

        let mut output = Vec::new();
        reader
            .read_to_end(&mut output)
            .context("failed to read captured output")?;
        let status = child
            .wait()
            .context("failed to wait for the child process")?;

        Ok(ExecuteResult::new(status, Some(output)))
    }
}

/// The outcome of running `perl` or `make` via [`Perl::execute_perl`] /
/// [`Perl::execute_make`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExecuteResult {
    /// The process exit status.
    pub status: ExitStatus,

    /// `true` when the process exited with status zero.
    pub is_success: bool,

    /// The exit code, or `None` when the process was terminated by a signal.
    pub code: Option<i32>,

    /// The child's merged stdout+stderr, captured only when the [`Perl`] wrapper
    /// had [`capture_output`](Perl::capture_output) set; `None` when the child
    /// inherited this process's streams.
    pub output: Option<Vec<u8>>,
}

impl ExecuteResult {
    fn new(status: ExitStatus, output: Option<Vec<u8>>) -> Self {
        Self {
            status,
            is_success: status.success(),
            code: status.code(),
            output,
        }
    }

    /// The captured [`output`](Self::output) decoded as UTF-8, with invalid
    /// sequences replaced. `None` when output was not captured.
    pub fn output_lossy(&self) -> Option<Cow<'_, str>> {
        self.output.as_deref().map(String::from_utf8_lossy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn have_perl() -> bool {
        which::which("perl").is_ok()
    }

    #[test]
    fn new_resolves_perl_from_path() {
        let Ok(perl) = Perl::new() else {
            eprintln!("skipping: no `perl` on PATH");
            return;
        };
        assert!(perl.perl.is_absolute());
        assert!(perl.perl.file_stem().is_some());
        assert!(perl.install_base.is_none());
        assert!(perl.lib.is_empty());
        // `make` mirrors whatever is (or isn't) on PATH.
        assert_eq!(perl.make.is_some(), which::which("make").is_ok());
    }

    #[test]
    fn with_perl_uses_the_given_interpreter() {
        let perl = Perl::with_perl("/usr/local/bin/perl");
        assert_eq!(perl.perl, Path::new("/usr/local/bin/perl"));
        assert!(perl.install_base.is_none());
        assert_eq!(perl.make.is_some(), which::which("make").is_ok());
    }

    #[test]
    fn with_install_base_sets_the_location() {
        let perl = Perl::with_perl("/usr/bin/perl").with_install_base("/opt/perl5");
        assert_eq!(perl.install_base.as_deref(), Some(Path::new("/opt/perl5")));
    }

    #[test]
    fn with_make_overrides_resolution() {
        let perl = Perl::with_perl("/usr/bin/perl").with_make("/usr/bin/gmake");
        assert_eq!(perl.make.as_deref(), Some(Path::new("/usr/bin/gmake")));
    }

    #[test]
    fn with_lib_accepts_various_path_types() {
        let perl = Perl::with_perl("/usr/bin/perl")
            .with_lib(["/opt/perl5/lib/perl5", "/home/me/perl5/lib/perl5"]);
        assert_eq!(
            perl.lib,
            vec![
                PathBuf::from("/opt/perl5/lib/perl5"),
                PathBuf::from("/home/me/perl5/lib/perl5"),
            ]
        );
    }

    #[test]
    #[cfg(unix)]
    fn perl5lib_joins_with_colon() {
        let perl = Perl::with_perl("/usr/bin/perl").with_lib(["/a/lib", "/b/c/lib"]);
        assert_eq!(perl.perl5lib(), OsString::from("/a/lib:/b/c/lib"));
    }

    #[test]
    fn perl5lib_is_empty_without_lib_dirs() {
        let perl = Perl::with_perl("/usr/bin/perl");
        assert_eq!(perl.perl5lib(), OsString::new());
    }

    fn env_of(cmd: &Command) -> HashMap<&OsStr, Option<&OsStr>> {
        cmd.get_envs().collect()
    }

    #[test]
    fn command_sets_env_from_lib_and_install_base() {
        let perl = Perl::with_perl("/usr/bin/perl")
            .with_make("/usr/bin/make")
            .with_lib(["/a/lib", "/b/lib"])
            .with_install_base("/opt/pl");
        let cmd = perl.perl_command();
        let env = env_of(&cmd);

        assert_eq!(env[OsStr::new("PERL")], Some(OsStr::new("/usr/bin/perl")));
        assert_eq!(env[OsStr::new("MAKE")], Some(OsStr::new("/usr/bin/make")));
        assert_eq!(
            env[OsStr::new("PERL5LIB")],
            Some(OsStr::new("/a/lib:/b/lib"))
        );
        assert_eq!(env[OsStr::new("PERLLIB")], None);
        assert_eq!(
            env[OsStr::new("PERL_LOCAL_LIB_ROOT")],
            Some(OsStr::new("/opt/pl"))
        );
        assert_eq!(
            env[OsStr::new("PERL_MB_OPT")],
            Some(OsStr::new("--install_base /opt/pl"))
        );
        assert_eq!(
            env[OsStr::new("PERL_MM_OPT")],
            Some(OsStr::new("INSTALL_BASE=/opt/pl"))
        );
    }

    #[test]
    fn command_unsets_env_when_nothing_configured() {
        let perl = Perl {
            perl: PathBuf::from("/usr/bin/perl"),
            make: None,
            install_base: None,
            lib: Vec::new(),
            capture_output: false,
            current_dir: None,
        };
        let cmd = perl.perl_command();
        let env = env_of(&cmd);

        // `PERL` is always set; everything else is cleared.
        assert_eq!(env[OsStr::new("PERL")], Some(OsStr::new("/usr/bin/perl")));
        for key in [
            "MAKE",
            "PERL5LIB",
            "PERLLIB",
            "PERL_LOCAL_LIB_ROOT",
            "PERL_MB_OPT",
            "PERL_MM_OPT",
        ] {
            assert_eq!(env[OsStr::new(key)], None, "{key} should be unset");
        }
    }

    #[test]
    fn make_command_errors_without_make() {
        let perl = Perl {
            perl: PathBuf::from("/usr/bin/perl"),
            make: None,
            install_base: None,
            lib: Vec::new(),
            capture_output: false,
            current_dir: None,
        };
        assert!(perl.make_command().is_err());
        assert!(
            perl.execute_make(["all"])
                .unwrap_err()
                .to_string()
                .contains("make")
        );
    }

    #[test]
    fn capture_output_is_off_by_default() {
        let perl = Perl::with_perl("/usr/bin/perl");
        assert!(!perl.capture_output);
        assert!(perl.with_capture_output(true).capture_output);
    }

    #[test]
    fn with_current_dir_sets_the_command_working_directory() {
        let plain = Perl::with_perl("/usr/bin/perl").perl_command();
        assert_eq!(plain.get_current_dir(), None);

        let perl = Perl::with_perl("/usr/bin/perl").with_current_dir("/tmp/build");
        assert_eq!(
            perl.perl_command().get_current_dir(),
            Some(Path::new("/tmp/build"))
        );
    }

    #[test]
    fn execute_perl_reports_success_and_exit_code() {
        if !have_perl() {
            eprintln!("skipping: no `perl` on PATH");
            return;
        }
        let perl = Perl::new().unwrap();

        let ok = perl.execute_perl(["-e", "exit 0"]).unwrap();
        assert!(ok.is_success);
        assert_eq!(ok.code, Some(0));
        assert!(ok.output.is_none(), "output not captured by default");

        let bad = perl.execute_perl(["-e", "exit 3"]).unwrap();
        assert!(!bad.is_success);
        assert_eq!(bad.code, Some(3));
    }

    #[test]
    fn execute_perl_captures_merged_output_when_enabled() {
        if !have_perl() {
            eprintln!("skipping: no `perl` on PATH");
            return;
        }
        let perl = Perl::new().unwrap().with_capture_output(true);
        let result = perl
            .execute_perl([
                "-e",
                r#"$| = 1; print "to stdout\n"; print STDERR "to stderr\n"; exit 7"#,
            ])
            .unwrap();

        assert!(!result.is_success);
        assert_eq!(result.code, Some(7));
        let output = result.output_lossy().expect("output captured");
        assert!(output.contains("to stdout"), "{output:?}");
        assert!(output.contains("to stderr"), "{output:?}");
    }
}
