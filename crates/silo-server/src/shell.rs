//! Shell integration that puts `<SILO_HOME>/shims` (and `<SILO_HOME>/bin`)
//! back in front of `PATH` after the user's startup files ran, since login
//! files (macOS `path_helper`, Debian `/etc/profile`) reorder or reset it.
//!
//! zsh runs through a `ZDOTDIR` wrapper, bash through `--rcfile`, fish
//! through an `XDG_DATA_DIRS` `vendor_conf.d` snippet; other shells only get
//! the environment.
//!
//! Interactive shells also get OSC 133 marks (A prompt, B input, C output
//! with `cmdline_url`, D end with the exit code), chained onto the user's
//! hooks. They are left out when something already emits them: fish 4+
//! itself, or a user integration whose functions or prompt contain `133;`.

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

/// zsh marks, installed after the user's files ran.
const ZSH_MARKS: &str = r#"  if [[ -o interactive && ${(v)functions}$PS1 != *'133;'* ]]; then
    typeset -g __silo_b=$'%{\e]133;B\a%}' __silo_ran=
    __silo_precmd() {
      local ret=$?
      [[ -n $__silo_ran ]] && builtin print -rn -- $'\e]133;D;'$ret$'\a'
      __silo_ran=
      builtin print -rn -- $'\e]133;A\a'
    }
    __silo_ps1() { [[ $PS1 == *"$__silo_b" ]] || PS1+=$__silo_b; }
    __silo_preexec() {
      emulate -L zsh
      setopt no_multibyte
      __silo_ran=1
      local s=$1 o= c i
      for (( i = 1; i <= $#s; i++ )); do
        c=$s[i]
        if [[ $c == [a-zA-Z0-9._~-] ]]; then o+=$c; else o+=%${(l:2::0:)$(( [##16] #c ))}; fi
      done
      builtin print -rn -- $'\e]133;C'${s:+";cmdline_url=$o"}$'\a'
    }
    precmd_functions=(__silo_precmd $precmd_functions __silo_ps1)
    preexec_functions=($preexec_functions __silo_preexec)
  fi
"#;

fn zsh_file(home: &Path, lead: &[PathBuf], file: &str) -> String {
    let ours = quote(&zsh_dir(home));
    let lead = quoted_list(lead);
    let done = match file {
        ".zshenv" => "[[ ! -o login && ! -o interactive ]]",
        ".zshrc" => "[[ ! -o login ]]",
        ".zlogin" => "true",
        _ => "false",
    };
    let marks = if matches!(file, ".zshrc" | ".zlogin") { ZSH_MARKS } else { "" };
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
         {marks}\
         fi\n"
    )
}

/// bash marks, installed after the user's files ran. Bash 3.2 has no
/// `PS0`, so it gets A/B and a D at every prompt after the first.
const BASH_MARKS: &str = r#"if [[ $- == *i* && $(builtin declare -f)${PS0-}${PS1-}${PROMPT_COMMAND-} != *'133;'* ]]; then
  __silo_prompt() {
    local ret=$?
    [[ -n $__silo_ran ]] && builtin printf '\e]133;D;%s\a' "$ret"
    __silo_ran=$__silo_d __silo_h=$HISTCMD
    builtin printf '\e]133;A\a'
    return $ret
  }
  __silo_ps() {
    local ret=$?
    [[ $PS1 == *"$__silo_b" ]] || PS1+=$__silo_b
    [[ -z $__silo_c || ${PS0-} == *"$__silo_c" ]] || PS0+=$__silo_c
    return $ret
  }
  __silo_cmd() {
    local LC_ALL=C s= o= c i
    if [[ $HISTCMD == $((__silo_h + 1)) ]]; then
      s=$(HISTTIMEFORMAT= builtin history 1)
      s=${s#"${s%%[![:space:]]*}"} s=${s#"${s%%[!0-9]*}"} s=${s#\*} s=${s#  }
    fi
    for (( i = 0; i < ${#s}; i++ )); do
      c=${s:i:1}
      case $c in
        [a-zA-Z0-9._~-]) o+=$c ;;
        *) builtin printf -v c '%d' "'$c"; builtin printf -v c '%%%02X' $((c & 255)); o+=$c ;;
      esac
    done
    builtin printf '\e]133;C%s\a' "${s:+;cmdline_url=$o}"
  }
  __silo_b='\[\e]133;B\a\]' __silo_c= __silo_d=1 __silo_ran= __silo_h= __silo_x=([1]=)
  if (( BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 4) )); then
    __silo_c='${__silo_x[__silo_ran=1]}$(__silo_cmd)' __silo_d=
  fi
  PROMPT_COMMAND=$'__silo_prompt\n'${PROMPT_COMMAND:+$PROMPT_COMMAND$'\n'}__silo_ps
fi
"#;

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
         unset _silo_path _silo_dir\n\
         {BASH_MARKS}"
    )
}

/// fish marks, decided at the first prompt (vendor snippets run before the
/// user's config). fish 4+ marks prompts itself unless `mark-prompt` is off.
const FISH_MARKS: &str = r#"if status is-interactive
    function __silo_marks --on-event fish_prompt
        if not set -q __silo_marks
            set -g __silo_marks 0
            status test-feature mark-prompt 2>/dev/null
            set -l native $status
            if string match -qr '^[0-3]\.' -- $version; or test $native -eq 1
                set -g __silo_marks 1
                for f in fish_prompt (functions --handlers-type generic | string replace -rf '^fish_(?:prompt|preexec|postexec) ' '' | string match -v -- '__silo_*')
                    if functions -q -- $f; and functions -- $f | string match -q -- '*133;*'
                        set __silo_marks 0
                    end
                end
            end
            if test $__silo_marks = 0
                functions -e __silo_marks
                return
            end
            function __silo_mark_c --on-event fish_preexec
                printf '\e]133;C;cmdline_url=%s\a' (string escape --style=url -- $argv[1])
            end
            function __silo_mark_d --on-event fish_postexec
                printf '\e]133;D;%s\a' $status
            end
        end
        printf '\e]133;A\a'
    end
end
"#;

fn fish_snippet(home: &Path, lead: &[PathBuf]) -> String {
    let data = quote(&fish_data(home));
    let lead = quoted_list(lead);
    format!(
        "set -l __silo_dirs (string split : -- $XDG_DATA_DIRS | string match -v -- {data})\n\
         if set -q __silo_dirs[1]; set -gx XDG_DATA_DIRS (string join : -- $__silo_dirs); else; set -e XDG_DATA_DIRS; end\n\
         function __silo_path --on-event fish_prompt\n\
         \x20   functions -e __silo_path\n\
         \x20   fish_add_path --global --move --path {lead}\n\
         end\n\
         {FISH_MARKS}"
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
    use std::{
        io::{Read, Write},
        sync::mpsc,
        time::{Duration, Instant},
    };

    use super::*;

    /// Every distinct installed `shell`, first from `SILO_TEST_SHELL_DIRS`.
    fn programs(shell: &str) -> Vec<PathBuf> {
        let extra = std::env::var_os("SILO_TEST_SHELL_DIRS").unwrap_or_default();
        let dirs = std::env::split_paths(&extra).filter(|d| !d.as_os_str().is_empty());
        let mut seen = Vec::new();
        let mut found = Vec::new();
        for program in dirs.chain(["/bin", "/usr/bin", "/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from)).map(|d| d.join(shell)) {
            if let Ok(real) = std::fs::canonicalize(&program)
                && !seen.contains(&real)
            {
                seen.push(real);
                found.push(program);
            }
        }
        found
    }

    /// Arguments and environment that keep a test shell inside `user`.
    fn sandbox(program: &Path, home: &Path, user: &Path) -> (Vec<OsString>, Vec<(OsString, OsString)>) {
        let (args, mut env) = hook(Shell::of(program).unwrap(), home);
        env.retain(|(key, _)| key != "SILO_ZDOTDIR");
        for (key, value) in [
            ("HOME", user.to_path_buf()),
            ("PATH", "/usr/bin:/bin".into()),
            ("XDG_CONFIG_HOME", user.join(".config")),
            ("XDG_DATA_HOME", user.join(".local/share")),
            ("XDG_CACHE_HOME", user.join(".cache")),
            ("SILO_ZDOTDIR", PathBuf::new()),
            ("PROMPT_COMMAND", PathBuf::new()),
            ("BASH_SILENCE_DEPRECATION_WARNING", "1".into()),
        ] {
            env.push((key.into(), value.into_os_string()));
        }
        (args, env)
    }

    fn run(program: &Path, flags: &[&str], home: &Path, user: &Path, script: &str) -> String {
        let (args, env) = sandbox(program, home, user);
        let out = std::process::Command::new(program)
            .args(args)
            .args(flags)
            .arg("-c")
            .arg(script)
            .env_remove("ZDOTDIR")
            .envs(env)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    fn count(haystack: &[u8], needle: &[u8]) -> usize {
        haystack.windows(needle.len()).filter(|w| *w == needle).count()
    }

    /// Type `lines` into an interactive `program` on a PTY, one per
    /// `ready> ` prompt, and return everything it wrote.
    fn session(program: &Path, flags: &[&str], home: &Path, user: &Path, lines: &[&str]) -> String {
        let (args, env) = sandbox(program, home, user);
        let mut command = vec![program.as_os_str().to_owned()];
        command.extend(args);
        command.extend(flags.iter().map(OsString::from));
        let config = silo_pty::SpawnConfig { command, cwd: Some(user.to_path_buf()), env };
        let size = silo_pty::PtySize { columns: 200, rows: 50, cell_width: 8, cell_height: 16 };
        let pty = silo_pty::Pty::spawn(&config, size, |_| {}).unwrap();
        let (mut reader, mut writer) = (pty.reader().unwrap(), pty.writer().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let (mut out, mut sent, mut answered) = (Vec::new(), 0, 0);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "{} timed out: {:?}", program.display(), String::from_utf8_lossy(&out));
            match rx.recv_timeout(left) {
                Ok(chunk) => out.extend(chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            // fish waits for the terminal to answer its device-attribute query.
            let queries = count(&out, b"\x1b[c") + count(&out, b"\x1b[0c");
            for _ in answered..queries {
                writer.write_all(b"\x1b[?62;22c").unwrap();
            }
            answered = queries;
            if sent < lines.len() && count(&out, b"ready> ") > sent {
                writer.write_all(format!("{}\r", lines[sent]).as_bytes()).unwrap();
                sent += 1;
            }
        }
        assert_eq!(sent, lines.len(), "{} exited early: {:?}", program.display(), String::from_utf8_lossy(&out));
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The OSC 133 marks in `out`, without the `133;` prefix.
    fn marks(out: &str) -> Vec<String> {
        out.split("\x1b]133;").skip(1).map(|m| m.split(['\x07', '\x1b']).next().unwrap_or_default().to_owned()).collect()
    }

    const LINES: &[&str] = &["true", "false", "", "echo \"a;b c%\"", "sh -c 'exit 3'", "exit"];

    /// The marks [`LINES`] produce: A (and B) per prompt, then C with the
    /// command line and D with its status for each command that ran.
    fn expected(b: bool, c: bool, empty_d: bool) -> Vec<String> {
        let runs = [("true", "0"), ("false", "1"), ("", "1"), ("echo%20%22a%3Bb%20c%25%22", "0"), ("sh%20-c%20%27exit%203%27", "3"), ("exit", "")];
        let mut want = Vec::new();
        for (cmdline, status) in runs {
            want.push("A".to_owned());
            if b {
                want.push("B".to_owned());
            }
            if c && !cmdline.is_empty() {
                want.push(format!("C;cmdline_url={cmdline}"));
            }
            if !status.is_empty() && (!cmdline.is_empty() || empty_d) {
                want.push(format!("D;{status}"));
            }
        }
        want
    }

    /// The user's hooks ran and still saw each command's status.
    fn user_hooks_ran(out: &str) -> bool {
        ["<pc1>", "1 ready> ", "<pc3>", "3 ready> "].iter().all(|s| out.contains(s))
    }

    #[test]
    fn login_shells_lead_path_with_silo_after_user_files() {
        let root = std::env::temp_dir().join(format!("silo-shell-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (home, user, own) = (root.join("silo home"), root.join("user"), root.join("own"));
        let lead = vec![home.join("shims"), home.join("bin")];
        std::fs::create_dir_all(user.join("zdot")).unwrap();
        std::fs::create_dir_all(user.join(".config/fish")).unwrap();
        std::fs::create_dir_all(own.join(".config/fish")).unwrap();
        for (file, text) in [
            ("user/.zshenv", "ZDOTDIR=$HOME/zdot\nexport FROM_ENV=1\n"),
            ("user/zdot/.zprofile", "export PATH=/usr/bin:/bin\n"),
            ("user/zdot/.zshrc", "export PATH=/sbin:$PATH\nFROM_RC=1\nPS1='%? ready> '\nprecmd() { print -rn -- \"<pc$?>\"; }\n"),
            ("user/.bash_profile", "export PATH=/usr/bin:/bin; [ -r ~/.bashrc ] && . ~/.bashrc\n"),
            ("user/.bashrc", "PATH=/sbin:$PATH; FROM_RC=1\nPS1='$? ready> '\nPROMPT_COMMAND='printf \"<pc%s>\" $?'\n"),
            (
                "user/.config/fish/config.fish",
                "function fish_prompt; printf '%s ready> ' $status; end\nfunction __u --on-event fish_postexec; printf '<pc%s>' $status; end\n",
            ),
            ("own/.zshrc", "__mine() { print -rn -- $'\\e]133;A\\a'; }\nprecmd_functions=(__mine)\n"),
            ("own/.bash_profile", "PROMPT_COMMAND='printf \"\\e]133;A\\a\"'\n"),
            (
                "own/.config/fish/config.fish",
                "function fish_prompt; printf 'ready> '; end\nfunction __mine --on-event fish_prompt; printf '\\e]133;A\\a'; end\n",
            ),
        ] {
            std::fs::write(root.join(file), text).unwrap();
        }
        install(&home, &lead).unwrap();
        let lead = lead.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().join(":");

        for zsh in programs("zsh") {
            let out = run(&zsh, &["-l", "-i"], &home, &user, "print -r -- \"$PATH|$FROM_ENV$FROM_RC|${ZDOTDIR-unset}|${SILO_ZDOTDIR-unset}\"");
            assert_eq!(out.trim(), format!("{lead}:/sbin:/usr/bin:/bin|11|{}|unset", user.join("zdot").display()));
            let out = session(&zsh, &["-l"], &home, &user, LINES);
            assert_eq!(marks(&out), expected(true, true, false), "{}: {out:?}", zsh.display());
            assert!(user_hooks_ran(&out), "{}: {out:?}", zsh.display());
            let out = run(&zsh, &["-l"], &home, &user, "print -r -- \"$precmd_functions|${(k)functions[(I)__silo_*]}\"");
            assert_eq!(out, "|\n", "{}: non-interactive", zsh.display());
            let out = run(&zsh, &["-i"], &home, &own, "print -r -- $precmd_functions");
            assert_eq!(out, "__mine\n", "{}: the user's own marks", zsh.display());
            eprintln!("marks: {} ok", zsh.display());
        }
        for bash in programs("bash") {
            let out = run(&bash, &["-i"], &home, &user, "PATH=/x:$PATH; . \"$HOME/../silo home/shell/bash/rc\"; echo \"$PATH|$FROM_RC\"");
            assert_eq!(out.trim(), format!("{lead}:/sbin:/usr/bin:/bin|1"));
            let version = run(&bash, &[], &home, &user, "echo \"${BASH_VERSINFO[0]}${BASH_VERSINFO[1]}\"");
            let ps0 = version.trim().parse::<u32>().unwrap() >= 44;
            let out = session(&bash, &["-i"], &home, &user, LINES);
            assert_eq!(marks(&out), expected(true, ps0, !ps0), "{}: {out:?}", bash.display());
            assert!(user_hooks_ran(&out), "{}: {out:?}", bash.display());
            let out = run(
                &bash,
                &[],
                &home,
                &user,
                ". \"$HOME/../silo home/shell/bash/rc\"; echo \"$PROMPT_COMMAND|${PS0-}|$(declare -F | grep __silo_)\"",
            );
            assert_eq!(out, "printf \"<pc%s>\" $?||\n", "{}: non-interactive", bash.display());
            let out = run(&bash, &["-i"], &home, &own, "echo \"$PROMPT_COMMAND\"");
            assert_eq!(out, "printf \"\\e]133;A\\a\"\n", "{}: the user's own marks", bash.display());
            eprintln!("marks: {} (PS0: {ps0}) ok", bash.display());
        }
        for (n, fish) in programs("fish").into_iter().enumerate() {
            // Each build gets its own data: fish 4.3+ migrates the universal
            // variables an older fish left (printing a notice and starting a
            // `sleep 7` job that makes the first `exit` refuse). The cache
            // stops the detached man-page completion run on first start.
            let (user, own) = (root.join(format!("fish{n}/user")), root.join(format!("fish{n}/own")));
            for (from, to) in [(&root.join("user"), &user), (&root.join("own"), &own)] {
                std::fs::create_dir_all(to.join(".config/fish")).unwrap();
                std::fs::create_dir_all(to.join(".cache/fish/generated_completions")).unwrap();
                std::fs::copy(from.join(".config/fish/config.fish"), to.join(".config/fish/config.fish")).unwrap();
            }
            let (user, own) = (&user, &own);
            let out = run(&fish, &[], &home, user, "functions -q __silo_marks; or echo none");
            assert_eq!(out, "none\n", "{}: non-interactive", fish.display());
            let native = run(&fish, &[], &home, user, "string match -qr '^[0-3]\\.' -- $version; or echo native") == "native\n";
            // fish runs `fish_postexec` for `exit` too, with the last status.
            let ours = [expected(false, true, false), vec!["D;3".to_owned()]].concat();
            let out = session(&fish, &["--features", "no-mark-prompt"], &home, user, LINES);
            assert_eq!(marks(&out), ours, "{}: {out:?}", fish.display());
            assert!(user_hooks_ran(&out), "{}: {out:?}", fish.display());
            assert!(!out.contains("fish:"), "{}: nothing from fish about our snippet: {out:?}", fish.display());
            let out = session(&fish, &[], &home, user, LINES);
            if native {
                let ends = marks(&out).iter().filter(|m| m.starts_with(['C', 'D'])).map(|m| &m[..1]).collect::<String>();
                assert_eq!(ends, "CDCDCDCDCD", "{}: one C/D per command: {out:?}", fish.display());
            } else {
                assert_eq!(marks(&out), ours, "{}: {out:?}", fish.display());
            }
            assert!(!out.contains("fish:"), "{}: nothing from fish about our snippet: {out:?}", fish.display());
            let out = session(&fish, &["--features", "no-mark-prompt"], &home, own, &["exit"]);
            assert_eq!(marks(&out), ["A"], "{}: the user's own marks", fish.display());
            eprintln!("marks: {} (native: {native}) ok", fish.display());
        }
        assert!(is_hooked_login(&[OsString::from("/bin/bash"), "--rcfile".into(), bash_rc(&home).into_os_string()]));
        let _ = std::fs::remove_dir_all(&root);
    }
}
