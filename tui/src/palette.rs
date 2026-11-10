//! Every action of the dashboard in one table: what the help screen lists and what the command
//! palette searches. Running an action sends its key to the dashboard, so the palette and the
//! keyboard can never disagree about what an action does.
use crate::app::Tab;
use ratatui::crossterm::event::KeyCode;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Tab(Tab),
}

/// An action only the palette offers: it has no key, so the footer stays as it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    /// Open `toolchains.toml` in the editor.
    EditToolchains,
    /// Which editor the edit actions use: auto, always the user's, always the built-in one.
    EditorMode,
    /// Every data file: view, edit, compare with or restore the shipped default, add, delete.
    DataFiles,
    /// The filesystem policy every new sandbox gets (`sandbox-policy.yaml`).
    EditPolicy,
    /// The set-up wizard from its first screen: resumes a stopped install, repairs what is missing.
    ResumeSetup,
    /// `osh upgrade`: the latest OpenShell, fresh gateway configs, restarted gateways.
    Upgrade,
    /// `osh doctor` in a viewer.
    Doctor,
    /// `osh uninstall`: OpenShell, its services, osh's data and osh itself.
    Uninstall,
    /// Install an optional tool osh uses.
    InstallGitleaks,
    InstallClipboard,
    /// Pass the host tmux's keys to a focused sandbox shell, or not.
    TmuxKeys,
    /// Forget a "Which git provider hosts HOST?" answer.
    ForgetGitHost,
}

pub struct Action {
    pub scope: Scope,
    /// The key as shown (`Enter`, `S`, `1`); empty for a palette command.
    pub key: &'static str,
    pub code: KeyCode,
    pub title: &'static str,
    /// Set for a palette command, which runs this instead of sending `code`.
    pub cmd: Option<Command>,
}

const fn a(scope: Scope, key: &'static str, code: KeyCode, title: &'static str) -> Action {
    Action {
        scope,
        key,
        code,
        title,
        cmd: None,
    }
}

const fn command(scope: Scope, title: &'static str, cmd: Command) -> Action {
    Action {
        scope,
        key: "",
        code: KeyCode::Null,
        title,
        cmd: Some(cmd),
    }
}

use Scope::{Global, Tab as T};

pub const ACTIONS: &[Action] = &[
    a(
        T(Tab::Sandboxes),
        "Enter",
        KeyCode::Enter,
        "Open a shell in the sandbox",
    ),
    a(
        T(Tab::Sandboxes),
        "l",
        KeyCode::Char('l'),
        "Show the sandbox logs",
    ),
    a(
        T(Tab::Sandboxes),
        "i",
        KeyCode::Char('i'),
        "Show the sandbox's settings and effective policy",
    ),
    a(
        T(Tab::Sandboxes),
        "p",
        KeyCode::Char('p'),
        "Review blocked requests",
    ),
    a(
        T(Tab::Sandboxes),
        "n",
        KeyCode::Char('n'),
        "Create a sandbox",
    ),
    a(
        T(Tab::Sandboxes),
        "d",
        KeyCode::Char('d'),
        "Delete the sandbox",
    ),
    a(
        T(Tab::Sandboxes),
        "s",
        KeyCode::Char('s'),
        "Stop the sandbox",
    ),
    a(
        T(Tab::Sandboxes),
        "S",
        KeyCode::Char('S'),
        "Start the sandbox",
    ),
    a(
        T(Tab::Sandboxes),
        "a",
        KeyCode::Char('a'),
        "Attach providers to the sandbox",
    ),
    a(
        T(Tab::Sandboxes),
        "x",
        KeyCode::Char('x'),
        "Detach providers from the sandbox",
    ),
    a(
        T(Tab::Sandboxes),
        "K",
        KeyCode::Char('K'),
        "Install agent skills",
    ),
    a(
        T(Tab::Sandboxes),
        "T",
        KeyCode::Char('T'),
        "Transfer agent state",
    ),
    a(
        T(Tab::Sandboxes),
        "t",
        KeyCode::Char('t'),
        "Renew the repo token",
    ),
    a(
        T(Tab::Providers),
        "n",
        KeyCode::Char('n'),
        "Create a provider",
    ),
    a(
        T(Tab::Providers),
        "u",
        KeyCode::Char('u'),
        "Update the provider's credentials",
    ),
    a(
        T(Tab::Providers),
        "d",
        KeyCode::Char('d'),
        "Delete the provider",
    ),
    a(
        T(Tab::Profiles),
        "u",
        KeyCode::Char('u'),
        "Import or update the profile from providers/",
    ),
    a(
        T(Tab::Profiles),
        "e",
        KeyCode::Char('e'),
        "Edit, view, compare or delete the profile file",
    ),
    a(
        T(Tab::Profiles),
        "n",
        KeyCode::Char('n'),
        "New provider profile (opens in your editor)",
    ),
    a(T(Tab::Images), "b", KeyCode::Char('b'), "Build the image"),
    a(
        T(Tab::Images),
        "e",
        KeyCode::Char('e'),
        "Edit, view, compare or restore the Dockerfile",
    ),
    a(
        T(Tab::Images),
        "n",
        KeyCode::Char('n'),
        "New Dockerfile (opens in your editor)",
    ),
    a(
        T(Tab::Gateways),
        "Enter",
        KeyCode::Enter,
        "Select the gateway",
    ),
    a(
        T(Tab::Gateways),
        "s",
        KeyCode::Char('s'),
        "Make the gateway the default",
    ),
    a(T(Tab::Gateways), "a", KeyCode::Char('a'), "Add a gateway"),
    a(
        T(Tab::Gateways),
        "R",
        KeyCode::Char('R'),
        "Start or restart the gateway (applies its config)",
    ),
    a(
        T(Tab::Gateways),
        "l",
        KeyCode::Char('l'),
        "Show the gateway's service log",
    ),
    a(
        T(Tab::Gateways),
        "e",
        KeyCode::Char('e'),
        "Edit the gateway's config",
    ),
    a(
        T(Tab::Gateways),
        "V",
        KeyCode::Char('V'),
        "Set the VM size (vCPUs, memory) of a VM gateway",
    ),
    a(
        T(Tab::Gateways),
        "d",
        KeyCode::Char('d'),
        "Wipe the gateway (delete all its data)",
    ),
    a(
        T(Tab::Gateways),
        "x",
        KeyCode::Char('x'),
        "Remove the gateway",
    ),
    a(Global, "1", KeyCode::Char('1'), "Go to Sandboxes"),
    a(Global, "2", KeyCode::Char('2'), "Go to Providers"),
    a(Global, "3", KeyCode::Char('3'), "Go to Profiles"),
    a(Global, "4", KeyCode::Char('4'), "Go to Images"),
    a(Global, "5", KeyCode::Char('5'), "Go to Gateways"),
    a(Global, "g", KeyCode::Char('g'), "Switch gateway"),
    a(Global, "r", KeyCode::Char('r'), "Refresh"),
    a(Global, "!", KeyCode::Char('!'), "Show alerts"),
    a(Global, "?", KeyCode::Char('?'), "Show help"),
    a(Global, "q", KeyCode::Char('q'), "Quit"),
    command(
        Global,
        "Edit toolchains.toml (hosts new sandboxes may read from)",
        Command::EditToolchains,
    ),
    command(
        Global,
        "Editor: switch auto / your own ($VISUAL, $EDITOR) / built-in",
        Command::EditorMode,
    ),
    command(
        Global,
        "Data files: profiles, Dockerfiles, policy, configs (edit, compare, restore defaults)",
        Command::DataFiles,
    ),
    command(
        Global,
        "Sandbox policy: the filesystem policy of new sandboxes (sandbox-policy.yaml)",
        Command::EditPolicy,
    ),
    command(
        Global,
        "Resume or repair set-up (the set-up wizard)",
        Command::ResumeSetup,
    ),
    command(
        Global,
        "Upgrade OpenShell, refresh gateway configs, restart gateways",
        Command::Upgrade,
    ),
    command(
        Global,
        "Check this machine (what osh needs and what is missing)",
        Command::Doctor,
    ),
    command(
        Global,
        "Install gitleaks (secret scan of files copied into sandboxes)",
        Command::InstallGitleaks,
    ),
    command(
        Global,
        "Install the clipboard tool (paste host images with Ctrl-V)",
        Command::InstallClipboard,
    ),
    command(
        Global,
        "Host tmux keys in sandbox shells: switch on or off",
        Command::TmuxKeys,
    ),
    command(
        Global,
        "Git hosts: forget a \"which git provider hosts it\" answer",
        Command::ForgetGitHost,
    ),
    command(
        Global,
        "Uninstall osh and OpenShell (removes all sandboxes)",
        Command::Uninstall,
    ),
];

/// What the current tab can do: its own actions first, then the global ones.
pub fn available(tab: Tab) -> Vec<&'static Action> {
    let own = ACTIONS.iter().filter(|x| x.scope == Scope::Tab(tab));
    let all = ACTIONS.iter().filter(|x| x.scope == Scope::Global);
    own.chain(all).collect()
}

/// The positions (in chars) of `query` inside `title` as a case-insensitive subsequence, or none.
fn subsequence(title: &str, query: &str) -> Option<Vec<usize>> {
    let mut at = Vec::new();
    let mut chars = title.chars().enumerate();
    for q in query.chars().filter(|c| !c.is_whitespace()) {
        let q = q.to_ascii_lowercase();
        let (i, _) = chars.by_ref().find(|(_, c)| c.to_ascii_lowercase() == q)?;
        at.push(i);
    }
    Some(at)
}

/// Actions matching `query`, best first, with the matched character positions to highlight.
/// An empty query lists them all in table order. Tighter and earlier matches rank higher.
pub fn filter(tab: Tab, query: &str) -> Vec<(&'static Action, Vec<usize>)> {
    let mut hits: Vec<(usize, &'static Action, Vec<usize>)> = available(tab)
        .into_iter()
        .enumerate()
        .filter_map(|(order, x)| {
            let at = subsequence(x.title, query)?;
            let span = at.last().zip(at.first()).map_or(0, |(l, f)| l - f);
            let first = at.first().copied().unwrap_or(0);
            // Compact matches beat scattered ones; table order breaks ties.
            Some((span * 4 + first + order / 4, x, at))
        })
        .collect();
    if !query.trim().is_empty() {
        hits.sort_by_key(|(score, ..)| *score);
    }
    hits.into_iter().map(|(_, x, at)| (x, at)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TABS;

    #[test]
    fn every_footer_key_is_an_action_so_the_palette_cannot_miss_one() {
        for (tab, name) in TABS {
            let own: Vec<&str> = ACTIONS
                .iter()
                .filter(|x| x.scope == Scope::Tab(tab))
                .map(|x| x.key)
                .collect();
            for entry in crate::ui::footer_help(tab).split("  ") {
                let keys = entry.split_whitespace().next().unwrap_or_default();
                for k in keys.split('/').filter(|k| !k.is_empty()) {
                    assert!(
                        own.contains(&k),
                        "{name} footer key {k:?} is not in the action table"
                    );
                }
            }
        }
    }

    #[test]
    fn a_tab_lists_its_own_actions_before_the_global_ones() {
        let v = available(Tab::Images);
        assert_eq!(v[0].title, "Build the image");
        assert!(v.iter().any(|x| x.title == "Quit"));
        assert!(!v.iter().any(|x| x.title == "Add a gateway"));
    }

    #[test]
    fn filtering_is_fuzzy_ranked_and_reports_match_positions() {
        let r = filter(Tab::Sandboxes, "go to g");
        assert_eq!(r[0].0.title, "Go to Gateways");
        let r = filter(Tab::Sandboxes, "del");
        assert_eq!(r[0].0.title, "Delete the sandbox");
        assert_eq!(r[0].1, vec![0, 1, 2]);
        assert!(filter(Tab::Sandboxes, "zzzz").is_empty());
        assert_eq!(
            filter(Tab::Gateways, "").len(),
            available(Tab::Gateways).len()
        );
    }
}
