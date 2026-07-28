//! Authenticated, length-prefixed local IPC for the FreeCAD bridge.

use bridge_dto::{BridgeSnapshot, IPC_VERSION, IpcRequest, IpcResponse, ResponseStatus};
use hmac::{Hmac, Mac};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::Sha256;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use thiserror::Error;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;
const MAX_FRAME: usize = 4 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("IPC connection failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("IPC serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("IPC frame is too large: {0} bytes")]
    FrameTooLarge(usize),
    #[error("bridge returned {status:?}: {message}")]
    Bridge {
        status: ResponseStatus,
        message: String,
    },
    #[error("bridge authentication failed")]
    Authentication,
    #[error("unsupported transport on this platform")]
    UnsupportedTransport,
}

pub trait IpcIo: Read + Write {}
impl<T: Read + Write> IpcIo for T {}

pub struct IpcClient {
    stream: Box<dyn IpcIo + Send>,
    secret: Vec<u8>,
    session_epoch: String,
}

impl IpcClient {
    #[cfg(unix)]
    pub fn connect_unix(
        path: impl AsRef<Path>,
        secret: impl AsRef<[u8]>,
    ) -> Result<Self, IpcError> {
        let stream = std::os::unix::net::UnixStream::connect(path)?;
        Ok(Self {
            stream: Box::new(stream),
            secret: secret.as_ref().to_vec(),
            session_epoch: String::new(),
        })
    }

    pub fn connect_endpoint(endpoint: &str, secret: impl AsRef<[u8]>) -> Result<Self, IpcError> {
        if let Some(address) = endpoint.strip_prefix("tcp://") {
            return Self::connect_tcp(address, secret);
        }
        Self::connect_unix(endpoint, secret)
    }

    pub fn connect_tcp(address: &str, secret: impl AsRef<[u8]>) -> Result<Self, IpcError> {
        let stream = TcpStream::connect(address)?;
        stream.set_nodelay(true)?;
        Ok(Self {
            stream: Box::new(stream),
            secret: secret.as_ref().to_vec(),
            session_epoch: String::new(),
        })
    }

    #[cfg(not(unix))]
    pub fn connect_unix(
        _path: impl AsRef<Path>,
        _secret: impl AsRef<[u8]>,
    ) -> Result<Self, IpcError> {
        Err(IpcError::UnsupportedTransport)
    }

    pub fn authenticate(&mut self) -> Result<IpcResponse, IpcError> {
        let nonce = Uuid::new_v4().to_string();
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).map_err(|_| IpcError::Authentication)?;
        mac.update(nonce.as_bytes());
        let response = self.request(
            "bridge.authenticate",
            serde_json::json!({"nonce": nonce, "proof": hex::encode(mac.finalize().into_bytes())}),
        )?;
        if response.status != ResponseStatus::Ok {
            return Err(IpcError::Authentication);
        }
        self.session_epoch = response.session_epoch.clone();
        Ok(response)
    }

    pub fn request(&mut self, message: &str, payload: Value) -> Result<IpcResponse, IpcError> {
        let request = IpcRequest {
            protocol_version: IPC_VERSION.into(),
            message: message.into(),
            request_id: Uuid::new_v4().to_string(),
            session_epoch: self.session_epoch.clone(),
            deadline_ms: 10_000,
            payload,
        };
        write_frame(&mut self.stream, &request)?;
        let response: IpcResponse = read_frame(&mut self.stream)?;
        if response.status == ResponseStatus::Error || response.status == ResponseStatus::Busy {
            return Err(IpcError::Bridge {
                status: response.status,
                message: response
                    .diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.message.clone())
                    .collect::<Vec<_>>()
                    .join("; "),
            });
        }
        Ok(response)
    }

    pub fn snapshot(&mut self, payload: Value) -> Result<BridgeSnapshot, IpcError> {
        let response = self.request("bridge.snapshot", payload)?;
        decode_payload(response)
    }

    pub fn call<T: DeserializeOwned>(
        &mut self,
        message: &str,
        payload: Value,
    ) -> Result<T, IpcError> {
        decode_payload(self.request(message, payload)?)
    }
}

pub fn write_frame<T: Serialize>(stream: &mut impl Write, value: &T) -> Result<(), IpcError> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME {
        return Err(IpcError::FrameTooLarge(bytes.len()));
    }
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}

pub fn read_frame<T: DeserializeOwned>(stream: &mut impl Read) -> Result<T, IpcError> {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length)?;
    let size = u32::from_be_bytes(length) as usize;
    if size > MAX_FRAME {
        return Err(IpcError::FrameTooLarge(size));
    }
    let mut bytes = vec![0_u8; size];
    stream.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn decode_payload<T: DeserializeOwned>(response: IpcResponse) -> Result<T, IpcError> {
    serde_json::from_value(response.payload.unwrap_or(Value::Null)).map_err(IpcError::Serialization)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frames_are_length_prefixed_and_bounded() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &serde_json::json!({"ok": true})).unwrap();
        let value: Value = read_frame(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(value["ok"], true);
    }

    #[test]
    fn oversized_frame_is_rejected_before_write() {
        let result = write_frame(&mut Vec::new(), &"x".repeat(MAX_FRAME + 1));
        assert!(matches!(result, Err(IpcError::FrameTooLarge(_))));
    }
}
