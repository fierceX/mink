#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SlashCommand {
    Flash,
    Pro,
    Model(String),
    Compact,
    Help,
    Skills,
    Plan,
    Todos,
    Inputs,
    Resume(String),
    Withdraw(String),
    SubAgent(String),
    Artifact(String),
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SlashCommandError {
    pub input: String,
}

pub(crate) fn parse_slash_command(input: &str) -> Result<Option<SlashCommand>, SlashCommandError> {
    if !input.starts_with('/') {
        return Ok(None);
    }

    let command = match input {
        "/flash" => SlashCommand::Flash,
        "/pro" => SlashCommand::Pro,
        "/compact" => SlashCommand::Compact,
        "/help" => SlashCommand::Help,
        "/skills" => SlashCommand::Skills,
        "/plan" => SlashCommand::Plan,
        "/todos" => SlashCommand::Todos,
        "/inputs" => SlashCommand::Inputs,
        _ if input.starts_with("/resume ") && !input[8..].trim().is_empty() => {
            SlashCommand::Resume(input[8..].trim().to_string())
        }
        _ if input.starts_with("/withdraw ") && !input[10..].trim().is_empty() => {
            SlashCommand::Withdraw(input[10..].trim().to_string())
        }
        _ if input.starts_with("/sub-agent ")
            && !input["/sub-agent ".len()..].trim().is_empty() =>
        {
            SlashCommand::SubAgent(input["/sub-agent ".len()..].trim().to_string())
        }
        _ if input.starts_with("/artifact ") && !input["/artifact ".len()..].trim().is_empty() => {
            SlashCommand::Artifact(input["/artifact ".len()..].trim().to_string())
        }
        "/exit" | "/quit" | "/q" => SlashCommand::Quit,
        _ if input.starts_with("/model ") && !input["/model ".len()..].trim().is_empty() => {
            SlashCommand::Model(input["/model ".len()..].trim().to_string())
        }
        _ => {
            return Err(SlashCommandError {
                input: input.to_string(),
            });
        }
    };

    Ok(Some(command))
}
