//! Environment variables for the flags of `graphrt serve` that turn on the
//! chat page, conversations and monitoring, so a container started without a
//! command line of its own (`docker run -e ...`, a compose file, a platform's
//! settings page) can use them:
//!
//! | Variable | Flag |
//! | --- | --- |
//! | `GRAPHRT_CHAT` (commas) | `--chat` |
//! | `GRAPHRT_CHAT_ACCESS` | `--chat-access` |
//! | `GRAPHRT_CHAT_TITLE` | `--chat-title` |
//! | `GRAPHRT_CHAT_GUEST_RATE_LIMIT` | `--chat-guest-rate-limit` |
//! | `GRAPHRT_CHAT_GUEST_BUDGET` | `--chat-guest-budget` |
//! | `GRAPHRT_CONVERSATIONS` | `--conversations` |
//! | `GRAPHRT_MONITOR` | `--monitor` |
//! | `GRAPHRT_DEVICES` | `--devices` |
//! | `GRAPHRT_APPROVALS` | `--approvals` |
//! | `GRAPHRT_EXPIRE_APPROVALS` | `--expire-approvals` |
//! | `GRAPHRT_PUSH` | `--push` |
//!
//! A flag on the command line wins over its variable. A switch is on when its
//! variable is `1`, `true`, `yes` or `on`, and off when it is `0`, `false`,
//! `no`, `off` or empty. (`--cors-origin`, `--public-url` and `--store` read
//! `GRAPHRT_CORS_ORIGINS`, `GRAPHRT_PUBLIC_URL` and `GRAPHRT_STORE` where they
//! are used.)

use crate::args::ServeArgs;
use crate::error::CliError;

/// Fills the flags that were not given from their environment variables.
///
/// @param args - the parsed flags of `graphrt serve` (&mut ServeArgs)
/// @param lookup - a variable of the process environment or of `--env-file`
/// @throws CliError (usage) - a variable whose value its flag would not take
#[cfg_attr(
    not(any(feature = "web-chat", feature = "conversations", feature = "devices")),
    allow(unused_variables, clippy::unnecessary_wraps)
)]
pub(super) fn apply(args: &mut ServeArgs, lookup: impl Fn(&str) -> Option<String>) -> Result<(), CliError> {
    let value = |name: &str| lookup(name).map(|text| text.trim().to_owned()).filter(|text| !text.is_empty());
    #[cfg(feature = "web-chat")]
    {
        use clap::ValueEnum as _;

        let chat = &mut args.chat;
        if chat.chat.is_empty()
            && let Some(list) = value("GRAPHRT_CHAT")
        {
            chat.chat = list.split(',').map(str::trim).filter(|spec| !spec.is_empty()).map(str::to_owned).collect();
        }
        if chat.chat_access.is_none()
            && let Some(who) = value("GRAPHRT_CHAT_ACCESS")
        {
            let access = crate::args::ChatAccess::from_str(&who, true)
                .map_err(|_| CliError::usage(format!("$GRAPHRT_CHAT_ACCESS `{who}`: expected key, users or guests")))?;
            chat.chat_access = Some(access);
        }
        if chat.chat_title.is_none() {
            chat.chat_title = value("GRAPHRT_CHAT_TITLE");
        }
        if chat.chat_guest_rate_limit.is_none() {
            chat.chat_guest_rate_limit = number(&value, "GRAPHRT_CHAT_GUEST_RATE_LIMIT")?;
        }
        if chat.chat_guest_budget.is_none() {
            chat.chat_guest_budget = number(&value, "GRAPHRT_CHAT_GUEST_BUDGET")?;
        }
    }
    #[cfg(feature = "conversations")]
    {
        args.conversations.conversations |= switch(&value, "GRAPHRT_CONVERSATIONS")?;
    }
    #[cfg(feature = "devices")]
    {
        let monitor = &mut args.monitor;
        #[cfg(feature = "web-monitor")]
        {
            monitor.monitor |= switch(&value, "GRAPHRT_MONITOR")?;
        }
        monitor.devices |= switch(&value, "GRAPHRT_DEVICES")?;
        #[cfg(feature = "approvals")]
        {
            monitor.approvals |= switch(&value, "GRAPHRT_APPROVALS")?;
            monitor.expire_approvals |= switch(&value, "GRAPHRT_EXPIRE_APPROVALS")?;
        }
        #[cfg(feature = "web-push")]
        if monitor.push.is_none() {
            monitor.push = value("GRAPHRT_PUSH");
        }
    }
    Ok(())
}

/// Whether the switch variable `name` is on.
#[cfg(any(feature = "conversations", feature = "devices"))]
fn switch(value: &impl Fn(&str) -> Option<String>, name: &str) -> Result<bool, CliError> {
    let Some(text) = value(name) else { return Ok(false) };
    match text.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(CliError::usage(format!("${name} `{text}`: expected 1 or 0 (also true/false, yes/no, on/off)"))),
    }
}

/// The whole number in the variable `name`, if it is set.
#[cfg(feature = "web-chat")]
fn number(value: &impl Fn(&str) -> Option<String>, name: &str) -> Result<Option<u32>, CliError> {
    value(name)
        .map(|text| text.parse().map_err(|_| CliError::usage(format!("${name} `{text}`: expected a whole number"))))
        .transpose()
}

#[cfg(test)]
#[cfg(all(feature = "web-chat", feature = "conversations", feature = "web-monitor", feature = "web-push"))]
#[allow(clippy::unwrap_used)]
mod tests {
    use clap::Parser as _;

    use super::*;
    use crate::args::{ChatAccess, Cli, Command};

    fn parsed(flags: &[&str]) -> ServeArgs {
        let line = ["graphrt", "serve", "flows"].iter().chain(flags).copied();
        match Cli::try_parse_from(line).unwrap().command {
            Command::Serve(args) => *args,
            _ => unreachable!("the line is a serve command"),
        }
    }

    fn applied(flags: &[&str], env: &[(&str, &str)]) -> Result<ServeArgs, CliError> {
        let mut args = parsed(flags);
        apply(&mut args, |name| env.iter().find(|(known, _)| *known == name).map(|(_, value)| (*value).to_owned()))?;
        Ok(args)
    }

    #[test]
    fn nothing_is_on_without_flags_or_variables() {
        let args = applied(&[], &[]).unwrap();
        assert!(args.chat.chat.is_empty() && args.chat.chat_access.is_none());
        assert!(!args.conversations.conversations);
        assert!(!args.monitor.monitor && !args.monitor.devices && !args.monitor.approvals);
        assert!(!args.monitor.expire_approvals && args.monitor.push.is_none());
    }

    #[test]
    fn variables_turn_on_what_the_flags_would() {
        let env = [
            ("GRAPHRT_CHAT", "support=message, sales"),
            ("GRAPHRT_CHAT_ACCESS", "guests"),
            ("GRAPHRT_CHAT_TITLE", "Ask us"),
            ("GRAPHRT_CHAT_GUEST_RATE_LIMIT", "4"),
            ("GRAPHRT_CHAT_GUEST_BUDGET", "40"),
            ("GRAPHRT_CONVERSATIONS", "true"),
            ("GRAPHRT_MONITOR", "1"),
            ("GRAPHRT_DEVICES", "yes"),
            ("GRAPHRT_APPROVALS", "on"),
            ("GRAPHRT_EXPIRE_APPROVALS", "TRUE"),
            ("GRAPHRT_PUSH", "mailto:owner@example.com"),
        ];
        let args = applied(&[], &env).unwrap();
        assert_eq!(args.chat.chat, ["support=message", "sales"]);
        assert_eq!(args.chat.access(), ChatAccess::Guests);
        assert_eq!(args.chat.chat_title.as_deref(), Some("Ask us"));
        assert_eq!((args.chat.chat_guest_rate_limit, args.chat.chat_guest_budget), (Some(4), Some(40)));
        assert!(args.conversations.conversations);
        assert!(args.monitor.monitor && args.monitor.devices && args.monitor.approvals);
        assert!(args.monitor.expire_approvals);
        assert_eq!(args.monitor.push.as_deref(), Some("mailto:owner@example.com"));
    }

    #[test]
    fn a_flag_wins_over_its_variable() {
        let env = [
            ("GRAPHRT_CHAT", "sales"),
            ("GRAPHRT_CHAT_ACCESS", "guests"),
            ("GRAPHRT_CHAT_GUEST_BUDGET", "40"),
            ("GRAPHRT_PUSH", "mailto:env@example.com"),
        ];
        let flags = [
            "--chat",
            "support",
            "--chat-access",
            "key",
            "--chat-guest-budget",
            "7",
            "--push",
            "mailto:flag@example.com",
        ];
        let args = applied(&flags, &env).unwrap();
        assert_eq!(args.chat.chat, ["support"]);
        assert_eq!(args.chat.access(), ChatAccess::Key);
        assert_eq!(args.chat.chat_guest_budget, Some(7));
        assert_eq!(args.monitor.push.as_deref(), Some("mailto:flag@example.com"));
    }

    #[test]
    fn off_values_and_empty_variables_leave_a_switch_off() {
        let env = [("GRAPHRT_MONITOR", "0"), ("GRAPHRT_DEVICES", "off"), ("GRAPHRT_CONVERSATIONS", " ")];
        let args = applied(&[], &env).unwrap();
        assert!(!args.monitor.monitor && !args.monitor.devices && !args.conversations.conversations);
        let args = applied(&["--monitor"], &[("GRAPHRT_MONITOR", "0")]).unwrap();
        assert!(args.monitor.monitor, "the variable never turns off what the flag turned on");
    }

    #[test]
    fn values_a_flag_would_refuse_are_usage_errors_naming_the_variable() {
        for (name, value) in
            [("GRAPHRT_MONITOR", "maybe"), ("GRAPHRT_CHAT_ACCESS", "everyone"), ("GRAPHRT_CHAT_GUEST_BUDGET", "many")]
        {
            let error = applied(&[], &[(name, value)]).unwrap_err();
            assert!(error.message.contains(name) && error.message.contains(value), "{}", error.message);
        }
    }
}
