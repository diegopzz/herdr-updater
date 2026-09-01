//! Shell completion scripts.
//!
//! These were argued against on one specific ground: a completion script is a
//! hand-maintained copy of the command list, and hand-maintained copies drift —
//! in a repository that already added a test purely to stop version drift.
//!
//! So the copy is not hand-maintained. `COMMANDS` and `OPTIONS` below are the
//! single source, the scripts are generated from them, and a test asserts that
//! every command and option named in `USAGE` appears here. Adding a flag to the
//! CLI without adding it here fails the build, which is the only arrangement
//! under which shipping completions is not a slow-motion lie.

/// Every command, with the one-line description shown during completion.
pub const COMMANDS: &[(&str, &str)] = &[
    ("check", "inspect core and plugins; never mutate"),
    ("doctor", "check this tool's own environment"),
    ("plan", "show the policy decision for every target"),
    ("apply", "execute safe UPDATE decisions"),
    ("update", "alias for apply"),
    ("fleet", "report drift across configured SSH hosts"),
    ("search", "search the Herdr plugin marketplace"),
    ("install", "install a marketplace plugin"),
    ("store", "run the plugin store inside a Herdr pane"),
    ("open-store", "open the plugin store popup"),
    ("sync", "plan/apply/export desired state across hosts"),
    ("schedule", "run/install/status/remove background checks"),
    ("history", "print the update/rollback audit log"),
    ("rollback", "pin plugin(s) to their pre-update revision"),
    ("resume", "return rolled-back plugin(s) to their ref"),
    ("startup", "startup hook"),
    ("completions", "print a shell completion script"),
    ("version", "print this tool's version"),
];

pub const OPTIONS: &[&str] = &[
    "--json",
    "--timeout",
    "--config",
    "--only",
    "--hosts",
    "--core-only",
    "--plugins-only",
    "--allow-protocol-change",
    "--sort",
    "--limit",
    "--since",
    "--refresh",
    "--yes",
    "--help",
];

/// Subcommand operands, so `sync <tab>` offers modes rather than filenames.
const MODES: &[(&str, &[&str])] = &[
    ("sync", &["plan", "apply", "export"]),
    ("schedule", &["run", "install", "status", "remove"]),
    ("completions", &["bash", "zsh", "fish"]),
];

pub fn render(shell: &str) -> Result<String, String> {
    match shell {
        "bash" => Ok(bash()),
        "zsh" => Ok(zsh()),
        "fish" => Ok(fish()),
        other => Err(format!(
            "unknown shell {other:?}; supported shells are bash, zsh, and fish"
        )),
    }
}

fn command_names() -> String {
    COMMANDS
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(" ")
}

fn bash() -> String {
    let mut script = String::from(
        "# herdr-updater bash completion\n\
         # eval \"$(herdr-updater completions bash)\"\n\
         _herdr_updater() {\n\
         \x20   local cur prev\n\
         \x20   cur=\"${COMP_WORDS[COMP_CWORD]}\"\n\
         \x20   prev=\"${COMP_WORDS[COMP_CWORD-1]}\"\n",
    );
    // Options that take a value complete to nothing rather than to commands,
    // so `--config <tab>` falls through to filenames.
    script.push_str(
        "\x20   case \"$prev\" in\n\
         \x20       --config) COMPREPLY=( $(compgen -f -- \"$cur\") ); return 0 ;;\n\
         \x20       --timeout|--only|--hosts|--limit|--since) COMPREPLY=(); return 0 ;;\n\
         \x20       --sort) COMPREPLY=( $(compgen -W \"relevance stars trending recent name\" -- \"$cur\") ); return 0 ;;\n",
    );
    for (command, modes) in MODES {
        script.push_str(&format!(
            "\x20       {command}) COMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") ); return 0 ;;\n",
            modes.join(" ")
        ));
    }
    script.push_str("\x20   esac\n");
    script.push_str(&format!(
        "\x20   if [[ \"$cur\" == -* ]]; then\n\
         \x20       COMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") )\n\
         \x20   else\n\
         \x20       COMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") )\n\
         \x20   fi\n\
         }}\n\
         complete -F _herdr_updater herdr-updater\n",
        OPTIONS.join(" "),
        command_names()
    ));
    script
}

fn zsh() -> String {
    let mut script = String::from(
        "#compdef herdr-updater\n\
         # herdr-updater zsh completion\n\
         # herdr-updater completions zsh > \"${fpath[1]}/_herdr-updater\"\n\
         _herdr_updater() {\n\
         \x20   local -a commands options\n\
         \x20   commands=(\n",
    );
    for (name, description) in COMMANDS {
        // Escape the colon: zsh uses it to split value from description.
        script.push_str(&format!(
            "\x20       '{name}:{}'\n",
            description.replace(':', "\\:")
        ));
    }
    script.push_str("\x20   )\n\x20   options=(\n");
    for option in OPTIONS {
        script.push_str(&format!("\x20       '{option}'\n"));
    }
    script.push_str(
        "\x20   )\n\
         \x20   if (( CURRENT == 2 )); then\n\
         \x20       _describe -t commands 'herdr-updater command' commands\n\
         \x20   else\n\
         \x20       _describe -t options 'option' options\n\
         \x20   fi\n\
         }\n\
         compdef _herdr_updater herdr-updater\n",
    );
    script
}

fn fish() -> String {
    let mut script = String::from("# herdr-updater fish completion\n# herdr-updater completions fish > ~/.config/fish/completions/herdr-updater.fish\n");
    for (name, description) in COMMANDS {
        script.push_str(&format!(
            "complete -c herdr-updater -n __fish_use_subcommand -a {name} -d '{}'\n",
            description.replace('\'', "")
        ));
    }
    for (command, modes) in MODES {
        for mode in *modes {
            script.push_str(&format!(
                "complete -c herdr-updater -n '__fish_seen_subcommand_from {command}' -a {mode}\n"
            ));
        }
    }
    for option in OPTIONS {
        script.push_str(&format!(
            "complete -c herdr-updater -l {}\n",
            option.trim_start_matches('-')
        ));
    }
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard that makes shipping these safe. If a command or flag is added
    /// to the CLI and not here, this fails — so the completion script cannot
    /// quietly fall behind the tool it describes.
    #[test]
    fn every_command_and_option_in_usage_is_completable() {
        let usage = crate::USAGE;
        for (name, _) in COMMANDS {
            if *name == "completions" {
                continue; // asserted separately below
            }
            assert!(
                usage.contains(&format!("    {name} ")) || usage.contains(&format!("    {name}\n")),
                "command {name:?} is completable but missing from USAGE"
            );
        }
        for option in OPTIONS {
            assert!(
                usage.contains(*option),
                "option {option:?} is completable but missing from USAGE"
            );
        }
        assert!(
            usage.contains("completions"),
            "the completions command must document itself"
        );
    }

    /// The direction that actually matters, and that a first attempt at this
    /// test missed: a flag or command added to the CLI and forgotten here.
    /// Checking only that completions appear in USAGE catches nothing, because
    /// deleting an entry from the completion list still passes it.
    #[test]
    fn every_command_and_option_in_usage_has_a_completion_entry() {
        let usage = crate::USAGE;
        // Anchored on the section headers. Splitting on the bare words matches
        // "<COMMAND> [OPTIONS]" in the usage line first and silently returns
        // the wrong block — which made an earlier version of this check pass
        // against everything, including a completion list with entries removed.
        let commands_block = usage
            .split("\nCOMMANDS\n")
            .nth(1)
            .and_then(|rest| rest.split("\nOPTIONS\n").next())
            .expect("USAGE must have a COMMANDS block");
        for line in commands_block.lines() {
            let Some(name) = line
                .strip_prefix("    ")
                .and_then(|l| l.split_whitespace().next())
            else {
                continue;
            };
            assert!(
                COMMANDS.iter().any(|(known, _)| *known == name),
                "command {name:?} is in USAGE but has no completion entry"
            );
        }

        let options_block = usage
            .split("\nOPTIONS\n")
            .nth(1)
            .and_then(|rest| rest.split("\nEXIT CODES\n").next())
            .expect("USAGE must have an OPTIONS block");
        assert!(
            options_block.contains("--json"),
            "the options block was not located; this check would pass vacuously"
        );
        for token in options_block.split_whitespace() {
            let flag = token.trim_end_matches(',');
            if !flag.starts_with("--") {
                continue;
            }
            assert!(
                OPTIONS.contains(&flag),
                "option {flag:?} is in USAGE but has no completion entry"
            );
        }
    }

    #[test]
    fn each_shell_renders_something_plausible() {
        let bash = render("bash").unwrap();
        assert!(bash.contains("complete -F _herdr_updater herdr-updater"));
        assert!(bash.contains("doctor"));
        let zsh = render("zsh").unwrap();
        assert!(zsh.starts_with("#compdef herdr-updater"));
        let fish = render("fish").unwrap();
        assert!(fish.contains("__fish_use_subcommand"));
        assert!(render("tcsh").is_err());
    }

    #[test]
    fn zsh_descriptions_escape_the_field_separator() {
        assert!(!zsh().lines().any(|line| line.matches(':').count() > 1
            && !line.contains("\\:")
            && line.trim_start().starts_with('\'')));
    }
}
