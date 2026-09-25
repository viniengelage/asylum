//! An SSH port forward made with the system's `ssh`, so `~/.ssh/config`, keys and the agent
//! work as they do in the terminal. It lives as long as any session that dialed through it.

use anyhow::{Context as _, anyhow};
use futures::AsyncReadExt as _;
use serde::{Deserialize, Serialize};
use std::{
    net::{Ipv4Addr, SocketAddr, TcpListener},
    sync::Mutex,
    time::{Duration, Instant},
};
use util::command::{Child, Stdio};

const READY_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTunnel {
    /// A host from `~/.ssh/config` or `user@host`.
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl SshTunnel {
    pub fn label(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}", self.host),
            None => self.host.clone(),
        }
    }
}

/// A running `ssh -N -L`; dropping it ends the process.
pub struct Tunnel {
    child: Mutex<Child>,
    pub local_port: u16,
}

impl std::fmt::Debug for Tunnel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Tunnel")
            .field("local_port", &self.local_port)
            .finish()
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock()
            && let Err(error) = child.kill()
        {
            log::warn!("Banco: não deu para encerrar o túnel SSH: {error}");
        }
    }
}

fn free_local_port() -> anyhow::Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

/// The arguments for `ssh`: no shell, no password prompt (there is no terminal to type it in),
/// and fail instead of connecting without the forward.
pub fn ssh_arguments(
    tunnel: &SshTunnel,
    local_port: u16,
    database_host: &str,
    database_port: u16,
) -> Vec<String> {
    let mut arguments = vec![
        "-N".to_owned(),
        "-o".to_owned(),
        "BatchMode=yes".to_owned(),
        "-o".to_owned(),
        "ExitOnForwardFailure=yes".to_owned(),
        "-o".to_owned(),
        "ServerAliveInterval=30".to_owned(),
        "-L".to_owned(),
        format!("127.0.0.1:{local_port}:{database_host}:{database_port}"),
    ];
    if let Some(port) = tunnel.port {
        arguments.push("-p".to_owned());
        arguments.push(port.to_string());
    }
    arguments.push(tunnel.host.clone());
    arguments
}

/// Starts the forward and waits until the local port accepts connections. Runs on the tokio
/// runtime, which the readiness check's socket and timer need.
pub async fn open(
    tunnel: &SshTunnel,
    database_host: &str,
    database_port: u16,
) -> anyhow::Result<Tunnel> {
    let local_port = free_local_port()?;
    let mut child = util::command::new_command("ssh")
        .args(ssh_arguments(tunnel, local_port, database_host, database_port))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("não deu para rodar o ssh do sistema")?;
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, local_port));
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_status()? {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                pipe.read_to_string(&mut stderr).await.ok();
            }
            let stderr = stderr.trim();
            return Err(anyhow!(
                "o túnel SSH para {} saiu ({status}){}",
                tunnel.label(),
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            ));
        }
        let probe = tokio::time::timeout(
            Duration::from_millis(200),
            tokio::net::TcpStream::connect(address),
        )
        .await;
        if matches!(probe, Ok(Ok(_))) {
            return Ok(Tunnel {
                child: Mutex::new(child),
                local_port,
            });
        }
        if started.elapsed() > READY_TIMEOUT {
            child.kill().ok();
            return Err(anyhow!(
                "o túnel SSH para {} não abriu em {} s",
                tunnel.label(),
                READY_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwards_to_the_database_through_the_host() {
        let tunnel = SshTunnel {
            host: "bastion".into(),
            port: Some(2222),
        };
        assert_eq!(
            ssh_arguments(&tunnel, 61000, "db.internal", 5432).join(" "),
            "-N -o BatchMode=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=30 \
             -L 127.0.0.1:61000:db.internal:5432 -p 2222 bastion"
        );
    }

    /// Both halves swap `ssh` on the PATH, so they share one test instead of racing.
    #[test]
    fn waits_for_the_forward_and_reports_failures() {
        let runtime = reqwest_client::runtime();
        let tunnel = SshTunnel {
            host: "asylum-no-such-host.invalid".into(),
            port: None,
        };
        let error = runtime
            .block_on(open(&tunnel, "localhost", 5432))
            .unwrap_err();
        assert!(format!("{error:#}").contains("asylum-no-such-host.invalid"));

        // A stand-in `ssh` that only listens on the forwarded local port.
        let directory = std::env::temp_dir().join(format!("asylum-fake-ssh-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let script = directory.join("ssh");
        std::fs::write(
            &script,
            "#!/usr/bin/env python3\n\
             import socket, sys, time\n\
             forward = sys.argv[sys.argv.index('-L') + 1]\n\
             port = int(forward.split(':')[1])\n\
             server = socket.socket()\n\
             server.bind(('127.0.0.1', port))\n\
             server.listen()\n\
             time.sleep(60)\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let original_path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![directory.clone()];
        paths.extend(std::env::split_paths(&original_path));
        // SAFETY: no other test in this crate reads PATH or spawns processes concurrently.
        unsafe { std::env::set_var("PATH", std::env::join_paths(paths).unwrap()) };
        let opened = runtime.block_on(open(
            &SshTunnel {
                host: "bastion".into(),
                port: None,
            },
            "db.internal",
            5432,
        ));
        // SAFETY: as above.
        unsafe { std::env::set_var("PATH", &original_path) };
        let tunnel = opened.unwrap();
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, tunnel.local_port));
        assert!(std::net::TcpStream::connect(address).is_ok());
        drop(tunnel);
        std::thread::sleep(Duration::from_millis(300));
        assert!(std::net::TcpStream::connect(address).is_err());
        std::fs::remove_dir_all(&directory).ok();
    }
}
