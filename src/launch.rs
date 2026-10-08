//! Why a program could not be started, worked out from what is on disk.
//!
//! When `exec` fails, the operating system says very little: `ENOENT`, "no
//! such file or directory", is the same answer for a name that is on no
//! directory of `PATH`, for a full path written on somebody else's machine,
//! and for a script whose `#!` line names an interpreter that has since been
//! uninstalled. Reported as it stands — or worse, guessed at as "not on your
//! PATH" — it sends a person who can run `aws` perfectly well at their own
//! prompt to go and install it.
//!
//! The usual reason a program works at the prompt and not from `eks` is that
//! the shell finds it some way no other program can: an alias or a function,
//! or a `PATH` entry written with a literal `~`, which bash expands while it
//! looks a command up and `execvp` never does. So on failure this module
//! repeats the lookup by hand and says which of those it was.
//!
//! [`diagnose`] is pure: it is handed the error, the `PATH`, the home
//! directory, and a function that says what is at a path, so every one of
//! those cases is a fixture. [`explain`] is the same thing with the real
//! environment and the real filesystem, and only runs after a start has
//! already failed — nothing here costs a successful start anything.

use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Where to get the AWS CLI, for the messages that say it is missing.
pub const AWS_INSTALL_URL: &str =
    "https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html";

/// What is at a path, as far as starting a program from it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// Nothing at all.
    Absent,
    /// A symbolic link to something that is no longer there — what a package
    /// manager leaves behind when it removes the thing a link pointed into.
    Dangling,
    /// A directory, which nobody can run.
    Directory,
    /// A file. `interpreter` is the program its `#!` line names, when it has
    /// one.
    File {
        executable: bool,
        interpreter: Option<String>,
    },
}

/// Who chose the program, which decides where the fix goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A kubeconfig's `exec` block named it, so `command:` can name another.
    Kubeconfig,
    /// `eks` runs it by name, so the only fix is the environment.
    Eks,
}

/// Why a program did not start, in the terms the fix is written in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotStarted {
    /// A bare name that is in none of the directories on `PATH`.
    NotOnPath { program: String, directories: usize },
    /// A bare name, and no `PATH` to look it up on — what a process started
    /// by something other than a login shell can be given.
    NoPath { program: String },
    /// A bare name that is in a directory `PATH` spells with a literal `~`.
    /// A shell expands that while it looks the name up, so the program works
    /// at its prompt; nothing else does.
    Tilde { program: String, entry: String },
    /// A full path, and nothing at it.
    NoFile { path: String },
    /// A link where the program should be, pointing at nothing.
    Dangling { path: String },
    /// A script whose `#!` line names an interpreter that is not there.
    NoInterpreter { path: String, interpreter: String },
    /// Something is there, but it may not be run.
    NotExecutable { path: String },
    /// None of the above: the operating system's own words.
    Other { program: String, reason: String },
}

impl NotStarted {
    /// The program, by the name the remedy should use for it: `aws`, not
    /// `/usr/local/bin/aws`, because `command -v aws` is what finds the right
    /// one.
    fn name(&self) -> &str {
        let program = match self {
            Self::NotOnPath { program, .. }
            | Self::NoPath { program }
            | Self::Tilde { program, .. }
            | Self::Other { program, .. } => program,
            Self::NoFile { path }
            | Self::Dangling { path }
            | Self::NoInterpreter { path, .. }
            | Self::NotExecutable { path } => path,
        };
        program.rsplit('/').next().unwrap_or(program)
    }

    /// What to do about it. `origin` decides whether a kubeconfig's
    /// `command:` is one of the ways out.
    ///
    /// Each answer ends at an action, never at a guess: a person told "not on
    /// your PATH" when `aws` answers at their prompt has learnt nothing, but
    /// one told to run `type aws` is a step from the fix.
    #[must_use]
    pub fn remedy(&self, origin: Origin) -> String {
        let name = self.name();
        let full_path = match origin {
            Origin::Kubeconfig => format!(
                "set `command:` in that context's `exec` block to the full path `command -v {name}` prints"
            ),
            Origin::Eks => {
                format!("put the directory `command -v {name}` prints on the PATH eks starts with")
            }
        };
        let install = self.install();

        match self {
            Self::NotOnPath { .. } => format!(
                "If `{name}` works in your shell, the shell is finding it some way other programs cannot: \
                 run `type {name}` there. An alias or a function cannot be run from outside the shell, so \
                 point at the program it calls; a PATH set only inside the shell needs exporting from \
                 your profile. Either way, {full_path}.{install}"
            ),
            Self::NoPath { .. } => format!(
                "Start eks from a shell, which gives it the PATH you have at the prompt, or {full_path}."
            ),
            Self::Tilde { entry, .. } => {
                let spelled = entry.replacen('~', "$HOME", 1);
                format!(
                    "Bash expands `~` there while it looks a command up, so `{name}` works at its prompt, \
                     but programs it starts do not. Where your PATH is set — a shell profile, usually — \
                     write `{spelled}` instead of `{entry}`."
                )
            }
            Self::NoFile { .. } => match origin {
                Origin::Kubeconfig => format!(
                    "The kubeconfig names that path, perhaps one written on another machine. \
                     Run `command -v {name}` and put what it prints in `command:` — or write \
                     plain `{name}` there, to look it up on PATH as kubectl would.{install}"
                ),
                Origin::Eks => {
                    format!("Run `command -v {name}` to see where it really is.{install}")
                }
            },
            Self::Dangling { .. } | Self::NoInterpreter { .. } => format!(
                "Something it was installed with has since been removed or upgraded away — a Homebrew \
                 Python, often. Reinstalling `{name}` puts it back.{install}"
            ),
            Self::NotExecutable { path } => {
                format!("If it is the program you meant, `chmod +x {path}`; otherwise {full_path}.")
            }
            // No program at all: an `exec` block with no `command:`.
            Self::Other { .. } if name.is_empty() => {
                "Give that context's `exec` block a `command:` to run.".to_owned()
            }
            Self::Other { .. } => format!(
                "Run `{name}` yourself from the same shell to see the whole of what the system says."
            ),
        }
    }

    /// Where to get the program, when it is one we know: a sentence to end
    /// a remedy with, or nothing.
    fn install(&self) -> String {
        if self.name() == "aws" {
            format!(" If it is not installed, install AWS CLI version 2: {AWS_INSTALL_URL}")
        } else {
            String::new()
        }
    }
}

impl fmt::Display for NotStarted {
    /// The fact, as a clause: "`aws` is not in any of the 6 directories on
    /// the PATH eks was started with". The remedy is [`NotStarted::remedy`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotOnPath {
                program,
                directories,
            } => {
                let directories = match directories {
                    1 => "the one directory".to_owned(),
                    n => format!("any of the {n} directories"),
                };
                write!(
                    f,
                    "`{program}` is not in {directories} on the PATH eks was started with"
                )
            }
            Self::NoPath { program } => write!(
                f,
                "eks was started with no PATH, so `{program}` cannot be looked up"
            ),
            Self::Tilde { program, entry } => write!(
                f,
                "`{program}` is in `{entry}`, but your PATH spells that directory with a literal `~`"
            ),
            Self::NoFile { path } => write!(f, "there is no file at `{path}`"),
            Self::Dangling { path } => {
                write!(f, "`{path}` is a link to something that is no longer there")
            }
            Self::NoInterpreter { path, interpreter } => write!(
                f,
                "`{path}` is a script run by `{interpreter}`, which is not there"
            ),
            Self::NotExecutable { path } => write!(f, "`{path}` is not executable"),
            Self::Other { program, reason } if program.is_empty() => f.write_str(reason),
            Self::Other { program, reason } => write!(f, "`{program}`: {reason}"),
        }
    }
}

/// Work out why `program` did not start.
///
/// `error` is what the start returned, `path` the `PATH` the start searched,
/// `home` the directory `~` would stand for, and `probe` says what is at a
/// path. The lookup follows `execvp`'s: a name with a `/` in it is used as it
/// is, a bare name is tried in each directory of `PATH` in order, and a file
/// found but not executable is passed over for a later one.
pub fn diagnose(
    program: &str,
    error: &io::Error,
    path: Option<&OsStr>,
    home: Option<&Path>,
    probe: impl Fn(&Path) -> Entry,
) -> NotStarted {
    let other = || NotStarted::Other {
        program: program.to_owned(),
        reason: error.to_string(),
    };
    if !matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
    ) {
        return other();
    }

    if program.contains('/') {
        return at(program, &probe).unwrap_or_else(other);
    }

    let directories: Vec<PathBuf> = path
        .map(|path| std::env::split_paths(path).collect())
        .unwrap_or_default();
    // POSIX reads an empty `PATH` entry as the working directory, but nobody
    // means that, and an unset or empty `PATH` is its own message.
    let directories: Vec<PathBuf> = directories
        .into_iter()
        .filter(|directory| !directory.as_os_str().is_empty())
        .collect();
    if directories.is_empty() {
        return NotStarted::NoPath {
            program: program.to_owned(),
        };
    }

    let mut refused = None;
    for directory in &directories {
        let candidate = directory.join(program);
        match probe(&candidate) {
            Entry::File {
                executable: true, ..
            } => {
                // The first runnable file is the one the lookup started, so
                // whatever went wrong went wrong with it.
                return at(&candidate.to_string_lossy(), &probe).unwrap_or_else(other);
            }
            Entry::File {
                executable: false, ..
            } => {
                refused.get_or_insert(candidate);
            }
            Entry::Absent | Entry::Dangling | Entry::Directory => {}
        }
    }

    // Only once the real lookup has come up empty: a `~` entry is worth a
    // sentence when it is the reason, and noise when it is not.
    if let Some(home) = home {
        for directory in &directories {
            let spelled = directory.to_string_lossy();
            let Some(rest) = spelled.strip_prefix('~') else {
                continue;
            };
            if !(rest.is_empty() || rest.starts_with('/')) {
                // `~alice/bin` is somebody else's home; not worth guessing at.
                continue;
            }
            let expanded = home.join(rest.trim_start_matches('/')).join(program);
            if let Entry::File {
                executable: true, ..
            } = probe(&expanded)
            {
                return NotStarted::Tilde {
                    program: program.to_owned(),
                    entry: spelled.into_owned(),
                };
            }
        }
    }

    if let Some(path) = refused {
        return NotStarted::NotExecutable {
            path: path.to_string_lossy().into_owned(),
        };
    }

    NotStarted::NotOnPath {
        program: program.to_owned(),
        directories: directories.len(),
    }
}

/// What is wrong with the file at `path`, if anything this module can name.
fn at(path: &str, probe: &impl Fn(&Path) -> Entry) -> Option<NotStarted> {
    let path_owned = || path.to_owned();
    match probe(Path::new(path)) {
        Entry::Absent => Some(NotStarted::NoFile { path: path_owned() }),
        Entry::Dangling => Some(NotStarted::Dangling { path: path_owned() }),
        Entry::File {
            executable: false, ..
        } => Some(NotStarted::NotExecutable { path: path_owned() }),
        Entry::File {
            executable: true,
            interpreter: Some(interpreter),
        } if interpreter.starts_with('/') && probe(Path::new(&interpreter)) == Entry::Absent => {
            Some(NotStarted::NoInterpreter {
                path: path_owned(),
                interpreter,
            })
        }
        // A directory is nobody's command on purpose, and the system's own
        // "permission denied" names it well enough.
        Entry::Directory | Entry::File { .. } => None,
    }
}

/// The interpreter a script's first line names: `/usr/bin/python3` from
/// `#!/usr/bin/python3 -E`. `None` for anything that is not a script.
#[must_use]
pub fn interpreter(head: &[u8]) -> Option<String> {
    let line = head.strip_prefix(b"#!")?;
    let line = line.split(|&byte| byte == b'\n').next().unwrap_or(line);
    let line = std::str::from_utf8(line).ok()?;
    line.split_whitespace().next().map(str::to_owned)
}

/// [`diagnose`], against this process's own environment and filesystem.
///
/// Called after a start has failed, never before one: the lookup it repeats
/// is a handful of `stat` calls, and only a failure needs them.
#[must_use]
pub fn explain(program: &str, error: &io::Error) -> NotStarted {
    let path = std::env::var_os("PATH");
    let home = std::env::var_os("HOME").map(PathBuf::from);
    diagnose(program, error, path.as_deref(), home.as_deref(), probe)
}

/// What is at `path` on this machine.
fn probe(path: &Path) -> Entry {
    let Ok(metadata) = std::fs::metadata(path) else {
        // `metadata` follows links; one that leads nowhere still has a
        // `symlink_metadata` of its own.
        return if std::fs::symlink_metadata(path).is_ok() {
            Entry::Dangling
        } else {
            Entry::Absent
        };
    };
    if metadata.is_dir() {
        return Entry::Directory;
    }

    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = true;

    let interpreter = if executable { head(path) } else { None };
    Entry::File {
        executable,
        interpreter,
    }
}

/// The interpreter named at the top of the file at `path`. Reads only the
/// first line's worth, since the file may be a hundred-megabyte binary.
fn head(path: &Path) -> Option<String> {
    let mut bytes = Vec::with_capacity(256);
    std::fs::File::open(path)
        .ok()?
        .take(256)
        .read_to_end(&mut bytes)
        .ok()?;
    interpreter(&bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::HashMap;

    use super::*;

    fn not_found() -> io::Error {
        io::Error::from(io::ErrorKind::NotFound)
    }

    fn runnable() -> Entry {
        Entry::File {
            executable: true,
            interpreter: None,
        }
    }

    fn script(interpreter: &str) -> Entry {
        Entry::File {
            executable: true,
            interpreter: Some(interpreter.to_owned()),
        }
    }

    /// A filesystem of exactly the entries listed; everything else absent.
    fn disk(entries: &[(&str, Entry)]) -> impl Fn(&Path) -> Entry {
        let entries: HashMap<PathBuf, Entry> = entries
            .iter()
            .map(|(path, entry)| (PathBuf::from(path), entry.clone()))
            .collect();
        move |path| entries.get(path).cloned().unwrap_or(Entry::Absent)
    }

    fn diagnose_on(program: &str, path: &str, entries: &[(&str, Entry)]) -> NotStarted {
        diagnose(
            program,
            &not_found(),
            Some(OsStr::new(path)),
            Some(Path::new("/home/sam")),
            disk(entries),
        )
    }

    // --- diagnose ----------------------------------------------------------

    #[test]
    fn a_name_on_no_directory_of_path_says_how_many_were_searched() {
        let why = diagnose_on("aws", "/usr/bin:/bin:/usr/sbin", &[]);

        assert_eq!(
            why,
            NotStarted::NotOnPath {
                program: "aws".to_owned(),
                directories: 3,
            }
        );
        assert_eq!(
            why.to_string(),
            "`aws` is not in any of the 3 directories on the PATH eks was started with"
        );
    }

    #[test]
    fn one_directory_on_path_is_counted_in_words() {
        let why = diagnose_on("aws", "/usr/bin", &[]);

        assert!(why.to_string().contains("the one directory"), "{why}");
    }

    #[test]
    fn a_path_entry_with_a_literal_tilde_is_named_when_the_program_is_behind_it() {
        // `export PATH="~/.local/bin:$PATH"` — the quotes stop the shell
        // expanding the `~` when PATH is set, and bash expands it again on
        // every lookup, so `aws` works at the prompt and nowhere else.
        let why = diagnose_on(
            "aws",
            "~/.local/bin:/usr/bin",
            &[("/home/sam/.local/bin/aws", runnable())],
        );

        assert_eq!(
            why,
            NotStarted::Tilde {
                program: "aws".to_owned(),
                entry: "~/.local/bin".to_owned(),
            }
        );
        let remedy = why.remedy(Origin::Kubeconfig);
        assert!(remedy.contains("`$HOME/.local/bin`"), "{remedy}");
        assert!(remedy.contains("instead of `~/.local/bin`"), "{remedy}");
    }

    #[test]
    fn a_bare_tilde_entry_means_the_home_directory_itself() {
        let why = diagnose_on("aws", "~:/usr/bin", &[("/home/sam/aws", runnable())]);

        assert!(
            matches!(why, NotStarted::Tilde { ref entry, .. } if entry == "~"),
            "{why:?}"
        );
    }

    #[test]
    fn a_tilde_entry_that_does_not_hold_the_program_is_not_blamed() {
        let why = diagnose_on("aws", "~/bin:/usr/bin", &[]);

        assert!(matches!(why, NotStarted::NotOnPath { .. }), "{why:?}");
    }

    #[test]
    fn another_users_tilde_is_not_guessed_at() {
        let why = diagnose_on(
            "aws",
            "~alice/bin",
            &[("/home/sam/alice/bin/aws", runnable())],
        );

        assert!(matches!(why, NotStarted::NotOnPath { .. }), "{why:?}");
    }

    #[test]
    fn without_a_home_directory_a_tilde_entry_cannot_be_checked() {
        let why = diagnose(
            "aws",
            &not_found(),
            Some(OsStr::new("~/bin")),
            None,
            disk(&[("/home/sam/bin/aws", runnable())]),
        );

        assert!(matches!(why, NotStarted::NotOnPath { .. }), "{why:?}");
    }

    #[test]
    fn no_path_at_all_is_its_own_message() {
        for path in [None, Some(OsStr::new("")), Some(OsStr::new("::"))] {
            let why = diagnose("aws", &not_found(), path, None, disk(&[]));

            assert_eq!(
                why,
                NotStarted::NoPath {
                    program: "aws".to_owned()
                },
                "{path:?}"
            );
        }
    }

    #[test]
    fn a_full_path_with_nothing_there_says_so_and_names_it() {
        // A kubeconfig written on a Mac with Homebrew under /usr/local, read
        // on one with it under /opt/homebrew.
        let why = diagnose_on(
            "/usr/local/bin/aws",
            "/opt/homebrew/bin:/usr/bin",
            &[("/opt/homebrew/bin/aws", runnable())],
        );

        assert_eq!(
            why,
            NotStarted::NoFile {
                path: "/usr/local/bin/aws".to_owned()
            }
        );
        let remedy = why.remedy(Origin::Kubeconfig);
        assert!(remedy.contains("`command -v aws`"), "{remedy}");
        assert!(remedy.contains("plain `aws`"), "{remedy}");
    }

    #[test]
    fn a_dangling_link_is_told_apart_from_nothing() {
        let why = diagnose_on(
            "/opt/homebrew/bin/aws",
            "/usr/bin",
            &[("/opt/homebrew/bin/aws", Entry::Dangling)],
        );

        assert!(matches!(why, NotStarted::Dangling { .. }), "{why:?}");
        assert!(why.remedy(Origin::Eks).contains("Reinstalling `aws`"));
    }

    #[test]
    fn a_script_whose_interpreter_is_gone_names_the_interpreter() {
        // The shape a Homebrew Python upgrade leaves: the `aws` script is on
        // PATH, and the Python its first line names is not. The kernel says
        // ENOENT for that too.
        let why = diagnose_on(
            "aws",
            "/usr/bin:/opt/homebrew/bin",
            &[(
                "/opt/homebrew/bin/aws",
                script("/opt/homebrew/opt/python@3.11/bin/python3.11"),
            )],
        );

        assert_eq!(
            why,
            NotStarted::NoInterpreter {
                path: "/opt/homebrew/bin/aws".to_owned(),
                interpreter: "/opt/homebrew/opt/python@3.11/bin/python3.11".to_owned(),
            }
        );
    }

    #[test]
    fn a_script_whose_interpreter_is_there_is_not_blamed_on_it() {
        let why = diagnose_on(
            "aws",
            "/usr/bin",
            &[
                ("/usr/bin/aws", script("/usr/bin/python3")),
                ("/usr/bin/python3", runnable()),
            ],
        );

        assert!(matches!(why, NotStarted::Other { .. }), "{why:?}");
    }

    #[test]
    fn a_file_that_is_not_executable_is_passed_over_for_a_later_one_as_execvp_does() {
        let why = diagnose(
            "aws",
            &io::Error::from(io::ErrorKind::PermissionDenied),
            Some(OsStr::new("/home/sam/bin:/usr/bin")),
            None,
            disk(&[(
                "/home/sam/bin/aws",
                Entry::File {
                    executable: false,
                    interpreter: None,
                },
            )]),
        );

        assert_eq!(
            why,
            NotStarted::NotExecutable {
                path: "/home/sam/bin/aws".to_owned()
            }
        );
        assert!(
            why.remedy(Origin::Eks)
                .contains("`chmod +x /home/sam/bin/aws`")
        );
    }

    #[test]
    fn an_error_this_module_cannot_explain_is_reported_in_the_systems_words() {
        let why = diagnose(
            "aws",
            &io::Error::other("Exec format error (os error 8)"),
            Some(OsStr::new("/usr/bin")),
            None,
            disk(&[("/usr/bin/aws", runnable())]),
        );

        assert_eq!(why.to_string(), "`aws`: Exec format error (os error 8)");
    }

    #[cfg(unix)]
    #[test]
    fn a_binary_built_for_another_cpu_is_not_looked_up_on_path() {
        // `ENOEXEC`, which is what Linux says for the wrong architecture. macOS
        // says `EBADARCH` instead. Either way the program was found, so the
        // lookup is not repeated and nothing blames PATH.
        let why = diagnose(
            "aws",
            &io::Error::from_raw_os_error(8),
            Some(OsStr::new("/usr/bin")),
            None,
            |_: &Path| panic!("a start that found its program needs no lookup"),
        );

        assert!(matches!(why, NotStarted::Other { .. }), "{why:?}");
        assert!(why.to_string().contains("Exec format error"), "{why}");
    }

    #[test]
    fn a_failure_with_no_program_to_name_is_just_the_reason() {
        let why = NotStarted::Other {
            program: String::new(),
            reason: "its `exec` block names no command".to_owned(),
        };

        assert_eq!(why.to_string(), "its `exec` block names no command");
    }

    // --- remedy ------------------------------------------------------------

    #[test]
    fn nothing_to_run_is_not_told_to_run_an_empty_name() {
        let why = NotStarted::Other {
            program: String::new(),
            reason: "its `exec` block names no command".to_owned(),
        };

        let remedy = why.remedy(Origin::Kubeconfig);

        assert!(!remedy.contains("``"), "{remedy}");
        assert!(remedy.contains("`command:`"), "{remedy}");
    }

    #[test]
    fn a_name_the_shell_finds_and_eks_does_not_is_sent_to_type() {
        let why = diagnose_on("aws", "/usr/bin", &[]);

        let remedy = why.remedy(Origin::Kubeconfig);

        assert!(remedy.contains("If `aws` works in your shell"), "{remedy}");
        assert!(remedy.contains("`type aws`"), "{remedy}");
        assert!(remedy.contains("alias"), "{remedy}");
        assert!(remedy.contains("`command:`"), "{remedy}");
        assert!(remedy.contains(AWS_INSTALL_URL), "{remedy}");
    }

    #[test]
    fn a_program_eks_names_itself_is_never_fixed_in_a_kubeconfig() {
        for why in [
            diagnose_on("aws", "/usr/bin", &[]),
            NotStarted::NoPath {
                program: "aws".to_owned(),
            },
            NotStarted::NotExecutable {
                path: "/usr/bin/aws".to_owned(),
            },
        ] {
            let remedy = why.remedy(Origin::Eks);
            assert!(!remedy.contains("command:"), "{remedy}");
            assert!(remedy.contains("PATH"), "{remedy}");
        }
    }

    #[test]
    fn only_the_aws_cli_comes_with_an_install_link() {
        let why = diagnose_on("kubelogin", "/usr/bin", &[]);

        let remedy = why.remedy(Origin::Kubeconfig);

        assert!(!remedy.contains(AWS_INSTALL_URL), "{remedy}");
        assert!(remedy.contains("`type kubelogin`"), "{remedy}");
    }

    // --- interpreter -------------------------------------------------------

    #[test]
    fn the_interpreter_is_the_first_word_of_the_first_line() {
        assert_eq!(
            interpreter(b"#!/usr/bin/python3 -E\nimport sys\n").as_deref(),
            Some("/usr/bin/python3")
        );
        assert_eq!(interpreter(b"#! /bin/sh\n").as_deref(), Some("/bin/sh"));
    }

    #[test]
    fn a_file_that_is_not_a_script_has_no_interpreter() {
        assert_eq!(interpreter(b"\x7fELF\x02\x01"), None);
        assert_eq!(interpreter(b""), None);
        assert_eq!(interpreter(b"#!"), None);
        assert_eq!(interpreter(b"#!\xff\xfe"), None);
    }

    // --- explain, on a real filesystem --------------------------------------

    #[cfg(unix)]
    #[test]
    fn the_real_probe_tells_a_missing_file_a_dangling_link_and_a_dead_interpreter_apart() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("link");
        symlink(dir.path().join("gone"), &link).unwrap();
        let script = dir.path().join("script");
        std::fs::write(&script, "#!/eks-test/no/such/python\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(probe(&dir.path().join("nothing")), Entry::Absent);
        assert_eq!(probe(&link), Entry::Dangling);
        assert_eq!(probe(dir.path()), Entry::Directory);
        assert_eq!(probe(&script), self::script("/eks-test/no/such/python"));
        assert_eq!(
            probe(&plain),
            Entry::File {
                executable: false,
                interpreter: None
            }
        );

        let script = script.to_string_lossy();
        assert!(
            matches!(
                explain(&script, &not_found()),
                NotStarted::NoInterpreter { .. }
            ),
            "{:?}",
            explain(&script, &not_found())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_real_start_of_a_script_with_a_dead_interpreter_is_diagnosed_from_its_error() {
        // The case that makes this module necessary: the kernel's answer is
        // ENOENT, the same as for a program that is not there at all.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("aws");
        // Written by a child `sh`, not by this process: a file this process
        // holds open for writing is inherited by whatever another test thread
        // forks at that moment, and executing it would fail with `ETXTBSY`.
        let written = std::process::Command::new("sh")
            .args([
                "-c",
                "printf '#!/eks-test/no/such/python\\n' > \"$1\" && chmod 755 \"$1\"",
                "sh",
            ])
            .arg(&script)
            .status()
            .unwrap();
        assert!(written.success());

        let error = std::process::Command::new(&script)
            .spawn()
            .expect_err("its interpreter does not exist");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);

        let why = explain(&script.to_string_lossy(), &error);
        assert!(matches!(why, NotStarted::NoInterpreter { .. }), "{why:?}");
        assert!(
            why.to_string().contains("/eks-test/no/such/python"),
            "{why}"
        );
    }
}
