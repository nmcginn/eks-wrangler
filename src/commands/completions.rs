//! Shell completions and the man page.
//!
//! Both are rendered straight from the [`Cli`] definition `clap` already
//! parses with, through `clap_complete` and `clap_mangen`, so a flag this
//! parser does not know about cannot appear in either — and one it does know
//! about cannot go undescribed.

use clap::CommandFactory as _;
use clap_complete::Shell;

use crate::cli::Cli;

/// The name completions and the man page are generated under.
///
/// `Cli`'s own `#[command(name = "eks")]` decides what `--help` and error
/// messages call the program; this is the second place that name has to
/// agree with it, so it is spelled once rather than copied into each call
/// site below.
const BIN_NAME: &str = "eks";

/// Render a shell's completion script.
///
/// A pure function over the shell choice: `clap_complete::generate` only
/// walks the [`clap::Command`] tree `Cli::command()` builds and writes text
/// to a buffer, so there is no I/O here to separate out — the same reason
/// [`man`] beside it is just as plain.
#[must_use]
pub fn shell(shell: Shell) -> String {
    let mut buf = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), BIN_NAME, &mut buf);
    // `clap_complete`'s templates are ASCII/UTF-8 by construction; a lossy
    // conversion is only ever a formality here, never a real loss.
    String::from_utf8_lossy(&buf).into_owned()
}

/// Render the man page as roff.
#[must_use]
pub fn man() -> String {
    let mut buf = Vec::new();
    let mut cmd = Cli::command();
    cmd.set_bin_name(BIN_NAME);
    // `Man::render` only fails on a write error, which a `Vec<u8>` cannot
    // produce — there is no failure here for a caller to act on.
    let _ = clap_mangen::Man::new(cmd).render(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use clap::ValueEnum as _;

    use super::*;

    #[test]
    fn every_shell_renders_nonempty_output_naming_the_binary() {
        // Covers every shell `clap_complete::Shell` knows, not only the three
        // the roadmap named — the crate gives the rest for free, and a shell
        // that panicked or came back empty would be a silent regression the
        // roadmap's own three-shell test could miss.
        for shell_choice in Shell::value_variants() {
            let script = shell(*shell_choice);
            assert!(!script.is_empty(), "{shell_choice} produced nothing");
            assert!(
                script.contains(BIN_NAME),
                "{shell_choice} completion script does not mention {BIN_NAME}: {script}"
            );
        }
    }

    #[test]
    fn bash_completion_defines_a_complete_function() {
        let script = shell(Shell::Bash);
        assert!(script.contains("complete"), "{script}");
    }

    #[test]
    fn zsh_completion_declares_itself_a_compdef() {
        // Zsh's completion loader looks for this line to recognise the file
        // as a completion definition rather than an ordinary script.
        let script = shell(Shell::Zsh);
        assert!(script.contains("#compdef eks"), "{script}");
    }

    #[test]
    fn fish_completion_uses_fishs_own_complete_builtin() {
        let script = shell(Shell::Fish);
        assert!(script.contains("complete -c eks"), "{script}");
    }

    #[test]
    fn completion_scripts_mention_the_nodes_subcommand() {
        // A weak proof the generator is actually walking `Cli`'s real
        // subcommand tree rather than a stub: `nodes` is not the binary's
        // own name, so it can only appear here by clap having read it off
        // `Command::Nodes`.
        for shell_choice in Shell::value_variants() {
            let script = shell(*shell_choice);
            assert!(
                script.contains("nodes"),
                "{shell_choice} completion is missing the nodes subcommand: {script}"
            );
        }
    }

    #[test]
    fn man_page_is_roff_naming_the_binary() {
        // `.TH` is roff's title heading, `Man::render` writes a couple of
        // preamble macros ahead of it, so this checks for the line rather
        // than assuming it opens the document.
        let page = man();
        assert!(page.contains(".TH eks 1"), "{page}");
    }

    #[test]
    fn man_page_documents_the_nodes_subcommand() {
        let page = man();
        assert!(page.contains("nodes"), "{page}");
    }
}
