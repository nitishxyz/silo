//! Shell integration that puts `<SILO_HOME>/shims` (and `<SILO_HOME>/bin`)
//! back in front of `PATH` after the user's startup files ran, since login
//! files (macOS `path_helper`, Debian `/etc/profile`) reorder or reset it.
//!
//! zsh runs through a `ZDOTDIR` wrapper, bash through `--rcfile`, fish
//! through an `XDG_DATA_DIRS` `vendor_conf.d` snippet; other shells only get
//! the environment.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// A login shell Silo integrates with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
}

impl Shell {
    /// The shell `program` runs, by file name.
    #[must_use]
    pub fn of(program: &Path) -> Option<Self> {
        match program.file_name()?.to_str()?.trim_start_matches('-') {
            "zsh" => Some(Self::Zsh),
            "bash" => Some(Self::Bash),
            "fish" => Some(Self::Fish),
            _ => None,
        }
    }
}

pub fn dir(home: &Path) -> PathBuf {
    home.join("shell")
}

fn zsh_dir(home: &Path) -> PathBuf {
    dir(home).join("zsh")
}

fn bash_rc(home: &Path) -> PathBuf {
    dir(home).join("bash").join("rc")
}

fn fish_data(home: &Path) -> PathBuf {
    dir(home).join("fish")
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

fn quoted_list(lead: &[PathBuf]) -> String {
    lead.iter().map(|p| quote(p)).collect::<Vec<_>>().join(" ")
}

const ZSH_FILES: &[&str] = &[".zshenv", ".zprofile", ".zshrc", ".zlogin"];

fn zsh_file(home: &Path, lead: &[PathBuf], file: &str) -> String {
    let ours = quote(&zsh_dir(home));
    let lead = quoted_list(lead);
    let done = match file {
        ".zshenv" => "[[ ! -o login && ! -o interactive ]]",
        ".zshrc" => "[[ ! -o login ]]",
        ".zlogin" => "true",
        _ => "false",
    };
    format!(
        "ZDOTDIR=${{SILO_ZDOTDIR:-$HOME}}\n\
         [[ -r $ZDOTDIR/{file} ]] && builtin source -- $ZDOTDIR/{file}\n\
         SILO_ZDOTDIR=$ZDOTDIR\n\
         ZDOTDIR={ours}\n\
         if {done}; then\n\
         \x20 if [[ $SILO_ZDOTDIR == $HOME ]]; then unset ZDOTDIR; else ZDOTDIR=$SILO_ZDOTDIR; fi\n\
         \x20 unset SILO_ZDOTDIR\n\
         \x20 typeset -ga _silo_lead=({lead})\n\
         \x20 path=($_silo_lead ${{path:|_silo_lead}})\n\
         \x20 unset _silo_lead\n\
         fi\n"
    )
}

fn bash_rc_script(lead: &[PathBuf]) -> String {
    let joined = quote(Path::new(&lead.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().join(":")));
    let lead = quoted_list(lead);
    format!(
        "[ -r /etc/profile ] && . /etc/profile\n\
         if [ -r ~/.bash_profile ]; then . ~/.bash_profile\n\
         elif [ -r ~/.bash_login ]; then . ~/.bash_login\n\
         elif [ -r ~/.profile ]; then . ~/.profile\n\
         fi\n\
         _silo_path=\":$PATH:\"\n\
         for _silo_dir in {lead}; do\n\
         \x20 while [[ $_silo_path == *\":$_silo_dir:\"* ]]; do _silo_path=${{_silo_path/\":$_silo_dir:\"/:}}; done\n\
         done\n\
         _silo_path=${{_silo_path#:}}; _silo_path=${{_silo_path%:}}\n\
         PATH={joined}${{_silo_path:+:$_silo_path}}\n\
         unset _silo_path _silo_dir\n"
    )
}

fn fish_snippet(home: &Path, lead: &[PathBuf]) -> String {
    let data = quote(&fish_data(home));
    let lead = quoted_list(lead);
    format!(
        "set -l __silo_dirs (string split : -- $XDG_DATA_DIRS | string match -v -- {data})\n\
         if set -q __silo_dirs[1]; set -gx XDG_DATA_DIRS (string join : -- $__silo_dirs); else; set -e XDG_DATA_DIRS; end\n\
         function __silo_path --on-event fish_prompt\n\
         \x20   functions -e __silo_path\n\
         \x20   fish_add_path --global --move --path {lead}\n\
         end\n"
    )
}

/// Write the integration files into `<home>/shell` (only files that differ).
///
/// # Errors
/// A directory or file cannot be written.
pub fn install(home: &Path, lead: &[PathBuf]) -> std::io::Result<()> {
    let mut files: Vec<(PathBuf, String)> = ZSH_FILES.iter().map(|f| (zsh_dir(home).join(f), zsh_file(home, lead, f))).collect();
    files.push((bash_rc(home), bash_rc_script(lead)));
    files.push((fish_data(home).join("fish/vendor_conf.d/silo.fish"), fish_snippet(home, lead)));
    for (path, text) in files {
        if std::fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
            continue;
        }
        let parent = path.parent().expect("integration files live in a directory");
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(".silo.tmp");
        std::fs::write(&tmp, &text)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(())
}

/// Whether `command` is the login shell run through [`hook`]'s arguments.
#[must_use]
pub fn is_hooked_login(command: &[OsString]) -> bool {
    command.len() == 3 && command[1] == "--rcfile" && Path::new(&command[2]).ends_with("shell/bash/rc")
}

/// Arguments and environment that make `shell` run Silo's integration.
#[must_use]
pub fn hook(shell: Shell, home: &Path) -> (Vec<OsString>, Vec<(OsString, OsString)>) {
    match shell {
        Shell::Zsh => {
            let mut env = vec![("ZDOTDIR".into(), zsh_dir(home).into_os_string())];
            if let Some(user) = std::env::var_os("ZDOTDIR").filter(|d| !d.is_empty()) {
                env.push(("SILO_ZDOTDIR".into(), user));
            }
            (Vec::new(), env)
        }
        Shell::Bash => (vec!["--rcfile".into(), bash_rc(home).into_os_string()], Vec::new()),
        Shell::Fish => {
            let rest = std::env::var_os("XDG_DATA_DIRS").filter(|d| !d.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".into());
            let dirs = std::iter::once(fish_data(home)).chain(std::env::split_paths(&rest));
            (Vec::new(), vec![("XDG_DATA_DIRS".into(), std::env::join_paths(dirs).unwrap_or(rest))])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(shell: &str, home: &Path, user: &Path, script: &str) -> Option<String> {
        let program = ["/bin", "/usr/bin", "/opt/homebrew/bin", "/usr/local/bin"].iter().map(|d| Path::new(d).join(shell)).find(|p| p.exists())?;
        let (args, env) = hook(Shell::of(&program)?, home);
        let out = std::process::Command::new(&program)
            .args(args)
            .args(if shell == "zsh" { &["-l", "-i", "-c"][..] } else { &["-i", "-c"][..] })
            .arg(script)
            .env_remove("ZDOTDIR")
            .envs(env)
            .env("HOME", user)
            .env("PATH", "/usr/bin:/bin")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .unwrap();
        Some(String::from_utf8(out.stdout).unwrap())
    }

    #[test]
    fn login_shells_lead_path_with_silo_after_user_files() {
        let root = std::env::temp_dir().join(format!("silo-shell-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (home, user) = (root.join("silo home"), root.join("user"));
        let lead = vec![home.join("shims"), home.join("bin")];
        std::fs::create_dir_all(user.join("zdot")).unwrap();
        for (file, text) in [
            (".zshenv", "ZDOTDIR=$HOME/zdot\nexport FROM_ENV=1\n"),
            ("zdot/.zprofile", "export PATH=/usr/bin:/bin\n"),
            ("zdot/.zshrc", "export PATH=/sbin:$PATH\nFROM_RC=1\n"),
            (".bash_profile", "export PATH=/usr/bin:/bin; [ -r ~/.bashrc ] && . ~/.bashrc\n"),
            (".bashrc", "PATH=/sbin:$PATH; FROM_RC=1\n"),
        ] {
            std::fs::write(user.join(file), text).unwrap();
        }
        install(&home, &lead).unwrap();
        let lead = lead.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().join(":");
        if let Some(out) = run("zsh", &home, &user, "print -r -- \"$PATH|$FROM_ENV$FROM_RC|${ZDOTDIR-unset}|${SILO_ZDOTDIR-unset}\"") {
            assert_eq!(out.trim(), format!("{lead}:/sbin:/usr/bin:/bin|11|{}|unset", user.join("zdot").display()));
        }
        if let Some(out) = run("bash", &home, &user, "PATH=/x:$PATH; . \"$HOME/../silo home/shell/bash/rc\"; echo \"$PATH|$FROM_RC\"") {
            assert_eq!(out.trim(), format!("{lead}:/sbin:/usr/bin:/bin|1"));
        }
        assert!(is_hooked_login(&[OsString::from("/bin/bash"), "--rcfile".into(), bash_rc(&home).into_os_string()]));
        let _ = std::fs::remove_dir_all(&root);
    }
}
