use anyhow::{bail, Result};
use parking_lot::Mutex;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc;

#[derive(Default)]
pub struct WorkspaceTerminals {
    pub agent: Option<TerminalSession>,
    pub shells: HashMap<String, TerminalSession>,
}

pub struct TerminalSession {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    child: Arc<Mutex<Box<dyn Child + Send>>>,
}

pub enum TerminalOutput {
    Bytes(Vec<u8>),
    Exited(Option<i32>),
}

impl TerminalSession {
    pub async fn send_input(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock();
        writer.write_all(bytes)?;
        Ok(())
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        self.master.lock().resize(size)?;
        Ok(())
    }

    pub async fn stop(self) -> Result<()> {
        self.child.lock().kill()?;
        Ok(())
    }
}

pub async fn start_terminal(
    cwd: PathBuf,
    cmd: Vec<String>,
) -> Result<(TerminalSession, mpsc::Receiver<TerminalOutput>)> {
    let Some(program) = cmd.first() else {
        bail!("terminal command cannot be empty");
    };

    let pty_system = native_pty_system();
    let pty_pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut builder = CommandBuilder::new(program);
    for arg in cmd.iter().skip(1) {
        builder.arg(arg);
    }
    builder.cwd(cwd);

    let child = pty_pair.slave.spawn_command(builder)?;
    let mut reader = pty_pair.master.try_clone_reader()?;
    let writer = pty_pair.master.take_writer()?;

    let (tx, rx) = mpsc::channel(512);
    let tx_reader = tx.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = tx_reader.blocking_send(TerminalOutput::Exited(None));
                    break;
                }
                Ok(n) => {
                    if tx_reader
                        .blocking_send(TerminalOutput::Bytes(buf[..n].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => {
                    let _ = tx_reader.blocking_send(TerminalOutput::Exited(None));
                    break;
                }
            }
        }
    });

    Ok((
        TerminalSession {
            writer: Arc::new(Mutex::new(writer)),
            master: Arc::new(Mutex::new(pty_pair.master)),
            child: Arc::new(Mutex::new(child)),
        },
        rx,
    ))
}
