// Copyright (c) 2026 chulingera2025
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! CLI: the `raddex run`, `raddex upgrade`, and `raddex check` subcommands.
//!
//! `check` and a reload share the exact same [`snapshot::build`] pipeline
//! (Q7): a config that passes `raddex check` reloads cleanly, and vice versa.
//!
//! `raddex run` accepts pingora's two upgrade-related mode flags (M7, ADR-008):
//! `-u/--upgrade` starts as the replacement side of a zero-downtime upgrade
//! (acquire the running instance's listening fds), and `-t/--test` validates
//! the config and construction, then exits — the `raddex upgrade` pre-flight.
//! `raddex upgrade` orchestrates the whole upgrade using the *new* binary.

use crate::config::ast::ConfigError;
use crate::config::snapshot;
use crate::migrate::ImportFormat;
use crate::server::startup::{self, RunOptions};
use crate::server::upgrade;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// The options shared by `raddex run` and `raddex upgrade` (the latter re-invokes
/// the former with the same flags).
#[derive(Args, Debug)]
struct ServerArgs {
    /// Path to the Raddexfile.
    #[arg(short, long, default_value = CONFIG_FILE_NAME)]
    config: PathBuf,
    /// Directory for ACME certificates and the account credentials.
    #[arg(long, default_value = "raddex_certs")]
    cert_dir: PathBuf,
    /// ACME directory URL (Let's Encrypt production by default).
    #[arg(long, default_value = "https://acme-v02.api.letsencrypt.org/directory")]
    acme_directory: String,
    /// PEM root CA that trusts the ACME server (required for a test server
    /// such as Pebble whose CA is not publicly trusted).
    #[arg(long)]
    acme_root_pem: Option<PathBuf>,
    /// Append structured JSON access logs to this file.
    #[arg(long)]
    access_log: Option<PathBuf>,
    /// Expose Prometheus /metrics on this address (e.g. 127.0.0.1:9100).
    #[arg(long)]
    metrics_addr: Option<String>,
    /// Write this process's PID to this file so `raddex upgrade` can find it.
    #[arg(long)]
    pidfile: Option<PathBuf>,
    /// Unix socket both the old and new process use to hand over listening fds.
    #[arg(long, default_value = "/tmp/raddex_upgrade.sock")]
    upgrade_sock: String,
    /// Number of Pingora worker threads allocated to the HTTP service.
    #[arg(long, default_value_t = 1, value_parser = positive_threads)]
    threads: usize,
}

/// The current default config file name.
const CONFIG_FILE_NAME: &str = "Raddexfile";
/// The pre-`v0.4.0` config file name, still accepted as a fallback so an
/// existing deployment keeps starting after the `raddy` → `raddex` rename.
const LEGACY_CONFIG_FILE_NAME: &str = "Raddyfile";

/// Resolve the config path, falling back to the legacy `Raddyfile` name.
///
/// When `path` names a `Raddexfile` that does not exist but a `Raddyfile` sits
/// beside it, the legacy file is used and a deprecation warning is printed. Any
/// other path is returned untouched so the normal "cannot read" error surfaces
/// from the config loader rather than being masked here.
///
/// The fallback is removed in `v0.4.0`; see `CHANGELOG.md`.
fn resolve_config_path(path: PathBuf) -> PathBuf {
    if path.exists() || path.file_name() != Some(CONFIG_FILE_NAME.as_ref()) {
        return path;
    }
    let legacy = path.with_file_name(LEGACY_CONFIG_FILE_NAME);
    if !legacy.exists() {
        return path;
    }
    eprintln!(
        "warning: using deprecated config file {}; rename it to {} \
         (the {LEGACY_CONFIG_FILE_NAME} fallback is removed in v0.4.0)",
        legacy.display(),
        path.display()
    );
    legacy
}

/// Parse a positive worker-thread count for the server service.
fn positive_threads(value: &str) -> Result<usize, String> {
    let threads = value
        .parse::<usize>()
        .map_err(|_| format!("invalid worker thread count '{value}'"))?;
    if threads == 0 {
        return Err("worker thread count must be at least 1".to_string());
    }
    Ok(threads)
}

impl ServerArgs {
    /// Build the runtime options for a `run` (or an upgrade driver) invocation.
    fn into_run_options(self, upgrade: bool, test: bool) -> RunOptions {
        RunOptions {
            cert_dir: self.cert_dir,
            acme_directory: self.acme_directory,
            acme_root_pem: self.acme_root_pem,
            access_log: self.access_log,
            metrics_addr: self.metrics_addr,
            upgrade,
            test,
            pidfile: self.pidfile,
            upgrade_sock: self.upgrade_sock,
            threads: self.threads,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "raddex",
    version,
    about = "A minimal high-performance reverse proxy gateway built on Pingora"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the reverse proxy server in the foreground.
    Run {
        #[command(flatten)]
        server: ServerArgs,
        /// Start as the *new* side of a zero-downtime upgrade: acquire the
        /// running instance's listening fds (normally spawned by `raddex upgrade`).
        #[arg(short, long)]
        upgrade: bool,
        /// Validate the config and construction, then exit 0/1 without binding
        /// any listener (the `raddex upgrade` pre-flight).
        #[arg(short, long)]
        test: bool,
    },
    /// Zero-downtime binary upgrade (requires `--pidfile`): pre-flight the new
    /// binary, spawn a replacement with `-u`, then SIGQUIT the running instance.
    Upgrade {
        #[command(flatten)]
        server: ServerArgs,
    },
    /// Validate a Raddexfile and exit (the same checks a reload performs).
    Check {
        /// Path to the Raddexfile.
        #[arg(short, long, default_value = CONFIG_FILE_NAME)]
        config: PathBuf,
    },
    /// Convert a Caddyfile or nginx.conf subset into a Raddexfile. An
    /// independent converter: it never changes the Raddexfile grammar, and it
    /// validates its own output before printing.
    Import {
        /// The source format.
        #[arg(value_enum)]
        format: ImportFormat,
        /// Path to the source config file.
        source: PathBuf,
        /// Write the Raddexfile to this file instead of printing to stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

/// Entry point for the `raddex` binary.
pub fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Check { config } => {
            let config = resolve_config_path(config);
            match snapshot::build(&config) {
                Ok(_) => {
                    println!("{}: ok", config.display());
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        Command::Run {
            server,
            upgrade,
            test,
        } => {
            let config = resolve_config_path(server.config.clone());
            let opts = server.into_run_options(upgrade, test);
            if let Err(e) = startup::run(&config, &opts) {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        Command::Upgrade { server } => {
            let config = resolve_config_path(server.config.clone());
            let opts = server.into_run_options(false, false);
            if let Err(e) = upgrade::upgrade(&config, &opts) {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        Command::Import {
            format,
            source,
            output,
        } => match crate::migrate::import(format, &source) {
            Ok(converted) => {
                if converted.raddexfile.trim().is_empty() {
                    eprintln!("nothing convertible from {}", source.display());
                    std::process::exit(1);
                }
                // Validate with the same pipeline a reload uses (Q7), so the
                // converter can never hand the operator an unparseable config.
                if let Err(e) = validate_converted(&converted.raddexfile) {
                    eprintln!("migration produced invalid Raddexfile: {e}");
                    std::process::exit(1);
                }
                for warning in &converted.warnings {
                    eprintln!("warning: {warning}");
                }
                match &output {
                    Some(path) => {
                        if let Err(e) = std::fs::write(path, &converted.raddexfile) {
                            eprintln!("failed to write {}: {e}", path.display());
                            std::process::exit(1);
                        }
                        println!("wrote {}", path.display());
                    }
                    None => print!("{}", converted.raddexfile),
                }
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },
    }
}

/// Validate an emitted Raddexfile with the parse → validate pipeline (Q7).
fn validate_converted(raddexfile: &str) -> Result<(), ConfigError> {
    let parsed = crate::config::parser::parse("import", raddexfile)?;
    crate::config::validate::validate_and_compile("import", &parsed)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_thread_count_defaults_to_one() {
        let cli = Cli::try_parse_from(["raddex", "run"]).expect("default CLI should parse");
        let Command::Run { server, .. } = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(server.threads, 1);
    }

    #[test]
    fn worker_thread_count_rejects_zero() {
        let error = match Cli::try_parse_from(["raddex", "run", "--threads", "0"]) {
            Ok(_) => panic!("zero worker threads must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("at least 1"));
    }

    /// A scratch directory unique to one test.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("raddex_cli_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn legacy_raddyfile_is_used_when_the_new_name_is_absent() {
        // An existing pre-rename deployment must keep starting: `Raddexfile` is
        // missing, the neighbouring `Raddyfile` is picked up instead.
        let dir = scratch("legacy");
        let legacy = dir.join(LEGACY_CONFIG_FILE_NAME);
        std::fs::write(&legacy, ":8080 {\n    respond 200 ok\n}\n").expect("write legacy config");
        assert_eq!(resolve_config_path(dir.join(CONFIG_FILE_NAME)), legacy);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn new_name_wins_when_both_exist() {
        let dir = scratch("both");
        let current = dir.join(CONFIG_FILE_NAME);
        std::fs::write(&current, ":8080 {\n}\n").expect("write current config");
        std::fs::write(dir.join(LEGACY_CONFIG_FILE_NAME), ":9090 {\n}\n").expect("write legacy");
        assert_eq!(resolve_config_path(current.clone()), current);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fallback_does_not_mask_a_genuinely_missing_config() {
        // With neither file present the original path is returned unchanged, so
        // the config loader reports "cannot read <Raddexfile>" as usual.
        let dir = scratch("missing");
        let current = dir.join(CONFIG_FILE_NAME);
        assert_eq!(resolve_config_path(current.clone()), current);
        // A non-default file name never triggers the fallback.
        let custom = dir.join("my.conf");
        assert_eq!(resolve_config_path(custom.clone()), custom);
        std::fs::remove_dir_all(&dir).ok();
    }
}
