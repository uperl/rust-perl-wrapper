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
