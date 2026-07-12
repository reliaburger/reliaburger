//! Parsing for the navigation-only `:` command palette.

/// Editable command palette state.
#[derive(Debug, Clone, Default)]
pub struct PaletteState {
    /// Text after the leading colon.
    pub input: String,
}

/// A parsed palette command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteCommand {
    Apps,
    Nodes,
    Jobs,
    Events,
    Logs(Option<String>),
    Routes,
    Search,
    Help,
    Quit,
}

/// Parse one navigation command.
pub fn parse_command(input: &str) -> Result<PaletteCommand, String> {
    let input = input.trim().trim_start_matches(':');
    let mut words = input.split_whitespace();
    let command = words.next().unwrap_or_default();
    let argument = words.next().map(str::to_string);
    if words.next().is_some() {
        return Err("command takes at most one argument".to_string());
    }
    match command {
        "apps" | "a" => Ok(PaletteCommand::Apps),
        "nodes" | "n" => Ok(PaletteCommand::Nodes),
        "jobs" | "j" => Ok(PaletteCommand::Jobs),
        "events" | "e" => Ok(PaletteCommand::Events),
        "logs" | "l" => Ok(PaletteCommand::Logs(argument)),
        "routes" | "r" => Ok(PaletteCommand::Routes),
        "search" | "s" => Ok(PaletteCommand::Search),
        "help" | "?" => Ok(PaletteCommand::Help),
        "quit" | "q" => Ok(PaletteCommand::Quit),
        _ => Err(format!("unknown command: {command}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_accepts_optional_app() {
        assert_eq!(
            parse_command(":logs web"),
            Ok(PaletteCommand::Logs(Some("web".to_string())))
        );
    }

    #[test]
    fn unknown_command_is_an_error() {
        assert_eq!(
            parse_command(":wat"),
            Err("unknown command: wat".to_string())
        );
    }
}
