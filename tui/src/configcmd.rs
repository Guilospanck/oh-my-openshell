//! `osh config ...`: argv to `osh_core::settings::run_cli`, output to the terminal. The logic and
//! its tests live in core; this file only supplies the real folder, environment and editor.

use std::path::Path;

/// Run `argv path` on the terminal this process has and wait for it.
fn open_in_editor(argv: &[String], path: &Path) -> Result<(), String> {
    let (prog, rest) = argv.split_first().ok_or("no editor command")?;
    let status = std::process::Command::new(prog)
        .args(rest)
        .arg(path)
        .status()
        .map_err(|e| format!("cannot start {prog}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{prog} exited with {status}"))
    }
}

fn edit(path: &Path) -> Result<(), String> {
    let editor =
        crate::exted::host_editor().map_err(|e| format!("{e}; the file is {}", path.display()))?;
    open_in_editor(&editor.argv, path)
}

pub fn run(args: &[String]) -> i32 {
    let out = osh_core::settings::run_cli(
        args,
        &osh_core::assets::data_dir(),
        &|k| std::env::var_os(k),
        &edit,
    );
    print!("{}", out.stdout);
    eprint!("{}", out.stderr);
    out.code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_editor_is_run_with_the_file_and_its_exit_decides() {
        let p = Path::new("/nonexistent/osh.toml");
        assert!(open_in_editor(&["true".into()], p).is_ok());
        assert!(open_in_editor(&["false".into()], p)
            .unwrap_err()
            .contains("exited"));
        assert!(open_in_editor(&["/nonexistent/editor".into()], p)
            .unwrap_err()
            .contains("cannot start"));
        assert!(open_in_editor(&[], p).is_err());
    }
}
