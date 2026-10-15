//! External control through URLs: `com.brettinternet.mic-mute://mute`, `unmute`, `toggle`, and
//! `status`. The app bundle registers the scheme (`url_schemes` in `Cargo.toml`), and tao
//! delivers the URLs macOS hands the app as `Event::Opened`.
//!
//! Any app, and any web page through the browser, can open such a URL. So only actions that
//! can't open a microphone run by default; `unmute` and `toggle` need the `allow_url_unmute`
//! setting.

use crate::mic_command::MicCommand;
use log::warn;
use std::collections::VecDeque;
use std::fmt;

/// The scheme the bundle registers.
pub const URL_SCHEME: &str = "com.brettinternet.mic-mute";
/// Actions kept until the loop runs them. Only a launch by URL delivers any before the loop can
/// act; later ones run in the same pass.
const MAX_QUEUED_ACTIONS: usize = 16;
/// The most characters of an unknown action name the log shows.
const MAX_LOGGED_NAME_CHARS: usize = 32;

/// What a URL asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlAction {
    Mute,
    Unmute,
    Toggle,
    /// Shows the current state in the popup: a URL can't return a value.
    Status,
}

impl UrlAction {
    const ALL: [Self; 4] = [Self::Mute, Self::Unmute, Self::Toggle, Self::Status];

    /// The action's name in the URL: `mute` in `com.brettinternet.mic-mute://mute`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Mute => "mute",
            Self::Unmute => "unmute",
            Self::Toggle => "toggle",
            Self::Status => "status",
        }
    }

    /// Whether running the action can unmute a microphone.
    pub fn can_unmute(self) -> bool {
        matches!(self, Self::Unmute | Self::Toggle)
    }

    /// Whether the action may run: one that can unmute needs `allow_url_unmute`.
    pub fn is_allowed(self, allow_url_unmute: bool) -> bool {
        allow_url_unmute || !self.can_unmute()
    }

    /// The change to the microphones; `None` for `status`, which changes nothing.
    pub fn command(self) -> Option<MicCommand> {
        match self {
            Self::Mute => Some(MicCommand::Mute),
            Self::Unmute => Some(MicCommand::Unmute),
            Self::Toggle => Some(MicCommand::Toggle),
            Self::Status => None,
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| action.name().eq_ignore_ascii_case(name))
    }
}

/// Why a URL was ignored. None of them holds the URL's query or fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlRejection {
    /// Another scheme, such as a web page handed over from another device.
    OtherScheme,
    /// Not `com.brettinternet.mic-mute://<action>`: no action, a path after it, a user name,
    /// or a port.
    Malformed,
    /// An action this version doesn't know, shortened for the log.
    UnknownAction(String),
}

impl fmt::Display for UrlRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OtherScheme => write!(f, "it isn't a {URL_SCHEME}: URL"),
            Self::Malformed => write!(f, "it isn't of the form {URL_SCHEME}://<action>"),
            Self::UnknownAction(name) => write!(
                f,
                "unknown action {name:?} (use mute, unmute, toggle, or status)"
            ),
        }
    }
}

/// Parses `url`, the text of a URL macOS asked the app to open:
/// `com.brettinternet.mic-mute://<action>`, optionally with a trailing `/`, in any letter case.
/// A query or fragment is ignored.
pub fn parse_url(url: &str) -> Result<UrlAction, UrlRejection> {
    let (scheme, rest) = url.split_once(':').ok_or(UrlRejection::OtherScheme)?;
    if !scheme.eq_ignore_ascii_case(URL_SCHEME) {
        return Err(UrlRejection::OtherScheme);
    }
    let rest = rest.strip_prefix("//").ok_or(UrlRejection::Malformed)?;
    // `split` yields at least one piece, so the default never applies.
    let target = rest.split(['?', '#']).next().unwrap_or_default();
    let (name, path) = target.split_once('/').unwrap_or((target, ""));
    if name.is_empty() || !path.is_empty() || name.contains(['@', ':']) {
        return Err(UrlRejection::Malformed);
    }
    UrlAction::from_name(name).ok_or_else(|| {
        UrlRejection::UnknownAction(name.chars().take(MAX_LOGGED_NAME_CHARS).collect())
    })
}

/// URL actions waiting for the event loop, in the order their URLs arrived.
#[derive(Debug, Default)]
pub struct UrlActions {
    queued: VecDeque<UrlAction>,
}

impl UrlActions {
    /// Queues the action of each URL. Logs and drops URLs that name none, and any action beyond
    /// [`MAX_QUEUED_ACTIONS`]. The log never shows a URL's query or fragment.
    pub fn push_urls<'a>(&mut self, urls: impl IntoIterator<Item = &'a str>) {
        for url in urls {
            match parse_url(url) {
                Ok(action) if self.queued.len() < MAX_QUEUED_ACTIONS => {
                    self.queued.push_back(action);
                }
                Ok(action) => warn!(
                    "Ignoring the {} URL action: {} URL actions are already waiting",
                    action.name(),
                    MAX_QUEUED_ACTIONS
                ),
                Err(rejection) => warn!("Ignoring a URL: {}", rejection),
            }
        }
    }

    /// The oldest waiting action.
    pub fn pop(&mut self) -> Option<UrlAction> {
        self.queued.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(action: &str) -> String {
        format!("{URL_SCHEME}://{action}")
    }

    #[test]
    fn every_action_parses_from_its_name() {
        for action in UrlAction::ALL {
            assert_eq!(parse_url(&url(action.name())), Ok(action), "{action:?}");
        }
    }

    #[test]
    fn scheme_and_action_are_case_insensitive() {
        assert_eq!(
            parse_url("COM.BrettInternet.Mic-Mute://Mute"),
            Ok(UrlAction::Mute)
        );
        assert_eq!(parse_url(&url("STATUS")), Ok(UrlAction::Status));
    }

    #[test]
    fn a_trailing_slash_query_or_fragment_is_ignored() {
        for (text, action) in [
            ("mute/", UrlAction::Mute),
            ("unmute?source=raycast", UrlAction::Unmute),
            ("toggle/?a=1&b=2", UrlAction::Toggle),
            ("status#now", UrlAction::Status),
        ] {
            assert_eq!(parse_url(&url(text)), Ok(action), "{text}");
        }
    }

    #[test]
    fn other_schemes_are_rejected() {
        for text in ["https://example.com/mute", "file:///tmp/mute", "mute", ""] {
            assert_eq!(parse_url(text), Err(UrlRejection::OtherScheme), "{text}");
        }
    }

    #[test]
    fn urls_without_exactly_one_action_are_malformed() {
        let scheme = URL_SCHEME;
        for text in [
            format!("{scheme}:mute"),
            format!("{scheme}:/mute"),
            format!("{scheme}://"),
            format!("{scheme}:///mute"),
            format!("{scheme}://?mute"),
            format!("{scheme}://mute/now"),
            format!("{scheme}://mute//"),
            format!("{scheme}://user@mute"),
            format!("{scheme}://user:secret@mute"),
            format!("{scheme}://mute:80"),
        ] {
            assert_eq!(parse_url(&text), Err(UrlRejection::Malformed), "{text}");
        }
    }

    #[test]
    fn unknown_actions_are_named_without_the_query() {
        let rejection = parse_url(&url("unmte?token=secret")).unwrap_err();
        assert_eq!(rejection, UrlRejection::UnknownAction("unmte".to_string()));
        let text = rejection.to_string();
        assert!(text.contains("\"unmte\""), "{text}");
        assert!(!text.contains("secret"), "{text}");
    }

    #[test]
    fn long_unknown_action_names_are_shortened() {
        let long = "x".repeat(200);
        let Err(UrlRejection::UnknownAction(name)) = parse_url(&url(&long)) else {
            panic!("expected an unknown action");
        };
        assert_eq!(name.chars().count(), MAX_LOGGED_NAME_CHARS);
    }

    #[test]
    fn only_mute_and_status_run_without_the_opt_in() {
        let allowed: Vec<_> = UrlAction::ALL
            .into_iter()
            .filter(|action| action.is_allowed(false))
            .collect();
        assert_eq!(allowed, [UrlAction::Mute, UrlAction::Status]);
        assert!(UrlAction::ALL
            .into_iter()
            .all(|action| action.is_allowed(true)));
    }

    #[test]
    fn actions_that_can_unmute_are_unmute_and_toggle() {
        for action in UrlAction::ALL {
            let unmutes = matches!(
                action.command(),
                Some(MicCommand::Unmute | MicCommand::Toggle)
            );
            assert_eq!(action.can_unmute(), unmutes, "{action:?}");
        }
    }

    #[test]
    fn status_changes_nothing() {
        assert_eq!(UrlAction::Status.command(), None);
        assert_eq!(UrlAction::Mute.command(), Some(MicCommand::Mute));
    }

    #[test]
    fn queued_actions_run_in_arrival_order_and_bad_urls_are_dropped() {
        let mut actions = UrlActions::default();
        let mute = url("mute");
        let status = url("status");
        actions.push_urls([mute.as_str(), "https://example.com", status.as_str()]);
        actions.push_urls([url("bogus").as_str()]);

        assert_eq!(actions.pop(), Some(UrlAction::Mute));
        assert_eq!(actions.pop(), Some(UrlAction::Status));
        assert_eq!(actions.pop(), None);
    }

    #[test]
    fn the_queue_is_bounded() {
        let mut actions = UrlActions::default();
        let mute = url("mute");
        actions.push_urls(std::iter::repeat_n(mute.as_str(), MAX_QUEUED_ACTIONS + 5));

        let mut count = 0;
        while actions.pop().is_some() {
            count += 1;
        }
        assert_eq!(count, MAX_QUEUED_ACTIONS);
    }
}
