//! The languages Aqua knows by their file names, and the commands each one has (card OA4-05, task CU-06).
//!
//! The extension of a file, the ecosystem it belongs to and the check a language has were decided in five places (`coding_runner::check_for`, `code_verification::check_command_for`,
//! `coding_session::check_hint`, the extension list of `browser_agent` and the one of `code_verification`), and the lists did not agree. This is the one table. It decides nothing about
//! what may run: a command here is a suggestion, and `term`'s rules say whether it asks.

use serde::{Deserialize, Serialize};

/// A language by the extension of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,
    CSharp,
    Java,
    Kotlin,
    C,
    Cpp,
    Ruby,
    Php,
    Shell,
    PowerShell,
}

/// What a folder of code is built with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ecosystem {
    Rust,
    Node,
    Python,
    Go,
    DotNet,
    Maven,
    Gradle,
    CMake,
    Make,
    Deno,
    Php,
    Ruby,
}

impl Ecosystem {
    pub fn name(self) -> &'static str {
        match self {
            Ecosystem::Rust => "Rust",
            Ecosystem::Node => "JavaScript",
            Ecosystem::Python => "Python",
            Ecosystem::Go => "Go",
            Ecosystem::DotNet => ".NET",
            Ecosystem::Maven => "Maven",
            Ecosystem::Gradle => "Gradle",
            Ecosystem::CMake => "CMake",
            Ecosystem::Make => "Make",
            Ecosystem::Deno => "Deno",
            Ecosystem::Php => "PHP",
            Ecosystem::Ruby => "Ruby",
        }
    }
}

impl Language {
    pub fn name(self) -> &'static str {
        match self {
            Language::Rust => "Rust",
            Language::Python => "Python",
            Language::JavaScript => "JavaScript",
            Language::TypeScript => "TypeScript",
            Language::Go => "Go",
            Language::CSharp => "C#",
            Language::Java => "Java",
            Language::Kotlin => "Kotlin",
            Language::C => "C",
            Language::Cpp => "C++",
            Language::Ruby => "Ruby",
            Language::Php => "PHP",
            Language::Shell => "shell",
            Language::PowerShell => "PowerShell",
        }
    }

    /// The ecosystem whose project holds this language's files, when it has a usual one.
    pub fn ecosystem(self) -> Option<Ecosystem> {
        match self {
            Language::Rust => Some(Ecosystem::Rust),
            Language::Python => Some(Ecosystem::Python),
            Language::JavaScript | Language::TypeScript => Some(Ecosystem::Node),
            Language::Go => Some(Ecosystem::Go),
            Language::CSharp => Some(Ecosystem::DotNet),
            Language::Java | Language::Kotlin => Some(Ecosystem::Maven),
            Language::C | Language::Cpp => Some(Ecosystem::CMake),
            Language::Ruby => Some(Ecosystem::Ruby),
            Language::Php => Some(Ecosystem::Php),
            Language::Shell | Language::PowerShell => None,
        }
    }

    /// The language server that is the usual program for this language. Nothing here starts it.
    pub fn language_server(self) -> Option<&'static str> {
        match self {
            Language::Rust => Some("rust-analyzer"),
            Language::JavaScript | Language::TypeScript => Some("typescript-language-server"),
            Language::Python => Some("pyright-langserver"),
            _ => None,
        }
    }

    /// The command that runs this language's tests, as a program and its arguments (what the check after an edit names). `None` for a language with no usual one here.
    pub fn test_command(self) -> Option<(&'static str, &'static [&'static str])> {
        match self {
            Language::Rust => Some(("cargo", &["test", "--offline"])),
            Language::Python => Some(("python", &["-m", "pytest"])),
            Language::JavaScript | Language::TypeScript => Some(("npm", &["test"])),
            _ => None,
        }
    }

    /// The quick check that follows an edit of a file of this language (card OA4-04, tasks AG-07 and CU-06): the compiler or the type checker, not the tests. `path` is the file that was
    /// changed (a Python file is compiled by name). `None` for a language with no usual one here. The three tables of the agent loop that named a command after an edit (the one that
    /// runs it, the one that tells the model and the one that tells it what to run before it says it is done) are views of this and of [`Language::test_command`].
    pub fn edit_check(self, path: &str) -> Option<(&'static str, Vec<String>)> {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<String>>();
        match self {
            Language::Rust => Some(("cargo", args(&["check", "--offline", "--message-format", "short"]))),
            Language::Python => Some(("python", vec!["-m".into(), "py_compile".into(), path.to_string()])),
            Language::JavaScript | Language::TypeScript => Some(("npm", args(&["run", "typecheck", "--if-present"]))),
            _ => None,
        }
    }
}

/// The language of a file by its extension (case does not matter), or `None` for a document, a setting or a name that has none.
pub fn language_of(path: &str) -> Option<Language> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    Some(match ext.to_ascii_lowercase().as_str() {
        "rs" => Language::Rust,
        "py" | "pyw" => Language::Python,
        "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
        "ts" | "tsx" | "mts" | "cts" => Language::TypeScript,
        "go" => Language::Go,
        "cs" => Language::CSharp,
        "java" => Language::Java,
        "kt" | "kts" => Language::Kotlin,
        "c" | "h" => Language::C,
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => Language::Cpp,
        "rb" => Language::Ruby,
        "php" => Language::Php,
        "sh" | "bash" => Language::Shell,
        "ps1" => Language::PowerShell,
        _ => return None,
    })
}

/// Is this the name of a file of source code (not a document or a setting)?
pub fn is_source_path(path: &str) -> bool {
    language_of(path).is_some()
}

/// The names that hold a project's own description of how it is built, for the folders `detect` reads. Kept here so that the list of markers is one list.
pub const PROJECT_MARKERS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "pytest.ini",
    "tox.ini",
    "go.mod",
    "go.work",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "CMakeLists.txt",
    "Makefile",
    "deno.json",
    "deno.jsonc",
    "composer.json",
    "Gemfile",
    "global.json",
];

/// Does a folder's file name mark a project root? (`.git` and the `.sln`, `.csproj`, `.fsproj` and `.slnx` files are checked by [`is_marker_name`] too.)
pub fn is_marker_name(name: &str) -> bool {
    if PROJECT_MARKERS.contains(&name) {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    name == ".git" || [".sln", ".slnx", ".csproj", ".fsproj"].iter().any(|e| lower.ends_with(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_file_has_a_language_and_a_document_or_a_dotfile_has_none() {
        assert_eq!(language_of("src/lib.rs"), Some(Language::Rust));
        assert_eq!(language_of(r"C:\work\App.TSX"), Some(Language::TypeScript));
        assert_eq!(language_of("tools/run.py"), Some(Language::Python));
        assert_eq!(language_of("Program.cs"), Some(Language::CSharp));
        assert_eq!(language_of("README.md"), None);
        assert_eq!(language_of("settings.json"), None);
        assert_eq!(language_of(".rs"), None, "a name that is only an extension is no file of that language");
        assert_eq!(language_of("Makefile"), None);
        assert!(is_source_path("a/b/c.go") && !is_source_path("a/b/c.txt"));
    }

    #[test]
    fn the_test_command_and_the_language_server_are_the_ones_the_check_after_an_edit_has_always_named() {
        assert_eq!(Language::Rust.test_command(), Some(("cargo", &["test", "--offline"][..])));
        assert_eq!(Language::Python.test_command(), Some(("python", &["-m", "pytest"][..])));
        assert_eq!(Language::TypeScript.test_command(), Some(("npm", &["test"][..])));
        assert_eq!(Language::Go.test_command(), None);
        assert_eq!(Language::Rust.language_server(), Some("rust-analyzer"));
        assert_eq!(Language::JavaScript.language_server(), Some("typescript-language-server"));
        assert_eq!(Language::Python.language_server(), Some("pyright-langserver"));
        assert_eq!(Language::Java.language_server(), None);
    }

    /// Card OA4-04 (CU-06): the quick check after an edit is the one the loop has always run, from this table, for the languages that have one.
    #[test]
    fn the_check_after_an_edit_is_the_one_the_loop_has_always_run() {
        let show = |l: Language, path: &str| l.edit_check(path).map(|(p, a)| format!("{p} {}", a.join(" ")));
        assert_eq!(show(Language::Rust, "src/lib.rs").as_deref(), Some("cargo check --offline --message-format short"));
        assert_eq!(show(Language::Python, "tools/run.py").as_deref(), Some("python -m py_compile tools/run.py"), "a Python file is compiled by name");
        assert_eq!(show(Language::TypeScript, "web/App.tsx").as_deref(), Some("npm run typecheck --if-present"));
        assert_eq!(show(Language::JavaScript, "web/app.js").as_deref(), Some("npm run typecheck --if-present"));
        for l in [Language::Go, Language::Java, Language::CSharp, Language::Shell, Language::Php] {
            assert!(l.edit_check("x").is_none(), "{l:?}");
        }
        // Every language with a quick check has the tests command too, and the path a file goes by decides the language.
        for path in ["a.rs", "a.py", "a.ts", "a.tsx", "a.js", "a.jsx", "a.mjs", "a.pyw"] {
            let l = language_of(path).unwrap();
            assert!(l.edit_check(path).is_some() && l.test_command().is_some(), "{path}");
        }
    }

    #[test]
    fn a_project_marker_is_a_manifest_a_git_folder_or_a_solution_file() {
        for n in ["Cargo.toml", "package.json", ".git", "App.sln", "x.csproj", "pom.xml", "go.mod"] {
            assert!(is_marker_name(n), "{n}");
        }
        for n in ["main.rs", "README.md", "src", "Cargo.lock"] {
            assert!(!is_marker_name(n), "{n}");
        }
    }
}
