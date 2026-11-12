//! Remote control, compatible with the Oscilloscope's auxiliary control channel.
//!
//! The Oscilloscope listens on UDP port 3130 (one below the discovery port) for JSON
//! instructions, so other programs can drive it:
//!
//! ```json
//! {"version": 0, "target": "WIFI_HOST", "action": ["RECORD_BEGIN"]}
//! {"version": 0, "target": "WIFI_HOST", "action": ["USER_NOTE", "stimulus on"]}
//! {"version": 0, "target": "WIFI_HOST", "action": ["EMOTIBIT_CONNECT", "MD-V6-0000356"]}
//! ```
//!
//! Supported actions: `EMOTIBIT_CONNECT <id>`, `EMOTIBIT_DISCONNECT`, `RECORD_BEGIN`,
//! `RECORD_END`, `USER_NOTE [text]`. One extension: `SET_POWER_MODE <name>` (`normal`, `low`,
//! `max-low`, `wireless-off`, `hibernate`).

use anyhow::{Context, Result, anyhow, bail};
use log::{debug, warn};
use serde::Deserialize;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::emotibit_client::EmotiBitHandle;
use crate::protocol::ADVERTISING_PORT;
use crate::status::PowerMode;

/// The port the Oscilloscope listens on for remote instructions.
pub const AUX_PORT: u16 = ADVERTISING_PORT - 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteCommand {
    Connect(String),
    Disconnect,
    RecordBegin,
    RecordEnd,
    UserNote(String),
    SetPowerMode(PowerMode),
}

#[derive(Deserialize)]
struct Instruction {
    version: i64,
    target: String,
    #[serde(default)]
    action: Vec<String>,
}

/// Parse one JSON instruction.
pub fn parse_instruction(json: &str) -> Result<RemoteCommand> {
    let i: Instruction = serde_json::from_str(json).context("not a valid instruction")?;
    if i.version != 0 {
        bail!("unsupported instruction version {}", i.version);
    }
    if i.target != "WIFI_HOST" {
        bail!("unknown target {:?}", i.target);
    }
    let name = i.action.first().ok_or_else(|| anyhow!("empty action"))?;
    let arg = i.action.get(1).cloned();
    Ok(match name.as_str() {
        "EMOTIBIT_CONNECT" => RemoteCommand::Connect(arg.ok_or_else(|| anyhow!("EMOTIBIT_CONNECT needs a device id"))?),
        "EMOTIBIT_DISCONNECT" => RemoteCommand::Disconnect,
        "RECORD_BEGIN" => RemoteCommand::RecordBegin,
        "RECORD_END" => RemoteCommand::RecordEnd,
        "USER_NOTE" => RemoteCommand::UserNote(arg.unwrap_or_default()),
        "SET_POWER_MODE" => {
            let m = arg.ok_or_else(|| anyhow!("SET_POWER_MODE needs a mode"))?;
            RemoteCommand::SetPowerMode(PowerMode::from_name(&m).ok_or_else(|| anyhow!("unknown power mode {m:?}"))?)
        }
        other => bail!("unknown action {other:?}"),
    })
}

/// Listen on UDP `port` and deliver parsed commands. The caller applies them, because
/// `Connect` needs a client and a new handle.
pub async fn spawn_remote(port: u16) -> Result<(JoinHandle<()>, mpsc::Receiver<RemoteCommand>)> {
    let socket = UdpSocket::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("cannot bind UDP port {port} (is the Oscilloscope running?)"))?;
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        loop {
            let Ok((n, from)) = socket.recv_from(&mut buf).await else { break };
            let text = String::from_utf8_lossy(&buf[..n]);
            match parse_instruction(text.trim()) {
                Ok(cmd) => {
                    debug!("remote command from {from}: {cmd:?}");
                    if tx.send(cmd).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("ignored instruction from {from}: {e}"),
            }
        }
    });
    Ok((task, rx))
}

/// Apply every command that acts on an already-connected device. `Connect` is returned
/// to the caller untouched.
pub async fn apply(handle: &EmotiBitHandle, cmd: RemoteCommand) -> Result<Option<RemoteCommand>> {
    match cmd {
        RemoteCommand::Disconnect => handle.disconnect().await?,
        RemoteCommand::RecordBegin => handle.record_begin().await?,
        RemoteCommand::RecordEnd => handle.record_end().await?,
        RemoteCommand::UserNote(t) => handle.user_note(&t).await?,
        RemoteCommand::SetPowerMode(m) => handle.set_power_mode(m).await?,
        c @ RemoteCommand::Connect(_) => return Ok(Some(c)),
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ins(action: &str) -> String {
        format!(r#"{{"version":0,"target":"WIFI_HOST","action":{action}}}"#)
    }

    #[test]
    fn parses_the_oscilloscope_actions() {
        assert_eq!(parse_instruction(&ins(r#"["RECORD_BEGIN"]"#)).unwrap(), RemoteCommand::RecordBegin);
        assert_eq!(parse_instruction(&ins(r#"["RECORD_END"]"#)).unwrap(), RemoteCommand::RecordEnd);
        assert_eq!(parse_instruction(&ins(r#"["EMOTIBIT_DISCONNECT"]"#)).unwrap(), RemoteCommand::Disconnect);
        assert_eq!(
            parse_instruction(&ins(r#"["USER_NOTE","hello"]"#)).unwrap(),
            RemoteCommand::UserNote("hello".into())
        );
        assert_eq!(
            parse_instruction(&ins(r#"["EMOTIBIT_CONNECT","MD-V6-1"]"#)).unwrap(),
            RemoteCommand::Connect("MD-V6-1".into())
        );
        assert_eq!(
            parse_instruction(&ins(r#"["SET_POWER_MODE","low"]"#)).unwrap(),
            RemoteCommand::SetPowerMode(PowerMode::Low)
        );
    }

    #[test]
    fn rejects_bad_instructions() {
        assert!(parse_instruction("not json").is_err());
        assert!(parse_instruction(r#"{"version":1,"target":"WIFI_HOST","action":["RECORD_BEGIN"]}"#).is_err());
        assert!(parse_instruction(r#"{"version":0,"target":"OTHER","action":["RECORD_BEGIN"]}"#).is_err());
        assert!(parse_instruction(&ins(r#"["NOPE"]"#)).is_err());
        assert!(parse_instruction(&ins("[]")).is_err());
        assert!(parse_instruction(&ins(r#"["EMOTIBIT_CONNECT"]"#)).is_err());
    }

    #[tokio::test]
    async fn udp_listener_delivers_commands() {
        // Find a free port, release it, and listen there.
        let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (task, mut rx) = spawn_remote(port).await.unwrap();

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender.send_to(b"garbage", ("127.0.0.1", port)).await.unwrap(); // ignored
        sender.send_to(ins(r#"["USER_NOTE","hi"]"#).as_bytes(), ("127.0.0.1", port)).await.unwrap();

        let got = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await.unwrap();
        assert_eq!(got, Some(RemoteCommand::UserNote("hi".into())));
        task.abort();
    }
}
