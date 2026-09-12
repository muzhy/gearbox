use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

pub struct FileTool {
    workspace: PathBuf,
    protected_config: PathBuf,
    max_file_bytes: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileArguments {
    path: String,
}

impl FileTool {
    // The CLI supplies canonical paths and validated limits.
    pub fn new(workspace: PathBuf, protected_config: PathBuf, max_file_bytes: u64) -> Self {
        assert!(max_file_bytes > 0 && max_file_bytes.checked_add(1).is_some());
        Self {
            workspace,
            protected_config,
            max_file_bytes,
        }
    }

    pub fn definition(&self) -> Value {
        json!({
            "type": "function",
            "name": "read_file",
            "description": format!(
                "Read a UTF-8 file inside the working directory, up to {} bytes. The active configuration file cannot be read.",
                self.max_file_bytes
            ),
            "strict": true,
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "A relative path inside the working directory."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        })
    }

    pub fn execute(&self, name: &str, arguments: &str) -> String {
        let result = if name == "read_file" {
            self.read_file(arguments)
        } else {
            tracing::warn!(tool = %name.escape_debug(), "unknown tool");
            Err("unknown tool; only read_file is supported")
        };
        match result {
            Ok(content) => json!({"ok": true, "content": content}).to_string(),
            Err(error) => json!({"ok": false, "error": error}).to_string(),
        }
    }

    fn read_file(&self, arguments: &str) -> Result<String, &'static str> {
        // Serde's struct deserializer also accepts positional arrays; the tool API
        // requires an object, while the typed parser below rejects duplicate keys.
        if !arguments.trim_start().starts_with('{') {
            return Err("read_file requires an object containing only a string path");
        }
        let arguments: ReadFileArguments = serde_json::from_str(arguments)
            .map_err(|_| "read_file requires an object containing only a string path")?;
        tracing::info!("tool: read_file path=\"{}\"", arguments.path.escape_debug());
        if arguments.path.trim().is_empty() {
            return Err("path must not be empty or whitespace");
        }
        let path = Path::new(&arguments.path);
        // Check Windows forms on every platform, including drive-relative C:foo.
        let bytes = arguments.path.as_bytes();
        if arguments.path.starts_with(['/', '\\'])
            || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
            || path
                .components()
                .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
        {
            return Err("path must be relative, without a root or drive prefix");
        }
        #[cfg(windows)]
        if arguments.path.contains(':') {
            return Err("Windows alternate data stream paths are not supported");
        }

        let resolved = fs::canonicalize(self.workspace.join(path))
            .map_err(|_| "file does not exist or cannot be accessed")?;
        if !resolved.starts_with(&self.workspace) {
            return Err("path resolves outside the working directory");
        }
        let metadata = fs::metadata(&resolved).map_err(|_| "cannot inspect file")?;
        if !metadata.is_file() {
            return Err("path must refer to a regular file");
        }
        if resolved == self.protected_config
            || same_file::is_same_file(&resolved, &self.protected_config)
                .map_err(|_| "cannot verify file identity against the active configuration")?
        {
            return Err("reading the active configuration file is forbidden");
        }

        let file = File::open(&resolved).map_err(|_| "cannot open file for reading")?;
        let mut bytes = Vec::new();
        file.take(self.max_file_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot read file")?;
        if bytes.len() as u64 > self.max_file_bytes {
            return Err("file exceeds the configured max_file_bytes limit");
        }
        String::from_utf8(bytes).map_err(|_| "file is not valid UTF-8")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{TempDir, tempdir};

    struct Fixture {
        _temp: TempDir,
        workspace: PathBuf,
        config: PathBuf,
        tool: FileTool,
    }

    impl Fixture {
        fn new(max_file_bytes: u64) -> Self {
            let temp = tempdir().unwrap();
            let workspace = temp.path().join("work");
            fs::create_dir(&workspace).unwrap();
            let config = workspace.join("config.toml");
            fs::write(&config, "secret configuration").unwrap();
            let workspace = fs::canonicalize(workspace).unwrap();
            let config = fs::canonicalize(config).unwrap();
            let tool = FileTool::new(workspace.clone(), config.clone(), max_file_bytes);
            Self {
                _temp: temp,
                workspace,
                config,
                tool,
            }
        }

        fn read(&self, path: &str) -> Value {
            self.call("read_file", &json!({"path": path}).to_string())
        }

        fn call(&self, name: &str, arguments: &str) -> Value {
            serde_json::from_str(&self.tool.execute(name, arguments)).unwrap()
        }
    }

    fn assert_error(result: &Value, message: &str) {
        assert_eq!(result["ok"], false, "{result}");
        assert!(
            result["error"].as_str().unwrap().contains(message),
            "{result}"
        );
        assert!(result.get("content").is_none());
    }

    #[test]
    fn reads_utf8_empty_files_and_exact_size_limit() {
        let fixture = Fixture::new(6);
        fs::write(fixture.workspace.join("name with spaces.txt"), "你好").unwrap();
        fs::write(fixture.workspace.join("empty.txt"), "").unwrap();
        assert_eq!(
            fixture.read("name with spaces.txt"),
            json!({"ok": true, "content": "你好"})
        );
        assert_eq!(
            fixture.read("empty.txt"),
            json!({"ok": true, "content": ""})
        );
        fs::write(fixture.workspace.join("too-big.txt"), "你好!").unwrap();
        assert_error(&fixture.read("too-big.txt"), "max_file_bytes");
    }

    #[test]
    fn file_size_and_tool_description_use_configured_limit() {
        let fixture = Fixture::new(3);
        fs::write(fixture.workspace.join("file.txt"), "four").unwrap();
        assert_error(&fixture.read("file.txt"), "max_file_bytes");
        let larger = FileTool::new(fixture.workspace.clone(), fixture.config.clone(), 4);
        let result: Value =
            serde_json::from_str(&larger.execute("read_file", r#"{"path":"file.txt"}"#)).unwrap();
        assert_eq!(result, json!({"ok": true, "content": "four"}));
        assert!(
            larger.definition()["description"]
                .as_str()
                .unwrap()
                .contains("4 bytes")
        );
    }

    #[test]
    fn rejects_unknown_tools_and_invalid_argument_shapes() {
        let fixture = Fixture::new(100);
        assert_error(&fixture.call("write_file", "{}"), "unknown tool");
        for arguments in [
            "not json",
            "null",
            "[]",
            r#"["file.txt"]"#,
            "{}",
            r#"{"path":null}"#,
            r#"{"path":1}"#,
            r#"{"path":true}"#,
            r#"{"path":{}}"#,
            r#"{"path":"file.txt","extra":true}"#,
            r#"{"path":"a","path":"b"}"#,
        ] {
            assert_error(&fixture.call("read_file", arguments), "only a string path");
        }
        for path in ["", " ", "\t\n"] {
            assert_error(&fixture.read(path), "must not be empty");
        }
    }

    #[test]
    fn rejects_unreadable_kinds_and_invalid_utf8() {
        let fixture = Fixture::new(100);
        assert_error(&fixture.read("missing.txt"), "does not exist");
        assert_error(&fixture.read("."), "regular file");
        fs::write(fixture.workspace.join("binary"), [0xff, 0xfe]).unwrap();
        assert_error(&fixture.read("binary"), "not valid UTF-8");
    }

    #[test]
    fn rejects_rooted_and_drive_relative_paths() {
        let fixture = Fixture::new(100);
        for path in [
            "/etc/passwd",
            "\\file",
            "C:\\file",
            "C:/file",
            "C:file",
            "\\\\server\\share\\file",
            "\\\\?\\C:\\file",
        ] {
            assert_error(&fixture.read(path), "must be relative");
        }
        assert_error(
            &fixture.read(fixture.config.to_str().unwrap()),
            "must be relative",
        );
    }

    #[test]
    fn checks_real_path_boundary_and_protects_configuration() {
        let fixture = Fixture::new(100);
        let outside = fixture._temp.path().join("outside.txt");
        fs::write(outside, "external content").unwrap();
        assert_error(
            &fixture.read("../outside.txt"),
            "outside the working directory",
        );
        assert_error(
            &fixture.read("config.toml"),
            "configuration file is forbidden",
        );
        // A shared textual prefix must not count as containment.
        let adjacent = fixture._temp.path().join("work-extra");
        fs::create_dir(&adjacent).unwrap();
        fs::write(adjacent.join("file"), "external content").unwrap();
        assert_error(
            &fixture.read("../work-extra/file"),
            "outside the working directory",
        );
        let error = fixture.read("../outside.txt").to_string();
        assert!(!error.contains("external content"));
        assert!(!error.contains(fixture._temp.path().to_str().unwrap()));
    }

    #[test]
    fn rejects_configuration_hard_links() {
        let fixture = Fixture::new(100);
        fs::hard_link(&fixture.config, fixture.workspace.join("config-alias.txt")).unwrap();
        assert_error(
            &fixture.read("config-alias.txt"),
            "configuration file is forbidden",
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_alternate_data_streams() {
        let fixture = Fixture::new(100);
        for path in [
            "config.toml::$DATA",
            "config.toml:secret",
            "dir/file:stream",
        ] {
            assert_error(&fixture.read(path), "alternate data stream");
        }
    }

    #[test]
    fn rejects_directory_links_outside_workspace_and_to_configuration() {
        let fixture = Fixture::new(100);
        let outside = fixture._temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("file"), "external content").unwrap();
        directory_link(&outside, &fixture.workspace.join("outside-link"));
        assert_error(
            &fixture.read("outside-link/file"),
            "outside the working directory",
        );
        directory_link(&fixture.workspace, &fixture.workspace.join("inside-link"));
        assert_error(
            &fixture.read("inside-link/config.toml"),
            "configuration file is forbidden",
        );
        fs::write(fixture.workspace.join("readable"), "allowed").unwrap();
        assert_eq!(
            fixture.read("inside-link/readable"),
            json!({"ok": true, "content": "allowed"})
        );
    }

    #[cfg(unix)]
    fn directory_link(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[cfg(windows)]
    fn directory_link(target: &Path, link: &Path) {
        // Junctions exercise canonicalization without requiring symlink privileges.
        // Windows PowerShell writes an invalid junction target for verbatim paths.
        let shell_path = |path: &Path| {
            let path = path.to_str().expect("test paths must be valid Unicode");
            if let Some(unc_path) = path.strip_prefix(r"\\?\UNC\") {
                format!(r"\\{unc_path}")
            } else {
                path.strip_prefix(r"\\?\").unwrap_or(path).to_owned()
            }
        };
        let output = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile", "-NonInteractive", "-Command",
                "$ErrorActionPreference = 'Stop'; New-Item -ItemType Junction -Path $env:GEARBOX_TEST_LINK -Target $env:GEARBOX_TEST_TARGET | Out-Null",
            ])
            .env("GEARBOX_TEST_LINK", shell_path(link))
            .env("GEARBOX_TEST_TARGET", shell_path(target))
            .output()
            .expect("PowerShell is required to create the Windows test junction");
        assert!(
            output.status.success(),
            "could not create test junction: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::canonicalize(link).expect("test junction must resolve"),
            fs::canonicalize(target).unwrap()
        );
        assert!(same_file::is_same_file(link, target).unwrap());
    }
}
