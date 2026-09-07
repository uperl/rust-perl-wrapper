# perl-wrapper

A small wrapper around a Perl interpreter and the environment used to build and
install CPAN distributions with it.

`Perl` records the full path to `perl`, the full path to `make` (or `None`), an
optional module install location, and a list of directories for `PERL5LIB`. Its
`execute_perl` / `execute_make` methods run those executables with the build
environment applied (`PERL`, `MAKE`, `PERL5LIB`, `PERL_LOCAL_LIB_ROOT`,
`PERL_MB_OPT`, `PERL_MM_OPT`, and `PERLLIB` cleared), optionally capturing the
child's merged stdout/stderr, and return an `ExecuteResult` carrying the exit
status.

```rust
use perl_wrapper::Perl;

let perl = Perl::new()?
    .with_install_base("/opt/perl5")
    .with_lib(["/opt/perl5/lib/perl5"]);

let result = perl.execute_perl(["-e", "print qq{hello\\n}"])?;
assert!(result.is_success);
```

`module("Foo::Bar")` reports whether a module is installed without loading it.
It searches `lib`, the `local::lib` layout under `install_base`, and the
interpreter's `@INC`, returning `None` when nothing matches or `Some(Module)`
with the module name, the full path to its `.pm` file, and the version read from
the source (`None` when the module declares none). The module is never compiled
or run — the version is recovered by scanning the source for a `$VERSION`
assignment or `package NAME VERSION` statement.

```rust
use perl_wrapper::Perl;

let perl = Perl::new()?;
if let Some(m) = perl.module("Data::Dumper") {
    println!("{} {:?} at {}", m.name, m.version, m.path.display());
}
```

`inc()` and `archname()` return the interpreter's effective `@INC` (with `lib`
prepended, as `PERL5LIB` puts it there) and its `$Config{archname}`. Both run
`perl` once through `execute_perl`, parse the captured output, and cache the
result on the `Perl` value, so repeated calls are free.
