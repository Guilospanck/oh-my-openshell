//! Which environment file the OpenShell gateway gets on this machine. A systemd `EnvironmentFile`
//! cannot branch on the OS, so osh ships one file per OS and architecture and picks the right one:
//! `gateway-linux.env`, `gateway-macos-arm64.env`, `gateway-macos-x86_64.env`. A copy in
//! `~/.config/openshell` (written by the user, or linked by their dotfiles) wins over the embedded
//! default in the osh data dir. The old `gateway.env` is no longer read.
use crate::assets;
use crate::gateway::Paths;
use crate::platform::Os;
use std::path::PathBuf;

/// The file name for `os` on CPU architecture `arch` (`std::env::consts::ARCH` spelling).
pub fn name(os: Os, arch: &str) -> &'static str {
    match (os, arch) {
        (Os::Linux, _) => "gateway-linux.env",
        (Os::MacOs, "aarch64" | "arm64") => "gateway-macos-arm64.env",
        (Os::MacOs, _) => "gateway-macos-x86_64.env",
    }
}

/// The file name for the machine osh is running on.
pub fn host_name() -> &'static str {
    let os = if cfg!(target_os = "macos") {
        Os::MacOs
    } else {
        Os::Linux
    };
    name(os, std::env::consts::ARCH)
}

/// Where the gateway's environment is read from: the user's copy in the config folder when there
/// is one, else the default osh keeps in its data dir. Not checked for existence when neither is.
pub fn path(p: &Paths) -> PathBuf {
    let user = p.config_home.join(host_name());
    if user.is_file() {
        user
    } else {
        p.data.join(host_name())
    }
}

/// The text of that file, or the embedded default when the data dir has not got it yet.
pub fn read(p: &Paths) -> Option<String> {
    std::fs::read_to_string(path(p))
        .ok()
        .or_else(|| assets::embedded(host_name()).map(str::to_string))
}

/// The systemd drop-in for `openshell-gateway.service`, which the deb and rpm packages ship with
/// `EnvironmentFile=-%E/openshell/gateway.env` (read for every OS, so wrong for most). An empty
/// `EnvironmentFile=` resets that list; the two lines after it are this machine's file, the data
/// dir default first so a copy in the config folder overrides it. Returns (path, text).
pub fn gateway_dropin(p: &Paths, os: Os, arch: &str) -> (PathBuf, String) {
    let n = name(os, arch);
    let esc = |s: String| s.replace('\\', "\\\\").replace('%', "%%");
    let text = format!(
        "# Written by osh: this gateway reads {n}, not gateway.env.\n\
         [Service]\n\
         EnvironmentFile=\n\
         EnvironmentFile=-{data}\n\
         EnvironmentFile=-%E/openshell/{n}\n",
        data = esc(p.data.join(n).display().to_string()),
    );
    let dir = p
        .config_home
        .parent()
        .unwrap_or(&p.home)
        .join("systemd/user/openshell-gateway.service.d");
    (dir.join("osh-env.conf"), text)
}

/// Test support: a host where the drop-in is already in place, like one osh set up before.
#[cfg(test)]
pub fn with_dropin(h: &mut crate::platform::testing::FakeHost, p: &Paths) {
    let (path, text) = gateway_dropin(p, Os::Linux, std::env::consts::ARCH);
    h.files.insert(path.to_string_lossy().into_owned(), text);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &std::path::Path) -> Paths {
        Paths::from_env(
            Some(root.join("home").into()),
            None,
            Some(root.join("data")),
        )
        .unwrap()
    }

    #[test]
    fn the_file_follows_the_os_and_for_macos_the_architecture() {
        assert_eq!(name(Os::Linux, "aarch64"), "gateway-linux.env");
        assert_eq!(name(Os::Linux, "x86_64"), "gateway-linux.env");
        assert_eq!(name(Os::MacOs, "aarch64"), "gateway-macos-arm64.env");
        assert_eq!(name(Os::MacOs, "arm64"), "gateway-macos-arm64.env");
        assert_eq!(name(Os::MacOs, "x86_64"), "gateway-macos-x86_64.env");
    }

    #[test]
    fn every_platform_file_is_embedded_and_only_macos_sets_path_and_docker_host() {
        for (os, arch) in [
            (Os::Linux, "x86_64"),
            (Os::MacOs, "aarch64"),
            (Os::MacOs, "x86_64"),
        ] {
            let text = assets::embedded(name(os, arch)).expect("embedded");
            let sets = |k: &str| text.lines().any(|l| l.trim_start().starts_with(k));
            if os == Os::Linux {
                // Homebrew's folders and Colima's socket do not exist on Linux.
                assert!(!sets("PATH=") && !sets("DOCKER_HOST="), "{text}");
            } else {
                assert!(sets("PATH=") && sets("DOCKER_HOST="), "{text}");
            }
        }
        // Intel Homebrew lives in /usr/local, Apple Silicon's in /opt/homebrew: each comes first.
        assert!(assets::embedded("gateway-macos-x86_64.env")
            .unwrap()
            .contains("PATH=\"/usr/local/bin:/opt/homebrew/bin"));
        assert!(assets::embedded("gateway-macos-arm64.env")
            .unwrap()
            .contains("PATH=\"/opt/homebrew/bin:/usr/local/bin"));
    }

    #[test]
    fn a_copy_in_the_config_folder_wins_over_the_data_dir_default() {
        let root = std::env::temp_dir().join(format!("osh-envfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let p = paths(&root);
        std::fs::create_dir_all(&p.data).unwrap();
        std::fs::write(p.data.join(host_name()), "A=data\n").unwrap();
        assert_eq!(path(&p), p.data.join(host_name()));
        assert_eq!(read(&p).as_deref(), Some("A=data\n"));
        std::fs::create_dir_all(&p.config_home).unwrap();
        std::fs::write(p.config_home.join(host_name()), "A=user\n").unwrap();
        assert_eq!(path(&p), p.config_home.join(host_name()));
        assert_eq!(read(&p).as_deref(), Some("A=user\n"));
        // The legacy name is never read.
        std::fs::remove_file(p.config_home.join(host_name())).unwrap();
        std::fs::remove_file(p.data.join(host_name())).unwrap();
        std::fs::write(
            p.config_home.join("gateway.env"),
            "PATH=/opt/homebrew/bin\n",
        )
        .unwrap();
        assert_eq!(read(&p).as_deref(), assets::embedded(host_name()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_dropin_resets_the_packaged_environment_file_and_points_at_this_machines() {
        let p = paths(std::path::Path::new("/r"));
        let (path, text) = gateway_dropin(&p, Os::Linux, "aarch64");
        assert_eq!(
            path,
            PathBuf::from("/r/home/.config/systemd/user/openshell-gateway.service.d/osh-env.conf")
        );
        let lines: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(
            lines,
            [
                "[Service]",
                "EnvironmentFile=",
                "EnvironmentFile=-/r/data/gateway-linux.env",
                "EnvironmentFile=-%E/openshell/gateway-linux.env",
            ]
        );
        assert!(!text.contains("gateway.env\n"));
    }
}
