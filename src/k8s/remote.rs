//! Running a command in a container: what to run, how to wire it to this
//! terminal, and what its ending means.
//!
//! The API server runs a command in a container over a WebSocket that `kube`
//! turns into three pipes and a status ([`kube::api::AttachedProcess`]). What
//! is left for `eks` is the part `kubectl exec` makes the user do by hand:
//! knowing which shell an image has, deciding whether the session wants a
//! TTY, carrying bytes both ways until the command ends, and turning the
//! status the kubelet sends back into an exit code — or, when the command
//! never started, into a sentence about why.
//!
//! [`shells`], [`ending`], [`params`], and [`wants_tty`] are pure. [`relay`]
//! is generic over its readers and writers, so the whole carrying of bytes is
//! tested against in-memory pipes; the only things it never sees in a test
//! are a real terminal and a real cluster, which `commands::exec` supplies.
//! Kept apart from [`crate::k8s::exec`], which is the kubeconfig's credential
//! helper — a different `exec` that happens to share the word.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::future::Future;
use std::io;

use futures_util::stream::{Stream, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
use kube::api::{AttachParams, TerminalSize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The label a node, or a pod's `nodeSelector`, says its operating system in.
pub const OS_LABEL: &str = "kubernetes.io/os";

/// The operating system a container runs on, which decides what its shell is
/// called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Windows,
}

impl Os {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "linux" => Some(Self::Linux),
            "windows" => Some(Self::Windows),
            _ => None,
        }
    }
}

/// What a pod itself says about the OS it runs on: `spec.os.name`, then a
/// `nodeSelector` on [`OS_LABEL`].
///
/// `None` is the common case — nearly every Linux pod says neither — and is
/// read as "probably Linux, ask the node if that turns out wrong".
#[must_use]
pub fn os_of_pod(pod: &Pod) -> Option<Os> {
    let spec = pod.spec.as_ref()?;
    spec.os
        .as_ref()
        .and_then(|os| Os::parse(&os.name))
        .or_else(|| {
            spec.node_selector
                .as_ref()
                .and_then(|selector| selector.get(OS_LABEL))
                .and_then(|name| Os::parse(name))
        })
}

/// What a node's labels say about its OS.
#[must_use]
pub fn os_of_labels(labels: Option<&BTreeMap<String, String>>) -> Option<Os> {
    labels
        .and_then(|labels| labels.get(OS_LABEL))
        .and_then(|name| Os::parse(name))
}

/// The commands to try, in order, when the user asked for a shell rather
/// than a command.
///
/// On Linux the first asks `/bin/sh` to hand over to `/bin/bash` when there
/// is one, so the common image costs one round trip and still gets the
/// friendlier shell. The second is for the rare image with `bash` and no
/// `/bin/sh` at all. Both failing to start is how a shell-less (distroless)
/// image is recognised.
#[must_use]
pub fn shells(os: Os) -> Vec<Vec<String>> {
    let owned = |argv: &[&str]| argv.iter().map(|arg| (*arg).to_owned()).collect();
    match os {
        Os::Linux => vec![
            owned(&[
                "/bin/sh",
                "-c",
                "if [ -x /bin/bash ]; then exec /bin/bash; else exec /bin/sh; fi",
            ]),
            owned(&["/bin/bash"]),
        ],
        Os::Windows => vec![owned(&["cmd.exe"])],
    }
}

/// Whether the session runs with a TTY: only when both ends of it are a
/// terminal.
///
/// Stdin, because a TTY puts the terminal in raw mode and a pipe has no
/// mode — `echo hi | eks exec api -- cat` has to see `hi`, not a terminal
/// session. And stdout, because a TTY turns every newline into `\r\n` and
/// merges stderr into stdout: `eks exec api -- cat /etc/hosts > hosts` would
/// otherwise save a file nobody asked for. `kubectl exec -it` checks only
/// stdin and gets that second case wrong.
#[must_use]
pub fn wants_tty(stdin_is_terminal: bool, stdout_is_terminal: bool) -> bool {
    stdin_is_terminal && stdout_is_terminal
}

/// The attach parameters for one session.
///
/// Stdin is always attached: with a TTY it carries the keyboard, and without
/// one it carries whatever was piped in, which reaches the remote command as
/// end-of-input once the pipe is drained. Stderr is attached only without a
/// TTY, because the API server refuses the two together — a terminal has one
/// output stream, and the remote side writes both into it.
#[must_use]
pub fn params(container: &str, tty: bool) -> AttachParams {
    AttachParams {
        container: Some(container.to_owned()),
        stdin: true,
        stdout: true,
        stderr: !tty,
        tty,
        // A screenful of output per message instead of `kube`'s 1 KiB, so a
        // `cat` of a large file is not a thousand round trips through the
        // pipe — and the same for a paste into the remote shell.
        max_stdin_buf_size: Some(BUFFER),
        max_stdout_buf_size: Some(BUFFER),
        max_stderr_buf_size: Some(BUFFER),
    }
}

/// How much one read or write carries, in either direction.
const BUFFER: usize = 16 * 1024;

/// How a remote command's session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// The command ran and exited with this code, which becomes `eks`'s own.
    Exited(u8),
    /// The command was never started because the container has no such
    /// executable. Carries the runtime's own words, for `-v`.
    Missing(String),
    /// The command could not be started, or the session failed, for some
    /// other reason. Carries the API server's message.
    Failed(String),
    /// The connection closed without a status at all — the API server or the
    /// network went away mid-session.
    Lost,
}

/// Decode the status the kubelet sends on the session's error channel.
///
/// `Success` is exit code 0. A non-zero exit arrives as a `Failure` with
/// reason `NonZeroExitCode` and the code as a cause, the way `kubectl` reads
/// it. Any other `Failure` means the command did not run, and the runtime's
/// message is all there is to say why — which is how a missing executable
/// shows up, since the container runtime refuses to start it rather than
/// reporting an exit.
#[must_use]
pub fn ending(status: Option<&Status>) -> Ending {
    let Some(status) = status else {
        return Ending::Lost;
    };
    if status.status.as_deref() == Some("Success") {
        return Ending::Exited(0);
    }

    let message = status.message.clone().unwrap_or_default();
    if status.reason.as_deref() == Some("NonZeroExitCode") {
        let code = status
            .details
            .as_ref()
            .and_then(|details| details.causes.as_ref())
            .and_then(|causes| {
                causes
                    .iter()
                    .find(|cause| cause.reason.as_deref() == Some("ExitCode"))
            })
            .and_then(|cause| cause.message.as_deref())
            .and_then(|code| code.trim().parse::<i64>().ok());
        // A code outside what a process can exit with — or none at all — still
        // has to be a failure; `1` is the shell's own "failed, no detail".
        return Ending::Exited(code.and_then(|code| u8::try_from(code).ok()).unwrap_or(1));
    }

    if missing_executable(&message) {
        Ending::Missing(message)
    } else {
        Ending::Failed(message)
    }
}

/// Whether a runtime's refusal means the executable is not in the image.
///
/// containerd and CRI-O on Linux say `executable file not found` for a bare
/// name looked up on `$PATH`, and `no such file or directory` for an absolute
/// path; Windows says `The system cannot find the file specified`. Matched on
/// lowercase text because nothing more structured reaches the client.
fn missing_executable(message: &str) -> bool {
    let message = message.to_lowercase();
    message.contains("executable file not found")
        || message.contains("no such file or directory")
        || message.contains("cannot find the file specified")
}

/// The remote end of a session: the pipes `kube` hands over, each present
/// only when [`params`] asked for it.
#[derive(Debug)]
pub struct Remote<W, O, E> {
    pub stdin: Option<W>,
    pub stdout: Option<O>,
    pub stderr: Option<E>,
}

/// The local end of a session.
#[derive(Debug)]
pub struct Local<'a, I, O, E> {
    /// Borrowed rather than owned, so a session that never started — a shell
    /// that is not in the image — leaves it to the next attempt with nothing
    /// lost: a read still in flight resumes on the next poll.
    pub stdin: &'a mut I,
    pub stdout: &'a mut O,
    pub stderr: &'a mut E,
}

/// Carry bytes between the two ends until the remote command has ended and
/// everything it printed has been written out.
///
/// The session is over when the remote stdout and stderr have both reached
/// their end *and* the status has arrived: the status alone could beat the
/// last of the output through the pipes, and the output alone says nothing
/// about the exit code. Local stdin is never waited for — it may be a
/// keyboard nobody is touching — so it is fed for as long as the session
/// lasts and then simply abandoned; at its own end the remote stdin is
/// closed, which is what lets `echo hi | eks exec api -- cat` finish.
///
/// Every terminal size `sizes` yields is handed to `resize`, which forwards
/// it to the remote TTY. Without a TTY, `sizes` is empty.
///
/// A failure to write locally — most often a closed pipe, as in
/// `eks exec api -- cat big.log | head` — ends the session with that error.
/// A failure to write remotely only stops feeding stdin: the remote side
/// has stopped reading, and its status is still the answer to wait for.
pub async fn relay<W, O, E, LI, LO, LE>(
    remote: Remote<W, O, E>,
    status: impl Future<Output = Option<Status>>,
    local: Local<'_, LI, LO, LE>,
    sizes: impl Stream<Item = TerminalSize>,
    mut resize: impl FnMut(TerminalSize),
) -> io::Result<Ending>
where
    W: AsyncWrite + Unpin,
    O: AsyncRead + Unpin,
    E: AsyncRead + Unpin,
    LI: AsyncRead + Unpin,
    LO: AsyncWrite + Unpin,
    LE: AsyncWrite + Unpin,
{
    let Local {
        stdin: local_in,
        stdout: local_out,
        stderr: local_err,
    } = local;

    // `try_join!`, so a local write that fails ends the session at once
    // rather than after the other output and the status have finished too —
    // with the reader gone, neither ever will.
    let finished = async {
        let ((), (), status) = tokio::try_join!(
            drain(remote.stdout, local_out),
            drain(remote.stderr, local_err),
            async { Ok(status.await) },
        )?;
        Ok(ending(status.as_ref()))
    };

    let feeding = async {
        if let Some(remote_in) = remote.stdin {
            feed(local_in, remote_in).await;
        }
        std::future::pending::<Infallible>().await
    };

    let resizing = async {
        let mut sizes = std::pin::pin!(sizes);
        while let Some(size) = sizes.next().await {
            resize(size);
        }
        std::future::pending::<Infallible>().await
    };

    // Feeding and resizing never finish: once their input ends they wait
    // forever, so only the remote command's ending can end the session.
    tokio::select! {
        ending = finished => ending,
        never = feeding => match never {},
        never = resizing => match never {},
    }
}

/// Copy a remote output to its local counterpart until it ends, flushing as
/// each piece arrives so an interactive prompt appears when it is printed
/// rather than when a buffer fills.
async fn drain(
    remote: Option<impl AsyncRead + Unpin>,
    local: &mut (impl AsyncWrite + Unpin),
) -> io::Result<()> {
    let Some(mut remote) = remote else {
        return Ok(());
    };
    let mut buffer = vec![0; BUFFER];
    loop {
        let read = remote.read(&mut buffer).await?;
        if read == 0 {
            return local.flush().await;
        }
        local.write_all(&buffer[..read]).await?;
        local.flush().await?;
    }
}

/// Copy local stdin to the remote command until either end stops, then drop
/// the remote writer — which is what tells the API server stdin is closed.
async fn feed(local: &mut (impl AsyncRead + Unpin), mut remote: impl AsyncWrite + Unpin) {
    let mut buffer = vec![0; BUFFER];
    loop {
        let Ok(read) = local.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            let _ = remote.shutdown().await;
            return;
        }
        if remote.write_all(&buffer[..read]).await.is_err() || remote.flush().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::time::Duration;

    use k8s_openapi::api::core::v1::{PodOS, PodSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{StatusCause, StatusDetails};
    use tokio::io::{duplex, empty, sink};

    use super::*;

    fn success() -> Status {
        Status {
            status: Some("Success".to_owned()),
            ..Status::default()
        }
    }

    fn exited(code: &str) -> Status {
        Status {
            status: Some("Failure".to_owned()),
            reason: Some("NonZeroExitCode".to_owned()),
            message: Some(format!(
                "command terminated with non-zero exit code: error executing command [sh -c exit {code}], exit code {code}"
            )),
            details: Some(StatusDetails {
                causes: Some(vec![StatusCause {
                    reason: Some("ExitCode".to_owned()),
                    message: Some(code.to_owned()),
                    ..StatusCause::default()
                }]),
                ..StatusDetails::default()
            }),
            ..Status::default()
        }
    }

    fn failure(message: &str) -> Status {
        Status {
            status: Some("Failure".to_owned()),
            reason: Some("InternalError".to_owned()),
            message: Some(message.to_owned()),
            ..Status::default()
        }
    }

    // --- OS and shells ---

    fn pod_with(spec: PodSpec) -> Pod {
        Pod {
            spec: Some(spec),
            ..Pod::default()
        }
    }

    #[test]
    fn a_pod_that_says_nothing_about_its_os_is_unknown() {
        assert_eq!(os_of_pod(&pod_with(PodSpec::default())), None);
        assert_eq!(os_of_pod(&Pod::default()), None);
    }

    #[test]
    fn spec_os_names_the_os() {
        let pod = pod_with(PodSpec {
            os: Some(PodOS {
                name: "windows".to_owned(),
            }),
            ..PodSpec::default()
        });
        assert_eq!(os_of_pod(&pod), Some(Os::Windows));
    }

    #[test]
    fn a_node_selector_on_the_os_label_names_the_os() {
        let pod = pod_with(PodSpec {
            node_selector: Some(BTreeMap::from([(
                OS_LABEL.to_owned(),
                "windows".to_owned(),
            )])),
            ..PodSpec::default()
        });
        assert_eq!(os_of_pod(&pod), Some(Os::Windows));
    }

    #[test]
    fn a_node_label_names_the_os_and_anything_else_is_unknown() {
        let windows = BTreeMap::from([(OS_LABEL.to_owned(), "windows".to_owned())]);
        let plan9 = BTreeMap::from([(OS_LABEL.to_owned(), "plan9".to_owned())]);
        assert_eq!(os_of_labels(Some(&windows)), Some(Os::Windows));
        assert_eq!(os_of_labels(Some(&plan9)), None);
        assert_eq!(os_of_labels(None), None);
    }

    #[test]
    fn linux_prefers_bash_through_sh_then_tries_bash_alone() {
        let shells = shells(Os::Linux);
        assert_eq!(shells.len(), 2);
        assert_eq!(shells[0][0], "/bin/sh");
        assert!(shells[0][2].contains("exec /bin/bash"));
        assert!(
            shells[0][2].find("/bin/bash") < shells[0][2].rfind("exec /bin/sh"),
            "bash is preferred over sh"
        );
        assert_eq!(shells[1], ["/bin/bash"]);
    }

    #[test]
    fn windows_runs_cmd() {
        assert_eq!(shells(Os::Windows), [["cmd.exe"]]);
    }

    // --- TTY and params ---

    #[test]
    fn a_tty_needs_a_terminal_at_both_ends() {
        assert!(wants_tty(true, true));
        assert!(!wants_tty(false, true), "piped stdin");
        assert!(!wants_tty(true, false), "redirected stdout");
        assert!(!wants_tty(false, false));
    }

    #[test]
    fn a_tty_session_attaches_stdin_and_stdout_but_not_stderr() {
        let ap = params("app", true);
        assert_eq!(ap.container.as_deref(), Some("app"));
        assert!(ap.tty && ap.stdin && ap.stdout);
        assert!(!ap.stderr, "the API server refuses stderr beside a tty");
    }

    #[test]
    fn a_piped_session_attaches_all_three_streams_without_a_tty() {
        let ap = params("app", false);
        assert!(!ap.tty);
        assert!(ap.stdin && ap.stdout && ap.stderr);
    }

    // --- ending ---

    #[test]
    fn success_is_exit_code_zero() {
        assert_eq!(ending(Some(&success())), Ending::Exited(0));
    }

    #[test]
    fn a_non_zero_exit_carries_its_code() {
        assert_eq!(ending(Some(&exited("3"))), Ending::Exited(3));
        assert_eq!(ending(Some(&exited("137"))), Ending::Exited(137));
    }

    #[test]
    fn a_non_zero_exit_with_an_unreadable_code_is_still_a_failure() {
        assert_eq!(ending(Some(&exited("lots"))), Ending::Exited(1));
        assert_eq!(ending(Some(&exited("4096"))), Ending::Exited(1));
        assert_eq!(ending(Some(&exited("-1"))), Ending::Exited(1));

        let mut no_causes = exited("3");
        no_causes.details = None;
        assert_eq!(ending(Some(&no_causes)), Ending::Exited(1));
    }

    #[test]
    fn no_status_at_all_is_a_lost_connection() {
        assert_eq!(ending(None), Ending::Lost);
    }

    #[test]
    fn a_missing_absolute_path_is_missing() {
        let message = "Internal error occurred: error executing command in container: failed to exec in container: failed to start exec \"abc\": OCI runtime exec failed: exec failed: unable to start container process: exec: \"/bin/bash\": stat /bin/bash: no such file or directory: unknown";
        assert_eq!(
            ending(Some(&failure(message))),
            Ending::Missing(message.to_owned())
        );
    }

    #[test]
    fn a_name_not_on_the_path_is_missing() {
        let message = "OCI runtime exec failed: exec failed: unable to start container process: exec: \"htop\": executable file not found in $PATH: unknown";
        assert!(matches!(
            ending(Some(&failure(message))),
            Ending::Missing(_)
        ));
    }

    #[test]
    fn a_missing_windows_executable_is_missing() {
        let message = "hcs::System::CreateProcess: The system cannot find the file specified.";
        assert!(matches!(
            ending(Some(&failure(message))),
            Ending::Missing(_)
        ));
    }

    #[test]
    fn any_other_failure_carries_the_api_servers_message() {
        assert_eq!(
            ending(Some(&failure("container not running (abc)"))),
            Ending::Failed("container not running (abc)".to_owned())
        );
    }

    // --- relay ---

    /// A remote command played by the test: what it prints, and the status
    /// it ends with once its outputs are closed.
    struct Played {
        stdout: tokio::io::DuplexStream,
        stderr: tokio::io::DuplexStream,
        stdin: tokio::io::DuplexStream,
    }

    /// Pipes standing in for the three `kube` hands over, with the far ends
    /// returned so a test can play the remote command.
    fn remote() -> (
        Remote<tokio::io::DuplexStream, tokio::io::DuplexStream, tokio::io::DuplexStream>,
        Played,
    ) {
        let (stdin_ours, stdin_theirs) = duplex(64);
        let (stdout_theirs, stdout_ours) = duplex(64);
        let (stderr_theirs, stderr_ours) = duplex(64);
        (
            Remote {
                stdin: Some(stdin_ours),
                stdout: Some(stdout_ours),
                stderr: Some(stderr_ours),
            },
            Played {
                stdout: stdout_theirs,
                stderr: stderr_theirs,
                stdin: stdin_theirs,
            },
        )
    }

    fn no_sizes() -> futures_util::stream::Empty<TerminalSize> {
        futures_util::stream::empty()
    }

    #[tokio::test]
    async fn output_reaches_the_local_streams_and_the_exit_code_comes_back() {
        let (remote, mut played) = remote();
        let (status_tx, status_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            played.stdout.write_all(b"out\n").await.unwrap();
            played.stderr.write_all(b"err\n").await.unwrap();
            drop(played);
            status_tx.send(exited("3")).unwrap();
        });

        let (mut stdin, mut stdout, mut stderr) = (empty(), Vec::new(), Vec::new());
        let ending = relay(
            remote,
            async { status_rx.await.ok() },
            Local {
                stdin: &mut stdin,
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
            no_sizes(),
            |_| {},
        )
        .await
        .unwrap();

        assert_eq!(ending, Ending::Exited(3));
        assert_eq!(stdout, b"out\n");
        assert_eq!(stderr, b"err\n");
    }

    #[tokio::test]
    async fn piped_stdin_reaches_the_remote_command_and_then_closes_it() {
        // `echo hi | eks exec api -- cat`: the remote `cat` sees `hi`, then
        // end of input, and only then exits — so the relay has to close the
        // remote stdin when the local one runs dry.
        let (remote, played) = remote();
        let Played {
            mut stdout,
            stderr,
            mut stdin,
        } = played;
        let (status_tx, status_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            // `cat`: copy stdin to stdout until stdin ends.
            let mut seen = Vec::new();
            stdin.read_to_end(&mut seen).await.unwrap();
            stdout.write_all(&seen).await.unwrap();
            drop((stdout, stderr));
            status_tx.send(success()).unwrap();
        });

        let mut local_in: &[u8] = b"hi\n";
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let ending = relay(
            remote,
            async { status_rx.await.ok() },
            Local {
                stdin: &mut local_in,
                stdout: &mut out,
                stderr: &mut err,
            },
            no_sizes(),
            |_| {},
        )
        .await
        .unwrap();

        assert_eq!(ending, Ending::Exited(0));
        assert_eq!(out, b"hi\n");
    }

    #[tokio::test]
    async fn output_still_in_the_pipe_when_the_status_arrives_is_not_lost() {
        let (remote, mut played) = remote();

        let writer = tokio::spawn(async move {
            // More than the pipe holds, so it is still being written while
            // the status below is already resolved.
            played.stdout.write_all(&[b'x'; 1000]).await.unwrap();
            drop(played);
        });

        let (mut stdin, mut stdout, mut stderr) = (empty(), Vec::new(), Vec::new());
        let ending = relay(
            remote,
            async { Some(success()) },
            Local {
                stdin: &mut stdin,
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
            no_sizes(),
            |_| {},
        )
        .await
        .unwrap();
        writer.await.unwrap();

        assert_eq!(ending, Ending::Exited(0));
        assert_eq!(stdout.len(), 1000);
    }

    #[tokio::test]
    async fn a_keyboard_nobody_types_on_does_not_hold_the_session_open() {
        // A TTY's stdin never ends on its own. The session has to finish when
        // the remote command does, abandoning the read still waiting on it.
        let (remote, played) = remote();
        drop((played.stdout, played.stderr));
        let (_keyboard, mut idle_stdin) = duplex(8);

        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let ending = tokio::time::timeout(
            Duration::from_secs(5),
            relay(
                remote,
                async { Some(success()) },
                Local {
                    stdin: &mut idle_stdin,
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                },
                no_sizes(),
                |_| {},
            ),
        )
        .await
        .expect("the session waited for stdin")
        .unwrap();

        assert_eq!(ending, Ending::Exited(0));
    }

    #[tokio::test]
    async fn stdin_left_unread_after_one_session_is_there_for_the_next() {
        // The shell search may start a second session after the first found
        // no shell; keys typed in between belong to the one that works.
        let (mut keyboard, mut stdin) = duplex(64);

        let (first, played) = remote();
        drop((played.stdout, played.stderr));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let missing = relay(
            first,
            async { Some(failure("stat /bin/bash: no such file or directory")) },
            Local {
                stdin: &mut stdin,
                stdout: &mut out,
                stderr: &mut err,
            },
            no_sizes(),
            |_| {},
        )
        .await
        .unwrap();
        assert!(matches!(missing, Ending::Missing(_)));

        keyboard.write_all(b"ls\n").await.unwrap();
        drop(keyboard);

        let (second, played) = remote();
        let Played {
            stdout,
            stderr,
            stdin: mut remote_stdin,
        } = played;
        let (status_tx, status_rx) = tokio::sync::oneshot::channel();
        let shell = tokio::spawn(async move {
            let mut seen = Vec::new();
            remote_stdin.read_to_end(&mut seen).await.unwrap();
            drop((stdout, stderr));
            status_tx.send(success()).unwrap();
            seen
        });
        relay(
            second,
            async { status_rx.await.ok() },
            Local {
                stdin: &mut stdin,
                stdout: &mut out,
                stderr: &mut err,
            },
            no_sizes(),
            |_| {},
        )
        .await
        .unwrap();

        assert_eq!(shell.await.unwrap(), b"ls\n");
    }

    #[tokio::test]
    async fn every_terminal_size_is_forwarded_to_the_remote_tty() {
        let (remote, played) = remote();
        let (sizes_tx, sizes_rx) = tokio::sync::mpsc::unbounded_channel();
        let (status_tx, status_rx) = tokio::sync::oneshot::channel();
        let sizes = futures_util::stream::unfold(sizes_rx, |mut rx| async move {
            rx.recv().await.map(|size| (size, rx))
        });

        let mut forwarded = Vec::new();
        let driver = async {
            sizes_tx
                .send(TerminalSize {
                    width: 80,
                    height: 24,
                })
                .unwrap();
            sizes_tx
                .send(TerminalSize {
                    width: 120,
                    height: 40,
                })
                .unwrap();
            // Let the relay pick both up before the command ends.
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop((played.stdout, played.stderr));
            status_tx.send(success()).unwrap();
        };

        let (mut stdin, mut stdout, mut stderr) = (empty(), Vec::new(), Vec::new());
        let (ending, ()) = tokio::join!(
            relay(
                remote,
                async { status_rx.await.ok() },
                Local {
                    stdin: &mut stdin,
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                },
                sizes,
                |size| forwarded.push((size.width, size.height)),
            ),
            driver,
        );

        assert_eq!(ending.unwrap(), Ending::Exited(0));
        assert_eq!(forwarded, [(80, 24), (120, 40)]);
    }

    #[tokio::test]
    async fn a_tty_session_has_no_stderr_to_drain() {
        let (stdin_ours, _stdin_theirs) = duplex(8);
        let (mut stdout_theirs, stdout_ours) = duplex(64);
        let remote: Remote<_, _, tokio::io::DuplexStream> = Remote {
            stdin: Some(stdin_ours),
            stdout: Some(stdout_ours),
            stderr: None,
        };
        stdout_theirs.write_all(b"$ ").await.unwrap();
        drop(stdout_theirs);

        let (mut stdin, mut stdout, mut stderr) = (empty(), Vec::new(), sink());
        let ending = relay(
            remote,
            async { Some(success()) },
            Local {
                stdin: &mut stdin,
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
            no_sizes(),
            |_| {},
        )
        .await
        .unwrap();

        assert_eq!(ending, Ending::Exited(0));
        assert_eq!(stdout, b"$ ");
    }

    #[tokio::test]
    async fn a_closed_local_stdout_ends_the_session_with_that_error() {
        // `eks exec api -- cat big.log | head`.
        struct Closed;
        impl AsyncWrite for Closed {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                _: &[u8],
            ) -> std::task::Poll<io::Result<usize>> {
                std::task::Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
            }
            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }

        let (remote, mut played) = remote();
        played.stdout.write_all(b"line\n").await.unwrap();

        let (mut stdin, mut stdout, mut stderr) = (empty(), Closed, Vec::new());
        let error = relay(
            remote,
            std::future::pending(),
            Local {
                stdin: &mut stdin,
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
            no_sizes(),
            |_| {},
        )
        .await
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn a_connection_that_drops_without_a_status_is_lost() {
        let (remote, played) = remote();
        drop(played);

        let (mut stdin, mut stdout, mut stderr) = (empty(), Vec::new(), Vec::new());
        let ending = relay(
            remote,
            async { None },
            Local {
                stdin: &mut stdin,
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
            no_sizes(),
            |_| {},
        )
        .await
        .unwrap();

        assert_eq!(ending, Ending::Lost);
    }
}
