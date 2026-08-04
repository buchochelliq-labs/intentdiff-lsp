//! Known-server specs + user config loading (#100 S2 slice 4), ported from
//! `src/intentumdiff/lsp/servers.py` and the pure decision half of `launcher.py`.
//!
//! Three layers:
//! - [`known_server_specs`]: the built-in auto-start table (data, value-for-value);
//! - [`load_lsp_servers_json`]: strict validated loading of user-defined entries
//!   (`lsp_servers.json`), including the fail-closed TCP-autostart trust gate semantics —
//!   note the loader keys by LANGUAGE and the FIRST entry for a language wins (the Python
//!   docstring claims last-wins; the code does first-wins, and the code is the contract);
//! - [`resolve_launch`]: `LspServerProcess.start`'s decision tree as pure logic — a stdio
//!   plan (command with `{pid}` substituted), a manual TCP connect, a fail-closed refusal
//!   for un-opted-in TCP autostart (#88), or a described-but-not-executed TCP autostart
//!   plan (the port-reservation spawn is deliberately not implemented natively yet — the
//!   Python launcher remains the only executor of that legacy mode).

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

/// Transport for a server spec or user entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Tcp,
    Stdio,
}

/// Auto-start specification for a language server (`LspServerSpec`).
#[derive(Debug, Clone, PartialEq)]
pub struct ServerSpec {
    pub command: Vec<String>,
    pub transport: Transport,
    pub startup_timeout: f64,
    pub install_hint: String,
    pub allow_unverified_tcp_autostart: bool,
}

impl ServerSpec {
    fn stdio(command: &[&str], startup_timeout: f64, install_hint: &str) -> Self {
        Self {
            command: command.iter().map(|s| (*s).to_owned()).collect(),
            transport: Transport::Stdio,
            startup_timeout,
            install_hint: install_hint.to_owned(),
            allow_unverified_tcp_autostart: false,
        }
    }
}

/// `KNOWN_SERVER_SPECS`, value-for-value.
pub fn known_server_specs() -> HashMap<&'static str, ServerSpec> {
    HashMap::from([
        ("python", ServerSpec::stdio(&["pylsp"], 10.0, "uv pip install python-lsp-server")),
        ("go", ServerSpec::stdio(&["gopls"], 20.0, "go install golang.org/x/tools/gopls@latest")),
        ("ruby", ServerSpec::stdio(&["solargraph", "stdio"], 15.0, "gem install solargraph")),
        ("php", ServerSpec::stdio(
            &["phpactor", "language-server"], 10.0,
            "composer global require phpactor/phpactor",
        )),
        ("javascript", ServerSpec::stdio(
            &["typescript-language-server", "--stdio"], 15.0,
            "npm install -g typescript-language-server typescript",
        )),
        ("typescript", ServerSpec::stdio(
            &["typescript-language-server", "--stdio"], 15.0,
            "npm install -g typescript-language-server typescript",
        )),
        ("tsx", ServerSpec::stdio(
            &["typescript-language-server", "--stdio"], 15.0,
            "npm install -g typescript-language-server typescript",
        )),
        ("rust", ServerSpec::stdio(&["rust-analyzer"], 15.0, "rustup component add rust-analyzer")),
        ("c", ServerSpec::stdio(&["clangd"], 15.0, "https://clangd.llvm.org/installation")),
        ("cpp", ServerSpec::stdio(&["clangd"], 15.0, "https://clangd.llvm.org/installation")),
        ("bash", ServerSpec::stdio(
            &["bash-language-server", "start"], 15.0,
            "npm install -g bash-language-server",
        )),
        ("kotlin", ServerSpec::stdio(
            &["kotlin-language-server"], 15.0,
            "https://github.com/fwcd/kotlin-language-server/releases",
        )),
        ("swift", ServerSpec::stdio(
            &["sourcekit-lsp"], 15.0,
            "Included with Xcode or https://swift.org/download",
        )),
        ("java", ServerSpec::stdio(&["jdtls"], 15.0, "https://github.com/eclipse/eclipse.jdt.ls")),
        ("csharp", ServerSpec::stdio(
            &["OmniSharp", "--languageserver"], 15.0,
            "https://github.com/OmniSharp/omnisharp-roslyn/releases",
        )),
        ("dbt-sql", ServerSpec::stdio(
            &["dbt-lsp"], 15.0, "https://docs.getdbt.com/docs/install-dbt-extension",
        )),
        ("dbt-yaml", ServerSpec::stdio(
            &["dbt-lsp"], 15.0, "https://docs.getdbt.com/docs/install-dbt-extension",
        )),
    ])
}

/// One validated user entry from `lsp_servers.json` (`LspServerEntry`).
#[derive(Debug, Clone, PartialEq)]
pub struct ServerEntry {
    pub language: String,
    pub transport: Transport,
    pub command: Vec<String>,
    pub host: String,
    pub port: Option<u16>,
    pub startup_timeout: f64,
    pub install_hint: String,
    pub allow_unverified_tcp_autostart: bool,
}

impl ServerEntry {
    /// `transport="tcp"` with no command: a pre-started server we just connect to.
    pub fn is_manual_connect(&self) -> bool {
        self.transport == Transport::Tcp && self.command.is_empty()
    }

    pub fn to_spec(&self) -> Result<ServerSpec, String> {
        if self.command.is_empty() {
            return Err(
                "Cannot convert a manual-connect entry (no command) to a server spec".to_owned(),
            );
        }
        Ok(ServerSpec {
            command: self.command.clone(),
            transport: self.transport,
            startup_timeout: self.startup_timeout,
            install_hint: self.install_hint.clone(),
            allow_unverified_tcp_autostart: self.allow_unverified_tcp_autostart,
        })
    }

    fn parse(name: &str, data: &Value) -> Result<Self, String> {
        let obj = data
            .as_object()
            .ok_or_else(|| format!("entry {name:?} must be a JSON object"))?;
        let language = obj
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if language.is_empty() {
            return Err(format!("invalid entry {name:?}: language is required"));
        }
        let transport = match obj.get("transport").and_then(Value::as_str) {
            None | Some("stdio") => Transport::Stdio,
            Some("tcp") => Transport::Tcp,
            Some(other) => {
                return Err(format!("invalid entry {name:?}: unknown transport {other:?}"));
            }
        };
        let command: Vec<String> = match obj.get("command") {
            None => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str().map(str::to_owned).ok_or_else(|| {
                        format!("invalid entry {name:?}: command items must be strings")
                    })
                })
                .collect::<Result<_, _>>()?,
            Some(_) => {
                return Err(format!("invalid entry {name:?}: command must be an array"));
            }
        };
        let host = match obj.get("host") {
            None => "localhost".to_owned(),
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(_) => return Err(format!("invalid entry {name:?}: host must be a non-empty string")),
        };
        let port = match obj.get("port") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let n = v
                    .as_i64()
                    .ok_or_else(|| format!("invalid entry {name:?}: port must be an integer"))?;
                if !(1..=65535).contains(&n) {
                    return Err(format!(
                        "invalid entry {name:?}: port must be between 1 and 65535"
                    ));
                }
                Some(n as u16)
            }
        };
        let startup_timeout = match obj.get("startup_timeout") {
            None => 15.0,
            Some(v) => {
                let t = v.as_f64().ok_or_else(|| {
                    format!("invalid entry {name:?}: startup_timeout must be a number")
                })?;
                if t <= 0.0 {
                    return Err(format!(
                        "invalid entry {name:?}: startup_timeout must be positive"
                    ));
                }
                t
            }
        };
        let install_hint = obj
            .get("install_hint")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let allow_unverified_tcp_autostart = obj
            .get("allow_unverified_tcp_autostart")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // The Python model validators, rule-for-rule.
        if transport == Transport::Stdio && command.is_empty() {
            return Err(format!(
                "invalid entry {name:?}: command is required for stdio transport"
            ));
        }
        if transport == Transport::Tcp && command.is_empty() && port.is_none() {
            return Err(format!(
                "invalid entry {name:?}: for tcp without command (manual-connect mode) port is required"
            ));
        }

        Ok(Self {
            language,
            transport,
            command,
            host,
            port,
            startup_timeout,
            install_hint,
            allow_unverified_tcp_autostart,
        })
    }
}

/// `load_lsp_servers_json`: language → entry; FIRST entry for a language wins; a missing
/// file is an empty map; malformed JSON or an invalid entry is an error.
pub fn load_lsp_servers_json(path: &Path) -> Result<HashMap<String, ServerEntry>, String> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let parsed: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("{}: invalid JSON — {e}", path.display()))?;
    let obj = parsed
        .as_object()
        .ok_or_else(|| format!("{}: expected a JSON object at the top level", path.display()))?;
    let mut result: HashMap<String, ServerEntry> = HashMap::new();
    for (name, data) in obj {
        let entry = ServerEntry::parse(name, data)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        result.entry(entry.language.clone()).or_insert(entry);
    }
    Ok(result)
}

/// What `LspServerProcess.start` would do — as data, with no process side effects.
#[derive(Debug, Clone, PartialEq)]
pub enum LaunchPlan {
    /// Spawn this command over stdio pipes (the client owns the process). `{pid}`
    /// placeholders are already substituted.
    Stdio { command: Vec<String> },
    /// Connect to an already-running TCP server.
    ManualTcp { host: String, port: u16 },
    /// The legacy opted-in TCP autostart, DESCRIBED but not executed: the native layer
    /// deliberately does not implement the port-reservation spawn (the Python launcher
    /// remains its only executor). `{host}`/`{port}` placeholders are left in place.
    TcpAutostart { command: Vec<String>, host: String, startup_timeout: f64 },
}

/// The pure decision tree of `LspServerProcess.start` (#88: TCP autostart fails closed
/// unless explicitly opted in; the refusal wording mirrors the Python launcher).
pub fn resolve_launch(
    language: &str,
    spec: Option<&ServerSpec>,
    host: &str,
) -> Result<LaunchPlan, String> {
    let known = known_server_specs();
    let spec = match spec {
        Some(s) => s.clone(),
        None => match known.get(language) {
            Some(s) => s.clone(),
            None => {
                let mut available: Vec<&str> = known.keys().copied().collect();
                available.sort_unstable();
                return Err(format!(
                    "No auto-start spec for language {language:?}. Available: {}",
                    available.join(", ")
                ));
            }
        },
    };
    match spec.transport {
        Transport::Tcp => {
            if !spec.allow_unverified_tcp_autostart {
                return Err(
                    "Auto-started TCP LSP servers are disabled by default. Use a stdio \
                     server spec or connect to an already-running manual TCP endpoint. \
                     Set allow_unverified_tcp_autostart only for trusted legacy commands \
                     that accept the local port-race residual risk."
                        .to_owned(),
                );
            }
            Ok(LaunchPlan::TcpAutostart {
                command: spec.command,
                host: host.to_owned(),
                startup_timeout: spec.startup_timeout,
            })
        }
        Transport::Stdio => {
            let pid = std::process::id().to_string();
            let command = spec
                .command
                .iter()
                .map(|part| part.replace("{pid}", &pid))
                .collect();
            Ok(LaunchPlan::Stdio { command })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_specs_match_the_python_table_on_sentinels() {
        let specs = known_server_specs();
        assert_eq!(specs.len(), 17);
        let python = &specs["python"];
        assert_eq!(python.command, vec!["pylsp"]);
        assert_eq!(python.startup_timeout, 10.0);
        assert_eq!(python.install_hint, "uv pip install python-lsp-server");
        assert_eq!(specs["go"].startup_timeout, 20.0);
        assert_eq!(specs["ruby"].command, vec!["solargraph", "stdio"]);
        assert_eq!(specs["csharp"].command, vec!["OmniSharp", "--languageserver"]);
        assert!(specs.values().all(|s| s.transport == Transport::Stdio));
        assert!(specs.values().all(|s| !s.allow_unverified_tcp_autostart));
    }

    fn parse_entry(name: &str, json: serde_json::Value) -> Result<ServerEntry, String> {
        ServerEntry::parse(name, &json)
    }

    #[test]
    fn entry_validation_mirrors_the_python_model() {
        // stdio requires a command.
        assert!(parse_entry("a", serde_json::json!({"language": "python"}))
            .unwrap_err()
            .contains("command is required for stdio"));
        // tcp without command requires a port.
        assert!(parse_entry(
            "b",
            serde_json::json!({"language": "go", "transport": "tcp"})
        )
        .unwrap_err()
        .contains("port is required"));
        // port bounds.
        assert!(parse_entry(
            "c",
            serde_json::json!({"language": "go", "transport": "tcp", "port": 70000})
        )
        .unwrap_err()
        .contains("between 1 and 65535"));
        // language required.
        assert!(parse_entry("d", serde_json::json!({"transport": "stdio", "command": ["x"]}))
            .unwrap_err()
            .contains("language is required"));
        // A valid manual-connect entry.
        let manual = parse_entry(
            "e",
            serde_json::json!({"language": "go", "transport": "tcp", "port": 2091}),
        )
        .unwrap();
        assert!(manual.is_manual_connect());
        assert!(manual.to_spec().is_err());
    }

    #[test]
    fn loader_keys_by_language_and_first_entry_wins() {
        let dir = std::env::temp_dir().join(format!("lsp-specs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("lsp_servers.json");
        std::fs::write(
            &file,
            r#"{
                "first-pylsp": {"language": "python", "command": ["pylsp"]},
                "second-pylsp": {"language": "python", "command": ["other-pylsp"]},
                "running-gopls": {"language": "go", "transport": "tcp", "port": 2091}
            }"#,
        )
        .unwrap();
        let loaded = load_lsp_servers_json(&file).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded["python"].command, vec!["pylsp"]);
        assert!(loaded["go"].is_manual_connect());
        let _ = std::fs::remove_file(&file);
        // Missing file → empty map, no error.
        assert!(load_lsp_servers_json(&dir.join("absent.json")).unwrap().is_empty());
    }

    #[test]
    fn loader_rejects_malformed_files() {
        let dir = std::env::temp_dir().join(format!("lsp-specs-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("lsp_servers.json");
        std::fs::write(&file, "[1, 2]").unwrap();
        assert!(load_lsp_servers_json(&file).unwrap_err().contains("top level"));
        std::fs::write(&file, "{not json").unwrap();
        assert!(load_lsp_servers_json(&file).unwrap_err().contains("invalid JSON"));
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn tcp_autostart_fails_closed_without_the_opt_in() {
        let spec = ServerSpec {
            command: vec!["legacy-server".to_owned(), "--port={port}".to_owned()],
            transport: Transport::Tcp,
            startup_timeout: 15.0,
            install_hint: String::new(),
            allow_unverified_tcp_autostart: false,
        };
        let err = resolve_launch("python", Some(&spec), "127.0.0.1").unwrap_err();
        assert!(err.contains("disabled by default"));

        let opted_in = ServerSpec { allow_unverified_tcp_autostart: true, ..spec };
        match resolve_launch("python", Some(&opted_in), "127.0.0.1").unwrap() {
            LaunchPlan::TcpAutostart { command, host, .. } => {
                assert_eq!(command[1], "--port={port}"); // placeholders left for the executor
                assert_eq!(host, "127.0.0.1");
            }
            other => panic!("expected TcpAutostart, got {other:?}"),
        }
    }

    #[test]
    fn stdio_plan_substitutes_pid_and_unknown_language_lists_available() {
        match resolve_launch("rust", None, "127.0.0.1").unwrap() {
            LaunchPlan::Stdio { command } => assert_eq!(command, vec!["rust-analyzer"]),
            other => panic!("expected Stdio, got {other:?}"),
        }
        let spec = ServerSpec::stdio(&["srv", "--client-pid={pid}"], 15.0, "");
        match resolve_launch("x", Some(&spec), "h").unwrap() {
            LaunchPlan::Stdio { command } => {
                assert_eq!(command[1], format!("--client-pid={}", std::process::id()));
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
        let err = resolve_launch("cobol-2099", None, "h").unwrap_err();
        assert!(err.contains("No auto-start spec"));
        assert!(err.contains("python")); // the sorted available list
    }
}
