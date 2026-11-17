//! The MCP server as a system service: the systemd user unit (Linux) or launchd agent (macOS)
//! that runs `osh mcp serve`, and the `Step`s that install, start, stop and remove it. Nothing
//! here touches the host; `setup::run_steps` does, after consent. Patterned on `gwops`.

use crate::gateway::{chain, cmd, script, step, Paths};
use crate::platform::{Facts, Host, Os};
use crate::setup::{Step, StepKind};
use osh_core::mcpserver::state;
use std::path::{Path, PathBuf};

pub fn unit_name() -> &'static str {
    "osh-mcp.service"
}

pub fn agent_label() -> &'static str {
    "dev.guilospanck.osh-mcp"
}

pub fn unit_path(p: &Paths) -> PathBuf {
    let config = p.config_home.parent().unwrap_or(&p.home);
    config.join("systemd/user").join(unit_name())
}

pub fn plist_path(p: &Paths) -> PathBuf {
    p.home
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", agent_label()))
}

/// Where the launchd agent's output goes (systemd uses the journal).
pub fn logs_dir(p: &Paths) -> PathBuf {
    p.data.join("mcp-logs")
}

/// A value inside a double-quoted systemd setting: the quote, the backslash and `%` (which
/// systemd expands) are escaped.
fn unit_quote(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
}

/// The Linux user unit. `PATH` lists where `openshell` and the container runtime may live
/// (a user unit does not inherit the login shell's PATH); the token is never in the unit: the
/// server reads it from the data folder.
pub fn systemd_unit(p: &Paths) -> String {
    let env: String = crate::gwops::service_env(p)
        .iter()
        .map(|(k, v)| format!("Environment=\"{k}={}\"\n", unit_quote(v)))
        .collect();
    format!(
        "[Unit]\nDescription=osh MCP server\nAfter=default.target\n\n\
         [Service]\nType=simple\n\
         Environment=\"PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin:/home/linuxbrew/.linuxbrew/bin\"\n\
         {env}\
         ExecStart=\"{exe}\" mcp serve\n\
         Restart=on-failure\nRestartSec=5s\nUMask=0077\n\n\
         [Install]\nWantedBy=default.target\n",
        exe = unit_quote(&p.exe.display().to_string())
    )
}

/// The macOS launchd agent: Homebrew's folders on PATH, restarted while it fails.
pub fn launchd_plist(p: &Paths, logs: &Path) -> String {
    let env = crate::gwops::service_env(p);
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    crate::gwops::agent_plist(agent_label(), &p.exe, &["mcp", "serve"], &env, logs, "mcp")
}

fn st(id: &'static str, title: &str, kind: StepKind) -> Step {
    step(id, title, kind)
}

/// Write the unit or agent, then enable and start it.
pub fn install_steps(f: &Facts, p: &Paths) -> Vec<Step> {
    let steps = match f.os {
        Os::Linux => vec![
            st(
                "mcp-unit",
                "Write the MCP server's systemd user unit",
                StepKind::Write {
                    path: unit_path(p),
                    content: systemd_unit(p),
                },
            ),
            st(
                "mcp-reload",
                "Reload systemd user units",
                cmd("systemctl", &["--user", "daemon-reload"]),
            ),
            st(
                "mcp-enable",
                "Start the MCP server",
                cmd("systemctl", &["--user", "enable", "--now", unit_name()]),
            ),
        ],
        Os::MacOs => {
            let logs = logs_dir(p);
            vec![
                st(
                    "mcp-agent",
                    "Write the MCP server's launchd agent",
                    StepKind::Write {
                        path: plist_path(p),
                        content: launchd_plist(p, &logs),
                    },
                ),
                st(
                    "mcp-logs",
                    "Create the log directory",
                    cmd("mkdir", &["-p", &logs.to_string_lossy()]),
                ),
                st(
                    "mcp-bootout",
                    "Stop a running agent",
                    script(
                        r#"launchctl bootout "gui/$(id -u)/$1" 2>/dev/null || true"#,
                        &[agent_label()],
                    ),
                ),
                st(
                    "mcp-enable",
                    "Start the MCP server",
                    script(
                        r#"launchctl enable "gui/$(id -u)/$2"; launchctl bootstrap "gui/$(id -u)" "$1""#,
                        &[&plist_path(p).to_string_lossy(), agent_label()],
                    ),
                ),
            ]
        }
    };
    chain(steps, None)
}

/// Turn an installed service on (and keep it on at the next login).
pub fn start_steps(f: &Facts, p: &Paths) -> Vec<Step> {
    let steps = match f.os {
        Os::Linux => vec![st(
            "mcp-start",
            "Start the MCP server",
            cmd("systemctl", &["--user", "enable", "--now", unit_name()]),
        )],
        Os::MacOs => vec![st(
            "mcp-start",
            "Start the MCP server",
            script(
                r#"launchctl enable "gui/$(id -u)/$2"; launchctl bootout "gui/$(id -u)/$2" 2>/dev/null || true; launchctl bootstrap "gui/$(id -u)" "$1""#,
                &[&plist_path(p).to_string_lossy(), agent_label()],
            ),
        )],
    };
    chain(steps, None)
}

/// Turn the service off and keep it off at the next login.
pub fn stop_steps(f: &Facts, _p: &Paths) -> Vec<Step> {
    let steps = match f.os {
        Os::Linux => vec![st(
            "mcp-stop",
            "Stop the MCP server",
            cmd("systemctl", &["--user", "disable", "--now", unit_name()]),
        )],
        Os::MacOs => vec![st(
            "mcp-stop",
            "Stop the MCP server",
            script(
                r#"launchctl bootout "gui/$(id -u)/$1" 2>/dev/null || true; launchctl disable "gui/$(id -u)/$1""#,
                &[agent_label()],
            ),
        )],
    };
    chain(steps, None)
}

/// Stop the service and delete its unit or agent (the token and state files stay).
pub fn remove_steps(f: &Facts, p: &Paths) -> Vec<Step> {
    let steps = match f.os {
        Os::Linux => vec![
            st(
                "mcp-stop",
                "Stop the MCP server",
                script(
                    r#"systemctl --user disable --now "$1" 2>/dev/null || true"#,
                    &[unit_name()],
                ),
            ),
            st(
                "mcp-rm",
                "Delete the systemd user unit",
                cmd("rm", &["-f", &unit_path(p).to_string_lossy()]),
            ),
            st(
                "mcp-reload",
                "Reload systemd user units",
                cmd("systemctl", &["--user", "daemon-reload"]),
            ),
        ],
        Os::MacOs => vec![
            st(
                "mcp-stop",
                "Stop the MCP server",
                script(
                    r#"launchctl bootout "gui/$(id -u)/$1" 2>/dev/null || true"#,
                    &[agent_label()],
                ),
            ),
            st(
                "mcp-rm",
                "Delete the launchd agent",
                cmd("rm", &["-f", &plist_path(p).to_string_lossy()]),
            ),
        ],
    };
    chain(steps, None)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Svc {
    /// No unit or agent is installed and no server is running.
    NotInstalled,
    /// Installed, and no server is answering.
    Stopped,
    /// A server is running (installed by osh or started by hand).
    Running,
}

/// What the dashboard and `osh mcp status` show. A live state file wins: whoever started the
/// server, it is on.
pub fn status(os: Os, h: &dyn Host, p: &Paths) -> Svc {
    if state::read(&p.data).is_some() {
        return Svc::Running;
    }
    let installed = match os {
        Os::Linux => h.exists(&unit_path(p)),
        Os::MacOs => h.exists(&plist_path(p)),
    };
    if installed {
        Svc::Stopped
    } else {
        Svc::NotInstalled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::testing::{sample_facts, FakeHost};

    fn paths() -> Paths {
        Paths {
            home: "/h".into(),
            config_home: "/h/.config/openshell".into(),
            state: "/h/.local/state/openshell".into(),
            share: "/h/.local/share/openshell".into(),
            data: "/h/.local/share/osh".into(),
            exe: "/usr/local/bin/osh".into(),
        }
    }

    fn ids(steps: &[Step]) -> Vec<&'static str> {
        steps.iter().map(|s| s.id).collect()
    }

    fn shown(steps: &[Step]) -> String {
        steps
            .iter()
            .map(|s| s.display())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_systemd_unit_runs_mcp_serve_and_never_holds_the_token() {
        let u = systemd_unit(&paths());
        assert!(
            u.contains("ExecStart=\"/usr/local/bin/osh\" mcp serve\n"),
            "{u}"
        );
        assert!(
            u.contains("Environment=\"OSH_DIR=/h/.local/share/osh\""),
            "{u}"
        );
        assert!(u.contains("Environment=\"PATH=%h/.local/bin:"), "{u}");
        assert!(u.contains("Restart=on-failure") && u.contains("UMask=0077"));
        assert!(u.contains("WantedBy=default.target"));
        let lower = u.to_lowercase();
        assert!(!lower.contains("token") && !lower.contains("bearer"), "{u}");
    }

    #[test]
    fn the_unit_escapes_what_systemd_would_expand() {
        let mut p = paths();
        p.exe = "/opt/my \"osh\"/100%/osh".into();
        let u = systemd_unit(&p);
        assert!(
            u.contains("ExecStart=\"/opt/my \\\"osh\\\"/100%%/osh\" mcp serve"),
            "{u}"
        );
    }

    #[test]
    fn the_launchd_agent_has_label_arguments_environment_and_log_names() {
        let x = launchd_plist(&paths(), Path::new("/logs"));
        assert!(
            x.contains("<key>Label</key><string>dev.guilospanck.osh-mcp</string>"),
            "{x}"
        );
        assert!(
            x.contains(
                "<string>/usr/local/bin/osh</string><string>mcp</string><string>serve</string>"
            ),
            "{x}"
        );
        assert!(
            x.contains("<key>OSH_DIR</key><string>/h/.local/share/osh</string>"),
            "{x}"
        );
        assert!(x.contains("<key>KeepAlive</key>") && x.contains("<key>RunAtLoad</key><true/>"));
        assert!(
            x.contains("/logs/mcp.out.log") && x.contains("/logs/mcp.err.log"),
            "{x}"
        );
        for tag in ["dict", "array", "plist"] {
            assert_eq!(
                x.matches(&format!("<{tag}>")).count() + x.matches(&format!("<{tag} ")).count(),
                x.matches(&format!("</{tag}>")).count(),
                "unbalanced <{tag}>: {x}"
            );
        }
    }

    #[test]
    fn install_on_linux_writes_the_unit_then_reloads_then_enables() {
        let steps = install_steps(&sample_facts(Os::Linux), &paths());
        assert_eq!(ids(&steps), ["mcp-unit", "mcp-reload", "mcp-enable"]);
        assert_eq!(steps[0].after, None);
        assert_eq!(steps[1].after, Some("mcp-unit"));
        assert_eq!(steps[2].after, Some("mcp-reload"));
        let StepKind::Write { path, content } = &steps[0].kind else {
            panic!("a write step first");
        };
        assert_eq!(path, &unit_path(&paths()));
        assert_eq!(content, &systemd_unit(&paths()));
        let s = shown(&steps);
        assert!(s.contains("systemctl --user daemon-reload"), "{s}");
        assert!(
            s.contains("systemctl --user enable --now osh-mcp.service"),
            "{s}"
        );
    }

    #[test]
    fn install_on_macos_writes_the_agent_makes_the_logs_and_bootstraps_it() {
        let steps = install_steps(&sample_facts(Os::MacOs), &paths());
        assert_eq!(
            ids(&steps),
            ["mcp-agent", "mcp-logs", "mcp-bootout", "mcp-enable"]
        );
        for pair in steps.windows(2) {
            assert_eq!(pair[1].after, Some(pair[0].id));
        }
        let StepKind::Write { path, content } = &steps[0].kind else {
            panic!("a write step first");
        };
        assert_eq!(path, &plist_path(&paths()));
        assert_eq!(content, &launchd_plist(&paths(), &logs_dir(&paths())));
        let s = shown(&steps);
        assert!(s.contains("launchctl bootstrap"), "{s}");
    }

    #[test]
    fn stop_keeps_it_off_and_start_turns_it_on_on_both_systems() {
        let linux = sample_facts(Os::Linux);
        let stop = shown(&stop_steps(&linux, &paths()));
        assert!(
            stop.contains("systemctl --user disable --now osh-mcp.service"),
            "{stop}"
        );
        let start = shown(&start_steps(&linux, &paths()));
        assert!(
            start.contains("systemctl --user enable --now osh-mcp.service"),
            "{start}"
        );
        let mac = sample_facts(Os::MacOs);
        let stop = shown(&stop_steps(&mac, &paths()));
        assert!(
            stop.contains("launchctl bootout") && stop.contains("launchctl disable"),
            "{stop}"
        );
        let start = shown(&start_steps(&mac, &paths()));
        assert!(
            start.contains("launchctl enable") && start.contains("launchctl bootstrap"),
            "{start}"
        );
    }

    #[test]
    fn remove_stops_and_deletes_the_unit_or_agent() {
        let linux = shown(&remove_steps(&sample_facts(Os::Linux), &paths()));
        assert!(
            linux.contains("disable --now") && linux.contains("osh-mcp.service"),
            "{linux}"
        );
        assert!(
            linux.contains("rm -f") && linux.contains("daemon-reload"),
            "{linux}"
        );
        let mac = shown(&remove_steps(&sample_facts(Os::MacOs), &paths()));
        assert!(
            mac.contains("launchctl bootout") && mac.contains("rm -f"),
            "{mac}"
        );
    }

    fn live_state_in(tag: &str) -> Paths {
        let mut p = paths();
        let d = std::env::temp_dir().join(format!("osh-mcpsvc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        p.data = d;
        p
    }

    #[test]
    fn status_tells_not_installed_stopped_and_running_apart() {
        let p = live_state_in("status");
        let mut h = FakeHost::default();
        assert_eq!(status(Os::Linux, &h, &p), Svc::NotInstalled);
        h.paths.insert(unit_path(&p).to_string_lossy().into_owned());
        assert_eq!(status(Os::Linux, &h, &p), Svc::Stopped);
        assert_eq!(
            status(Os::MacOs, &h, &p),
            Svc::NotInstalled,
            "the agent is another file"
        );
        // A live server (this process owns the state file) is running whatever installed it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        state::claim(&p.data, &listener.local_addr().unwrap().to_string()).unwrap();
        assert_eq!(status(Os::Linux, &h, &p), Svc::Running);
        assert_eq!(status(Os::MacOs, &FakeHost::default(), &p), Svc::Running);
        let _ = std::fs::remove_dir_all(&p.data);
    }
}
