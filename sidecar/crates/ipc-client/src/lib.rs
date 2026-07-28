//! Authenticated, length-prefixed local IPC for the FreeCAD bridge.

use bridge_dto::{BridgeSnapshot, IPC_VERSION, IpcRequest, IpcResponse, ResponseStatus, WireError};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use thiserror::Error;
use uuid::Uuid;

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
        let response = self.request(
            "bridge.authenticate",
            serde_json::json!({
                "nonce": nonce,
                "proof": bridge_dto::pairing_proof(&self.secret, &nonce)
                    .map_err(|_| IpcError::Authentication)?
            }),
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
    bridge_dto::write_frame(stream, value).map_err(Into::into)
}

pub fn read_frame<T: DeserializeOwned>(stream: &mut impl Read) -> Result<T, IpcError> {
    bridge_dto::read_frame(stream).map_err(Into::into)
}

impl From<WireError> for IpcError {
    fn from(error: WireError) -> Self {
        match error {
            WireError::Io(error) => Self::Io(error),
            WireError::Serialization(error) => Self::Serialization(error),
            WireError::FrameTooLarge(size) => Self::FrameTooLarge(size),
            WireError::InvalidProof => Self::Authentication,
        }
    }
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
        let result = write_frame(&mut Vec::new(), &"x".repeat(bridge_dto::MAX_FRAME + 1));
        assert!(matches!(result, Err(IpcError::FrameTooLarge(_))));
    }
}
