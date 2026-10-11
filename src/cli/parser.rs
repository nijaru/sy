use super::args::{ParsedArguments, PathError};
use super::Cli;
use std::ffi::OsStr;

/// Public parser boundary. usage v6 currently discards values attached to
/// valueless switches; reject them rather than interpreting --delete=false as
/// deletion authorization. All flag vocabulary still belongs to the usage spec.
#[derive(Debug)]
pub struct Arguments(ParsedArguments);

impl Arguments {
    pub fn parse() -> Self {
        let argv: Vec<_> = std::env::args_os().skip(1).collect();
        let words: Vec<_> = argv.iter().map(|word| word.as_os_str()).collect();
        if !usage::is_spec_request(ParsedArguments::command(), &words)
            && usage::complete::CompletionRequest::parse(&argv).is_none()
        {
            if let Err(error) = reject_switch_values(&words) {
                eprint!("{}", usage::render_failure(Self::spec(), &words, &error));
                std::process::exit(2);
            }
        }
        Self(ParsedArguments::parse())
    }

    pub fn parse_from<'v>(words: &[&'v OsStr]) -> Result<Self, usage::Error<'static, 'v>> {
        reject_switch_values(words)?;
        ParsedArguments::parse_from(words).map(Self)
    }

    pub fn into_cli(self) -> Result<Cli, PathError> {
        self.0.into_cli()
    }

    pub fn spec() -> &'static usage::spec::Spec<'static> {
        ParsedArguments::spec()
    }

    pub fn to_kdl() -> String {
        ParsedArguments::to_kdl()
    }

    pub fn completion_script(shell: usage::complete::Shell) -> String {
        ParsedArguments::completion_script(shell)
    }
}

fn reject_switch_values<'v>(words: &[&'v OsStr]) -> Result<(), usage::Error<'static, 'v>> {
    let mut values = Vec::new();
    let mut action = None;
    // Let the real grammar identify values, including --filter's hyphen values
    // and paths after --. A textual scan alone would reject those incorrectly.
    let mut parser = usage::Parser::new(ParsedArguments::command(), words);
    while let Some(event) = parser.next_event() {
        match event {
            Ok(usage::Event::Arg { value, .. })
            | Ok(usage::Event::Flag {
                value: Some(value), ..
            }) => values.push(value.as_ptr()),
            Ok(usage::Event::Flag { flag, .. })
                if usage::is_help_flag(flag) || usage::is_version_flag(flag) =>
            {
                action = Some(flag);
                break;
            }
            Err(_) => break, // The derived parser owns its normal diagnostics.
            _ => {}
        }
    }
    for word in words {
        let token = word.as_encoded_bytes();
        if values.contains(&token.as_ptr()) {
            continue;
        }
        if let Some(body) = token.strip_prefix(b"--") {
            let name = body.split(|&byte| byte == b'=').next().unwrap_or(body);
            let flag = ParsedArguments::command()
                .flags
                .iter()
                .find(|flag| flag.longs.iter().any(|long| long.as_bytes() == name));
            let builtin = name == b"help" || name == b"version";
            if body.contains(&b'=') && (builtin || flag.is_some_and(|flag| !flag.takes_value)) {
                return Err(usage::Error::UnexpectedArg { token });
            }
            if action.is_some_and(|flag| flag.longs.iter().any(|long| long.as_bytes() == name)) {
                break;
            }
        } else if let Some(body) = token.strip_prefix(b"-") {
            if action.is_some_and(|flag| flag.shorts.iter().any(|short| body.contains(short))) {
                break;
            }
        }
    }
    Ok(())
}
