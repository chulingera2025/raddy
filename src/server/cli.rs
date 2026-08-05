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

//! CLI: the `raddy run`, `raddy upgrade`, and `raddy check` subcommands.
//!
//! `check` and a reload share the exact same [`snapshot::build`] pipeline
//! (Q7): a config that passes `raddy check` reloads cleanly, and vice versa.
//!
//! `raddy run` accepts pingora's two upgrade-related mode flags (M7, ADR-008):
//! `-u/--upgrade` starts as the replacement side of a zero-downtime upgrade
//! (acquire the running instance's listening fds), and `-t/--test` validates
//! the config and construction, then exits — the `raddy upgrade` pre-flight.
//! `raddy upgrade` orchestrates the whole upgrade using the *new* binary.

use crate::config::snapshot;
use crate::server::startup::{self, RunOptions};
use crate::server::upgrade;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// The options shared by `raddy run` and `raddy upgrade` (the latter re-invokes
/// the former with the same flags).
#[derive(Args, Debug)]
struct ServerArgs {
    /// Path to the Raddyfile.
    #[arg(short, long, default_value = "Raddyfile")]
    config: PathBuf,
    /// Directory for ACME certificates and the account credentials.
    #[arg(long, default_value = "raddy_certs")]
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
    /// Write this process's PID to this file so `raddy upgrade` can find it.
    #[arg(long)]
    pidfile: Option<PathBuf>,
    /// Unix socket both the old and new process use to hand over listening fds.
    #[arg(long, default_value = "/tmp/raddy_upgrade.sock")]
    upgrade_sock: String,
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
        }
    }
}

#[derive(Parser)]
#[command(
    name = "raddy",
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
        /// running instance's listening fds (normally spawned by `raddy upgrade`).
        #[arg(short, long)]
        upgrade: bool,
        /// Validate the config and construction, then exit 0/1 without binding
        /// any listener (the `raddy upgrade` pre-flight).
        #[arg(short, long)]
        test: bool,
    },
    /// Zero-downtime binary upgrade (requires `--pidfile`): pre-flight the new
    /// binary, spawn a replacement with `-u`, then SIGQUIT the running instance.
    Upgrade {
        #[command(flatten)]
        server: ServerArgs,
    },
    /// Validate a Raddyfile and exit (the same checks a reload performs).
    Check {
        /// Path to the Raddyfile.
        #[arg(short, long, default_value = "Raddyfile")]
        config: PathBuf,
    },
}

/// Entry point for the `raddy` binary.
pub fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Check { config } => match snapshot::build(&config) {
            Ok(_) => {
                println!("{}: ok", config.display());
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },
        Command::Run {
            server,
            upgrade,
            test,
        } => {
            let config = server.config.clone();
            let opts = server.into_run_options(upgrade, test);
            if let Err(e) = startup::run(&config, &opts) {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        Command::Upgrade { server } => {
            let config = server.config.clone();
            let opts = server.into_run_options(false, false);
            if let Err(e) = upgrade::upgrade(&config, &opts) {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }
}
