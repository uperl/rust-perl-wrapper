//! A configured Perl interpreter: where `perl` and `make` live, where newly
//! built modules are installed, and which directories go on `PERL5LIB`.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::OnceLock;

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

    /// Memoised `@INC` and `$Config{archname}` for this interpreter, filled in
    /// the first time [`inc`](Self::inc) or [`archname`](Self::archname) is
    /// called. Cloning a [`Perl`] carries over whatever the cache already holds;
    /// [`with_lib`](Self::with_lib) and [`with_current_dir`](Self::with_current_dir)
    /// clear it, since they change what the interpreter would report.
    interpreter: OnceLock<InterpreterConfig>,
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
            interpreter: OnceLock::new(),
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
        self.interpreter = OnceLock::new();
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
        self.interpreter = OnceLock::new();
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

    /// Look up an installed Perl module by name (for example `"Foo::Bar"`).
    ///
    /// The module's `.pm` file is searched for on the interpreter's module
    /// search path: [`lib`](Self::lib) first, then the `local::lib`-style
    /// directories under [`install_base`](Self::install_base), then the
    /// interpreter's own `@INC`. `@INC` is obtained by asking the interpreter to
    /// print it (`perl -e 'print for @INC'`); if that cannot be run only `lib`
    /// and `install_base` are searched.
    ///
    /// Returns `None` when no matching file is found (the module is not
    /// installed) or when `name` is not a syntactically valid module name.
    /// Otherwise the returned [`Module`] carries the requested `name`, the full
    /// path to the file, and the version declared in its source, if any.
    ///
    /// The module is **not** loaded, compiled, or executed. The version is
    /// recovered by scanning the source text for a `$VERSION` assignment or a
    /// `package NAME VERSION` statement (outside POD and `__END__`/`__DATA__`),
    /// the same shapes [`ExtUtils::MakeMaker`] recognises; unusual computed
    /// versions may therefore come back as `None`.
    ///
    /// [`ExtUtils::MakeMaker`]: https://metacpan.org/pod/ExtUtils::MakeMaker
    pub fn module(&self, name: &str) -> Option<Module> {
        let relative = module_relative_path(name)?;

        for dir in self.module_search_path() {
            let candidate = dir.join(&relative);
            if !candidate.is_file() {
                continue;
            }
            let path = candidate.canonicalize().unwrap_or(candidate);
            let version = fs::read(&path)
                .ok()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .and_then(|source| parse_version(&source));
            return Some(Module {
                name: name.to_string(),
                path,
                version,
            });
        }

        None
    }

    /// The interpreter's effective `@INC`: the directories `perl` searches for
    /// modules, in order, with this wrapper's [`lib`](Self::lib) entries already
    /// prepended (they are passed to the child through `PERL5LIB`).
    ///
    /// The list is read once, by running a short script through
    /// [`execute_perl`](Self::execute_perl) (`print for @INC`), then cached on
    /// this wrapper; later calls return the cached slice without spawning a
    /// process. `@INC` entries that are not filesystem paths — the code
    /// references a `use lib` hook installs, which stringify with a `(0x…)`
    /// address — are filtered out. An empty slice is returned (and cached) when
    /// the interpreter cannot be run.
    ///
    /// [`install_base`](Self::install_base) is deliberately *not* reflected
    /// here (it is never placed on `PERL5LIB`), even though
    /// [`module`](Self::module) also searches it.
    pub fn inc(&self) -> &[PathBuf] {
        &self.interpreter_config().inc
    }

    /// The interpreter's `$Config{archname}` (for example
    /// `x86_64-linux-gnu-thread-multi`), or `None` when it cannot be run.
    ///
    /// Read once via [`execute_perl`](Self::execute_perl) — in the same `perl`
    /// invocation as [`inc`](Self::inc) — and cached on this wrapper.
    pub fn archname(&self) -> Option<&str> {
        self.interpreter_config().archname.as_deref()
    }

    /// The cached [`InterpreterConfig`], computed on first use.
    fn interpreter_config(&self) -> &InterpreterConfig {
        self.interpreter
            .get_or_init(|| self.query_interpreter_config())
    }

    /// Run a short script through [`execute_perl`](Self::execute_perl) to read
    /// `@INC` and `$Config{archname}`, and parse its captured output. Any
    /// failure (cannot spawn, non-zero exit, no output) yields an empty
    /// [`InterpreterConfig`].
    fn query_interpreter_config(&self) -> InterpreterConfig {
        const SCRIPT: &str = "use Config (); \
             print \"archname\\t$Config::Config{archname}\\n\"; \
             print \"inc\\t$_\\n\" for @INC;";

        // `execute_perl` only captures output when `capture_output` is set, so
        // run the query through a copy that always captures.
        let perl = self.clone().with_capture_output(true);
        let Ok(result) = perl.execute_perl(["-e", SCRIPT]) else {
            return InterpreterConfig::default();
        };
        if !result.is_success {
            return InterpreterConfig::default();
        }
        result
            .output_lossy()
            .map(|output| InterpreterConfig::parse(&output))
            .unwrap_or_default()
    }

    /// The directories to search for a module's `.pm` file, most significant
    /// first: [`lib`](Self::lib), then the `local::lib` layout under
    /// [`install_base`](Self::install_base), then the interpreter's
    /// [`inc`](Self::inc).
    fn module_search_path(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();

        for dir in &self.lib {
            push_unique(&mut dirs, dir.clone());
        }

        if let Some(base) = &self.install_base {
            let perl5 = base.join("lib").join("perl5");
            if let Some(arch) = self.archname() {
                push_unique(&mut dirs, perl5.join(arch));
            }
            push_unique(&mut dirs, perl5);
        }

        for dir in self.inc() {
            push_unique(&mut dirs, dir.clone());
        }

        dirs
    }
}

/// An installed Perl module located on disk by [`Perl::module`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Module {
    /// The module name as requested, for example `Foo::Bar`.
    pub name: String,

    /// Full path to the module's `.pm` file.
    pub path: PathBuf,

    /// The version declared in the module source, or `None` when the module
    /// declares no version (or it could not be recovered without executing the
    /// module).
    pub version: Option<String>,
}

/// The interpreter facts cached on a [`Perl`] by [`Perl::inc`] and
/// [`Perl::archname`]. An all-default value means "the interpreter could not be
/// queried".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct InterpreterConfig {
    /// `$Config{archname}`, or `None` when it was not reported.
    archname: Option<String>,

    /// Effective `@INC`, filtered to filesystem paths.
    inc: Vec<PathBuf>,
}

impl InterpreterConfig {
    /// Parse the `archname\t…` / `inc\t…` lines the query script prints.
    fn parse(output: &str) -> Self {
        let mut archname = None;
        let mut inc = Vec::new();
        for line in output.lines() {
            if let Some(value) = line.strip_prefix("archname\t") {
                let value = value.trim();
                if !value.is_empty() {
                    archname = Some(value.to_string());
                }
            } else if let Some(value) = line.strip_prefix("inc\t")
                && is_directory_entry(value)
            {
                inc.push(PathBuf::from(value));
            }
        }
        Self { archname, inc }
    }
}

/// Whether an `@INC` entry names a filesystem path rather than a `use lib` hook
/// (a coderef or blessed object, which stringifies with a `(0x…)` address).
fn is_directory_entry(entry: &str) -> bool {
    !entry.is_empty() && !entry.contains("(0x")
}

/// Turn a module name (`Foo::Bar`) into its relative file path (`Foo/Bar.pm`),
/// or `None` when the name is not a valid `::`-separated identifier.
fn module_relative_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }

    let mut path = PathBuf::new();
    for segment in name.split("::") {
        let mut chars = segment.chars();
        let first = chars.next()?;
        if !(first.is_ascii_alphabetic() || first == '_') {
            return None;
        }
        if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return None;
        }
        path.push(segment);
    }

    path.set_extension("pm");
    Some(path)
}

/// Push `dir` onto `dirs` unless it is empty or already present.
fn push_unique(dirs: &mut Vec<PathBuf>, dir: PathBuf) {
    if !dir.as_os_str().is_empty() && !dirs.iter().any(|d| d == &dir) {
        dirs.push(dir);
    }
}

/// Recover a module's version from its source text without executing it.
///
/// Lines inside POD and anything after `__END__` / `__DATA__` are ignored. The
/// first `package NAME VERSION` statement or `$VERSION` (or `*VERSION`)
/// assignment wins; the right-hand side is read as a literal rather than
/// evaluated.
fn parse_version(source: &str) -> Option<String> {
    let mut in_pod = false;

    for raw in source.lines() {
        if in_pod {
            if raw.starts_with("=cut") {
                in_pod = false;
            }
            continue;
        }
        if let Some(rest) = raw.strip_prefix('=')
            && rest.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        {
            in_pod = true;
            continue;
        }

        let end = raw.trim_end();
        if end == "__END__" || end == "__DATA__" {
            break;
        }

        let code = raw.trim_start();
        if code.starts_with('#') {
            continue;
        }

        if let Some(version) = parse_package_statement_version(code) {
            return Some(version);
        }
        if let Some(version) = parse_version_assignment(raw) {
            return Some(version);
        }
    }

    None
}

/// Parse the version from a `package NAME VERSION ...` statement, if `line` is
/// one that carries a version.
fn parse_package_statement_version(line: &str) -> Option<String> {
    let rest = line.strip_prefix("package")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();

    let name_end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':' || c == '\''))
        .unwrap_or(rest.len());
    if name_end == 0 {
        return None;
    }

    take_numeric_version(rest[name_end..].trim_start())
}

/// Parse the version from a `$VERSION = ...;` (or `*VERSION = ...;`) assignment.
fn parse_version_assignment(line: &str) -> Option<String> {
    let mut from = 0;
    while let Some(rel) = line[from..].find("VERSION") {
        let start = from + rel;
        let after = start + "VERSION".len();
        from = after;

        // `VERSION` must be a complete word.
        if line[after..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            continue;
        }

        // Immediately to the left: an optional `Pkg::` qualifier, then a
        // `$` or `*` sigil.
        let prefix = &line[..start];
        let qualifier_start = prefix
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_' || *c == ':' || *c == '\'')
            .last()
            .map(|(i, _)| i)
            .unwrap_or(prefix.len());
        match prefix[..qualifier_start].chars().next_back() {
            Some('$') | Some('*') => {}
            _ => continue,
        }

        let Some(eq) = find_assignment(&line[after..]) else {
            continue;
        };
        if let Some(version) = extract_version_value(&line[after + eq + 1..]) {
            return Some(version);
        }
    }

    None
}

/// Index of the first `=` in `s` that is a plain assignment (not `==`, `=~` or
/// `=>`). Returns `None` if a `;` or `#` is reached first.
fn find_assignment(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'=' => match bytes.get(i + 1) {
                Some(b'=') | Some(b'~') | Some(b'>') => i += 2,
                _ => return Some(i),
            },
            b';' | b'#' => return None,
            _ => i += 1,
        }
    }
    None
}

/// Read a version literal from the right-hand side of an assignment: a quoted
/// string, a bare numeric/`v`-string literal, or the first quoted string inside
/// a call such as `qv('1.2.3')` or `version->declare('v1.2.3')`.
fn extract_version_value(rhs: &str) -> Option<String> {
    let rhs = rhs.trim_start();
    match rhs.chars().next()? {
        quote @ ('\'' | '"') => {
            let inner = &rhs[1..];
            let close = inner.find(quote)?;
            clean_version_string(&inner[..close])
        }
        c if c.is_ascii_digit() => clean_version_string(take_number(rhs)),
        'v' | 'V' if rhs[1..].starts_with(|c: char| c.is_ascii_digit()) => {
            clean_version_string(take_number(rhs))
        }
        _ => {
            let start = rhs.find(['\'', '"'])?;
            let quote = rhs.as_bytes()[start] as char;
            let inner = &rhs[start + 1..];
            let close = inner.find(quote)?;
            clean_version_string(&inner[..close])
        }
    }
}

/// A leading `package` version literal (`1.23`, `v1.2.3`) followed by end of
/// statement (`;` or `{`), or `None` when there is no such literal.
fn take_numeric_version(s: &str) -> Option<String> {
    let literal = take_number(s);
    if literal.is_empty() || literal == "v" || literal == "V" {
        return None;
    }
    let tail = s[literal.len()..].trim_start();
    if !(tail.is_empty() || tail.starts_with(';') || tail.starts_with('{')) {
        return None;
    }
    clean_version_string(literal)
}

/// Take a leading `[vV]?[0-9._]*` run from `s`.
fn take_number(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut i = 0;
    if matches!(bytes.first(), Some(b'v') | Some(b'V')) {
        i += 1;
    }
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.' || bytes[i] == b'_') {
        i += 1;
    }
    &s[..i]
}

/// Normalise an extracted version literal, or reject it if it does not look like
/// a version (no digits, or references a variable / format).
fn clean_version_string(value: &str) -> Option<String> {
    let value = value.trim().trim_end_matches(['.', '_']);
    if value.is_empty()
        || value.contains(['%', '$', '@'])
        || !value.chars().any(|c| c.is_ascii_digit())
    {
        return None;
    }
    Some(value.to_string())
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
            interpreter: OnceLock::new(),
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
            interpreter: OnceLock::new(),
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

    fn scratch_dir(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("perl-wrapper-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_module(root: &Path, name: &str, body: &str) -> PathBuf {
        let rel = module_relative_path(name).unwrap();
        let path = root.join(&rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn module_relative_path_maps_and_validates_names() {
        assert_eq!(
            module_relative_path("Foo::Bar"),
            Some(PathBuf::from("Foo/Bar.pm"))
        );
        assert_eq!(module_relative_path("Foo"), Some(PathBuf::from("Foo.pm")));
        assert_eq!(
            module_relative_path("Foo::Bar::Baz"),
            Some(PathBuf::from("Foo/Bar/Baz.pm"))
        );

        for bad in [
            "",
            "Foo::",
            "::Foo",
            "Foo:::Bar",
            "Foo::1Bar",
            "Foo Bar",
            "Foo::Bar-Baz",
        ] {
            assert_eq!(
                module_relative_path(bad),
                None,
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn parse_version_recognises_common_shapes() {
        let cases = [
            ("our $VERSION = '1.23';", Some("1.23")),
            ("$VERSION = \"0.001\";", Some("0.001")),
            ("our $VERSION = '0.001_02'; # VERSION", Some("0.001_02")),
            ("$Foo::Bar::VERSION = '4.5';", Some("4.5")),
            ("  our  $VERSION  =  1.5 ;", Some("1.5")),
            (
                "use version; our $VERSION = version->declare('v1.2.3');",
                Some("v1.2.3"),
            ),
            ("our $VERSION = qv('2.0.1');", Some("2.0.1")),
            ("package Foo::Bar 1.802;", Some("1.802")),
            ("package Foo::Bar v2.3.4 {", Some("v2.3.4")),
            ("package Foo::Bar;", None),
            ("if ($VERSION == 1) { }", None),
            ("$VERSION =~ s/_//g;", None),
            ("# our $VERSION = '9.9';", None),
            ("sub VERSION { return 3 }", None),
        ];
        for (src, want) in cases {
            assert_eq!(
                parse_version(src).as_deref(),
                want,
                "parse_version({src:?})"
            );
        }
    }

    #[test]
    fn parse_version_skips_pod_and_data_sections() {
        let src = "package Foo;\n\
                   =pod\n\
                   our $VERSION = '9.99';\n\
                   =cut\n\
                   our $VERSION = '1.00';\n\
                   __END__\n\
                   our $VERSION = '2.00';\n";
        assert_eq!(parse_version(src).as_deref(), Some("1.00"));
    }

    #[test]
    fn parse_version_none_when_absent() {
        let src = "package Foo::Bar;\nuse strict;\nsub new { bless {}, shift }\n1;\n";
        assert_eq!(parse_version(src), None);
    }

    #[test]
    fn module_finds_file_and_version_via_lib() {
        let root = scratch_dir("lib");
        let path = write_module(
            &root,
            "My::Thing",
            "package My::Thing;\nour $VERSION = '3.14';\n1;\n",
        );

        // An interpreter path that cannot run, so only `lib` / `install_base`
        // are searched.
        let perl = Perl::with_perl("/nonexistent/perl").with_lib([root.clone()]);

        let found = perl.module("My::Thing").expect("module found");
        assert_eq!(found.name, "My::Thing");
        assert_eq!(found.version.as_deref(), Some("3.14"));
        assert_eq!(found.path, path.canonicalize().unwrap());

        assert!(perl.module("My::Missing").is_none());
        assert!(perl.module("not a module name").is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn module_reports_none_version_when_source_declares_none() {
        let root = scratch_dir("nover");
        write_module(
            &root,
            "My::Plain",
            "package My::Plain;\nsub new { bless {}, shift }\n1;\n",
        );
        let perl = Perl::with_perl("/nonexistent/perl").with_lib([root.clone()]);

        let found = perl.module("My::Plain").expect("module found");
        assert_eq!(found.version, None);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn module_searches_install_base_layout() {
        let root = scratch_dir("base");
        let lib = root.join("lib").join("perl5");
        fs::create_dir_all(lib.join("My")).unwrap();
        fs::write(
            lib.join("My").join("Installed.pm"),
            "package My::Installed;\nour $VERSION = '0.02';\n1;\n",
        )
        .unwrap();

        let perl = Perl::with_perl("/nonexistent/perl").with_install_base(root.clone());
        let found = perl.module("My::Installed").expect("module found");
        assert_eq!(found.version.as_deref(), Some("0.02"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn module_finds_core_module_through_real_inc() {
        if !have_perl() {
            eprintln!("skipping: no `perl` on PATH");
            return;
        }
        let perl = Perl::new().unwrap();

        let strict = perl.module("strict").expect("strict is always installed");
        assert_eq!(strict.name, "strict");
        assert!(strict.path.is_absolute());
        assert!(strict.path.ends_with("strict.pm"), "{:?}", strict.path);
        assert!(strict.path.is_file());
        // `strict.pm` has declared a `$VERSION` for many years.
        assert!(strict.version.is_some(), "{:?}", strict.version);

        assert!(perl.module("No::Such::Module::From::This::Test").is_none());
    }

    #[test]
    fn is_directory_entry_rejects_use_lib_hooks() {
        assert!(is_directory_entry("/usr/lib/perl5"));
        assert!(is_directory_entry("."));
        assert!(!is_directory_entry(""));
        assert!(!is_directory_entry("CODE(0x55d3e2a1b2c0)"));
        assert!(!is_directory_entry("My::Hook=HASH(0x561f00)"));
    }

    #[test]
    fn interpreter_config_parses_query_output() {
        let output = "archname\tx86_64-linux\n\
                      inc\t/opt/perl/lib/site_perl\n\
                      inc\tCODE(0x559abc)\n\
                      inc\t/opt/perl/lib\n";
        let config = InterpreterConfig::parse(output);
        assert_eq!(config.archname.as_deref(), Some("x86_64-linux"));
        assert_eq!(
            config.inc,
            vec![
                PathBuf::from("/opt/perl/lib/site_perl"),
                PathBuf::from("/opt/perl/lib"),
            ]
        );
    }

    #[test]
    fn inc_and_archname_empty_without_a_working_interpreter() {
        let perl = Perl::with_perl("/nonexistent/perl");
        assert!(perl.inc().is_empty());
        assert_eq!(perl.archname(), None);
    }

    #[test]
    fn inc_is_cached_and_reflects_lib() {
        if !have_perl() {
            eprintln!("skipping: no `perl` on PATH");
            return;
        }
        let root = scratch_dir("inc");
        let perl = Perl::new().unwrap().with_lib([root.clone()]);

        let first = perl.inc().to_vec();
        assert!(!first.is_empty());
        // `lib` is threaded through `PERL5LIB`, so it lands in the effective @INC.
        assert!(
            first.iter().any(|dir| dir == &root),
            "{root:?} not in {first:?}"
        );
        // A second call returns the same cached data.
        assert_eq!(perl.inc(), first.as_slice());

        // `archname` is reported and non-empty (its exact spelling and how it
        // relates to the @INC layout varies by build, e.g. Debian multiarch).
        assert!(!perl.archname().expect("archname reported").is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn with_lib_clears_a_populated_interpreter_cache() {
        if !have_perl() {
            eprintln!("skipping: no `perl` on PATH");
            return;
        }
        let root = scratch_dir("recache");
        let perl = Perl::new().unwrap();
        assert!(!perl.inc().iter().any(|dir| dir == &root));

        let perl = perl.with_lib([root.clone()]);
        assert!(
            perl.inc().iter().any(|dir| dir == &root),
            "cache was not refreshed after with_lib"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
