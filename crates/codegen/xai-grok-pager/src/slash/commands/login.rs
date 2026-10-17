use crate::app::actions::Action;
use crate::slash::command::{AppCtx, CommandExecCtx, CommandResult, SlashCommand, slash_meta};
use xai_grok_config::{Capability, Distribution};

pub struct LoginCommand;

impl SlashCommand for LoginCommand {
    slash_meta! {
        name: "login",
        description: "Log in or re-authenticate (Grok or Cursor)",
        usage: "/login [cursor]",
    }

    /// Not offered where the build has no account (unless the Cursor Agent CLI can log in); the router refuses a typed one.
    fn visible(&self, _ctx: &AppCtx) -> bool {
        Distribution::current().allows(Capability::AccountLogin)
            || xai_grok_shell::agent::cursor_cli::cursor_agent_available()
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let provider = args.trim().to_ascii_lowercase();
        if provider.is_empty() {
            // On a Cursor-billed model, bare `/login` means Cursor login so
            // new users who picked `cursor/auto` are not sent through grok.com.
            let on_cursor = ctx
                .models
                .current_model_id_str()
                .is_some_and(xai_grok_shell::agent::cursor_cli::is_cursor_billed_model);
            if on_cursor {
                CommandResult::Action(Action::LoginCursor)
            } else {
                CommandResult::Action(Action::Login)
            }
        } else if provider == "cursor" || provider == "cursor.cli" {
            CommandResult::Action(Action::LoginCursor)
        } else {
            CommandResult::Message(format!(
                "Unknown login provider '{provider}'. Use `/login` or `/login cursor`."
            ))
        }
    }
}
