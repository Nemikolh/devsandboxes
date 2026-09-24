//! Declarative table of the prompt's command grammar. Both the parser and the
//! tab completer read this one table, so a new flag or command cannot drift
//! between the two (the motivating bug: `rebuild --force` parsed but never
//! completed). I/O-free and unit-tested, like the rest of the prompt logic.

/// What completes (and what is expected) in an argument position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgValue {
    /// Instance names from the snapshot.
    Instance,
    /// Instance names; service names on the Services tab.
    InstanceOrService,
    /// Service names from the snapshot (tab-independent).
    Service,
    /// Sandbox config names.
    Sandbox,
    /// The branch `run` would use (worktree-branch/default).
    Branch,
    /// Free-form: parses fine, no candidates.
    Free,
}

#[derive(Debug)]
pub struct FlagSpec {
    /// Flag token as typed, e.g. `"--force"`.
    pub name: &'static str,
    /// `Some` = the flag consumes the next token as its value; `None` = boolean.
    pub value: Option<ArgValue>,
}

#[derive(Debug)]
pub struct CommandSpec {
    pub name: &'static str,
    /// Alternate spellings that parse but don't complete (mirrors the CLI's
    /// `rebuild`/`recreate` aliasing).
    pub aliases: &'static [&'static str],
    /// Shown as `usage: <this>` when required arguments are missing.
    pub usage: &'static str,
    pub flags: &'static [FlagSpec],
    /// Required positional arguments, in order.
    pub positionals: &'static [ArgValue],
    /// After the positionals, the rest of the line is verbatim argv (`exec`);
    /// at least one trailing token is required.
    pub trailing: bool,
}

/// Every prompt command, in first-token completion display order.
pub const SPECS: &[CommandSpec] = &[
    CommandSpec {
        name: "run",
        aliases: &[],
        usage: "run <sandbox> [--name n] [--branch b] [--base ref]",
        flags: &[
            FlagSpec { name: "--name", value: Some(ArgValue::Free) },
            FlagSpec { name: "--branch", value: Some(ArgValue::Branch) },
            FlagSpec { name: "--base", value: Some(ArgValue::Free) },
        ],
        positionals: &[ArgValue::Sandbox],
        trailing: false,
    },
    CommandSpec {
        name: "exec",
        aliases: &[],
        usage: "exec <instance> <cmd…>",
        flags: &[],
        positionals: &[ArgValue::Instance],
        trailing: true,
    },
    CommandSpec {
        name: "code",
        aliases: &[],
        usage: "code <instance>",
        flags: &[],
        positionals: &[ArgValue::Instance],
        trailing: false,
    },
    CommandSpec {
        name: "rm",
        aliases: &[],
        usage: "rm <instance>",
        flags: &[],
        positionals: &[ArgValue::Instance],
        trailing: false,
    },
    CommandSpec {
        name: "rename",
        aliases: &[],
        usage: "rename <instance> <new-name>",
        flags: &[],
        positionals: &[ArgValue::Instance, ArgValue::Free],
        trailing: false,
    },
    CommandSpec {
        name: "stop",
        aliases: &[],
        usage: "stop <instance>",
        flags: &[],
        positionals: &[ArgValue::Instance],
        trailing: false,
    },
    CommandSpec {
        name: "start",
        aliases: &[],
        usage: "start <instance>",
        flags: &[],
        positionals: &[ArgValue::Instance],
        trailing: false,
    },
    CommandSpec {
        name: "rebuild",
        aliases: &["recreate"],
        usage: "rebuild [--force] <instance>",
        flags: &[FlagSpec { name: "--force", value: None }],
        positionals: &[ArgValue::InstanceOrService],
        trailing: false,
    },
    CommandSpec {
        name: "port",
        aliases: &[],
        usage: "port <instance> [--service s] [--address a] <[host:]port>",
        flags: &[
            FlagSpec { name: "--service", value: Some(ArgValue::Service) },
            FlagSpec { name: "--address", value: Some(ArgValue::Free) },
        ],
        positionals: &[ArgValue::Instance, ArgValue::Free],
        trailing: false,
    },
];

/// Look up a command by primary name or alias.
pub fn find(cmd: &str) -> Option<&'static CommandSpec> {
    SPECS.iter().find(|s| s.name == cmd || s.aliases.contains(&cmd))
}

/// The outcome of [`parse_args`]: which flags were set, in a shape the
/// per-command `PromptAction` mapping can consume without re-tokenizing.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParsedArgs {
    /// Boolean flags present on the line.
    pub flags: Vec<&'static str>,
    /// Value-taking flags with their values; repeats keep every occurrence,
    /// [`Self::value`] returns the last (last-one-wins, like the old parser).
    pub values: Vec<(&'static str, String)>,
    /// Positional arguments, in spec order. Always exactly `spec.positionals.len()`.
    pub positionals: Vec<String>,
    /// Verbatim argv after the positionals (only for `trailing` specs).
    pub trailing: Vec<String>,
}

impl ParsedArgs {
    /// Was the boolean flag `name` set?
    pub fn flag(&self, name: &str) -> bool {
        self.flags.contains(&name)
    }

    /// Last value given for the value-taking flag `name`, if any.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.iter().rev().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
    }
}

/// Generic token walk over `spec`'s grammar. Flags are position-independent;
/// only `--`-prefixed tokens count as flags (so `exec box ls -la` keeps `-la`),
/// and once a trailing spec's positionals are filled the rest of the line is
/// captured verbatim, flags included. Errors mirror the hand-rolled parser's
/// wording so the step-2 rewire changes no user-visible messages.
pub fn parse_args(spec: &CommandSpec, tokens: &[&str]) -> Result<ParsedArgs, String> {
    let mut out = ParsedArgs::default();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        if spec.trailing && out.positionals.len() == spec.positionals.len() {
            out.trailing.extend(tokens[i..].iter().map(|s| (*s).to_string()));
            break;
        }
        if tok.starts_with("--") {
            let Some(flag) = spec.flags.iter().find(|f| f.name == tok) else {
                return Err(format!("unknown flag `{tok}`"));
            };
            match flag.value {
                Some(_) => {
                    let val = tokens
                        .get(i + 1)
                        .ok_or_else(|| format!("`{}` needs a value", flag.name))?;
                    out.values.push((flag.name, (*val).to_string()));
                    i += 2;
                    continue;
                }
                None => out.flags.push(flag.name),
            }
        } else {
            if out.positionals.len() == spec.positionals.len() {
                return Err(format!("unexpected argument `{tok}`"));
            }
            out.positionals.push(tok.to_string());
        }
        i += 1;
    }
    if out.positionals.len() < spec.positionals.len() || (spec.trailing && out.trailing.is_empty())
    {
        return Err(format!("usage: {}", spec.usage));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `line` through the spec table, like `parse_line` will in step 2.
    fn parse(line: &str) -> Result<ParsedArgs, String> {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let (cmd, rest) = tokens.split_first().expect("test lines have a command");
        parse_args(find(cmd).expect("test lines use known commands"), rest)
    }

    #[test]
    fn find_resolves_names_and_aliases() {
        assert_eq!(find("run").map(|s| s.name), Some("run"));
        assert_eq!(find("recreate").map(|s| s.name), Some("rebuild"));
        assert_eq!(find("rebuild").map(|s| s.name), Some("rebuild"));
        assert!(find("frobnicate").is_none());
        assert!(find("").is_none());
    }

    #[test]
    fn parse_run_variants() {
        let args = parse("run web").unwrap();
        assert_eq!(args.positionals, vec!["web".to_string()]);
        assert_eq!(args.value("--name"), None);
        assert_eq!(args.value("--branch"), None);

        let args = parse("run web --name api").unwrap();
        assert_eq!(args.positionals, vec!["web".to_string()]);
        assert_eq!(args.value("--name"), Some("api"));

        let args = parse("run web --branch feat/x").unwrap();
        assert_eq!(args.value("--branch"), Some("feat/x"));

        // Flags are position-independent.
        let args = parse("run --name api web").unwrap();
        assert_eq!(args.positionals, vec!["web".to_string()]);
        assert_eq!(args.value("--name"), Some("api"));

        assert_eq!(parse("run"), Err("usage: run <sandbox> [--name n] [--branch b] [--base ref]".into()));
        assert_eq!(parse("run web --name"), Err("`--name` needs a value".into()));
        assert_eq!(parse("run web --branch"), Err("`--branch` needs a value".into()));
        assert_eq!(parse("run web --bogus"), Err("unknown flag `--bogus`".into()));
        assert_eq!(parse("run a b"), Err("unexpected argument `b`".into()));
    }

    #[test]
    fn parse_run_repeated_flag_last_wins() {
        let args = parse("run web --name a --name b").unwrap();
        assert_eq!(args.value("--name"), Some("b"));
    }

    #[test]
    fn parse_exec_requires_command() {
        let args = parse("exec box ls -la").unwrap();
        assert_eq!(args.positionals, vec!["box".to_string()]);
        assert_eq!(args.trailing, vec!["ls".to_string(), "-la".to_string()]);

        assert_eq!(parse("exec box"), Err("usage: exec <instance> <cmd…>".into()));
        assert_eq!(parse("exec"), Err("usage: exec <instance> <cmd…>".into()));
    }

    #[test]
    fn parse_exec_trailing_keeps_flaglike_tokens() {
        // Once trailing capture starts, `--`-prefixed tokens are argv, not flags.
        let args = parse("exec box git log --oneline").unwrap();
        assert_eq!(
            args.trailing,
            vec!["git".to_string(), "log".to_string(), "--oneline".to_string()]
        );
    }

    #[test]
    fn parse_code_and_rm() {
        assert_eq!(parse("code box").unwrap().positionals, vec!["box".to_string()]);
        assert_eq!(parse("rm box").unwrap().positionals, vec!["box".to_string()]);
        assert_eq!(parse("code"), Err("usage: code <instance>".into()));
        assert_eq!(parse("code a b"), Err("unexpected argument `b`".into()));
        assert_eq!(parse("rm"), Err("usage: rm <instance>".into()));
    }

    #[test]
    fn parse_rename() {
        let args = parse("rename old new").unwrap();
        assert_eq!(args.positionals, vec!["old".to_string(), "new".to_string()]);
        assert_eq!(parse("rename"), Err("usage: rename <instance> <new-name>".into()));
        assert_eq!(parse("rename old"), Err("usage: rename <instance> <new-name>".into()));
        assert_eq!(parse("rename old new extra"), Err("unexpected argument `extra`".into()));
    }

    #[test]
    fn parse_stop() {
        assert_eq!(parse("stop box").unwrap().positionals, vec!["box".to_string()]);
        assert_eq!(parse("stop"), Err("usage: stop <instance>".into()));
        assert!(parse("stop a b").is_err());
    }

    #[test]
    fn parse_start() {
        assert_eq!(parse("start box").unwrap().positionals, vec!["box".to_string()]);
        assert_eq!(parse("start"), Err("usage: start <instance>".into()));
        assert!(parse("start a b").is_err());
    }

    #[test]
    fn parse_rebuild_and_recreate_alias() {
        let args = parse("rebuild box").unwrap();
        assert_eq!(args.positionals, vec!["box".to_string()]);
        assert!(!args.flag("--force"));

        // Alias resolves to the same spec, so the grammar is identical.
        let args = parse("recreate box").unwrap();
        assert_eq!(args.positionals, vec!["box".to_string()]);

        // `--force` is position-independent, like the CLI flag.
        assert!(parse("rebuild --force box").unwrap().flag("--force"));
        assert!(parse("rebuild box --force").unwrap().flag("--force"));

        assert_eq!(parse("rebuild"), Err("usage: rebuild [--force] <instance>".into()));
        assert_eq!(parse("rebuild --force"), Err("usage: rebuild [--force] <instance>".into()));
        assert_eq!(parse("rebuild --bogus box"), Err("unknown flag `--bogus`".into()));
        assert!(parse("rebuild a b").is_err());
        assert!(parse("recreate").is_err());
    }

    #[test]
    fn parse_empty_tokens_wants_usage() {
        let spec = find("rm").unwrap();
        assert_eq!(parse_args(spec, &[]), Err("usage: rm <instance>".into()));
    }

    #[test]
    fn specs_cover_all_commands_in_display_order() {
        let names: Vec<&str> = SPECS.iter().map(|s| s.name).collect();
        assert_eq!(names, ["run", "exec", "code", "rm", "rename", "stop", "start", "rebuild", "port"]);
    }
}
