//! Writes `docs/user/reference/cli-commands.md`, rendered from `piquelctl`'s
//! clap model so it matches `--help` exactly, or with `--check` compares it
//! with the checked-in page. `just generate` and `just openapi-check` run it
//! after the daemon's `generate_openapi`, which regenerates the client this
//! depends on.

use clap::{Arg, ArgAction, Command};
use piqueld_generated::{ArtifactError, Artifacts, Page};
use std::fmt::Write as _;

/// The page's introduction, after its title.
const INTRO: &str = "\
This page lists every `piquelctl` command with its arguments and options. It
is generated from the same definitions as `piquelctl --help`, so `--help` on
any command prints the same text. For how to install the CLI, connect it to a
daemon, and read its output, see [piquelctl](cli.md). The
[user guide](../introduction.md) shows the commands at work.

Every command also accepts `-h` (a summary) and `--help` (everything below),
and `piquelctl --version` prints the CLI's version.";

/// Renders the reference, one section per command, depth first.
struct CliReference {
    page: Page,
}

impl CliReference {
    /// Where the page is written, relative to the repository root.
    const PATH: &str = "docs/user/reference/cli-commands.md";

    /// Renders the page from the complete command tree, `piquelctl::command()`.
    fn render(mut command: Command) -> String {
        // Building fills in each subcommand's full name and its usage.
        command.build();
        let mut reference = Self {
            page: Page::new(
                "piquelctl's clap definitions in `apps/piquelctl/src`",
                "piquelctl commands",
                INTRO,
            ),
        };
        reference.command(&mut command, true);
        reference.page.finish()
    }

    /// Adds `command`'s section, then its subcommands'. The root's section
    /// also lists the global options, which every command accepts.
    fn command(&mut self, command: &mut Command, root: bool) {
        let path = command
            .get_bin_name()
            .unwrap_or(command.get_name())
            .to_owned();
        self.page.heading(2, &format!("`{path}`"));
        if let Some(about) = command.get_long_about().or(command.get_about()) {
            self.page.paragraphs(&Self::sentence(&about.to_string()));
        }
        let usage = command.render_usage().to_string();
        self.page.code(usage.trim_start_matches("Usage: "));

        let (positionals, options): (Vec<&Arg>, Vec<&Arg>) = command
            .get_arguments()
            .filter(|argument| Self::documented(argument) && argument.is_global_set() == root)
            .partition(|argument| argument.is_positional());
        for (title, arguments) in [
            ("Arguments", positionals),
            (if root { "Global options" } else { "Options" }, options),
        ] {
            if !arguments.is_empty() {
                self.page.paragraphs(&format!("{title}:"));
                for argument in arguments {
                    self.argument(argument);
                }
            }
        }

        let subcommands: Vec<&Command> = command
            .get_subcommands()
            .filter(|subcommand| Self::listed(subcommand))
            .collect();
        if !subcommands.is_empty() {
            self.page.paragraphs("Commands:");
            for subcommand in subcommands {
                let name = format!("{path} {}", subcommand.get_name());
                let about = subcommand
                    .get_about()
                    .map(|about| Self::sentence(&about.to_string()))
                    .unwrap_or_default();
                self.page.item(
                    0,
                    &format!(
                        "[`{name}`](#{}): {}",
                        name.replace(' ', "-"),
                        Page::prose(&about)
                    ),
                );
            }
        }
        for subcommand in command
            .get_subcommands_mut()
            .filter(|subcommand| Self::listed(subcommand))
        {
            self.command(subcommand, false);
        }
    }

    /// Adds one argument or option: how to write it, what it does, its
    /// default and the values it accepts.
    fn argument(&mut self, argument: &Arg) {
        let syntax = match (argument.get_short(), argument.get_long()) {
            (Some(short), Some(_)) => format!("-{short}, {argument}"),
            _ => argument.to_string(),
        };
        let help = argument
            .get_long_help()
            .or(argument.get_help())
            .map(|help| Self::sentence(&help.to_string()))
            .unwrap_or_default();
        let mut text = format!("`{syntax}`: {}", Page::prose(&help));
        if argument.is_required_set() && !argument.is_positional() {
            text.push_str(" Required.");
        }
        let takes_values = argument.get_action().takes_values();
        let defaults: Vec<String> = argument
            .get_default_values()
            .iter()
            .map(|value| format!("`{}`", value.to_string_lossy()))
            .collect();
        if takes_values && !defaults.is_empty() {
            let _ = write!(text, " Default: {}.", defaults.join(", "));
        }
        let values: Vec<_> = argument
            .get_possible_values()
            .into_iter()
            .filter(|value| !value.is_hide_set())
            .collect();
        if takes_values
            && !values.is_empty()
            && values.iter().all(|value| value.get_help().is_none())
        {
            let names: Vec<String> = values
                .iter()
                .map(|value| format!("`{}`", value.get_name()))
                .collect();
            let _ = write!(text, " Values: {}.", names.join(", "));
        }
        self.page.item(0, &text);
        if takes_values && values.iter().any(|value| value.get_help().is_some()) {
            for value in values {
                let help = value
                    .get_help()
                    .map(|help| Self::sentence(&help.to_string()))
                    .unwrap_or_default();
                self.page.item(
                    1,
                    &format!("`{}`: {}", value.get_name(), Page::prose(&help)),
                );
            }
        }
    }

    /// Whether an argument is documented here: clap's own `--help` and
    /// `--version` are described once, in the introduction.
    fn documented(argument: &Arg) -> bool {
        !argument.is_hide_set()
            && !matches!(
                argument.get_action(),
                ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
            )
    }

    /// Whether a subcommand is listed: not hidden, and not clap's `help`
    /// subcommand, which prints `--help` for others.
    fn listed(subcommand: &Command) -> bool {
        !subcommand.is_hide_set() && subcommand.get_name() != "help"
    }

    /// Help text ending with a full stop. Clap drops the full stop of a
    /// one-sentence summary.
    fn sentence(help: &str) -> String {
        let help = help.trim();
        if help.is_empty() || help.ends_with(['.', '?', '!', ':']) {
            help.to_owned()
        } else {
            format!("{help}.")
        }
    }
}

fn main() -> Result<(), ArtifactError> {
    Artifacts::from_args()?.sync([(
        CliReference::PATH,
        CliReference::render(piquelctl::command()),
    )])
}
